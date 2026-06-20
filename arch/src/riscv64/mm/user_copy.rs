//! RISC-V64 的 `copy_from_user` / `copy_to_user` / `strnlen_user`。
//!
//! 利用 RISC-V 的 `SSTATUS.SUM`（Supervisor User Memory access）位：
//! 置 SUM=1 后 S-mode 可直接访问 U 标记的页面，配合 `__ex_table` 实现
//! 安全拷贝。采用分级宽度策略：先对齐到 8 字节边界，主循环走 `ld`/`sd`
//! 双字搬运，尾部不足 8 字节部分依次 4/2/1 字节补齐。

use general::mm::UserAccessOps;
use mm::UserAccessError;

/// Sv48 用户空间上界（不含）。
const USER_SPACE_TOP: usize = 0x0000_8000_0000_0000;

/// SSTATUS.SUM 位（bit 18）。
const SSTATUS_SUM: usize = 1 << 18;

// ── SUM 操作 ──────────────────────────────────────────────────────────────────

#[inline(always)]
pub unsafe fn set_sum() {
    unsafe {
        core::arch::asm!(
            "li {tmp}, {sum}",
            "csrs sstatus, {tmp}",
            tmp = out(reg) _,
            sum = const SSTATUS_SUM,
            options(nostack, preserves_flags)
        );
    }
}

#[inline(always)]
pub(super) unsafe fn clear_sum() {
    unsafe {
        core::arch::asm!(
            "li {tmp}, {sum}",
            "csrc sstatus, {tmp}",
            tmp = out(reg) _,
            sum = const SSTATUS_SUM,
            options(nostack, preserves_flags)
        );
    }
}

#[inline(always)]
unsafe fn enable_sum_and_save() -> usize {
    let old: usize;
    unsafe {
        core::arch::asm!(
            "csrrs {old}, sstatus, {sum}",
            old = out(reg) old,
            sum = in(reg) SSTATUS_SUM,
            options(nostack, preserves_flags)
        );
    }
    old
}

#[inline(always)]
unsafe fn restore_sum(old_sstatus: usize) {
    if old_sstatus & SSTATUS_SUM != 0 {
        unsafe { set_sum() };
    } else {
        unsafe { clear_sum() };
    }
}

// ── 用户拷贝（分级宽度：8/4/2/1 字节） ─────────────────────────────────────

/// 从用户空间单字节安全加载。fault 时返回 Err。
/// asm 块内首条指令显式将 ok 置零，保证非 fault 路径下 faulted 寄存器有定值。
#[inline(always)]
unsafe fn load_user_u8(addr: usize) -> Result<u8, UserAccessError> {
    let val: u8;
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "li {ok}, 0",
            "2: lbu {val}, 0({ptr})",
            "j 3f",
            "4: li {ok}, 1",
            "3:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".8byte 2b, 4b",
            ".popsection",
            ptr = in(reg) addr,
            val = out(reg) val,
            ok = out(reg) faulted,
            options(nostack, readonly)
        );
    }
    if faulted != 0 {
        Err(UserAccessError::Fault)
    } else {
        Ok(val)
    }
}

/// 从用户空间双字安全加载。仅在地址 8 字节对齐时调用，避免 misaligned access trap。
#[inline(always)]
unsafe fn load_user_u64(addr: usize) -> Result<u64, UserAccessError> {
    let val: u64;
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "li {ok}, 0",
            "2: ld {val}, 0({ptr})",
            "j 3f",
            "4: li {ok}, 1",
            "3:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".8byte 2b, 4b",
            ".popsection",
            ptr = in(reg) addr,
            val = out(reg) val,
            ok = out(reg) faulted,
            options(nostack, readonly)
        );
    }
    if faulted != 0 {
        Err(UserAccessError::Fault)
    } else {
        Ok(val)
    }
}

/// 向用户空间单字节安全存储。
#[inline(always)]
unsafe fn store_user_u8(addr: usize, val: u8) -> Result<(), UserAccessError> {
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "li {ok}, 0",
            "2: sb {val}, 0({ptr})",
            "j 3f",
            "4: li {ok}, 1",
            "3:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".8byte 2b, 4b",
            ".popsection",
            ptr = in(reg) addr,
            val = in(reg) val,
            ok = out(reg) faulted,
            options(nostack)
        );
    }
    if faulted != 0 {
        Err(UserAccessError::Fault)
    } else {
        Ok(())
    }
}

/// 向用户空间双字安全存储。仅在地址 8 字节对齐时调用。
#[inline(always)]
unsafe fn store_user_u64(addr: usize, val: u64) -> Result<(), UserAccessError> {
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "li {ok}, 0",
            "2: sd {val}, 0({ptr})",
            "j 3f",
            "4: li {ok}, 1",
            "3:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".8byte 2b, 4b",
            ".popsection",
            ptr = in(reg) addr,
            val = in(reg) val,
            ok = out(reg) faulted,
            options(nostack)
        );
    }
    if faulted != 0 {
        Err(UserAccessError::Fault)
    } else {
        Ok(())
    }
}

/// 从用户空间拷贝到内核缓冲区。主循环以 64 字节展开搬运，尾部 8/1 补齐。
#[inline]
unsafe fn sum_copy_from_user(
    dst: *mut u8,
    src_user: usize,
    len: usize,
) -> Result<(), UserAccessError> {
    let old_sstatus = unsafe { enable_sum_and_save() };
    let mut i = 0usize;

    while i < len && (src_user + i) & 7 != 0 {
        let b = unsafe { load_user_u8(src_user + i) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        unsafe { dst.add(i).write(b) };
        i += 1;
    }

    while i + 8 <= len {
        let val = unsafe { load_user_u64(src_user + i) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        unsafe { (dst.add(i) as *mut u64).write_unaligned(val) };
        i += 8;
    }

    while i < len {
        let b = unsafe { load_user_u8(src_user + i) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        unsafe { dst.add(i).write(b) };
        i += 1;
    }

    unsafe { restore_sum(old_sstatus) };
    Ok(())
}

/// 从内核缓冲区拷贝到用户空间。主循环以 8 字节为单位搬运。
#[inline]
unsafe fn sum_copy_to_user(
    dst_user: usize,
    src: *const u8,
    len: usize,
) -> Result<(), UserAccessError> {
    let old_sstatus = unsafe { enable_sum_and_save() };
    let mut i = 0usize;

    while i < len && (dst_user + i) & 7 != 0 {
        let b = unsafe { src.add(i).read() };
        unsafe { store_user_u8(dst_user + i, b) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        i += 1;
    }

    while i + 8 <= len {
        let val = unsafe { (src.add(i) as *const u64).read_unaligned() };
        unsafe { store_user_u64(dst_user + i, val) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        i += 8;
    }

    while i < len {
        let b = unsafe { src.add(i).read() };
        unsafe { store_user_u8(dst_user + i, b) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        i += 1;
    }

    unsafe { restore_sum(old_sstatus) };
    Ok(())
}

// ── 对外接口 ──────────────────────────────────────────────────────────────────

#[inline]
unsafe fn copy_from_user(dst: *mut u8, src_user: usize, len: usize) -> Result<(), UserAccessError> {
    if src_user
        .checked_add(len)
        .map_or(true, |end| end > USER_SPACE_TOP)
    {
        return Err(UserAccessError::Fault);
    }
    unsafe { sum_copy_from_user(dst, src_user, len) }
}

#[inline]
unsafe fn copy_to_user(dst_user: usize, src: *const u8, len: usize) -> Result<(), UserAccessError> {
    if dst_user
        .checked_add(len)
        .map_or(true, |end| end > USER_SPACE_TOP)
    {
        return Err(UserAccessError::Fault);
    }
    unsafe { sum_copy_to_user(dst_user, src, len) }
}

unsafe fn strnlen_user(start_user: usize, max: usize) -> Result<usize, UserAccessError> {
    if start_user >= USER_SPACE_TOP {
        return Err(UserAccessError::Fault);
    }
    let effective_max = max.min(USER_SPACE_TOP - start_user);
    let mut i = 0usize;
    let old_sstatus = unsafe { enable_sum_and_save() };
    while i < effective_max {
        match unsafe { load_user_u8(start_user + i) } {
            Ok(0) => {
                unsafe { restore_sum(old_sstatus) };
                return Ok(i);
            }
            Ok(_) => {
                i += 1;
            }
            Err(e) => {
                unsafe { restore_sum(old_sstatus) };
                return Err(e);
            }
        }
    }
    unsafe { restore_sum(old_sstatus) };
    Err(UserAccessError::TooLong)
}

pub(super) static USER_ACCESS_OPS: UserAccessOps = UserAccessOps {
    copy_from_user,
    copy_to_user,
    strnlen_user,
};

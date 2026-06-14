//! RISC-V64 的 `copy_from_user` / `copy_to_user` / `strnlen_user`。
//!
//! 利用 RISC-V 的 `SSTATUS.SUM`（Supervisor User Memory access）位：
//! 置 SUM=1 后 S-mode 可直接访问 U 标记的页面，配合 `__ex_table` 实现
//! 逐字节安全拷贝。每条 load/store 指令单独挂一条 `__ex_table`，
//! fault 时 fixup 到本字节的错误返回路径。
//!
//! ## 设计
//!
//! 采用逐字节拷贝 + 单条指令 `__ex_table` 保护的模式。每个 asm 块只包含
//! 一条 `lbu`/`sb` 指令和一个 fixup label，`faulted` 标志在 Rust 侧初始化，
//! 避免在 asm 块内使用 `li {ok}, 0` 这类指令。
//! 字节间的回写走 `write_volatile`（内核 buf 永远安全）。

use mm::UserAccessError;
use general::mm::UserAccessOps;

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

// ── 用户拷贝 ──────────────────────────────────────────────────────────────────

/// 从用户空间逐字节拷贝到内核缓冲区。
#[inline(never)]
unsafe fn sum_copy_from_user(dst: *mut u8, src_user: usize, len: usize) -> Result<(), UserAccessError> {
    let mut i = 0usize;
    unsafe { set_sum() };
    while i < len {
        let ptr = (src_user + i) as *const u8;
        let b: u8;
        let mut faulted: usize = 0;
        unsafe {
            core::arch::asm!(
                "2: lbu {val}, 0({ptr})",
                "j 3f",
                "4: li {ok}, 1",
                "3:",
                ".pushsection __ex_table,\"a\"",
                ".balign 8",
                ".8byte 2b, 4b",
                ".popsection",
                ptr = in(reg) ptr,
                val = out(reg) b,
                ok = inlateout(reg) faulted,
                options(nostack, readonly)
            );
        }
        if faulted != 0 {
            unsafe { clear_sum() };
            return Err(UserAccessError::Fault);
        }
        unsafe { core::ptr::write_volatile(dst.add(i), b) };
        i += 1;
    }
    unsafe { clear_sum() };
    Ok(())
}

/// 从内核缓冲区逐字节拷贝到用户空间。
#[inline(never)]
unsafe fn sum_copy_to_user(dst_user: usize, src: *const u8, len: usize) -> Result<(), UserAccessError> {
    let mut i = 0usize;
    unsafe { set_sum() };
    while i < len {
        let b = unsafe { core::ptr::read_volatile(src.add(i)) };
        let ptr = (dst_user + i) as *mut u8;
        let mut faulted: usize = 0;
        unsafe {
            core::arch::asm!(
                "2: sb {val}, 0({ptr})",
                "j 3f",
                "4: li {ok}, 1",
                "3:",
                ".pushsection __ex_table,\"a\"",
                ".balign 8",
                ".8byte 2b, 4b",
                ".popsection",
                ptr = in(reg) ptr,
                val = in(reg) b,
                ok = inlateout(reg) faulted,
                options(nostack)
            );
        }
        if faulted != 0 {
            unsafe { clear_sum() };
            return Err(UserAccessError::Fault);
        }
        i += 1;
    }
    unsafe { clear_sum() };
    Ok(())
}

// ── 对外接口 ──────────────────────────────────────────────────────────────────

unsafe fn copy_from_user(dst: *mut u8, src_user: usize, len: usize) -> Result<(), UserAccessError> {
    if src_user.checked_add(len).map_or(true, |end| end > USER_SPACE_TOP) {
        return Err(UserAccessError::Fault);
    }
    unsafe { sum_copy_from_user(dst, src_user, len) }
}

unsafe fn copy_to_user(dst_user: usize, src: *const u8, len: usize) -> Result<(), UserAccessError> {
    if dst_user.checked_add(len).map_or(true, |end| end > USER_SPACE_TOP) {
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
    unsafe { set_sum() };
    while i < effective_max {
        let ptr = (start_user + i) as *const u8;
        let b: u8;
        let mut faulted: usize = 0;
        unsafe {
            core::arch::asm!(
                "2: lbu {val}, 0({ptr})",
                "j 3f",
                "4: li {ok}, 1",
                "3:",
                ".pushsection __ex_table,\"a\"",
                ".balign 8",
                ".8byte 2b, 4b",
                ".popsection",
                ptr = in(reg) ptr,
                val = out(reg) b,
                ok = inlateout(reg) faulted,
                options(nostack, readonly)
            );
        }
        if faulted != 0 {
            unsafe { clear_sum() };
            return Err(UserAccessError::Fault);
        }
        if b == 0 {
            unsafe { clear_sum() };
            return Ok(i);
        }
        i += 1;
    }
    unsafe { clear_sum() };
    Ok(effective_max)
}

pub(super) static USER_ACCESS_OPS: UserAccessOps = UserAccessOps {
    copy_from_user,
    copy_to_user,
    strnlen_user,
};

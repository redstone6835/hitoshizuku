//! RISC-V64 的 `copy_from_user` / `copy_to_user` / `strnlen_user`。
//!
//! 利用 RISC-V 的 `SSTATUS.SUM`（Supervisor User Memory access）位：
//! 置 SUM=1 后 S-mode 可直接访问 U 标记的页面，配合 `__ex_table` 实现
//! 安全拷贝。采用分级宽度策略：先对齐到 8 字节边界，主循环走 `ld`/`sd`
//! 双字搬运，尾部不足 8 字节部分依次 4/2/1 字节补齐。

use general::mm::UserAccessOps;
use mm::UserAccessError;

#[inline]
fn user_space_top() -> usize {
    crate::riscv64::paging::active_paging_mode().user_space_top()
}

/// SSTATUS.SUM 位（bit 18）。
const SSTATUS_SUM: usize = 1 << 18;
/// SSTATUS.MXR 位（bit 19）：允许 load 读取 execute-only 页面。
const SSTATUS_MXR: usize = 1 << 19;

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
    // enable_sum_and_save() 已经保证当前 SUM=1；原状态同样为 1 时无需重复写 CSR。
    if old_sstatus & SSTATUS_SUM == 0 {
        unsafe { clear_sum() };
    }
}

#[inline(always)]
unsafe fn enable_mxr_and_save() -> usize {
    let old: usize;
    unsafe {
        core::arch::asm!(
            "csrrs {old}, sstatus, {mxr}",
            old = out(reg) old,
            mxr = in(reg) SSTATUS_MXR,
            options(nostack, preserves_flags)
        );
    }
    old
}

#[inline(always)]
unsafe fn restore_mxr(old_sstatus: usize) {
    if old_sstatus & SSTATUS_MXR == 0 {
        crate::clear_csr!(sstatus, SSTATUS_MXR);
    }
}

// ── 用户拷贝（分级宽度：8/4/2/1 字节） ─────────────────────────────────────

macro_rules! define_user_load {
    ($(#[$meta:meta])* $name:ident, $value_ty:ty, $instruction:literal) => {
        $(#[$meta])*
        #[inline(always)]
        unsafe fn $name(addr: usize) -> Result<$value_ty, UserAccessError> {
            let value: $value_ty;
            let faulted: usize;
            unsafe {
                core::arch::asm!(
                    "li {ok}, 0",
                    $instruction,
                    "j 3f",
                    "4: li {ok}, 1",
                    "3:",
                    ".pushsection __ex_table,\"a\"",
                    ".balign 8",
                    ".8byte 2b, 4b",
                    ".popsection",
                    ptr = in(reg) addr,
                    value = out(reg) value,
                    ok = out(reg) faulted,
                    options(nostack, readonly)
                );
            }
            if faulted != 0 {
                Err(UserAccessError::Fault)
            } else {
                Ok(value)
            }
        }
    };
}

macro_rules! define_user_store {
    ($(#[$meta:meta])* $name:ident, $value_ty:ty, $instruction:literal) => {
        $(#[$meta])*
        #[inline(always)]
        unsafe fn $name(addr: usize, value: $value_ty) -> Result<(), UserAccessError> {
            let faulted: usize;
            unsafe {
                core::arch::asm!(
                    "li {ok}, 0",
                    $instruction,
                    "j 3f",
                    "4: li {ok}, 1",
                    "3:",
                    ".pushsection __ex_table,\"a\"",
                    ".balign 8",
                    ".8byte 2b, 4b",
                    ".popsection",
                    ptr = in(reg) addr,
                    value = in(reg) value,
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
    };
}

define_user_load!(
    /// 从用户空间单字节安全加载。fault 时返回 Err。
    /// asm 块内首条指令显式将 ok 置零，保证非 fault 路径下 faulted 寄存器有定值。
    load_user_u8,
    u8,
    "2: lbu {value}, 0({ptr})"
);

define_user_load!(
    /// 从用户空间半字安全加载。仅在地址 2 字节对齐时调用。
    load_user_u16,
    u16,
    "2: lhu {value}, 0({ptr})"
);

define_user_load!(
    /// 从用户空间字安全加载。仅在地址 4 字节对齐时调用。
    load_user_u32,
    u32,
    "2: lwu {value}, 0({ptr})"
);

define_user_load!(
    /// 从用户空间双字安全加载。仅在地址 8 字节对齐时调用，避免 misaligned access trap。
    load_user_u64,
    u64,
    "2: ld {value}, 0({ptr})"
);

define_user_store!(
    /// 向用户空间单字节安全存储。
    store_user_u8,
    u8,
    "2: sb {value}, 0({ptr})"
);

define_user_store!(
    /// 向用户空间半字安全存储。仅在地址 2 字节对齐时调用。
    store_user_u16,
    u16,
    "2: sh {value}, 0({ptr})"
);

define_user_store!(
    /// 向用户空间字安全存储。仅在地址 4 字节对齐时调用。
    store_user_u32,
    u32,
    "2: sw {value}, 0({ptr})"
);

define_user_store!(
    /// 向用户空间双字安全存储。仅在地址 8 字节对齐时调用。
    store_user_u64,
    u64,
    "2: sd {value}, 0({ptr})"
);

/// 从用户空间拷贝到内核缓冲区。主循环以 8 字节为单位搬运，尾部按 4/2/1 补齐。
#[inline(never)]
unsafe fn sum_copy_from_user(
    dst: *mut u8,
    src_user: usize,
    len: usize,
) -> Result<(), UserAccessError> {
    let old_sstatus = unsafe { enable_sum_and_save() };
    let mut i = 0usize;

    // 头部：对齐到 8 字节边界
    while i < len && (src_user + i) & 7 != 0 {
        let b = unsafe { load_user_u8(src_user + i) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        unsafe { dst.add(i).write(b) };
        i += 1;
    }

    // 主循环：一次展开 4 个双字，减少 Rust 循环分支和索引更新。
    while i + 32 <= len {
        let v0 = unsafe { load_user_u64(src_user + i) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        let v1 = unsafe { load_user_u64(src_user + i + 8) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        let v2 = unsafe { load_user_u64(src_user + i + 16) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        let v3 = unsafe { load_user_u64(src_user + i + 24) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        unsafe {
            (dst.add(i) as *mut u64).write_unaligned(v0);
            (dst.add(i + 8) as *mut u64).write_unaligned(v1);
            (dst.add(i + 16) as *mut u64).write_unaligned(v2);
            (dst.add(i + 24) as *mut u64).write_unaligned(v3);
        }
        i += 32;
    }

    // 剩余主体：8 字节双字搬运
    while i + 8 <= len {
        let val = unsafe { load_user_u64(src_user + i) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        unsafe { (dst.add(i) as *mut u64).write_unaligned(val) };
        i += 8;
    }

    // 尾部：优先使用对齐的 4/2 字节搬运，减少小结构体拷贝中的 asm/fixup 次数。
    if i + 4 <= len && (src_user + i) & 3 == 0 {
        let val = unsafe { load_user_u32(src_user + i) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        unsafe { (dst.add(i) as *mut u32).write_unaligned(val) };
        i += 4;
    }
    if i + 2 <= len && (src_user + i) & 1 == 0 {
        let val = unsafe { load_user_u16(src_user + i) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        unsafe { (dst.add(i) as *mut u16).write_unaligned(val) };
        i += 2;
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
#[inline(never)]
unsafe fn sum_copy_to_user(
    dst_user: usize,
    src: *const u8,
    len: usize,
) -> Result<(), UserAccessError> {
    let old_sstatus = unsafe { enable_sum_and_save() };
    let mut i = 0usize;

    // 头部：对齐到 8 字节边界
    while i < len && (dst_user + i) & 7 != 0 {
        let b = unsafe { src.add(i).read() };
        unsafe { store_user_u8(dst_user + i, b) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        i += 1;
    }

    // 主循环：一次展开 4 个双字。
    while i + 32 <= len {
        let v0 = unsafe { (src.add(i) as *const u64).read_unaligned() };
        let v1 = unsafe { (src.add(i + 8) as *const u64).read_unaligned() };
        let v2 = unsafe { (src.add(i + 16) as *const u64).read_unaligned() };
        let v3 = unsafe { (src.add(i + 24) as *const u64).read_unaligned() };
        unsafe { store_user_u64(dst_user + i, v0) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        unsafe { store_user_u64(dst_user + i + 8, v1) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        unsafe { store_user_u64(dst_user + i + 16, v2) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        unsafe { store_user_u64(dst_user + i + 24, v3) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        i += 32;
    }

    // 剩余主体：8 字节双字搬运
    while i + 8 <= len {
        let val = unsafe { (src.add(i) as *const u64).read_unaligned() };
        unsafe { store_user_u64(dst_user + i, val) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        i += 8;
    }

    // 尾部：与 copy_from_user 对称，优先使用对齐的 4/2 字节存储。
    if i + 4 <= len && (dst_user + i) & 3 == 0 {
        let val = unsafe { (src.add(i) as *const u32).read_unaligned() };
        unsafe { store_user_u32(dst_user + i, val) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        i += 4;
    }
    if i + 2 <= len && (dst_user + i) & 1 == 0 {
        let val = unsafe { (src.add(i) as *const u16).read_unaligned() };
        unsafe { store_user_u16(dst_user + i, val) }.map_err(|e| {
            unsafe { restore_sum(old_sstatus) };
            e
        })?;
        i += 2;
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

unsafe fn copy_from_user(dst: *mut u8, src_user: usize, len: usize) -> Result<(), UserAccessError> {
    if src_user
        .checked_add(len)
        .map_or(true, |end| end > user_space_top())
    {
        return Err(UserAccessError::Fault);
    }
    unsafe { sum_copy_from_user(dst, src_user, len) }
}

/// 从用户可执行映射安全取指。
///
/// 普通 `copy_from_user` 必须遵循页面 R 权限；非法指令、Vector/FPU lazy decoder
/// 和 syscall-PC 校验还需要读取 execute-only 页面，因此仅在这段受 `__ex_table`
/// 保护的窗口临时设置 MXR，并在成功或 fault 后恢复原状态。
pub(crate) fn copy_instruction_from_user(
    src_user: usize,
    dst: &mut [u8],
) -> Result<(), UserAccessError> {
    let old_sstatus = unsafe { enable_mxr_and_save() };
    let result = unsafe { copy_from_user(dst.as_mut_ptr(), src_user, dst.len()) };
    unsafe { restore_mxr(old_sstatus) };
    result
}

unsafe fn copy_to_user(dst_user: usize, src: *const u8, len: usize) -> Result<(), UserAccessError> {
    if dst_user
        .checked_add(len)
        .map_or(true, |end| end > user_space_top())
    {
        return Err(UserAccessError::Fault);
    }
    unsafe { sum_copy_to_user(dst_user, src, len) }
}

unsafe fn strnlen_user(start_user: usize, max: usize) -> Result<usize, UserAccessError> {
    let user_space_top = user_space_top();
    if start_user >= user_space_top {
        return Err(UserAccessError::Fault);
    }
    let effective_max = max.min(user_space_top - start_user);
    let mut i = 0usize;
    let old_sstatus = unsafe { enable_sum_and_save() };

    // 先对齐到 8 字节；页大小同样按 8 对齐，之后的 ld 不会跨页形成额外误 fault。
    while i < effective_max && (start_user + i) & 7 != 0 {
        match unsafe { load_user_u8(start_user + i) } {
            Ok(0) => {
                unsafe { restore_sum(old_sstatus) };
                return Ok(i);
            }
            Ok(_) => i += 1,
            Err(e) => {
                unsafe { restore_sum(old_sstatus) };
                return Err(e);
            }
        }
    }

    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    while i + 8 <= effective_max {
        let word = match unsafe { load_user_u64(start_user + i) } {
            Ok(word) => word,
            Err(e) => {
                unsafe { restore_sum(old_sstatus) };
                return Err(e);
            }
        };
        if word.wrapping_sub(ONES) & !word & HIGHS != 0 {
            for (offset, byte) in word.to_le_bytes().into_iter().enumerate() {
                if byte == 0 {
                    unsafe { restore_sum(old_sstatus) };
                    return Ok(i + offset);
                }
            }
        }
        i += 8;
    }

    while i < effective_max {
        // 复用统一的 load_user_u8 helper，保证 fault 标志路径与 copy_from_user 一致。
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

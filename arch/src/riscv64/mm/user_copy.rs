//! RISC-V64 的 `copy_from_user` / `copy_to_user` / `strnlen_user`。
//!
//! 利用 RISC-V 的 `SSTATUS.SUM`（Supervisor User Memory access）位：
//! 置 SUM=1 后 S-mode 可直接访问 U 标记的页面，配合 `__ex_table` 实现
//! 高性能批量拷贝。fault 时由 fault_decode 改写 sepc 到 fixup label。
//!
//! ## 性能
//!
//! 拷贝采用对齐优化策略：头部逐字节对齐到 8 字节边界 → 中间 8 字节 ld/sd
//! 批量搬运 → 尾部逐字节收尾。相比逐字节方案，批量段吞吐提升约 4-8x。

use mm::UserAccessError;
use general::mm::UserAccessOps;

/// Sv48 用户空间上界（不含）。
const USER_SPACE_TOP: usize = 0x0000_8000_0000_0000;

/// SSTATUS.SUM 位（bit 18）。
const SSTATUS_SUM: usize = 1 << 18;

// ── SUM 操作 ──────────────────────────────────────────────────────────────────

/// 置 SSTATUS.SUM=1，允许 S-mode 直接访问 U=1 的用户页面。
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

/// 清 SSTATUS.SUM=0，恢复 S-mode 对 U 页面的访问保护。
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

// ── 批量拷贝（SUM 保护下） ────────────────────────────────────────────────────
//
// 使用 inline asm 实现 memcpy-like 循环，整个循环体挂一条 ex_table。
// fault 时跳到 fixup label 返回错误。

/// SUM 保护下从用户空间批量拷贝到内核。
/// 头部逐字节对齐到 8 字节 → 中间 8 字节批量 ld/sd → 尾部逐字节。
/// 每段各挂 ex_table，fault 时返回 Err。
#[inline(never)]
unsafe fn sum_copy_from_user(dst: *mut u8, src: usize, len: usize) -> Result<(), UserAccessError> {
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "li {ok}, 0",
            "beqz {len}, 99f",

            // ── 头部：逐字节对齐 src 到 8 字节边界 ──
            "10: andi {tmp}, {src}, 7",
            "beqz {tmp}, 20f",
            "11: lbu {tmp}, 0({src})",
            "sb {tmp}, 0({dst})",
            "addi {src}, {src}, 1",
            "addi {dst}, {dst}, 1",
            "addi {len}, {len}, -1",
            "beqz {len}, 99f",
            "andi {tmp}, {src}, 7",
            "bnez {tmp}, 11b",

            // ── 中间：8 字节批量拷贝 ──
            "20: li {tmp}, 8",
            "bltu {len}, {tmp}, 30f",
            "21: ld {tmp}, 0({src})",
            "sd {tmp}, 0({dst})",
            "addi {src}, {src}, 8",
            "addi {dst}, {dst}, 8",
            "addi {len}, {len}, -8",
            "li {tmp}, 8",
            "bgeu {len}, {tmp}, 21b",

            // ── 尾部：逐字节收尾 ──
            "30: beqz {len}, 99f",
            "31: lbu {tmp}, 0({src})",
            "sb {tmp}, 0({dst})",
            "addi {src}, {src}, 1",
            "addi {dst}, {dst}, 1",
            "addi {len}, {len}, -1",
            "bnez {len}, 31b",
            "j 99f",

            // ── fault fixup ──
            "90: li {ok}, 1",
            "99:",

            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".8byte 11b, 90b",
            ".8byte 21b, 90b",
            ".8byte 31b, 90b",
            ".popsection",

            src = inout(reg) src => _,
            dst = inout(reg) dst => _,
            len = inout(reg) len => _,
            tmp = lateout(reg) _,
            ok = lateout(reg) faulted,
            options(nostack)
        );
    }
    if faulted == 0 { Ok(()) } else { Err(UserAccessError::Fault) }
}

/// SUM 保护下从内核批量拷贝到用户空间。
/// 头部逐字节对齐 dst 到 8 字节 → 中间 8 字节批量 → 尾部逐字节。
#[inline(never)]
unsafe fn sum_copy_to_user(dst: usize, src: *const u8, len: usize) -> Result<(), UserAccessError> {
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "li {ok}, 0",
            "beqz {len}, 99f",

            // ── 头部：逐字节对齐 dst 到 8 字节边界 ──
            "10: andi {tmp}, {dst}, 7",
            "beqz {tmp}, 20f",
            "11: lbu {tmp}, 0({src})",
            "sb {tmp}, 0({dst})",
            "addi {src}, {src}, 1",
            "addi {dst}, {dst}, 1",
            "addi {len}, {len}, -1",
            "beqz {len}, 99f",
            "andi {tmp}, {dst}, 7",
            "bnez {tmp}, 11b",

            // ── 中间：8 字节批量拷贝 ──
            "20: li {tmp}, 8",
            "bltu {len}, {tmp}, 30f",
            "21: ld {tmp}, 0({src})",
            "sd {tmp}, 0({dst})",
            "addi {src}, {src}, 8",
            "addi {dst}, {dst}, 8",
            "addi {len}, {len}, -8",
            "li {tmp}, 8",
            "bgeu {len}, {tmp}, 21b",

            // ── 尾部：逐字节收尾 ──
            "30: beqz {len}, 99f",
            "31: lbu {tmp}, 0({src})",
            "sb {tmp}, 0({dst})",
            "addi {src}, {src}, 1",
            "addi {dst}, {dst}, 1",
            "addi {len}, {len}, -1",
            "bnez {len}, 31b",
            "j 99f",

            // ── fault fixup ──
            "90: li {ok}, 1",
            "99:",

            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".8byte 11b, 90b",
            ".8byte 21b, 90b",
            ".8byte 31b, 90b",
            ".popsection",

            src = inout(reg) src => _,
            dst = inout(reg) dst => _,
            len = inout(reg) len => _,
            tmp = lateout(reg) _,
            ok = lateout(reg) faulted,
            options(nostack)
        );
    }
    if faulted == 0 { Ok(()) } else { Err(UserAccessError::Fault) }
}

// ── 公开接口 ──────────────────────────────────────────────────────────────────

unsafe fn copy_from_user(dst: *mut u8, src_user: usize, len: usize) -> Result<(), UserAccessError> {
    if src_user.checked_add(len).map_or(true, |end| end > USER_SPACE_TOP) {
        return Err(UserAccessError::Fault);
    }
    unsafe { set_sum() };
    let r = unsafe { sum_copy_from_user(dst, src_user, len) };
    unsafe { clear_sum() };
    r
}

unsafe fn copy_to_user(dst_user: usize, src: *const u8, len: usize) -> Result<(), UserAccessError> {
    if dst_user.checked_add(len).map_or(true, |end| end > USER_SPACE_TOP) {
        return Err(UserAccessError::Fault);
    }
    unsafe { set_sum() };
    let r = unsafe { sum_copy_to_user(dst_user, src, len) };
    unsafe { clear_sum() };
    r
}

unsafe fn strnlen_user(start_user: usize, max: usize) -> Result<usize, UserAccessError> {
    // 起始地址必须在用户空间内
    if start_user >= USER_SPACE_TOP {
        return Err(UserAccessError::Fault);
    }
    // clamp max 到地址空间边界，避免 start + max 溢出或越界判定为非法
    let effective_max = max.min(USER_SPACE_TOP - start_user);
    let mut i = 0usize;
    unsafe { set_sum() };
    while i < effective_max {
        let b: u8;
        let faulted: usize;
        unsafe {
            core::arch::asm!(
                "li {ok}, 0",
                "2: lbu {val}, 0({addr})",
                "j 5f", "3: li {ok}, 1", "5:",
                ".pushsection __ex_table,\"a\"", ".balign 8",
                ".8byte 2b, 3b", ".popsection",
                addr = in(reg) start_user + i,
                val = lateout(reg) b,
                ok = lateout(reg) faulted,
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

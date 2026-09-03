//! x86_64 用户缓冲区访问原语。
//!
//! 用户访问必须经过这里，不能在通用 syscall 实现里直接解引用用户指针。
//! 裸机路径使用 SMAP 的 STAC/CLAC 窗口和 `__ex_table` fixup；hosted 路径只
//! 提供无特权的测试回退，并在进入拷贝前拒绝非 canonical/内核地址。

#[cfg(target_os = "none")]
use core::arch::x86_64::__cpuid_count;

use general::mm::UserAccessOps;
use mm::UserAccessError;

/// LA48 用户地址空间上界（不含）。LA57 尚未作为用户 ABI 发布。
pub const USER_SPACE_TOP: usize = 0x0000_8000_0000_0000;

#[inline]
fn valid_user_range(start: usize, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let Some(end) = start.checked_add(len) else {
        return false;
    };
    start < USER_SPACE_TOP && end <= USER_SPACE_TOP
}

#[cfg(target_os = "none")]
#[inline]
fn smap_supported() -> bool {
    let max_basic = __cpuid_count(0, 0).eax;
    if max_basic < 7 {
        return false;
    }
    // CPUID.(EAX=7,ECX=0):EBX.SMAP (bit 20).
    __cpuid_count(7, 0).ebx & (1 << 20) != 0
}

#[cfg(target_os = "none")]
#[inline]
unsafe fn stac_if_supported() {
    if smap_supported() {
        // Safety: caller is in CPL0 and bounds-checked the user range.
        // Linux's STAC/CLAC helpers include a compiler barrier.  Without the
        // memory clobber, a user load could be moved before STAC or after CLAC.
        // STAC changes EFLAGS.AC, so it must not be declared
        // `preserves_flags`.
        unsafe { core::arch::asm!("stac", options(nostack)) };
    }
}

#[cfg(target_os = "none")]
#[inline]
unsafe fn clac_if_supported() {
    if smap_supported() {
        // Safety: pairs with stac_if_supported in the same non-preemptible copy.
        // CLAC likewise changes EFLAGS.AC.
        unsafe { core::arch::asm!("clac", options(nostack)) };
    }
}

/// Copy bytes from a user address.  The exception table maps a fault in the
/// `rep movsb` instruction to the local fixup label, which returns `Fault` after
/// CLAC restores SMAP state.
#[cfg(target_os = "none")]
#[inline(never)]
unsafe fn copy_from_user_raw(dst: *mut u8, src: usize, len: usize) -> Result<(), UserAccessError> {
    if len == 0 {
        return Ok(());
    }
    unsafe { stac_if_supported() };
    let dst_reg = dst as usize;
    let src_reg = src;
    let count = len;
    let mut fault: u32;
    unsafe {
        core::arch::asm!(
            "xor {fault:e}, {fault:e}",
            "2: rep movsb",
            "jmp 4f",
            "3: mov {fault:e}, 1",
            "4:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".8byte 2b, 3b",
            ".popsection",
            inout("rdi") dst_reg => _,
            inout("rsi") src_reg => _,
            inout("rcx") count => _,
            fault = lateout(reg) fault,
            options(nostack)
        );
    }
    unsafe { clac_if_supported() };
    if fault != 0 {
        Err(UserAccessError::Fault)
    } else {
        Ok(())
    }
}

/// Hosted copy fallback.  It intentionally does not attempt to catch a host
/// SIGSEGV; callers/tests must pass a valid process pointer after range checks.
#[cfg(not(target_os = "none"))]
#[inline(never)]
unsafe fn copy_from_user_raw(dst: *mut u8, src: usize, len: usize) -> Result<(), UserAccessError> {
    if len != 0 {
        unsafe { core::ptr::copy_nonoverlapping(src as *const u8, dst, len) };
    }
    Ok(())
}

#[cfg(target_os = "none")]
#[inline(never)]
unsafe fn copy_to_user_raw(dst: usize, src: *const u8, len: usize) -> Result<(), UserAccessError> {
    if len == 0 {
        return Ok(());
    }
    unsafe { stac_if_supported() };
    let dst_reg = dst;
    let src_reg = src as usize;
    let count = len;
    let mut fault: u32;
    unsafe {
        core::arch::asm!(
            "xor {fault:e}, {fault:e}",
            "2: rep movsb",
            "jmp 4f",
            "3: mov {fault:e}, 1",
            "4:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".8byte 2b, 3b",
            ".popsection",
            inout("rdi") dst_reg => _,
            inout("rsi") src_reg => _,
            inout("rcx") count => _,
            fault = lateout(reg) fault,
            options(nostack)
        );
    }
    unsafe { clac_if_supported() };
    if fault != 0 {
        Err(UserAccessError::Fault)
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "none"))]
#[inline(never)]
unsafe fn copy_to_user_raw(dst: usize, src: *const u8, len: usize) -> Result<(), UserAccessError> {
    if len != 0 {
        unsafe { core::ptr::copy_nonoverlapping(src, dst as *mut u8, len) };
    }
    Ok(())
}

unsafe fn copy_from_user(dst: *mut u8, src_user: usize, len: usize) -> Result<(), UserAccessError> {
    if !valid_user_range(src_user, len) {
        return Err(UserAccessError::Fault);
    }
    unsafe { copy_from_user_raw(dst, src_user, len) }
}

unsafe fn copy_to_user(dst_user: usize, src: *const u8, len: usize) -> Result<(), UserAccessError> {
    if !valid_user_range(dst_user, len) {
        return Err(UserAccessError::Fault);
    }
    unsafe { copy_to_user_raw(dst_user, src, len) }
}

unsafe fn strnlen_user(start_user: usize, max: usize) -> Result<usize, UserAccessError> {
    if start_user >= USER_SPACE_TOP {
        return Err(UserAccessError::Fault);
    }
    let effective_max = max.min(USER_SPACE_TOP - start_user);
    let mut index = 0usize;
    while index < effective_max {
        let mut byte = 0u8;
        unsafe { copy_from_user(&mut byte, start_user + index, 1)? };
        if byte == 0 {
            return Ok(index);
        }
        index += 1;
    }
    Err(UserAccessError::TooLong)
}

pub(super) static USER_ACCESS_OPS: UserAccessOps = UserAccessOps {
    copy_from_user,
    copy_to_user,
    strnlen_user,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_validation_rejects_kernel_and_overflow() {
        assert!(valid_user_range(0x1000, 0x1000));
        assert!(!valid_user_range(USER_SPACE_TOP, 1));
        assert!(!valid_user_range(USER_SPACE_TOP - 1, 2));
        assert!(!valid_user_range(usize::MAX - 1, 4));
        assert!(valid_user_range(0, 0));
    }

    #[test]
    fn hosted_copy_and_string_length() {
        let source = *b"x86\0tail";
        let mut target = [0u8; 8];
        unsafe { copy_from_user(target.as_mut_ptr(), source.as_ptr() as usize, source.len()) }
            .unwrap();
        assert_eq!(&target, &source);
        assert_eq!(
            unsafe { strnlen_user(source.as_ptr() as usize, source.len()) }.unwrap(),
            3
        );
    }
}

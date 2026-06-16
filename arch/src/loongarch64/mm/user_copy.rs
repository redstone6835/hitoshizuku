//! LoongArch64 的 `copy_from_user` / `copy_to_user` / `strnlen_user`。
//!
//! 上层通过 [`general::mm::UserAccessOps`] 调用这里。本文件**唯一对外符号**
//! 是 `static USER_ACCESS_OPS`，由同级 `mm::register()` 注入 general。
//!
//! 每个用户态 load/store 都在 `__ex_table` 注册 fault PC 和 fixup PC。若内核态
//! 访问用户 buffer 缺页，fault dispatcher 会改写 TrapFrame.pc 到 fixup 分支，
//! 当前 helper 就能把这次访问归约为 `Err(UserAccessError::Fault)`。

use mm::UserAccessError;

use general::mm::UserAccessOps;

/// 当前 LoongArch64 用户虚拟地址上界（不含）。
const USER_SPACE_TOP: usize = 0x0000_8000_0000_0000;

#[inline]
fn user_range_within(addr: usize, len: usize) -> bool {
    addr.checked_add(len)
        .is_some_and(|end| end <= USER_SPACE_TOP)
}

#[inline(always)]
unsafe fn load_user_u8(src_user: usize) -> Result<u8, UserAccessError> {
    let value: usize;
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "ori {faulted}, $zero, 0",
            "2:",
            "ld.bu {value}, {src}, 0",
            "b 5f",
            "4:",
            "ori {faulted}, $zero, 1",
            "5:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".quad 2b, 4b",
            ".popsection",
            src = in(reg) src_user,
            value = lateout(reg) value,
            faulted = out(reg) faulted,
            options(nostack)
        );
    }
    if faulted == 0 {
        Ok(value as u8)
    } else {
        Err(UserAccessError::Fault)
    }
}

#[inline(always)]
unsafe fn load_user_u16(src_user: usize) -> Result<u16, UserAccessError> {
    let value: usize;
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "ori {faulted}, $zero, 0",
            "2:",
            "ld.hu {value}, {src}, 0",
            "b 5f",
            "4:",
            "ori {faulted}, $zero, 1",
            "5:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".quad 2b, 4b",
            ".popsection",
            src = in(reg) src_user,
            value = lateout(reg) value,
            faulted = out(reg) faulted,
            options(nostack)
        );
    }
    if faulted == 0 {
        Ok(value as u16)
    } else {
        Err(UserAccessError::Fault)
    }
}

#[inline(always)]
unsafe fn load_user_u32(src_user: usize) -> Result<u32, UserAccessError> {
    let value: usize;
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "ori {faulted}, $zero, 0",
            "2:",
            "ld.wu {value}, {src}, 0",
            "b 5f",
            "4:",
            "ori {faulted}, $zero, 1",
            "5:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".quad 2b, 4b",
            ".popsection",
            src = in(reg) src_user,
            value = lateout(reg) value,
            faulted = out(reg) faulted,
            options(nostack)
        );
    }
    if faulted == 0 {
        Ok(value as u32)
    } else {
        Err(UserAccessError::Fault)
    }
}

#[inline(always)]
unsafe fn load_user_u64(src_user: usize) -> Result<u64, UserAccessError> {
    let value: usize;
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "ori {faulted}, $zero, 0",
            "2:",
            "ld.d {value}, {src}, 0",
            "b 5f",
            "4:",
            "ori {faulted}, $zero, 1",
            "5:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".quad 2b, 4b",
            ".popsection",
            src = in(reg) src_user,
            value = lateout(reg) value,
            faulted = out(reg) faulted,
            options(nostack)
        );
    }
    if faulted == 0 {
        Ok(value as u64)
    } else {
        Err(UserAccessError::Fault)
    }
}

#[inline(always)]
unsafe fn store_user_u8(dst_user: usize, byte: u8) -> Result<(), UserAccessError> {
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "ori {faulted}, $zero, 0",
            "2:",
            "st.b {byte}, {dst}, 0",
            "b 5f",
            "4:",
            "ori {faulted}, $zero, 1",
            "5:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".quad 2b, 4b",
            ".popsection",
            dst = in(reg) dst_user,
            byte = in(reg) byte as usize,
            faulted = out(reg) faulted,
            options(nostack)
        );
    }
    if faulted == 0 {
        Ok(())
    } else {
        Err(UserAccessError::Fault)
    }
}

#[inline(always)]
unsafe fn store_user_u16(dst_user: usize, value: u16) -> Result<(), UserAccessError> {
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "ori {faulted}, $zero, 0",
            "2:",
            "st.h {value}, {dst}, 0",
            "b 5f",
            "4:",
            "ori {faulted}, $zero, 1",
            "5:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".quad 2b, 4b",
            ".popsection",
            dst = in(reg) dst_user,
            value = in(reg) value as usize,
            faulted = out(reg) faulted,
            options(nostack)
        );
    }
    if faulted == 0 {
        Ok(())
    } else {
        Err(UserAccessError::Fault)
    }
}

#[inline(always)]
unsafe fn store_user_u32(dst_user: usize, value: u32) -> Result<(), UserAccessError> {
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "ori {faulted}, $zero, 0",
            "2:",
            "st.w {value}, {dst}, 0",
            "b 5f",
            "4:",
            "ori {faulted}, $zero, 1",
            "5:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".quad 2b, 4b",
            ".popsection",
            dst = in(reg) dst_user,
            value = in(reg) value as usize,
            faulted = out(reg) faulted,
            options(nostack)
        );
    }
    if faulted == 0 {
        Ok(())
    } else {
        Err(UserAccessError::Fault)
    }
}

#[inline(always)]
unsafe fn store_user_u64(dst_user: usize, value: u64) -> Result<(), UserAccessError> {
    let faulted: usize;
    unsafe {
        core::arch::asm!(
            "ori {faulted}, $zero, 0",
            "2:",
            "st.d {value}, {dst}, 0",
            "b 5f",
            "4:",
            "ori {faulted}, $zero, 1",
            "5:",
            ".pushsection __ex_table,\"a\"",
            ".balign 8",
            ".quad 2b, 4b",
            ".popsection",
            dst = in(reg) dst_user,
            value = in(reg) value as usize,
            faulted = out(reg) faulted,
            options(nostack)
        );
    }
    if faulted == 0 {
        Ok(())
    } else {
        Err(UserAccessError::Fault)
    }
}

/// 带 `__ex_table` fixup 的用户缓冲读取。
///
/// # Safety
/// `dst` 必须指向 `len` 字节可写内核内存；`src_user` 可以是任意用户值。
unsafe fn copy_from_user(dst: *mut u8, src_user: usize, len: usize) -> Result<(), UserAccessError> {
    if !user_range_within(src_user, len) {
        return Err(UserAccessError::Fault);
    }

    let mut copied = 0usize;

    // 热路径按机器字批量搬运；只有用户地址满足对应对齐时使用宽 load，
    // 避免 LoongArch 非对齐访问陷入异常。内核侧用 write_unaligned 接受任意切片地址。
    while copied < len && (src_user.wrapping_add(copied) & 7) != 0 {
        let user_addr = src_user.checked_add(copied).ok_or(UserAccessError::Fault)?;
        let byte = unsafe { load_user_u8(user_addr)? };
        unsafe { core::ptr::write(dst.add(copied), byte) };
        copied += 1;
    }
    while len - copied >= 8 {
        let user_addr = src_user.checked_add(copied).ok_or(UserAccessError::Fault)?;
        let value = unsafe { load_user_u64(user_addr)? };
        unsafe { core::ptr::write_unaligned(dst.add(copied) as *mut u64, value) };
        copied += 8;
    }
    if len - copied >= 4 && (src_user.wrapping_add(copied) & 3) == 0 {
        let user_addr = src_user.checked_add(copied).ok_or(UserAccessError::Fault)?;
        let value = unsafe { load_user_u32(user_addr)? };
        unsafe { core::ptr::write_unaligned(dst.add(copied) as *mut u32, value) };
        copied += 4;
    }
    if len - copied >= 2 && (src_user.wrapping_add(copied) & 1) == 0 {
        let user_addr = src_user.checked_add(copied).ok_or(UserAccessError::Fault)?;
        let value = unsafe { load_user_u16(user_addr)? };
        unsafe { core::ptr::write_unaligned(dst.add(copied) as *mut u16, value) };
        copied += 2;
    }
    while copied < len {
        let user_addr = src_user.checked_add(copied).ok_or(UserAccessError::Fault)?;
        let byte = unsafe { load_user_u8(user_addr)? };
        unsafe { core::ptr::write(dst.add(copied), byte) };
        copied += 1;
    }
    Ok(())
}

/// # Safety
/// 对偶 of [`copy_from_user`]；`src` 必须指向 `len` 字节可读内核内存。
unsafe fn copy_to_user(dst_user: usize, src: *const u8, len: usize) -> Result<(), UserAccessError> {
    if !user_range_within(dst_user, len) {
        return Err(UserAccessError::Fault);
    }

    let mut copied = 0usize;

    // 与 copy_from_user 对称：用户地址对齐后走宽 store，内核源地址允许非对齐读取。
    while copied < len && (dst_user.wrapping_add(copied) & 7) != 0 {
        let byte = unsafe { core::ptr::read(src.add(copied)) };
        let user_addr = dst_user.checked_add(copied).ok_or(UserAccessError::Fault)?;
        unsafe { store_user_u8(user_addr, byte)? };
        copied += 1;
    }
    while len - copied >= 8 {
        let value = unsafe { core::ptr::read_unaligned(src.add(copied) as *const u64) };
        let user_addr = dst_user.checked_add(copied).ok_or(UserAccessError::Fault)?;
        unsafe { store_user_u64(user_addr, value)? };
        copied += 8;
    }
    if len - copied >= 4 && (dst_user.wrapping_add(copied) & 3) == 0 {
        let value = unsafe { core::ptr::read_unaligned(src.add(copied) as *const u32) };
        let user_addr = dst_user.checked_add(copied).ok_or(UserAccessError::Fault)?;
        unsafe { store_user_u32(user_addr, value)? };
        copied += 4;
    }
    if len - copied >= 2 && (dst_user.wrapping_add(copied) & 1) == 0 {
        let value = unsafe { core::ptr::read_unaligned(src.add(copied) as *const u16) };
        let user_addr = dst_user.checked_add(copied).ok_or(UserAccessError::Fault)?;
        unsafe { store_user_u16(user_addr, value)? };
        copied += 2;
    }
    while copied < len {
        let byte = unsafe { core::ptr::read(src.add(copied)) };
        let user_addr = dst_user.checked_add(copied).ok_or(UserAccessError::Fault)?;
        unsafe { store_user_u8(user_addr, byte)? };
        copied += 1;
    }
    Ok(())
}

/// # Safety
/// `start_user` 可以是任意用户值；扫到 NUL 或 max 为止。
unsafe fn strnlen_user(start_user: usize, max: usize) -> Result<usize, UserAccessError> {
    if start_user >= USER_SPACE_TOP {
        return Err(UserAccessError::Fault);
    }
    let max = max.min(USER_SPACE_TOP - start_user);
    let mut i = 0usize;
    while i < max {
        let user_addr = start_user.checked_add(i).ok_or(UserAccessError::Fault)?;
        let byte = unsafe { load_user_u8(user_addr)? };
        if byte == 0 {
            return Ok(i);
        }
        i += 1;
    }
    Err(UserAccessError::TooLong)
}

/// 注入到 general 的 vtable。
pub(super) static USER_ACCESS_OPS: UserAccessOps = UserAccessOps {
    copy_from_user,
    copy_to_user,
    strnlen_user,
};

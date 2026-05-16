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

/// 带 `__ex_table` fixup 的用户缓冲读取。
///
/// # Safety
/// `dst` 必须指向 `len` 字节可写内核内存；`src_user` 可以是任意用户值。
unsafe fn copy_from_user(dst: *mut u8, src_user: usize, len: usize) -> Result<(), UserAccessError> {
    for i in 0..len {
        let user_addr = src_user.checked_add(i).ok_or(UserAccessError::Fault)?;
        let byte = unsafe { load_user_u8(user_addr)? };
        unsafe { core::ptr::write(dst.add(i), byte) };
    }
    Ok(())
}

/// # Safety
/// 对偶 of [`copy_from_user`]；`src` 必须指向 `len` 字节可读内核内存。
unsafe fn copy_to_user(dst_user: usize, src: *const u8, len: usize) -> Result<(), UserAccessError> {
    for i in 0..len {
        let byte = unsafe { core::ptr::read(src.add(i)) };
        let user_addr = dst_user.checked_add(i).ok_or(UserAccessError::Fault)?;
        unsafe { store_user_u8(user_addr, byte)? };
    }
    Ok(())
}

/// # Safety
/// `start_user` 可以是任意用户值；扫到 NUL 或 max 为止。
unsafe fn strnlen_user(start_user: usize, max: usize) -> Result<usize, UserAccessError> {
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

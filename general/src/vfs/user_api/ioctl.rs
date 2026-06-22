//! ioctl 用户缓冲区辅助函数。
//!
//! VFS 设备文件适配层经常需要读写小的用户态 ABI 结构。这里集中封装
//! `copy_from_user`/`copy_to_user` 和固定宽度整数的小端转换，避免每个设备
//! 文件各自手写偏移访问并在长度变化时引入越界 panic。

use core::mem::{MaybeUninit, size_of};

use errno::Errno;

use crate::mm::{copy_from_user, copy_to_user};

/// 从用户地址读取一段原始字节。
pub fn read_bytes_from_user(user: usize, dst: &mut [u8]) -> Result<(), Errno> {
    if user == 0 && !dst.is_empty() {
        return Err(Errno::EFAULT);
    }
    copy_from_user(user, dst).map_err(|e| e.as_errno())
}

/// 向用户地址写入一段原始字节。
pub fn write_bytes_to_user(user: usize, src: &[u8]) -> Result<(), Errno> {
    if user == 0 && !src.is_empty() {
        return Err(Errno::EFAULT);
    }
    copy_to_user(user, src).map_err(|e| e.as_errno())
}

/// 从用户空间读取一个 POD ABI 结构。
///
/// 调用方只能把本函数用于 `repr(C)`/固定布局且可按字节复制的结构；这条约束
/// 由 `Copy` 和设备文件适配层的调用点共同保证。
pub fn read_pod_from_user<T: Copy>(user: usize) -> Result<T, Errno> {
    let mut value = MaybeUninit::<T>::zeroed();
    // Safety: 按字节填满栈上对象后再 assume_init；调用点限制为固定布局 POD。
    let bytes =
        unsafe { core::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
    read_bytes_from_user(user, bytes)?;
    // Safety: 上面的 read_bytes_from_user 已经覆盖整个对象字节范围。
    Ok(unsafe { value.assume_init() })
}

/// 向用户空间写入一个 POD ABI 结构。
pub fn write_pod_to_user<T: Copy>(user: usize, value: &T) -> Result<(), Errno> {
    // Safety: 只把固定布局 POD 结构按字节复制到用户空间，不暴露 Rust 引用。
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    write_bytes_to_user(user, bytes)
}

/// 从用户地址读取一个 i32。
pub fn read_i32_from_user(user: usize) -> Result<i32, Errno> {
    let mut raw = [0u8; core::mem::size_of::<i32>()];
    read_bytes_from_user(user, &mut raw)?;
    Ok(i32::from_le_bytes(raw))
}

/// 向用户地址写入一个 i32。
pub fn write_i32_to_user(user: usize, value: i32) -> Result<(), Errno> {
    write_bytes_to_user(user, &value.to_le_bytes())
}

/// 向用户地址写入一个 u32。
pub fn write_u32_to_user(user: usize, value: u32) -> Result<(), Errno> {
    write_bytes_to_user(user, &value.to_le_bytes())
}

/// 向用户地址写入一个 u64。
pub fn write_u64_to_user(user: usize, value: u64) -> Result<(), Errno> {
    write_bytes_to_user(user, &value.to_le_bytes())
}

/// 向用户地址写入一个 usize。
pub fn write_usize_to_user(user: usize, value: usize) -> Result<(), Errno> {
    write_bytes_to_user(user, &value.to_le_bytes())
}

/// 在 ABI 字节数组的指定偏移写入一个 u32。
pub fn put_u32(out: &mut [u8], off: usize, value: u32) -> Option<()> {
    let end = off.checked_add(core::mem::size_of::<u32>())?;
    let dst = out.get_mut(off..end)?;
    dst.copy_from_slice(&value.to_le_bytes());
    Some(())
}

/// 从 ABI 字节数组的指定偏移读取一个 u32。
pub fn read_u32(raw: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(core::mem::size_of::<u32>())?;
    let bytes = raw.get(off..end)?;
    let mut out = [0u8; core::mem::size_of::<u32>()];
    out.copy_from_slice(bytes);
    Some(u32::from_le_bytes(out))
}

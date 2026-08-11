//! Rust freestanding 代码生成可能引用的基础内存原语。

/// 复制不重叠内存。使用 volatile 单字节访问，避免优化器把实现重新折叠成自身调用。
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn memcpy(target: *mut u8, source: *const u8, length: usize) -> *mut u8 {
    let mut index = 0;
    while index < length {
        let value = unsafe { source.add(index).read_volatile() };
        unsafe { target.add(index).write_volatile(value) };
        index += 1;
    }
    target
}

/// 复制可能重叠的内存区间。
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn memmove(target: *mut u8, source: *const u8, length: usize) -> *mut u8 {
    if (target as usize) <= (source as usize) {
        let _ = unsafe { memcpy(target, source, length) };
        return target;
    }
    let mut remaining = length;
    while remaining != 0 {
        remaining -= 1;
        let value = unsafe { source.add(remaining).read_volatile() };
        unsafe { target.add(remaining).write_volatile(value) };
    }
    target
}

/// 用单字节值填充内存。
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn memset(target: *mut u8, value: i32, length: usize) -> *mut u8 {
    let value = value as u8;
    let mut index = 0;
    while index < length {
        unsafe { target.add(index).write_volatile(value) };
        index += 1;
    }
    target
}

/// 按无符号字节字典序比较两个内存区间。
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn memcmp(left: *const u8, right: *const u8, length: usize) -> i32 {
    let mut index = 0;
    while index < length {
        let left_byte = unsafe { left.add(index).read_volatile() };
        let right_byte = unsafe { right.add(index).read_volatile() };
        if left_byte != right_byte {
            return i32::from(left_byte) - i32::from(right_byte);
        }
        index += 1;
    }
    0
}

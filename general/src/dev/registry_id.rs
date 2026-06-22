//! 设备注册表内部使用的单调编号分配器。
//!
//! 这些编号只用于 registry handle 的所有权校验，不代表设备号、总线地址或
//! 用户可见名称。编号一旦发出就不复用，避免旧 handle 在设备热移除、驱动重载
//! 或启动期回滚路径中误命中新注册对象。

use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RegistryIdExhausted;

/// 从受外层锁保护的 `next_id` 中分配一个非零编号。
pub(crate) fn alloc_locked_id(next_id: &mut u64) -> Result<u64, RegistryIdExhausted> {
    let id = *next_id;
    if id == 0 {
        return Err(RegistryIdExhausted);
    }
    *next_id = id.checked_add(1).unwrap_or(0);
    Ok(id)
}

/// 从可并发访问的 `next_id` 中分配一个非零编号。
pub(crate) fn alloc_atomic_id(next_id: &AtomicU64) -> Result<u64, RegistryIdExhausted> {
    loop {
        let id = next_id.load(Ordering::Relaxed);
        if id == 0 {
            return Err(RegistryIdExhausted);
        }
        let next = id.checked_add(1).unwrap_or(0);
        if next_id
            .compare_exchange(id, next, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(id);
        }
    }
}

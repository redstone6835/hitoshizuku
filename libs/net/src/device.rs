//! 网络设备对象。
//!
//! [`NetDevice`] 是驱动 probe 后创建的设备实例，持有 [`NetDriver`] 引用和
//! 接口元数据（名称、ID、状态）。它对应 Linux 的 `struct net_device`——
//! 一个已注册的网络接口在内核中的身份。
//!
//! 在 PnP 框架中，`general` 层会把 `NetDevice` 包装进 `NetFunction`
//! 并注册到 `FunctionRegistry`。本 crate 不依赖 `general`——它只定义
//! 设备对象本身。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::driver::NetDriver;

// ── 接口 ID ──────────────────────────────────────────────────────────────────

/// 网络接口唯一标识。
///
/// 由 [`NetDevice::new`] 时自动分配（全局单调递增）。用于
/// [`NetStack`](crate::stack::NetStack) 中的 attach/detach 索引。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InterfaceId(pub u32);

impl InterfaceId {
    /// 分配下一个接口 ID（全局唯一）。
    fn next() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// 获取原始数值表示。
    pub fn raw(self) -> u32 {
        self.0
    }
}

// ── 设备状态 ─────────────────────────────────────────────────────────────────

/// 设备生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DeviceState {
    /// 设备已注册且正常工作。
    Active = 0,
    /// 设备已被标记为移除（热插拔或驱动卸载）。
    Gone = 1,
}

// ── NetDevice ────────────────────────────────────────────────────────────────

/// 一个已注册的网络设备。
///
/// 可安全克隆（内部 Arc），用于在协议栈和 PnP 框架之间共享引用。
pub struct NetDevice {
    id: InterfaceId,
    name: Box<str>,
    driver: Arc<dyn NetDriver>,
    state: AtomicU8,
    /// 因 TX 队列满而丢弃的帧数（adapter 统计）。
    tx_dropped: AtomicU64,
}

impl NetDevice {
    /// 创建一个新的网络设备。
    ///
    /// - `name`：接口名（如 `"eth0"`），显示用途。
    /// - `driver`：底层驱动的共享引用。
    pub fn new(name: &str, driver: Arc<dyn NetDriver>) -> Self {
        Self {
            id: InterfaceId::next(),
            name: name.into(),
            driver,
            state: AtomicU8::new(DeviceState::Active as u8),
            tx_dropped: AtomicU64::new(0),
        }
    }

    /// 接口 ID（全局唯一）。
    pub fn id(&self) -> InterfaceId {
        self.id
    }

    /// 接口名称。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 底层驱动引用。
    pub fn driver(&self) -> &Arc<dyn NetDriver> {
        &self.driver
    }

    /// 当前设备状态。
    pub fn state(&self) -> DeviceState {
        match self.state.load(Ordering::Acquire) {
            0 => DeviceState::Active,
            _ => DeviceState::Gone,
        }
    }

    /// 设备是否仍然活跃。
    pub fn is_active(&self) -> bool {
        self.state() == DeviceState::Active
    }

    /// 标记设备为已移除。
    ///
    /// 此操作不可逆——一旦标记为 Gone，设备对象不会复活。
    /// 后续对 driver 的操作可能返回 None / 失败，但不会 panic。
    pub fn mark_gone(&self) {
        self.state.store(DeviceState::Gone as u8, Ordering::Release);
    }

    /// 递增 TX 丢帧计数（adapter 层 alloc_tx 失败时调用）。
    pub fn inc_tx_dropped(&self) {
        self.tx_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// 获取 TX 丢帧计数快照。
    pub fn tx_dropped(&self) -> u64 {
        self.tx_dropped.load(Ordering::Relaxed)
    }
}

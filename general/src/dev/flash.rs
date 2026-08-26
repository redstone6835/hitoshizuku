//! 固件声明的 MMIO flash 区域抽象。
//!
//! 这里登记的是“可由 CPU 线性读取的非易失存储窗口”。它不映射到 POSIX 设备号；
//! 后续若需要擦写协议或文件系统适配，应基于这个 typed flash 资源继续分层实现。

use alloc::sync::Arc;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

use crate::dev::pnp::{PnpHandleResource, PnpResourceKind};

use super::registry_id;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlashWindow {
    pub phys: usize,
    pub size: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlashCapabilities {
    pub readable: bool,
    pub writable: bool,
    pub erasable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlashError {
    Invalid,
    OutOfRange,
    Unsupported,
    NotFound,
    OutOfMemory,
}

pub trait FlashDevice: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> FlashCapabilities;
    fn bank_width(&self) -> usize;
    fn window_count(&self) -> usize;
    fn window_at(&self, index: usize) -> Option<FlashWindow>;
    fn read(&self, offset: usize, out: &mut [u8]) -> Result<(), FlashError>;
}

/// 一段具有统一擦除块大小的 flash geometry。
///
/// 该类型只属于 `kernel.general.flash@2`，不得添加到
/// [`FlashDevice`] 的 vtable；`@1` 会被已编译 ELM 以 exact-Rust ABI 导入。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlashEraseRegion {
    pub offset: usize,
    pub block_size: usize,
    pub block_count: usize,
}

/// `kernel.general.flash@2` 擦写接口的 I/O 错误。
///
/// `@1` 的 [`FlashError`] 枚举必须保持原有五个 variant 和 discriminant，
/// 因此擦写状态使用独立类型表达。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlashIoError {
    Invalid,
    OutOfRange,
    Unsupported,
    OutOfMemory,
    NeedsErase,
    Busy,
    Io,
}

/// Flash 的可选擦写扩展面。
///
/// 设备身份、窗口与只读能力仍由 ABI 稳定的 [`FlashDevice`] 提供；
/// 驱动通过 [`register_v2`] 把两个 trait object 作为同一注册的两个视图提交。
/// 这样 `@1` consumer 仍可以发现新设备，且无需接受变更过的 vtable。
pub trait FlashDeviceV2: Send + Sync {
    fn erase_region_count(&self) -> usize;
    fn erase_region_at(&self, index: usize) -> Option<FlashEraseRegion>;
    fn write(&self, offset: usize, data: &[u8]) -> Result<(), FlashIoError>;
    fn erase(&self, offset: usize, len: usize) -> Result<(), FlashIoError>;
}

/// `@2` snapshot 中的一个 flash 设备及其擦写扩展。
#[derive(Clone)]
pub struct FlashDeviceSnapshotV2 {
    pub device: Arc<dyn FlashDevice>,
    pub operations: Arc<dyn FlashDeviceV2>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlashHandle {
    id: u64,
}

impl FlashHandle {
    pub const fn id(self) -> u64 {
        self.id
    }
}

struct FlashRegistration {
    handle: FlashHandle,
    dev: Arc<dyn FlashDevice>,
    operations_v2: Option<Arc<dyn FlashDeviceV2>>,
}

struct FlashRegistry {
    next_id: u64,
    devices: Vec<FlashRegistration>,
}

impl FlashRegistry {
    const fn new() -> Self {
        Self {
            next_id: 1,
            devices: Vec::new(),
        }
    }
}

static FLASH_DEVICES: Spinlock<FlashRegistry> = Spinlock::new(FlashRegistry::new());

#[kernel_symbols::export(
    name = "general.dev.flash.register",
    contract = "kernel.general.flash@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 1u64
)]
pub fn register(dev: Arc<dyn FlashDevice>) -> Result<FlashHandle, FlashError> {
    register_inner(dev, None)
}

fn register_inner(
    dev: Arc<dyn FlashDevice>,
    operations_v2: Option<Arc<dyn FlashDeviceV2>>,
) -> Result<FlashHandle, FlashError> {
    let mut registry = FLASH_DEVICES.lock();
    registry
        .devices
        .try_reserve(1)
        .map_err(|_| FlashError::OutOfMemory)?;
    let id =
        registry_id::alloc_locked_id(&mut registry.next_id).map_err(|_| FlashError::OutOfMemory)?;
    // FlashHandle 只证明一次注册关系的所有权；即使同一窗口后续重新 probe，
    // 旧 handle 也不能注销新的 flash 对象。
    let handle = FlashHandle { id };
    registry.devices.push(FlashRegistration {
        handle,
        dev,
        operations_v2,
    });
    Ok(handle)
}

/// 同时登记 ABI 稳定的只读视图和 `@2` 擦写扩展。
///
/// `dev` 会进入旧 [`snapshot`]，所以既有 `@1` consumer 仍能发现该设备。
#[kernel_symbols::export(
    name = "general.dev.flash.register_v2",
    contract = "kernel.general.flash@2",
    version = 2,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 3u64
)]
pub fn register_v2(
    dev: Arc<dyn FlashDevice>,
    operations: Arc<dyn FlashDeviceV2>,
) -> Result<FlashHandle, FlashError> {
    register_inner(dev, Some(operations))
}

#[kernel_symbols::export(
    name = "general.dev.flash.unregister",
    contract = "kernel.general.flash@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister(handle: FlashHandle) -> Result<(), FlashError> {
    let mut registry = FLASH_DEVICES.lock();
    let Some(index) = registry
        .devices
        .iter()
        .position(|registered| registered.handle == handle)
    else {
        return Err(FlashError::NotFound);
    };
    registry.devices.swap_remove(index);
    Ok(())
}

#[kernel_symbols::export(
    name = "general.dev.flash.unregister_v2",
    contract = "kernel.general.flash@2",
    version = 2,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_v2(handle: FlashHandle) -> Result<(), FlashError> {
    unregister(handle)
}

fn release_flash_resource(handle: FlashHandle) -> bool {
    unregister(handle).is_ok()
}

/// 将 flash handle 包装成 PnP-owned resource。
#[kernel_symbols::export(
    name = "general.dev.flash.pnp_resource",
    contract = "kernel.general.flash@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn pnp_resource(handle: FlashHandle, label: &'static str) -> PnpHandleResource<FlashHandle> {
    PnpHandleResource::new(
        PnpResourceKind::Flash,
        label,
        handle,
        release_flash_resource,
    )
}

#[kernel_symbols::export(
    name = "general.dev.flash.pnp_resource_v2",
    contract = "kernel.general.flash@2",
    version = 2,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn pnp_resource_v2(handle: FlashHandle, label: &'static str) -> PnpHandleResource<FlashHandle> {
    PnpHandleResource::new(
        PnpResourceKind::Flash,
        label,
        handle,
        release_flash_resource,
    )
}

/// 在常驻 General 侧构造完成类型擦除的 flash `@2` registration 资源。
#[kernel_symbols::export(
    name = "general.dev.flash.pnp_resource_v2_boxed",
    contract = "kernel.general.flash@2",
    version = 2,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn pnp_resource_v2_boxed(
    handle: FlashHandle,
    label: &'static str,
) -> alloc::boxed::Box<dyn crate::dev::pnp::PnpResource> {
    alloc::boxed::Box::new(pnp_resource_v2(handle, label))
}

#[kernel_symbols::export(
    name = "general.dev.flash.snapshot",
    contract = "kernel.general.flash@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn snapshot() -> Vec<Arc<dyn FlashDevice>> {
    FLASH_DEVICES
        .lock()
        .devices
        .iter()
        .map(|registered| Arc::clone(&registered.dev))
        .collect()
}

#[kernel_symbols::export(
    name = "general.dev.flash.snapshot_v2",
    contract = "kernel.general.flash@2",
    version = 2,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn snapshot_v2() -> Vec<FlashDeviceSnapshotV2> {
    FLASH_DEVICES
        .lock()
        .devices
        .iter()
        .filter_map(|registered| {
            Some(FlashDeviceSnapshotV2 {
                device: Arc::clone(&registered.dev),
                operations: Arc::clone(registered.operations_v2.as_ref()?),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::FlashError;

    #[test]
    fn flash_v1_error_discriminants_stay_stable() {
        assert_eq!(FlashError::Invalid as u8, 0);
        assert_eq!(FlashError::OutOfRange as u8, 1);
        assert_eq!(FlashError::Unsupported as u8, 2);
        assert_eq!(FlashError::NotFound as u8, 3);
        assert_eq!(FlashError::OutOfMemory as u8, 4);
    }
}

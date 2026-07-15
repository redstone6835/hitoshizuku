//! 固件描述的非枚举总线抽象。
//!
//! `simple-bus`、`qemu,platform` 这类节点本身通常不暴露 I/O function，但它们定义
//! 子设备地址空间、`ranges` 翻译和 DMA 属性。登记到这里后，设备管理层可以看到
//! 固件拓扑里的总线节点，而不需要把它们伪装成字符设备或块设备。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

use crate::dev::pnp::{self, PnpDependency, PnpHandleResource, PnpResourceKind};

use super::registry_id;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareBusRange {
    pub child_start: u128,
    pub parent_start: usize,
    pub size: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirmwareBusDescriptor {
    pub name: Box<str>,
    pub phandle: Option<u32>,
    pub child_address_cells: u8,
    pub child_size_cells: u8,
    pub parent_address_cells: u8,
    pub ranges: Vec<FirmwareBusRange>,
    pub dma_coherent: bool,
}

pub trait FirmwareBus: Send + Sync {
    fn descriptor(&self) -> &FirmwareBusDescriptor;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirmwareBusError {
    NotFound,
    OutOfMemory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareBusHandle {
    id: u64,
}

impl FirmwareBusHandle {
    pub const fn id(self) -> u64 {
        self.id
    }
}

struct FirmwareBusRegistration {
    handle: FirmwareBusHandle,
    bus: Arc<dyn FirmwareBus>,
}

struct FirmwareBusRegistry {
    next_id: u64,
    buses: Vec<FirmwareBusRegistration>,
}

impl FirmwareBusRegistry {
    const fn new() -> Self {
        Self {
            next_id: 1,
            buses: Vec::new(),
        }
    }
}

static FIRMWARE_BUSES: Spinlock<FirmwareBusRegistry> = Spinlock::new(FirmwareBusRegistry::new());

#[kernel_symbols::export(
    name = "general.dev.firmware_bus.register",
    contract = "kernel.general.firmware-bus@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn register(bus: Arc<dyn FirmwareBus>) -> Result<FirmwareBusHandle, FirmwareBusError> {
    let mut registry = FIRMWARE_BUSES.lock();
    registry
        .buses
        .try_reserve(1)
        .map_err(|_| FirmwareBusError::OutOfMemory)?;
    let id = registry_id::alloc_locked_id(&mut registry.next_id)
        .map_err(|_| FirmwareBusError::OutOfMemory)?;
    // 固件总线节点只在设备管理层内部表达拓扑；handle ID 用于区分每一次登记生命周期。
    let handle = FirmwareBusHandle { id };
    registry.buses.push(FirmwareBusRegistration { handle, bus });
    drop(registry);
    if super::elm_lifecycle::track_firmware_bus(handle).is_err() {
        let _ = unregister(handle);
        return Err(FirmwareBusError::OutOfMemory);
    }
    pnp::notify_dependency_ready(PnpDependency::FirmwareBus);
    Ok(handle)
}

#[kernel_symbols::export(
    name = "general.dev.firmware_bus.unregister",
    contract = "kernel.general.firmware-bus@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister(handle: FirmwareBusHandle) -> Result<(), FirmwareBusError> {
    let mut registry = FIRMWARE_BUSES.lock();
    let Some(index) = registry
        .buses
        .iter()
        .position(|registered| registered.handle == handle)
    else {
        return Err(FirmwareBusError::NotFound);
    };
    registry.buses.swap_remove(index);
    drop(registry);
    super::elm_lifecycle::forget_firmware_bus(handle);
    Ok(())
}

fn release_firmware_bus_resource(handle: FirmwareBusHandle) -> bool {
    unregister(handle).is_ok()
}

/// 将固件总线登记 handle 包装成 PnP-owned resource。
#[kernel_symbols::export(
    name = "general.dev.firmware_bus.pnp_resource",
    contract = "kernel.general.device-resource@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn pnp_resource(
    handle: FirmwareBusHandle,
    label: &'static str,
) -> PnpHandleResource<FirmwareBusHandle> {
    PnpHandleResource::new(
        PnpResourceKind::FirmwareBus,
        label,
        handle,
        release_firmware_bus_resource,
    )
}

#[kernel_symbols::export(
    name = "general.dev.firmware_bus.snapshot",
    contract = "kernel.general.firmware-bus@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
)]
pub fn snapshot() -> Vec<Arc<dyn FirmwareBus>> {
    FIRMWARE_BUSES
        .lock()
        .buses
        .iter()
        .map(|registered| Arc::clone(&registered.bus))
        .collect()
}

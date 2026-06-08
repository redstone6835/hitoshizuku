//! 固件描述的非枚举总线抽象。
//!
//! `simple-bus`、`qemu,platform` 这类节点本身通常不暴露 I/O function，但它们定义
//! 子设备地址空间、`ranges` 翻译和 DMA 属性。登记到这里后，设备管理层可以看到
//! 固件拓扑里的总线节点，而不需要把它们伪装成字符设备或块设备。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

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
    Ok(handle)
}

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
    Ok(())
}

pub fn snapshot() -> Vec<Arc<dyn FirmwareBus>> {
    FIRMWARE_BUSES
        .lock()
        .buses
        .iter()
        .map(|registered| Arc::clone(&registered.bus))
        .collect()
}

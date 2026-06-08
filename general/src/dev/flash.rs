//! 固件声明的 MMIO flash 区域抽象。
//!
//! 这里登记的是“可由 CPU 线性读取的非易失存储窗口”。它不映射到 POSIX 设备号；
//! 后续若需要擦写协议或文件系统适配，应基于这个 typed flash 资源继续分层实现。

use alloc::sync::Arc;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

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
}

pub trait FlashDevice: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> FlashCapabilities;
    fn bank_width(&self) -> usize;
    fn window_count(&self) -> usize;
    fn window_at(&self, index: usize) -> Option<FlashWindow>;
    fn read(&self, offset: usize, out: &mut [u8]) -> Result<(), FlashError>;
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

pub fn register(dev: Arc<dyn FlashDevice>) -> Result<FlashHandle, FlashError> {
    let mut registry = FLASH_DEVICES.lock();
    registry
        .devices
        .try_reserve(1)
        .map_err(|_| FlashError::Invalid)?;
    let handle = FlashHandle {
        id: registry.next_id,
    };
    registry.next_id = registry.next_id.wrapping_add(1).max(1);
    registry.devices.push(FlashRegistration { handle, dev });
    Ok(handle)
}

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

pub fn snapshot() -> Vec<Arc<dyn FlashDevice>> {
    FLASH_DEVICES
        .lock()
        .devices
        .iter()
        .map(|registered| Arc::clone(&registered.dev))
        .collect()
}

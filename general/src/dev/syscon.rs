//! 通用 system-controller 寄存器块抽象。
//!
//! DTB 里的 `syscon` 节点表示一小段被多个功能复用的控制寄存器。底层驱动只把
//! 这个寄存器块登记为按 phandle 查询的 typed 资源；poweroff/reboot 等功能节点
//! 再通过 `regmap` 引用它。这里不创建 `/dev` 节点，也不使用 POSIX 设备号。

use alloc::sync::Arc;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysconAccessWidth {
    U8,
    U16,
    U32,
    U64,
}

impl SysconAccessWidth {
    pub const fn from_bytes(bytes: usize) -> Option<Self> {
        match bytes {
            1 => Some(Self::U8),
            2 => Some(Self::U16),
            4 => Some(Self::U32),
            8 => Some(Self::U64),
            _ => None,
        }
    }

    pub const fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
            Self::U64 => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysconError {
    Invalid,
    OutOfRange,
    NotFound,
    AlreadyRegistered,
}

pub trait SysconDevice: Send + Sync {
    /// 固件 phandle。没有 phandle 的 syscon 不能被其它 DTB 功能节点通过
    /// `regmap` 引用，因此不会进入全局 registry。
    fn phandle(&self) -> u32;

    /// 固件声明的寄存器窗口物理地址与长度。
    fn phys_range(&self) -> (usize, usize);

    /// 本 syscon 节点声明的默认访问宽度。
    fn default_width(&self) -> SysconAccessWidth;

    /// 将功能节点里的逻辑 offset 转换成物理寄存器地址。
    fn phys_addr_for(&self, offset: usize, width: SysconAccessWidth) -> Option<usize>;

    fn read(&self, offset: usize, width: SysconAccessWidth) -> Result<u64, SysconError>;

    fn write(&self, offset: usize, width: SysconAccessWidth, value: u64)
    -> Result<(), SysconError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysconHandle {
    phandle: u32,
    id: u64,
}

impl SysconHandle {
    pub const fn phandle(self) -> u32 {
        self.phandle
    }

    pub const fn id(self) -> u64 {
        self.id
    }
}

struct SysconRegistration {
    handle: SysconHandle,
    dev: Arc<dyn SysconDevice>,
}

struct SysconRegistry {
    next_id: u64,
    devices: Vec<SysconRegistration>,
}

impl SysconRegistry {
    const fn new() -> Self {
        Self {
            next_id: 1,
            devices: Vec::new(),
        }
    }
}

static SYSCONS: Spinlock<SysconRegistry> = Spinlock::new(SysconRegistry::new());

pub fn register(dev: Arc<dyn SysconDevice>) -> Result<SysconHandle, SysconError> {
    let phandle = dev.phandle();
    if phandle == 0 {
        return Err(SysconError::Invalid);
    }

    let mut registry = SYSCONS.lock();
    if registry
        .devices
        .iter()
        .any(|registered| registered.handle.phandle == phandle)
    {
        return Err(SysconError::AlreadyRegistered);
    }
    registry
        .devices
        .try_reserve(1)
        .map_err(|_| SysconError::Invalid)?;
    let handle = SysconHandle {
        phandle,
        id: registry.next_id,
    };
    registry.next_id = registry.next_id.wrapping_add(1).max(1);
    registry.devices.push(SysconRegistration { handle, dev });
    Ok(handle)
}

pub fn unregister(handle: SysconHandle) -> Result<(), SysconError> {
    let mut registry = SYSCONS.lock();
    let Some(index) = registry
        .devices
        .iter()
        .position(|registered| registered.handle == handle)
    else {
        return Err(SysconError::NotFound);
    };
    registry.devices.swap_remove(index);
    Ok(())
}

pub fn get(phandle: u32) -> Option<Arc<dyn SysconDevice>> {
    SYSCONS
        .lock()
        .devices
        .iter()
        .find(|registered| registered.handle.phandle == phandle)
        .map(|registered| Arc::clone(&registered.dev))
}

pub fn write(
    phandle: u32,
    offset: usize,
    width: SysconAccessWidth,
    value: u64,
) -> Result<(), SysconError> {
    let dev = get(phandle).ok_or(SysconError::NotFound)?;
    dev.write(offset, width, value)
}

pub fn read(phandle: u32, offset: usize, width: SysconAccessWidth) -> Result<u64, SysconError> {
    let dev = get(phandle).ok_or(SysconError::NotFound)?;
    dev.read(offset, width)
}

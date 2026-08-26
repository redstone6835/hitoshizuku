//! 通用 system-controller 寄存器块抽象。
//!
//! DTB 里的 `syscon` 节点表示一小段被多个功能复用的控制寄存器。底层驱动只把
//! 这个寄存器块登记为按 phandle 查询的 typed 资源；poweroff/reboot 等功能节点
//! 再通过 `regmap` 引用它。这里不创建 `/dev` 节点，也不使用 POSIX 设备号。

use alloc::sync::Arc;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

use crate::dev::pnp::{self, PnpDependency, PnpHandleResource, PnpResourceKind};

use super::registry_id;

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
    OutOfMemory,
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

#[kernel_symbols::export(
    name = "general.dev.syscon.register",
    contract = "kernel.general.syscon@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 1u64
)]
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
        .map_err(|_| SysconError::OutOfMemory)?;
    let id = registry_id::alloc_locked_id(&mut registry.next_id)
        .map_err(|_| SysconError::OutOfMemory)?;
    // 同一个 phandle 被注销后可以再次登记，但旧 SysconHandle 不能继续拥有新对象。
    let handle = SysconHandle { phandle, id };
    registry.devices.push(SysconRegistration { handle, dev });
    drop(registry);
    pnp::notify_dependency_ready(PnpDependency::Syscon(phandle));
    Ok(handle)
}

#[kernel_symbols::export(
    name = "general.dev.syscon.unregister",
    contract = "kernel.general.syscon@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
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

fn release_syscon_resource(handle: SysconHandle) -> bool {
    unregister(handle).is_ok()
}

/// 将 syscon handle 包装成 PnP-owned resource。
///
/// 驱动 probe 成功登记 syscon 后应立即交给 PnP 设备拥有，remove/rollback 时由
/// core 统一注销，避免驱动私有状态和 registry 生命周期分离。
#[kernel_symbols::export(
    name = "general.dev.syscon.pnp_resource",
    contract = "kernel.general.syscon@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn pnp_resource(handle: SysconHandle, label: &'static str) -> PnpHandleResource<SysconHandle> {
    PnpHandleResource::new(
        PnpResourceKind::Syscon,
        label,
        handle,
        release_syscon_resource,
    )
}

/// 在常驻 General 侧构造完成类型擦除的 syscon registration 资源。
#[kernel_symbols::export(
    name = "general.dev.syscon.pnp_resource_boxed",
    contract = "kernel.general.syscon@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn pnp_resource_boxed(
    handle: SysconHandle,
    label: &'static str,
) -> alloc::boxed::Box<dyn crate::dev::pnp::PnpResource> {
    alloc::boxed::Box::new(pnp_resource(handle, label))
}

#[kernel_symbols::export(
    name = "general.dev.syscon.get",
    contract = "kernel.general.syscon@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn get(phandle: u32) -> Option<Arc<dyn SysconDevice>> {
    SYSCONS
        .lock()
        .devices
        .iter()
        .find(|registered| registered.handle.phandle == phandle)
        .map(|registered| Arc::clone(&registered.dev))
}

#[kernel_symbols::export(
    name = "general.dev.syscon.write",
    contract = "kernel.general.syscon@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn write(
    phandle: u32,
    offset: usize,
    width: SysconAccessWidth,
    value: u64,
) -> Result<(), SysconError> {
    let dev = get(phandle).ok_or(SysconError::NotFound)?;
    dev.write(offset, width, value)
}

#[kernel_symbols::export(
    name = "general.dev.syscon.read",
    contract = "kernel.general.syscon@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE
)]
pub fn read(phandle: u32, offset: usize, width: SysconAccessWidth) -> Result<u64, SysconError> {
    let dev = get(phandle).ok_or(SysconError::NotFound)?;
    dev.read(offset, width)
}

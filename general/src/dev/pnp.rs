//! PnP 设备抽象框架。
//!
//! # 分层架构
//!
//! ```text
//! Bus 层           PCI / USB / Platform 扫描硬件，创建 PnpDevice
//!                          ↓
//! PnP 层           管理硬件身份、拓扑、状态机、driver probe/remove
//!                          ↓
//! Function 层       CharDevice / BlockDevice 提供 I/O 能力
//!                          ↓
//! devtmpfs 层       只负责 /dev 节点，不理解 PCI/USB
//! ```
//!
//! # 状态机
//!
//! ```text
//! Discovered ──→ Probing ──→ Bound ──→ Removing ──→ Gone
//!      ↑            │  ↑        │                      │
//!      └────────────┘  │        │                      │
//!                      │        │                      │
//!          probe 失败回退       └── 驱动卸载 ──────────→ (future)
//! ```
//!
//! # 热插拔
//!
//! 设备可以在任意时刻被创建（`PnpDevice::new` + `PNP_DEVICES.push` +
//! `PNP_DRIVERS.probe_device`）或移除（`dev.remove_device`）。
//! remove 流程严格保证：先阻止新 I/O → 排空已有 I/O → 关闭硬件 → 清理注册。

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::{self, Debug};
use core::hash::{Hash, Hasher};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vfs::sync::Spinlock;

use crate::dev::enumerate::DEVICES;
use crate::dev::function::{DevNodeSet, DeviceFunction, FunctionRegistryError};

// ── PnP 错误类型 ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PnpError {
    /// 设备状态不允许当前操作
    InvalidState,
    /// 非法的状态转换
    InvalidTransition,
    /// 未找到匹配的驱动
    NoDriver,
    /// 驱动 probe 失败
    ProbeFailed,
    /// 同名 function 已存在
    FunctionExists,
    /// 设备名冲突
    NameConflict,
    /// devtmpfs 未就绪
    NoDevtmpfs,
    /// devtmpfs 操作失败
    DevtmpfsError,
    /// 内存不足
    OutOfMemory,
}

impl From<FunctionRegistryError> for PnpError {
    fn from(e: FunctionRegistryError) -> Self {
        match e {
            FunctionRegistryError::NameExists => PnpError::NameConflict,
            FunctionRegistryError::OutOfMemory => PnpError::OutOfMemory,
        }
    }
}

// ── PnpId：硬件身份 ──────────────────────────────────────────────────────

/// PnP 设备的稳定硬件身份。
///
/// 该身份只描述设备在总线上的位置或固件节点，不包含 `/dev` 节点名。驱动匹配
/// 应结合 [`PnpBusInfo`] 里的总线私有信息完成。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PnpId {
    /// PCI/PCIe function，由 segment/bus/device/function 唯一定位。
    Pci {
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
    },
    /// USB device 或 interface。
    Usb {
        bus_id: u8,
        address: u8,
        interface: Option<u8>,
    },
    /// 固件枚举的 platform 设备。
    Platform { name: Box<str>, index: u32 },
}

impl Hash for PnpId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
        match self {
            PnpId::Pci {
                segment,
                bus,
                device,
                function,
            } => {
                segment.hash(state);
                bus.hash(state);
                device.hash(state);
                function.hash(state);
            }
            PnpId::Usb {
                bus_id,
                address,
                interface,
            } => {
                bus_id.hash(state);
                address.hash(state);
                interface.hash(state);
            }
            PnpId::Platform { name, index } => {
                name.as_ref().hash(state);
                index.hash(state);
            }
        }
    }
}

impl PnpId {
    pub fn bus_type(&self) -> BusType {
        match self {
            PnpId::Pci { .. } => BusType::PCI,
            PnpId::Usb { .. } => BusType::USB,
            PnpId::Platform { .. } => BusType::PLATFORM,
        }
    }
}

impl fmt::Display for PnpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PnpId::Pci {
                segment,
                bus,
                device,
                function,
            } => write!(
                f,
                "pci:{:04x}:{:02x}:{:02x}.{}",
                segment, bus, device, function
            ),
            PnpId::Usb {
                bus_id,
                address,
                interface,
            } => {
                if let Some(iface) = interface {
                    write!(f, "usb:{}-{}:{}", bus_id, address, iface)
                } else {
                    write!(f, "usb:{}-{}", bus_id, address)
                }
            }
            PnpId::Platform { name, index } => write!(f, "platform:{}[{}]", name, index),
        }
    }
}

// ── 总线类型与 PnpBusInfo ────────────────────────────────────────────────

/// PnP 内部使用的总线类型标识。
///
/// 该类型替代散落的 `"pci"`、`"usb"`、`"platform"` 字符串比较。总线枚举层
/// 和驱动只需要返回同一个 `BusType` 常量，注册表即可做类型安全的匹配。保留
/// [`BusType::new`] 是为了后续新增总线时不必修改 PnP core。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BusType(&'static str);

impl BusType {
    pub const PCI: Self = Self("pci");
    pub const USB: Self = Self("usb");
    pub const PLATFORM: Self = Self("platform");
    pub const GENERIC: Self = Self("generic");

    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for BusType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

pub trait PnpBusInfo: Send + Sync + Any + Debug {
    /// 返回该设备来自哪一种总线。
    fn bus_type(&self) -> BusType;

    /// 供具体总线封装在驱动 probe 时恢复强类型信息。
    fn as_any(&self) -> &dyn Any;
}

// ── 驱动初始化上下文 ─────────────────────────────────────────────────────

/// 驱动 factory 创建内建驱动实例时需要的启动期能力。
///
/// 该上下文由内核启动路径在注册内建驱动前设置。它只包含内建驱动初始化所需的
/// 地址转换回调，不把固件解析或总线扫描逻辑暴露给驱动 catalog。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DevInitContext {
    /// 将设备 MMIO 物理地址转换为可访问的内核虚拟地址。
    pub device_mmio_to_virt: fn(usize) -> usize,
    /// 将内核虚拟地址转换为设备 DMA 可使用的物理地址。
    pub virt_to_phys: fn(usize) -> usize,
    /// 用硬件 RTC 读出的 Unix 纳秒时间更新内核 realtime 时钟。
    pub set_realtime_ns: Option<fn(u64)>,
}

impl DevInitContext {
    pub const fn new(
        device_mmio_to_virt: fn(usize) -> usize,
        virt_to_phys: fn(usize) -> usize,
    ) -> Self {
        Self {
            device_mmio_to_virt,
            virt_to_phys,
            set_realtime_ns: None,
        }
    }

    pub const fn with_realtime_clock(mut self, set_realtime_ns: fn(u64)) -> Self {
        self.set_realtime_ns = Some(set_realtime_ns);
        self
    }
}

static DEV_INIT_CONTEXT: Spinlock<Option<DevInitContext>> = Spinlock::new(None);

/// 安装全局驱动初始化上下文。
///
/// 必须在调用 [`register_driver_factory`] 或内建驱动 bootstrap 前完成。
pub fn set_dev_init_context(ctx: DevInitContext) {
    *DEV_INIT_CONTEXT.lock() = Some(ctx);
}

fn dev_init_context() -> Result<DevInitContext, PnpError> {
    DEV_INIT_CONTEXT.lock().ok_or(PnpError::InvalidState)
}

// ── PnpState ─────────────────────────────────────────────────────────────

/// PnP 设备生命周期状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PnpState {
    /// 已被总线发现并放入全局设备表，尚未绑定驱动。
    Discovered,
    /// 正在调用驱动 `probe()`。
    Probing,
    /// 已成功绑定驱动，function 已完成注册。
    Bound,
    /// 正在执行热拔/移除流程。
    Removing,
    /// 设备已从全局表移除，不再接受新操作。
    Gone,
}

impl PnpState {
    pub const fn can_transition_to(self, next: Self) -> bool {
        use PnpState::*;
        matches!(
            (self, next),
            (Discovered, Probing)
                | (Discovered, Removing)
                | (Probing, Discovered)
                | (Probing, Bound)
                | (Probing, Removing)
                | (Bound, Removing)
                | (Removing, Gone)
        )
    }
}

// ── PnpDevice ────────────────────────────────────────────────────────────

struct PnpDeviceInner {
    state: PnpState,
    parent: Option<Weak<PnpDevice>>,
    children: Vec<Arc<PnpDevice>>,
    functions: Vec<Arc<dyn DeviceFunction>>,
    bound_driver: Option<Arc<dyn PnpDriver>>,
    driver_data: Option<Arc<dyn Any + Send + Sync>>,
}

/// PnP 设备对象。
///
/// 总线层创建该对象并放入 [`PNP_DEVICES`]；驱动 probe 成功后可以通过
/// [`PnpDevice::register_function`] 暴露一个或多个开放设备 function。
pub struct PnpDevice {
    pub id: PnpId,
    pub name: Box<str>,
    pub info: Box<dyn PnpBusInfo>,
    inner: Spinlock<PnpDeviceInner>,
    removal_lock: AtomicBool,
}

impl Debug for PnpDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner.lock();
        f.debug_struct("PnpDevice")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("bus_type", &self.info.bus_type())
            .field("state", &inner.state)
            .field("parent", &inner.parent)
            .field("children_count", &inner.children.len())
            .field("functions_count", &inner.functions.len())
            .field("driver", &inner.bound_driver.as_ref().map(|d| d.name()))
            .finish()
    }
}

impl PnpDevice {
    pub fn new(id: PnpId, name: Box<str>, info: Box<dyn PnpBusInfo>) -> Arc<Self> {
        Arc::new(Self {
            id,
            name,
            info,
            inner: Spinlock::new(PnpDeviceInner {
                state: PnpState::Discovered,
                parent: None,
                children: Vec::new(),
                functions: Vec::new(),
                bound_driver: None,
                driver_data: None,
            }),
            removal_lock: AtomicBool::new(false),
        })
    }

    pub fn state(&self) -> PnpState {
        self.inner.lock().state
    }

    /// 返回当前绑定驱动的名称。
    pub fn bound_driver_name(&self) -> Option<&'static str> {
        self.inner.lock().bound_driver.as_ref().map(|d| d.name())
    }

    /// 返回该设备已注册的 function 快照。
    pub fn functions(&self) -> Vec<Arc<dyn DeviceFunction>> {
        self.inner.lock().functions.iter().cloned().collect()
    }

    /// 返回子设备快照。
    pub fn children(&self) -> Vec<Arc<PnpDevice>> {
        self.inner.lock().children.clone()
    }

    /// 返回父设备；根设备没有父设备。
    pub fn parent(&self) -> Option<Arc<PnpDevice>> {
        self.inner.lock().parent.as_ref()?.upgrade()
    }

    /// 保存驱动私有数据。
    pub fn set_driver_data(&self, data: Arc<dyn Any + Send + Sync>) {
        self.inner.lock().driver_data = Some(data);
    }

    /// 取出并清空驱动私有数据。
    pub fn take_driver_data(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.inner.lock().driver_data.take()
    }

    // ── 父子关系 ──

    pub fn attach_child(self: &Arc<Self>, child: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let mut inner = self.inner.lock();
        if inner.state == PnpState::Gone || inner.state == PnpState::Removing {
            return Err(PnpError::InvalidState);
        }
        let mut child_inner = child.inner.lock();
        if child_inner.parent.is_some() {
            drop(child_inner);
            return Err(PnpError::InvalidState);
        }
        child_inner.parent = Some(Arc::downgrade(self));
        drop(child_inner);
        inner.children.push(Arc::clone(child));
        Ok(())
    }

    pub fn detach_child(self: &Arc<Self>, child: &Arc<PnpDevice>) {
        {
            let mut inner = self.inner.lock();
            inner
                .children
                .retain(|existing| !Arc::ptr_eq(existing, child));
        }
        let mut child_inner = child.inner.lock();
        if child_inner
            .parent
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|parent| Arc::ptr_eq(&parent, self))
        {
            child_inner.parent = None;
        }
    }

    pub fn attach_function(&self, func: Arc<dyn DeviceFunction>) -> Result<(), PnpError> {
        let dev_name = func.dev_name();
        let mut inner = self.inner.lock();
        if inner.state != PnpState::Probing {
            return Err(PnpError::InvalidState);
        }
        if inner
            .functions
            .iter()
            .any(|f| f.class_id() == func.class_id() && f.dev_name() == dev_name)
        {
            return Err(PnpError::FunctionExists);
        }
        inner.functions.push(func);
        Ok(())
    }

    fn transition(self: &Arc<Self>, from: PnpState, to: PnpState) -> Result<(), PnpError> {
        let mut inner = self.inner.lock();
        if inner.state != from {
            return Err(PnpError::InvalidState);
        }
        if !from.can_transition_to(to) {
            return Err(PnpError::InvalidTransition);
        }
        inner.state = to;
        Ok(())
    }
}

// ── PnpDriver ────────────────────────────────────────────────────────────

pub trait PnpDriver: Send + Sync {
    /// 驱动名称，用于日志、去重和调试输出。
    fn name(&self) -> &'static str;

    /// 驱动绑定的总线类型；返回 [`BusType::GENERIC`] 表示作为兜底驱动参与匹配。
    fn bus_type(&self) -> BusType;

    /// 判断该驱动是否支持给定 PnP 设备。
    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool;

    /// 初始化硬件并注册该设备暴露的 function。
    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError>;

    /// 移除设备时关闭硬件并释放驱动私有状态。
    fn remove(&self, dev: &Arc<PnpDevice>);
}

// ── DriverFactory / PnP 驱动注册表 ───────────────────────────────────────

/// 已注册驱动的运行时编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DriverId(u64);

impl DriverId {
    /// 返回编号的原始数值。
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// 驱动注册句柄。
///
/// 调用方用该句柄注销驱动，或触发该驱动对既有未绑定设备重新 probe。
pub struct DriverHandle {
    id: DriverId,
}

impl DriverHandle {
    /// 返回句柄内部的稳定驱动编号。
    pub const fn id(&self) -> DriverId {
        self.id
    }
}

pub trait DriverFactory: Send + Sync {
    /// factory 创建的驱动名称。
    fn name(&self) -> &'static str;
    /// 根据启动期上下文创建一个可注册的 PnP 驱动实例。
    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError>;
}

struct RegisteredDriver {
    id: DriverId,
    driver: Arc<dyn PnpDriver>,
}

/// PnP 驱动运行时注册表。
///
/// 内建驱动通过 factory 在启动时注册；后续如果需要动态驱动，也应进入同一张表。
/// 设备发现路径只调用 [`PnpDriverRegistry::probe_device`]，不关心驱动来源。
pub struct PnpDriverRegistry {
    next_driver_id: AtomicU64,
    drivers: Spinlock<Vec<RegisteredDriver>>,
}

impl PnpDriverRegistry {
    pub const fn new() -> Self {
        Self {
            next_driver_id: AtomicU64::new(1),
            drivers: Spinlock::new(Vec::new()),
        }
    }

    /// 注册一个驱动 factory 并立即创建驱动实例。
    pub fn register_factory(
        &self,
        factory: Arc<dyn DriverFactory>,
    ) -> Result<DriverHandle, PnpError> {
        let ctx = dev_init_context()?;
        let driver = factory.create(&ctx)?;
        let mut drivers = self.drivers.lock();
        if drivers
            .iter()
            .any(|registered| registered.driver.name() == driver.name())
        {
            return Err(PnpError::NameConflict);
        }
        drivers.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
        let id = DriverId(self.next_driver_id.fetch_add(1, Ordering::Relaxed));
        drivers.push(RegisteredDriver {
            id,
            driver: Arc::clone(&driver),
        });
        Ok(DriverHandle { id })
    }

    /// 从后续匹配中移除一个驱动。
    pub fn unregister(&self, handle: DriverHandle) -> Result<(), PnpError> {
        let mut drivers = self.drivers.lock();
        let pos = drivers
            .iter()
            .position(|registered| registered.id == handle.id)
            .ok_or(PnpError::NoDriver)?;
        drivers.swap_remove(pos);
        Ok(())
    }

    /// 用指定驱动重新尝试认领已经发现但尚未绑定的设备。
    pub fn probe_existing_devices(&self, driver_id: DriverId) -> Result<usize, PnpError> {
        let driver = self.driver_by_id(driver_id).ok_or(PnpError::NoDriver)?;
        let mut bound = 0usize;
        for dev in PNP_DEVICES.list() {
            if dev.state() != PnpState::Discovered {
                continue;
            }
            if !driver_can_probe_bus(driver.as_ref(), dev.info.bus_type()) {
                continue;
            }
            if !driver.matches(&dev.id, dev.info.as_ref()) {
                continue;
            }
            match self.bind_driver_to_device(&dev, Arc::clone(&driver)) {
                Ok(()) => bound += 1,
                Err(PnpError::InvalidState) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(bound)
    }

    /// 为一个新发现的设备寻找匹配驱动并执行 probe。
    pub fn probe_device(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let Some(driver) = self.find_matching_driver(dev) else {
            return Err(PnpError::NoDriver);
        };
        self.bind_driver_to_device(dev, driver)
    }

    fn bind_driver_to_device(
        &self,
        dev: &Arc<PnpDevice>,
        driver: Arc<dyn PnpDriver>,
    ) -> Result<(), PnpError> {
        dev.transition(PnpState::Discovered, PnpState::Probing)?;

        match driver.probe(dev) {
            Ok(()) => {
                let mut inner = dev.inner.lock();
                if inner.state != PnpState::Probing {
                    drop(inner);
                    dev.rollback_probe_side_effects();
                    return Err(PnpError::InvalidState);
                }
                inner.bound_driver = Some(driver);
                inner.state = PnpState::Bound;
                Ok(())
            }
            Err(err) => {
                dev.rollback_probe_side_effects();
                Err(err)
            }
        }
    }

    fn find_matching_driver(&self, dev: &Arc<PnpDevice>) -> Option<Arc<dyn PnpDriver>> {
        let drivers = self.drivers.lock();
        drivers
            .iter()
            .find(|registered| {
                registered.driver.bus_type() == dev.info.bus_type()
                    && registered.driver.matches(&dev.id, dev.info.as_ref())
            })
            .map(|registered| Arc::clone(&registered.driver))
            .or_else(|| {
                drivers
                    .iter()
                    .find(|registered| {
                        driver_is_generic(registered.driver.as_ref())
                            && registered.driver.matches(&dev.id, dev.info.as_ref())
                    })
                    .map(|registered| Arc::clone(&registered.driver))
            })
    }

    fn driver_by_id(&self, id: DriverId) -> Option<Arc<dyn PnpDriver>> {
        self.drivers
            .lock()
            .iter()
            .find(|registered| registered.id == id)
            .map(|registered| Arc::clone(&registered.driver))
    }
}

impl Default for PnpDriverRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn driver_is_generic(driver: &dyn PnpDriver) -> bool {
    driver.bus_type() == BusType::GENERIC
}

fn driver_can_probe_bus(driver: &dyn PnpDriver, bus_type: BusType) -> bool {
    driver.bus_type() == bus_type || driver_is_generic(driver)
}

/// 注册驱动 factory 的全局便捷入口。
pub fn register_driver_factory(factory: Arc<dyn DriverFactory>) -> Result<DriverHandle, PnpError> {
    PNP_DRIVERS.register_factory(factory)
}

/// 注销驱动的全局便捷入口。
pub fn unregister_driver(handle: DriverHandle) -> Result<(), PnpError> {
    PNP_DRIVERS.unregister(handle)
}

/// 让指定驱动认领当前尚未绑定的既有设备。
pub fn probe_existing_devices(driver_id: DriverId) -> Result<usize, PnpError> {
    PNP_DRIVERS.probe_existing_devices(driver_id)
}

// ── PnpDeviceList ────────────────────────────────────────────────────────

/// PnP 设备全局列表。
///
/// 该列表保存总线已经发现的设备对象。驱动绑定状态保存在每个 [`PnpDevice`]
/// 内部，因此列表只负责唯一性、查询和热拔移除。
pub struct PnpDeviceList {
    devices: Spinlock<Vec<Arc<PnpDevice>>>,
}

impl PnpDeviceList {
    pub const fn new() -> Self {
        Self {
            devices: Spinlock::new(Vec::new()),
        }
    }

    /// 插入一个新发现的设备。
    ///
    /// 同一个硬件身份在未进入 [`PnpState::Gone`] 前不能重复注册。
    pub fn push(&self, dev: Arc<PnpDevice>) -> Result<Arc<PnpDevice>, PnpError> {
        let mut list = self.devices.lock();
        list.retain(|d| d.state() != PnpState::Gone);
        if list
            .iter()
            .any(|d| d.id == dev.id && d.state() != PnpState::Gone)
        {
            return Err(PnpError::NameConflict);
        }
        list.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
        list.push(Arc::clone(&dev));
        Ok(dev)
    }

    /// 从全局列表中移除指定设备。
    pub fn remove(&self, id: &PnpId) -> Option<Arc<PnpDevice>> {
        let mut list = self.devices.lock();
        let pos = list.iter().position(|d| d.id == *id)?;
        Some(list.swap_remove(pos))
    }

    /// 按 PnP 硬件身份查找设备。
    pub fn lookup(&self, id: &PnpId) -> Option<Arc<PnpDevice>> {
        self.devices
            .lock()
            .iter()
            .find(|d| d.id == *id && d.state() != PnpState::Gone)
            .cloned()
    }

    /// 返回所有尚未 Gone 的设备快照。
    pub fn list(&self) -> Vec<Arc<PnpDevice>> {
        self.devices
            .lock()
            .iter()
            .filter(|d| d.state() != PnpState::Gone)
            .cloned()
            .collect()
    }
}

impl Default for PnpDeviceList {
    fn default() -> Self {
        Self::new()
    }
}

// ── devtmpfs 回调 ────────────────────────────────────────────────────────

/// PnP 与 devtmpfs 之间的最小桥接回调。
///
/// PnP core 不直接依赖 VFS；当 function 带有 [`DevNodeSet`] 时，通过这里安装的
/// 回调把节点创建委托给 devtmpfs。
pub struct PnpDevtmpfsCallbacks {
    pub bind: fn(&DevNodeSet) -> Result<(), PnpError>,
    pub unbind: fn(&DevNodeSet) -> Result<(), PnpError>,
}

static DEVTMPFS_CB: Spinlock<Option<PnpDevtmpfsCallbacks>> = Spinlock::new(None);

/// 安装 devtmpfs 桥接回调。
pub fn set_devtmpfs_callbacks(cb: PnpDevtmpfsCallbacks) {
    *DEVTMPFS_CB.lock() = Some(cb);
}

fn devtmpfs_bind_function(func: &Arc<dyn DeviceFunction>) -> Result<(), PnpError> {
    let Some(nodes) = func.devnodes() else {
        return Ok(());
    };
    let guard = DEVTMPFS_CB.lock();
    let cb = guard.as_ref().ok_or(PnpError::NoDevtmpfs)?;
    (cb.bind)(&nodes)
}

fn devtmpfs_unbind(nodes: &DevNodeSet) -> Result<(), PnpError> {
    let guard = DEVTMPFS_CB.lock();
    let cb = guard.as_ref().ok_or(PnpError::NoDevtmpfs)?;
    (cb.unbind)(nodes)
}

// ── 功能注册 helpers ────────────────────────────────────────────────────

impl PnpDevice {
    /// 事务式注册开放设备 function：DEVICES → devtmpfs → PnpDevice.attach。
    pub fn register_function(
        self: &Arc<Self>,
        func: Arc<dyn DeviceFunction>,
    ) -> Result<(), PnpError> {
        self.attach_function(Arc::clone(&func))?;

        if let Err(e) = DEVICES.register_function(Arc::clone(&func)) {
            self.detach_function(&func);
            func.mark_gone();
            return Err(e.into());
        }

        if let Err(e) = devtmpfs_bind_function(&func) {
            DEVICES.unregister_function(&func);
            self.detach_function(&func);
            func.mark_gone();
            return Err(e);
        }

        Ok(())
    }

    fn detach_function(&self, func: &Arc<dyn DeviceFunction>) {
        let mut inner = self.inner.lock();
        inner
            .functions
            .retain(|existing| !Arc::ptr_eq(existing, func));
    }

    fn unregister_function_external(&self, func: &Arc<dyn DeviceFunction>) {
        if let Some(nodes) = func.devnodes() {
            let _ = devtmpfs_unbind(&nodes);
        }
        DEVICES.unregister_function(func);
    }

    fn rollback_probe_side_effects(self: &Arc<Self>) {
        let (functions, children) = {
            let mut inner = self.inner.lock();
            inner.bound_driver = None;
            inner.driver_data = None;
            let functions = core::mem::take(&mut inner.functions);
            let children = inner.children.clone();
            if inner.state == PnpState::Probing {
                inner.state = PnpState::Discovered;
            }
            (functions, children)
        };

        for child in children.iter().rev() {
            child.remove_device();
        }

        for func in &functions {
            func.mark_gone();
            self.unregister_function_external(func);
        }
    }
}

// ── remove_device：热拔移除流程 ─────────────────────────────────────────

impl PnpDevice {
    /// 安全移除设备及其所有子设备。
    ///
    /// 流程：
    /// 1. 标记 Removing（阻止新 probe）
    /// 2. 递归移除子设备（深度优先，叶节点先移除）
    /// 3. 标记 function gone（阻止新 I/O）
    /// 4. 排空已有 I/O
    /// 5. 调用 driver.remove()（安全关闭硬件）
    /// 6. 从 devtmpfs 和 DEVICES 解绑
    /// 7. 标记 Gone
    /// 8. 从 PNP_DEVICES 移除
    pub fn remove_device(self: &Arc<Self>) {
        if self
            .removal_lock
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let current_state = self.state();
        if current_state == PnpState::Gone || current_state == PnpState::Removing {
            self.removal_lock.store(false, Ordering::Release);
            return;
        }

        {
            let mut inner = self.inner.lock();
            inner.state = PnpState::Removing;
        }

        // 阶段 2：递归移除子设备
        let children: Vec<Arc<PnpDevice>> = self.inner.lock().children.clone();
        for child in children.iter().rev() {
            child.remove_device();
        }

        // 阶段 3：标记 function gone
        let functions: Vec<Arc<dyn DeviceFunction>> = {
            let mut inner = self.inner.lock();
            core::mem::take(&mut inner.functions)
        };

        for func in &functions {
            func.mark_gone();
        }

        // 阶段 4：排空 I/O
        for func in &functions {
            func.drain_io();
        }

        // 阶段 5：调用 driver.remove
        let driver = { self.inner.lock().bound_driver.take() };
        if let Some(driver) = driver {
            driver.remove(self);
        }
        let _ = self.inner.lock().driver_data.take();

        // 阶段 6：解绑 devtmpfs 和 DEVICES
        for func in &functions {
            self.unregister_function_external(func);
        }

        // 阶段 7/8：标记 Gone 并从全局列表移除
        {
            let mut inner = self.inner.lock();
            inner.children.clear();
            inner.state = PnpState::Gone;
        }
        PNP_DEVICES.remove(&self.id);
        if let Some(parent) = self.parent() {
            parent.detach_child(self);
        }
    }
}

// ── 全局单例 ─────────────────────────────────────────────────────────────

pub static PNP_DEVICES: PnpDeviceList = PnpDeviceList::new();
pub static PNP_DRIVERS: PnpDriverRegistry = PnpDriverRegistry::new();

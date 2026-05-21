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
use core::sync::atomic::{AtomicBool, Ordering};

use vfs::sync::Spinlock;

use crate::dev::block::{BlockDevice, BlockRegistryError};
use crate::dev::char::{CharDevice, CharDeviceListError};
use crate::dev::enumerate::DEVICES;

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

impl From<BlockRegistryError> for PnpError {
    fn from(e: BlockRegistryError) -> Self {
        match e {
            BlockRegistryError::NameExists => PnpError::NameConflict,
            BlockRegistryError::DeviceGone => PnpError::InvalidState,
            BlockRegistryError::OutOfMemory => PnpError::OutOfMemory,
        }
    }
}

impl From<CharDeviceListError> for PnpError {
    fn from(e: CharDeviceListError) -> Self {
        match e {
            CharDeviceListError::NameExists => PnpError::NameConflict,
            CharDeviceListError::DeviceGone => PnpError::InvalidState,
            CharDeviceListError::OutOfMemory => PnpError::OutOfMemory,
        }
    }
}

// ── PnpId：硬件身份 ──────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PnpId {
    Pci {
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
    },
    Usb {
        bus_id: u8,
        address: u8,
        interface: Option<u8>,
    },
    Platform {
        name: Box<str>,
        index: u32,
    },
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
    pub fn bus_type(&self) -> &'static str {
        match self {
            PnpId::Pci { .. } => "pci",
            PnpId::Usb { .. } => "usb",
            PnpId::Platform { .. } => "platform",
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
            } => write!(f, "pci:{:04x}:{:02x}:{:02x}.{}", segment, bus, device, function),
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

// ── PnpBusInfo ───────────────────────────────────────────────────────────

pub trait PnpBusInfo: Send + Sync + Any + Debug {
    fn bus_type(&self) -> &'static str;
    fn as_any(&self) -> &dyn Any;
}

// ── PnpState ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PnpState {
    Discovered,
    Probing,
    Bound,
    Removing,
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

// ── PnpFunction ──────────────────────────────────────────────────────────

pub enum PnpFunction {
    Char {
        dev: CharDevice,
        dev_name: Box<str>,
    },
    Block {
        dev: Arc<BlockDevice>,
        dev_name: Box<str>,
    },
}

impl PnpFunction {
    pub fn dev_name(&self) -> &str {
        match self {
            PnpFunction::Char { dev_name, .. } => dev_name,
            PnpFunction::Block { dev_name, .. } => dev_name,
        }
    }

    pub fn function_type(&self) -> &'static str {
        match self {
            PnpFunction::Char { .. } => "char",
            PnpFunction::Block { .. } => "block",
        }
    }

    fn mark_gone(&self) {
        match self {
            PnpFunction::Char { dev, .. } => dev.mark_gone(),
            PnpFunction::Block { dev, .. } => dev.mark_gone(),
        }
    }

    fn drain_io(&self) {
        if let PnpFunction::Block { dev, .. } = self {
            while dev.in_flight() > 0 {
                dev.poll();
                core::hint::spin_loop();
            }
        }
    }
}

// ── PnpDevice ────────────────────────────────────────────────────────────

struct PnpDeviceInner {
    state: PnpState,
    parent: Option<Weak<PnpDevice>>,
    children: Vec<Arc<PnpDevice>>,
    functions: Vec<PnpFunction>,
    bound_driver: Option<&'static dyn PnpDriver>,
    driver_data: Option<Arc<dyn Any + Send + Sync>>,
}

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
            .field("driver", &inner.bound_driver.map(|d| d.name()))
            .finish()
    }
}

impl PnpDevice {
    pub fn new(
        id: PnpId,
        name: Box<str>,
        info: Box<dyn PnpBusInfo>,
    ) -> Arc<Self> {
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

    pub fn bound_driver_name(&self) -> Option<&'static str> {
        self.inner.lock().bound_driver.map(|d| d.name())
    }

    pub fn functions(&self) -> Vec<PnpFunction> {
        self.inner
            .lock()
            .functions
            .iter()
            .map(|f| match f {
                PnpFunction::Char { dev, dev_name } => PnpFunction::Char {
                    dev: dev.clone(),
                    dev_name: dev_name.clone(),
                },
                PnpFunction::Block { dev, dev_name } => PnpFunction::Block {
                    dev: Arc::clone(dev),
                    dev_name: dev_name.clone(),
                },
            })
            .collect()
    }

    pub fn children(&self) -> Vec<Arc<PnpDevice>> {
        self.inner.lock().children.clone()
    }

    pub fn parent(&self) -> Option<Arc<PnpDevice>> {
        self.inner.lock().parent.as_ref()?.upgrade()
    }

    pub fn set_driver_data(&self, data: Arc<dyn Any + Send + Sync>) {
        self.inner.lock().driver_data = Some(data);
    }

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

    pub fn attach_function(&self, func: PnpFunction) -> Result<(), PnpError> {
        let dev_name = func.dev_name();
        let mut inner = self.inner.lock();
        if inner.state != PnpState::Probing {
            return Err(PnpError::InvalidState);
        }
        if inner.functions.iter().any(|f| f.dev_name() == dev_name) {
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
    fn name(&self) -> &'static str;

    fn bus_type(&self) -> &'static str;

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool;

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError>;

    fn remove(&self, dev: &Arc<PnpDevice>);
}

// ── PnpDriverRegistry ────────────────────────────────────────────────────

pub struct PnpDriverRegistry {
    pci_drivers: Spinlock<Vec<&'static dyn PnpDriver>>,
    usb_drivers: Spinlock<Vec<&'static dyn PnpDriver>>,
    platform_drivers: Spinlock<Vec<&'static dyn PnpDriver>>,
    generic_drivers: Spinlock<Vec<&'static dyn PnpDriver>>,
}

impl PnpDriverRegistry {
    pub const fn new() -> Self {
        Self {
            pci_drivers: Spinlock::new(Vec::new()),
            usb_drivers: Spinlock::new(Vec::new()),
            platform_drivers: Spinlock::new(Vec::new()),
            generic_drivers: Spinlock::new(Vec::new()),
        }
    }

    pub fn register(&self, driver: &'static dyn PnpDriver) {
        match driver.bus_type() {
            "pci" => self.pci_drivers.lock().push(driver),
            "usb" => self.usb_drivers.lock().push(driver),
            "platform" => self.platform_drivers.lock().push(driver),
            _ => self.generic_drivers.lock().push(driver),
        }
    }

    pub fn probe_device(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        dev.transition(PnpState::Discovered, PnpState::Probing)?;

        let driver = {
            let candidates = self.drivers_for_bus(dev.info.bus_type());
            let guard = candidates.lock();
            let found = guard
                .iter()
                .copied()
                .find(|d| d.matches(&dev.id, dev.info.as_ref()))
                .or_else(|| {
                    drop(guard);
                    // 回退到 generic 驱动
                    let generic = self.generic_drivers.lock();
                    generic
                        .iter()
                        .copied()
                        .find(|d| d.matches(&dev.id, dev.info.as_ref()))
                });

            match found {
                Some(d) => d,
                None => {
                    let _ = dev.transition(PnpState::Probing, PnpState::Discovered);
                    return Err(PnpError::NoDriver);
                }
            }
        };

        match driver.probe(dev) {
            Ok(()) => {
                dev.inner.lock().bound_driver = Some(driver);
                dev.transition(PnpState::Probing, PnpState::Bound)?;
                Ok(())
            }
            Err(err) => {
                let drained: Vec<PnpFunction> =
                    dev.inner.lock().functions.drain(..).collect();
                for func in &drained {
                    func.mark_gone();
                    dev.unregister_function_external(func);
                }
                let _ = dev.transition(PnpState::Probing, PnpState::Discovered);
                Err(err)
            }
        }
    }

    fn drivers_for_bus(&self, bus_type: &str) -> &Spinlock<Vec<&'static dyn PnpDriver>> {
        match bus_type {
            "pci" => &self.pci_drivers,
            "usb" => &self.usb_drivers,
            "platform" => &self.platform_drivers,
            _ => &self.generic_drivers,
        }
    }
}

impl Default for PnpDriverRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── PnpDeviceList ────────────────────────────────────────────────────────

pub struct PnpDeviceList {
    devices: Spinlock<Vec<Arc<PnpDevice>>>,
}

impl PnpDeviceList {
    pub const fn new() -> Self {
        Self {
            devices: Spinlock::new(Vec::new()),
        }
    }

    pub fn push(&self, dev: Arc<PnpDevice>) -> Result<Arc<PnpDevice>, PnpError> {
        let mut list = self.devices.lock();
        list.retain(|d| d.state() != PnpState::Gone);
        if list.iter().any(|d| d.id == dev.id && d.state() != PnpState::Gone) {
            return Err(PnpError::NameConflict);
        }
        list.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
        list.push(Arc::clone(&dev));
        Ok(dev)
    }

    pub fn remove(&self, id: &PnpId) -> Option<Arc<PnpDevice>> {
        let mut list = self.devices.lock();
        let pos = list.iter().position(|d| d.id == *id)?;
        Some(list.swap_remove(pos))
    }

    pub fn lookup(&self, id: &PnpId) -> Option<Arc<PnpDevice>> {
        self.devices
            .lock()
            .iter()
            .find(|d| d.id == *id && d.state() != PnpState::Gone)
            .cloned()
    }

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

pub struct PnpDevtmpfsCallbacks {
    pub bind_block: fn(&str, Arc<BlockDevice>) -> Result<(), PnpError>,
    pub bind_char: fn(&str, CharDevice) -> Result<(), PnpError>,
    pub unbind: fn(&str) -> Result<(), PnpError>,
}

static DEVTMPFS_CB: Spinlock<Option<PnpDevtmpfsCallbacks>> = Spinlock::new(None);

pub fn set_devtmpfs_callbacks(cb: PnpDevtmpfsCallbacks) {
    *DEVTMPFS_CB.lock() = Some(cb);
}

fn devtmpfs_bind_block(dev_name: &str, dev: Arc<BlockDevice>) -> Result<(), PnpError> {
    let guard = DEVTMPFS_CB.lock();
    let cb = guard.as_ref().ok_or(PnpError::NoDevtmpfs)?;
    (cb.bind_block)(dev_name, dev)
}

fn devtmpfs_bind_char(dev_name: &str, dev: CharDevice) -> Result<(), PnpError> {
    let guard = DEVTMPFS_CB.lock();
    let cb = guard.as_ref().ok_or(PnpError::NoDevtmpfs)?;
    (cb.bind_char)(dev_name, dev)
}

fn devtmpfs_unbind(dev_name: &str) -> Result<(), PnpError> {
    let guard = DEVTMPFS_CB.lock();
    let cb = guard.as_ref().ok_or(PnpError::NoDevtmpfs)?;
    (cb.unbind)(dev_name)
}

// ── 功能注册 helpers ────────────────────────────────────────────────────

impl PnpDevice {
    /// 事务式注册块设备 function：Devices → devtmpfs → PnpDevice.attach
    ///
    /// 任一步骤失败则回滚已完成的步骤。
    pub fn register_block_function(
        self: &Arc<Self>,
        dev_name: &str,
        block: Arc<BlockDevice>,
    ) -> Result<(), PnpError> {
        DEVICES.block_devs.push(&block)?;

        if let Err(e) = devtmpfs_bind_block(dev_name, Arc::clone(&block)) {
            DEVICES.block_devs.remove(dev_name);
            return Err(e);
        }

        let func = PnpFunction::Block {
            dev: block,
            dev_name: dev_name.into(),
        };

        if let Err(e) = self.attach_function(func) {
            let _ = devtmpfs_unbind(dev_name);
            DEVICES.block_devs.remove(dev_name);
            return Err(e);
        }

        Ok(())
    }

    /// 事务式注册字符设备 function：Devices → devtmpfs → PnpDevice.attach
    ///
    /// 任一步骤失败则回滚已完成的步骤。
    pub fn register_char_function(
        self: &Arc<Self>,
        dev_name: &str,
        ch: CharDevice,
    ) -> Result<(), PnpError> {
        DEVICES.char_devs.push(ch.clone())?;

        if let Err(e) = devtmpfs_bind_char(dev_name, ch.clone()) {
            DEVICES.char_devs.remove(dev_name);
            return Err(e);
        }

        let func = PnpFunction::Char {
            dev: ch,
            dev_name: dev_name.into(),
        };

        if let Err(e) = self.attach_function(func) {
            let _ = devtmpfs_unbind(dev_name);
            DEVICES.char_devs.remove(dev_name);
            return Err(e);
        }

        Ok(())
    }

    fn unregister_function_external(&self, func: &PnpFunction) {
        let dev_name = func.dev_name();
        let _ = devtmpfs_unbind(dev_name);
        match func {
            PnpFunction::Char { .. } => {
                DEVICES.char_devs.remove(dev_name);
            }
            PnpFunction::Block { .. } => {
                DEVICES.block_devs.remove(dev_name);
            }
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
        if current_state != PnpState::Bound && current_state != PnpState::Probing {
            self.removal_lock.store(false, Ordering::Release);
            return;
        }

        {
            let mut inner = self.inner.lock();
            inner.state = PnpState::Removing;
        }

        // Phase 2: 递归移除子设备
        let children: Vec<Arc<PnpDevice>> = self.inner.lock().children.clone();
        for child in children.iter().rev() {
            child.remove_device();
        }

        // Phase 3: 标记 function gone
        let functions: Vec<PnpFunction> = {
            let mut inner = self.inner.lock();
            core::mem::take(&mut inner.functions)
        };

        for func in &functions {
            func.mark_gone();
        }

        // Phase 4: 排空 I/O
        for func in &functions {
            func.drain_io();
        }

        // Phase 5: 调用 driver.remove
        let driver = { self.inner.lock().bound_driver.take() };
        if let Some(driver) = driver {
            driver.remove(self);
        }
        let _ = self.inner.lock().driver_data.take();

        // Phase 6: 解绑 devtmpfs 和 DEVICES
        for func in &functions {
            self.unregister_function_external(func);
        }

        // Phase 7/8: 标记 Gone 并从全局列表移除
        {
            let mut inner = self.inner.lock();
            inner.state = PnpState::Gone;
        }
        PNP_DEVICES.remove(&self.id);
    }
}

// ── 全局单例 ─────────────────────────────────────────────────────────────

pub static PNP_DEVICES: PnpDeviceList = PnpDeviceList::new();
pub static PNP_DRIVERS: PnpDriverRegistry = PnpDriverRegistry::new();

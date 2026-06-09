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
//!          probe 失败回退       └── 驱动注销 ──────────→ Discovered
//!                                      │
//!                                      └── 硬件移除 ───→ Gone
//! ```
//!
//! # 热插拔
//!
//! 设备可以在任意时刻被创建（`PnpDevice::new` + `PNP_DEVICES.get_or_insert` +
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
use crate::dev::function::{
    DevNodeNameAllocError, DevNodeSet, DeviceFunction, FunctionRegistryError,
};
use crate::dev::registry_id;

// ── PnP 错误类型 ─────────────────────────────────────────────────────────

/// PnP core 认识的资源类别。
///
/// 这里描述的是设备管理层资源，不是 POSIX 设备号或 `/dev` 节点。驱动 probe
/// 失败时应尽量带上具体类别，便于 deferred probe 和启动日志定位真实缺口。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PnpResourceKind {
    Mmio,
    Irq,
    IrqDomain,
    Msi,
    MsiController,
    Syscon,
    Flash,
    FwCfg,
    FirmwareBus,
    PciHostBridge,
    Dma,
    Function,
    Other(&'static str),
}

/// deferred probe 的精确依赖键。
///
/// 设备返回该依赖后，PnP core 会记录“缺的是哪个资源”，对应资源登记成功时
/// 可以只重试受影响设备，而不是无条件扫描所有 Discovered 设备。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PnpDependency {
    IrqController(u32),
    DefaultIrqDomain,
    MsiController(u32),
    Syscon(u32),
    FwCfg,
    FirmwareBus,
    PciHostBridge(u16),
    Dma,
    Other(&'static str),
}

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
    /// probe 依赖暂未就绪，设备应保留为 Discovered 等待后续重试
    ProbeDeferred,
    /// 多个驱动以相同优先级匹配同一设备
    DriverAmbiguous,
    /// 同名 function 已存在
    FunctionExists,
    /// 设备名冲突
    NameConflict,
    /// devtmpfs 未就绪
    /// FIXME(dev-core): 这是 VFS 投影层错误泄漏到 PnP core 的遗留接口。
    /// 后续应由独立 projection manager 记录 `/dev` 发布失败，不能让底层设备
    /// 生命周期直接依赖 devtmpfs 是否安装。
    NoDevtmpfs,
    /// devtmpfs 操作失败
    /// FIXME(dev-core): 同上，devtmpfs 的具体错误需要留在 VFS/兼容层，并通过
    /// 诊断事件或 projection 状态暴露给 sysfs/procfs。
    DevtmpfsError,
    /// 内存不足
    OutOfMemory,
    /// 固件或总线没有提供驱动必需的资源。
    MissingResource {
        kind: PnpResourceKind,
        detail: &'static str,
    },
    /// 固件资源存在，但格式或取值不满足该资源的解析规则。
    MalformedResource {
        kind: PnpResourceKind,
        detail: &'static str,
    },
    /// probe 依赖的其它资源尚未登记，设备应等待该依赖就绪后精准重试。
    DependencyNotReady(PnpDependency),
    /// 资源登记失败，但不属于单纯内存不足或命名冲突。
    RegistrationFailed {
        kind: PnpResourceKind,
        detail: &'static str,
    },
    /// 设备或平台明确不支持驱动请求的能力。
    Unsupported { feature: &'static str },
    /// 硬件访问失败或返回了不可恢复的异常状态。
    HardwareFailure { detail: &'static str },
}

impl PnpError {
    pub const fn missing(kind: PnpResourceKind, detail: &'static str) -> Self {
        Self::MissingResource { kind, detail }
    }

    pub const fn malformed(kind: PnpResourceKind, detail: &'static str) -> Self {
        Self::MalformedResource { kind, detail }
    }

    pub const fn dependency(dependency: PnpDependency) -> Self {
        Self::DependencyNotReady(dependency)
    }

    pub const fn registration_failed(kind: PnpResourceKind, detail: &'static str) -> Self {
        Self::RegistrationFailed { kind, detail }
    }

    pub const fn unsupported(feature: &'static str) -> Self {
        Self::Unsupported { feature }
    }

    pub const fn hardware_failure(detail: &'static str) -> Self {
        Self::HardwareFailure { detail }
    }

    pub const fn is_deferred(self) -> bool {
        matches!(self, Self::ProbeDeferred | Self::DependencyNotReady(_))
    }

    pub const fn deferred_dependency(self) -> Option<PnpDependency> {
        match self {
            Self::DependencyNotReady(dependency) => Some(dependency),
            _ => None,
        }
    }
}

impl From<FunctionRegistryError> for PnpError {
    fn from(e: FunctionRegistryError) -> Self {
        match e {
            FunctionRegistryError::NameExists => PnpError::NameConflict,
            FunctionRegistryError::OutOfMemory => PnpError::OutOfMemory,
        }
    }
}

impl From<DevNodeNameAllocError> for PnpError {
    fn from(e: DevNodeNameAllocError) -> Self {
        match e {
            DevNodeNameAllocError::OutOfMemory => PnpError::OutOfMemory,
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
    Platform {
        name: Box<str>,
        identity: PlatformIdentity,
    },
}

/// platform 设备的完整固件身份。
///
/// platform 设备不像 PCI 那样天然拥有 BDF。这里保留固件路径、match id 和资源
/// tuple 本身，而不是把它们压成整数 hash；这样 PnP 去重不会受 hash 碰撞影响，
/// 诊断信息也能追溯到原始固件节点。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlatformIdentity {
    firmware_path: Option<Box<str>>,
    parent_path: Option<Box<str>>,
    match_ids: Box<[PlatformIdentityMatchId]>,
    resources: Box<[PlatformIdentityResource]>,
}

impl PlatformIdentity {
    pub fn new(
        firmware_path: Option<Box<str>>,
        parent_path: Option<Box<str>>,
        match_ids: Box<[PlatformIdentityMatchId]>,
        resources: Box<[PlatformIdentityResource]>,
    ) -> Self {
        Self {
            firmware_path,
            parent_path,
            match_ids,
            resources,
        }
    }

    pub fn firmware_path(&self) -> Option<&str> {
        self.firmware_path.as_deref()
    }

    pub fn parent_path(&self) -> Option<&str> {
        self.parent_path.as_deref()
    }

    pub fn match_ids(&self) -> &[PlatformIdentityMatchId] {
        &self.match_ids
    }

    pub fn resources(&self) -> &[PlatformIdentityResource] {
        &self.resources
    }
}

/// platform 设备参与身份判等的固件匹配 id。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PlatformIdentityMatchId {
    DtbCompatible(Box<str>),
    AcpiHid(Box<str>),
    AcpiCid(Box<str>),
}

/// platform IRQ 资源参与身份判等的触发方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlatformIdentityIrqTrigger {
    Edge,
    Level,
}

/// platform IRQ 资源参与身份判等的极性。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlatformIdentityIrqPolarity {
    ActiveHigh,
    ActiveLow,
}

/// platform IRQ 资源参与身份判等的共享策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlatformIdentityIrqSharing {
    Exclusive,
    Shared,
}

/// platform IRQ 资源参与身份判等的通用属性。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PlatformIdentityIrqAttributes {
    pub trigger: Option<PlatformIdentityIrqTrigger>,
    pub polarity: Option<PlatformIdentityIrqPolarity>,
    pub sharing: Option<PlatformIdentityIrqSharing>,
    pub wake_capable: bool,
}

/// platform 设备参与身份判等的固件资源。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PlatformIdentityResource {
    Mmio {
        phys: usize,
        size: usize,
    },
    Irq {
        controller: Option<u32>,
        cells: Box<[u32]>,
        attributes: PlatformIdentityIrqAttributes,
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
            PnpId::Platform { name, identity } => {
                name.as_ref().hash(state);
                identity.hash(state);
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
            PnpId::Platform { name, identity } => {
                if let Some(path) = identity.firmware_path() {
                    write!(f, "platform:{}@{}", name, path)
                } else {
                    write!(
                        f,
                        "platform:{}[ids={},resources={}]",
                        name,
                        identity.match_ids().len(),
                        identity.resources().len()
                    )
                }
            }
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

/// 可撤销的 realtime 时钟源声明。
///
/// `id` 必须在当前启动期间唯一且非 0，通常可由 MMIO 物理基址派生。安装
/// hook 接受后，驱动在 remove 时必须用同一个 `id` 调 unregister。安全语义
/// 上，这只表示“这个 RTC 仍是当前可信来源”；卸载时不回滚已经设置的 realtime
/// offset，避免拔掉 RTC 后时间倒退，只允许后续替代 RTC 接管来源身份。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RealtimeClockSource {
    pub id: usize,
    pub name: &'static str,
    pub realtime_ns: u64,
}

/// 驱动 factory 创建内建驱动实例时需要的启动期能力。
///
/// 该上下文由内核启动路径在注册内建驱动前设置。它只包含内建驱动初始化所需的
/// MMIO 映射和时钟来源回调，不把固件解析、总线扫描或 DMA 地址策略暴露给
/// 驱动 catalog。DMA 地址统一通过 [`crate::dev::dma`] 的平台 hook 管理。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DevInitContext {
    /// 将设备 MMIO 物理地址转换为可访问的内核虚拟地址。
    pub device_mmio_to_virt: fn(usize) -> usize,
    /// 用硬件 RTC 读出的 Unix 纳秒时间更新内核 realtime 时钟。
    pub set_realtime_ns: Option<fn(u64)>,
    /// 安装一个可撤销 realtime 来源。返回 `true` 表示本来源成为当前 owner。
    pub install_realtime_source: Option<fn(RealtimeClockSource) -> bool>,
    /// 注销一个已安装 realtime 来源。只有 owner id 匹配时才应生效。
    pub unregister_realtime_source: Option<fn(usize)>,
}

impl DevInitContext {
    pub const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
            set_realtime_ns: None,
            install_realtime_source: None,
            unregister_realtime_source: None,
        }
    }

    pub const fn with_realtime_clock(mut self, set_realtime_ns: fn(u64)) -> Self {
        self.set_realtime_ns = Some(set_realtime_ns);
        self
    }

    pub const fn with_realtime_source_hooks(
        mut self,
        install_realtime_source: fn(RealtimeClockSource) -> bool,
        unregister_realtime_source: fn(usize),
    ) -> Self {
        self.install_realtime_source = Some(install_realtime_source);
        self.unregister_realtime_source = Some(unregister_realtime_source);
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
    /// 正在执行热拔或驱动解绑流程。
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
                | (Removing, Discovered)
                | (Removing, Gone)
        )
    }
}

// ── PnP-owned resource ──────────────────────────────────────────────────

/// PnP 设备拥有的可回收资源。
///
/// 驱动在 probe 期间通过 [`PnpDevice::own_resource`] 把 IRQ/MSI/syscon 等
/// registry handle 交给 PnP core。core 在 probe 回滚、驱动解绑和热拔时按
/// LIFO 顺序释放，避免每个驱动重复手写清理路径。
pub trait PnpResource: Send {
    /// 资源类别，用于日志和错误定位。
    fn kind(&self) -> PnpResourceKind;
    /// 资源诊断标签，必须是稳定静态字符串，不能依赖用户态 ABI 名称。
    fn label(&self) -> &'static str;
    /// 释放资源。实现必须允许底层资源已被提前释放并安全返回错误。
    fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError>;
}

/// PnP 设备当前拥有资源的只读快照。
///
/// sysfs/procfs 等诊断视图只能观察资源类别和稳定标签，不能拿到底层 handle；
/// handle 的释放所有权仍然完全留在 PnP core 内部，避免兼容层破坏 remove 事务。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PnpOwnedResourceSnapshot {
    pub kind: PnpResourceKind,
    pub label: &'static str,
}

/// PnP resource 自动释放失败。
///
/// release 失败不会阻止 remove 流程继续；它只进入日志，提示存在重复释放、
/// handle 代次不匹配或底层 registry 状态异常。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PnpResourceReleaseError {
    pub kind: PnpResourceKind,
    pub label: &'static str,
    pub detail: &'static str,
}

impl PnpResourceReleaseError {
    pub const fn new(kind: PnpResourceKind, label: &'static str, detail: &'static str) -> Self {
        Self {
            kind,
            label,
            detail,
        }
    }
}

/// 小型 handle resource 包装器。
///
/// 大多数设备资源都是“注册函数返回 handle、注销函数消费 handle”的形态。这个
/// 包装器让驱动不需要为每一种 handle 写新的资源类型。
pub struct PnpHandleResource<H>
where
    H: Copy + Send + 'static,
{
    kind: PnpResourceKind,
    label: &'static str,
    handle: H,
    release: fn(H) -> bool,
}

impl<H> PnpHandleResource<H>
where
    H: Copy + Send + 'static,
{
    pub const fn new(
        kind: PnpResourceKind,
        label: &'static str,
        handle: H,
        release: fn(H) -> bool,
    ) -> Self {
        Self {
            kind,
            label,
            handle,
            release,
        }
    }
}

impl<H> PnpResource for PnpHandleResource<H>
where
    H: Copy + Send + 'static,
{
    fn kind(&self) -> PnpResourceKind {
        self.kind
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        if (self.release)(self.handle) {
            Ok(())
        } else {
            Err(PnpResourceReleaseError::new(
                self.kind,
                self.label,
                "resource release callback reported failure",
            ))
        }
    }
}

fn release_pnp_resources(mut resources: Vec<Box<dyn PnpResource>>, owner: &PnpId) {
    while let Some(resource) = resources.pop() {
        let kind = resource.kind();
        let label = resource.label();
        if let Err(err) = resource.release() {
            log::error!(
                "[pnp] failed to release {:?} resource {} for {}: {}",
                kind,
                label,
                owner,
                err.detail
            );
        }
    }
}

// ── PnpDevice ────────────────────────────────────────────────────────────

struct PnpDeviceInner {
    state: PnpState,
    parent: Option<Weak<PnpDevice>>,
    children: Vec<Arc<PnpDevice>>,
    functions: Vec<Arc<dyn DeviceFunction>>,
    resources: Vec<Box<dyn PnpResource>>,
    bound_driver: Option<Arc<dyn PnpDriver>>,
    driver_data: Option<Arc<dyn Any + Send + Sync>>,
    deferred_dependency: Option<PnpDependency>,
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
            .field("resources_count", &inner.resources.len())
            .field("deferred_dependency", &inner.deferred_dependency)
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
                resources: Vec::new(),
                bound_driver: None,
                driver_data: None,
                deferred_dependency: None,
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

    fn bound_to_driver(&self, driver: &Arc<dyn PnpDriver>) -> bool {
        self.inner
            .lock()
            .bound_driver
            .as_ref()
            .is_some_and(|bound| Arc::ptr_eq(bound, driver))
    }

    /// 返回该设备已注册的 function 快照。
    pub fn try_functions(&self) -> Option<Vec<Arc<dyn DeviceFunction>>> {
        let inner = self.inner.lock();
        let mut out = Vec::new();
        // function 快照会被 procfs/sysfs 诊断路径读取；显式预留可把 OOM
        // 表达为快照缺失，而不是在持锁 collect 时 panic。
        out.try_reserve(inner.functions.len()).ok()?;
        out.extend(inner.functions.iter().cloned());
        Some(out)
    }

    /// 返回该设备已注册的 function 快照。
    pub fn functions(&self) -> Vec<Arc<dyn DeviceFunction>> {
        self.try_functions().unwrap_or_default()
    }

    /// 返回子设备快照。
    pub fn try_children(&self) -> Option<Vec<Arc<PnpDevice>>> {
        let inner = self.inner.lock();
        let mut out = Vec::new();
        out.try_reserve(inner.children.len()).ok()?;
        out.extend(inner.children.iter().cloned());
        Some(out)
    }

    /// 返回子设备快照。
    pub fn children(&self) -> Vec<Arc<PnpDevice>> {
        self.try_children().unwrap_or_default()
    }

    /// 返回最近一次 deferred probe 记录的精确依赖。
    pub fn deferred_dependency(&self) -> Option<PnpDependency> {
        self.inner.lock().deferred_dependency
    }

    /// 返回该设备已交给 PnP core 管理的资源快照。
    pub fn try_owned_resources(&self) -> Option<Vec<PnpOwnedResourceSnapshot>> {
        let inner = self.inner.lock();
        let mut out = Vec::new();
        out.try_reserve(inner.resources.len()).ok()?;
        out.extend(
            inner
                .resources
                .iter()
                .map(|resource| PnpOwnedResourceSnapshot {
                    kind: resource.kind(),
                    label: resource.label(),
                }),
        );
        Some(out)
    }

    /// 返回该设备已交给 PnP core 管理的资源快照。
    pub fn owned_resources(&self) -> Vec<PnpOwnedResourceSnapshot> {
        self.try_owned_resources().unwrap_or_default()
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
        // PnP 拓扑必须保持有向无环：设备不能把自己挂成子节点，也不能把祖先
        // 重新挂到自己下面。否则 remove/unbind 的递归清理会形成无限递归。
        if Arc::ptr_eq(self, child) || self.has_ancestor(child) {
            return Err(PnpError::InvalidState);
        }

        let mut inner = self.inner.lock();
        if inner.state == PnpState::Gone || inner.state == PnpState::Removing {
            return Err(PnpError::InvalidState);
        }
        // 父子关系是 remove/unbind 递归清理的基础结构，插入前先完成容量预留，
        // 避免 OOM 时已经写入 child.parent 却没有进入 parent.children。
        inner
            .children
            .try_reserve(1)
            .map_err(|_| PnpError::OutOfMemory)?;
        let mut child_inner = child.inner.lock();
        // 正在移除或已经 Gone 的设备不能重新进入拓扑；这类对象的 function 和
        // driver_data 正在被清理，重新挂接会破坏热拔生命周期。
        if child_inner.state == PnpState::Gone || child_inner.state == PnpState::Removing {
            return Err(PnpError::InvalidState);
        }
        if child_inner.parent.is_some() {
            drop(child_inner);
            return Err(PnpError::InvalidState);
        }
        child_inner.parent = Some(Arc::downgrade(self));
        drop(child_inner);
        inner.children.push(Arc::clone(child));
        Ok(())
    }

    fn has_ancestor(&self, needle: &Arc<PnpDevice>) -> bool {
        let mut current = self.parent();
        while let Some(parent) = current {
            if Arc::ptr_eq(&parent, needle) {
                return true;
            }
            current = parent.parent();
        }
        false
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
        // function 注册后会继续进入 DEVICES 和 devtmpfs；这里先预留空间，
        // 确保 attach 阶段失败时不会留下半注册状态。
        inner
            .functions
            .try_reserve(1)
            .map_err(|_| PnpError::OutOfMemory)?;
        inner.functions.push(func);
        Ok(())
    }

    /// 将一个资源交给当前 PnP 设备拥有。
    ///
    /// 允许在 probe 期间登记，也允许已绑定驱动后登记运行期资源。core 会在 probe
    /// 回滚、驱动解绑和热拔时按登记反序释放，驱动不应再手写同一个 handle 的注销。
    pub fn own_resource<R>(&self, resource: R) -> Result<(), PnpError>
    where
        R: PnpResource + 'static,
    {
        let mut inner = self.inner.lock();
        if !matches!(inner.state, PnpState::Probing | PnpState::Bound) {
            return Err(PnpError::InvalidState);
        }
        inner
            .resources
            .try_reserve(1)
            .map_err(|_| PnpError::OutOfMemory)?;
        inner.resources.push(Box::new(resource));
        Ok(())
    }

    fn set_deferred_dependency(&self, dependency: Option<PnpDependency>) {
        self.inner.lock().deferred_dependency = dependency;
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

    /// 该驱动在同类匹配中的优先级。
    ///
    /// PnP core 先选择设备所属总线的驱动，再考虑 [`BusType::GENERIC`] 兜底；
    /// 在同一层级内，优先级高的驱动胜出。驱动需要覆盖默认值时，应只表达
    /// 自身匹配策略的强弱，不应依赖内建 catalog 的注册顺序。
    fn priority(&self) -> PnpDriverPriority {
        PnpDriverPriority::DEFAULT
    }

    /// 判断该驱动是否支持给定 PnP 设备。
    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool;

    /// 初始化硬件并注册该设备暴露的 function。
    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError>;

    /// 移除设备时关闭硬件并释放驱动私有状态。
    fn remove(&self, dev: &Arc<PnpDevice>);
}

// ── DriverFactory / PnP 驱动注册表 ───────────────────────────────────────

/// PnP 驱动匹配优先级。
///
/// 该值只在多个驱动同时匹配同一设备、且 bus 层级相同时参与比较。它把“谁更
/// 具体”显式写进驱动能力，而不是让注册顺序成为隐藏策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PnpDriverPriority(i16);

impl PnpDriverPriority {
    /// 兜底或弱匹配驱动使用的优先级。
    pub const FALLBACK: Self = Self(-100);
    /// 普通精确驱动的默认优先级。
    pub const DEFAULT: Self = Self(0);
    /// 更具体的驱动可使用的高优先级。
    pub const SPECIFIC: Self = Self(100);

    /// 构造一个自定义优先级。
    pub const fn new(raw: i16) -> Self {
        Self(raw)
    }

    /// 返回原始数值，供日志或诊断使用。
    pub const fn raw(self) -> i16 {
        self.0
    }
}

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

fn alloc_driver_id(next_id: &AtomicU64) -> Result<DriverId, PnpError> {
    // DriverHandle 可能跨热拔、驱动注销和重新注册流程存活，编号一旦发出就不能复用。
    registry_id::alloc_atomic_id(next_id)
        .map(DriverId)
        .map_err(|_| PnpError::OutOfMemory)
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
    retrying_deferred: AtomicBool,
    drivers: Spinlock<Vec<RegisteredDriver>>,
}

impl PnpDriverRegistry {
    pub const fn new() -> Self {
        Self {
            next_driver_id: AtomicU64::new(1),
            retrying_deferred: AtomicBool::new(false),
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
        let driver_name = driver.name();
        let id = {
            let mut drivers = self.drivers.lock();
            if drivers
                .iter()
                .any(|registered| registered.driver.name() == driver_name)
            {
                return Err(PnpError::NameConflict);
            }
            drivers.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
            let id = alloc_driver_id(&self.next_driver_id)?;
            drivers.push(RegisteredDriver {
                id,
                driver: Arc::clone(&driver),
            });
            id
        };

        // 新驱动注册后立即尝试认领已经枚举但未绑定的设备。probe 失败属于单个
        // 设备的运行时状态，不应反向撤销驱动注册；后续依赖就绪或手动 retry
        // 仍可再次进入同一条 PnP 绑定路径。
        match self.probe_existing_devices(id) {
            Ok(bound) if bound != 0 => {
                log::debug!(
                    "[pnp] driver {} claimed {} existing device(s)",
                    driver_name,
                    bound
                );
            }
            Ok(_) => {}
            Err(err) => {
                log::debug!(
                    "[pnp] driver {} existing-device probe stopped with {:?}",
                    driver_name,
                    err
                );
            }
        }

        Ok(DriverHandle { id })
    }

    /// 注销驱动并解绑当前由它管理的设备。
    ///
    /// 驱动注销不同于硬件热拔：PnP 设备对象仍保留在全局表中，状态回到
    /// `Discovered`，随后可以被剩余驱动重新 probe。这样动态驱动或后续更具体
    /// 的驱动接入时，不需要重新执行固件/总线枚举。
    pub fn unregister(&self, handle: DriverHandle) -> Result<(), PnpError> {
        let driver = {
            let mut drivers = self.drivers.lock();
            let pos = drivers
                .iter()
                .position(|registered| registered.id == handle.id)
                .ok_or(PnpError::NoDriver)?;
            drivers.swap_remove(pos).driver
        };

        let mut last_error = None;
        for dev in PNP_DEVICES.try_list().ok_or(PnpError::OutOfMemory)? {
            if !dev.bound_to_driver(&driver) {
                continue;
            }
            match dev.unbind_driver_if_matches(&driver) {
                Ok(true) | Ok(false) => {}
                Err(err) => last_error = Some(err),
            }
        }

        if let Some(err) = last_error {
            return Err(err);
        }
        let _ = self.retry_deferred_devices();
        Ok(())
    }

    /// 用指定驱动重新尝试认领已经发现但尚未绑定的设备。
    pub fn probe_existing_devices(&self, driver_id: DriverId) -> Result<usize, PnpError> {
        let driver = self.driver_by_id(driver_id).ok_or(PnpError::NoDriver)?;
        let mut bound = 0usize;
        for dev in PNP_DEVICES.try_list().ok_or(PnpError::OutOfMemory)? {
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
                Err(err) if err.is_deferred() => {}
                Err(err) => return Err(err),
            }
        }
        if bound != 0 {
            let _ = self.retry_deferred_devices();
        }
        Ok(bound)
    }

    /// 为一个新发现的设备寻找匹配驱动并执行 probe。
    pub fn probe_device(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let result = self.probe_device_once(dev);
        if result.is_ok() {
            let _ = self.retry_deferred_devices();
        }
        result
    }

    fn probe_device_once(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let Some(driver) = self.find_matching_driver(dev)? else {
            return Err(PnpError::NoDriver);
        };
        self.bind_driver_to_device(dev, driver)
    }

    /// 重试所有仍处于 Discovered 状态的设备。
    ///
    /// 典型场景是 interrupt-controller、桥设备或其它基础设施刚刚 probe 成功，
    /// 之前返回 deferred error 的普通设备现在可能已经具备依赖。
    /// 本函数用一个轻量 reentry guard 防止 probe 链条中重复递归进入。
    pub fn retry_deferred_devices(&self) -> Result<usize, PnpError> {
        self.retry_deferred_matching(|_| true)
    }

    /// 只重试等待指定依赖的 deferred 设备。
    pub fn retry_deferred_dependency(&self, dependency: PnpDependency) -> Result<usize, PnpError> {
        self.retry_deferred_matching(|dev| dev.deferred_dependency() == Some(dependency))
    }

    fn retry_deferred_matching(
        &self,
        mut accepts: impl FnMut(&Arc<PnpDevice>) -> bool,
    ) -> Result<usize, PnpError> {
        if self
            .retrying_deferred
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(0);
        }

        let mut total_bound = 0usize;
        loop {
            let mut round_bound = 0usize;
            for dev in PNP_DEVICES.try_list().ok_or(PnpError::OutOfMemory)? {
                if dev.state() != PnpState::Discovered {
                    continue;
                }
                if !accepts(&dev) {
                    continue;
                }
                match self.probe_device_once(&dev) {
                    Ok(()) => round_bound += 1,
                    Err(PnpError::NoDriver | PnpError::InvalidState) => {}
                    Err(err) if err.is_deferred() => {}
                    Err(err) => {
                        self.retrying_deferred.store(false, Ordering::Release);
                        return Err(err);
                    }
                }
            }
            if round_bound == 0 {
                self.retrying_deferred.store(false, Ordering::Release);
                return Ok(total_bound);
            }
            total_bound += round_bound;
        }
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
                inner.deferred_dependency = None;
                Ok(())
            }
            Err(err) => {
                let deferred_dependency = err.deferred_dependency();
                dev.rollback_probe_side_effects();
                dev.set_deferred_dependency(deferred_dependency);
                Err(err)
            }
        }
    }

    fn find_matching_driver(
        &self,
        dev: &Arc<PnpDevice>,
    ) -> Result<Option<Arc<dyn PnpDriver>>, PnpError> {
        let drivers = self.drivers.lock();
        let mut best: Option<((u8, PnpDriverPriority), Arc<dyn PnpDriver>)> = None;

        for registered in drivers.iter() {
            if !driver_can_probe_bus(registered.driver.as_ref(), dev.info.bus_type()) {
                continue;
            }
            if !registered.driver.matches(&dev.id, dev.info.as_ref()) {
                continue;
            }
            let bus_rank = if registered.driver.bus_type() == dev.info.bus_type() {
                1
            } else {
                0
            };
            let key = (bus_rank, registered.driver.priority());
            match best.as_ref() {
                None => best = Some((key, Arc::clone(&registered.driver))),
                Some((best_key, _)) if key > *best_key => {
                    best = Some((key, Arc::clone(&registered.driver)));
                }
                Some((best_key, _)) if key == *best_key => return Err(PnpError::DriverAmbiguous),
                _ => {}
            }
        }

        Ok(best.map(|(_, driver)| driver))
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

/// 通知一个 deferred 依赖已经就绪。
///
/// 资源 registry 在成功登记 controller、syscon、fwcfg 等依赖后调用该函数。
/// PnP core 会只重试记录了同一依赖的设备；旧式没有精确依赖的 deferred 设备仍由
/// [`PnpDriverRegistry::retry_deferred_devices`] 兜底处理。
pub fn notify_dependency_ready(dependency: PnpDependency) {
    let _ = PNP_DRIVERS.retry_deferred_dependency(dependency);
}

// ── PnpDeviceList ────────────────────────────────────────────────────────

/// PnP 设备全局列表。
///
/// 该列表保存总线已经发现的设备对象。驱动绑定状态保存在每个 [`PnpDevice`]
/// 内部，因此列表只负责唯一性、查询和热拔移除。
pub struct PnpDeviceList {
    devices: Spinlock<Vec<Arc<PnpDevice>>>,
}

/// PnP 设备登记结果。
///
/// 总线重新扫描或 deferred retry 可能再次提交同一个硬件身份。调用方需要知道本次
/// 是新插入还是复用了既有对象，以便 probe 硬失败时只回滚新插入的设备。
pub struct PnpDeviceRegistration {
    pub device: Arc<PnpDevice>,
    pub inserted: bool,
}

impl PnpDeviceList {
    pub const fn new() -> Self {
        Self {
            devices: Spinlock::new(Vec::new()),
        }
    }

    /// 插入新设备；若同一硬件身份已经存在，则返回既有对象。
    ///
    /// 这是总线扫描唯一的登记入口。它不会把重复发现视为错误，适合固件节点重试、
    /// 热插拔重新扫描或驱动依赖恢复后的 probe retry。
    pub fn get_or_insert(&self, dev: Arc<PnpDevice>) -> Result<PnpDeviceRegistration, PnpError> {
        let mut list = self.devices.lock();
        list.retain(|d| d.state() != PnpState::Gone);
        if let Some(existing) = list
            .iter()
            .find(|existing| existing.id == dev.id && existing.state() != PnpState::Gone)
            .cloned()
        {
            return Ok(PnpDeviceRegistration {
                device: existing,
                inserted: false,
            });
        }
        list.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
        list.push(Arc::clone(&dev));
        Ok(PnpDeviceRegistration {
            device: dev,
            inserted: true,
        })
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
    pub fn try_list(&self) -> Option<Vec<Arc<PnpDevice>>> {
        let list = self.devices.lock();
        let mut out = Vec::new();
        out.try_reserve(list.len()).ok()?;
        out.extend(list.iter().filter(|d| d.state() != PnpState::Gone).cloned());
        Some(out)
    }

    /// 返回所有尚未 Gone 的设备快照。
    pub fn list(&self) -> Vec<Arc<PnpDevice>> {
        self.try_list().unwrap_or_default()
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
///
/// FIXME(dev-core): 这个回调虽然避免了直接导入 VFS 类型，但仍让 PnP core 知道
/// devtmpfs 的发布时机。应收敛为通用 function projection 事件，由 devtmpfs、
/// sysfs、procfs 各自订阅并维护自己的兼容视图。
#[derive(Clone, Copy)]
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
    let cb = DEVTMPFS_CB
        .lock()
        .as_ref()
        .copied()
        .ok_or(PnpError::NoDevtmpfs)?;
    (cb.bind)(&nodes)
}

fn devtmpfs_unbind(nodes: &DevNodeSet) -> Result<(), PnpError> {
    let cb = DEVTMPFS_CB
        .lock()
        .as_ref()
        .copied()
        .ok_or(PnpError::NoDevtmpfs)?;
    (cb.unbind)(nodes)
}

// ── 功能注册 helpers ────────────────────────────────────────────────────

impl PnpDevice {
    /// 事务式注册开放设备 function：DEVICES → devtmpfs → PnpDevice.attach。
    ///
    /// FIXME(dev-core): 事务顺序仍包含 devtmpfs bind，说明 function 生命周期和
    /// POSIX/VFS 节点发布还没有彻底拆开。后续应先完成底层 function 注册，再由
    /// projection 层异步/事务式补建兼容节点并记录失败原因。
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
            if e != PnpError::NoDevtmpfs {
                DEVICES.unregister_function(&func);
                self.detach_function(&func);
                func.mark_gone();
                return Err(e);
            }
            // devtmpfs 是 POSIX/VFS 投影层；桥接未安装时不能反向阻止底层设备注册。
            // 启动路径通常会先安装 bridge，热插拔/特殊初始化路径可在之后补建节点。
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
        let (functions, children, resources) = {
            let mut inner = self.inner.lock();
            inner.bound_driver = None;
            inner.driver_data = None;
            let functions = core::mem::take(&mut inner.functions);
            let children = core::mem::take(&mut inner.children);
            let resources = core::mem::take(&mut inner.resources);
            if inner.state == PnpState::Probing {
                inner.state = PnpState::Discovered;
            }
            (functions, children, resources)
        };

        for child in children.iter().rev() {
            child.remove_device();
        }

        for func in &functions {
            func.mark_gone();
            self.unregister_function_external(func);
        }

        release_pnp_resources(resources, &self.id);
    }

    fn unbind_driver_if_matches(
        self: &Arc<Self>,
        driver: &Arc<dyn PnpDriver>,
    ) -> Result<bool, PnpError> {
        if self
            .removal_lock
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return Err(PnpError::InvalidState);
        }

        let (bound_driver, functions, children, resources) = {
            let mut inner = self.inner.lock();
            let matches_driver = inner
                .bound_driver
                .as_ref()
                .is_some_and(|bound| Arc::ptr_eq(bound, driver));
            if inner.state != PnpState::Bound || !matches_driver {
                self.removal_lock.store(false, Ordering::Release);
                return Ok(false);
            }

            let bound_driver = inner.bound_driver.take().ok_or(PnpError::InvalidState)?;
            inner.state = PnpState::Removing;
            let functions = core::mem::take(&mut inner.functions);
            let children = core::mem::take(&mut inner.children);
            let resources = core::mem::take(&mut inner.resources);
            (bound_driver, functions, children, resources)
        };

        // 子设备通常由当前驱动在 probe 期间枚举出来。驱动解绑时按叶子优先移除
        // 这些派生设备，但保留当前硬件设备对象，供其它驱动重新匹配。
        for child in children.iter().rev() {
            child.remove_device();
        }

        for func in &functions {
            func.mark_gone();
        }
        for func in &functions {
            func.drain_io();
        }

        bound_driver.remove(self);
        let _ = self.inner.lock().driver_data.take();

        release_pnp_resources(resources, &self.id);

        for func in &functions {
            self.unregister_function_external(func);
        }

        {
            let mut inner = self.inner.lock();
            inner.children.clear();
            inner.deferred_dependency = None;
            if inner.state != PnpState::Removing {
                self.removal_lock.store(false, Ordering::Release);
                return Err(PnpError::InvalidState);
            }
            inner.state = PnpState::Discovered;
        }

        self.removal_lock.store(false, Ordering::Release);
        Ok(true)
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

        // 阶段 2/3：取出子设备与 function。进入 Removing 后它们不再属于活跃拓扑，
        // 直接 move 出内部 Vec，避免热拔路径为了 clone 子设备列表再次分配。
        let (children, functions, resources): (
            Vec<Arc<PnpDevice>>,
            Vec<Arc<dyn DeviceFunction>>,
            Vec<Box<dyn PnpResource>>,
        ) = {
            let mut inner = self.inner.lock();
            (
                core::mem::take(&mut inner.children),
                core::mem::take(&mut inner.functions),
                core::mem::take(&mut inner.resources),
            )
        };

        // 阶段 2：递归移除子设备
        for child in children.iter().rev() {
            child.remove_device();
        }

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

        release_pnp_resources(resources, &self.id);

        // 阶段 6：解绑 devtmpfs 和 DEVICES
        for func in &functions {
            self.unregister_function_external(func);
        }

        // 阶段 7/8：标记 Gone 并从全局列表移除
        {
            let mut inner = self.inner.lock();
            inner.children.clear();
            inner.deferred_dependency = None;
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

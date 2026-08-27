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
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::{self, Debug};
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vfs::sync::Spinlock;

use crate::dev::enumerate::DEVICES;
use crate::dev::function::{
    DeviceFunction, FunctionProjectionNameAllocError, FunctionRegistryError,
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
    /// 由标准 DT phandle 标识的 clock/reset/GPIO 等 provider。
    DtbProvider {
        kind: u16,
        phandle: u32,
    },
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
    /// 内存不足
    OutOfMemory,
    /// 设备拥有的资源仍被外部 consumer 使用，当前不能安全解绑或热移除。
    ResourceBusy {
        kind: PnpResourceKind,
        detail: &'static str,
    },
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
            FunctionRegistryError::NotFound => {
                PnpError::registration_failed(PnpResourceKind::Function, "function not registered")
            }
            FunctionRegistryError::OutOfMemory => PnpError::OutOfMemory,
            FunctionRegistryError::InvalidName | FunctionRegistryError::IdExhausted => {
                PnpError::registration_failed(PnpResourceKind::Function, "invalid function class")
            }
        }
    }
}

impl From<FunctionProjectionNameAllocError> for PnpError {
    fn from(e: FunctionProjectionNameAllocError) -> Self {
        match e {
            FunctionProjectionNameAllocError::OutOfMemory => PnpError::OutOfMemory,
        }
    }
}

// ── PnpId：硬件身份 ──────────────────────────────────────────────────────

/// PnP 设备的稳定硬件身份。
///
/// 该身份只描述设备在总线上的位置或固件节点，不包含 `/dev` 节点名。驱动匹配
/// 应结合 [`PnpBusInfo`] 里的总线私有信息完成。
#[derive(Clone, Debug, Eq)]
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
    /// 由可扩展总线或 ELM 发现源提交的不透明设备身份。
    ///
    /// PnP core 只比较 `bus`、契约标识和规范化字节串，不解释身份内容。这样新
    /// 总线可以在不修改 PnP 状态机的情况下加入设备树；具体语义由匹配该总线的
    /// 驱动或发现源负责。
    Dynamic {
        fingerprint: u64,
        bus: BusType,
        contract: Box<str>,
        identity: Box<[u8]>,
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
            PnpId::Dynamic { fingerprint, .. } => {
                fingerprint.hash(state);
            }
        }
    }
}

impl PartialEq for PnpId {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Pci {
                    segment: sa,
                    bus: ba,
                    device: da,
                    function: fa,
                },
                Self::Pci {
                    segment: sb,
                    bus: bb,
                    device: db,
                    function: fb,
                },
            ) => sa == sb && ba == bb && da == db && fa == fb,
            (
                Self::Usb {
                    bus_id: bia,
                    address: aa,
                    interface: ia,
                },
                Self::Usb {
                    bus_id: bib,
                    address: ab,
                    interface: ib,
                },
            ) => bia == bib && aa == ab && ia == ib,
            (
                Self::Platform {
                    name: na,
                    identity: ia,
                },
                Self::Platform {
                    name: nb,
                    identity: ib,
                },
            ) => na == nb && ia == ib,
            (
                Self::Dynamic {
                    fingerprint: fa,
                    bus: ba,
                    contract: ca,
                    identity: ia,
                },
                Self::Dynamic {
                    fingerprint: fb,
                    bus: bb,
                    contract: cb,
                    identity: ib,
                },
            ) => fa == fb && ba == bb && ca == cb && ia == ib,
            _ => false,
        }
    }
}

impl PnpId {
    pub fn bus_type(&self) -> BusType {
        match self {
            PnpId::Pci { .. } => BusType::PCI,
            PnpId::Usb { .. } => BusType::USB,
            PnpId::Platform { .. } => BusType::PLATFORM,
            PnpId::Dynamic { bus, .. } => *bus,
        }
    }
}

#[kernel_symbols::export]
impl fmt::Display for PnpId {
    #[kernel_symbols::export(
        name = "general.dev.pnp.PnpId.fmt",
        contract = "kernel.general.device-query@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
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
            PnpId::Dynamic {
                bus,
                contract,
                identity,
                ..
            } => write!(f, "dynamic:{}:{}:{:02x?}", bus.raw_id(), contract, identity),
        }
    }
}

impl PnpId {
    /// 构造一个动态设备身份。
    pub fn dynamic(bus: BusType, contract: &str, identity: &[u8]) -> Result<Self, PnpError> {
        if bus == BusType::GENERIC || contract.is_empty() || identity.is_empty() {
            return Err(PnpError::InvalidState);
        }
        let mut contract_copy = String::new();
        contract_copy
            .try_reserve(contract.len())
            .map_err(|_| PnpError::OutOfMemory)?;
        contract_copy.push_str(contract);
        let mut identity_copy = Vec::new();
        identity_copy
            .try_reserve(identity.len())
            .map_err(|_| PnpError::OutOfMemory)?;
        identity_copy.extend_from_slice(identity);
        let fingerprint: u64 = {
            let mut h: u64 = 14695981039346656037u64; // FNV offset basis
            let mix = |h: &mut u64, bytes: &[u8]| {
                for &b in bytes {
                    *h ^= b as u64;
                    *h = h.wrapping_mul(1099511628211u64); // FNV prime
                }
            };
            mix(&mut h, &bus.raw_id().to_le_bytes());
            mix(&mut h, contract.as_bytes());
            mix(&mut h, identity);
            h
        };
        Ok(Self::Dynamic {
            fingerprint,
            bus,
            contract: contract_copy.into_boxed_str(),
            identity: identity_copy.into_boxed_slice(),
        })
    }

    /// 返回动态身份的契约标识；固定总线身份返回 `None`。
    pub fn identity_contract(&self) -> Option<&str> {
        match self {
            Self::Dynamic { contract, .. } => Some(contract),
            _ => None,
        }
    }

    /// 返回动态身份的规范化字节串；固定总线身份返回 `None`。
    pub fn identity_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Dynamic { identity, .. } => Some(identity),
            _ => None,
        }
    }
}

// ── 总线类型与 PnpBusInfo ────────────────────────────────────────────────

/// PnP 内部使用的总线类型标识。
///
/// 该类型替代散落的 `"pci"`、`"usb"`、`"platform"` 字符串比较。总线枚举层
/// 和驱动只需要返回同一个 `BusType` 常量，注册表即可做类型安全的匹配。保留
/// [`BusType::new`] 是为了后续新增总线时不必修改 PnP core。
#[derive(Clone, Copy, Debug)]
pub struct BusType {
    id: u64,
    name: Option<&'static str>,
}

impl PartialEq for BusType {
    fn eq(&self, other: &Self) -> bool {
        match (self.name, other.name) {
            (Some(left), Some(right)) => left == right,
            (None, None) => self.id == other.id,
            _ => false,
        }
    }
}

impl Eq for BusType {}

impl Hash for BusType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self.name {
            Some(name) => {
                0u8.hash(state);
                name.hash(state);
            }
            None => {
                1u8.hash(state);
                self.id.hash(state);
            }
        }
    }
}

impl BusType {
    pub const PCI: Self = Self::new("pci");
    pub const USB: Self = Self::new("usb");
    pub const PLATFORM: Self = Self::new("platform");
    pub const GENERIC: Self = Self::new("generic");

    pub const fn new(name: &'static str) -> Self {
        Self {
            id: stable_identifier_hash(name),
            name: Some(name),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self.name {
            Some(name) => name,
            None => "dynamic",
        }
    }

    /// 返回不透明总线编号，动态总线生命周期内不会复用。
    pub const fn raw_id(self) -> u64 {
        self.id
    }

    /// 从内核分配的动态总线编号构造总线类型。
    pub const fn dynamic(raw_id: u64) -> Self {
        Self {
            id: raw_id,
            name: None,
        }
    }
}

impl fmt::Display for BusType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub trait PnpBusInfo: Send + Sync + Any + Debug {
    /// 返回该设备来自哪一种总线。
    fn bus_type(&self) -> BusType;

    /// 返回用于诊断的总线 identifier。
    ///
    /// 动态总线不能把 ELM 镜像中的字符串引用放进长期设备对象，因此由其
    /// `PnpBusInfo` 自己持有内核生命周期内有效的名称。
    fn bus_name(&self) -> &str {
        self.bus_type().as_str()
    }

    /// 供具体总线封装在驱动 probe 时恢复强类型信息。
    fn as_any(&self) -> &dyn Any;
}

/// 动态总线设备的通用属性。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynamicPnpProperty {
    pub name: Box<str>,
    pub value: Box<[u8]>,
}

/// 动态总线设备的通用资源描述。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynamicPnpResource {
    pub kind: u32,
    pub index: u32,
    pub start: u64,
    pub length: u64,
    pub flags: u64,
    pub payload: Box<[u8]>,
}

/// 可扩展设备来源使用的 PnP 总线信息。
///
/// 该对象只保存规范化描述，不提供任何隐含的硬件访问能力。驱动仍需通过
/// 设备资源 API 显式取得 MMIO、IRQ、DMA 或其它资源。
#[derive(Clone, Debug)]
pub struct DynamicPnpBusInfo {
    bus: BusType,
    bus_name: Box<str>,
    contract: Box<str>,
    properties: Vec<DynamicPnpProperty>,
    resources: Vec<DynamicPnpResource>,
}

impl DynamicPnpBusInfo {
    /// 构造动态总线信息，并复制所有传入数据到内核拥有的存储中。
    pub fn new(
        bus: BusType,
        bus_name: &str,
        contract: &str,
        properties: Vec<DynamicPnpProperty>,
        resources: Vec<DynamicPnpResource>,
    ) -> Result<Self, PnpError> {
        if bus == BusType::GENERIC || bus_name.is_empty() || contract.is_empty() {
            return Err(PnpError::InvalidState);
        }
        Ok(Self {
            bus,
            bus_name: copy_boxed_str(bus_name)?,
            contract: copy_boxed_str(contract)?,
            properties,
            resources,
        })
    }

    pub fn bus_name(&self) -> &str {
        &self.bus_name
    }

    pub fn contract(&self) -> &str {
        &self.contract
    }

    pub fn properties(&self) -> &[DynamicPnpProperty] {
        &self.properties
    }

    pub fn resources(&self) -> &[DynamicPnpResource] {
        &self.resources
    }
}

impl PnpBusInfo for DynamicPnpBusInfo {
    fn bus_type(&self) -> BusType {
        self.bus
    }

    fn bus_name(&self) -> &str {
        &self.bus_name
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

const fn stable_identifier_hash(value: &str) -> u64 {
    let bytes = value.as_bytes();
    let mut hash = 0xcbf29ce484222325u64;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        index += 1;
    }
    hash
}

fn copy_boxed_str(value: &str) -> Result<Box<str>, PnpError> {
    let mut out = String::new();
    out.try_reserve(value.len())
        .map_err(|_| PnpError::OutOfMemory)?;
    out.push_str(value);
    Ok(out.into_boxed_str())
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
    /// 固件选择的启动 CPU/hart ID，供中断控制器选择对应的本地上下文。
    pub boot_cpu_id: usize,
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
            boot_cpu_id: 0,
            set_realtime_ns: None,
            install_realtime_source: None,
            unregister_realtime_source: None,
        }
    }

    pub const fn with_boot_cpu_id(mut self, boot_cpu_id: usize) -> Self {
        self.boot_cpu_id = boot_cpu_id;
        self
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
static NEXT_PNP_DEVICE_ID: AtomicU64 = AtomicU64::new(1);

/// 安装全局驱动初始化上下文。
///
/// 必须在调用 [`register_driver_factory`] 或内建驱动 bootstrap 前完成。
#[kernel_symbols::export(
    name = "general.dev.pnp.set_dev_init_context",
    contract = "kernel.general.device-admin@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn set_dev_init_context(ctx: DevInitContext) {
    *DEV_INIT_CONTEXT.lock() = Some(ctx);
}

fn dev_init_context() -> Result<DevInitContext, PnpError> {
    DEV_INIT_CONTEXT.lock().ok_or(PnpError::InvalidState)
}

/// 使用平台安装的设备 MMIO 转换规则取得内核虚拟地址。
#[kernel_symbols::export(
    name = "general.dev.pnp.device_mmio_to_virt",
    contract = "kernel.general.device-resource@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE
)]
pub fn device_mmio_to_virt(physical_address: usize) -> Result<usize, PnpError> {
    let context = dev_init_context()?;
    Ok((context.device_mmio_to_virt)(physical_address))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ElmPnpOwner {
    cell_id: elm_model::ElmId,
    generation: elm_model::Generation,
}

impl ElmPnpOwner {
    const fn from_context(context: elm_model::ElmCurrentContext) -> Self {
        Self {
            cell_id: context.cell_id,
            generation: context.generation,
        }
    }

    fn current() -> Option<Self> {
        elm_model::current_context().map(Self::from_context)
    }
}

fn elm_context_from_snapshot(context: elm_model::ElmCurrentContext) -> elm_model::ElmContext {
    elm_model::ElmContext::new(
        context.cell_id,
        context.parent_id,
        context.generation,
        context.state,
        context.phase,
        context.flags,
    )
    .with_kind(context.kind)
    .with_allowed_actions(context.allowed_actions)
}

/// 从已捕获的完整身份快照重建一次可嵌套 ELM 调用边界。
///
/// IRQ/MSI 等常驻 registry 的 callback proxy 与 PnP 代理共用此入口，确保
/// cell、generation、lifecycle phase 和策略权限快照一致。
pub(crate) fn enter_elm_snapshot(
    context: elm_model::ElmCurrentContext,
) -> Option<elm_model::ElmCurrentContextGuard> {
    elm_model::try_enter_current_context(&elm_context_from_snapshot(context))
}

/// 从硬中断现场进入已捕获的 ELM 身份，且不访问当前任务的扩展状态。
pub(crate) fn enter_elm_interrupt_snapshot(
    context: elm_model::ElmCurrentContext,
) -> Option<elm_model::ElmCurrentContextGuard> {
    elm_model::try_enter_interrupt_context(&elm_context_from_snapshot(context))
}

/// 常驻 PnP core 持有的动态 ELM function 代理。
///
/// `DeviceFunction` 的 trait vtable 与析构入口可能位于可卸载镜像中。代理在登记时
/// 复制所有身份元数据，只在确实需要进入实现的操作上恢复完整 ELM 上下文；无法进入
/// 原 owner/generation 时停止调用动态 vtable，并把该 generation 标记为失败。
struct ElmDeviceFunctionProxy {
    context: elm_model::ElmCurrentContext,
    class_id: crate::dev::function::DeviceClassId,
    class_name: String,
    dev_name: String,
    operation_contract: Option<String>,
    exposes_resident_any: bool,
    gone: AtomicBool,
    function: Option<Arc<dyn DeviceFunction>>,
}

impl ElmDeviceFunctionProxy {
    fn wrap(
        function: Arc<dyn DeviceFunction>,
        context: elm_model::ElmCurrentContext,
    ) -> Result<Arc<dyn DeviceFunction>, (PnpError, Arc<dyn DeviceFunction>)> {
        if let Some(proxy) = function.as_any().downcast_ref::<Self>() {
            return if proxy.context == context {
                Ok(function)
            } else {
                Err((PnpError::InvalidState, function))
            };
        }

        let class_id = function.class_id();
        let exposes_resident_any = {
            let any = function.as_any();
            any.is::<crate::dev::function::CharFunction>()
                || any.is::<crate::dev::function::BlockFunction>()
                || any.is::<crate::dev::rtc::RtcFunction>()
                || any.is::<crate::dev::net::NetFunction>()
        };
        let class_name_source = function.class_name();
        let dev_name_source = function.dev_name();
        let operation_contract_source = function.operation_contract();
        // 代理 Arc 与缓存字符串属于常驻 PnP 元数据，不能因为注册发生在 ELM
        // 回调中就计入该 generation 的隐式分配账户。
        let Some(_accounting) = allocator::suspend_implicit_allocation_accounting() else {
            return Err((PnpError::OutOfMemory, function));
        };
        let copy_string = |value: &str| -> Result<String, PnpError> {
            let mut out = String::new();
            out.try_reserve_exact(value.len())
                .map_err(|_| PnpError::OutOfMemory)?;
            out.push_str(value);
            Ok(out)
        };
        let class_name = match copy_string(class_name_source) {
            Ok(name) => name,
            Err(error) => return Err((error, function)),
        };
        let dev_name = match copy_string(dev_name_source) {
            Ok(name) => name,
            Err(error) => return Err((error, function)),
        };
        let operation_contract = match operation_contract_source {
            Some(contract) => match copy_string(contract) {
                Ok(contract) => Some(contract),
                Err(error) => return Err((error, function)),
            },
            None => None,
        };
        Ok(Arc::new(Self {
            context,
            class_id,
            class_name,
            dev_name,
            operation_contract,
            exposes_resident_any,
            gone: AtomicBool::new(false),
            function: Some(function),
        }))
    }

    fn function(&self) -> &dyn DeviceFunction {
        self.function
            .as_deref()
            .expect("ELM device function proxy used after drop")
    }

    fn enter(&self, operation: &'static str) -> Option<elm_model::ElmCurrentContextGuard> {
        let guard = enter_elm_snapshot(self.context);
        if guard.is_none() {
            log::error!(
                "[pnp] cannot enter ELM context for function {} operation {}: cell={} generation={}",
                self.dev_name,
                operation,
                self.context.cell_id.0,
                self.context.generation.0
            );
            super::elm_lifecycle::mark_context_failed(self.context);
        }
        guard
    }
}

impl DeviceFunction for ElmDeviceFunctionProxy {
    fn class_id(&self) -> crate::dev::function::DeviceClassId {
        self.class_id
    }

    fn dev_name(&self) -> &str {
        &self.dev_name
    }

    fn class_name(&self) -> &str {
        &self.class_name
    }

    fn operation_contract(&self) -> Option<&str> {
        self.operation_contract.as_deref()
    }

    fn invoke(
        &self,
        opcode: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, crate::dev::function::DeviceFunctionInvokeError> {
        if self.gone.load(Ordering::Acquire) {
            return Err(crate::dev::function::DeviceFunctionInvokeError::Gone);
        }
        let Some(_guard) = self.enter("invoke") else {
            return Err(crate::dev::function::DeviceFunctionInvokeError::Gone);
        };
        if self.gone.load(Ordering::Acquire) {
            return Err(crate::dev::function::DeviceFunctionInvokeError::Gone);
        }
        self.function().invoke(opcode, input, output)
    }

    fn mark_gone(&self) {
        if self.gone.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(_guard) = self.enter("mark_gone") else {
            return;
        };
        self.function().mark_gone();
    }

    fn drain_io(&self) {
        let Some(_guard) = self.enter("drain_io") else {
            return;
        };
        self.function().drain_io();
    }

    fn as_any(&self) -> &dyn Any {
        if !self.exposes_resident_any {
            // 动态具体类型不能把 `Any` vtable 引用泄露到 guard 之外；扩展 function
            // 应通过 operation contract/invoke 提供稳定调用面。
            return self;
        }
        let Some(_guard) = self.enter("as_any") else {
            return self;
        };
        self.function().as_any()
    }
}

impl Drop for ElmDeviceFunctionProxy {
    fn drop(&mut self) {
        let Some(function) = self.function.take() else {
            return;
        };
        let Some(_guard) = self.enter("drop") else {
            // 无法恢复精确 generation 时绝不能执行动态 drop glue。保留最后一个
            // Arc 并让 owner 卸载事务失败，优先避免跳入已经失效的镜像。
            core::mem::forget(function);
            return;
        };
        drop(function);
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
    /// 返回可由资源拥有者用于主动撤销的稳定键；普通 LIFO 资源返回空值。
    fn identity(&self) -> Option<u64> {
        None
    }
    /// 冻结资源并确认后续 [`Self::release`] 可以无失败提交。
    ///
    /// 成功后实现必须拒绝新的 lease、调用或其它工作，直到 [`Self::cancel_release`]
    /// 或 [`Self::release`]。prepare 不能注销 handle、销毁对象或改变设备可见状态。
    fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
        Ok(())
    }
    /// 撤销一次成功的 [`Self::prepare_release`]。
    fn cancel_release(&self) {}
    /// 同一设备内的资源 prepare 顺序。
    fn release_order(&self) -> PnpResourceReleaseOrder {
        PnpResourceReleaseOrder::Regular
    }
    /// 本资源在移除事务中提供的依赖键。
    ///
    /// PnP core 用它把显式 consumer 排在 provider 之前提交。普通驱动资源没有
    /// 跨设备依赖，保持 `None` 即可。
    fn provided_dependency(&self) -> Option<PnpDependency> {
        None
    }
    /// 返回本资源是否消费给定 provider 依赖。
    ///
    /// 该查询只在所有相关设备都已经冻结后执行，不能改变资源状态。
    fn consumes_dependency(&self, _dependency: PnpDependency) -> bool {
        false
    }
    /// 释放资源。实现必须允许底层资源已被提前释放并安全返回错误。
    fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError>;
}

/// prepare 阶段的资源依赖顺序。
///
/// consumer 先冻结并声明自己会随事务释放，provider 随后才能区分子树内部引用与
/// 真正的外部 lease。提交仍保持每台设备内部的 LIFO 顺序。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PnpResourceReleaseOrder {
    Consumer,
    Regular,
    Provider,
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
/// prepare 或 release 阶段的资源错误。
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

/// 常驻 PnP core 持有的动态 ELM 资源代理。
///
/// 资源实现的 trait vtable 可以位于动态镜像中，因此 prepare、cancel、release
/// 以及最后的 drop 都必须恢复登记时的完整 ELM 上下文。代理自身的 vtable
/// 留在常驻内核，避免 core 在无主上下文中直接跳入模块代码。
struct ElmPnpResourceProxy {
    context: elm_model::ElmCurrentContext,
    kind: PnpResourceKind,
    label: &'static str,
    identity: Option<u64>,
    order: PnpResourceReleaseOrder,
    provided_dependency: Option<PnpDependency>,
    resource: Option<Box<dyn PnpResource>>,
}

impl ElmPnpResourceProxy {
    fn wrap(
        resource: Box<dyn PnpResource>,
        context: elm_model::ElmCurrentContext,
    ) -> Result<Box<dyn PnpResource>, (PnpError, Box<dyn PnpResource>)> {
        let kind = resource.kind();
        let label = resource.label();
        let identity = resource.identity();
        let order = resource.release_order();
        let provided_dependency = resource.provided_dependency();
        // 代理是常驻内核元数据，不能因为在 ELM 回调中构造就计入该单元的
        // 隐式分配账户；真正的模块资源仍由 inner Box 保持。
        let Some(_accounting) = allocator::suspend_implicit_allocation_accounting() else {
            return Err((PnpError::OutOfMemory, resource));
        };
        Ok(Box::new(Self {
            context,
            kind,
            label,
            identity,
            order,
            provided_dependency,
            resource: Some(resource),
        }))
    }

    fn resource(&self) -> &dyn PnpResource {
        self.resource
            .as_deref()
            .expect("ELM PnP resource proxy used after release")
    }

    fn context_failure(&self, operation: &'static str) -> PnpResourceReleaseError {
        log::error!(
            "[pnp] cannot enter ELM context for resource {} operation {}: cell={} generation={}",
            self.label,
            operation,
            self.context.cell_id.0,
            self.context.generation.0
        );
        super::elm_lifecycle::mark_context_failed(self.context);
        PnpResourceReleaseError::new(self.kind, self.label, "cannot enter owning ELM context")
    }
}

impl PnpResource for ElmPnpResourceProxy {
    fn kind(&self) -> PnpResourceKind {
        self.kind
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn identity(&self) -> Option<u64> {
        self.identity
    }

    fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
        let Some(_guard) = enter_elm_snapshot(self.context) else {
            return Err(self.context_failure("prepare_release"));
        };
        self.resource().prepare_release()
    }

    fn cancel_release(&self) {
        let Some(_guard) = enter_elm_snapshot(self.context) else {
            let _ = self.context_failure("cancel_release");
            return;
        };
        self.resource().cancel_release();
    }

    fn release_order(&self) -> PnpResourceReleaseOrder {
        self.order
    }

    fn provided_dependency(&self) -> Option<PnpDependency> {
        self.provided_dependency
    }

    fn consumes_dependency(&self, dependency: PnpDependency) -> bool {
        let Some(_guard) = enter_elm_snapshot(self.context) else {
            let _ = self.context_failure("consumes_dependency");
            // 不能确认依赖时保守地建立 consumer 边，避免 provider 被提前拆除。
            return true;
        };
        self.resource().consumes_dependency(dependency)
    }

    fn release(mut self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        let resource = self
            .resource
            .take()
            .expect("ELM PnP resource proxy released twice");
        let Some(_guard) = enter_elm_snapshot(self.context) else {
            let error = self.context_failure("release");
            // 无法进入主上下文时不能调用动态 vtable 的 drop glue。保留 inner
            // 并标记 owner 失败，让卸载流程 fail closed。
            core::mem::forget(resource);
            return Err(error);
        };
        resource.release()
    }
}

impl Drop for ElmPnpResourceProxy {
    fn drop(&mut self) {
        let Some(resource) = self.resource.take() else {
            return;
        };
        let Some(_guard) = enter_elm_snapshot(self.context) else {
            let _ = self.context_failure("drop");
            core::mem::forget(resource);
            return;
        };
        drop(resource);
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
    prepare: Option<fn(H) -> bool>,
    cancel: Option<fn(H)>,
    prepared: AtomicBool,
    order: PnpResourceReleaseOrder,
    provided_dependency: Option<PnpDependency>,
    consumed_dependency: Option<PnpDependency>,
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
            prepare: None,
            cancel: None,
            prepared: AtomicBool::new(false),
            order: PnpResourceReleaseOrder::Regular,
            provided_dependency: None,
            consumed_dependency: None,
            release,
        }
    }

    pub(crate) const fn new_checked(
        kind: PnpResourceKind,
        label: &'static str,
        handle: H,
        prepare: fn(H) -> bool,
        cancel: fn(H),
        order: PnpResourceReleaseOrder,
        release: fn(H) -> bool,
    ) -> Self {
        Self {
            kind,
            label,
            handle,
            prepare: Some(prepare),
            cancel: Some(cancel),
            prepared: AtomicBool::new(false),
            order,
            provided_dependency: None,
            consumed_dependency: None,
            release,
        }
    }

    /// 声明该 handle 在跨设备移除事务中提供一个依赖。
    pub const fn with_provided_dependency(mut self, dependency: PnpDependency) -> Self {
        self.provided_dependency = Some(dependency);
        self
    }

    /// 声明该 handle 在跨设备移除事务中消费一个依赖。
    pub const fn with_consumed_dependency(mut self, dependency: PnpDependency) -> Self {
        self.consumed_dependency = Some(dependency);
        self
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

    fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
        if self.prepared.load(Ordering::Acquire) {
            Ok(())
        } else if self.prepare.is_none_or(|prepare| prepare(self.handle)) {
            self.prepared.store(true, Ordering::Release);
            Ok(())
        } else {
            Err(PnpResourceReleaseError::new(
                self.kind,
                self.label,
                "resource is still leased or has work in flight",
            ))
        }
    }

    fn cancel_release(&self) {
        if self.prepared.swap(false, Ordering::AcqRel)
            && let Some(cancel) = self.cancel
        {
            cancel(self.handle);
        }
    }

    fn release_order(&self) -> PnpResourceReleaseOrder {
        self.order
    }

    fn provided_dependency(&self) -> Option<PnpDependency> {
        self.provided_dependency
    }

    fn consumes_dependency(&self, dependency: PnpDependency) -> bool {
        self.consumed_dependency == Some(dependency)
    }

    fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        if self.prepare.is_some() && !self.prepared.load(Ordering::Acquire) {
            self.prepare_release()?;
        }
        if (self.release)(self.handle) {
            Ok(())
        } else {
            self.cancel_release();
            Err(PnpResourceReleaseError::new(
                self.kind,
                self.label,
                "resource release callback reported failure",
            ))
        }
    }
}

fn release_pnp_resources(
    mut resources: Vec<Box<dyn PnpResource>>,
    owner: &PnpId,
) -> Result<(), PnpError> {
    for order in [
        PnpResourceReleaseOrder::Consumer,
        PnpResourceReleaseOrder::Regular,
        PnpResourceReleaseOrder::Provider,
    ] {
        while let Some(index) = resources
            .iter()
            .rposition(|resource| resource.release_order() == order)
        {
            let resource = resources.remove(index);
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
                return Err(PnpError::HardwareFailure {
                    detail: "prepared PnP resource release failed",
                });
            }
        }
    }
    Ok(())
}

// ── PnpDevice ────────────────────────────────────────────────────────────

struct PnpDeviceInner {
    state: PnpState,
    parent: Option<Weak<PnpDevice>>,
    children: Vec<Arc<PnpDevice>>,
    functions: Vec<Arc<dyn DeviceFunction>>,
    /// 总线/固件枚举阶段安装、跨驱动 unbind 保留到设备 Gone 的资源。
    bus_resources: Vec<Box<dyn PnpResource>>,
    resources: Vec<Box<dyn PnpResource>>,
    bound_driver: Option<Arc<dyn PnpDriver>>,
    driver_owner: Option<ElmPnpOwner>,
    /// 当前 probe/bind 事务的设备内代次；每次重新 probe 都递增且不复用。
    driver_binding_generation: u64,
    driver_data: Option<Arc<dyn Any + Send + Sync>>,
    deferred_dependency: Option<PnpDependency>,
}

/// PnP 设备对象。
///
/// 总线层创建该对象并放入 [`PNP_DEVICES`]；驱动 probe 成功后可以通过
/// [`PnpDevice::register_function`] 暴露一个或多个开放设备 function。
pub struct PnpDevice {
    runtime_id: u64,
    pub id: PnpId,
    pub name: Box<str>,
    pub info: Box<dyn PnpBusInfo>,
    inner: Spinlock<PnpDeviceInner>,
    removal_lock: AtomicBool,
}

#[derive(Clone, Copy)]
struct PnpDriverResourceAccess {
    binding_generation: u64,
    owner: Option<ElmPnpOwner>,
    context: Option<elm_model::ElmCurrentContext>,
}

/// provider 驱动在 probe 期间取得的运行期资源授权。
///
/// scope 的字段对驱动不可见，且同时绑定精确的 [`PnpDevice`] 对象、当前驱动
/// bind 代次和 ELM owner generation。IRQ domain、MSI controller 等 provider
/// 可以把它保存在自身状态中，稍后即使由另一个 consumer ELM 的回调触发，也只会
/// 向原 provider 设备登记资源。设备解绑、重新 probe 或进入移除事务后旧 scope
/// 自动失效。
pub struct PnpProviderResourceScope {
    device: Weak<PnpDevice>,
    runtime_id: u64,
    binding_generation: u64,
    owner: Option<ElmPnpOwner>,
    /// 动态 provider 的完整上下文快照；内建 provider 始终为 `None`。
    context: Option<elm_model::ElmCurrentContext>,
}

// 两个字段只用于把底层 RAII guard 保持到外层 guard 析构，不需要主动读取。
#[allow(dead_code)]
enum PnpProviderExecutionContext {
    Dynamic(elm_model::ElmCurrentContextGuard),
    Builtin(elm_model::ElmCurrentContextSuspensionGuard),
    AlreadyActive,
}

/// provider 运行期回调的短生命周期执行边界。
///
/// 动态 provider 在其原始 ELM generation 下执行；内建 provider 临时隐藏外层
/// consumer ELM。guard 必须留在栈上直到 MSI/IRQ 分配、handler 构造和 PnP 资源
/// 登记全部完成，drop 后恢复外层调用者。
#[must_use = "必须持有 guard 直到 provider 运行期资源事务结束"]
pub struct PnpProviderContextGuard {
    context: ManuallyDrop<PnpProviderExecutionContext>,
    _not_send: PhantomData<*mut ()>,
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
            .field(
                "resources_count",
                &(inner.resources.len() + inner.bus_resources.len()),
            )
            .field("deferred_dependency", &inner.deferred_dependency)
            .field(
                "driver_binding_generation",
                &inner.driver_binding_generation,
            )
            .field("driver", &inner.bound_driver.as_ref().map(|d| d.name()))
            .finish()
    }
}

#[kernel_symbols::export]
impl PnpDevice {
    /// 构造一个尚未进入全局设备表的 PnP 对象。
    ///
    /// 每个对象会取得本次启动期间永不复用的运行时编号；编号空间耗尽时返回
    /// [`PnpError::OutOfMemory`]，不会因外部设备枚举触发内核 panic。
    pub fn new(
        id: PnpId,
        name: Box<str>,
        info: Box<dyn PnpBusInfo>,
    ) -> Result<Arc<Self>, PnpError> {
        let runtime_id =
            registry_id::alloc_atomic_id(&NEXT_PNP_DEVICE_ID).map_err(|_| PnpError::OutOfMemory)?;
        Ok(Arc::new(Self {
            runtime_id,
            id,
            name,
            info,
            inner: Spinlock::new(PnpDeviceInner {
                state: PnpState::Discovered,
                parent: None,
                children: Vec::new(),
                functions: Vec::new(),
                bus_resources: Vec::new(),
                resources: Vec::new(),
                bound_driver: None,
                driver_owner: None,
                driver_binding_generation: 0,
                driver_data: None,
                deferred_dependency: None,
            }),
            removal_lock: AtomicBool::new(false),
        }))
    }

    /// 返回本次启动期间唯一且不复用的设备对象编号。
    pub const fn runtime_id(&self) -> u64 {
        self.runtime_id
    }

    pub(crate) fn removal_is_prepared(&self) -> bool {
        self.removal_lock.load(Ordering::Acquire)
    }

    pub fn state(&self) -> PnpState {
        self.inner.lock().state
    }

    /// 返回当前绑定驱动的名称。
    pub fn bound_driver_name(&self) -> Option<String> {
        let inner = self.inner.lock();
        let name = inner.bound_driver.as_ref().map(|d| d.name())?;
        let mut out = String::new();
        out.try_reserve(name.len()).ok()?;
        out.push_str(name);
        Some(out)
    }

    /// 返回设备当前是否由驱动管理，不为诊断名称分配临时字符串。
    pub fn is_bound(&self) -> bool {
        self.inner.lock().bound_driver.is_some()
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

    /// 返回当前 function 数量，不构造快照。
    pub fn function_count(&self) -> usize {
        self.inner.lock().functions.len()
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
        out.try_reserve(inner.resources.len() + inner.bus_resources.len())
            .ok()?;
        out.extend(
            inner
                .bus_resources
                .iter()
                .map(|resource| PnpOwnedResourceSnapshot {
                    kind: resource.kind(),
                    label: resource.label(),
                }),
        );
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
    #[kernel_symbols::export(
        name = "general.dev.pnp.PnpDevice.parent",
        contract = "kernel.general.pnp-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn parent(&self) -> Option<Arc<PnpDevice>> {
        self.inner.lock().parent.as_ref()?.upgrade()
    }

    /// 保存驱动私有数据。
    #[kernel_symbols::export(
        name = "general.dev.pnp.PnpDevice.set_driver_data",
        contract = "kernel.general.pnp-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE,
        retained_args = 2u64
    )]
    pub fn set_driver_data(&self, data: Arc<dyn Any + Send + Sync>) {
        if self.removal_lock.load(Ordering::Acquire) {
            return;
        }
        let caller = ElmPnpOwner::current();
        let mut inner = self.inner.lock();
        let owner_matches = match inner.driver_owner {
            Some(owner) => caller == Some(owner),
            None => inner.state == PnpState::Probing || caller.is_none(),
        };
        if !self.removal_lock.load(Ordering::Acquire)
            && matches!(inner.state, PnpState::Probing | PnpState::Bound)
            && owner_matches
        {
            inner.driver_data = Some(data);
        } else {
            log::error!(
                "[pnp] rejected driver data with mismatched ELM owner for {}",
                self.id
            );
        }
    }

    /// 取出并清空驱动私有数据。
    #[kernel_symbols::export(
        name = "general.dev.pnp.PnpDevice.take_driver_data",
        contract = "kernel.general.pnp-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn take_driver_data(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        let caller = ElmPnpOwner::current();
        let mut inner = self.inner.lock();
        if inner
            .driver_owner
            .is_some_and(|owner| caller != Some(owner))
        {
            log::error!(
                "[pnp] rejected driver data access from stale ELM owner for {}",
                self.id
            );
            return None;
        }
        inner.driver_data.take()
    }

    // ── 父子关系 ──

    pub fn attach_child(self: &Arc<Self>, child: &Arc<PnpDevice>) -> Result<(), PnpError> {
        // PnP 拓扑必须保持有向无环：设备不能把自己挂成子节点，也不能把祖先
        // 重新挂到自己下面。否则 remove/unbind 的递归清理会形成无限递归。
        if self.removal_lock.load(Ordering::Acquire)
            || child.removal_lock.load(Ordering::Acquire)
            || Arc::ptr_eq(self, child)
            || self.has_ancestor(child)
        {
            return Err(PnpError::InvalidState);
        }

        let mut inner = self.inner.lock();
        if self.removal_lock.load(Ordering::Acquire)
            || inner.state == PnpState::Gone
            || inner.state == PnpState::Removing
        {
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
        if child.removal_lock.load(Ordering::Acquire)
            || child_inner.state == PnpState::Gone
            || child_inner.state == PnpState::Removing
        {
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
        let func = self.prepare_device_function(func)?;
        self.attach_prepared_function(func)
    }

    fn attach_prepared_function(&self, func: Arc<dyn DeviceFunction>) -> Result<(), PnpError> {
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
        self.own_boxed_resource(Box::new(resource))
    }

    /// 在 provider 驱动 probe 期间捕获一个运行期资源 scope。
    ///
    /// 该 scope 不是“以当前调用者身份写任意设备”的通行证；它永久绑定当前设备
    /// 对象与本次 bind 代次。动态驱动还会记录完整 ELM 上下文，供稍后的 provider
    /// callback 恢复资源 vtable 的真实 owner。
    #[kernel_symbols::export(
        name = "general.dev.pnp.PnpDevice.provider_resource_scope",
        contract = "kernel.general.device-resource@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn provider_resource_scope(self: &Arc<Self>) -> Result<PnpProviderResourceScope, PnpError> {
        if self.removal_lock.load(Ordering::Acquire) {
            return Err(PnpError::InvalidState);
        }
        let caller = elm_model::current_context();
        let inner = self.inner.lock();
        if self.removal_lock.load(Ordering::Acquire) || inner.state != PnpState::Probing {
            return Err(PnpError::InvalidState);
        }
        let context = match inner.driver_owner {
            Some(owner) => {
                let context = caller.ok_or(PnpError::InvalidState)?;
                if owner != ElmPnpOwner::from_context(context) {
                    return Err(PnpError::InvalidState);
                }
                Some(context)
            }
            // 内建 provider 的 probe 可能嵌套在 consumer ELM 的依赖重试中；
            // 外层上下文不是资源 owner，不能被带入 scope。
            None => None,
        };
        Ok(PnpProviderResourceScope {
            device: Arc::downgrade(self),
            runtime_id: self.runtime_id,
            binding_generation: inner.driver_binding_generation,
            owner: inner.driver_owner,
            context,
        })
    }

    fn driver_resource_context(
        state: PnpState,
        owner: Option<ElmPnpOwner>,
        caller: Option<elm_model::ElmCurrentContext>,
    ) -> Result<Option<elm_model::ElmCurrentContext>, PnpError> {
        if !matches!(state, PnpState::Probing | PnpState::Bound) {
            return Err(PnpError::InvalidState);
        }
        match (owner, caller) {
            (Some(expected), Some(context)) if expected == ElmPnpOwner::from_context(context) => {
                Ok(Some(context))
            }
            (Some(_), _) => Err(PnpError::InvalidState),
            // 内建驱动的 probe 可能由一个嵌套的 ELM 依赖就绪回调触发。
            // 此时 current_context 属于外层调用者，不能把内建资源误标成该 ELM。
            (None, _) if state == PnpState::Probing => Ok(None),
            // 已绑定的内建设备只接受常驻内核路径增加资源，阻止动态
            // 单元把自定义 vtable 悬挂到不受其驱动卸载事务管理的设备上。
            (None, None) => Ok(None),
            (None, Some(_)) => Err(PnpError::InvalidState),
        }
    }

    fn current_driver_resource_access(&self) -> Result<PnpDriverResourceAccess, PnpError> {
        if self.removal_lock.load(Ordering::Acquire) {
            return Err(PnpError::InvalidState);
        }
        let caller = elm_model::current_context();
        let inner = self.inner.lock();
        if self.removal_lock.load(Ordering::Acquire) {
            return Err(PnpError::InvalidState);
        }
        let context = Self::driver_resource_context(inner.state, inner.driver_owner, caller)?;
        Ok(PnpDriverResourceAccess {
            binding_generation: inner.driver_binding_generation,
            owner: inner.driver_owner,
            context,
        })
    }

    fn provider_resource_access(
        &self,
        scope: &PnpProviderResourceScope,
    ) -> Result<PnpDriverResourceAccess, PnpError> {
        if self.removal_lock.load(Ordering::Acquire) || self.runtime_id != scope.runtime_id {
            return Err(PnpError::InvalidState);
        }
        let access = PnpDriverResourceAccess {
            binding_generation: scope.binding_generation,
            owner: scope.owner,
            context: scope.context,
        };
        let inner = self.inner.lock();
        if self.removal_lock.load(Ordering::Acquire)
            || !Self::driver_resource_access_matches(&inner, access)
        {
            return Err(PnpError::InvalidState);
        }
        Ok(access)
    }

    fn driver_resource_access_matches(
        inner: &PnpDeviceInner,
        access: PnpDriverResourceAccess,
    ) -> bool {
        if !matches!(inner.state, PnpState::Probing | PnpState::Bound)
            || inner.driver_binding_generation != access.binding_generation
            || inner.driver_owner != access.owner
        {
            return false;
        }
        match (access.owner, access.context) {
            (Some(owner), Some(context)) => owner == ElmPnpOwner::from_context(context),
            (None, None) => true,
            _ => false,
        }
    }

    fn function_registration_context(
        &self,
    ) -> Result<Option<elm_model::ElmCurrentContext>, PnpError> {
        if self.removal_lock.load(Ordering::Acquire) {
            return Err(PnpError::InvalidState);
        }
        let caller = elm_model::current_context();
        let inner = self.inner.lock();
        if self.removal_lock.load(Ordering::Acquire) {
            return Err(PnpError::InvalidState);
        }
        Self::driver_resource_context(inner.state, inner.driver_owner, caller)
    }

    fn prepare_device_function(
        &self,
        function: Arc<dyn DeviceFunction>,
    ) -> Result<Arc<dyn DeviceFunction>, PnpError> {
        match self.function_registration_context()? {
            Some(context) => {
                ElmDeviceFunctionProxy::wrap(function, context).map_err(|(error, _function)| error)
            }
            None => Ok(function),
        }
    }

    fn insert_driver_resource(
        &self,
        resource: Box<dyn PnpResource>,
    ) -> Result<(), (PnpError, Box<dyn PnpResource>)> {
        let access = match self.current_driver_resource_access() {
            Ok(access) => access,
            Err(error) => return Err((error, resource)),
        };
        self.insert_driver_resource_with_access(resource, access)
    }

    fn insert_driver_resource_with_access(
        &self,
        resource: Box<dyn PnpResource>,
        access: PnpDriverResourceAccess,
    ) -> Result<(), (PnpError, Box<dyn PnpResource>)> {
        let resource = match access.context {
            Some(context) => match ElmPnpResourceProxy::wrap(resource, context) {
                Ok(resource) => resource,
                Err(error) => return Err(error),
            },
            None => resource,
        };

        let mut inner = self.inner.lock();
        if self.removal_lock.load(Ordering::Acquire)
            || !Self::driver_resource_access_matches(&inner, access)
        {
            return Err((PnpError::InvalidState, resource));
        }
        let Some(_accounting) = allocator::suspend_implicit_allocation_accounting() else {
            return Err((PnpError::OutOfMemory, resource));
        };
        if inner.resources.try_reserve(1).is_err() {
            return Err((PnpError::OutOfMemory, resource));
        }
        inner.resources.push(resource);
        Ok(())
    }

    /// 将已经完成类型擦除的资源交给当前 PnP 设备拥有。
    ///
    /// 该入口允许常驻子系统在内核侧构造 trait object，再交给动态 ELM 使用，
    /// 避免把常驻类型的私有 vtable 当成模块链接 ABI。
    #[kernel_symbols::export(
        name = "general.dev.pnp.PnpDevice.own_boxed_resource",
        contract = "kernel.general.device-resource@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE,
        retained_args = 2u64
    )]
    pub fn own_boxed_resource(&self, resource: Box<dyn PnpResource>) -> Result<(), PnpError> {
        self.insert_driver_resource(resource)
            .map_err(|(error, _resource)| error)
    }

    /// 安装一个由总线枚举层拥有、跨驱动 unbind 保留到设备 Gone 的资源。
    ///
    /// 该入口只允许设备仍处于 `Discovered` 时调用。资源不会参与单独驱动注销，
    /// 但会参与设备/overlay 移除事务的 prepare、cancel 与提交。
    pub(crate) fn own_bus_resource(&self, resource: Box<dyn PnpResource>) -> Result<(), PnpError> {
        if self.removal_lock.load(Ordering::Acquire) {
            return Err(PnpError::InvalidState);
        }
        let mut inner = self.inner.lock();
        if self.removal_lock.load(Ordering::Acquire) || inner.state != PnpState::Discovered {
            return Err(PnpError::InvalidState);
        }
        inner
            .bus_resources
            .try_reserve(1)
            .map_err(|_| PnpError::OutOfMemory)?;
        inner.bus_resources.push(resource);
        Ok(())
    }

    /// 将常驻子系统构造的资源交给设备；登记失败时立即执行资源释放。
    ///
    /// 动态 ELM 不应拿回常驻资源的具体类型或 trait vtable。PCI 等常驻子系统
    /// 可通过此入口把构造、登记和失败回滚收口在内核侧。
    pub(crate) fn own_boxed_resource_or_release(
        &self,
        resource: Box<dyn PnpResource>,
    ) -> Result<(), PnpError> {
        let Err((error, resource)) = self.insert_driver_resource(resource) else {
            return Ok(());
        };
        let kind = resource.kind();
        let label = resource.label();
        if let Err(release_error) = resource.release() {
            log::error!(
                "[pnp] 回滚 {:?} 资源 {} 失败: {}",
                kind,
                label,
                release_error.detail
            );
        }
        Err(error)
    }

    /// 为即将成组登记的资源预留槽位。
    ///
    /// 驱动应在创建会同步发布“依赖已就绪”的外部 handle 前调用。在没有
    /// 其它并发登记消耗这些槽位的前提下，预留成功后同一 probe 中接下来的
    /// `own_resource` 不会再因 Vec 扩容失败，避免 provider 已可见却无法交给
    /// PnP 事务的半登记状态。动态 ELM 只能为本 cell/generation 正在 probe
    /// 或已绑定的设备预留。
    #[kernel_symbols::export(
        name = "general.dev.pnp.PnpDevice.reserve_owned_resources",
        contract = "kernel.general.device-resource@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn reserve_owned_resources(&self, additional: usize) -> Result<(), PnpError> {
        let access = self.current_driver_resource_access()?;
        self.reserve_owned_resources_with_access(additional, access)
    }

    fn reserve_owned_resources_with_access(
        &self,
        additional: usize,
        access: PnpDriverResourceAccess,
    ) -> Result<(), PnpError> {
        let mut inner = self.inner.lock();
        if self.removal_lock.load(Ordering::Acquire)
            || !Self::driver_resource_access_matches(&inner, access)
        {
            return Err(PnpError::InvalidState);
        }
        let _accounting =
            allocator::suspend_implicit_allocation_accounting().ok_or(PnpError::OutOfMemory)?;
        inner
            .resources
            .try_reserve(additional)
            .map_err(|_| PnpError::OutOfMemory)
    }

    /// 按资源稳定键主动撤销一条由设备拥有的资源。
    ///
    /// 该入口只用于资源本身允许主动撤销的扩展对象；没有 identity 的传统资源仍由
    /// PnP 设备解绑或热拔时统一按登记逆序释放。
    pub fn release_owned_resource(&self, identity: u64) -> Result<(), PnpError> {
        if identity == 0 || self.removal_lock.load(Ordering::Acquire) {
            return Err(PnpError::InvalidState);
        }
        let resource = {
            let mut inner = self.inner.lock();
            if self.removal_lock.load(Ordering::Acquire) {
                return Err(PnpError::InvalidState);
            }
            let Some(index) = inner
                .resources
                .iter()
                .position(|resource| resource.identity() == Some(identity))
            else {
                return Err(PnpError::MissingResource {
                    kind: PnpResourceKind::Other("pnp-resource"),
                    detail: "owned resource not found",
                });
            };
            inner.resources.remove(index)
        };
        resource.release().map_err(|_| PnpError::InvalidState)
    }

    fn set_deferred_dependency(&self, dependency: Option<PnpDependency>) {
        self.inner.lock().deferred_dependency = dependency;
    }

    fn begin_probe(self: &Arc<Self>, owner: Option<ElmPnpOwner>) -> Result<(), PnpError> {
        if self.removal_lock.load(Ordering::Acquire) {
            return Err(PnpError::InvalidState);
        }
        let mut inner = self.inner.lock();
        if self.removal_lock.load(Ordering::Acquire) || inner.state != PnpState::Discovered {
            return Err(PnpError::InvalidState);
        }
        inner.driver_binding_generation = inner
            .driver_binding_generation
            .checked_add(1)
            .ok_or(PnpError::OutOfMemory)?;
        inner.driver_owner = owner;
        inner.state = PnpState::Probing;
        Ok(())
    }
}

#[kernel_symbols::export]
impl PnpProviderResourceScope {
    fn resolve_device(&self) -> Result<Arc<PnpDevice>, PnpError> {
        let device = self.device.upgrade().ok_or(PnpError::InvalidState)?;
        if device.runtime_id != self.runtime_id {
            return Err(PnpError::InvalidState);
        }
        Ok(device)
    }

    fn enter_provider_execution_context(
        &self,
        operation: &'static str,
    ) -> Result<PnpProviderExecutionContext, PnpError> {
        match self.context {
            Some(context) if elm_model::current_context() == Some(context) => {
                Ok(PnpProviderExecutionContext::AlreadyActive)
            }
            Some(context) => {
                let Some(guard) = enter_elm_snapshot(context) else {
                    log::error!(
                        "[pnp] cannot enter provider ELM context for runtime resource {}: cell={} generation={}",
                        operation,
                        context.cell_id.0,
                        context.generation.0
                    );
                    super::elm_lifecycle::mark_context_failed(context);
                    return Err(PnpError::InvalidState);
                };
                Ok(PnpProviderExecutionContext::Dynamic(guard))
            }
            None if elm_model::current_context().is_none() => {
                Ok(PnpProviderExecutionContext::AlreadyActive)
            }
            None => elm_model::suspend_current_context()
                .map(PnpProviderExecutionContext::Builtin)
                .ok_or_else(|| {
                    log::error!(
                        "[pnp] cannot suspend consumer ELM context for built-in provider operation {}",
                        operation
                    );
                    PnpError::InvalidState
                }),
        }
    }

    /// 进入签发本 scope 的 provider 执行上下文。
    ///
    /// guard 覆盖的代码可以安全执行隐式分配、MSI vector 分配、IRQ handler
    /// 注册和最终 PnP 资源登记；这些操作不会继承外层 consumer 的 ELM owner。
    #[kernel_symbols::export(
        name = "general.dev.pnp.PnpProviderResourceScope.enter_context",
        contract = "kernel.general.device-resource@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn enter_context(&self) -> Result<PnpProviderContextGuard, PnpError> {
        let device = self.resolve_device()?;
        device.provider_resource_access(self)?;
        let context = self.enter_provider_execution_context("enter_context")?;
        Ok(PnpProviderContextGuard {
            context: ManuallyDrop::new(context),
            _not_send: PhantomData,
        })
    }

    /// 将运行期资源交给签发本 scope 的 provider 设备。
    pub fn own_resource<R>(&self, resource: R) -> Result<(), PnpError>
    where
        R: PnpResource + 'static,
    {
        // 先进入 provider 边界再分配 Box，避免内建 provider 的资源对象被计入
        // 触发回调的动态 consumer 分配账户。
        let _provider_context = self.enter_context()?;
        self.own_boxed_resource(Box::new(resource))
    }

    /// 将已擦除类型的运行期资源交给签发本 scope 的 provider 设备。
    #[kernel_symbols::export(
        name = "general.dev.pnp.PnpProviderResourceScope.own_boxed_resource",
        contract = "kernel.general.device-resource@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE,
        retained_args = 2u64
    )]
    pub fn own_boxed_resource(&self, resource: Box<dyn PnpResource>) -> Result<(), PnpError> {
        // 若原 generation 已无法进入，不能在 consumer 上下文中执行动态资源的
        // drop glue；保留对象并让所属 ELM fail closed。
        let _provider_guard = match self.enter_provider_execution_context("own_resource") {
            Ok(guard) => guard,
            Err(error) => {
                core::mem::forget(resource);
                return Err(error);
            }
        };
        let result = match self.resolve_device() {
            Ok(device) => match device.provider_resource_access(self) {
                Ok(access) => device.insert_driver_resource_with_access(resource, access),
                Err(error) => Err((error, resource)),
            },
            Err(error) => Err((error, resource)),
        };
        match result {
            Ok(()) => Ok(()),
            Err((error, resource)) => {
                // provider guard 仍然存活；无论是原始资源还是已经包装的代理，
                // 失败回滚都不会借用外层 consumer 的 ELM 身份。
                drop(resource);
                Err(error)
            }
        }
    }

    /// 为 provider 设备后续成组登记的运行期资源预留槽位。
    #[kernel_symbols::export(
        name = "general.dev.pnp.PnpProviderResourceScope.reserve_owned_resources",
        contract = "kernel.general.device-resource@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn reserve_owned_resources(&self, additional: usize) -> Result<(), PnpError> {
        let _provider_guard = self.enter_provider_execution_context("reserve_owned_resources")?;
        let device = self.resolve_device()?;
        let access = device.provider_resource_access(self)?;
        device.reserve_owned_resources_with_access(additional, access)
    }
}

#[kernel_symbols::export]
impl Drop for PnpProviderContextGuard {
    #[kernel_symbols::export(
        name = "general.dev.pnp.PnpProviderContextGuard.drop",
        contract = "kernel.general.device-resource@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    fn drop(&mut self) {
        // Safety: `context` 只在构造时初始化一次，且本 Drop 是唯一消费路径；
        // `ManuallyDrop` 阻止跨 ELM 调用方生成内部 guard 的析构 glue。
        unsafe { ManuallyDrop::drop(&mut self.context) };
    }
}

// ── PnpDriver ────────────────────────────────────────────────────────────

pub trait PnpDriver: Send + Sync {
    /// 驱动名称，用于日志、去重和调试输出。
    fn name(&self) -> &str;

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

    /// 在拥有完整设备对象的发现路径上执行匹配。
    ///
    /// 默认实现保持旧的 `(id, info)` 语义；需要观察父子关系、运行期属性或其它
    /// 设备对象状态的动态驱动可以覆盖本入口，避免通过全局列表反查对象。
    fn matches_device(&self, device: &Arc<PnpDevice>) -> bool {
        self.matches(&device.id, device.info.as_ref())
    }

    /// 初始化硬件并注册该设备暴露的 function。
    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError>;

    /// 移除设备时关闭硬件并释放驱动私有状态。
    fn remove(&self, dev: &Arc<PnpDevice>);

    /// 确认 core 稍后可以安全进入 [`Self::try_remove`]。
    ///
    /// 内建驱动不需要额外门禁。动态 ELM 代理用此阶段在任何设备或资源
    /// 发生不可逆变更前验证主上下文仍可进入。
    fn prepare_remove(&self) -> Result<(), PnpError> {
        Ok(())
    }

    /// 执行可报告上下文门禁失败的 remove。
    fn try_remove(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        self.remove(dev);
        Ok(())
    }

    /// 释放一个驱动私有 trait object。
    ///
    /// 动态代理覆盖此入口，使 probe 回滚与 remove 兜底清理中的 drop glue
    /// 也在所属 ELM generation 下执行。
    fn release_driver_data(&self, data: Arc<dyn Any + Send + Sync>) -> Result<(), PnpError> {
        drop(data);
        Ok(())
    }
}

/// 常驻内核 vtable 与动态 ELM 驱动实例之间的身份代理。
///
/// 名称、总线和优先级在登记时复制到常驻内存；所有后续会跳入模块
/// vtable 的路径均先恢复完整 [`elm_model::ElmCurrentContext`]。
struct ElmPnpDriverProxy {
    context: elm_model::ElmCurrentContext,
    name: String,
    bus_type: BusType,
    priority: PnpDriverPriority,
    driver: Option<Arc<dyn PnpDriver>>,
}

impl ElmPnpDriverProxy {
    fn wrap(
        driver: Arc<dyn PnpDriver>,
        context: elm_model::ElmCurrentContext,
    ) -> Result<Arc<dyn PnpDriver>, PnpError> {
        let bus_type = driver.bus_type();
        let priority = driver.priority();
        let driver_name = driver.name();
        // proxy Arc 和名称缓存是常驻注册表元数据，不应阻塞所属 ELM
        // 在驱动已正常注销后回收隐式分配。
        let _accounting =
            allocator::suspend_implicit_allocation_accounting().ok_or(PnpError::OutOfMemory)?;
        let mut name = String::new();
        name.try_reserve_exact(driver_name.len())
            .map_err(|_| PnpError::OutOfMemory)?;
        name.push_str(driver_name);
        Ok(Arc::new(Self {
            context,
            name,
            bus_type,
            priority,
            driver: Some(driver),
        }))
    }

    fn driver(&self) -> &dyn PnpDriver {
        self.driver
            .as_deref()
            .expect("ELM PnP driver proxy used after drop")
    }

    fn enter(&self, operation: &'static str) -> Option<elm_model::ElmCurrentContextGuard> {
        let guard = enter_elm_snapshot(self.context);
        if guard.is_none() {
            log::error!(
                "[pnp] cannot enter ELM context for driver {} operation {}: cell={} generation={}",
                self.name,
                operation,
                self.context.cell_id.0,
                self.context.generation.0
            );
        }
        guard
    }
}

impl PnpDriver for ElmPnpDriverProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn bus_type(&self) -> BusType {
        self.bus_type
    }

    fn priority(&self) -> PnpDriverPriority {
        self.priority
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        let Some(_guard) = self.enter("matches") else {
            return false;
        };
        self.driver().matches(id, info)
    }

    fn matches_device(&self, device: &Arc<PnpDevice>) -> bool {
        let Some(_guard) = self.enter("matches_device") else {
            return false;
        };
        self.driver().matches_device(device)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let Some(_guard) = self.enter("probe") else {
            return Err(PnpError::InvalidState);
        };
        self.driver().probe(dev)
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if self.try_remove(dev).is_err() {
            super::elm_lifecycle::mark_context_failed(self.context);
        }
    }

    fn prepare_remove(&self) -> Result<(), PnpError> {
        let Some(_guard) = self.enter("prepare_remove") else {
            return Err(PnpError::InvalidState);
        };
        self.driver().prepare_remove()
    }

    fn try_remove(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let Some(_guard) = self.enter("remove") else {
            super::elm_lifecycle::mark_context_failed(self.context);
            return Err(PnpError::InvalidState);
        };
        self.driver().try_remove(dev)
    }

    fn release_driver_data(&self, data: Arc<dyn Any + Send + Sync>) -> Result<(), PnpError> {
        let Some(_guard) = self.enter("driver_data_drop") else {
            super::elm_lifecycle::mark_context_failed(self.context);
            core::mem::forget(data);
            return Err(PnpError::InvalidState);
        };
        drop(data);
        Ok(())
    }
}

impl Drop for ElmPnpDriverProxy {
    fn drop(&mut self) {
        let Some(driver) = self.driver.take() else {
            return;
        };
        let Some(_guard) = self.enter("drop") else {
            super::elm_lifecycle::mark_context_failed(self.context);
            core::mem::forget(driver);
            return;
        };
        drop(driver);
    }
}

#[derive(Clone)]
enum PreparedDeviceAction {
    Remove,
    Unbind(Arc<dyn PnpDriver>),
}

#[derive(Clone)]
struct PreparedPnpDevice {
    device: Arc<PnpDevice>,
    action: PreparedDeviceAction,
}

/// 一组 PnP 子树的可取消移除事务。
///
/// `prepare` 只冻结设备和资源，不改变设备状态。事务按叶节点优先保存设备，并在
/// 所有 consumer 都已声明随事务释放后才冻结 provider。未调用 [`Self::commit`] 的
/// 事务会在析构时自动 cancel。
pub struct PnpRemovalTransaction {
    devices: Vec<PreparedPnpDevice>,
    active: bool,
}

impl PnpRemovalTransaction {
    /// 冻结给定设备及其完整子树。输入中互为祖先的设备会合并为一棵事务子树。
    pub fn prepare(devices: &[Arc<PnpDevice>]) -> Result<Self, PnpError> {
        Self::prepare_with_actions(
            devices
                .iter()
                .map(|device| (Arc::clone(device), PreparedDeviceAction::Remove)),
        )
    }

    fn prepare_with_actions(
        roots: impl Iterator<Item = (Arc<PnpDevice>, PreparedDeviceAction)>,
    ) -> Result<Self, PnpError> {
        let mut requested = Vec::new();
        for root in roots {
            requested
                .try_reserve(1)
                .map_err(|_| PnpError::OutOfMemory)?;
            requested.push(root);
        }

        let mut transaction = Self {
            devices: Vec::new(),
            active: true,
        };
        for (root, action) in &requested {
            let covered_by_ancestor = requested.iter().any(|(candidate, _)| {
                !Arc::ptr_eq(candidate, root) && root.has_ancestor(candidate)
            });
            if covered_by_ancestor {
                continue;
            }
            if let Err(error) = transaction.lock_subtree(root) {
                transaction.cancel();
                return Err(error);
            }
            let Some(prepared) = transaction
                .devices
                .iter_mut()
                .find(|prepared| Arc::ptr_eq(&prepared.device, root))
            else {
                transaction.cancel();
                return Err(PnpError::InvalidState);
            };
            prepared.action = action.clone();
        }

        if let Err(error) = transaction.reorder_for_resource_dependencies() {
            transaction.cancel();
            return Err(error);
        }
        if let Err(error) = transaction.prepare_driver_callbacks() {
            transaction.cancel();
            return Err(error);
        }
        if let Err(error) = transaction.prepare_resources() {
            transaction.cancel();
            return Err(error);
        }
        Ok(transaction)
    }

    fn lock_subtree(&mut self, device: &Arc<PnpDevice>) -> Result<(), PnpError> {
        if self
            .devices
            .iter()
            .any(|prepared| Arc::ptr_eq(&prepared.device, device))
        {
            return Ok(());
        }
        if device
            .removal_lock
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(PnpError::InvalidState);
        }

        let children = {
            let inner = device.inner.lock();
            if !matches!(inner.state, PnpState::Discovered | PnpState::Bound) {
                device.removal_lock.store(false, Ordering::Release);
                return Err(PnpError::InvalidState);
            }
            let mut children = Vec::new();
            if children.try_reserve(inner.children.len()).is_err() {
                device.removal_lock.store(false, Ordering::Release);
                return Err(PnpError::OutOfMemory);
            }
            children.extend(inner.children.iter().cloned());
            children
        };
        for child in children.iter().rev() {
            if let Err(error) = self.lock_subtree(child) {
                device.removal_lock.store(false, Ordering::Release);
                return Err(error);
            }
        }
        if self.devices.try_reserve(1).is_err() {
            device.removal_lock.store(false, Ordering::Release);
            return Err(PnpError::OutOfMemory);
        }
        self.devices.push(PreparedPnpDevice {
            device: Arc::clone(device),
            action: PreparedDeviceAction::Remove,
        });
        Ok(())
    }

    fn action_removes_device(action: &PreparedDeviceAction) -> bool {
        matches!(action, PreparedDeviceAction::Remove)
    }

    fn prepared_providers(
        prepared: &PreparedPnpDevice,
        index: usize,
        providers: &mut Vec<(usize, PnpDependency)>,
    ) -> Result<(), PnpError> {
        let inner = prepared.device.inner.lock();
        let bus = Self::action_removes_device(&prepared.action)
            .then_some(inner.bus_resources.as_slice())
            .into_iter()
            .flatten()
            .map(Box::as_ref);
        for resource in bus.chain(inner.resources.iter().map(Box::as_ref)) {
            let Some(dependency) = resource.provided_dependency() else {
                continue;
            };
            if providers
                .iter()
                .any(|(owner, existing)| *owner == index && *existing == dependency)
            {
                continue;
            }
            providers
                .try_reserve(1)
                .map_err(|_| PnpError::OutOfMemory)?;
            providers.push((index, dependency));
        }
        Ok(())
    }

    fn prepared_consumes(prepared: &PreparedPnpDevice, dependency: PnpDependency) -> bool {
        let inner = prepared.device.inner.lock();
        let bus = Self::action_removes_device(&prepared.action)
            .then_some(inner.bus_resources.as_slice())
            .into_iter()
            .flatten()
            .map(Box::as_ref);
        bus.chain(inner.resources.iter().map(Box::as_ref))
            .any(|resource| resource.consumes_dependency(dependency))
    }

    /// 在保持原有叶节点优先顺序的基础上加入显式 consumer -> provider 边。
    fn reorder_for_resource_dependencies(&mut self) -> Result<(), PnpError> {
        let count = self.devices.len();
        if count < 2 {
            return Ok(());
        }

        let edge_count = count.checked_mul(count).ok_or(PnpError::OutOfMemory)?;
        let mut edges = Vec::new();
        edges
            .try_reserve_exact(edge_count)
            .map_err(|_| PnpError::OutOfMemory)?;
        edges.resize(edge_count, false);
        let mut indegree = Vec::new();
        indegree
            .try_reserve_exact(count)
            .map_err(|_| PnpError::OutOfMemory)?;
        indegree.resize(count, 0usize);

        let add_edge = |from: usize,
                        to: usize,
                        edges: &mut [bool],
                        indegree: &mut [usize]|
         -> Result<(), PnpError> {
            if from == to || edges[from * count + to] {
                return Ok(());
            }
            edges[from * count + to] = true;
            indegree[to] = indegree[to].checked_add(1).ok_or(PnpError::OutOfMemory)?;
            Ok(())
        };

        // 先固化原有 PnP 拓扑：child 必须在 parent 前提交。
        for (child_index, prepared) in self.devices.iter().enumerate() {
            let Some(parent) = prepared.device.parent() else {
                continue;
            };
            if let Some(parent_index) = self
                .devices
                .iter()
                .position(|candidate| Arc::ptr_eq(&candidate.device, &parent))
            {
                add_edge(child_index, parent_index, &mut edges, &mut indegree)?;
            }
        }

        let mut providers = Vec::new();
        for (index, prepared) in self.devices.iter().enumerate() {
            Self::prepared_providers(prepared, index, &mut providers)?;
        }
        for (provider_index, dependency) in providers {
            for (consumer_index, prepared) in self.devices.iter().enumerate() {
                if consumer_index != provider_index && Self::prepared_consumes(prepared, dependency)
                {
                    add_edge(consumer_index, provider_index, &mut edges, &mut indegree)?;
                }
            }
        }

        let mut placed = Vec::new();
        placed
            .try_reserve_exact(count)
            .map_err(|_| PnpError::OutOfMemory)?;
        placed.resize(count, false);
        let mut ordered = Vec::new();
        ordered
            .try_reserve_exact(count)
            .map_err(|_| PnpError::OutOfMemory)?;
        while ordered.len() != count {
            let Some(next) = (0..count).find(|&index| !placed[index] && indegree[index] == 0)
            else {
                return Err(PnpError::InvalidState);
            };
            placed[next] = true;
            ordered.push(self.devices[next].clone());
            for target in 0..count {
                if edges[next * count + target] {
                    indegree[target] = indegree[target].saturating_sub(1);
                }
            }
        }
        self.devices = ordered;
        Ok(())
    }

    fn prepare_driver_callbacks(&self) -> Result<(), PnpError> {
        for prepared in &self.devices {
            let driver = prepared.device.inner.lock().bound_driver.clone();
            if let Some(driver) = driver {
                driver.prepare_remove()?;
            }
        }
        Ok(())
    }

    fn prepare_resources(&self) -> Result<(), PnpError> {
        for order in [
            PnpResourceReleaseOrder::Consumer,
            PnpResourceReleaseOrder::Regular,
            PnpResourceReleaseOrder::Provider,
        ] {
            for prepared in &self.devices {
                let inner = prepared.device.inner.lock();
                let bus = Self::action_removes_device(&prepared.action)
                    .then_some(inner.bus_resources.as_slice())
                    .into_iter()
                    .flatten()
                    .map(Box::as_ref);
                for resource in bus.chain(inner.resources.iter().map(Box::as_ref)).rev() {
                    if resource.release_order() != order {
                        continue;
                    }
                    if let Err(error) = resource.prepare_release() {
                        return Err(PnpError::ResourceBusy {
                            kind: error.kind,
                            detail: error.detail,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn cancel(&mut self) {
        if !self.active {
            return;
        }
        for order in [
            PnpResourceReleaseOrder::Provider,
            PnpResourceReleaseOrder::Regular,
            PnpResourceReleaseOrder::Consumer,
        ] {
            for prepared in self.devices.iter().rev() {
                let inner = prepared.device.inner.lock();
                let bus = Self::action_removes_device(&prepared.action)
                    .then_some(inner.bus_resources.as_slice())
                    .into_iter()
                    .flatten()
                    .map(Box::as_ref);
                for resource in bus.chain(inner.resources.iter().map(Box::as_ref)) {
                    if resource.release_order() == order {
                        resource.cancel_release();
                    }
                }
            }
        }
        for prepared in self.devices.iter().rev() {
            prepared.device.removal_lock.store(false, Ordering::Release);
        }
        self.active = false;
    }

    /// 提交已经完整 prepare 的子树，设备按 consumer-first 顺序进入 remove。
    pub fn commit(mut self) -> Result<(), PnpError> {
        for prepared in &self.devices {
            if let Err(error) = prepared.device.commit_prepared_action(&prepared.action) {
                // commit 失败表示资源实现违反了 prepare 契约。已经执行的 remove 无法
                // 回滚，剩余设备保持冻结，调用方必须把上层事务标记为 tainted。
                self.active = false;
                return Err(error);
            }
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for PnpRemovalTransaction {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// 已关闭驱动接纳入口、但尚未解绑设备的可取消注销事务。
pub(crate) struct PreparedDriverDetach<'a> {
    registry: &'a PnpDriverRegistry,
    handles: Vec<DriverHandle>,
    removal: Option<PnpRemovalTransaction>,
    restore_accepting: bool,
}

impl PreparedDriverDetach<'_> {
    pub(crate) fn commit(mut self) -> Result<(), PnpError> {
        let removal = self.removal.take().ok_or(PnpError::InvalidState)?;
        if let Err(error) = removal.commit() {
            // PnP commit 已进入不可逆阶段，不能重新开放驱动。
            self.restore_accepting = false;
            return Err(error);
        }
        self.registry
            .drivers
            .lock()
            .retain(|registered| !self.handles.iter().any(|handle| registered.id == handle.id));
        self.restore_accepting = false;
        let _ = self.registry.retry_deferred_devices();
        Ok(())
    }
}

impl Drop for PreparedDriverDetach<'_> {
    fn drop(&mut self) {
        if !self.restore_accepting {
            return;
        }
        // 先撤销 provider/设备冻结，再重新开放 probe，避免 accepting=true 与
        // removal_lock 尚未释放之间出现一次丢失的匹配机会。
        drop(self.removal.take());
        let drivers = self.registry.drivers.lock();
        for registered in drivers.iter() {
            if self.handles.iter().any(|handle| registered.id == handle.id) {
                registered.accepting.store(true, Ordering::Release);
            }
        }
    }
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    fn name(&self) -> &str;
    /// 根据启动期上下文创建一个可注册的 PnP 驱动实例。
    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError>;
}

struct RegisteredDriver {
    id: DriverId,
    driver: Arc<dyn PnpDriver>,
    owner: Option<ElmPnpOwner>,
    accepting: AtomicBool,
}

#[derive(Clone)]
struct DriverCandidate {
    driver: Arc<dyn PnpDriver>,
    owner: Option<ElmPnpOwner>,
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
        let elm_context = elm_model::current_context();
        let factory_name = factory.name();
        let driver = match factory.create(&ctx) {
            Ok(driver) => driver,
            Err(error) => {
                log::error!(
                    "[pnp] driver factory {} create failed: {:?}",
                    factory_name,
                    error
                );
                return Err(error);
            }
        };
        let driver = match elm_context {
            Some(context) => ElmPnpDriverProxy::wrap(driver, context)?,
            None => driver,
        };
        let driver_name = driver.name();
        let id = {
            let mut drivers = self.drivers.lock();
            if drivers
                .iter()
                .any(|registered| registered.driver.name() == driver_name)
            {
                log::error!("[pnp] duplicate driver name: {}", driver_name);
                return Err(PnpError::NameConflict);
            }
            {
                // 驱动表容量属于常驻内核注册表，不能在动态 ELM 卸载后继续计入该单元。
                let _accounting = allocator::suspend_implicit_allocation_accounting()
                    .ok_or(PnpError::OutOfMemory)?;
                drivers.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
            }
            let id = alloc_driver_id(&self.next_driver_id)?;
            drivers.push(RegisteredDriver {
                id,
                driver: Arc::clone(&driver),
                owner: elm_context.map(ElmPnpOwner::from_context),
                accepting: AtomicBool::new(true),
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

        log::debug!("[pnp] registered driver {}", driver_name);
        Ok(DriverHandle { id })
    }

    /// 注销驱动并解绑当前由它管理的设备。
    ///
    /// 驱动注销不同于硬件热拔：PnP 设备对象仍保留在全局表中，状态回到
    /// `Discovered`，随后可以被剩余驱动重新 probe。这样动态驱动或后续更具体
    /// 的驱动接入时，不需要重新执行固件/总线枚举。
    pub fn unregister(&self, handle: DriverHandle) -> Result<(), PnpError> {
        self.prepare_detach(core::slice::from_ref(&handle), &[])?
            .commit()
    }

    pub(crate) fn prepare_detach<'a>(
        &'a self,
        handles: &[DriverHandle],
        remove_devices: &[Arc<PnpDevice>],
    ) -> Result<PreparedDriverDetach<'a>, PnpError> {
        let mut selected = Vec::new();
        {
            let drivers = self.drivers.lock();
            selected
                .try_reserve(handles.len())
                .map_err(|_| PnpError::OutOfMemory)?;
            for handle in handles {
                if selected
                    .iter()
                    .any(|(selected_handle, _)| selected_handle == handle)
                {
                    continue;
                }
                let registered = drivers
                    .iter()
                    .find(|registered| registered.id == handle.id)
                    .ok_or(PnpError::NoDriver)?;
                selected.push((*handle, Arc::clone(&registered.driver)));
            }
            for (_, driver) in &selected {
                if let Some(registered) = drivers
                    .iter()
                    .find(|registered| Arc::ptr_eq(&registered.driver, driver))
                {
                    registered.accepting.store(false, Ordering::Release);
                }
            }
        }

        let result = (|| {
            let devices = PNP_DEVICES.try_list().ok_or(PnpError::OutOfMemory)?;
            let mut actions = Vec::new();
            actions
                .try_reserve(devices.len().saturating_add(remove_devices.len()))
                .map_err(|_| PnpError::OutOfMemory)?;
            for device in devices {
                if let Some((_, driver)) = selected
                    .iter()
                    .find(|(_, driver)| device.bound_to_driver(driver))
                {
                    actions.push((device, PreparedDeviceAction::Unbind(Arc::clone(driver))));
                }
            }
            actions.extend(
                remove_devices
                    .iter()
                    .map(|device| (Arc::clone(device), PreparedDeviceAction::Remove)),
            );
            PnpRemovalTransaction::prepare_with_actions(actions.into_iter())
        })();

        match result {
            Ok(removal) => {
                let mut owned_handles = Vec::new();
                if owned_handles.try_reserve(selected.len()).is_err() {
                    drop(removal);
                    let drivers = self.drivers.lock();
                    for (_, driver) in &selected {
                        if let Some(registered) = drivers
                            .iter()
                            .find(|registered| Arc::ptr_eq(&registered.driver, driver))
                        {
                            registered.accepting.store(true, Ordering::Release);
                        }
                    }
                    return Err(PnpError::OutOfMemory);
                }
                owned_handles.extend(selected.iter().map(|(handle, _)| *handle));
                Ok(PreparedDriverDetach {
                    registry: self,
                    handles: owned_handles,
                    removal: Some(removal),
                    restore_accepting: true,
                })
            }
            Err(error) => {
                let drivers = self.drivers.lock();
                for (_, driver) in &selected {
                    if let Some(registered) = drivers
                        .iter()
                        .find(|registered| Arc::ptr_eq(&registered.driver, driver))
                    {
                        registered.accepting.store(true, Ordering::Release);
                    }
                }
                Err(error)
            }
        }
    }

    /// 用指定驱动重新尝试认领已经发现但尚未绑定的设备。
    pub fn probe_existing_devices(&self, driver_id: DriverId) -> Result<usize, PnpError> {
        let candidate = self.driver_by_id(driver_id).ok_or(PnpError::NoDriver)?;
        let mut bound = 0usize;
        for dev in PNP_DEVICES.try_list().ok_or(PnpError::OutOfMemory)? {
            if dev.state() != PnpState::Discovered {
                continue;
            }
            if !driver_can_probe_bus(candidate.driver.as_ref(), dev.info.bus_type()) {
                continue;
            }
            if !candidate.driver.matches_device(&dev) {
                continue;
            }
            match self.bind_driver_to_device(&dev, candidate.clone()) {
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
            let devices = match PNP_DEVICES.try_list() {
                Some(devices) => devices,
                None => {
                    self.retrying_deferred.store(false, Ordering::Release);
                    return Err(PnpError::OutOfMemory);
                }
            };
            for dev in devices {
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
        candidate: DriverCandidate,
    ) -> Result<(), PnpError> {
        let driver = candidate.driver;
        if !self.driver_is_accepting(&driver) {
            return Err(PnpError::NoDriver);
        }
        dev.begin_probe(candidate.owner)?;

        match driver.probe(dev) {
            Ok(()) => {
                // accepting 校验与 Bound 提交必须位于同一段注册表锁内。否则注销线程
                // 可能已经完成设备快照，旧 probe 随后却把已摘除驱动重新装回设备。
                let drivers = self.drivers.lock();
                if !drivers.iter().any(|registered| {
                    registered.accepting.load(Ordering::Acquire)
                        && Arc::ptr_eq(&registered.driver, &driver)
                }) {
                    drop(drivers);
                    dev.rollback_probe_side_effects(&driver);
                    return Err(PnpError::NoDriver);
                }
                let mut inner = dev.inner.lock();
                if inner.state != PnpState::Probing {
                    drop(inner);
                    drop(drivers);
                    dev.rollback_probe_side_effects(&driver);
                    return Err(PnpError::InvalidState);
                }
                inner.bound_driver = Some(driver);
                inner.state = PnpState::Bound;
                inner.deferred_dependency = None;
                Ok(())
            }
            Err(err) => {
                let deferred_dependency = err.deferred_dependency();
                dev.rollback_probe_side_effects(&driver);
                dev.set_deferred_dependency(deferred_dependency);
                Err(err)
            }
        }
    }

    fn find_matching_driver(
        &self,
        dev: &Arc<PnpDevice>,
    ) -> Result<Option<DriverCandidate>, PnpError> {
        // 驱动的 matches_device 可能进入 ELM 回调，并在回调中注册其它设备资源。
        // 不能在执行不受信任回调时持有 drivers 锁，否则嵌套注册/注销会死锁。
        let candidates = {
            let drivers = self.drivers.lock();
            let mut candidates = Vec::new();
            candidates
                .try_reserve(drivers.len())
                .map_err(|_| PnpError::OutOfMemory)?;
            candidates.extend(
                drivers
                    .iter()
                    .filter(|registered| registered.accepting.load(Ordering::Acquire))
                    .map(|registered| DriverCandidate {
                        driver: Arc::clone(&registered.driver),
                        owner: registered.owner,
                    }),
            );
            candidates
        };
        let mut best: Option<((u8, PnpDriverPriority), DriverCandidate)> = None;

        for candidate in candidates {
            let driver = &candidate.driver;
            if !driver_can_probe_bus(driver.as_ref(), dev.info.bus_type()) {
                continue;
            }
            if !driver.matches_device(dev) {
                continue;
            }
            let bus_rank = if driver.bus_type() == dev.info.bus_type() {
                1
            } else {
                0
            };
            let key = (bus_rank, driver.priority());
            match best.as_ref() {
                None => best = Some((key, candidate)),
                Some((best_key, _)) if key > *best_key => {
                    best = Some((key, candidate));
                }
                Some((best_key, _)) if key == *best_key => {
                    log::warning!(
                        "[pnp] DriverAmbiguous for {}: '{}' vs '{}' (bus_rank={}, priority={:?})",
                        dev.id,
                        best.as_ref().unwrap().1.driver.name(),
                        candidate.driver.name(),
                        bus_rank,
                        driver.priority(),
                    );
                    return Err(PnpError::DriverAmbiguous);
                }
                _ => {}
            }
        }

        Ok(best.map(|(_, driver)| driver))
    }

    fn driver_by_id(&self, id: DriverId) -> Option<DriverCandidate> {
        self.drivers
            .lock()
            .iter()
            .find(|registered| registered.id == id && registered.accepting.load(Ordering::Acquire))
            .map(|registered| DriverCandidate {
                driver: Arc::clone(&registered.driver),
                owner: registered.owner,
            })
    }

    fn driver_is_accepting(&self, driver: &Arc<dyn PnpDriver>) -> bool {
        self.drivers.lock().iter().any(|registered| {
            registered.accepting.load(Ordering::Acquire) && Arc::ptr_eq(&registered.driver, driver)
        })
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
#[kernel_symbols::export(
    name = "general.dev.pnp.register_driver_factory",
    contract = "kernel.general.pnp-driver@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn register_driver_factory(factory: Arc<dyn DriverFactory>) -> Result<DriverHandle, PnpError> {
    let handle = PNP_DRIVERS.register_factory(factory)?;
    if super::elm_lifecycle::track_driver(handle).is_err() {
        log::error!(
            "[pnp] 无法登记驱动资源归属: driver_id={}",
            handle.id().raw()
        );
        let _ = PNP_DRIVERS.unregister(handle);
        return Err(PnpError::OutOfMemory);
    }
    Ok(handle)
}

/// 注销驱动的全局便捷入口。
#[kernel_symbols::export(
    name = "general.dev.pnp.unregister_driver",
    contract = "kernel.general.pnp-driver@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_driver(handle: DriverHandle) -> Result<(), PnpError> {
    PNP_DRIVERS.unregister(handle)?;
    super::elm_lifecycle::forget_driver(handle);
    Ok(())
}

/// 让指定驱动认领当前尚未绑定的既有设备。
#[kernel_symbols::export(
    name = "general.dev.pnp.probe_existing_devices",
    contract = "kernel.general.pnp-driver@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn probe_existing_devices(driver_id: DriverId) -> Result<usize, PnpError> {
    PNP_DRIVERS.probe_existing_devices(driver_id)
}

/// 通知一个 deferred 依赖已经就绪。
///
/// 资源 registry 在成功登记 controller、syscon、fwcfg 等依赖后调用该函数。
/// PnP core 会只重试记录了同一依赖的设备；旧式没有精确依赖的 deferred 设备仍由
/// [`PnpDriverRegistry::retry_deferred_devices`] 兜底处理。
#[kernel_symbols::export(
    name = "general.dev.pnp.notify_dependency_ready",
    contract = "kernel.general.pnp-driver@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
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

/// PnP 设备全局可见性事件类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PnpDeviceEventKind {
    /// 新对象已经进入全局 PnP 设备表。
    Registered,
    /// 精确对象已经离开全局 PnP 设备表，旧观察句柄必须失效。
    Removed,
}

/// PnP 设备全局可见性事件。
#[derive(Clone)]
pub struct PnpDeviceEvent {
    kind: PnpDeviceEventKind,
    device: Arc<PnpDevice>,
}

impl PnpDeviceEvent {
    pub const fn kind(&self) -> PnpDeviceEventKind {
        self.kind
    }

    pub fn device(&self) -> &Arc<PnpDevice> {
        &self.device
    }
}

pub type PnpDeviceEventCallback = fn(&PnpDeviceEvent);

#[derive(Clone, Copy)]
struct PnpDeviceEventSubscriber {
    owner: &'static str,
    name: &'static str,
    callback: PnpDeviceEventCallback,
}

static PNP_DEVICE_EVENT_SUBSCRIBERS: Spinlock<Vec<PnpDeviceEventSubscriber>> =
    Spinlock::new(Vec::new());

/// 注册 PnP 设备生命周期观察者；相同 owner/name 的重复注册是幂等的。
#[kernel_symbols::export(
    name = "general.dev.pnp.subscribe_device_events",
    contract = "kernel.general.pnp-events@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn subscribe_device_events(
    owner: &'static str,
    name: &'static str,
    callback: PnpDeviceEventCallback,
) -> Result<bool, PnpError> {
    let mut owned_name = String::new();
    owned_name
        .try_reserve(name.len())
        .map_err(|_| PnpError::OutOfMemory)?;
    owned_name.push_str(name);
    let mut subscribers = PNP_DEVICE_EVENT_SUBSCRIBERS.lock();
    if subscribers
        .iter()
        .any(|subscriber| subscriber.owner == owner && subscriber.name == name)
    {
        return Ok(false);
    }
    subscribers
        .try_reserve(1)
        .map_err(|_| PnpError::OutOfMemory)?;
    subscribers.push(PnpDeviceEventSubscriber {
        owner,
        name,
        callback,
    });
    drop(subscribers);
    if super::elm_lifecycle::track_event_subscription(owner, owned_name.into_boxed_str()).is_err() {
        let _ = unsubscribe_device_events(owner, name);
        return Err(PnpError::OutOfMemory);
    }
    Ok(true)
}

/// 注销 PnP 设备生命周期观察者。
#[kernel_symbols::export(
    name = "general.dev.pnp.unsubscribe_device_events",
    contract = "kernel.general.pnp-events@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unsubscribe_device_events(owner: &'static str, name: &str) -> Result<(), PnpError> {
    let mut subscribers = PNP_DEVICE_EVENT_SUBSCRIBERS.lock();
    let Some(index) = subscribers
        .iter()
        .position(|subscriber| subscriber.owner == owner && subscriber.name == name)
    else {
        return Err(PnpError::MissingResource {
            kind: PnpResourceKind::Other("pnp-device-event"),
            detail: "device event subscriber not found",
        });
    };
    subscribers.swap_remove(index);
    drop(subscribers);
    super::elm_lifecycle::forget_event_subscription(owner, name);
    Ok(())
}

fn publish_device_event(kind: PnpDeviceEventKind, device: Arc<PnpDevice>) {
    let event = PnpDeviceEvent { kind, device };
    let mut last_key = None;
    while let Some(subscriber) = next_device_event_subscriber(last_key) {
        (subscriber.callback)(&event);
        last_key = Some((subscriber.owner, subscriber.name));
    }
}

fn next_device_event_subscriber(
    last_key: Option<(&'static str, &'static str)>,
) -> Option<PnpDeviceEventSubscriber> {
    let subscribers = PNP_DEVICE_EVENT_SUBSCRIBERS.lock();
    let mut next = None;
    for subscriber in subscribers.iter().copied() {
        let key = (subscriber.owner, subscriber.name);
        if last_key.is_some_and(|last_key| key <= last_key) {
            continue;
        }
        if next
            .is_none_or(|existing: PnpDeviceEventSubscriber| key < (existing.owner, existing.name))
        {
            next = Some(subscriber);
        }
    }
    next
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
        loop {
            // 先只在列表锁内复制 Arc，不读取设备状态；设备移除路径的锁顺序是
            // device -> list，读取 state 必须放到释放列表锁之后，避免反转死锁。
            let existing = {
                let list = self.devices.lock();
                list.iter().find(|existing| existing.id == dev.id).cloned()
            };
            if let Some(existing) = existing {
                if existing.state() != PnpState::Gone {
                    return Ok(PnpDeviceRegistration {
                        device: existing,
                        inserted: false,
                    });
                }
                // Gone 是终态；重新取得列表锁后只移除刚才观察到的对象，随后重新
                // 检查一次，保证并发扫描不会覆盖另一个线程刚插入的同身份设备。
                let removed = {
                    let mut list = self.devices.lock();
                    list.iter()
                        .position(|item| Arc::ptr_eq(item, &existing))
                        .map(|index| list.swap_remove(index))
                };
                if let Some(removed) = removed {
                    publish_device_event(PnpDeviceEventKind::Removed, removed);
                }
                continue;
            }

            let mut list = self.devices.lock();
            if list.iter().any(|existing| existing.id == dev.id) {
                drop(list);
                continue;
            }
            list.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
            list.push(Arc::clone(&dev));
            drop(list);
            publish_device_event(PnpDeviceEventKind::Registered, Arc::clone(&dev));
            if super::elm_lifecycle::track_device(Arc::clone(&dev)).is_err() {
                log::error!("[pnp] 无法登记设备资源归属: {}", dev.id);
                let _ = self.remove_exact(&dev);
                return Err(PnpError::OutOfMemory);
            }
            return Ok(PnpDeviceRegistration {
                device: dev,
                inserted: true,
            });
        }
    }

    /// 只移除与给定对象指针完全相同的设备。
    ///
    /// 热拔末段允许同一硬件身份被重新枚举；旧对象不能仅凭 `PnpId` 删除已经替换它的
    /// 新对象，因此设备生命周期和注册回滚路径必须使用本入口。
    pub fn remove_exact(&self, device: &Arc<PnpDevice>) -> Option<Arc<PnpDevice>> {
        let removed = {
            let mut list = self.devices.lock();
            let pos = list
                .iter()
                .position(|existing| Arc::ptr_eq(existing, device))?;
            list.swap_remove(pos)
        };
        publish_device_event(PnpDeviceEventKind::Removed, Arc::clone(&removed));
        super::elm_lifecycle::forget_device(&removed);
        Some(removed)
    }

    /// 按 PnP 硬件身份查找设备。
    pub fn lookup(&self, id: &PnpId) -> Option<Arc<PnpDevice>> {
        let device = {
            let list = self.devices.lock();
            list.iter().find(|device| device.id == *id).cloned()
        }?;
        (device.state() != PnpState::Gone).then_some(device)
    }

    /// 返回所有尚未 Gone 的设备快照。
    pub fn try_list(&self) -> Option<Vec<Arc<PnpDevice>>> {
        let devices = {
            let list = self.devices.lock();
            let mut devices = Vec::new();
            devices.try_reserve(list.len()).ok()?;
            devices.extend(list.iter().cloned());
            devices
        };
        let mut out = Vec::new();
        out.try_reserve(devices.len()).ok()?;
        out.extend(
            devices
                .into_iter()
                .filter(|device| device.state() != PnpState::Gone),
        );
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

/// 为外部同名 Rust 门面构造真实 PnP 设备对象。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDevice.new",
    contract = "kernel.general.pnp-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn new(
    id: PnpId,
    name: Box<str>,
    info: Box<dyn PnpBusInfo>,
) -> Result<Arc<PnpDevice>, PnpError> {
    PnpDevice::new(id, name, info)
}

/// 调用全局 PnP 设备表的 `get_or_insert`。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDeviceList.get_or_insert",
    contract = "kernel.general.pnp-registry@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn get_or_insert(
    devices: &PnpDeviceList,
    device: Arc<PnpDevice>,
) -> Result<PnpDeviceRegistration, PnpError> {
    devices.get_or_insert(device)
}

/// 调用全局 PnP 设备表的精确移除入口。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDeviceList.remove_exact",
    contract = "kernel.general.pnp-registry@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn remove_exact(devices: &PnpDeviceList, device: &Arc<PnpDevice>) -> Option<Arc<PnpDevice>> {
    devices.remove_exact(device)
}

/// 调用全局 PnP 设备表的身份查询入口。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDeviceList.lookup",
    contract = "kernel.general.pnp-registry@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
)]
pub fn lookup(devices: &PnpDeviceList, id: &PnpId) -> Option<Arc<PnpDevice>> {
    devices.lookup(id)
}

/// 返回全局 PnP 设备表的一致快照。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDeviceList.list",
    contract = "kernel.general.pnp-registry@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
)]
pub fn list(devices: &PnpDeviceList) -> Vec<Arc<PnpDevice>> {
    devices.list()
}

/// 让全局 PnP 驱动表为一个设备执行匹配和 probe。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDriverRegistry.probe_device",
    contract = "kernel.general.pnp-driver@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn probe_device(drivers: &PnpDriverRegistry, device: &Arc<PnpDevice>) -> Result<(), PnpError> {
    drivers.probe_device(device)
}

/// 重试全部 deferred PnP 设备。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDriverRegistry.retry_deferred_devices",
    contract = "kernel.general.pnp-driver@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn retry_deferred_devices(drivers: &PnpDriverRegistry) -> Result<usize, PnpError> {
    drivers.retry_deferred_devices()
}

// ── 功能注册 helpers ────────────────────────────────────────────────────

impl PnpDevice {
    /// 事务式注册开放设备 function。
    ///
    /// 这里的事务边界只覆盖 PnP 设备与全局 function registry。`/dev`、`/sys`
    /// 等用户态视图通过 function 生命周期事件自行投影，不能反向决定底层设备
    /// 是否 probe 成功。
    pub fn register_function(
        self: &Arc<Self>,
        func: Arc<dyn DeviceFunction>,
    ) -> Result<(), PnpError> {
        let func = self.prepare_device_function(func)?;
        self.attach_prepared_function(Arc::clone(&func))?;

        if let Err(e) = DEVICES.register_function(Arc::clone(&func)) {
            self.detach_function(&func);
            func.mark_gone();
            return Err(e.into());
        }

        Ok(())
    }

    /// 在设备仍处于 probe 或 bound 状态时主动注销一个 function。
    pub fn unregister_function(
        self: &Arc<Self>,
        class_id: crate::dev::function::DeviceClassId,
        dev_name: &str,
    ) -> Result<(), PnpError> {
        let function = {
            let mut inner = self.inner.lock();
            if !matches!(inner.state, PnpState::Probing | PnpState::Bound) {
                return Err(PnpError::InvalidState);
            }
            let Some(index) = inner.functions.iter().position(|function| {
                function.class_id() == class_id && function.dev_name() == dev_name
            }) else {
                return Err(PnpError::registration_failed(
                    PnpResourceKind::Function,
                    "function not attached",
                ));
            };
            inner.functions.remove(index)
        };
        function.mark_gone();
        function.drain_io();
        self.unregister_function_external(&function);
        Ok(())
    }

    fn detach_function(&self, func: &Arc<dyn DeviceFunction>) {
        let mut inner = self.inner.lock();
        inner
            .functions
            .retain(|existing| !Arc::ptr_eq(existing, func));
    }

    fn unregister_function_external(&self, func: &Arc<dyn DeviceFunction>) {
        DEVICES.unregister_quiesced_function(func);
        super::elm_lifecycle::forget_device_function(self, func.class_id(), func.dev_name());
    }

    fn rollback_probe_side_effects(self: &Arc<Self>, driver: &Arc<dyn PnpDriver>) {
        let (functions, children, resources, driver_data) = {
            let mut inner = self.inner.lock();
            inner.bound_driver = None;
            inner.driver_owner = None;
            let driver_data = inner.driver_data.take();
            let functions = core::mem::take(&mut inner.functions);
            let children = core::mem::take(&mut inner.children);
            let resources = core::mem::take(&mut inner.resources);
            if inner.state == PnpState::Probing {
                inner.state = PnpState::Discovered;
            }
            (functions, children, resources, driver_data)
        };

        for child in children.iter().rev() {
            child.remove_device();
        }

        for func in &functions {
            func.mark_gone();
        }
        for func in &functions {
            func.drain_io();
        }
        for func in &functions {
            self.unregister_function_external(func);
        }

        if let Some(driver_data) = driver_data
            && let Err(error) = driver.release_driver_data(driver_data)
        {
            log::error!(
                "[pnp] failed to release driver data while rolling back {}: {:?}",
                self.id,
                error
            );
        }

        let _ = release_pnp_resources(resources, &self.id);
    }

    fn commit_prepared_action(
        self: &Arc<Self>,
        action: &PreparedDeviceAction,
    ) -> Result<(), PnpError> {
        if !self.removal_lock.load(Ordering::Acquire) {
            return Err(PnpError::InvalidState);
        }

        let (bound_driver, functions, resources, bus_resources) = {
            let mut inner = self.inner.lock();
            if !inner.children.is_empty() {
                return Err(PnpError::InvalidState);
            }
            match action {
                PreparedDeviceAction::Remove => {
                    if !matches!(inner.state, PnpState::Discovered | PnpState::Bound) {
                        return Err(PnpError::InvalidState);
                    }
                }
                PreparedDeviceAction::Unbind(expected) => {
                    if inner.state != PnpState::Bound
                        || !inner
                            .bound_driver
                            .as_ref()
                            .is_some_and(|bound| Arc::ptr_eq(bound, expected))
                    {
                        return Err(PnpError::InvalidState);
                    }
                }
            }
            inner.state = PnpState::Removing;
            let bus_resources = if matches!(action, PreparedDeviceAction::Remove) {
                core::mem::take(&mut inner.bus_resources)
            } else {
                Vec::new()
            };
            (
                inner.bound_driver.take(),
                core::mem::take(&mut inner.functions),
                core::mem::take(&mut inner.resources),
                bus_resources,
            )
        };

        for function in &functions {
            function.mark_gone();
        }
        for function in &functions {
            function.drain_io();
        }
        if let Some(driver) = bound_driver.as_ref() {
            driver.try_remove(self)?;
        }
        // remove 回调必须仍能通过 take_driver_data() 取得私有状态来静默硬件。
        // 回调没有消费的数据再由 core 在所属 ELM context 下兜底析构。
        let driver_data = self.inner.lock().driver_data.take();
        // DMA/IOMMU consumer context 等必须先于 provider registration 析构。
        if let Some(driver_data) = driver_data {
            match bound_driver.as_ref() {
                Some(driver) => driver.release_driver_data(driver_data)?,
                None => drop(driver_data),
            }
        }
        release_pnp_resources(resources, &self.id)?;
        for function in &functions {
            self.unregister_function_external(function);
        }
        drop(functions);
        // 总线资源跨普通驱动 unbind 保留；设备 Gone 时在驱动资源之后释放，确保
        // function、MSI doorbell、DMA buffer 等下游映射已经撤销，再解除 IOMMU
        // consumer lease。
        release_pnp_resources(bus_resources, &self.id)?;

        match action {
            PreparedDeviceAction::Remove => {
                {
                    let mut inner = self.inner.lock();
                    inner.driver_owner = None;
                    inner.deferred_dependency = None;
                    inner.state = PnpState::Gone;
                }
                PNP_DEVICES.remove_exact(self);
                if let Some(parent) = self.parent() {
                    parent.detach_child(self);
                }
            }
            PreparedDeviceAction::Unbind(_) => {
                let mut inner = self.inner.lock();
                inner.driver_owner = None;
                inner.deferred_dependency = None;
                inner.state = PnpState::Discovered;
                drop(inner);
                self.removal_lock.store(false, Ordering::Release);
            }
        }
        Ok(())
    }
}

/// 在真实 PnP 对象之间建立父子关系。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDevice.attach_child",
    contract = "kernel.general.pnp-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn attach_child(parent: &Arc<PnpDevice>, child: &Arc<PnpDevice>) -> Result<(), PnpError> {
    parent.attach_child(child)
}

/// 从真实 PnP 对象中解除父子关系。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDevice.detach_child",
    contract = "kernel.general.pnp-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn detach_child(parent: &Arc<PnpDevice>, child: &Arc<PnpDevice>) {
    parent.detach_child(child);
}

/// 在真实 PnP 对象上事务式注册一个设备 function。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDevice.register_function",
    contract = "kernel.general.pnp-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn register_function(
    device: &Arc<PnpDevice>,
    function: Arc<dyn DeviceFunction>,
) -> Result<(), PnpError> {
    // 在读取可能来自动态 vtable 的身份字段前，先确认调用者仍是该设备当前绑定的
    // 精确 cell/generation。后续 `register_function` 会在真正发布前再次校验。
    device.function_registration_context()?;
    let class_id = function.class_id();
    let mut owned_name = String::new();
    {
        let name = function.dev_name();
        owned_name
            .try_reserve(name.len())
            .map_err(|_| PnpError::OutOfMemory)?;
        owned_name.push_str(name);
    }
    let mut tracked_name = String::new();
    tracked_name
        .try_reserve(owned_name.len())
        .map_err(|_| PnpError::OutOfMemory)?;
    tracked_name.push_str(&owned_name);
    device.register_function(function)?;
    if super::elm_lifecycle::track_device_function(
        Arc::clone(device),
        class_id,
        tracked_name.into_boxed_str(),
    )
    .is_err()
    {
        let _ = device.unregister_function(class_id, &owned_name);
        return Err(PnpError::OutOfMemory);
    }
    Ok(())
}

/// 在真实 PnP 对象上注销一个设备 function。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDevice.unregister_function",
    contract = "kernel.general.pnp-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_function(
    device: &Arc<PnpDevice>,
    class_id: crate::dev::function::DeviceClassId,
    name: &str,
) -> Result<(), PnpError> {
    device.unregister_function(class_id, name)
}

/// 执行真实 PnP 设备的完整热拔流程。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDevice.remove_device",
    contract = "kernel.general.pnp-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn remove_device(device: &Arc<PnpDevice>) {
    device.remove_device();
}

/// 在真实 PnP 设备上执行带资源预检的热移除。
#[doc(hidden)]
#[kernel_symbols::export(
    name = "general.dev.pnp.PnpDevice.try_remove_device",
    contract = "kernel.general.pnp-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn direct_pnp_try_remove_device(device: &Arc<PnpDevice>) -> Result<(), PnpError> {
    device.try_remove_device()
}

// ── remove_device：热拔移除流程 ─────────────────────────────────────────

impl PnpDevice {
    /// 验证当前子树可以进入移除事务；该兼容入口不会跨调用保留冻结状态。
    /// 真正的热拔必须使用 [`PnpRemovalTransaction`] 或 [`Self::try_remove_device`]。
    pub fn preflight_remove(self: &Arc<Self>) -> Result<(), PnpError> {
        if self.state() == PnpState::Gone {
            return Ok(());
        }
        drop(PnpRemovalTransaction::prepare(core::slice::from_ref(self))?);
        Ok(())
    }

    /// prepare 完整子树后一次性提交热移除。
    pub fn try_remove_device(self: &Arc<Self>) -> Result<(), PnpError> {
        if self.state() == PnpState::Gone {
            return Ok(());
        }
        PnpRemovalTransaction::prepare(core::slice::from_ref(self))?.commit()
    }

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
        if let Err(error) = self.try_remove_device() {
            log::error!("[pnp] cannot remove {} safely: {:?}", self.id, error);
        }
    }
}

// ── 全局单例 ─────────────────────────────────────────────────────────────

#[kernel_symbols::export(
    name = "general.dev.pnp.PNP_DEVICES",
    contract = "kernel.general.pnp-registry@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
        | kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub static PNP_DEVICES: PnpDeviceList = PnpDeviceList::new();
#[kernel_symbols::export(
    name = "general.dev.pnp.PNP_DRIVERS",
    contract = "kernel.general.pnp-registry@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub static PNP_DRIVERS: PnpDriverRegistry = PnpDriverRegistry::new();

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::any::Any;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static TEST_ELM_CONTEXT_STATE: Spinlock<()> = Spinlock::new(());

    #[derive(Debug)]
    struct TestBusInfo;

    impl PnpBusInfo for TestBusInfo {
        fn bus_type(&self) -> BusType {
            BusType::GENERIC
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct TestPrepareResource {
        busy: bool,
        state: Arc<AtomicUsize>,
        cancels: Arc<AtomicUsize>,
    }

    struct TestDriver {
        removes: Arc<AtomicUsize>,
    }

    struct DriverDataTakingDriver {
        took_data: Arc<AtomicUsize>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct ContextObservation {
        operation: &'static str,
        context: elm_model::ElmCurrentContext,
    }

    struct ContextRecordingDriver {
        observations: Arc<Spinlock<Vec<ContextObservation>>>,
    }

    impl ContextRecordingDriver {
        fn record(&self, operation: &'static str) {
            self.observations.lock().push(ContextObservation {
                operation,
                context: elm_model::current_context()
                    .expect("ELM driver callback must have a current context"),
            });
        }
    }

    impl PnpDriver for ContextRecordingDriver {
        fn name(&self) -> &str {
            "elm-context-recording-driver"
        }

        fn bus_type(&self) -> BusType {
            BusType::GENERIC
        }

        fn matches(&self, _id: &PnpId, _info: &dyn PnpBusInfo) -> bool {
            self.record("matches");
            true
        }

        fn matches_device(&self, _device: &Arc<PnpDevice>) -> bool {
            self.record("matches_device");
            true
        }

        fn probe(&self, _dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
            self.record("probe");
            Ok(())
        }

        fn remove(&self, _dev: &Arc<PnpDevice>) {
            self.record("remove");
        }

        fn prepare_remove(&self) -> Result<(), PnpError> {
            self.record("prepare_remove");
            Ok(())
        }

        fn try_remove(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
            self.record("try_remove");
            self.remove(dev);
            Ok(())
        }
    }

    impl Drop for ContextRecordingDriver {
        fn drop(&mut self) {
            self.record("drop");
        }
    }

    struct ContextRecordingResource {
        observations: Arc<Spinlock<Vec<ContextObservation>>>,
        identity: u64,
    }

    struct ContextRecordingFunction {
        observations: Arc<Spinlock<Vec<ContextObservation>>>,
    }

    impl ContextRecordingFunction {
        fn record(&self, operation: &'static str) {
            self.observations.lock().push(ContextObservation {
                operation,
                context: elm_model::current_context()
                    .expect("ELM function callback must have a current context"),
            });
        }
    }

    impl DeviceFunction for ContextRecordingFunction {
        fn class_id(&self) -> crate::dev::function::DeviceClassId {
            self.record("class_id");
            crate::dev::function::DeviceClassId::new("elm-context-function")
        }

        fn dev_name(&self) -> &str {
            self.record("dev_name");
            "elm-context-function0"
        }

        fn class_name(&self) -> &str {
            self.record("class_name");
            "elm-context-function"
        }

        fn operation_contract(&self) -> Option<&str> {
            self.record("operation_contract");
            Some("test.elm-context-function@1")
        }

        fn invoke(
            &self,
            _opcode: u32,
            input: &[u8],
            output: &mut [u8],
        ) -> Result<usize, crate::dev::function::DeviceFunctionInvokeError> {
            self.record("invoke");
            let len = input.len().min(output.len());
            output[..len].copy_from_slice(&input[..len]);
            Ok(len)
        }

        fn mark_gone(&self) {
            self.record("mark_gone");
        }

        fn drain_io(&self) {
            self.record("drain_io");
        }

        fn as_any(&self) -> &dyn Any {
            self.record("as_any");
            self
        }
    }

    impl Drop for ContextRecordingFunction {
        fn drop(&mut self) {
            self.record("drop");
        }
    }

    impl ContextRecordingResource {
        fn record(&self, operation: &'static str) {
            self.observations.lock().push(ContextObservation {
                operation,
                context: elm_model::current_context()
                    .expect("ELM resource callback must have a current context"),
            });
        }
    }

    impl PnpResource for ContextRecordingResource {
        fn kind(&self) -> PnpResourceKind {
            PnpResourceKind::Other("elm-context-resource")
        }

        fn label(&self) -> &'static str {
            "elm-context-resource"
        }

        fn identity(&self) -> Option<u64> {
            Some(self.identity)
        }

        fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
            self.record("prepare_release");
            Ok(())
        }

        fn cancel_release(&self) {
            self.record("cancel_release");
        }

        fn consumes_dependency(&self, _dependency: PnpDependency) -> bool {
            self.record("consumes_dependency");
            true
        }

        fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
            self.record("release");
            Ok(())
        }
    }

    impl Drop for ContextRecordingResource {
        fn drop(&mut self) {
            self.record("drop");
        }
    }

    impl PnpDriver for TestDriver {
        fn name(&self) -> &str {
            "pnp-transaction-test"
        }

        fn bus_type(&self) -> BusType {
            BusType::GENERIC
        }

        fn matches(&self, _id: &PnpId, _info: &dyn PnpBusInfo) -> bool {
            true
        }

        fn probe(&self, _dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
            Ok(())
        }

        fn remove(&self, _dev: &Arc<PnpDevice>) {
            self.removes.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl PnpDriver for DriverDataTakingDriver {
        fn name(&self) -> &str {
            "pnp-driver-data-taking-test"
        }

        fn bus_type(&self) -> BusType {
            BusType::GENERIC
        }

        fn matches(&self, _id: &PnpId, _info: &dyn PnpBusInfo) -> bool {
            true
        }

        fn probe(&self, _dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
            Ok(())
        }

        fn remove(&self, dev: &Arc<PnpDevice>) {
            if dev.take_driver_data().is_some() {
                self.took_data.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    impl PnpResource for TestPrepareResource {
        fn kind(&self) -> PnpResourceKind {
            PnpResourceKind::Other("test-prepare")
        }

        fn label(&self) -> &'static str {
            "test-prepare"
        }

        fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
            if self.busy {
                return Err(PnpResourceReleaseError::new(
                    self.kind(),
                    self.label(),
                    "test resource is busy",
                ));
            }
            self.state.store(1, Ordering::Release);
            Ok(())
        }

        fn cancel_release(&self) {
            if self.state.swap(0, Ordering::AcqRel) == 1 {
                self.cancels.fetch_add(1, Ordering::Relaxed);
            }
        }

        fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
            self.state.store(2, Ordering::Release);
            Ok(())
        }
    }

    fn test_device(fingerprint: u64) -> Arc<PnpDevice> {
        PnpDevice::new(
            PnpId::Dynamic {
                fingerprint,
                bus: BusType::GENERIC,
                contract: "test@1".into(),
                identity: fingerprint.to_ne_bytes().into(),
            },
            alloc::format!("test-{fingerprint}").into_boxed_str(),
            Box::new(TestBusInfo),
        )
        .unwrap()
    }

    fn test_elm_context(
        cell: u64,
        generation: u64,
        phase: elm_model::ElmLifecyclePhase,
        flags: u32,
        allowed_actions: u32,
    ) -> elm_model::ElmContext {
        elm_model::ElmContext::new(
            elm_model::ElmId(cell),
            Some(elm_model::ElmId(cell + 1000)),
            elm_model::Generation(generation),
            elm_model::ElmState::Active,
            phase,
            flags,
        )
        .with_kind(elm_model::ElmKind::Driver)
        .with_allowed_actions(allowed_actions)
    }

    #[test]
    fn remove_callback_can_take_driver_data_before_core_fallback_drop() {
        let device = test_device(0x40f1);
        let took_data = Arc::new(AtomicUsize::new(0));
        let driver: Arc<dyn PnpDriver> = Arc::new(DriverDataTakingDriver {
            took_data: Arc::clone(&took_data),
        });
        {
            let mut inner = device.inner.lock();
            inner.state = PnpState::Bound;
            inner.bound_driver = Some(driver);
            inner.driver_data = Some(Arc::new(0x40f1_u64));
        }

        device.try_remove_device().unwrap();

        assert_eq!(took_data.load(Ordering::Relaxed), 1);
        assert!(device.inner.lock().driver_data.is_none());
        assert_eq!(device.state(), PnpState::Gone);
    }

    #[test]
    fn elm_driver_proxy_restores_complete_context_and_generation() {
        let _context_state = TEST_ELM_CONTEXT_STATE.lock();
        assert!(elm_model::current_context().is_none());
        let owner = test_elm_context(
            0x4101,
            7,
            elm_model::ElmLifecyclePhase::Initialize,
            0x55aa,
            0x12d,
        );
        let owner_snapshot = elm_model::ElmCurrentContext::from_context(&owner);
        let observations = Arc::new(Spinlock::new(Vec::new()));
        let proxy = {
            let _owner_guard = elm_model::enter_current_context(&owner).unwrap();
            ElmPnpDriverProxy::wrap(
                Arc::new(ContextRecordingDriver {
                    observations: Arc::clone(&observations),
                }),
                owner_snapshot,
            )
            .unwrap()
        };
        assert!(elm_model::current_context().is_none());

        let outer = test_elm_context(0x4101, 8, elm_model::ElmLifecyclePhase::Resume, 0x77, 0x3);
        let outer_snapshot = elm_model::ElmCurrentContext::from_context(&outer);
        let device = test_device(0x4101);
        {
            let _outer_guard = elm_model::enter_current_context(&outer).unwrap();
            assert!(proxy.matches(&device.id, device.info.as_ref()));
            assert_eq!(elm_model::current_context(), Some(outer_snapshot));
            assert!(proxy.matches_device(&device));
            assert_eq!(elm_model::current_context(), Some(outer_snapshot));
            proxy.probe(&device).unwrap();
            assert_eq!(elm_model::current_context(), Some(outer_snapshot));
            proxy.prepare_remove().unwrap();
            assert_eq!(elm_model::current_context(), Some(outer_snapshot));
            proxy.remove(&device);
            assert_eq!(elm_model::current_context(), Some(outer_snapshot));
        }
        drop(proxy);
        assert!(elm_model::current_context().is_none());

        let observations = observations.lock();
        assert_eq!(
            observations
                .iter()
                .map(|observation| observation.operation)
                .collect::<Vec<_>>(),
            [
                "matches",
                "matches_device",
                "probe",
                "prepare_remove",
                "try_remove",
                "remove",
                "drop"
            ]
        );
        assert!(
            observations
                .iter()
                .all(|observation| observation.context == owner_snapshot)
        );
    }

    #[test]
    fn elm_driver_proxy_keeps_owner_context_through_unregister() {
        let _context_state = TEST_ELM_CONTEXT_STATE.lock();
        assert!(elm_model::current_context().is_none());
        let owner = test_elm_context(
            0x4151,
            21,
            elm_model::ElmLifecyclePhase::Initialize,
            0x1234,
            0x17,
        );
        let owner_snapshot = elm_model::ElmCurrentContext::from_context(&owner);
        let observations = Arc::new(Spinlock::new(Vec::new()));
        let driver = {
            let _owner_guard = elm_model::enter_current_context(&owner).unwrap();
            ElmPnpDriverProxy::wrap(
                Arc::new(ContextRecordingDriver {
                    observations: Arc::clone(&observations),
                }),
                owner_snapshot,
            )
            .unwrap()
        };
        let registry = PnpDriverRegistry::new();
        let driver_id = DriverId(0x7fff_4151);
        registry.drivers.lock().push(RegisteredDriver {
            id: driver_id,
            driver: Arc::clone(&driver),
            owner: Some(ElmPnpOwner::from_context(owner_snapshot)),
            accepting: AtomicBool::new(true),
        });
        let device = test_device(0x4151);
        {
            let mut inner = device.inner.lock();
            inner.state = PnpState::Bound;
            inner.bound_driver = Some(Arc::clone(&driver));
            inner.driver_owner = Some(ElmPnpOwner::from_context(owner_snapshot));
        }
        let resource_observations = Arc::new(Spinlock::new(Vec::new()));
        {
            let _owner_guard = elm_model::enter_current_context(&owner).unwrap();
            device
                .own_resource(ContextRecordingResource {
                    observations: Arc::clone(&resource_observations),
                    identity: 0x4151,
                })
                .unwrap();
        }
        PNP_DEVICES.get_or_insert(Arc::clone(&device)).unwrap();

        let outer = test_elm_context(0x4151, 22, elm_model::ElmLifecyclePhase::Quiesce, 0, 1);
        let outer_snapshot = elm_model::ElmCurrentContext::from_context(&outer);
        {
            let _outer_guard = elm_model::enter_current_context(&outer).unwrap();
            registry.unregister(DriverHandle { id: driver_id }).unwrap();
            assert_eq!(elm_model::current_context(), Some(outer_snapshot));
        }
        assert_eq!(device.state(), PnpState::Discovered);
        assert!(device.inner.lock().driver_owner.is_none());
        PNP_DEVICES.remove_exact(&device);
        drop(driver);

        let observations = observations.lock();
        assert_eq!(
            observations
                .iter()
                .map(|observation| observation.operation)
                .collect::<Vec<_>>(),
            ["prepare_remove", "try_remove", "remove", "drop"]
        );
        assert!(
            observations
                .iter()
                .all(|observation| observation.context == owner_snapshot)
        );
        let resource_observations = resource_observations.lock();
        assert_eq!(
            resource_observations
                .iter()
                .map(|observation| observation.operation)
                .collect::<Vec<_>>(),
            ["prepare_release", "release", "drop"]
        );
        assert!(
            resource_observations
                .iter()
                .all(|observation| observation.context == owner_snapshot)
        );
        assert!(elm_model::current_context().is_none());
    }

    #[test]
    fn elm_device_function_proxy_restores_context_and_hides_dynamic_any() {
        let _context_state = TEST_ELM_CONTEXT_STATE.lock();
        assert!(elm_model::current_context().is_none());
        let owner = test_elm_context(
            0x4181,
            31,
            elm_model::ElmLifecyclePhase::Initialize,
            0x5a5a,
            0x1d,
        );
        let owner_snapshot = elm_model::ElmCurrentContext::from_context(&owner);
        let observations = Arc::new(Spinlock::new(Vec::new()));
        let proxy = {
            let _owner_guard = elm_model::enter_current_context(&owner).unwrap();
            ElmDeviceFunctionProxy::wrap(
                Arc::new(ContextRecordingFunction {
                    observations: Arc::clone(&observations),
                }),
                owner_snapshot,
            )
            .map_err(|(error, _function)| error)
            .unwrap()
        };
        observations.lock().clear();

        let outer = test_elm_context(0x4181, 32, elm_model::ElmLifecyclePhase::Resume, 0x77, 0x3);
        let outer_snapshot = elm_model::ElmCurrentContext::from_context(&outer);
        {
            let _outer_guard = elm_model::enter_current_context(&outer).unwrap();
            assert_eq!(
                proxy.class_id(),
                crate::dev::function::DeviceClassId::new("elm-context-function")
            );
            assert_eq!(proxy.class_name(), "elm-context-function");
            assert_eq!(proxy.dev_name(), "elm-context-function0");
            assert_eq!(
                proxy.operation_contract(),
                Some("test.elm-context-function@1")
            );
            assert!(proxy.as_any().is::<ElmDeviceFunctionProxy>());
            let mut output = [0u8; 4];
            assert_eq!(proxy.invoke(7, b"abc", &mut output), Ok(3));
            assert_eq!(&output[..3], b"abc");
            proxy.mark_gone();
            assert_eq!(
                proxy.invoke(8, b"stale", &mut output),
                Err(crate::dev::function::DeviceFunctionInvokeError::Gone)
            );
            proxy.drain_io();
            assert_eq!(elm_model::current_context(), Some(outer_snapshot));
        }
        drop(proxy);
        assert!(elm_model::current_context().is_none());

        let observations = observations.lock();
        assert_eq!(
            observations
                .iter()
                .map(|observation| observation.operation)
                .collect::<Vec<_>>(),
            ["invoke", "mark_gone", "drain_io", "drop"]
        );
        assert!(
            observations
                .iter()
                .all(|observation| observation.context == owner_snapshot)
        );
    }

    #[test]
    fn elm_device_function_proxy_preserves_resident_typed_projection() {
        let _context_state = TEST_ELM_CONTEXT_STATE.lock();
        assert!(elm_model::current_context().is_none());
        let owner = test_elm_context(0x4191, 41, elm_model::ElmLifecyclePhase::Initialize, 0, 1);
        let owner_snapshot = elm_model::ElmCurrentContext::from_context(&owner);
        let proxy = {
            let _owner_guard = elm_model::enter_current_context(&owner).unwrap();
            let function: Arc<dyn DeviceFunction> =
                Arc::new(crate::dev::function::CharFunction::new(
                    "elm-resident-char",
                    crate::dev::char::CharDevice::null(),
                ));
            ElmDeviceFunctionProxy::wrap(function, owner_snapshot)
                .map_err(|(error, _function)| error)
                .unwrap()
        };

        let outer = test_elm_context(0x4191, 42, elm_model::ElmLifecyclePhase::Resume, 0, 1);
        {
            let _outer_guard = elm_model::enter_current_context(&outer).unwrap();
            assert!(
                crate::dev::function::function_as::<crate::dev::function::CharFunction>(
                    proxy.as_ref()
                )
                .is_some()
            );
        }
        drop(proxy);
        assert!(elm_model::current_context().is_none());
    }

    #[test]
    fn dynamic_device_registration_installs_and_releases_function_proxy() {
        let _context_state = TEST_ELM_CONTEXT_STATE.lock();
        assert!(elm_model::current_context().is_none());
        let owner = test_elm_context(
            0x41a1,
            51,
            elm_model::ElmLifecyclePhase::Initialize,
            0x123,
            0x7,
        );
        let owner_snapshot = elm_model::ElmCurrentContext::from_context(&owner);
        let device = test_device(0x41a1);
        {
            let mut inner = device.inner.lock();
            inner.state = PnpState::Probing;
            inner.driver_owner = Some(ElmPnpOwner::from_context(owner_snapshot));
        }
        let observations = Arc::new(Spinlock::new(Vec::new()));
        {
            let _owner_guard = elm_model::enter_current_context(&owner).unwrap();
            device
                .register_function(Arc::new(ContextRecordingFunction {
                    observations: Arc::clone(&observations),
                }))
                .unwrap();
        }
        assert!(
            device.inner.lock().functions[0]
                .as_any()
                .is::<ElmDeviceFunctionProxy>()
        );
        observations.lock().clear();

        let outer = test_elm_context(0x41a1, 52, elm_model::ElmLifecyclePhase::Quiesce, 0, 1);
        let outer_snapshot = elm_model::ElmCurrentContext::from_context(&outer);
        {
            let _outer_guard = elm_model::enter_current_context(&outer).unwrap();
            device
                .unregister_function(
                    crate::dev::function::DeviceClassId::new("elm-context-function"),
                    "elm-context-function0",
                )
                .unwrap();
            assert_eq!(elm_model::current_context(), Some(outer_snapshot));
        }
        assert!(elm_model::current_context().is_none());
        assert_eq!(device.function_count(), 0);

        let observations = observations.lock();
        assert_eq!(
            observations
                .iter()
                .map(|observation| observation.operation)
                .collect::<Vec<_>>(),
            ["mark_gone", "drain_io", "drop"]
        );
        assert!(
            observations
                .iter()
                .all(|observation| observation.context == owner_snapshot)
        );
    }

    #[test]
    fn elm_resource_proxy_restores_context_for_full_release_protocol() {
        let _context_state = TEST_ELM_CONTEXT_STATE.lock();
        assert!(elm_model::current_context().is_none());
        let owner = test_elm_context(
            0x4201,
            11,
            elm_model::ElmLifecyclePhase::Initialize,
            0xa5,
            0x19,
        );
        let owner_snapshot = elm_model::ElmCurrentContext::from_context(&owner);
        let device = test_device(0x4201);
        {
            let mut inner = device.inner.lock();
            inner.state = PnpState::Bound;
            inner.driver_owner = Some(ElmPnpOwner::from_context(owner_snapshot));
        }
        let observations = Arc::new(Spinlock::new(Vec::new()));
        {
            let _owner_guard = elm_model::enter_current_context(&owner).unwrap();
            device
                .own_resource(ContextRecordingResource {
                    observations: Arc::clone(&observations),
                    identity: 0x4201,
                })
                .unwrap();
        }

        let outer = test_elm_context(0x4201, 12, elm_model::ElmLifecyclePhase::Resume, 0, 1);
        let outer_snapshot = elm_model::ElmCurrentContext::from_context(&outer);
        {
            let _outer_guard = elm_model::enter_current_context(&outer).unwrap();
            {
                let inner = device.inner.lock();
                let resource = inner.resources.first().unwrap();
                assert!(resource.consumes_dependency(PnpDependency::Other("test")));
                resource.prepare_release().unwrap();
                resource.cancel_release();
            }
            assert_eq!(elm_model::current_context(), Some(outer_snapshot));
            device.release_owned_resource(0x4201).unwrap();
            assert_eq!(elm_model::current_context(), Some(outer_snapshot));
        }
        assert!(elm_model::current_context().is_none());

        let observations = observations.lock();
        assert_eq!(
            observations
                .iter()
                .map(|observation| observation.operation)
                .collect::<Vec<_>>(),
            [
                "consumes_dependency",
                "prepare_release",
                "cancel_release",
                "release",
                "drop"
            ]
        );
        assert!(
            observations
                .iter()
                .all(|observation| observation.context == owner_snapshot)
        );
    }

    #[test]
    fn stale_generation_cannot_attach_or_reserve_bound_driver_resources() {
        let _context_state = TEST_ELM_CONTEXT_STATE.lock();
        assert!(elm_model::current_context().is_none());
        let owner = test_elm_context(0x4301, 3, elm_model::ElmLifecyclePhase::Initialize, 0, 1);
        let owner_snapshot = elm_model::ElmCurrentContext::from_context(&owner);
        let stale = test_elm_context(0x4301, 4, elm_model::ElmLifecyclePhase::Initialize, 0, 1);
        let device = test_device(0x4301);
        {
            let mut inner = device.inner.lock();
            inner.state = PnpState::Bound;
            inner.driver_owner = Some(ElmPnpOwner::from_context(owner_snapshot));
        }

        {
            let _stale_guard = elm_model::enter_current_context(&stale).unwrap();
            assert_eq!(
                device.reserve_owned_resources(2),
                Err(PnpError::InvalidState)
            );
            assert_eq!(
                device.own_resource(TestPrepareResource {
                    busy: false,
                    state: Arc::new(AtomicUsize::new(0)),
                    cancels: Arc::new(AtomicUsize::new(0)),
                }),
                Err(PnpError::InvalidState)
            );
            let function: Arc<dyn DeviceFunction> = Arc::new(ContextRecordingFunction {
                observations: Arc::new(Spinlock::new(Vec::new())),
            });
            assert!(matches!(
                device.prepare_device_function(function),
                Err(PnpError::InvalidState)
            ));
        }
        {
            let _owner_guard = elm_model::enter_current_context(&owner).unwrap();
            let old_capacity = device.inner.lock().resources.capacity();
            device.reserve_owned_resources(2).unwrap();
            assert!(device.inner.lock().resources.capacity() >= old_capacity.max(2));
        }
        assert!(elm_model::current_context().is_none());
    }

    #[test]
    fn builtin_provider_scope_authorizes_foreign_runtime_resource_registration() {
        let _context_state = TEST_ELM_CONTEXT_STATE.lock();
        assert!(elm_model::current_context().is_none());
        let device = test_device(0x4302);
        device.begin_probe(None).unwrap();
        let scope = device.provider_resource_scope().unwrap();
        device.inner.lock().state = PnpState::Bound;

        let foreign = test_elm_context(0x4302, 9, elm_model::ElmLifecyclePhase::Resume, 0, 1);
        let foreign_snapshot = elm_model::ElmCurrentContext::from_context(&foreign);
        let released = Arc::new(AtomicUsize::new(0));
        {
            let _foreign_guard = elm_model::enter_current_context(&foreign).unwrap();
            assert_eq!(
                device.reserve_owned_resources(1),
                Err(PnpError::InvalidState)
            );
            assert_eq!(
                device.own_resource(TestPrepareResource {
                    busy: false,
                    state: Arc::new(AtomicUsize::new(0)),
                    cancels: Arc::new(AtomicUsize::new(0)),
                }),
                Err(PnpError::InvalidState)
            );

            {
                let _provider_context = scope.enter_context().unwrap();
                assert!(elm_model::current_context().is_none());
                scope.reserve_owned_resources(1).unwrap();
                scope
                    .own_resource(TestPrepareResource {
                        busy: false,
                        state: Arc::clone(&released),
                        cancels: Arc::new(AtomicUsize::new(0)),
                    })
                    .unwrap();
                assert!(elm_model::current_context().is_none());
            }
            assert_eq!(elm_model::current_context(), Some(foreign_snapshot));
        }
        assert!(elm_model::current_context().is_none());
        assert_eq!(device.inner.lock().resources.len(), 1);
        let resources = {
            let mut inner = device.inner.lock();
            core::mem::take(&mut inner.resources)
        };
        release_pnp_resources(resources, &device.id).unwrap();
        assert_eq!(released.load(Ordering::Acquire), 2);
    }

    #[test]
    fn dynamic_provider_scope_wraps_resource_under_provider_context() {
        let _context_state = TEST_ELM_CONTEXT_STATE.lock();
        assert!(elm_model::current_context().is_none());
        let provider = test_elm_context(
            0x4303,
            17,
            elm_model::ElmLifecyclePhase::Initialize,
            0x5a,
            0x31,
        );
        let provider_snapshot = elm_model::ElmCurrentContext::from_context(&provider);
        let device = test_device(0x4303);
        let scope = {
            let _provider_guard = elm_model::enter_current_context(&provider).unwrap();
            device
                .begin_probe(Some(ElmPnpOwner::from_context(provider_snapshot)))
                .unwrap();
            let scope = device.provider_resource_scope().unwrap();
            device.inner.lock().state = PnpState::Bound;
            scope
        };

        let consumer = test_elm_context(0x5303, 4, elm_model::ElmLifecyclePhase::Resume, 0, 1);
        let consumer_snapshot = elm_model::ElmCurrentContext::from_context(&consumer);
        let observations = Arc::new(Spinlock::new(Vec::new()));
        {
            let _consumer_guard = elm_model::enter_current_context(&consumer).unwrap();
            {
                let _provider_context = scope.enter_context().unwrap();
                assert_eq!(elm_model::current_context(), Some(provider_snapshot));
                scope.reserve_owned_resources(1).unwrap();
                scope
                    .own_resource(ContextRecordingResource {
                        observations: Arc::clone(&observations),
                        identity: 0x4303,
                    })
                    .unwrap();
                assert_eq!(elm_model::current_context(), Some(provider_snapshot));
            }
            assert_eq!(elm_model::current_context(), Some(consumer_snapshot));
            {
                let inner = device.inner.lock();
                let resource = inner.resources.first().unwrap();
                assert!(resource.consumes_dependency(PnpDependency::Other("scope-test")));
                resource.prepare_release().unwrap();
                resource.cancel_release();
            }
            device.release_owned_resource(0x4303).unwrap();
            assert_eq!(elm_model::current_context(), Some(consumer_snapshot));
        }
        assert!(elm_model::current_context().is_none());

        let observations = observations.lock();
        assert_eq!(
            observations
                .iter()
                .map(|observation| observation.operation)
                .collect::<Vec<_>>(),
            [
                "consumes_dependency",
                "prepare_release",
                "cancel_release",
                "release",
                "drop"
            ]
        );
        assert!(
            observations
                .iter()
                .all(|observation| observation.context == provider_snapshot)
        );
    }

    #[test]
    fn provider_scope_rejects_stale_removing_and_retargeted_tokens() {
        let device = test_device(0x4304);
        device.begin_probe(None).unwrap();
        let stale = device.provider_resource_scope().unwrap();
        device.inner.lock().state = PnpState::Bound;

        device.inner.lock().state = PnpState::Discovered;
        device.begin_probe(None).unwrap();
        let current = device.provider_resource_scope().unwrap();
        device.inner.lock().state = PnpState::Bound;
        assert_eq!(
            stale.reserve_owned_resources(1),
            Err(PnpError::InvalidState)
        );
        assert!(matches!(stale.enter_context(), Err(PnpError::InvalidState)));
        assert_eq!(
            stale.own_resource(TestPrepareResource {
                busy: false,
                state: Arc::new(AtomicUsize::new(0)),
                cancels: Arc::new(AtomicUsize::new(0)),
            }),
            Err(PnpError::InvalidState)
        );

        let other = test_device(0x5304);
        let retargeted = PnpProviderResourceScope {
            device: Arc::downgrade(&other),
            runtime_id: current.runtime_id,
            binding_generation: current.binding_generation,
            owner: current.owner,
            context: current.context,
        };
        assert_eq!(
            retargeted.reserve_owned_resources(1),
            Err(PnpError::InvalidState)
        );
        assert!(matches!(
            retargeted.enter_context(),
            Err(PnpError::InvalidState)
        ));

        device.removal_lock.store(true, Ordering::Release);
        assert_eq!(
            current.reserve_owned_resources(1),
            Err(PnpError::InvalidState)
        );
        assert!(matches!(
            current.enter_context(),
            Err(PnpError::InvalidState)
        ));
        assert_eq!(
            current.own_resource(TestPrepareResource {
                busy: false,
                state: Arc::new(AtomicUsize::new(0)),
                cancels: Arc::new(AtomicUsize::new(0)),
            }),
            Err(PnpError::InvalidState)
        );
        device.removal_lock.store(false, Ordering::Release);
        current.reserve_owned_resources(0).unwrap();
        assert!(matches!(
            device.provider_resource_scope(),
            Err(PnpError::InvalidState)
        ));
    }

    #[test]
    fn busy_child_cancels_prepare_without_device_state_changes() {
        let parent = test_device(0x1001);
        let child = test_device(0x1002);
        parent.attach_child(&child).unwrap();

        let prepared_state = Arc::new(AtomicUsize::new(0));
        let cancels = Arc::new(AtomicUsize::new(0));
        {
            let mut inner = child.inner.lock();
            // prepare 按 LIFO 执行：先冻结第二条，再由第一条报告 Busy。
            inner.resources.push(Box::new(TestPrepareResource {
                busy: true,
                state: Arc::new(AtomicUsize::new(0)),
                cancels: Arc::new(AtomicUsize::new(0)),
            }));
            inner.resources.push(Box::new(TestPrepareResource {
                busy: false,
                state: Arc::clone(&prepared_state),
                cancels: Arc::clone(&cancels),
            }));
        }

        assert!(matches!(
            PnpRemovalTransaction::prepare(core::slice::from_ref(&parent)),
            Err(PnpError::ResourceBusy { .. })
        ));
        assert_eq!(parent.state(), PnpState::Discovered);
        assert_eq!(child.state(), PnpState::Discovered);
        assert!(!parent.removal_is_prepared());
        assert!(!child.removal_is_prepared());
        assert_eq!(prepared_state.load(Ordering::Acquire), 0);
        assert_eq!(cancels.load(Ordering::Relaxed), 1);
        assert!(
            parent
                .children()
                .iter()
                .any(|candidate| Arc::ptr_eq(candidate, &child))
        );
    }

    #[test]
    fn busy_driver_device_prevents_all_unbinds() {
        let registry = PnpDriverRegistry::new();
        let removes = Arc::new(AtomicUsize::new(0));
        let driver: Arc<dyn PnpDriver> = Arc::new(TestDriver {
            removes: Arc::clone(&removes),
        });
        let driver_id = DriverId(0x7fff_1001);
        registry.drivers.lock().push(RegisteredDriver {
            id: driver_id,
            driver: Arc::clone(&driver),
            owner: None,
            accepting: AtomicBool::new(true),
        });
        let handle = DriverHandle { id: driver_id };
        let first = test_device(0x2001);
        let busy = test_device(0x2002);
        for device in [&first, &busy] {
            let mut inner = device.inner.lock();
            inner.state = PnpState::Bound;
            inner.bound_driver = Some(Arc::clone(&driver));
        }
        first
            .inner
            .lock()
            .resources
            .push(Box::new(TestPrepareResource {
                busy: false,
                state: Arc::new(AtomicUsize::new(0)),
                cancels: Arc::new(AtomicUsize::new(0)),
            }));
        busy.inner
            .lock()
            .resources
            .push(Box::new(TestPrepareResource {
                busy: true,
                state: Arc::new(AtomicUsize::new(0)),
                cancels: Arc::new(AtomicUsize::new(0)),
            }));
        PNP_DEVICES.get_or_insert(Arc::clone(&first)).unwrap();
        PNP_DEVICES.get_or_insert(Arc::clone(&busy)).unwrap();

        assert!(matches!(
            registry.unregister(handle),
            Err(PnpError::ResourceBusy { .. })
        ));
        assert_eq!(first.state(), PnpState::Bound);
        assert_eq!(busy.state(), PnpState::Bound);
        assert_eq!(removes.load(Ordering::Relaxed), 0);
        assert!(registry.driver_is_accepting(&driver));

        for device in [&first, &busy] {
            let mut inner = device.inner.lock();
            inner.resources.clear();
            inner.bound_driver = None;
            inner.state = PnpState::Discovered;
            drop(inner);
            PNP_DEVICES.remove_exact(device);
        }
    }
}

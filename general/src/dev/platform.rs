//! 固件枚举的 platform 设备描述。
//!
//! DTB 与 ACPI 都会描述一类不挂在 PCI/USB 这类可枚举总线下的设备。启动路径
//! 将固件里的 compatible/HID、资源和少量属性转换成这里的中立结构，再交给 PnP
//! 注册表由 platform 驱动统一匹配和 probe。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use crate::dev::dma::{DmaBouncePolicy, DmaConstraints, DmaContext};
use crate::dev::irq::{self, IrqError, IrqHandle, IrqHandler, IrqLine};
use crate::dev::pnp::{
    BusType, PNP_DEVICES, PNP_DRIVERS, PlatformIdentity, PlatformIdentityIrqAttributes,
    PlatformIdentityIrqPolarity, PlatformIdentityIrqSharing, PlatformIdentityIrqTrigger,
    PlatformIdentityMatchId, PlatformIdentityResource, PnpBusInfo, PnpDevice, PnpError, PnpId,
    PnpState,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceMatchId {
    DtbCompatible(Box<str>),
    AcpiHid(Box<str>),
    AcpiCid(Box<str>),
}

impl DeviceMatchId {
    pub fn matches_str(&self, expected: &str) -> bool {
        match self {
            DeviceMatchId::DtbCompatible(id)
            | DeviceMatchId::AcpiHid(id)
            | DeviceMatchId::AcpiCid(id) => id.as_ref() == expected,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceResource {
    Mmio {
        phys: usize,
        size: usize,
    },
    /// 固件描述的中断 specifier。
    ///
    /// DTB `interrupts` cells 的长度和含义由对应 interrupt controller 的
    /// `#interrupt-cells` 决定。platform 层只保存 controller phandle 和原始
    /// cells，不猜测第几个 cell 是中断号或触发方式；后续 IRQ domain 接入后由
    /// 控制器驱动解释。
    Irq {
        controller: Option<u32>,
        cells: Box<[u32]>,
        attributes: IrqResourceAttributes,
    },
}

/// 固件 IRQ 触发方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqTrigger {
    Edge,
    Level,
}

/// 固件 IRQ 电平极性。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqPolarity {
    ActiveHigh,
    ActiveLow,
}

/// 固件 IRQ 共享属性。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqSharing {
    Exclusive,
    Shared,
}

/// 固件 IRQ descriptor 的通用属性。
///
/// DTB 通常把触发方式编码进 `cells`，对应字段可以保持 `None`；ACPI `_CRS`
/// 会单独给出触发、极性、共享和唤醒能力，这些属性不能在进入 PnP 资源层时丢失。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IrqResourceAttributes {
    pub trigger: Option<IrqTrigger>,
    pub polarity: Option<IrqPolarity>,
    pub sharing: Option<IrqSharing>,
    pub wake_capable: bool,
}

impl DeviceResource {
    /// 构造一个 MMIO 资源。`size == 0` 表示固件没有提供窗口长度，驱动只能把它
    /// 当作未知长度资源使用，不能据此做越界假设。
    pub const fn mmio(phys: usize, size: usize) -> Self {
        Self::Mmio { phys, size }
    }

    /// 如果该资源是 MMIO，返回 `(phys, size)`；其它资源返回 `None`。
    pub const fn as_mmio(&self) -> Option<(usize, usize)> {
        match self {
            Self::Mmio { phys, size } => Some((*phys, *size)),
            Self::Irq { .. } => None,
        }
    }

    /// 如果该资源是 IRQ，返回只读视图；其它资源返回 `None`。
    pub fn as_irq(&self) -> Option<FirmwareIrqResource<'_>> {
        match self {
            Self::Mmio { .. } => None,
            Self::Irq {
                controller,
                cells,
                attributes,
            } => Some(FirmwareIrqResource {
                controller: *controller,
                cells,
                attributes: *attributes,
            }),
        }
    }

    /// 构造一个固件 IRQ 资源。`controller == None` 表示由当前固件模型的默认
    /// IRQ domain 解释，例如 ACPI GSI；带 phandle/controller 的资源则由对应
    /// interrupt-controller driver 注册的 domain 翻译。
    pub fn irq(controller: Option<u32>, cells: Box<[u32]>) -> Self {
        Self::irq_with_attributes(controller, cells, IrqResourceAttributes::default())
    }

    /// 构造一个带 descriptor 属性的固件 IRQ 资源。
    pub fn irq_with_attributes(
        controller: Option<u32>,
        cells: Box<[u32]>,
        attributes: IrqResourceAttributes,
    ) -> Self {
        Self::Irq {
            controller,
            cells,
            attributes,
        }
    }
}

/// 固件 IRQ 资源的只读视图。
///
/// 该类型把 `DeviceResource::Irq` 的内部布局隐藏起来。驱动需要按顺序取第 N 个
/// IRQ、读取属性或翻译成规范化 [`IrqLine`] 时，都应通过这个视图完成。
#[derive(Clone, Copy, Debug)]
pub struct FirmwareIrqResource<'a> {
    controller: Option<u32>,
    cells: &'a [u32],
    attributes: IrqResourceAttributes,
}

impl<'a> FirmwareIrqResource<'a> {
    pub const fn controller(self) -> Option<u32> {
        self.controller
    }

    pub const fn cells(self) -> &'a [u32] {
        self.cells
    }

    pub const fn attributes(self) -> IrqResourceAttributes {
        self.attributes
    }

    pub fn resolve_line(self) -> Option<IrqLine> {
        irq::translate_firmware_irq(self.controller, self.cells)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformIrqResolveError {
    /// 固件没有给这个 platform 设备声明 IRQ 资源。
    NoResource,
    /// 固件声明了 IRQ，但对应 IRQ domain 暂未注册或无法翻译该 specifier。
    Unresolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformIrqRegistrationError {
    /// 固件没有给这个 platform 设备声明 IRQ 资源。
    NoResource,
    /// 固件声明了 IRQ，但对应 IRQ domain 暂未注册或无法翻译该 specifier。
    Unresolved,
    /// IRQ 已翻译成功，但注册 handler 失败。
    RegistrationFailed { line: IrqLine, err: IrqError },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceProperties {
    pub clock_hz: Option<u32>,
    pub baud: Option<u32>,
    /// 固件节点 phandle。DTB interrupt-controller driver 用它注册 IRQ domain；
    /// 没有 phandle 的固件来源保持 `None`。
    pub fw_phandle: Option<u32>,
    /// 固件描述的父 interrupt-controller phandle。interrupt-controller 自身也
    /// 可能需要它来建立级联关系，即使节点没有 `interrupts` 属性。
    pub fw_interrupt_parent: Option<u32>,
    /// 该 platform 节点是否声明为 interrupt-controller。
    pub interrupt_controller: bool,
    /// 固件节点为子地址空间声明的 `#address-cells`。
    pub fw_address_cells: Option<u8>,
    /// 固件节点为子地址空间声明的 `#size-cells`。
    pub fw_size_cells: Option<u8>,
    /// 父总线解释本节点 `reg`/`ranges` parent address 所用的 address cell 数。
    pub fw_parent_address_cells: Option<u8>,
    /// 父总线解释本节点 `reg` 所用的 size cell 数。
    pub fw_parent_size_cells: Option<u8>,
    pub stdout: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirmwarePropertyValue {
    Bool,
    U32(u32),
    U32List(Box<[u32]>),
    StringList(Box<[Box<str>]>),
    Bytes(Box<[u8]>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirmwareProperty {
    pub name: Box<str>,
    pub value: FirmwarePropertyValue,
}

#[derive(Debug)]
pub struct PlatformDeviceInfo {
    pub fw_name: Box<str>,
    /// 固件树中的完整路径。ACPI 等没有树形路径的来源可以保持 `None`。
    ///
    /// 该字段只用于设备拓扑和稳定 instance 区分，不参与 `/dev` 命名或 POSIX
    /// 设备号投影。
    pub fw_path: Option<Box<str>>,
    /// 固件树中的父节点路径。父节点如果也被登记成 platform 设备，启动路径会用它
    /// 建立 PnP parent/children 关系。
    pub fw_parent_path: Option<Box<str>>,
    pub ids: Vec<DeviceMatchId>,
    pub resources: Vec<DeviceResource>,
    pub properties: DeviceProperties,
    pub fw_properties: Vec<FirmwareProperty>,
}

#[kernel_symbols::export]
impl PlatformDeviceInfo {
    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.has_id",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
    )]
    pub fn has_id(&self, expected: &str) -> bool {
        self.ids.iter().any(|id| id.matches_str(expected))
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.first_mmio",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn first_mmio(&self) -> Option<(usize, usize)> {
        self.mmio_at(0)
    }

    /// 按声明顺序遍历 MMIO 资源。
    pub fn mmio_resources(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.resources.iter().filter_map(DeviceResource::as_mmio)
    }

    /// 按声明顺序返回第 `index` 个 MMIO 资源。
    ///
    /// 驱动如果需要“第二段窗口”这类关系，应通过本接口表达对固件资源顺序的依赖，
    /// 不要在具体驱动里直接遍历并匹配 `DeviceResource` 枚举。
    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.mmio_at",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn mmio_at(&self, index: usize) -> Option<(usize, usize)> {
        self.mmio_resources().nth(index)
    }

    /// 按声明顺序遍历 IRQ 资源。
    pub fn irq_resources(&self) -> impl Iterator<Item = FirmwareIrqResource<'_>> + '_ {
        self.resources.iter().filter_map(DeviceResource::as_irq)
    }

    /// 按声明顺序返回第 `index` 个 IRQ 资源。
    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.irq_at",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
    )]
    pub fn irq_at(&self, index: usize) -> Option<FirmwareIrqResource<'_>> {
        self.irq_resources().nth(index)
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.has_irq_resource",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
    )]
    pub fn has_irq_resource(&self) -> bool {
        self.irq_resources().next().is_some()
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.first_irq_line",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
    )]
    pub fn first_irq_line(&self) -> Option<IrqLine> {
        self.resolve_first_irq_line().ok()
    }

    /// 返回该 platform 设备的 DMA 上下文。
    ///
    /// platform 设备没有统一可枚举配置空间，DMA coherent 等能力来自固件属性。
    /// 未声明时不假设设备 cache coherent；地址转换仍走平台 mapper 的默认入口。
    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.dma_context",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA
    )]
    pub fn dma_context(&self) -> DmaContext {
        DmaContext::with_constraints(DmaConstraints {
            address_mask: usize::MAX,
            max_segment_size: usize::MAX,
            max_segments: 1,
            coherent: self.bool_property("dma-coherent"),
            supports_scatter_gather: false,
            bounce: DmaBouncePolicy::Disabled,
        })
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.resolve_irq_line_at",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
    )]
    pub fn resolve_irq_line_at(&self, index: usize) -> Result<IrqLine, PlatformIrqResolveError> {
        let irq = self
            .irq_at(index)
            .ok_or(PlatformIrqResolveError::NoResource)?;
        irq.resolve_line()
            .ok_or(PlatformIrqResolveError::Unresolved)
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.resolve_first_irq_line",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
    )]
    pub fn resolve_first_irq_line(&self) -> Result<IrqLine, PlatformIrqResolveError> {
        let mut saw_irq = false;
        for irq in self.irq_resources() {
            saw_irq = true;
            if let Some(line) = irq.resolve_line() {
                return Ok(line);
            }
        }
        Err(if saw_irq {
            PlatformIrqResolveError::Unresolved
        } else {
            PlatformIrqResolveError::NoResource
        })
    }

    /// 使用第 `index` 个固件 IRQ 资源注册 handler。
    ///
    /// 驱动只声明“我要消费哪个固件 IRQ 资源”，翻译细节仍由 IRQ domain 完成；
    /// platform 层负责把缺资源、依赖未就绪和 handler 注册失败拆成不同错误。
    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.register_irq_handler_at",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
        retained_args = 2u64
    )]
    pub fn register_irq_handler_at(
        &self,
        index: usize,
        handler: Arc<dyn IrqHandler>,
    ) -> Result<IrqHandle, PlatformIrqRegistrationError> {
        let irq_resource = self
            .irq_at(index)
            .ok_or(PlatformIrqRegistrationError::NoResource)?;
        let line = irq_resource
            .resolve_line()
            .ok_or(PlatformIrqRegistrationError::Unresolved)?;
        register_firmware_irq_handler(irq_resource, line, handler)
    }

    /// 使用第一个可翻译的固件 IRQ 资源注册 handler。
    ///
    /// 多个 IRQ 资源按固件声明顺序检查；已经声明但暂时无法翻译时返回
    /// [`PlatformIrqRegistrationError::Unresolved`]，让 PnP core 保留设备并等待
    /// interrupt-controller 驱动完成注册后重试 probe。
    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.register_first_irq_handler",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
        retained_args = 2u64
    )]
    pub fn register_first_irq_handler(
        &self,
        handler: Arc<dyn IrqHandler>,
    ) -> Result<IrqHandle, PlatformIrqRegistrationError> {
        let mut saw_irq = false;
        for irq_resource in self.irq_resources() {
            saw_irq = true;
            let Some(line) = irq_resource.resolve_line() else {
                continue;
            };
            return register_firmware_irq_handler(irq_resource, line, handler);
        }
        Err(if saw_irq {
            PlatformIrqRegistrationError::Unresolved
        } else {
            PlatformIrqRegistrationError::NoResource
        })
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.u32_property",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn u32_property(&self, name: &str) -> Option<u32> {
        self.fw_properties
            .iter()
            .find(|property| property.name.as_ref() == name)
            .and_then(|property| match property.value {
                FirmwarePropertyValue::U32(value) => Some(value),
                FirmwarePropertyValue::Bool
                | FirmwarePropertyValue::U32List(_)
                | FirmwarePropertyValue::StringList(_)
                | FirmwarePropertyValue::Bytes(_) => None,
            })
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.u32_list_property",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn u32_list_property(&self, name: &str) -> Option<&[u32]> {
        self.fw_properties
            .iter()
            .find(|property| property.name.as_ref() == name)
            .and_then(|property| match &property.value {
                FirmwarePropertyValue::U32List(values) => Some(values.as_ref()),
                FirmwarePropertyValue::Bool
                | FirmwarePropertyValue::U32(_)
                | FirmwarePropertyValue::StringList(_)
                | FirmwarePropertyValue::Bytes(_) => None,
            })
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.bool_property",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn bool_property(&self, name: &str) -> bool {
        self.fw_properties.iter().any(|property| {
            property.name.as_ref() == name && matches!(property.value, FirmwarePropertyValue::Bool)
        })
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.string_list_property",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn string_list_property(&self, name: &str) -> Option<&[Box<str>]> {
        self.fw_properties
            .iter()
            .find(|property| property.name.as_ref() == name)
            .and_then(|property| match &property.value {
                FirmwarePropertyValue::StringList(values) => Some(values.as_ref()),
                FirmwarePropertyValue::Bool
                | FirmwarePropertyValue::U32(_)
                | FirmwarePropertyValue::U32List(_)
                | FirmwarePropertyValue::Bytes(_) => None,
            })
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.bytes_property",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn bytes_property(&self, name: &str) -> Option<&[u8]> {
        self.fw_properties
            .iter()
            .find(|property| property.name.as_ref() == name)
            .and_then(|property| match &property.value {
                FirmwarePropertyValue::Bytes(values) => Some(values.as_ref()),
                FirmwarePropertyValue::Bool
                | FirmwarePropertyValue::U32(_)
                | FirmwarePropertyValue::U32List(_)
                | FirmwarePropertyValue::StringList(_) => None,
            })
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.mmio_by_name",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn mmio_by_name(&self, names: &[&str]) -> Option<(usize, usize)> {
        let reg_names = self.string_list_property("reg-names")?;
        let mut mmio_index = 0usize;
        for resource in &self.resources {
            let DeviceResource::Mmio { phys, size } = resource else {
                continue;
            };
            let matched = reg_names
                .get(mmio_index)
                .is_some_and(|reg_name| names.iter().any(|name| reg_name.as_ref() == *name));
            if matched {
                return Some((*phys, *size));
            }
            mmio_index += 1;
        }
        None
    }
}

fn register_firmware_irq_handler(
    irq_resource: FirmwareIrqResource<'_>,
    line: IrqLine,
    handler: Arc<dyn IrqHandler>,
) -> Result<IrqHandle, PlatformIrqRegistrationError> {
    // 固件 IRQ descriptor 中的触发/极性/共享信息必须进入 IRQ core；驱动只负责
    // 声明 handler，不应重复解析 controller-specific cells。
    let attributes = irq_resource.attributes();
    let mut request = irq::IrqRequest::shared(line, "platform-firmware-irq", handler);
    request.sharing = attributes
        .sharing
        .map(map_irq_sharing)
        .unwrap_or(irq::IrqSharing::Shared);
    request.trigger = attributes.trigger.map(map_irq_trigger);
    request.polarity = attributes.polarity.map(map_irq_polarity);
    irq::register_irq_request(request)
        .map_err(|err| PlatformIrqRegistrationError::RegistrationFailed { line, err })
}

fn map_irq_trigger(trigger: IrqTrigger) -> irq::IrqTrigger {
    match trigger {
        IrqTrigger::Edge => irq::IrqTrigger::Edge,
        IrqTrigger::Level => irq::IrqTrigger::Level,
    }
}

fn map_irq_polarity(polarity: IrqPolarity) -> irq::IrqPolarity {
    match polarity {
        IrqPolarity::ActiveHigh => irq::IrqPolarity::High,
        IrqPolarity::ActiveLow => irq::IrqPolarity::Low,
    }
}

fn map_irq_sharing(sharing: IrqSharing) -> irq::IrqSharing {
    match sharing {
        IrqSharing::Exclusive => irq::IrqSharing::Exclusive,
        IrqSharing::Shared => irq::IrqSharing::Shared,
    }
}

impl PnpBusInfo for PlatformDeviceInfo {
    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct PlatformRegistration {
    pub device: Arc<PnpDevice>,
    pub status: PlatformProbeStatus,
}

/// platform 设备首次进入 PnP core 后的 probe 结果。
///
/// `Deferred` 与 `NoDriver` 必须分开：前者表示驱动已经匹配但依赖尚未就绪，
/// 设备应保留在全局表中等待后续重试；后者只是当前驱动集合没有认领它。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformProbeStatus {
    Bound,
    NoDriver,
    Deferred,
}

impl PlatformRegistration {
    pub const fn bound(&self) -> bool {
        matches!(self.status, PlatformProbeStatus::Bound)
    }
}

#[kernel_symbols::export(
    name = "general.dev.platform.register_and_probe_platform_device",
    contract = "kernel.general.platform-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn register_and_probe_platform_device(
    info: PlatformDeviceInfo,
) -> Result<PlatformRegistration, PnpError> {
    let name = info.fw_name.clone();
    let identity = platform_identity(&info);
    let id = PnpId::Platform {
        name: name.clone(),
        identity,
    };
    let new_dev = PnpDevice::new(id, name, Box::new(info))?;
    let registration = PNP_DEVICES.get_or_insert(Arc::clone(&new_dev))?;
    let dev = registration.device;
    let inserted = registration.inserted;

    match dev.state() {
        PnpState::Bound => {
            return Ok(PlatformRegistration {
                device: dev,
                status: PlatformProbeStatus::Bound,
            });
        }
        PnpState::Discovered => {}
        PnpState::Probing | PnpState::Removing | PnpState::Gone => {
            if inserted {
                PNP_DEVICES.remove_exact(&dev);
            }
            return Err(PnpError::InvalidState);
        }
    }

    match PNP_DRIVERS.probe_device(&dev) {
        Ok(()) => Ok(PlatformRegistration {
            device: dev,
            status: PlatformProbeStatus::Bound,
        }),
        Err(PnpError::NoDriver) => Ok(PlatformRegistration {
            device: dev,
            status: PlatformProbeStatus::NoDriver,
        }),
        Err(err) if err.is_deferred() => Ok(PlatformRegistration {
            device: dev,
            status: PlatformProbeStatus::Deferred,
        }),
        Err(err) => {
            if inserted {
                PNP_DEVICES.remove_exact(&dev);
            }
            Err(err)
        }
    }
}

fn platform_identity(info: &PlatformDeviceInfo) -> PlatformIdentity {
    // platform 设备身份保留完整固件路径和资源 tuple，避免 32 位 hash 碰撞导致
    // 两个不同固件节点被 PnP core 误判为同一设备。
    let match_ids: Vec<PlatformIdentityMatchId> = info
        .ids
        .iter()
        .map(|id| match id {
            DeviceMatchId::DtbCompatible(value) => {
                PlatformIdentityMatchId::DtbCompatible(value.clone())
            }
            DeviceMatchId::AcpiHid(value) => PlatformIdentityMatchId::AcpiHid(value.clone()),
            DeviceMatchId::AcpiCid(value) => PlatformIdentityMatchId::AcpiCid(value.clone()),
        })
        .collect();
    let resources: Vec<PlatformIdentityResource> = info
        .resources
        .iter()
        .map(|resource| match resource {
            DeviceResource::Mmio { phys, size } => PlatformIdentityResource::Mmio {
                phys: *phys,
                size: *size,
            },
            DeviceResource::Irq {
                controller,
                cells,
                attributes,
            } => PlatformIdentityResource::Irq {
                controller: *controller,
                cells: cells.clone(),
                attributes: platform_identity_irq_attributes(*attributes),
            },
        })
        .collect();
    PlatformIdentity::new(
        info.fw_path.clone(),
        info.fw_parent_path.clone(),
        match_ids.into_boxed_slice(),
        resources.into_boxed_slice(),
    )
}

fn platform_identity_irq_attributes(
    attributes: IrqResourceAttributes,
) -> PlatformIdentityIrqAttributes {
    PlatformIdentityIrqAttributes {
        trigger: attributes.trigger.map(|trigger| match trigger {
            IrqTrigger::Edge => PlatformIdentityIrqTrigger::Edge,
            IrqTrigger::Level => PlatformIdentityIrqTrigger::Level,
        }),
        polarity: attributes.polarity.map(|polarity| match polarity {
            IrqPolarity::ActiveHigh => PlatformIdentityIrqPolarity::ActiveHigh,
            IrqPolarity::ActiveLow => PlatformIdentityIrqPolarity::ActiveLow,
        }),
        sharing: attributes.sharing.map(|sharing| match sharing {
            IrqSharing::Exclusive => PlatformIdentityIrqSharing::Exclusive,
            IrqSharing::Shared => PlatformIdentityIrqSharing::Shared,
        }),
        wake_capable: attributes.wake_capable,
    }
}

//! 固件枚举的 platform 设备描述。
//!
//! DTB 与 ACPI 都会描述一类不挂在 PCI/USB 这类可枚举总线下的设备。启动路径
//! 将固件里的 compatible/HID、资源和少量属性转换成这里的中立结构，再交给 PnP
//! 注册表由 platform 驱动统一匹配和 probe。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use crate::dev::dma::DmaContext;
#[cfg(test)]
use crate::dev::dma::{DmaBouncePolicy, DmaConstraints};
use crate::dev::dt_provider::{self, DtbProviderError, DtbResourceLease};
use crate::dev::irq::{self, IrqError, IrqHandle, IrqHandler, IrqLine};
use crate::dev::pnp::{
    BusType, PNP_DEVICES, PNP_DRIVERS, PlatformIdentity, PlatformIdentityIrqAttributes,
    PlatformIdentityIrqPolarity, PlatformIdentityIrqSharing, PlatformIdentityIrqTrigger,
    PlatformIdentityMatchId, PlatformIdentityResource, PnpBusInfo, PnpDevice, PnpError, PnpId,
    PnpState,
};
use crate::firmware::dtb::{
    DtbNodeInfo, DtbPcieHostInfo, DtbPlatformBindings, DtbProviderReference,
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
    IoPort {
        base: u16,
        size: u16,
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

    pub const fn io_port(base: u16, size: u16) -> Self {
        Self::IoPort { base, size }
    }

    /// 如果该资源是 MMIO，返回 `(phys, size)`；其它资源返回 `None`。
    pub const fn as_mmio(&self) -> Option<(usize, usize)> {
        match self {
            Self::Mmio { phys, size } => Some((*phys, *size)),
            Self::IoPort { .. } | Self::Irq { .. } => None,
        }
    }

    pub const fn as_io_port(&self) -> Option<(u16, u16)> {
        match self {
            Self::IoPort { base, size } => Some((*base, *size)),
            Self::Mmio { .. } | Self::Irq { .. } => None,
        }
    }

    /// 如果该资源是 IRQ，返回只读视图；其它资源返回 `None`。
    pub fn as_irq(&self) -> Option<FirmwareIrqResource<'_>> {
        match self {
            Self::Mmio { .. } | Self::IoPort { .. } => None,
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
    /// 固件描述的 NUMA node ID；没有拓扑信息时保持 `None`。
    pub numa_node_id: Option<u32>,
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
pub struct FirmwareProperty {
    pub name: Box<str>,
    /// 固件属性的权威原始值。任何 typed view 都只能从这里派生，不能替代它。
    raw_value: Box<[u8]>,
}

impl FirmwareProperty {
    /// 保存完整固件属性。typed view 在调用方按 binding 请求时无分配解码。
    pub fn new(name: Box<str>, raw_value: Box<[u8]>) -> Self {
        Self { name, raw_value }
    }

    pub fn raw_value(&self) -> &[u8] {
        &self.raw_value
    }

    pub fn as_bool(&self) -> bool {
        self.raw_value.is_empty()
    }

    pub fn as_u32(&self) -> Option<u32> {
        Some(u32::from_be_bytes(self.raw_value.as_ref().try_into().ok()?))
    }

    pub fn as_u32_list(&self) -> Option<FirmwareU32List<'_>> {
        FirmwareU32List::new(&self.raw_value)
    }

    pub fn as_string_list(&self) -> Option<FirmwareStringList<'_>> {
        FirmwareStringList::new(&self.raw_value)
    }
}

/// 大端 32-bit cell 列表的无分配借用视图。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareU32List<'a> {
    raw: &'a [u8],
    cursor: usize,
}

impl<'a> FirmwareU32List<'a> {
    fn new(raw: &'a [u8]) -> Option<Self> {
        raw.len()
            .is_multiple_of(4)
            .then_some(Self { raw, cursor: 0 })
    }

    pub fn get(self, index: usize) -> Option<u32> {
        self.into_iter().nth(index)
    }
}

/// 为动态 ELM 提供不带 `self` 接收者的 cell 列表随机访问入口。
#[kernel_symbols::export(
    name = "general.dev.platform.firmware_u32_list_get",
    contract = "kernel.general.platform-device@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE
)]
pub fn firmware_u32_list_get(list: FirmwareU32List<'_>, index: usize) -> Option<u32> {
    list.get(index)
}

impl Iterator for FirmwareU32List<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        let end = self.cursor.checked_add(4)?;
        let cell: [u8; 4] = self.raw.get(self.cursor..end)?.try_into().ok()?;
        self.cursor = end;
        Some(u32::from_be_bytes(cell))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (self.raw.len() - self.cursor) / 4;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for FirmwareU32List<'_> {}
impl core::iter::FusedIterator for FirmwareU32List<'_> {}

/// NUL 分隔 UTF-8 字符串列表的无分配借用视图。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareStringList<'a> {
    raw: &'a [u8],
    cursor: usize,
}

impl<'a> FirmwareStringList<'a> {
    fn new(raw: &'a [u8]) -> Option<Self> {
        if !raw.is_empty() && raw.last() != Some(&0) {
            return None;
        }
        let mut cursor = 0;
        while cursor < raw.len() {
            let relative_end = raw[cursor..].iter().position(|&byte| byte == 0)?;
            core::str::from_utf8(&raw[cursor..cursor + relative_end]).ok()?;
            cursor += relative_end + 1;
        }
        Some(Self { raw, cursor: 0 })
    }

    pub fn get(self, index: usize) -> Option<&'a str> {
        self.into_iter().nth(index)
    }
}

impl<'a> Iterator for FirmwareStringList<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor == self.raw.len() {
            return None;
        }
        let relative_end = self.raw[self.cursor..].iter().position(|&byte| byte == 0)?;
        let value =
            core::str::from_utf8(&self.raw[self.cursor..self.cursor + relative_end]).ok()?;
        self.cursor += relative_end + 1;
        Some(value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.raw[self.cursor..]
            .iter()
            .filter(|&&byte| byte == 0)
            .count();
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for FirmwareStringList<'_> {}
impl core::iter::FusedIterator for FirmwareStringList<'_> {}

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
    /// 与 IRQ 资源按声明顺序一一对应的 `interrupt-names`。
    ///
    /// ACPI 或未声明名称的 DT 节点使用空槽；查询接口不会从原始属性重新解析。
    pub irq_names: Vec<Option<Box<str>>>,
    pub properties: DeviceProperties,
    pub fw_properties: Vec<FirmwareProperty>,
    /// 枚举阶段已按固件父链固化的 per-device DMA 上下文。
    pub dma: DmaContext,
    /// DT 来源设备的规范化 provider、DMA/IOMMU 与 graph binding。
    ///
    /// ACPI 等其它固件来源保持 `None`。驱动应优先消费这里的 typed 关系，仅在
    /// 尚未纳入标准解码的 vendor binding 上读取 `fw_properties` 原始字节。
    pub dtb_bindings: Option<DtbPlatformBindings>,
    /// 与该 platform 节点同路径的规范化 DT PCI host 描述。
    ///
    /// 只有 `pci-host-*-generic` 节点设置该字段；ELM 驱动不需要重新切片原始
    /// `ranges`、`interrupt-map`、`msi-map` 或 DMA/IOMMU 属性。
    pub dtb_pcie_host: Option<DtbPcieHostInfo>,
    /// 由该 platform 节点负责枚举的 DT 子树快照。
    ///
    /// 快照包含节点自身，以及不属于任何更深层 platform 设备的后代。专用总线
    /// controller 因而能在 live overlay 提交前枚举候选 I2C/SPI/MDIO 子设备，
    /// 不必回读仍指向旧 generation 的全局节点图。
    pub dtb_owned_nodes: Option<Arc<[DtbNodeInfo]>>,
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
        name = "general.dev.platform.PlatformDeviceInfo.dtb_pcie_host",
        contract = "kernel.general.platform-device@2",
        version = 2,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
            | kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn dtb_pcie_host(&self) -> Option<&DtbPcieHostInfo> {
        self.dtb_pcie_host.as_ref()
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.dtb_owned_nodes",
        contract = "kernel.general.platform-device@3",
        version = 3,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
            | kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn dtb_owned_nodes(&self) -> Option<&[DtbNodeInfo]> {
        self.dtb_owned_nodes.as_deref()
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

    pub fn io_port_resources(&self) -> impl Iterator<Item = (u16, u16)> + '_ {
        self.resources.iter().filter_map(DeviceResource::as_io_port)
    }

    pub fn first_io_port(&self) -> Option<(u16, u16)> {
        self.io_port_resources().next()
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

    /// 按 `interrupt-names` 中的稳定名称返回 IRQ 资源。
    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.irq_by_name",
        contract = "kernel.general.platform-device@2",
        version = 2,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
    )]
    pub fn irq_by_name(&self, name: &str) -> Option<FirmwareIrqResource<'_>> {
        let index = self
            .irq_names
            .iter()
            .position(|candidate| candidate.as_deref() == Some(name))?;
        self.irq_at(index)
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
        self.dma.clone()
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

    /// 按 `interrupt-names` 名称翻译 IRQ domain。
    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.resolve_irq_line_by_name",
        contract = "kernel.general.platform-device@2",
        version = 2,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
    )]
    pub fn resolve_irq_line_by_name(&self, name: &str) -> Result<IrqLine, PlatformIrqResolveError> {
        let irq = self
            .irq_by_name(name)
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

    /// 使用 `interrupt-names` 指定的固件 IRQ 资源注册 handler。
    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.register_irq_handler_by_name",
        contract = "kernel.general.platform-device@2",
        version = 2,
        capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
        retained_args = 3u64
    )]
    pub fn register_irq_handler_by_name(
        &self,
        name: &str,
        handler: Arc<dyn IrqHandler>,
    ) -> Result<IrqHandle, PlatformIrqRegistrationError> {
        let irq_resource = self
            .irq_by_name(name)
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
            .and_then(FirmwareProperty::as_u32)
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.u32_list_property",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn u32_list_property(&self, name: &str) -> Option<FirmwareU32List<'_>> {
        self.fw_properties
            .iter()
            .find(|property| property.name.as_ref() == name)
            .and_then(FirmwareProperty::as_u32_list)
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.bool_property",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn bool_property(&self, name: &str) -> bool {
        self.fw_properties
            .iter()
            .any(|property| property.name.as_ref() == name && property.as_bool())
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.string_list_property",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn string_list_property(&self, name: &str) -> Option<FirmwareStringList<'_>> {
        self.fw_properties
            .iter()
            .find(|property| property.name.as_ref() == name)
            .and_then(FirmwareProperty::as_string_list)
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
            .map(FirmwareProperty::raw_value)
    }

    /// 按原始属性名遍历规范化 DT provider 引用。
    pub fn dtb_references(&self, property: &str) -> impl Iterator<Item = &DtbProviderReference> {
        self.dtb_bindings
            .iter()
            .flat_map(|bindings| bindings.references.iter())
            .filter(move |reference| reference.property.as_ref() == property)
    }

    /// 按 `*-names` 名称查找一个规范化 DT provider 引用。
    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.dtb_reference_by_name",
        contract = "kernel.general.platform-device@2",
        version = 2,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn dtb_reference_by_name(
        &self,
        property: &str,
        name: &str,
    ) -> Option<&DtbProviderReference> {
        self.dtb_references(property)
            .find(|reference| reference.name.as_deref() == Some(name))
    }

    /// 按同名属性中的声明顺序获取一个 provider 资源。
    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.acquire_dtb_resource_at",
        contract = "kernel.general.platform-device@2",
        version = 2,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn acquire_dtb_resource_at(
        &self,
        property: &str,
        index: usize,
    ) -> Result<DtbResourceLease, DtbProviderError> {
        let reference = self
            .dtb_references(property)
            .nth(index)
            .ok_or(DtbProviderError::Invalid)?;
        dt_provider::acquire_reference(reference)
    }

    /// 按标准 `*-names` 中的名字获取 provider 资源。
    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.acquire_named_dtb_resource",
        contract = "kernel.general.platform-device@2",
        version = 2,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn acquire_named_dtb_resource(
        &self,
        property: &str,
        name: &str,
    ) -> Result<DtbResourceLease, DtbProviderError> {
        let reference = self
            .dtb_reference_by_name(property, name)
            .ok_or(DtbProviderError::Invalid)?;
        dt_provider::acquire_reference(reference)
    }

    #[kernel_symbols::export(
        name = "general.dev.platform.PlatformDeviceInfo.mmio_by_name",
        contract = "kernel.general.platform-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn mmio_by_name(&self, names: &[&str]) -> Option<(usize, usize)> {
        let mut reg_names = self.string_list_property("reg-names")?;
        for resource in &self.resources {
            let DeviceResource::Mmio { phys, size } = resource else {
                continue;
            };
            let matched = reg_names
                .next()
                .is_some_and(|reg_name| names.contains(&reg_name));
            if matched {
                return Some((*phys, *size));
            }
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

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn firmware_properties_keep_raw_values_and_decode_by_requested_binding() {
        let info = platform_info(vec![
            firmware_property("cell", &0x1234_5678u32.to_be_bytes()),
            firmware_property("strings", b"ns16550a\0uart\0"),
            firmware_property("opaque", &[0xaa, 0xbb, 0xcc]),
            firmware_property("flag", &[]),
        ]);

        assert_eq!(
            info.bytes_property("cell"),
            Some(0x1234_5678u32.to_be_bytes().as_slice())
        );
        assert_eq!(info.u32_property("cell"), Some(0x1234_5678));
        assert_eq!(
            info.u32_list_property("cell")
                .map(|values| values.collect::<Vec<_>>()),
            Some(vec![0x1234_5678])
        );

        assert_eq!(
            info.bytes_property("strings"),
            Some(b"ns16550a\0uart\0".as_slice())
        );
        let strings = info
            .string_list_property("strings")
            .expect("NUL 结尾的合法字符串列表应可显式解码");
        assert_eq!(strings.len(), 2);
        assert_eq!(strings.get(0), Some("ns16550a"));
        assert_eq!(strings.get(1), Some("uart"));

        assert_eq!(
            info.bytes_property("opaque"),
            Some([0xaa, 0xbb, 0xcc].as_slice())
        );
        assert_eq!(info.u32_property("opaque"), None);
        assert_eq!(info.u32_list_property("opaque"), None);

        assert_eq!(info.bytes_property("flag"), Some([].as_slice()));
        assert!(info.bool_property("flag"));
        assert_eq!(info.u32_property("flag"), None);
        assert_eq!(info.u32_list_property("flag").map(Iterator::count), Some(0));
        assert_eq!(
            info.string_list_property("flag").map(Iterator::count),
            Some(0)
        );
        assert!(!info.bool_property("cell"));
    }

    #[test]
    fn large_zero_property_keeps_constant_size_borrowed_views() {
        let raw = vec![0; 1024 * 1024].into_boxed_slice();
        let property = FirmwareProperty::new("large".into(), raw);

        assert_eq!(property.raw_value().len(), 1024 * 1024);
        assert_eq!(property.as_u32_list().unwrap().len(), 256 * 1024);
        assert_eq!(property.as_string_list().unwrap().len(), 1024 * 1024);
        assert!(core::mem::size_of::<FirmwareProperty>() <= 4 * core::mem::size_of::<usize>());
    }

    #[test]
    fn drivers_can_query_normalized_dtb_references_without_slicing_properties() {
        let mut info = platform_info(Vec::new());
        info.dtb_bindings = Some(DtbPlatformBindings {
            references: vec![
                DtbProviderReference {
                    property: "clocks".into(),
                    name: Some("core".into()),
                    provider: None,
                    provider_path: None,
                    provider_available: None,
                    phandle: 0,
                    args: Vec::new().into_boxed_slice(),
                },
                DtbProviderReference {
                    property: "reset-gpios".into(),
                    name: None,
                    provider: None,
                    provider_path: None,
                    provider_available: None,
                    phandle: 0,
                    args: Vec::new().into_boxed_slice(),
                },
            ],
            ..DtbPlatformBindings::default()
        });

        let clock = info
            .dtb_reference_by_name("clocks", "core")
            .expect("named reference must be directly queryable");
        assert_eq!(clock.property.as_ref(), "clocks");
        assert_eq!(clock.phandle, 0);
        assert_eq!(info.dtb_references("reset-gpios").count(), 1);
        assert_eq!(info.dtb_references("resets").count(), 0);
    }

    #[test]
    fn drivers_can_select_interrupts_by_binding_name() {
        let mut info = platform_info(Vec::new());
        info.resources = vec![
            DeviceResource::mmio(0x1000, 0x100),
            DeviceResource::irq(Some(7), vec![3].into_boxed_slice()),
            DeviceResource::irq(Some(7), vec![4].into_boxed_slice()),
        ];
        info.irq_names = vec![Some("rx".into()), Some("tx".into())];

        let rx = info.irq_by_name("rx").expect("rx IRQ must be named");
        let tx = info.irq_by_name("tx").expect("tx IRQ must be named");
        assert_eq!((rx.controller(), rx.cells()), (Some(7), [3].as_slice()));
        assert_eq!((tx.controller(), tx.cells()), (Some(7), [4].as_slice()));
        assert!(info.irq_by_name("error").is_none());
    }

    fn firmware_property(name: &str, raw_value: &[u8]) -> FirmwareProperty {
        FirmwareProperty::new(name.into(), raw_value.into())
    }

    fn platform_info(fw_properties: Vec<FirmwareProperty>) -> PlatformDeviceInfo {
        PlatformDeviceInfo {
            fw_name: "test".into(),
            fw_path: None,
            fw_parent_path: None,
            ids: Vec::new(),
            resources: Vec::new(),
            irq_names: Vec::new(),
            properties: DeviceProperties::default(),
            fw_properties,
            dma: DmaContext::with_constraints(DmaConstraints {
                address_mask: usize::MAX,
                max_segment_size: usize::MAX,
                max_segments: 1,
                coherent: false,
                supports_scatter_gather: false,
                bounce: DmaBouncePolicy::Disabled,
            }),
            dtb_bindings: None,
            dtb_pcie_host: None,
            dtb_owned_nodes: None,
        }
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
    if inserted
        && let Some(resource) = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .and_then(|info| info.dma.claim_iommu_pnp_resource("platform-iommu-consumer"))
        && let Err(error) = dev.own_bus_resource(resource)
    {
        PNP_DEVICES.remove_exact(&dev);
        return Err(error);
    }

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
            DeviceResource::IoPort { base, size } => PlatformIdentityResource::IoPort {
                base: *base,
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

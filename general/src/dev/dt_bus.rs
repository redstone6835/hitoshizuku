//! Device Tree 专用子总线核心。
//!
//! I2C、SPI 与 MDIO 子节点的 `reg` 描述总线地址，而不是 CPU MMIO。本文模块
//! 从已安装的完整 DT 节点图中只枚举控制器的直接 enabled 子节点，并把它们投影
//! 为动态 PnP 设备。每个子设备同时保留完整拥有型 [`DtbNodeInfo`]，驱动无需重新
//! 解释临时 FDT 视图，也不会丢失 interrupt、provider binding 或原始属性。
//!
//! 控制器操作对象可以由 ELM 实现。registry 用不可复用的 generation handle、
//! in-flight call 和枚举状态保护其 vtable；注销成功前不会销毁仍可能被子
//! 驱动调用的对象。控制器 registration 由父 PnP 设备拥有，因此 probe 回滚、
//! driver unbind 与热拔都会沿 PnP 拓扑先移除子设备，再释放控制器。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use vfs::sync::Spinlock;

use super::platform::PlatformDeviceInfo;
use crate::firmware::dtb::{DtbDeviceProperty, DtbNodeInfo, node_graph_snapshot};

use super::pnp::{
    BusType, DynamicPnpBusInfo, DynamicPnpProperty, DynamicPnpResource, PNP_DEVICES, PNP_DRIVERS,
    PnpDevice, PnpError, PnpId, PnpRemovalTransaction, PnpResource, PnpResourceKind,
    PnpResourceReleaseError, PnpResourceReleaseOrder, PnpState,
};
use super::registry_id;

/// DT I2C 子设备的稳定 PnP 总线类型。
pub const DT_I2C_BUS: BusType = BusType::new("dt-i2c");
/// DT SPI 子设备的稳定 PnP 总线类型。
pub const DT_SPI_BUS: BusType = BusType::new("dt-spi");
/// DT MDIO 子设备的稳定 PnP 总线类型。
pub const DT_MDIO_BUS: BusType = BusType::new("dt-mdio");

pub const DT_I2C_CHILD_CONTRACT: &str = "kernel.general.dt-i2c-child@1";
pub const DT_SPI_CHILD_CONTRACT: &str = "kernel.general.dt-spi-child@1";
pub const DT_MDIO_CHILD_CONTRACT: &str = "kernel.general.dt-mdio-child@1";

/// [`DynamicPnpResource`] 中的专用总线地址。
///
/// `start` 是规范化地址，`length` 固定为 1，`payload` 为空。
pub const DT_BUS_RESOURCE_ADDRESS: u32 = 0x4454_4201;
/// [`DynamicPnpResource`] 中已经解析到最终 provider 的 interrupt specifier。
///
/// `start` 是 interrupt parent phandle（没有时为 0），`length` 是 cell 数，
/// `payload` 按 DT big-endian cell 顺序编码。
pub const DT_BUS_RESOURCE_INTERRUPT: u32 = 0x4454_4202;
/// [`DynamicPnpResource`] 中已经切分的 DT provider reference。
///
/// `start` 是最终 phandle，`length` 是参数 cell 数，`payload` 按 DT big-endian
/// cell 顺序编码。引用来自哪个属性、名称和 provider path 保留在完整 descriptor。
pub const DT_BUS_RESOURCE_PROVIDER: u32 = 0x4454_4203;

pub const DT_BUS_ADDRESS_FLAG_I2C_OWN_SLAVE: u64 = 1 << 0;
pub const DT_BUS_ADDRESS_FLAG_I2C_TEN_BIT: u64 = 1 << 1;
pub const DT_BUS_PROVIDER_FLAG_AVAILABLE: u64 = 1 << 0;
pub const DT_BUS_PROVIDER_FLAG_DISABLED: u64 = 1 << 1;
pub const DT_BUS_PROVIDER_FLAG_NULL: u64 = 1 << 2;

const I2C_TEN_BIT_ADDRESS: u32 = 1 << 31;
const I2C_OWN_SLAVE_ADDRESS: u32 = 1 << 30;

// ── 总线地址与操作契约 ──────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum DtbBusKind {
    I2c = 1,
    Spi = 2,
    Mdio = 3,
}

impl DtbBusKind {
    pub const fn bus_type(self) -> BusType {
        match self {
            Self::I2c => DT_I2C_BUS,
            Self::Spi => DT_SPI_BUS,
            Self::Mdio => DT_MDIO_BUS,
        }
    }

    pub const fn bus_name(self) -> &'static str {
        match self {
            Self::I2c => "dt-i2c",
            Self::Spi => "dt-spi",
            Self::Mdio => "dt-mdio",
        }
    }

    pub const fn child_contract(self) -> &'static str {
        match self {
            Self::I2c => DT_I2C_CHILD_CONTRACT,
            Self::Spi => DT_SPI_CHILD_CONTRACT,
            Self::Mdio => DT_MDIO_CHILD_CONTRACT,
        }
    }
}

/// 专用子总线上的规范化地址。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DtbBusAddress {
    I2c {
        address: u16,
        ten_bit: bool,
        own_slave: bool,
    },
    SpiChipSelect(u32),
    MdioPhy(u8),
}

impl DtbBusAddress {
    pub const fn kind(self) -> DtbBusKind {
        match self {
            Self::I2c { .. } => DtbBusKind::I2c,
            Self::SpiChipSelect(_) => DtbBusKind::Spi,
            Self::MdioPhy(_) => DtbBusKind::Mdio,
        }
    }

    pub const fn raw(self) -> u32 {
        match self {
            Self::I2c { address, .. } => address as u32,
            Self::SpiChipSelect(chip_select) => chip_select,
            Self::MdioPhy(address) => address as u32,
        }
    }
}

/// 控制器 registry、binding 校验或 PnP 发布错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbBusError {
    InvalidController,
    NodeGraphUnavailable,
    ControllerNodeNotFound,
    ControllerDisabled,
    InvalidControllerBinding,
    AlreadyRegistered,
    NotFound,
    Busy,
    OutOfMemory,
    MissingReg,
    InvalidReg,
    AddressOutOfRange,
    MissingChipSelect,
    ChipSelectOutOfRange,
    DuplicateAddress,
    IdentityConflict,
    Pnp(PnpError),
}

impl From<PnpError> for DtbBusError {
    fn from(error: PnpError) -> Self {
        match error {
            PnpError::OutOfMemory => Self::OutOfMemory,
            error => Self::Pnp(error),
        }
    }
}

/// 实际总线操作的统一错误分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbBusOperationError {
    WrongBus,
    InvalidRequest,
    DeviceGone,
    Busy,
    Unsupported,
    NoAcknowledge,
    ArbitrationLost,
    Timeout,
    HardwareFailure,
}

/// 一条 I2C combined transfer message。
///
/// message 顺序由调用方保留；控制器应在相邻 message 间使用 repeated START，并在
/// 最后一条之后结束 transaction。零长度 message 保留给标准 quick-command 能力，
/// 通用层不擅自拒绝。
pub enum DtbI2cMessage<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbI2cTarget {
    pub address: u16,
    pub ten_bit: bool,
    pub own_slave: bool,
}

/// 由 I2C controller ELM 实现的标准 combined-transfer 操作。
pub trait DtbI2cController: Send + Sync {
    fn transfer(
        &self,
        target: DtbI2cTarget,
        messages: &mut [DtbI2cMessage<'_>],
    ) -> Result<(), DtbBusOperationError>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DtbSpiMode {
    pub clock_phase: bool,
    pub clock_polarity: bool,
    pub chip_select_high: bool,
    pub three_wire: bool,
    pub lsb_first: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbSpiTarget {
    pub chip_select: u32,
    pub max_frequency_hz: Option<u32>,
    pub mode: DtbSpiMode,
}

/// 一段 SPI message transfer。
///
/// 同时提供 `tx` 与 `rx` 表示 full-duplex，二者长度必须相同。`cs_change` 与 Linux
/// `spi_transfer` 同义；通用层只转交该标准语义，不推断控制器私有时序。
pub struct DtbSpiTransfer<'a> {
    pub tx: Option<&'a [u8]>,
    pub rx: Option<&'a mut [u8]>,
    pub speed_hz: Option<u32>,
    pub bits_per_word: Option<u8>,
    pub delay_usecs: u16,
    pub cs_change: bool,
}

/// 由 SPI controller ELM 实现的 message 操作。
pub trait DtbSpiController: Send + Sync {
    /// 返回控制器当前可寻址的 chip-select 数量。
    fn chip_select_count(&self) -> u32;

    fn transfer(
        &self,
        target: DtbSpiTarget,
        transfers: &mut [DtbSpiTransfer<'_>],
    ) -> Result<(), DtbBusOperationError>;
}

/// 标准 IEEE 802.3 MDIO register 地址。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbMdioRegister {
    Clause22(u8),
    Clause45 { device: u8, register: u16 },
}

/// 由 MDIO controller ELM 实现的 Clause 22/Clause 45 操作。
pub trait DtbMdioController: Send + Sync {
    fn read(&self, phy: u8, register: DtbMdioRegister) -> Result<u16, DtbBusOperationError>;

    fn write(
        &self,
        phy: u8,
        register: DtbMdioRegister,
        value: u16,
    ) -> Result<(), DtbBusOperationError>;
}

#[derive(Clone)]
enum ControllerOps {
    I2c(Arc<dyn DtbI2cController>),
    Spi(Arc<dyn DtbSpiController>),
    Mdio(Arc<dyn DtbMdioController>),
}

impl ControllerOps {
    const fn kind(&self) -> DtbBusKind {
        match self {
            Self::I2c(_) => DtbBusKind::I2c,
            Self::Spi(_) => DtbBusKind::Spi,
            Self::Mdio(_) => DtbBusKind::Mdio,
        }
    }
}

/// 一次位于 registry 锁外的 ELM controller 调用。
struct ControllerCall {
    handle: DtbBusControllerHandle,
    controller: Option<ControllerOps>,
}

impl ControllerCall {
    fn controller(&self) -> &ControllerOps {
        self.controller
            .as_ref()
            .expect("live DT bus call always owns its controller")
    }
}

impl Drop for ControllerCall {
    fn drop(&mut self) {
        // Arc 的析构可能执行 ELM 代码；in-flight 计数必须覆盖完整析构过程。
        drop(self.controller.take());
        let mut registry = DT_BUS_CONTROLLERS.lock();
        let Some(entry) = registry
            .controllers
            .iter_mut()
            .find(|entry| entry.handle == self.handle)
        else {
            log::error!(
                "[dt-bus] controller call outlived generation={}",
                self.handle.generation
            );
            return;
        };
        entry.calls_in_flight = entry.calls_in_flight.saturating_sub(1);
    }
}

fn begin_controller_call(
    handle: DtbBusControllerHandle,
    expected_kind: DtbBusKind,
) -> Result<ControllerCall, DtbBusOperationError> {
    if handle.kind != expected_kind {
        return Err(DtbBusOperationError::WrongBus);
    }
    let controller = {
        let mut registry = DT_BUS_CONTROLLERS.lock();
        let entry = registry
            .controllers
            .iter_mut()
            .find(|entry| entry.handle == handle)
            .ok_or(DtbBusOperationError::DeviceGone)?;
        if entry.retiring || entry.removing_children {
            return Err(DtbBusOperationError::Busy);
        }
        entry.calls_in_flight = entry
            .calls_in_flight
            .checked_add(1)
            .ok_or(DtbBusOperationError::Busy)?;
        entry.controller.clone()
    };
    Ok(ControllerCall {
        handle,
        controller: Some(controller),
    })
}

// ── 拥有型节点描述与 target lease ───────────────────────────────────────

/// 一个已枚举子设备的完整、不可变 DT 描述。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbBusChildDescriptor {
    kind: DtbBusKind,
    controller_path: Box<str>,
    controller_generation: u64,
    address: DtbBusAddress,
    spi: Option<DtbSpiTarget>,
    node: DtbNodeInfo,
}

impl DtbBusChildDescriptor {
    pub const fn kind(&self) -> DtbBusKind {
        self.kind
    }

    pub fn controller_path(&self) -> &str {
        &self.controller_path
    }

    pub const fn controller_generation(&self) -> u64 {
        self.controller_generation
    }

    pub const fn address(&self) -> DtbBusAddress {
        self.address
    }

    pub const fn spi_target(&self) -> Option<DtbSpiTarget> {
        self.spi
    }

    /// 返回解析器产生的完整拥有型节点快照。
    pub const fn node(&self) -> &DtbNodeInfo {
        &self.node
    }
}

/// 控制器注册生命周期句柄。
///
/// `generation` 在本次启动期间永不复用，因此旧 ELM 句柄不会误注销同一路径后来
/// 注册的新控制器。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DtbBusControllerHandle {
    kind: DtbBusKind,
    generation: u64,
}

impl DtbBusControllerHandle {
    pub const fn kind(self) -> DtbBusKind {
        self.kind
    }

    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// 子驱动对 controller generation 的活动引用。
///
/// 对象内的 trait object 先于 registry 计数释放；因此最后一个 target Drop 完成后，
/// controller ELM vtable 才可能由 [`unregister_controller`] 销毁。
pub struct DtbBusTarget {
    handle: DtbBusControllerHandle,
    child: Weak<PnpDevice>,
    descriptor: Arc<DtbBusChildDescriptor>,
}

#[kernel_symbols::export]
impl DtbBusTarget {
    pub fn descriptor(&self) -> &DtbBusChildDescriptor {
        &self.descriptor
    }

    fn ensure_live(&self) -> Result<(), DtbBusOperationError> {
        let Some(child) = self.child.upgrade() else {
            return Err(DtbBusOperationError::DeviceGone);
        };
        if child.state() == PnpState::Gone {
            return Err(DtbBusOperationError::DeviceGone);
        }
        Ok(())
    }

    #[kernel_symbols::export(
        name = "general.dev.dt_bus.DtbBusTarget.i2c_transfer",
        contract = "kernel.general.dt-child-bus@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn i2c_transfer(
        &self,
        messages: &mut [DtbI2cMessage<'_>],
    ) -> Result<(), DtbBusOperationError> {
        self.ensure_live()?;
        let DtbBusAddress::I2c {
            address,
            ten_bit,
            own_slave,
        } = self.descriptor.address
        else {
            return Err(DtbBusOperationError::WrongBus);
        };
        let call = begin_controller_call(self.handle, DtbBusKind::I2c)?;
        let ControllerOps::I2c(controller) = call.controller() else {
            return Err(DtbBusOperationError::WrongBus);
        };
        controller.transfer(
            DtbI2cTarget {
                address,
                ten_bit,
                own_slave,
            },
            messages,
        )
    }

    #[kernel_symbols::export(
        name = "general.dev.dt_bus.DtbBusTarget.spi_transfer",
        contract = "kernel.general.dt-child-bus@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn spi_transfer(
        &self,
        transfers: &mut [DtbSpiTransfer<'_>],
    ) -> Result<(), DtbBusOperationError> {
        self.ensure_live()?;
        let Some(target) = self.descriptor.spi else {
            return Err(DtbBusOperationError::WrongBus);
        };
        let call = begin_controller_call(self.handle, DtbBusKind::Spi)?;
        let ControllerOps::Spi(controller) = call.controller() else {
            return Err(DtbBusOperationError::WrongBus);
        };
        for transfer in transfers.iter() {
            let tx_len = transfer.tx.map_or(0, <[u8]>::len);
            let rx_len = transfer.rx.as_deref().map_or(0, <[u8]>::len);
            if transfer.tx.is_none() && transfer.rx.is_none() {
                return Err(DtbBusOperationError::InvalidRequest);
            }
            if transfer.tx.is_some() && transfer.rx.is_some() && tx_len != rx_len {
                return Err(DtbBusOperationError::InvalidRequest);
            }
            if transfer.speed_hz == Some(0) || transfer.bits_per_word == Some(0) {
                return Err(DtbBusOperationError::InvalidRequest);
            }
            if let (Some(requested), Some(maximum)) = (transfer.speed_hz, target.max_frequency_hz)
                && requested > maximum
            {
                return Err(DtbBusOperationError::InvalidRequest);
            }
        }
        controller.transfer(target, transfers)
    }

    #[kernel_symbols::export(
        name = "general.dev.dt_bus.DtbBusTarget.mdio_read",
        contract = "kernel.general.dt-child-bus@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn mdio_read(&self, register: DtbMdioRegister) -> Result<u16, DtbBusOperationError> {
        self.ensure_live()?;
        let DtbBusAddress::MdioPhy(phy) = self.descriptor.address else {
            return Err(DtbBusOperationError::WrongBus);
        };
        validate_mdio_register(register)?;
        let call = begin_controller_call(self.handle, DtbBusKind::Mdio)?;
        let ControllerOps::Mdio(controller) = call.controller() else {
            return Err(DtbBusOperationError::WrongBus);
        };
        controller.read(phy, register)
    }

    #[kernel_symbols::export(
        name = "general.dev.dt_bus.DtbBusTarget.mdio_write",
        contract = "kernel.general.dt-child-bus@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn mdio_write(
        &self,
        register: DtbMdioRegister,
        value: u16,
    ) -> Result<(), DtbBusOperationError> {
        self.ensure_live()?;
        let DtbBusAddress::MdioPhy(phy) = self.descriptor.address else {
            return Err(DtbBusOperationError::WrongBus);
        };
        validate_mdio_register(register)?;
        let call = begin_controller_call(self.handle, DtbBusKind::Mdio)?;
        let ControllerOps::Mdio(controller) = call.controller() else {
            return Err(DtbBusOperationError::WrongBus);
        };
        controller.write(phy, register, value)
    }
}

fn validate_mdio_register(register: DtbMdioRegister) -> Result<(), DtbBusOperationError> {
    match register {
        DtbMdioRegister::Clause22(register) if register > 31 => {
            Err(DtbBusOperationError::InvalidRequest)
        }
        DtbMdioRegister::Clause45 { device, .. } if device > 31 => {
            Err(DtbBusOperationError::InvalidRequest)
        }
        _ => Ok(()),
    }
}

/// PnP core 拥有的 target lease。
pub struct DtbBusTargetPnpResource {
    target: Option<Arc<DtbBusTarget>>,
    label: &'static str,
}

impl PnpResource for DtbBusTargetPnpResource {
    fn kind(&self) -> PnpResourceKind {
        PnpResourceKind::Other("dt-bus-target")
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn release(mut self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        drop(self.target.take());
        Ok(())
    }
}

#[kernel_symbols::export(
    name = "general.dev.dt_bus.target_pnp_resource",
    contract = "kernel.general.dt-child-bus@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 1u64
)]
pub fn target_pnp_resource(
    target: Arc<DtbBusTarget>,
    label: &'static str,
) -> DtbBusTargetPnpResource {
    DtbBusTargetPnpResource {
        target: Some(target),
        label,
    }
}

// ── Controller registry ─────────────────────────────────────────────────

struct ChildRecord {
    device: Arc<PnpDevice>,
    descriptor: Arc<DtbBusChildDescriptor>,
}

struct ControllerRegistration {
    handle: DtbBusControllerHandle,
    controller_path: Box<str>,
    owner: Weak<PnpDevice>,
    controller: ControllerOps,
    spi_chip_select_count: Option<u32>,
    firmware_nodes: Arc<[DtbNodeInfo]>,
    children: Vec<ChildRecord>,
    calls_in_flight: usize,
    enumerating: bool,
    removing_children: bool,
    retiring: bool,
}

struct ControllerRegistry {
    next_generation: u64,
    controllers: Vec<ControllerRegistration>,
}

impl ControllerRegistry {
    const fn new() -> Self {
        Self {
            next_generation: 1,
            controllers: Vec::new(),
        }
    }
}

static DT_BUS_CONTROLLERS: Spinlock<ControllerRegistry> = Spinlock::new(ControllerRegistry::new());

struct DtbBusControllerPnpResource {
    handle: DtbBusControllerHandle,
    prepared: AtomicBool,
}

impl PnpResource for DtbBusControllerPnpResource {
    fn kind(&self) -> PnpResourceKind {
        PnpResourceKind::Other("dt-bus-controller")
    }

    fn label(&self) -> &'static str {
        "dt-bus-controller"
    }

    fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
        if self.prepared.load(Ordering::Acquire) {
            return Ok(());
        }
        match prepare_unregister_controller(self.handle) {
            Ok(()) | Err(DtbBusError::NotFound) => {
                self.prepared.store(true, Ordering::Release);
                Ok(())
            }
            Err(_) => Err(PnpResourceReleaseError::new(
                PnpResourceKind::Other("dt-bus-controller"),
                "dt-bus-controller",
                "controller generation is still busy",
            )),
        }
    }

    fn cancel_release(&self) {
        if self.prepared.swap(false, Ordering::AcqRel) {
            cancel_unregister_controller(self.handle);
        }
    }

    fn release_order(&self) -> PnpResourceReleaseOrder {
        PnpResourceReleaseOrder::Provider
    }

    fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        if !self.prepared.load(Ordering::Acquire) {
            self.prepare_release()?;
        }
        match commit_unregister_controller(self.handle) {
            Ok(()) | Err(DtbBusError::NotFound) => Ok(()),
            Err(_) => Err(PnpResourceReleaseError::new(
                PnpResourceKind::Other("dt-bus-controller"),
                "dt-bus-controller",
                "controller generation is still busy",
            )),
        }
    }
}

#[kernel_symbols::export(
    name = "general.dev.dt_bus.register_i2c_controller",
    contract = "kernel.general.dt-child-bus@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS
        | kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 4u64
)]
pub fn register_i2c_controller(
    owner: &Arc<PnpDevice>,
    controller_path: &str,
    controller: Arc<dyn DtbI2cController>,
) -> Result<DtbBusControllerHandle, DtbBusError> {
    register_owned_controller(owner, controller_path, ControllerOps::I2c(controller))
}

#[kernel_symbols::export(
    name = "general.dev.dt_bus.register_spi_controller",
    contract = "kernel.general.dt-child-bus@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS
        | kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 4u64
)]
pub fn register_spi_controller(
    owner: &Arc<PnpDevice>,
    controller_path: &str,
    controller: Arc<dyn DtbSpiController>,
) -> Result<DtbBusControllerHandle, DtbBusError> {
    register_owned_controller(owner, controller_path, ControllerOps::Spi(controller))
}

#[kernel_symbols::export(
    name = "general.dev.dt_bus.register_mdio_controller",
    contract = "kernel.general.dt-child-bus@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS
        | kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 4u64
)]
pub fn register_mdio_controller(
    owner: &Arc<PnpDevice>,
    controller_path: &str,
    controller: Arc<dyn DtbMdioController>,
) -> Result<DtbBusControllerHandle, DtbBusError> {
    register_owned_controller(owner, controller_path, ControllerOps::Mdio(controller))
}

fn register_owned_controller(
    owner: &Arc<PnpDevice>,
    controller_path: &str,
    controller: ControllerOps,
) -> Result<DtbBusControllerHandle, DtbBusError> {
    let nodes = firmware_nodes_for_owner(owner)?;
    if nodes.is_empty() {
        return Err(DtbBusError::NodeGraphUnavailable);
    }
    let spi_chip_select_count = validate_controller_node(&nodes, controller_path, &controller)?;
    let handle = register_controller_inner(
        owner,
        controller_path,
        controller,
        spi_chip_select_count,
        nodes,
    )?;
    let resource: Box<dyn PnpResource> = Box::new(DtbBusControllerPnpResource {
        handle,
        prepared: AtomicBool::new(false),
    });
    owner
        .own_boxed_resource_or_release(resource)
        .map_err(DtbBusError::from)?;
    Ok(handle)
}

fn register_controller_inner(
    owner: &Arc<PnpDevice>,
    controller_path: &str,
    controller: ControllerOps,
    spi_chip_select_count: Option<u32>,
    firmware_nodes: Arc<[DtbNodeInfo]>,
) -> Result<DtbBusControllerHandle, DtbBusError> {
    if !controller_path.starts_with('/') || controller_path == "/" {
        return Err(DtbBusError::InvalidController);
    }
    let kind = controller.kind();
    let controller_path = try_copy_boxed_str(controller_path)?;
    let mut registry = DT_BUS_CONTROLLERS.lock();
    if registry.controllers.iter().any(|entry| {
        entry.handle.kind == kind && entry.controller_path.as_ref() == controller_path.as_ref()
    }) {
        return Err(DtbBusError::AlreadyRegistered);
    }
    registry
        .controllers
        .try_reserve(1)
        .map_err(|_| DtbBusError::OutOfMemory)?;
    let generation = registry_id::alloc_locked_id(&mut registry.next_generation)
        .map_err(|_| DtbBusError::OutOfMemory)?;
    let handle = DtbBusControllerHandle { kind, generation };
    registry.controllers.push(ControllerRegistration {
        handle,
        controller_path,
        owner: Arc::downgrade(owner),
        controller,
        spi_chip_select_count,
        firmware_nodes,
        children: Vec::new(),
        calls_in_flight: 0,
        enumerating: false,
        removing_children: false,
        retiring: false,
    });
    drop(registry);
    if super::elm_lifecycle::track_dtb_bus_controller(handle).is_err() {
        let _ = unregister_controller(handle);
        return Err(DtbBusError::OutOfMemory);
    }
    Ok(handle)
}

fn firmware_nodes_for_owner(owner: &Arc<PnpDevice>) -> Result<Arc<[DtbNodeInfo]>, DtbBusError> {
    if let Some(nodes) = owner
        .info
        .as_any()
        .downcast_ref::<PlatformDeviceInfo>()
        .and_then(|info| info.dtb_owned_nodes.as_ref())
    {
        return Ok(Arc::clone(nodes));
    }
    if let Some(nodes) = {
        let registry = DT_BUS_CONTROLLERS.lock();
        registry.controllers.iter().find_map(|entry| {
            entry
                .children
                .iter()
                .any(|child| Arc::ptr_eq(&child.device, owner))
                .then(|| Arc::clone(&entry.firmware_nodes))
        })
    } {
        return Ok(nodes);
    }
    let nodes = node_graph_snapshot();
    if nodes.is_empty() {
        return Err(DtbBusError::NodeGraphUnavailable);
    }
    Ok(Arc::from(nodes.into_boxed_slice()))
}

fn validate_controller_node(
    nodes: &[DtbNodeInfo],
    controller_path: &str,
    controller: &ControllerOps,
) -> Result<Option<u32>, DtbBusError> {
    let node = nodes
        .iter()
        .find(|node| node.path.as_ref() == controller_path)
        .ok_or(DtbBusError::ControllerNodeNotFound)?;
    if !node.enabled {
        return Err(DtbBusError::ControllerDisabled);
    }
    if node.address_cells != 1 || node.size_cells != 0 {
        return Err(DtbBusError::InvalidControllerBinding);
    }
    match controller {
        ControllerOps::Spi(controller) => {
            let supported = controller.chip_select_count();
            if supported == 0 {
                return Err(DtbBusError::InvalidControllerBinding);
            }
            let declared = optional_u32_property(node, "num-cs")?;
            if declared == Some(0) || declared.is_some_and(|declared| declared > supported) {
                return Err(DtbBusError::InvalidControllerBinding);
            }
            Ok(Some(declared.unwrap_or(supported)))
        }
        _ => Ok(None),
    }
}

fn validate_controller_snapshot(
    nodes: &[DtbNodeInfo],
    controller_path: &str,
    kind: DtbBusKind,
    spi_chip_select_count: Option<u32>,
) -> Result<(), DtbBusError> {
    let node = nodes
        .iter()
        .find(|node| node.path.as_ref() == controller_path)
        .ok_or(DtbBusError::ControllerNodeNotFound)?;
    if !node.enabled {
        return Err(DtbBusError::ControllerDisabled);
    }
    if node.address_cells != 1 || node.size_cells != 0 {
        return Err(DtbBusError::InvalidControllerBinding);
    }
    if kind == DtbBusKind::Spi {
        let supported = spi_chip_select_count.ok_or(DtbBusError::InvalidControllerBinding)?;
        let declared = optional_u32_property(node, "num-cs")?;
        if declared == Some(0) || declared.is_some_and(|declared| declared > supported) {
            return Err(DtbBusError::InvalidControllerBinding);
        }
    }
    Ok(())
}

/// 注销一个精确 controller generation。
///
/// 活跃子设备、target lease 或枚举事务存在时不改变 registry，直接返回 `Busy`。
#[kernel_symbols::export(
    name = "general.dev.dt_bus.unregister_controller",
    contract = "kernel.general.dt-child-bus@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS
        | kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_controller(handle: DtbBusControllerHandle) -> Result<(), DtbBusError> {
    prepare_unregister_controller(handle)?;
    match commit_unregister_controller(handle) {
        Ok(()) => Ok(()),
        Err(error) => {
            cancel_unregister_controller(handle);
            Err(error)
        }
    }
}

pub(crate) fn prepare_unregister_controller(
    handle: DtbBusControllerHandle,
) -> Result<(), DtbBusError> {
    let children = {
        let mut registry = DT_BUS_CONTROLLERS.lock();
        let entry = registry
            .controllers
            .iter_mut()
            .find(|entry| entry.handle == handle)
            .ok_or(DtbBusError::NotFound)?;
        if entry.enumerating
            || entry.removing_children
            || entry.retiring
            || entry.calls_in_flight != 0
        {
            return Err(DtbBusError::Busy);
        }
        let mut children = Vec::new();
        children
            .try_reserve(entry.children.len())
            .map_err(|_| DtbBusError::OutOfMemory)?;
        children.extend(entry.children.iter().map(|child| Arc::clone(&child.device)));
        entry.retiring = true;
        children
    };
    if children
        .iter()
        .any(|child| child.state() != PnpState::Gone && !child.removal_is_prepared())
    {
        cancel_unregister_controller(handle);
        return Err(DtbBusError::Busy);
    }
    Ok(())
}

pub(crate) fn cancel_unregister_controller(handle: DtbBusControllerHandle) {
    if let Some(entry) = DT_BUS_CONTROLLERS
        .lock()
        .controllers
        .iter_mut()
        .find(|entry| entry.handle == handle)
    {
        entry.retiring = false;
    }
}

fn commit_unregister_controller(handle: DtbBusControllerHandle) -> Result<(), DtbBusError> {
    let registration = {
        let mut registry = DT_BUS_CONTROLLERS.lock();
        let Some(index) = registry
            .controllers
            .iter()
            .position(|entry| entry.handle == handle)
        else {
            return Err(DtbBusError::NotFound);
        };
        if !registry.controllers[index].retiring
            || registry.controllers[index]
                .children
                .iter()
                .any(|child| child.device.state() != PnpState::Gone)
        {
            return Err(DtbBusError::Busy);
        }
        let entry = &registry.controllers[index];
        if entry.calls_in_flight != 0 || entry.enumerating || entry.removing_children {
            return Err(DtbBusError::Busy);
        }
        registry.controllers.remove(index)
    };
    // ELM trait object 与完整节点快照都在 registry 锁外销毁。
    drop(registration);
    super::elm_lifecycle::forget_dtb_bus_controller(handle);
    Ok(())
}

/// 移除当前 generation 已枚举的全部子设备。
///
/// 句柄校验与进入 removal 状态是原子的；一旦开始，PnP remove 本身没有可恢复
/// 错误。已有 target 可以完成正在进行的 remove 回调，但新的 acquire 会被拒绝。
#[kernel_symbols::export(
    name = "general.dev.dt_bus.remove_controller_children",
    contract = "kernel.general.dt-child-bus@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS
        | kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn remove_controller_children(handle: DtbBusControllerHandle) -> Result<usize, DtbBusError> {
    let children = {
        let mut registry = DT_BUS_CONTROLLERS.lock();
        let entry = registry
            .controllers
            .iter_mut()
            .find(|entry| entry.handle == handle)
            .ok_or(DtbBusError::NotFound)?;
        if entry.enumerating || entry.removing_children || entry.retiring {
            return Err(DtbBusError::Busy);
        }
        let mut children = Vec::new();
        children
            .try_reserve(entry.children.len())
            .map_err(|_| DtbBusError::OutOfMemory)?;
        children.extend(entry.children.iter().map(|child| Arc::clone(&child.device)));
        entry.removing_children = true;
        children
    };
    let count = children.len();
    let removal = PnpRemovalTransaction::prepare(&children).map_err(DtbBusError::from);
    if let Err(error) =
        removal.and_then(|transaction| transaction.commit().map_err(DtbBusError::from))
    {
        if let Some(entry) = DT_BUS_CONTROLLERS
            .lock()
            .controllers
            .iter_mut()
            .find(|entry| entry.handle == handle)
        {
            entry.removing_children = false;
        }
        return Err(error);
    }
    let mut registry = DT_BUS_CONTROLLERS.lock();
    if let Some(entry) = registry
        .controllers
        .iter_mut()
        .find(|entry| entry.handle == handle)
    {
        entry.children.clear();
        entry.removing_children = false;
    }
    Ok(count)
}

// ── 子节点枚举 ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbBusProbeStatus {
    Bound,
    NoDriver,
    Deferred,
}

#[derive(Clone)]
pub struct DtbBusChildRegistration {
    pub device: Arc<PnpDevice>,
    pub descriptor: Arc<DtbBusChildDescriptor>,
    pub status: DtbBusProbeStatus,
}

/// 从当前 live DT 节点图枚举控制器的直接 enabled 子节点。
#[kernel_symbols::export(
    name = "general.dev.dt_bus.enumerate_controller_children",
    contract = "kernel.general.dt-child-bus@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS
        | kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn enumerate_controller_children(
    handle: DtbBusControllerHandle,
) -> Result<Vec<DtbBusChildRegistration>, DtbBusError> {
    let nodes = {
        let registry = DT_BUS_CONTROLLERS.lock();
        let entry = registry
            .controllers
            .iter()
            .find(|entry| entry.handle == handle)
            .ok_or(DtbBusError::NotFound)?;
        entry.firmware_nodes.to_vec()
    };
    enumerate_controller_children_from_nodes(handle, nodes)
}

fn enumerate_controller_children_from_nodes(
    handle: DtbBusControllerHandle,
    nodes: Vec<DtbNodeInfo>,
) -> Result<Vec<DtbBusChildRegistration>, DtbBusError> {
    let (controller_path, owner, spi_chip_select_count) = {
        let mut registry = DT_BUS_CONTROLLERS.lock();
        let entry = registry
            .controllers
            .iter_mut()
            .find(|entry| entry.handle == handle)
            .ok_or(DtbBusError::NotFound)?;
        if entry.enumerating || entry.removing_children || entry.retiring {
            return Err(DtbBusError::Busy);
        }
        if !entry.children.is_empty() {
            return Err(DtbBusError::Busy);
        }
        let owner = entry
            .owner
            .upgrade()
            .ok_or(DtbBusError::InvalidController)?;
        entry.enumerating = true;
        (
            entry.controller_path.clone(),
            owner,
            entry.spi_chip_select_count,
        )
    };

    let result = prepare_child_records(handle, &controller_path, spi_chip_select_count, nodes);
    let records = match result {
        Ok(records) => records,
        Err(error) => {
            finish_enumeration(handle, false);
            return Err(error);
        }
    };

    let mut staged = Vec::new();
    if staged.try_reserve(records.len()).is_err() {
        finish_enumeration(handle, false);
        return Err(DtbBusError::OutOfMemory);
    }
    staged.extend(records.iter().map(|record| Arc::clone(&record.device)));
    let mut results = Vec::new();
    if results.try_reserve(records.len()).is_err() {
        finish_enumeration(handle, false);
        return Err(DtbBusError::OutOfMemory);
    }

    {
        let mut registry = DT_BUS_CONTROLLERS.lock();
        let Some(entry) = registry
            .controllers
            .iter_mut()
            .find(|entry| entry.handle == handle && entry.enumerating)
        else {
            drop(registry);
            rollback_staged_children(handle, &staged);
            return Err(DtbBusError::NotFound);
        };
        entry.children = records;
    }

    for device in &staged {
        if let Err(error) = owner.attach_child(device) {
            rollback_staged_children(handle, &staged);
            return Err(error.into());
        }
        let registration = match PNP_DEVICES.get_or_insert(Arc::clone(device)) {
            Ok(registration) if registration.inserted => registration,
            Ok(_) => {
                rollback_staged_children(handle, &staged);
                return Err(DtbBusError::IdentityConflict);
            }
            Err(error) => {
                rollback_staged_children(handle, &staged);
                return Err(error.into());
            }
        };
        let status = match PNP_DRIVERS.probe_device(&registration.device) {
            Ok(()) => DtbBusProbeStatus::Bound,
            Err(PnpError::NoDriver) => DtbBusProbeStatus::NoDriver,
            Err(error) if error.is_deferred() => DtbBusProbeStatus::Deferred,
            Err(error) => {
                rollback_staged_children(handle, &staged);
                return Err(error.into());
            }
        };
        let descriptor = match {
            let registry = DT_BUS_CONTROLLERS.lock();
            registry
                .controllers
                .iter()
                .find(|entry| entry.handle == handle)
                .and_then(|entry| {
                    entry
                        .children
                        .iter()
                        .find(|child| Arc::ptr_eq(&child.device, device))
                })
                .map(|child| Arc::clone(&child.descriptor))
        } {
            Some(descriptor) => descriptor,
            None => {
                rollback_staged_children(handle, &staged);
                return Err(DtbBusError::NotFound);
            }
        };
        results.push(DtbBusChildRegistration {
            device: registration.device,
            descriptor,
            status,
        });
    }
    finish_enumeration(handle, true);
    Ok(results)
}

fn finish_enumeration(handle: DtbBusControllerHandle, keep_children: bool) {
    let mut registry = DT_BUS_CONTROLLERS.lock();
    let Some(entry) = registry
        .controllers
        .iter_mut()
        .find(|entry| entry.handle == handle)
    else {
        return;
    };
    if !keep_children {
        entry.children.clear();
    }
    entry.enumerating = false;
}

fn rollback_staged_children(handle: DtbBusControllerHandle, staged: &[Arc<PnpDevice>]) {
    for child in staged.iter().rev() {
        child.remove_device();
    }
    finish_enumeration(handle, false);
}

fn prepare_child_records(
    handle: DtbBusControllerHandle,
    controller_path: &str,
    spi_chip_select_count: Option<u32>,
    nodes: Vec<DtbNodeInfo>,
) -> Result<Vec<ChildRecord>, DtbBusError> {
    validate_controller_snapshot(&nodes, controller_path, handle.kind, spi_chip_select_count)?;
    let direct_count = nodes
        .iter()
        .filter(|node| node.enabled && node.parent_path.as_deref() == Some(controller_path))
        .count();
    let mut descriptors = Vec::new();
    descriptors
        .try_reserve(direct_count)
        .map_err(|_| DtbBusError::OutOfMemory)?;
    for node in nodes {
        if !node.enabled || node.parent_path.as_deref() != Some(controller_path) {
            continue;
        }
        let descriptor = child_descriptor(
            handle,
            try_copy_boxed_str(controller_path)?,
            node,
            spi_chip_select_count,
        )?;
        if descriptors
            .iter()
            .any(|existing: &DtbBusChildDescriptor| existing.address == descriptor.address)
        {
            return Err(DtbBusError::DuplicateAddress);
        }
        descriptors.push(descriptor);
    }

    let mut records = Vec::new();
    records
        .try_reserve(descriptors.len())
        .map_err(|_| DtbBusError::OutOfMemory)?;
    for descriptor in descriptors {
        let properties = dynamic_properties(&descriptor.node.properties)?;
        let resources = dynamic_resources(&descriptor)?;
        let info = DynamicPnpBusInfo::new(
            handle.kind.bus_type(),
            handle.kind.bus_name(),
            handle.kind.child_contract(),
            properties,
            resources,
        )?;
        let id = PnpId::dynamic(
            handle.kind.bus_type(),
            handle.kind.child_contract(),
            descriptor.node.path.as_bytes(),
        )?;
        let name = try_copy_boxed_str(&descriptor.node.name)?;
        let descriptor = Arc::new(descriptor);
        let device = PnpDevice::new(id, name, Box::new(info))?;
        records.push(ChildRecord { device, descriptor });
    }
    Ok(records)
}

fn child_descriptor(
    handle: DtbBusControllerHandle,
    controller_path: Box<str>,
    node: DtbNodeInfo,
    spi_chip_select_count: Option<u32>,
) -> Result<DtbBusChildDescriptor, DtbBusError> {
    let raw_address = match raw_reg_address(&node) {
        Err(DtbBusError::MissingReg) if handle.kind == DtbBusKind::Spi => {
            return Err(DtbBusError::MissingChipSelect);
        }
        result => result?,
    };
    let (address, spi) = match handle.kind {
        DtbBusKind::I2c => {
            let ten_bit = raw_address & I2C_TEN_BIT_ADDRESS != 0;
            let own_slave = raw_address & I2C_OWN_SLAVE_ADDRESS != 0;
            let address = raw_address & !(I2C_TEN_BIT_ADDRESS | I2C_OWN_SLAVE_ADDRESS);
            let maximum = if ten_bit { 0x3ff } else { 0x7f };
            if address > maximum {
                return Err(DtbBusError::AddressOutOfRange);
            }
            (
                DtbBusAddress::I2c {
                    address: address as u16,
                    ten_bit,
                    own_slave,
                },
                None,
            )
        }
        DtbBusKind::Spi => {
            let count = spi_chip_select_count.ok_or(DtbBusError::InvalidControllerBinding)?;
            if raw_address >= count {
                return Err(DtbBusError::ChipSelectOutOfRange);
            }
            let target = DtbSpiTarget {
                chip_select: raw_address,
                max_frequency_hz: optional_u32_property(&node, "spi-max-frequency")?,
                mode: DtbSpiMode {
                    clock_phase: boolean_property(&node, "spi-cpha")?,
                    clock_polarity: boolean_property(&node, "spi-cpol")?,
                    chip_select_high: boolean_property(&node, "spi-cs-high")?,
                    three_wire: boolean_property(&node, "spi-3wire")?,
                    lsb_first: boolean_property(&node, "spi-lsb-first")?,
                },
            };
            if target.max_frequency_hz == Some(0) {
                return Err(DtbBusError::InvalidReg);
            }
            (DtbBusAddress::SpiChipSelect(raw_address), Some(target))
        }
        DtbBusKind::Mdio => {
            if raw_address > 31 {
                return Err(DtbBusError::AddressOutOfRange);
            }
            (DtbBusAddress::MdioPhy(raw_address as u8), None)
        }
    };
    Ok(DtbBusChildDescriptor {
        kind: handle.kind,
        controller_path,
        controller_generation: handle.generation,
        address,
        spi,
        node,
    })
}

fn raw_reg_address(node: &DtbNodeInfo) -> Result<u32, DtbBusError> {
    let mut properties = node
        .properties
        .iter()
        .filter(|property| property.name.as_ref() == "reg");
    let property = properties
        .next()
        .ok_or_else(|| match node.parent_size_cells {
            0 if node.parent_address_cells == 1 => DtbBusError::MissingReg,
            _ => DtbBusError::InvalidControllerBinding,
        })?;
    if properties.next().is_some() || property.value.len() != 4 {
        return Err(DtbBusError::InvalidReg);
    }
    if node.parent_address_cells != 1 || node.parent_size_cells != 0 {
        return Err(DtbBusError::InvalidControllerBinding);
    }
    Ok(u32::from_be_bytes([
        property.value[0],
        property.value[1],
        property.value[2],
        property.value[3],
    ]))
}

fn optional_u32_property(node: &DtbNodeInfo, name: &str) -> Result<Option<u32>, DtbBusError> {
    let mut properties = node
        .properties
        .iter()
        .filter(|property| property.name.as_ref() == name);
    let Some(property) = properties.next() else {
        return Ok(None);
    };
    if properties.next().is_some() || property.value.len() != 4 {
        return Err(DtbBusError::InvalidControllerBinding);
    }
    Ok(Some(u32::from_be_bytes([
        property.value[0],
        property.value[1],
        property.value[2],
        property.value[3],
    ])))
}

fn boolean_property(node: &DtbNodeInfo, name: &str) -> Result<bool, DtbBusError> {
    let mut properties = node
        .properties
        .iter()
        .filter(|property| property.name.as_ref() == name);
    let Some(property) = properties.next() else {
        return Ok(false);
    };
    if properties.next().is_some() || !property.value.is_empty() {
        return Err(DtbBusError::InvalidControllerBinding);
    }
    Ok(true)
}

fn dynamic_properties(
    properties: &[DtbDeviceProperty],
) -> Result<Vec<DynamicPnpProperty>, DtbBusError> {
    let mut out = Vec::new();
    out.try_reserve(properties.len())
        .map_err(|_| DtbBusError::OutOfMemory)?;
    for property in properties {
        out.push(DynamicPnpProperty {
            name: try_copy_boxed_str(&property.name)?,
            value: try_copy_boxed_bytes(&property.value)?,
        });
    }
    Ok(out)
}

fn dynamic_resources(
    descriptor: &DtbBusChildDescriptor,
) -> Result<Vec<DynamicPnpResource>, DtbBusError> {
    let node = &descriptor.node;
    let reference_count = node.bindings.references.len();
    let capacity = 1usize
        .checked_add(node.interrupts.len())
        .and_then(|value| value.checked_add(reference_count))
        .ok_or(DtbBusError::OutOfMemory)?;
    let mut resources = Vec::new();
    resources
        .try_reserve(capacity)
        .map_err(|_| DtbBusError::OutOfMemory)?;
    let address_flags = match descriptor.address {
        DtbBusAddress::I2c {
            ten_bit, own_slave, ..
        } => {
            u64::from(own_slave) * DT_BUS_ADDRESS_FLAG_I2C_OWN_SLAVE
                | u64::from(ten_bit) * DT_BUS_ADDRESS_FLAG_I2C_TEN_BIT
        }
        _ => 0,
    };
    resources.push(DynamicPnpResource {
        kind: DT_BUS_RESOURCE_ADDRESS,
        index: 0,
        start: u64::from(descriptor.address.raw()),
        length: 1,
        flags: address_flags,
        payload: Box::new([]),
    });
    for (index, interrupt) in node.interrupts.iter().enumerate() {
        resources.push(DynamicPnpResource {
            kind: DT_BUS_RESOURCE_INTERRUPT,
            index: u32::try_from(index).map_err(|_| DtbBusError::OutOfMemory)?,
            start: u64::from(interrupt.parent.unwrap_or(0)),
            length: u64::try_from(interrupt.specifier.len())
                .map_err(|_| DtbBusError::OutOfMemory)?,
            flags: 0,
            payload: cells_payload(&interrupt.specifier)?,
        });
    }
    for (index, reference) in node.bindings.references.iter().enumerate() {
        let flags = match reference.provider_available {
            Some(true) => DT_BUS_PROVIDER_FLAG_AVAILABLE,
            Some(false) => DT_BUS_PROVIDER_FLAG_DISABLED,
            None => DT_BUS_PROVIDER_FLAG_NULL,
        };
        resources.push(DynamicPnpResource {
            kind: DT_BUS_RESOURCE_PROVIDER,
            index: u32::try_from(index).map_err(|_| DtbBusError::OutOfMemory)?,
            start: u64::from(reference.phandle),
            length: u64::try_from(reference.args.len()).map_err(|_| DtbBusError::OutOfMemory)?,
            flags,
            payload: cells_payload(&reference.args)?,
        });
    }
    Ok(resources)
}

fn cells_payload(cells: &[u32]) -> Result<Box<[u8]>, DtbBusError> {
    let bytes = cells.len().checked_mul(4).ok_or(DtbBusError::OutOfMemory)?;
    let mut out = Vec::new();
    out.try_reserve_exact(bytes)
        .map_err(|_| DtbBusError::OutOfMemory)?;
    for cell in cells {
        out.extend_from_slice(&cell.to_be_bytes());
    }
    Ok(out.into_boxed_slice())
}

fn try_copy_boxed_str(value: &str) -> Result<Box<str>, DtbBusError> {
    let mut out = String::new();
    out.try_reserve_exact(value.len())
        .map_err(|_| DtbBusError::OutOfMemory)?;
    out.push_str(value);
    Ok(out.into_boxed_str())
}

fn try_copy_boxed_bytes(value: &[u8]) -> Result<Box<[u8]>, DtbBusError> {
    let mut out = Vec::new();
    out.try_reserve_exact(value.len())
        .map_err(|_| DtbBusError::OutOfMemory)?;
    out.extend_from_slice(value);
    Ok(out.into_boxed_slice())
}

/// 返回已发布子设备的不可变 DT 总线描述，不获取 controller vtable。
///
/// 驱动可以在 `matches_device()` 中使用该入口检查 compatible 和原始 binding；只有
/// probe 真正需要执行总线操作时才应调用 [`acquire_target`]。
#[kernel_symbols::export(
    name = "general.dev.dt_bus.child_descriptor_for_device",
    contract = "kernel.general.dt-child-bus@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 1u64
)]
pub fn child_descriptor_for_device(
    device: &Arc<PnpDevice>,
) -> Result<Arc<DtbBusChildDescriptor>, DtbBusError> {
    let registry = DT_BUS_CONTROLLERS.lock();
    registry
        .controllers
        .iter()
        .flat_map(|entry| entry.children.iter())
        .find(|child| Arc::ptr_eq(&child.device, device))
        .map(|child| Arc::clone(&child.descriptor))
        .ok_or(DtbBusError::NotFound)
}

/// 为一个已发布的专用总线子设备获取 controller target。
#[kernel_symbols::export(
    name = "general.dev.dt_bus.acquire_target",
    contract = "kernel.general.dt-child-bus@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_BUS
        | kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 1u64
)]
pub fn acquire_target(device: &Arc<PnpDevice>) -> Result<Arc<DtbBusTarget>, DtbBusError> {
    if matches!(device.state(), PnpState::Removing | PnpState::Gone) {
        return Err(DtbBusError::Busy);
    }
    let (handle, descriptor) = {
        let mut registry = DT_BUS_CONTROLLERS.lock();
        let Some(entry_index) = registry.controllers.iter().position(|entry| {
            entry
                .children
                .iter()
                .any(|child| Arc::ptr_eq(&child.device, device))
        }) else {
            return Err(DtbBusError::NotFound);
        };
        let entry = &mut registry.controllers[entry_index];
        if entry.removing_children || entry.retiring {
            return Err(DtbBusError::Busy);
        }
        let child_index = entry
            .children
            .iter()
            .position(|child| Arc::ptr_eq(&child.device, device))
            .ok_or(DtbBusError::NotFound)?;
        let descriptor = Arc::clone(&entry.children[child_index].descriptor);
        (entry.handle, descriptor)
    };
    Ok(Arc::new(DtbBusTarget {
        handle,
        child: Arc::downgrade(device),
        descriptor,
    }))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::sync::Arc;
    use alloc::vec;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::dev::pnp::PnpBusInfo;

    const FDT_BEGIN_NODE: u32 = 1;
    const FDT_END_NODE: u32 = 2;
    const FDT_PROP: u32 = 3;
    const FDT_END: u32 = 9;

    static TEST_LOCK: Spinlock<()> = Spinlock::new(());

    #[derive(Clone, Copy)]
    enum FixtureBus {
        I2c,
        Spi { num_cs: u32 },
        Mdio,
    }

    impl FixtureBus {
        fn kind(self) -> DtbBusKind {
            match self {
                Self::I2c => DtbBusKind::I2c,
                Self::Spi { .. } => DtbBusKind::Spi,
                Self::Mdio => DtbBusKind::Mdio,
            }
        }

        fn controller_name(self) -> &'static str {
            match self {
                Self::I2c => "i2c@1000",
                Self::Spi { .. } => "spi@1000",
                Self::Mdio => "mdio@1000",
            }
        }

        fn controller_path(self) -> &'static str {
            match self {
                Self::I2c => "/soc/i2c@1000",
                Self::Spi { .. } => "/soc/spi@1000",
                Self::Mdio => "/soc/mdio@1000",
            }
        }

        fn compatible(self) -> &'static [u8] {
            match self {
                Self::I2c => b"test,i2c\0",
                Self::Spi { .. } => b"test,spi\0",
                Self::Mdio => b"test,mdio\0",
            }
        }
    }

    #[derive(Clone, Copy)]
    struct FixtureChild {
        name: &'static str,
        reg: Option<u32>,
        enabled: bool,
    }

    struct DtbBuilder {
        structure: Vec<u8>,
        strings: Vec<u8>,
    }

    impl DtbBuilder {
        fn new() -> Self {
            Self {
                structure: Vec::new(),
                strings: Vec::new(),
            }
        }

        fn begin_node(&mut self, name: &str) {
            push_u32(&mut self.structure, FDT_BEGIN_NODE);
            self.structure.extend_from_slice(name.as_bytes());
            self.structure.push(0);
            pad_to(&mut self.structure, 4);
        }

        fn property(&mut self, name: &str, value: &[u8]) {
            let name_offset = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            push_u32(&mut self.structure, FDT_PROP);
            push_u32(&mut self.structure, value.len() as u32);
            push_u32(&mut self.structure, name_offset);
            self.structure.extend_from_slice(value);
            pad_to(&mut self.structure, 4);
        }

        fn end_node(&mut self) {
            push_u32(&mut self.structure, FDT_END_NODE);
        }

        fn finish(mut self) -> Vec<u8> {
            push_u32(&mut self.structure, FDT_END);
            let mut blob = vec![0; 40];
            pad_to(&mut blob, 8);
            let reservations_offset = blob.len();
            blob.extend_from_slice(&[0; 16]);
            let structure_offset = blob.len();
            blob.extend_from_slice(&self.structure);
            let strings_offset = blob.len();
            blob.extend_from_slice(&self.strings);
            let total_size = blob.len();
            set_u32(&mut blob, 0, fdt::DTB_MAGIC);
            set_u32(&mut blob, 4, total_size as u32);
            set_u32(&mut blob, 8, structure_offset as u32);
            set_u32(&mut blob, 12, strings_offset as u32);
            set_u32(&mut blob, 16, reservations_offset as u32);
            set_u32(&mut blob, 20, 17);
            set_u32(&mut blob, 24, 16);
            set_u32(&mut blob, 28, 0);
            set_u32(&mut blob, 32, self.strings.len() as u32);
            set_u32(&mut blob, 36, self.structure.len() as u32);
            blob
        }
    }

    fn fixture_nodes(bus: FixtureBus, children: &[FixtureChild]) -> Vec<DtbNodeInfo> {
        let mut builder = DtbBuilder::new();
        builder.begin_node("");
        builder.property("compatible", b"test,board\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));

        builder.begin_node("soc");
        builder.property("compatible", b"simple-bus\0");
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[1]));
        builder.property("ranges", &[]);

        builder.begin_node("clock");
        builder.property("compatible", b"fixed-clock\0");
        builder.property("#clock-cells", &cells(&[0]));
        builder.property("clock-frequency", &cells(&[24_000_000]));
        builder.property("phandle", &cells(&[1]));
        builder.end_node();

        builder.begin_node("interrupt-controller");
        builder.property("compatible", b"test,interrupt-controller\0");
        builder.property("interrupt-controller", &[]);
        builder.property("#interrupt-cells", &cells(&[1]));
        builder.property("phandle", &cells(&[2]));
        builder.end_node();

        builder.begin_node(bus.controller_name());
        builder.property("compatible", bus.compatible());
        builder.property("reg", &cells(&[0x1000, 0x100]));
        builder.property("#address-cells", &cells(&[1]));
        builder.property("#size-cells", &cells(&[0]));
        if let FixtureBus::Spi { num_cs } = bus {
            builder.property("num-cs", &cells(&[num_cs]));
        }

        for child in children {
            builder.begin_node(child.name);
            builder.property("compatible", b"test,child\0test,fallback\0");
            if let Some(reg) = child.reg {
                builder.property("reg", &cells(&[reg]));
            }
            builder.property("clocks", &cells(&[1]));
            builder.property("interrupt-parent", &cells(&[2]));
            builder.property("interrupts", &cells(&[5]));
            if !child.enabled {
                builder.property("status", b"disabled\0");
            }
            if matches!(bus, FixtureBus::Spi { .. }) {
                builder.property("spi-max-frequency", &cells(&[10_000_000]));
                builder.property("spi-cpha", &[]);
            }
            builder.end_node();
        }

        builder.end_node();
        builder.end_node();
        builder.end_node();

        let blob = builder.finish();
        crate::firmware::dtb::parse_blob(&blob).unwrap().nodes
    }

    fn cells(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_be_bytes())
            .collect()
    }

    fn push_u32(output: &mut Vec<u8>, value: u32) {
        output.extend_from_slice(&value.to_be_bytes());
    }

    fn set_u32(output: &mut [u8], offset: usize, value: u32) {
        output[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn pad_to(output: &mut Vec<u8>, align: usize) {
        while output.len() % align != 0 {
            output.push(0);
        }
    }

    struct NullI2c;

    impl DtbI2cController for NullI2c {
        fn transfer(
            &self,
            _target: DtbI2cTarget,
            _messages: &mut [DtbI2cMessage<'_>],
        ) -> Result<(), DtbBusOperationError> {
            Ok(())
        }
    }

    struct NullSpi {
        count: u32,
    }

    impl DtbSpiController for NullSpi {
        fn chip_select_count(&self) -> u32 {
            self.count
        }

        fn transfer(
            &self,
            _target: DtbSpiTarget,
            _transfers: &mut [DtbSpiTransfer<'_>],
        ) -> Result<(), DtbBusOperationError> {
            Ok(())
        }
    }

    struct NullMdio;

    impl DtbMdioController for NullMdio {
        fn read(&self, _phy: u8, _register: DtbMdioRegister) -> Result<u16, DtbBusOperationError> {
            Ok(0)
        }

        fn write(
            &self,
            _phy: u8,
            _register: DtbMdioRegister,
            _value: u16,
        ) -> Result<(), DtbBusOperationError> {
            Ok(())
        }
    }

    fn controller_ops(bus: FixtureBus) -> ControllerOps {
        match bus {
            FixtureBus::I2c => ControllerOps::I2c(Arc::new(NullI2c)),
            FixtureBus::Spi { num_cs } => ControllerOps::Spi(Arc::new(NullSpi { count: num_cs })),
            FixtureBus::Mdio => ControllerOps::Mdio(Arc::new(NullMdio)),
        }
    }

    fn handle(kind: DtbBusKind) -> DtbBusControllerHandle {
        DtbBusControllerHandle {
            kind,
            generation: 77,
        }
    }

    fn direct_child(nodes: Vec<DtbNodeInfo>, path: &str) -> DtbNodeInfo {
        nodes
            .into_iter()
            .find(|node| node.path.as_ref() == path)
            .unwrap()
    }

    #[test]
    fn descriptor_preserves_complete_node_and_dynamic_projection() {
        let bus = FixtureBus::I2c;
        let nodes = fixture_nodes(
            bus,
            &[
                FixtureChild {
                    name: "sensor@50",
                    reg: Some(0x50),
                    enabled: true,
                },
                FixtureChild {
                    name: "disabled@51",
                    reg: Some(0x51),
                    enabled: false,
                },
            ],
        );
        validate_controller_node(&nodes, bus.controller_path(), &controller_ops(bus)).unwrap();
        let prepared = prepare_child_records(
            handle(bus.kind()),
            bus.controller_path(),
            None,
            nodes.clone(),
        )
        .unwrap();
        assert_eq!(prepared.len(), 1);
        assert_eq!(
            prepared[0].descriptor.node().path.as_ref(),
            "/soc/i2c@1000/sensor@50"
        );
        drop(prepared);

        let node = direct_child(nodes, "/soc/i2c@1000/sensor@50");
        let descriptor =
            child_descriptor(handle(bus.kind()), bus.controller_path().into(), node, None).unwrap();

        assert_eq!(
            descriptor.address(),
            DtbBusAddress::I2c {
                address: 0x50,
                ten_bit: false,
                own_slave: false,
            }
        );
        assert_eq!(descriptor.node().compatible.len(), 2);
        assert_eq!(descriptor.node().interrupts.len(), 1);
        assert_eq!(descriptor.node().interrupts[0].specifier.as_ref(), &[5]);
        assert_eq!(descriptor.node().bindings.references.len(), 1);
        assert_eq!(descriptor.node().bindings.references[0].phandle, 1);
        assert!(descriptor.node().properties.iter().any(|property| {
            property.name.as_ref() == "compatible"
                && property.value.as_ref() == b"test,child\0test,fallback\0"
        }));

        let properties = dynamic_properties(&descriptor.node().properties).unwrap();
        let resources = dynamic_resources(&descriptor).unwrap();
        assert_eq!(properties.len(), descriptor.node().properties.len());
        assert_eq!(resources[0].kind, DT_BUS_RESOURCE_ADDRESS);
        assert!(
            resources
                .iter()
                .any(|resource| resource.kind == DT_BUS_RESOURCE_INTERRUPT)
        );
        assert!(
            resources
                .iter()
                .any(|resource| resource.kind == DT_BUS_RESOURCE_PROVIDER)
        );
    }

    #[test]
    fn binding_limits_accept_ten_bit_i2c_and_reject_invalid_bus_addresses() {
        let ten_bit_nodes = fixture_nodes(
            FixtureBus::I2c,
            &[FixtureChild {
                name: "child@155",
                reg: Some(I2C_TEN_BIT_ADDRESS | I2C_OWN_SLAVE_ADDRESS | 0x155),
                enabled: true,
            }],
        );
        let ten_bit_node = direct_child(ten_bit_nodes, "/soc/i2c@1000/child@155");
        let ten_bit = child_descriptor(
            handle(DtbBusKind::I2c),
            "/soc/i2c@1000".into(),
            ten_bit_node,
            None,
        )
        .unwrap();
        assert_eq!(
            ten_bit.address(),
            DtbBusAddress::I2c {
                address: 0x155,
                ten_bit: true,
                own_slave: true,
            }
        );
        assert_eq!(
            dynamic_resources(&ten_bit).unwrap()[0].flags,
            DT_BUS_ADDRESS_FLAG_I2C_TEN_BIT | DT_BUS_ADDRESS_FLAG_I2C_OWN_SLAVE
        );

        let cases = [
            (FixtureBus::I2c, Some(0x80), DtbBusError::AddressOutOfRange),
            (
                FixtureBus::I2c,
                Some(I2C_TEN_BIT_ADDRESS | 0x400),
                DtbBusError::AddressOutOfRange,
            ),
            (
                FixtureBus::Spi { num_cs: 4 },
                Some(4),
                DtbBusError::ChipSelectOutOfRange,
            ),
            (
                FixtureBus::Spi { num_cs: 4 },
                None,
                DtbBusError::MissingChipSelect,
            ),
            (FixtureBus::Mdio, Some(32), DtbBusError::AddressOutOfRange),
        ];

        for (bus, reg, expected) in cases {
            let nodes = fixture_nodes(
                bus,
                &[FixtureChild {
                    name: "child@0",
                    reg,
                    enabled: true,
                }],
            );
            let node = direct_child(nodes, &alloc::format!("{}/child@0", bus.controller_path()));
            let count = match bus {
                FixtureBus::Spi { num_cs } => Some(num_cs),
                _ => None,
            };
            assert_eq!(
                child_descriptor(
                    handle(bus.kind()),
                    bus.controller_path().into(),
                    node,
                    count
                )
                .unwrap_err(),
                expected
            );
        }
    }

    fn owner_device(identity: &[u8]) -> Arc<PnpDevice> {
        let bus = BusType::new("dt-bus-test-owner");
        let info = DynamicPnpBusInfo::new(
            bus,
            "dt-bus-test-owner",
            "kernel.general.dt-bus-test-owner@1",
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        PnpDevice::new(
            PnpId::dynamic(bus, "kernel.general.dt-bus-test-owner@1", identity).unwrap(),
            "dt-bus-test-owner".into(),
            Box::new(info),
        )
        .unwrap()
    }

    struct CountingI2c {
        transfers: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl DtbI2cController for CountingI2c {
        fn transfer(
            &self,
            target: DtbI2cTarget,
            messages: &mut [DtbI2cMessage<'_>],
        ) -> Result<(), DtbBusOperationError> {
            assert_eq!(target.address, 0x50);
            assert!(!target.ten_bit);
            assert!(!target.own_slave);
            assert_eq!(messages.len(), 1);
            self.transfers.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    impl Drop for CountingI2c {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn registry_generation_topology_calls_and_removal_are_coherent() {
        let _lock = TEST_LOCK.lock();
        let bus = FixtureBus::I2c;
        let nodes = fixture_nodes(
            bus,
            &[FixtureChild {
                name: "sensor@50",
                reg: Some(0x50),
                enabled: true,
            }],
        );
        let owner = owner_device(b"registry-generation-topology");
        let transfers = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let controller: Arc<dyn DtbI2cController> = Arc::new(CountingI2c {
            transfers: Arc::clone(&transfers),
            drops: Arc::clone(&drops),
        });
        let firmware_nodes: Arc<[DtbNodeInfo]> = Arc::from(nodes.clone().into_boxed_slice());
        let first = register_controller_inner(
            &owner,
            bus.controller_path(),
            ControllerOps::I2c(controller),
            None,
            Arc::clone(&firmware_nodes),
        )
        .unwrap();
        let children = enumerate_controller_children_from_nodes(first, nodes.clone()).unwrap();
        assert_eq!(children.len(), 1);
        assert!(
            children[0]
                .device
                .parent()
                .is_some_and(|parent| Arc::ptr_eq(&parent, &owner))
        );
        assert_eq!(owner.children().len(), 1);
        let info = children[0]
            .device
            .info
            .as_any()
            .downcast_ref::<DynamicPnpBusInfo>()
            .unwrap();
        assert_eq!(info.bus_type(), DT_I2C_BUS);
        assert!(
            info.properties()
                .iter()
                .any(|property| property.name.as_ref() == "reg")
        );
        assert_eq!(
            child_descriptor_for_device(&children[0].device)
                .unwrap()
                .node()
                .path
                .as_ref(),
            "/soc/i2c@1000/sensor@50"
        );

        let target = acquire_target(&children[0].device).unwrap();
        let write = [0x12u8, 0x34];
        target
            .i2c_transfer(&mut [DtbI2cMessage::Write(&write)])
            .unwrap();
        assert_eq!(transfers.load(Ordering::Relaxed), 1);
        assert_eq!(unregister_controller(first), Err(DtbBusError::Busy));

        let call = begin_controller_call(first, DtbBusKind::I2c).unwrap();
        assert_eq!(remove_controller_children(first), Ok(1));
        assert!(owner.children().is_empty());
        assert_eq!(children[0].device.state(), PnpState::Gone);
        assert_eq!(unregister_controller(first), Err(DtbBusError::Busy));
        drop(call);
        unregister_controller(first).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(
            target.i2c_transfer(&mut [DtbI2cMessage::Write(&write)]),
            Err(DtbBusOperationError::DeviceGone)
        );

        let second = register_controller_inner(
            &owner,
            bus.controller_path(),
            ControllerOps::I2c(Arc::new(NullI2c)),
            None,
            firmware_nodes,
        )
        .unwrap();
        assert_ne!(second.generation(), first.generation());
        unregister_controller(second).unwrap();
    }

    #[test]
    fn duplicate_address_rolls_back_before_pnp_publication() {
        let _lock = TEST_LOCK.lock();
        let bus = FixtureBus::Mdio;
        let nodes = fixture_nodes(
            bus,
            &[
                FixtureChild {
                    name: "ethernet-phy@1",
                    reg: Some(1),
                    enabled: true,
                },
                FixtureChild {
                    name: "duplicate-phy@1",
                    reg: Some(1),
                    enabled: true,
                },
            ],
        );
        let owner = owner_device(b"duplicate-address-rollback");
        let firmware_nodes: Arc<[DtbNodeInfo]> = Arc::from(nodes.clone().into_boxed_slice());
        let handle = register_controller_inner(
            &owner,
            bus.controller_path(),
            ControllerOps::Mdio(Arc::new(NullMdio)),
            None,
            firmware_nodes,
        )
        .unwrap();
        assert_eq!(
            enumerate_controller_children_from_nodes(handle, nodes).err(),
            Some(DtbBusError::DuplicateAddress)
        );
        assert!(owner.children().is_empty());
        assert!(
            DT_BUS_CONTROLLERS
                .lock()
                .controllers
                .iter()
                .find(|entry| entry.handle == handle)
                .is_some_and(|entry| entry.children.is_empty() && !entry.enumerating)
        );
        unregister_controller(handle).unwrap();
    }
}

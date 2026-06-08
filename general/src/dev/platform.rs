//! 固件枚举的 platform 设备描述。
//!
//! DTB 与 ACPI 都会描述一类不挂在 PCI/USB 这类可枚举总线下的设备。启动路径
//! 将固件里的 compatible/HID、资源和少量属性转换成这里的中立结构，再交给 PnP
//! 注册表由 platform 驱动统一匹配和 probe。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use crate::dev::irq::{self, IrqLine};
use crate::dev::pnp::{
    BusType, PNP_DEVICES, PNP_DRIVERS, PnpBusInfo, PnpDevice, PnpError, PnpId, PnpState,
};

const PLATFORM_ID_FNV_OFFSET: u32 = 0x811c_9dc5;
const PLATFORM_ID_FNV_PRIME: u32 = 0x0100_0193;

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
    pub stdout: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirmwarePropertyValue {
    Bool,
    U32(u32),
    StringList(Box<[Box<str>]>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirmwareProperty {
    pub name: Box<str>,
    pub value: FirmwarePropertyValue,
}

#[derive(Debug)]
pub struct PlatformDeviceInfo {
    pub fw_name: Box<str>,
    pub ids: Vec<DeviceMatchId>,
    pub resources: Vec<DeviceResource>,
    pub properties: DeviceProperties,
    pub fw_properties: Vec<FirmwareProperty>,
}

impl PlatformDeviceInfo {
    pub fn has_id(&self, expected: &str) -> bool {
        self.ids.iter().any(|id| id.matches_str(expected))
    }

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
    pub fn mmio_at(&self, index: usize) -> Option<(usize, usize)> {
        self.mmio_resources().nth(index)
    }

    /// 按声明顺序遍历 IRQ 资源。
    pub fn irq_resources(&self) -> impl Iterator<Item = FirmwareIrqResource<'_>> + '_ {
        self.resources.iter().filter_map(DeviceResource::as_irq)
    }

    /// 按声明顺序返回第 `index` 个 IRQ 资源。
    pub fn irq_at(&self, index: usize) -> Option<FirmwareIrqResource<'_>> {
        self.irq_resources().nth(index)
    }

    pub fn has_irq_resource(&self) -> bool {
        self.irq_resources().next().is_some()
    }

    pub fn first_irq_line(&self) -> Option<IrqLine> {
        self.resolve_first_irq_line().ok()
    }

    pub fn resolve_irq_line_at(&self, index: usize) -> Result<IrqLine, PlatformIrqResolveError> {
        let irq = self
            .irq_at(index)
            .ok_or(PlatformIrqResolveError::NoResource)?;
        irq.resolve_line()
            .ok_or(PlatformIrqResolveError::Unresolved)
    }

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

    pub fn u32_property(&self, name: &str) -> Option<u32> {
        self.fw_properties
            .iter()
            .find(|property| property.name.as_ref() == name)
            .and_then(|property| match property.value {
                FirmwarePropertyValue::U32(value) => Some(value),
                FirmwarePropertyValue::Bool | FirmwarePropertyValue::StringList(_) => None,
            })
    }

    pub fn bool_property(&self, name: &str) -> bool {
        self.fw_properties.iter().any(|property| {
            property.name.as_ref() == name && matches!(property.value, FirmwarePropertyValue::Bool)
        })
    }

    pub fn string_list_property(&self, name: &str) -> Option<&[Box<str>]> {
        self.fw_properties
            .iter()
            .find(|property| property.name.as_ref() == name)
            .and_then(|property| match &property.value {
                FirmwarePropertyValue::StringList(values) => Some(values.as_ref()),
                FirmwarePropertyValue::Bool | FirmwarePropertyValue::U32(_) => None,
            })
    }

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

pub fn register_and_probe_platform_device(
    info: PlatformDeviceInfo,
) -> Result<PlatformRegistration, PnpError> {
    let name = info.fw_name.clone();
    let index = platform_instance_id(&info);
    let id = PnpId::Platform {
        name: name.clone(),
        index,
    };
    let new_dev = PnpDevice::new(id, name, Box::new(info));
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
                PNP_DEVICES.remove(&dev.id);
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
        Err(PnpError::ProbeDeferred) => Ok(PlatformRegistration {
            device: dev,
            status: PlatformProbeStatus::Deferred,
        }),
        Err(err) => {
            if inserted {
                PNP_DEVICES.remove(&dev.id);
            }
            Err(err)
        }
    }
}

fn platform_instance_id(info: &PlatformDeviceInfo) -> u32 {
    // Platform 设备没有 PCI BDF 这类天然地址，这里用固件名、match id 和资源 tuple
    // 生成稳定 instance。它只用于 PnP identity，不参与 `/dev`/POSIX 设备号投影。
    let mut hash = PLATFORM_ID_FNV_OFFSET;
    hash = fnv_mix_bytes(hash, info.fw_name.as_bytes());
    for id in &info.ids {
        match id {
            DeviceMatchId::DtbCompatible(value) => {
                hash = fnv_mix_u32(hash, 1);
                hash = fnv_mix_bytes(hash, value.as_bytes());
            }
            DeviceMatchId::AcpiHid(value) => {
                hash = fnv_mix_u32(hash, 2);
                hash = fnv_mix_bytes(hash, value.as_bytes());
            }
            DeviceMatchId::AcpiCid(value) => {
                hash = fnv_mix_u32(hash, 3);
                hash = fnv_mix_bytes(hash, value.as_bytes());
            }
        }
    }
    for resource in &info.resources {
        match resource {
            DeviceResource::Mmio { phys, size } => {
                hash = fnv_mix_u32(hash, 4);
                hash = fnv_mix_usize(hash, *phys);
                hash = fnv_mix_usize(hash, *size);
            }
            DeviceResource::Irq {
                controller,
                cells,
                attributes,
            } => {
                hash = fnv_mix_u32(hash, 5);
                hash = fnv_mix_u32(hash, controller.unwrap_or(0));
                for cell in cells.iter() {
                    hash = fnv_mix_u32(hash, *cell);
                }
                hash = fnv_mix_irq_attributes(hash, *attributes);
            }
        }
    }
    hash
}

fn fnv_mix_irq_attributes(mut hash: u32, attributes: IrqResourceAttributes) -> u32 {
    hash = fnv_mix_u32(
        hash,
        match attributes.trigger {
            None => 0,
            Some(IrqTrigger::Edge) => 1,
            Some(IrqTrigger::Level) => 2,
        },
    );
    hash = fnv_mix_u32(
        hash,
        match attributes.polarity {
            None => 0,
            Some(IrqPolarity::ActiveHigh) => 1,
            Some(IrqPolarity::ActiveLow) => 2,
        },
    );
    hash = fnv_mix_u32(
        hash,
        match attributes.sharing {
            None => 0,
            Some(IrqSharing::Exclusive) => 1,
            Some(IrqSharing::Shared) => 2,
        },
    );
    fnv_mix_u32(hash, if attributes.wake_capable { 1 } else { 0 })
}

fn fnv_mix_bytes(mut hash: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(PLATFORM_ID_FNV_PRIME);
    }
    hash
}

fn fnv_mix_u32(hash: u32, value: u32) -> u32 {
    fnv_mix_bytes(hash, &value.to_le_bytes())
}

fn fnv_mix_usize(hash: u32, value: usize) -> u32 {
    fnv_mix_bytes(hash, &value.to_le_bytes())
}

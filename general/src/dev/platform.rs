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
use crate::dev::pnp::{BusType, PNP_DEVICES, PNP_DRIVERS, PnpBusInfo, PnpDevice, PnpError, PnpId};

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
    },
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
        self.resources.iter().find_map(|resource| match resource {
            DeviceResource::Mmio { phys, size } => Some((*phys, *size)),
            DeviceResource::Irq { .. } => None,
        })
    }

    pub fn has_irq_resource(&self) -> bool {
        self.resources
            .iter()
            .any(|resource| matches!(resource, DeviceResource::Irq { .. }))
    }

    pub fn first_irq_line(&self) -> Option<IrqLine> {
        self.resources.iter().find_map(|resource| match resource {
            DeviceResource::Mmio { .. } => None,
            DeviceResource::Irq { controller, cells } => {
                irq::translate_firmware_irq(*controller, cells)
            }
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
    pub bound: bool,
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
    let dev = PnpDevice::new(id, name, Box::new(info));
    PNP_DEVICES.push(Arc::clone(&dev))?;

    match PNP_DRIVERS.probe_device(&dev) {
        Ok(()) => Ok(PlatformRegistration {
            device: dev,
            bound: true,
        }),
        Err(PnpError::NoDriver) => Ok(PlatformRegistration {
            device: dev,
            bound: false,
        }),
        Err(err) => {
            PNP_DEVICES.remove(&dev.id);
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
            DeviceResource::Irq { controller, cells } => {
                hash = fnv_mix_u32(hash, 5);
                hash = fnv_mix_u32(hash, controller.unwrap_or(0));
                for cell in cells.iter() {
                    hash = fnv_mix_u32(hash, *cell);
                }
            }
        }
    }
    hash
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

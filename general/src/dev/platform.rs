//! 固件枚举的 platform 设备描述。
//!
//! DTB 与 ACPI 都会描述一类不挂在 PCI/USB 这类可枚举总线下的设备。启动路径
//! 将固件里的 compatible/HID、资源和少量属性转换成这里的中立结构，再交给 PnP
//! 注册表由 platform 驱动统一匹配和 probe。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceResource {
    Mmio { phys: usize, size: usize },
    Irq(u32),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceProperties {
    pub clock_hz: Option<u32>,
    pub baud: Option<u32>,
    pub stdout: bool,
}

#[derive(Debug)]
pub struct PlatformDeviceInfo {
    pub fw_name: Box<str>,
    pub ids: Vec<DeviceMatchId>,
    pub resources: Vec<DeviceResource>,
    pub properties: DeviceProperties,
}

impl PlatformDeviceInfo {
    pub fn has_id(&self, expected: &str) -> bool {
        self.ids.iter().any(|id| id.matches_str(expected))
    }

    pub fn first_mmio(&self) -> Option<(usize, usize)> {
        self.resources.iter().find_map(|resource| match *resource {
            DeviceResource::Mmio { phys, size } => Some((phys, size)),
            DeviceResource::Irq(_) => None,
        })
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
        match *resource {
            DeviceResource::Mmio { phys, size } => {
                hash = fnv_mix_u32(hash, 4);
                hash = fnv_mix_usize(hash, phys);
                hash = fnv_mix_usize(hash, size);
            }
            DeviceResource::Irq(irq) => {
                hash = fnv_mix_u32(hash, 5);
                hash = fnv_mix_u32(hash, irq);
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

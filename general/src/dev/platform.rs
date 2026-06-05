//! 固件枚举的 platform 设备描述。
//!
//! DTB 与 ACPI 都会描述一类不挂在 PCI/USB 这类可枚举总线下的设备。启动路径
//! 将固件里的 compatible/HID、资源和少量属性转换成这里的中立结构，再交给 PnP
//! 注册表由 platform 驱动统一匹配和 probe。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::dev::pnp::{BusType, PNP_DEVICES, PNP_DRIVERS, PnpBusInfo, PnpDevice, PnpError, PnpId};

static NEXT_PLATFORM_INDEX: AtomicU32 = AtomicU32::new(0);

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
    let index = NEXT_PLATFORM_INDEX.fetch_add(1, Ordering::Relaxed);
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

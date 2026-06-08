//! 固件 platform bus 驱动。
//!
//! 该驱动匹配 `simple-bus`、`qemu,platform` 等只描述地址空间的节点，解析其
//! `ranges` 后登记到 [`crate::dev::firmware_bus`]。它不主动枚举子节点；DTB 解析器
//! 已经把子设备标准化成 platform PnP 设备，这里只保存总线本身的拓扑语义。

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::dev::firmware_bus::{
    self, FirmwareBus, FirmwareBusDescriptor, FirmwareBusError, FirmwareBusHandle, FirmwareBusRange,
};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    register_driver_factory,
};

const COMPAT_SIMPLE_BUS: &str = "simple-bus";
const COMPAT_SIMPLE_MFD: &str = "simple-mfd";
const COMPAT_QEMU_PLATFORM: &str = "qemu,platform";
const PROP_RANGES: &str = "ranges";

struct DtbFirmwareBus {
    descriptor: FirmwareBusDescriptor,
}

impl FirmwareBus for DtbFirmwareBus {
    fn descriptor(&self) -> &FirmwareBusDescriptor {
        &self.descriptor
    }
}

struct FirmwareBusBinding {
    handle: FirmwareBusHandle,
}

pub struct FirmwareBusPlatformDriver;

impl FirmwareBusPlatformDriver {
    const fn new() -> Self {
        Self
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_SIMPLE_BUS)
            || info.has_id(COMPAT_SIMPLE_MFD)
            || info.has_id(COMPAT_QEMU_PLATFORM)
    }
}

impl PnpDriver for FirmwareBusPlatformDriver {
    fn name(&self) -> &'static str {
        "platform-firmware-bus"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = platform_info(dev)?;
        let descriptor = firmware_bus_descriptor(info)?;
        let bus = Arc::new(DtbFirmwareBus { descriptor });
        let handle = firmware_bus::register(bus.clone()).map_err(map_firmware_bus_error)?;
        dev.set_driver_data(Arc::new(FirmwareBusBinding { handle }));
        log::printk!(
            "[firmware-bus] registered {} ranges={} dma-coherent={}",
            bus.descriptor().name.as_ref(),
            bus.descriptor().ranges.len(),
            bus.descriptor().dma_coherent as usize
        );
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<FirmwareBusBinding>()
        {
            let _ = firmware_bus::unregister(binding.handle);
        }
    }
}

fn map_firmware_bus_error(err: FirmwareBusError) -> PnpError {
    match err {
        FirmwareBusError::NotFound => PnpError::InvalidState,
        FirmwareBusError::OutOfMemory => PnpError::OutOfMemory,
    }
}

fn platform_info(dev: &Arc<PnpDevice>) -> Result<&PlatformDeviceInfo, PnpError> {
    dev.info
        .as_any()
        .downcast_ref::<PlatformDeviceInfo>()
        .ok_or(PnpError::InvalidState)
}

fn firmware_bus_descriptor(info: &PlatformDeviceInfo) -> Result<FirmwareBusDescriptor, PnpError> {
    let child_address_cells = info
        .properties
        .fw_address_cells
        .ok_or(PnpError::ProbeFailed)?;
    let child_size_cells = info.properties.fw_size_cells.ok_or(PnpError::ProbeFailed)?;
    let parent_address_cells = info
        .properties
        .fw_parent_address_cells
        .ok_or(PnpError::ProbeFailed)?;
    let ranges = parse_ranges(
        info.u32_list_property(PROP_RANGES).unwrap_or(&[]),
        child_address_cells as usize,
        parent_address_cells as usize,
        child_size_cells as usize,
    )?;
    Ok(FirmwareBusDescriptor {
        name: info.fw_name.clone(),
        phandle: info.properties.fw_phandle,
        child_address_cells,
        child_size_cells,
        parent_address_cells,
        ranges,
        dma_coherent: info.bool_property("dma-coherent"),
    })
}

fn parse_ranges(
    cells: &[u32],
    child_address_cells: usize,
    parent_address_cells: usize,
    size_cells: usize,
) -> Result<Vec<FirmwareBusRange>, PnpError> {
    if cells.is_empty() {
        return Ok(Vec::new());
    }
    let entry_cells = child_address_cells
        .checked_add(parent_address_cells)
        .and_then(|value| value.checked_add(size_cells))
        .ok_or(PnpError::ProbeFailed)?;
    if entry_cells == 0 || !cells.len().is_multiple_of(entry_cells) {
        return Err(PnpError::ProbeFailed);
    }

    let mut ranges = Vec::new();
    ranges
        .try_reserve(cells.len() / entry_cells)
        .map_err(|_| PnpError::OutOfMemory)?;
    for chunk in cells.chunks_exact(entry_cells) {
        let child_end = child_address_cells;
        let parent_end = child_end + parent_address_cells;
        let child_start = read_cells(&chunk[..child_end])?;
        let parent_start = usize::try_from(read_cells(&chunk[child_end..parent_end])?)
            .map_err(|_| PnpError::ProbeFailed)?;
        let size = usize::try_from(read_cells(&chunk[parent_end..])?)
            .map_err(|_| PnpError::ProbeFailed)?;
        if size == 0 || parent_start.checked_add(size).is_none() {
            return Err(PnpError::ProbeFailed);
        }
        ranges.push(FirmwareBusRange {
            child_start,
            parent_start,
            size,
        });
    }
    Ok(ranges)
}

fn read_cells(cells: &[u32]) -> Result<u128, PnpError> {
    if cells.len() > 4 {
        return Err(PnpError::ProbeFailed);
    }
    let mut value = 0u128;
    for cell in cells {
        value = (value << 32) | u128::from(*cell);
    }
    Ok(value)
}

struct FirmwareBusFactory;

impl DriverFactory for FirmwareBusFactory {
    fn name(&self) -> &'static str {
        "platform-firmware-bus"
    }

    fn create(&self, _ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(FirmwareBusPlatformDriver::new()))
    }
}

pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(FirmwareBusFactory)).map(|_| ())
}

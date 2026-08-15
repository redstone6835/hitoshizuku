//! Generic PCI CAM/ECAM platform ELM 驱动。

use alloc::sync::Arc;

use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};

const COMPAT_PCI_ECAM: &str = "pci-host-ecam-generic";
const COMPAT_PCIE_ECAM: &str = "pcie-host-ecam-generic";
const COMPAT_PCI_CAM: &str = "pci-host-cam-generic";
const COMPAT_LS2K1000_PCI: &str = "loongson,ls2k1000-pci";

struct GenericPciHostDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl GenericPciHostDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_PCI_ECAM)
            || info.has_id(COMPAT_PCIE_ECAM)
            || info.has_id(COMPAT_PCI_CAM)
            || info.has_id(COMPAT_LS2K1000_PCI)
    }
}

impl PnpDriver for GenericPciHostDriver {
    fn name(&self) -> &'static str {
        "platform-pci-host-ecam"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        if !matches!(id, PnpId::Platform { .. }) {
            return false;
        }
        info.as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let host = info.dtb_pcie_host().ok_or(PnpError::missing(
            PnpResourceKind::PciHostBridge,
            "normalized DT PCI host descriptor is missing",
        ))?;
        let _ = crate::runtime::probe_host(host, dev, self.device_mmio_to_virt)?;
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        let Some(info) = dev.info.as_any().downcast_ref::<PlatformDeviceInfo>() else {
            return;
        };
        let Some(host) = info.dtb_pcie_host() else {
            return;
        };
        // PnP core 在进入 remove 回调前已经深度优先移除所有 PCI function 子设备。
        crate::runtime::remove_host(host);
        log::printk!(
            "[pci-host-ecam] removed {} domain={} bus=[{:#x},{:#x}]",
            host.path,
            host.domain,
            host.bus_start,
            host.bus_end
        );
    }
}

struct GenericPciHostFactory;

impl DriverFactory for GenericPciHostFactory {
    fn name(&self) -> &'static str {
        "platform-pci-host-ecam"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(GenericPciHostDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(GenericPciHostFactory))
}

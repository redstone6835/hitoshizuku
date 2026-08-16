//! Synopsys DesignWare Ethernet MAC platform 驱动（最小实现）。
//!
//! 匹配 `starfive,jh7110-dwmac` / `snps,dwmac` 系列，记录 MMIO/IRQ 资源。
use alloc::sync::Arc;
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};

struct Driver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl Driver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self { device_mmio_to_virt }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
    info.has_id("starfive,jh7110-dwmac")
        ||     info.has_id("snps,dwmac-5.20")
        ||     info.has_id("snps,dwmac")
    }
}

struct Binding {
    phys: usize,
    size: usize,
    virt: usize,
    irq: Option<crate::dev::irq::IrqLine>,
}

impl PnpDriver for Driver {
    fn name(&self) -> &'static str {
        "bus-dwmac"
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
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;

        let (phys, size) = info
            .first_mmio()
            .ok_or(PnpError::missing(PnpResourceKind::Mmio, "missing controller reg"))?;
        let maybe_irq = info.first_irq_line();
        let virt = (self.device_mmio_to_virt)(phys);

        log::printk!(
            "[bus-dwmac] probe {} phys={:#x} size={:#x} virt={:#x} irq={:?}",
            dev.id,
            phys,
            size,
            virt,
            maybe_irq
        );

        dev.set_driver_data(Arc::new(Binding { phys, size, virt, irq: maybe_irq }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if dev.take_driver_data().is_some() {
            log::printk!("[bus-dwmac] removed {}", dev.id);
        }
    }
}

struct Factory;

impl DriverFactory for Factory {
    fn name(&self) -> &'static str {
        "bus-dwmac"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Driver::new(ctx.device_mmio_to_virt)))
    }
}

/// 注册内建驱动工厂，返回 `DriverHandle` 供上层管理。
pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(Factory))
}
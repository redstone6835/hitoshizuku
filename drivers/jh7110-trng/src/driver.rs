use alloc::sync::Arc;

use crate::dev::dt_provider::{DtbProviderError, DtbResourceLease, DtbResourceRequest};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use crate::engine::{Registers, TrngError, read_seed};

const COMPAT_JH7110_TRNG: &str = "starfive,jh7110-trng";
const REQUIRED_MMIO_SIZE: usize = 0x68;
const MAX_POLLS: usize = 1_000_000;

struct MmioRegisters {
    base: usize,
}

impl Registers for MmioRegisters {
    fn read32(&self, offset: usize) -> u32 {
        // Safety: probe 在构造对象前已校验设备树提供的 MMIO 窗口。
        unsafe { core::ptr::read_volatile(self.base.wrapping_add(offset) as *const u32) }
    }

    fn write32(&self, offset: usize, value: u32) {
        // Safety: 与 read32 使用同一个已校验的 MMIO 窗口。
        unsafe { core::ptr::write_volatile(self.base.wrapping_add(offset) as *mut u32, value) }
    }

    fn relax(&self) {
        core::hint::spin_loop();
    }
}

struct Jh7110TrngBinding;

struct Jh7110TrngDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl Jh7110TrngDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_JH7110_TRNG)
    }

    fn acquire_resource(
        info: &PlatformDeviceInfo,
        property: &str,
        name: Option<&str>,
        index: usize,
    ) -> Result<DtbResourceLease, PnpError> {
        let result = name
            .and_then(|name| info.dtb_reference_by_name(property, name))
            .map(|reference| crate::dev::dt_provider::acquire_reference(reference))
            .unwrap_or_else(|| info.acquire_dtb_resource_at(property, index));
        result.map_err(DtbProviderError::into_pnp_error)
    }

    fn enable_clock(clock: &DtbResourceLease) -> Result<(), PnpError> {
        clock
            .control(DtbResourceRequest::Enable)
            .map(|_| ())
            .map_err(DtbProviderError::into_pnp_error)
    }

    fn deassert_reset(reset: &DtbResourceLease) -> Result<(), PnpError> {
        match reset.control(DtbResourceRequest::Deassert) {
            Ok(_) => Ok(()),
            Err(DtbProviderError::UnsupportedOperation) => reset
                .control(DtbResourceRequest::Enable)
                .map(|_| ())
                .map_err(DtbProviderError::into_pnp_error),
            Err(error) => Err(error.into_pnp_error()),
        }
    }

    fn disable_clock(clock: &DtbResourceLease) {
        let _ = clock.control(DtbResourceRequest::Disable);
    }
}

impl PnpDriver for Jh7110TrngDriver {
    fn name(&self) -> &'static str {
        "platform-jh7110-trng"
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
            .ok_or(PnpError::missing(PnpResourceKind::Mmio, "trng reg missing"))?;
        if size < REQUIRED_MMIO_SIZE {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "trng reg window too small",
            ));
        }

        let hclk = Self::acquire_resource(info, "clocks", Some("hclk"), 0)?;
        let ahb = Self::acquire_resource(info, "clocks", Some("ahb"), 1)?;
        let reset = Self::acquire_resource(info, "resets", None, 0)?;

        Self::enable_clock(&hclk)?;
        if let Err(error) = Self::enable_clock(&ahb) {
            Self::disable_clock(&hclk);
            return Err(error);
        }
        if let Err(error) = Self::deassert_reset(&reset) {
            Self::disable_clock(&ahb);
            Self::disable_clock(&hclk);
            return Err(error);
        }

        let registers = MmioRegisters {
            base: (self.device_mmio_to_virt)(phys),
        };
        let seed = read_seed(&registers, MAX_POLLS).map_err(|error| {
            Self::disable_clock(&ahb);
            Self::disable_clock(&hclk);
            match error {
                TrngError::Timeout => PnpError::hardware_failure("trng operation timed out"),
                TrngError::Lockup => PnpError::hardware_failure("trng LFSR lockup"),
            }
        })?;

        general::dev::random::add_bootloader_randomness(&seed);
        Self::disable_clock(&ahb);
        Self::disable_clock(&hclk);

        log::printk!(
            "[jh7110-trng] bound {} phys={:#x} credited=256 bits",
            dev.id,
            phys,
        );
        dev.set_driver_data(Arc::new(Jh7110TrngBinding));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if dev.take_driver_data().is_some() {
            log::printk!("[jh7110-trng] removed {}", dev.id);
        }
    }
}

struct Jh7110TrngFactory;

impl DriverFactory for Jh7110TrngFactory {
    fn name(&self) -> &'static str {
        "platform-jh7110-trng"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Jh7110TrngDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(Jh7110TrngFactory))
}

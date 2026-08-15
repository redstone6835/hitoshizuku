//! LS2K1000 时钟控制器的设备树提供方驱动。
//!
//! 固件把 `reg` 指向 SYS0 PLL，并通过 one-cell specifier 暴露各路时钟。
//! 驱动只读取已确认的 PLL 与分频寄存器，不修改时钟树或猜测无效编码。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::mem::size_of;
use core::ptr::read_volatile;

use crate::dev::dt_provider::{
    self, DtbProvider, DtbProviderError, DtbProviderKey, DtbProviderKind, DtbResource,
    DtbResourceLease, DtbResourceReply, DtbResourceRequest,
};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResource, PnpResourceKind, PnpResourceReleaseError,
    PnpResourceReleaseOrder, register_driver_factory,
};
use crate::layout::{
    ClockId, Ls2kClockMmioLayout, Ls2kClockRegisters, Ls2kClockSnapshot, clock_id_from_specifier,
};

const COMPAT_LOONGSON_LS2X_CLOCK: &str = "loongson,ls2x-clk";
const PROP_CLOCK_CELLS: &str = "#clock-cells";
const PROP_CLOCKS: &str = "clocks";
const CLOCK_RESOURCE_KIND: PnpResourceKind = PnpResourceKind::Other("clock");

struct UpstreamClock {
    lease: DtbResourceLease,
}

impl UpstreamClock {
    fn done(&self, request: DtbResourceRequest<'_>) -> Result<DtbResourceReply, DtbProviderError> {
        match self.lease.control(request)? {
            DtbResourceReply::Done => Ok(DtbResourceReply::Done),
            _ => Err(DtbProviderError::HardwareFailure),
        }
    }

    fn rate(&self) -> Result<u64, DtbProviderError> {
        match self.lease.control(DtbResourceRequest::GetRate)? {
            DtbResourceReply::Value(rate) if rate != 0 => Ok(rate),
            _ => Err(DtbProviderError::HardwareFailure),
        }
    }
}

struct SharedUpstreamClockPnpResource {
    upstream: Option<Arc<UpstreamClock>>,
}

impl SharedUpstreamClockPnpResource {
    fn new(upstream: Arc<UpstreamClock>) -> Self {
        Self {
            upstream: Some(upstream),
        }
    }
}

impl PnpResource for SharedUpstreamClockPnpResource {
    fn kind(&self) -> PnpResourceKind {
        PnpResourceKind::Other("dt-provider-lease")
    }

    fn label(&self) -> &'static str {
        "ls2x-parent-clock"
    }

    fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
        self.upstream
            .as_ref()
            .ok_or_else(|| {
                PnpResourceReleaseError::new(
                    self.kind(),
                    self.label(),
                    "upstream clock lease was already released",
                )
            })?
            .lease
            .prepare_pnp_release()
            .map_err(|_| {
                PnpResourceReleaseError::new(
                    self.kind(),
                    self.label(),
                    "upstream clock lease cannot be frozen",
                )
            })
    }

    fn cancel_release(&self) {
        if let Some(upstream) = self.upstream.as_ref() {
            upstream.lease.cancel_pnp_release();
        }
    }

    fn release_order(&self) -> PnpResourceReleaseOrder {
        PnpResourceReleaseOrder::Consumer
    }

    fn release(mut self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        drop(self.upstream.take());
        Ok(())
    }
}

struct Ls2kClockHardware {
    registers: Ls2kClockRegisters,
}

impl Ls2kClockHardware {
    fn snapshot(&self) -> Ls2kClockSnapshot {
        let registers = self.registers;
        // Safety: 探测阶段已验证每个地址的 8 字节对齐和硬件窗口，映射由
        // `device_mmio_to_virt` 建立；这些寄存器是只读采样，不产生写入副作用。
        unsafe {
            Ls2kClockSnapshot {
                sys0: read_volatile(registers.sys0 as *const u64),
                sys1: read_volatile(registers.sys1 as *const u64),
                ddr0: read_volatile(registers.ddr0 as *const u64),
                ddr1: read_volatile(registers.ddr1 as *const u64),
                dc0: read_volatile(registers.dc0 as *const u64),
                dc1: read_volatile(registers.dc1 as *const u64),
                pix00: read_volatile(registers.pix00 as *const u64),
                pix01: read_volatile(registers.pix01 as *const u64),
                pix10: read_volatile(registers.pix10 as *const u64),
                pix11: read_volatile(registers.pix11 as *const u64),
                freq_scale: read_volatile(registers.freq_scale as *const u64),
            }
        }
    }
}

struct Ls2kClockResource {
    hardware: Arc<Ls2kClockHardware>,
    upstream: Arc<UpstreamClock>,
    id: ClockId,
}

impl DtbResource for Ls2kClockResource {
    fn control(
        &self,
        request: DtbResourceRequest<'_>,
    ) -> Result<DtbResourceReply, DtbProviderError> {
        match request {
            DtbResourceRequest::Enable => self.upstream.done(DtbResourceRequest::Enable),
            DtbResourceRequest::Disable => self.upstream.done(DtbResourceRequest::Disable),
            DtbResourceRequest::GetRate => self
                .hardware
                .snapshot()
                .rate(self.id, self.upstream.rate()?)
                .map(DtbResourceReply::Value)
                .ok_or(DtbProviderError::HardwareFailure),
            _ => Err(DtbProviderError::UnsupportedOperation),
        }
    }
}

struct Ls2kClockProvider {
    hardware: Arc<Ls2kClockHardware>,
    upstream: Arc<UpstreamClock>,
}

impl DtbProvider for Ls2kClockProvider {
    fn acquire(&self, specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError> {
        let id = clock_id_from_specifier(specifier).map_err(|_| DtbProviderError::AcquireFailed)?;
        if id == ClockId::I2sMclk {
            return Err(DtbProviderError::UnsupportedOperation);
        }
        Ok(Arc::new(Ls2kClockResource {
            hardware: Arc::clone(&self.hardware),
            upstream: Arc::clone(&self.upstream),
            id,
        }))
    }
}

struct Ls2kClockDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl Ls2kClockDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_LOONGSON_LS2X_CLOCK)
    }
}

impl PnpDriver for Ls2kClockDriver {
    fn name(&self) -> &'static str {
        "platform-loongson-ls2x-clock"
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
        validate_clock_cells(info)?;
        let phandle = info.properties.fw_phandle.ok_or(PnpError::missing(
            CLOCK_RESOURCE_KIND,
            "ls2x clock provider is missing a phandle",
        ))?;

        let mut windows = info.mmio_resources();
        let (phys, size) = windows.next().ok_or(PnpError::missing(
            PnpResourceKind::Mmio,
            "ls2x clock register window missing",
        ))?;
        if windows.next().is_some() {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "ls2x clock requires exactly one register window",
            ));
        }
        Ls2kClockMmioLayout::new(phys, size).map_err(|_| {
            PnpError::malformed(PnpResourceKind::Mmio, "invalid ls2x clock register window")
        })?;
        let virt = (self.device_mmio_to_virt)(phys);
        let registers = Ls2kClockMmioLayout::new(virt, size)
            .map_err(|_| PnpError::hardware_failure("invalid ls2x clock MMIO mapping"))?
            .registers();

        validate_parent_clock(info, phandle)?;
        dev.reserve_owned_resources(2)?;
        let upstream = Arc::new(UpstreamClock {
            lease: info
                .acquire_dtb_resource_at(PROP_CLOCKS, 0)
                .map_err(DtbProviderError::into_pnp_error)?,
        });
        let hardware = Arc::new(Ls2kClockHardware { registers });

        // 父 lease 先登记，自身 provider 后登记。PnP 先冻结并释放 consumer 侧引用，
        // 提供方自身仍持有同一 Arc，直到提供方注销后才最终释放父时钟。
        dev.own_resource(SharedUpstreamClockPnpResource::new(Arc::clone(&upstream)))?;
        let key = DtbProviderKey::new(DtbProviderKind::Clock, phandle);
        let handle = dt_provider::register(key, Arc::new(Ls2kClockProvider { hardware, upstream }))
            .map_err(DtbProviderError::into_pnp_error)?;
        if let Err(error) = dev.own_resource(dt_provider::provider_pnp_resource(
            handle,
            "loongson-ls2x-clock-provider",
        )) {
            let _ = dt_provider::unregister(handle);
            return Err(error);
        }

        log::printk!(
            "[loongson-clk] bound {} phandle={:#x} phys={:#x}",
            dev.name,
            phandle,
            phys
        );
        Ok(())
    }

    fn remove(&self, _dev: &Arc<PnpDevice>) {}
}

fn platform_info(dev: &Arc<PnpDevice>) -> Result<&PlatformDeviceInfo, PnpError> {
    dev.info
        .as_any()
        .downcast_ref::<PlatformDeviceInfo>()
        .ok_or(PnpError::InvalidState)
}

fn validate_clock_cells(info: &PlatformDeviceInfo) -> Result<(), PnpError> {
    let raw = info
        .bytes_property(PROP_CLOCK_CELLS)
        .ok_or(PnpError::missing(
            CLOCK_RESOURCE_KIND,
            "ls2x clock is missing #clock-cells",
        ))?;
    if raw.len() != size_of::<u32>() || info.u32_property(PROP_CLOCK_CELLS) != Some(1) {
        return Err(PnpError::malformed(
            CLOCK_RESOURCE_KIND,
            "ls2x clock #clock-cells must be one",
        ));
    }
    Ok(())
}

fn validate_parent_clock(info: &PlatformDeviceInfo, phandle: u32) -> Result<(), PnpError> {
    let mut parents = info.dtb_references(PROP_CLOCKS);
    let parent = parents.next().ok_or(PnpError::missing(
        CLOCK_RESOURCE_KIND,
        "ls2x clock is missing its reference clock",
    ))?;
    if parents.next().is_some() || parent.phandle == 0 || parent.phandle == phandle {
        return Err(PnpError::malformed(
            CLOCK_RESOURCE_KIND,
            "ls2x clock must reference exactly one external parent clock",
        ));
    }
    Ok(())
}

struct Ls2kClockFactory;

impl DriverFactory for Ls2kClockFactory {
    fn name(&self) -> &'static str {
        "platform-loongson-ls2x-clock"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Ls2kClockDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(Ls2kClockFactory))
}

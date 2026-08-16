//! StarFive JH7110 Clock/Reset (CRG) 平台驱动。
//!
//! 以设备树引用的 #clock-cells=<1> 时钟 ID 为键，提供 GetRate / Enable /
//! Disable 语义的 clock provider，并附带 no-op reset provider（JH7110 由
//! U-Boot 保持外设复位释放与时钟门控开启，内核阶段不主动改动，避免破坏控制台）。
//!
//! 时钟速率表来自板载 Debian 6.12 内核 /sys/kernel/debug/clk/clk_summary 实测
//! 数据，与 Linux drivers/clk/starfive/clk-starfive-jh7110-sys.c 的树结构一致：
//! 关键数值：OSC=24 MHz、UART0_CORE(146)=24 MHz、APB=49.5 MHz、
//! SDIO ciu(93/94)=49.5 MHz、SDIO biu(91/92)=198 MHz、GMAC AXI/AHB=198 MHz。

use alloc::sync::Arc;
use crate::dev::dt_provider::{
    self, DtbProvider, DtbProviderError, DtbProviderKey, DtbProviderKind, DtbResource,
    DtbResourceReply, DtbResourceRequest,
};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};

const COMPAT_JH7110_SYSCRG: &str = "starfive,jh7110-syscrg";
const COMPAT_JH7110_STGCRG: &str = "starfive,jh7110-stgcrg";
const COMPAT_JH7110_AONCRG: &str = "starfive,jh7110-aoncrg";

/// 外部 OSC 时钟 ID（JH7110_SYSCLK_END + 0）。
const SYSCLK_OSC: u32 = 190;

/// (clock id, rate Hz) 静态表；来源见模块文档。
const SYSCRG_RATES: &[(u32, u64)] = &[
    (0, 1_500_000_000),   // CPU_ROOT
    (4, 594_000_000),     // PERH_ROOT
    (7, 396_000_000),     // AXI_CFG0
    (8, 198_000_000),     // STG_AXIAHB
    (9, 198_000_000),     // AHB0
    (10, 198_000_000),    // AHB1
    (11, 49_500_000),     // APB_BUS
    (12, 49_500_000),     // APB0
    (40, 12_000_000),     // OSC_DIV2
    (87, 198_000_000),    // QSPI_AHB
    (88, 49_500_000),     // QSPI_APB
    (91, 198_000_000),    // SDIO0_AHB  biu
    (92, 198_000_000),    // SDIO1_AHB  biu
    (93, 49_500_000),     // SDIO0_SDCARD ciu
    (94, 49_500_000),     // SDIO1_SDCARD ciu
    (97, 198_000_000),    // GMAC1_AHB
    (98, 198_000_000),    // GMAC1_AXI
    (112, 49_500_000),    // IOMUX_APB
    (113, 49_500_000),    // MAILBOX_APB
    (121, 49_500_000),    // PWM_APB
    (122, 49_500_000),    // WDT_APB
    (124, 49_500_000),    // TIMER_APB
    (129, 49_500_000),    // TEMP_APB
    (131, 49_500_000),    // SPI0_APB
    (132, 49_500_000),
    (133, 49_500_000),
    (134, 49_500_000),
    (135, 49_500_000),
    (136, 49_500_000),
    (137, 49_500_000),
    (138, 49_500_000),    // I2C0_APB
    (139, 49_500_000),
    (140, 49_500_000),
    (141, 49_500_000),
    (142, 49_500_000),
    (143, 49_500_000),
    (144, 49_500_000),
    (145, 49_500_000),    // UART0_APB
    (146, 24_000_000),    // UART0_CORE (baudclk)
    (147, 49_500_000),
    (148, 24_000_000),
    (149, 49_500_000),
    (150, 24_000_000),
    (151, 49_500_000),
    (152, 59_400_000),    // UART3_CORE (perh_root gdiv)
    (153, 49_500_000),
    (154, 59_400_000),
    (155, 49_500_000),
    (156, 59_400_000),
    (SYSCLK_OSC, 24_000_000),
];

/// AON 域常见时钟（RTC/GMAC0 等）。
const AONCRG_RATES: &[(u32, u64)] = &[
    (0, 32_768),          // RTC_OSC
    (1, 24_000_000),      // OSC
    (2, 198_000_000),     // GMAC0_AHB
    (3, 198_000_000),     // GMAC0_AXI
];

fn table_rate(table: &'static [(u32, u64)], id: u32) -> Option<u64> {
    table.iter().find(|(clock_id, _)| *clock_id == id).map(|(_, rate)| *rate)
}

/// 单个时钟的资源视图。
struct JhClockResource {
    /// None 表示该时钟 ID 无静态速率（GetRate 报错，Enable/Disable 仍可接受）。
    rate: Option<u64>,
}

impl DtbResource for JhClockResource {
    fn control(&self, request: DtbResourceRequest<'_>) -> Result<DtbResourceReply, DtbProviderError> {
        match request {
            DtbResourceRequest::Enable | DtbResourceRequest::Disable => {
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::GetRate => self
                .rate
                .map(DtbResourceReply::Value)
                .ok_or(DtbProviderError::UnsupportedOperation),
            _ => Err(DtbProviderError::UnsupportedOperation),
        }
    }
}

/// 按 clock-id 查表的 provider。
struct JhClockProvider {
    table: &'static [(u32, u64)],
    osc_override: Option<u64>,
}

impl DtbProvider for JhClockProvider {
    fn acquire(&self, specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError> {
        let id = *specifier.first().ok_or(DtbProviderError::AcquireFailed)?;
        // clock-frequency 覆盖 OSC 及其直系门控（UART CORE），用于测试环境。
        let rate = match (self.osc_override, id) {
            (Some(osc), 146 | 148 | 150 | SYSCLK_OSC) => Some(osc),
            _ => table_rate(self.table, id),
        };
        Ok(Arc::new(JhClockResource { rate }))
    }
}

/// no-op 复位资源：JH7110 复位线由固件保持释放。
struct JhNoopResetResource;

impl DtbResource for JhNoopResetResource {
    fn control(&self, request: DtbResourceRequest<'_>) -> Result<DtbResourceReply, DtbProviderError> {
        match request {
            DtbResourceRequest::Enable | DtbResourceRequest::Disable => {
                Ok(DtbResourceReply::Done)
            }
            _ => Err(DtbProviderError::UnsupportedOperation),
        }
    }
}

struct JhNoopResetProvider;

impl DtbProvider for JhNoopResetProvider {
    fn acquire(&self, _specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError> {
        Ok(Arc::new(JhNoopResetResource))
    }
}

struct JhCrgDriver {
    osc_override: Option<u64>,
}

impl JhCrgDriver {
    const fn new(osc_override: Option<u64>) -> Self {
        Self { osc_override }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_JH7110_SYSCRG)
            || info.has_id(COMPAT_JH7110_STGCRG)
            || info.has_id(COMPAT_JH7110_AONCRG)
    }

    fn rate_table(info: &PlatformDeviceInfo) -> &'static [(u32, u64)] {
        if info.has_id(COMPAT_JH7110_AONCRG) {
            AONCRG_RATES
        } else {
            SYSCRG_RATES
        }
    }
}

impl PnpDriver for JhCrgDriver {
    fn name(&self) -> &'static str { "platform-jh7110-crg" }

    fn bus_type(&self) -> BusType { BusType::PLATFORM }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info.as_any().downcast_ref::<PlatformDeviceInfo>().is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &alloc::sync::Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let phandle = info.properties.fw_phandle.ok_or(PnpError::missing(
            PnpResourceKind::Other("clock"),
            "crg node missing phandle",
        ))?;

        let table = Self::rate_table(info);
        // 节点上的 clock-frequency 属性覆盖 OSC 基准（测试环境用）。
        let osc_override = info
            .u32_property("clock-frequency")
            .map(u64::from)
            .filter(|rate| *rate != 0);
        dev.reserve_owned_resources(2)?;

        let clock_key = DtbProviderKey::new(DtbProviderKind::Clock, phandle);
        let clock_handle = dt_provider::register(
            clock_key,
            Arc::new(JhClockProvider { table, osc_override }),
        )
        .map_err(DtbProviderError::into_pnp_error)?;
        if let Err(err) = dev.own_resource(dt_provider::provider_pnp_resource(
            clock_handle,
            "jh7110-crg-clock",
        )) {
            let _ = dt_provider::unregister(clock_handle);
            return Err(err);
        }

        let reset_key = DtbProviderKey::new(DtbProviderKind::Reset, phandle);
        let reset_handle = dt_provider::register(reset_key, Arc::new(JhNoopResetProvider))
            .map_err(DtbProviderError::into_pnp_error)?;
        if let Err(err) = dev.own_resource(dt_provider::provider_pnp_resource(
            reset_handle,
            "jh7110-crg-reset",
        )) {
            let _ = dt_provider::unregister(reset_handle);
            let _ = dt_provider::unregister(clock_handle);
            return Err(err);
        }

        log::printk!("[jh7110-crg] registered clock+reset phandle={:#x} rates={}", phandle, table.len());
        Ok(())
    }

    fn remove(&self, _dev: &alloc::sync::Arc<PnpDevice>) {
        log::printk!("[jh7110-crg] removed");
    }
}

struct JhCrgFactory;

impl DriverFactory for JhCrgFactory {
    fn name(&self) -> &'static str { "platform-jh7110-crg" }

    fn create(&self, _ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(JhCrgDriver::new(None)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(JhCrgFactory))
}
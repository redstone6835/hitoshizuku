//! Loongson LS2X 看门狗 platform ELM 驱动。
//!
//! 匹配 2K1000LA 板工厂 DTB 的 `loongson,ls2x-wdt` 节点（reg
//! 0x1fe27030 窗口 0xc，clocks=<&clk 12> APB 时钟）。
//!
//! 寄存器布局（与龙芯 2K1000 详细设计 WDT 模块一致）：
//! - WDT_EN  @ +0x0：bit0 使能。板级 syscon-reboot 向该寄存器写 1 实现复位
//!   （使能后倒计数从 TMR 初值立即下溢 → 复位信号）；
//! - WDT_TMR @ +0x4：32 位倒计时初值（RW），超时 = TMR / clk_hz；写入
//!   0xFFFFFFFF（-1）使定时器停摆、看门狗不工作；
//! - WDT_CNT @ +0x8：当前倒计数值（RO，诊断用）。
//!
//! 驱动把硬件能力实现为 [`general::dev::wdt::WdtDriver`]，并以
//! [`general::dev::wdt::WdtFunction`] 注册到 platform 设备上。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::dev::dt_provider::{
    DtbProviderError, DtbResourceLease, DtbResourceReply, DtbResourceRequest,
};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use crate::dev::wdt::{WdtDevice, WdtDriver, WdtError, WdtFunction};

const COMPAT_LOONGSON_LS2X_WDT: &str = "loongson,ls2x-wdt";

const PROP_CLOCKS: &str = "clocks";

// WDT 寄存器偏移（相对 0x1fe27030）。
const WDT_EN_REG: usize = 0x0;
const WDT_TMR_REG: usize = 0x4;
const WDT_CNT_REG: usize = 0x8;
/// 定时器寄存器写 -1 时看门狗停摆。
const WDT_TMR_DISABLED: u32 = 0xffff_ffff;
const WDT_EN_BIT: u32 = 1 << 0;
/// 驱动需要访问的最小寄存器窗口（覆盖 WDT_CNT）。
const MIN_REG_SIZE: usize = WDT_CNT_REG + core::mem::size_of::<u32>();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ls2xWdtError {
    RegisterWindowTooSmall,
    Overflow,
}

pub struct Ls2xWdt {
    base: usize,
    size: usize,
    clk_hz: u64,
    running: AtomicBool,
    timeout_secs: AtomicU32,
}

impl Ls2xWdt {
    pub const fn new(base: usize, size: usize, clk_hz: u64) -> Self {
        Self {
            base,
            size,
            clk_hz,
            running: AtomicBool::new(false),
            timeout_secs: AtomicU32::new(0),
        }
    }

    /// 硬件可表达的最大超时（秒）：32 位 TMR 在 clk_hz 下最多约 34 秒（125 MHz）。
    pub fn max_timeout_secs(&self) -> u32 {
        if self.clk_hz == 0 {
            return 0;
        }
        (u32::MAX as u64 / self.clk_hz) as u32
    }

    fn count_for(&self, secs: u32) -> Result<u32, Ls2xWdtError> {
        let ticks = (secs as u64)
            .checked_mul(self.clk_hz)
            .ok_or(Ls2xWdtError::Overflow)?;
        Ok(ticks.min(u32::MAX as u64) as u32)
    }

    fn write_timer(&self, secs: u32) -> Result<(), Ls2xWdtError> {
        let count = self.count_for(secs)?;
        self.write32(WDT_TMR_REG, count)
    }

    fn ensure_window(&self) -> Result<(), Ls2xWdtError> {
        if self.size != 0 && self.size < MIN_REG_SIZE {
            return Err(Ls2xWdtError::RegisterWindowTooSmall);
        }
        Ok(())
    }

    fn read32(&self, offset: usize) -> Result<u32, Ls2xWdtError> {
        self.ensure_window()?;
        let addr = self
            .base
            .checked_add(offset)
            .ok_or(Ls2xWdtError::Overflow)?;
        // Safety: `ensure_window` 已校验窗口覆盖该偏移；基址由 platform probe
        // 完成映射，寄存器是 32 位对齐 MMIO。
        Ok(unsafe { core::ptr::read_volatile(addr as *const u32) })
    }

    fn write32(&self, offset: usize, value: u32) -> Result<(), Ls2xWdtError> {
        self.ensure_window()?;
        let addr = self
            .base
            .checked_add(offset)
            .ok_or(Ls2xWdtError::Overflow)?;
        // Safety: 安全条件与 `read32` 相同，目标寄存器允许 32 位易失写入。
        unsafe { core::ptr::write_volatile(addr as *mut u32, value) };
        Ok(())
    }
}

impl WdtDriver for Ls2xWdt {
    fn timeout_secs(&self) -> u32 {
        self.timeout_secs.load(Ordering::Acquire)
    }

    fn max_timeout_secs(&self) -> u32 {
        self.max_timeout_secs()
    }

    fn set_timeout(&self, secs: u32) -> Result<u32, WdtError> {
        let actual = secs.min(self.max_timeout_secs());
        self.write_timer(actual).map_err(map_ls2x_wdt_error)?;
        self.timeout_secs.store(actual, Ordering::Release);
        Ok(actual)
    }

    fn start(&self) -> Result<(), WdtError> {
        // 先装载倒计时初值再使能，避免使能后立即从陈旧/零初值下溢。
        self.write_timer(self.timeout_secs())
            .map_err(map_ls2x_wdt_error)?;
        let mut en = self.read32(WDT_EN_REG).map_err(map_ls2x_wdt_error)?;
        en |= WDT_EN_BIT;
        self.write32(WDT_EN_REG, en).map_err(map_ls2x_wdt_error)?;
        self.running.store(true, Ordering::Release);
        Ok(())
    }

    fn stop(&self) -> Result<(), WdtError> {
        let mut en = self.read32(WDT_EN_REG).map_err(map_ls2x_wdt_error)?;
        en &= !WDT_EN_BIT;
        self.write32(WDT_EN_REG, en).map_err(map_ls2x_wdt_error)?;
        // 把定时器停摆在 -1，与设计文档“定时器值为 -1 时看门狗不工作”一致。
        self.write32(WDT_TMR_REG, WDT_TMR_DISABLED)
            .map_err(map_ls2x_wdt_error)?;
        self.running.store(false, Ordering::Release);
        Ok(())
    }

    fn ping(&self) -> Result<(), WdtError> {
        self.write_timer(self.timeout_secs())
            .map_err(map_ls2x_wdt_error)
    }

    fn running(&self) -> bool {
        match self.read32(WDT_EN_REG) {
            Ok(en) => en & WDT_EN_BIT != 0,
            Err(_) => self.running.load(Ordering::Acquire),
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn map_ls2x_wdt_error(err: Ls2xWdtError) -> WdtError {
    match err {
        Ls2xWdtError::RegisterWindowTooSmall | Ls2xWdtError::Overflow => WdtError::Invalid,
    }
}

struct Ls2xWdtBinding {
    wdt: Arc<Ls2xWdt>,
    wdt_dev: Arc<WdtDevice>,
    clock: Option<DtbResourceLease>,
}

pub struct Ls2xWdtPlatformDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl Ls2xWdtPlatformDriver {
    pub const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_LOONGSON_LS2X_WDT)
    }

    fn acquire_clock(
        &self,
        info: &PlatformDeviceInfo,
    ) -> Result<Option<(DtbResourceLease, u64)>, PnpError> {
        let clock = match info.acquire_dtb_resource_at(PROP_CLOCKS, 0) {
            Ok(lease) => lease,
            // 固件没有 clocks 属性时回退到标准 clock-frequency 属性
            // （SylixOS 风格的 2K1000 设备树）；两者都没有则无法换算超时。
            Err(DtbProviderError::Disabled | DtbProviderError::Invalid) => {
                return match info.properties.clock_hz {
                    Some(hz) if hz != 0 => Ok(None),
                    _ => Err(PnpError::hardware_failure("wdt has no clock rate source")),
                };
            }
            Err(error) => return Err(error.into_pnp_error()),
        };
        clock
            .control(DtbResourceRequest::Enable)
            .map_err(DtbProviderError::into_pnp_error)?;
        let rate = match clock
            .control(DtbResourceRequest::GetRate)
            .map_err(DtbProviderError::into_pnp_error)?
        {
            DtbResourceReply::Value(hz) => hz,
            _ => return Err(PnpError::hardware_failure("wdt clock has no rate")),
        };
        if rate == 0 {
            return Err(PnpError::hardware_failure("wdt clock rate is zero"));
        }
        Ok(Some((clock, rate)))
    }
}

impl PnpDriver for Ls2xWdtPlatformDriver {
    fn name(&self) -> &'static str {
        "platform-ls2x-wdt"
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
        let Some((phys, size)) = info.first_mmio() else {
            return Err(PnpError::missing(PnpResourceKind::Mmio, "wdt reg missing"));
        };
        let (clock, clk_hz) = match self.acquire_clock(info)? {
            Some((lease, hz)) => (Some(lease), hz),
            None => (None, u64::from(info.properties.clock_hz.unwrap_or(0))),
        };
        let wdt = Arc::new(Ls2xWdt::new((self.device_mmio_to_virt)(phys), size, clk_hz));
        if wdt.max_timeout_secs() == 0 {
            return Err(PnpError::hardware_failure("wdt clock rate too large"));
        }
        let wdt_driver: Arc<dyn WdtDriver> = wdt.clone();
        let projection_name = WdtDevice::alloc_stable_projection_name(&dev.name)
            .map_err(|_| PnpError::OutOfMemory)?;
        let wdt_dev = Arc::new(WdtDevice::new(projection_name, wdt_driver));
        dev.register_function(WdtFunction::new_arc(Arc::clone(&wdt_dev)))?;

        let timeout_secs = wdt_dev
            .set_timeout(wdt_dev.max_timeout_secs())
            .map_err(|_| PnpError::hardware_failure("wdt initial timeout setup failed"))?;
        log::printk!(
            "[platform-ls2x-wdt] bound {} phys={:#x} size={:#x} clk={} Hz max_timeout={}s effective_timeout={}s",
            dev.id,
            phys,
            size,
            clk_hz,
            wdt_dev.max_timeout_secs(),
            timeout_secs,
        );

        dev.set_driver_data(Arc::new(Ls2xWdtBinding {
            wdt,
            wdt_dev,
            clock,
        }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<Ls2xWdtBinding>()
        {
            let _ = binding.wdt.stop();
            if let Some(clock) = binding.clock.as_ref() {
                let _ = clock.control(DtbResourceRequest::Disable);
            }
            binding.wdt_dev.mark_gone();
        }
        log::printk!("[platform-ls2x-wdt] removed {}", dev.id);
    }
}

struct Ls2xWdtFactory;

impl DriverFactory for Ls2xWdtFactory {
    fn name(&self) -> &'static str {
        "platform-ls2x-wdt"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Ls2xWdtPlatformDriver::new(
            ctx.device_mmio_to_virt,
        )))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(Ls2xWdtFactory))
}

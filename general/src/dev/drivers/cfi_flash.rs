//! CFI NOR flash platform 资源驱动。
//!
//! 当前设备层只登记固件声明的线性 MMIO flash 窗口，并提供只读访问能力。擦写协议
//! 需要命令集状态机、擦除块管理和并发保护，应在后续专门的 flash command-set 层
//! 接管，不能在 platform 识别阶段把它伪装成普通块设备。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::read_volatile;

use crate::dev::flash::{self, FlashCapabilities, FlashDevice, FlashError, FlashWindow};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    register_driver_factory,
};

const COMPAT_CFI_FLASH: &str = "cfi-flash";
const PROP_BANK_WIDTH: &str = "bank-width";

#[derive(Clone, Copy)]
struct MappedFlashWindow {
    phys: usize,
    base: usize,
    size: usize,
}

struct CfiFlash {
    name: Box<str>,
    bank_width: usize,
    windows: Vec<MappedFlashWindow>,
}

impl CfiFlash {
    fn total_size(&self) -> Option<usize> {
        self.windows
            .iter()
            .try_fold(0usize, |total, window| total.checked_add(window.size))
    }

    fn locate(&self, mut offset: usize) -> Option<(usize, usize)> {
        for window in &self.windows {
            if offset < window.size {
                return Some((window.base.checked_add(offset)?, window.size - offset));
            }
            offset -= window.size;
        }
        None
    }
}

impl FlashDevice for CfiFlash {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> FlashCapabilities {
        FlashCapabilities {
            readable: true,
            writable: false,
            erasable: false,
        }
    }

    fn bank_width(&self) -> usize {
        self.bank_width
    }

    fn window_count(&self) -> usize {
        self.windows.len()
    }

    fn window_at(&self, index: usize) -> Option<FlashWindow> {
        self.windows.get(index).map(|window| FlashWindow {
            phys: window.phys,
            size: window.size,
        })
    }

    fn read(&self, mut offset: usize, out: &mut [u8]) -> Result<(), FlashError> {
        let total_size = self.total_size().ok_or(FlashError::OutOfRange)?;
        let end = offset
            .checked_add(out.len())
            .ok_or(FlashError::OutOfRange)?;
        if end > total_size {
            return Err(FlashError::OutOfRange);
        }

        let mut done = 0usize;
        while done < out.len() {
            let (addr, available) = self.locate(offset).ok_or(FlashError::OutOfRange)?;
            let count = available.min(out.len() - done);
            for index in 0..count {
                let byte_addr = addr.checked_add(index).ok_or(FlashError::OutOfRange)?;
                out[done + index] = unsafe { read_volatile(byte_addr as *const u8) };
            }
            done += count;
            offset += count;
        }
        Ok(())
    }
}

pub struct CfiFlashPlatformDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl CfiFlashPlatformDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_CFI_FLASH)
    }
}

impl PnpDriver for CfiFlashPlatformDriver {
    fn name(&self) -> &'static str {
        "platform-cfi-flash"
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
        let bank_width = flash_bank_width(info)?;
        let mut windows = Vec::new();
        for (phys, size) in info.mmio_resources() {
            if size == 0 {
                return Err(PnpError::malformed(
                    crate::dev::pnp::PnpResourceKind::Mmio,
                    "cfi flash mmio window has zero size",
                ));
            }
            windows.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
            windows.push(MappedFlashWindow {
                phys,
                base: (self.device_mmio_to_virt)(phys),
                size,
            });
        }
        if windows.is_empty() {
            return Err(PnpError::missing(
                crate::dev::pnp::PnpResourceKind::Mmio,
                "cfi flash has no mmio window",
            ));
        }

        let flash = Arc::new(CfiFlash {
            name: info.fw_name.clone(),
            bank_width,
            windows,
        });
        let handle = flash::register(flash.clone()).map_err(map_flash_error)?;
        if let Err(err) = dev.own_resource(flash::pnp_resource(handle, "platform-cfi-flash")) {
            let _ = flash::unregister(handle);
            return Err(err);
        }
        log::printk!(
            "[cfi-flash] registered {} windows={} bank-width={} total={:#x}",
            flash.name(),
            flash.window_count(),
            flash.bank_width(),
            flash.total_size().unwrap_or(0)
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

fn flash_bank_width(info: &PlatformDeviceInfo) -> Result<usize, PnpError> {
    let width = info.u32_property(PROP_BANK_WIDTH).unwrap_or(1) as usize;
    match width {
        1 | 2 | 4 | 8 => Ok(width),
        _ => Err(PnpError::malformed(
            crate::dev::pnp::PnpResourceKind::Flash,
            "invalid cfi flash bank-width",
        )),
    }
}

fn map_flash_error(err: FlashError) -> PnpError {
    match err {
        FlashError::Invalid | FlashError::OutOfRange | FlashError::Unsupported => {
            PnpError::malformed(
                crate::dev::pnp::PnpResourceKind::Flash,
                "invalid flash registry request",
            )
        }
        FlashError::NotFound => PnpError::InvalidState,
        FlashError::OutOfMemory => PnpError::OutOfMemory,
    }
}

struct CfiFlashFactory;

impl DriverFactory for CfiFlashFactory {
    fn name(&self) -> &'static str {
        "platform-cfi-flash"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(CfiFlashPlatformDriver::new(
            ctx.device_mmio_to_virt,
        )))
    }
}

pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(CfiFlashFactory)).map(|_| ())
}

//! QEMU fw_cfg MMIO platform 驱动。
//!
//! fw_cfg 是固件向内核提供只读配置项的通道。这里实现 MMIO 传输层，并把它
//! 安装到 [`crate::dev::fwcfg`] 的通用接口；是否把某个配置项解释成 initrd、
//! SMBIOS 或其它数据，由更高层决定。

use alloc::sync::Arc;
use core::ptr::{read_volatile, write_volatile};

use vfs::sync::Spinlock;

use crate::dev::fwcfg::{self, FwCfgDevice, FwCfgError, FwCfgHandle};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    register_driver_factory,
};

const COMPAT_QEMU_FW_CFG_MMIO: &str = "qemu,fw-cfg-mmio";

const FW_CFG_SIGNATURE_SELECTOR: u16 = 0x0000;
const FW_CFG_REVISION_SELECTOR: u16 = 0x0001;
const FW_CFG_MMIO_DATA_OFFSET: usize = 0x00;
const FW_CFG_MMIO_SELECTOR_OFFSET: usize = 0x08;
const FW_CFG_MMIO_DMA_OFFSET: usize = 0x10;
const FW_CFG_MMIO_MIN_SIZE: usize = FW_CFG_MMIO_DMA_OFFSET + 8;
const FW_CFG_SIGNATURE: &[u8; 4] = b"QEMU";

struct QemuFwCfgMmio {
    phys: usize,
    base: usize,
    lock: Spinlock<()>,
}

impl QemuFwCfgMmio {
    fn new(phys: usize, base: usize) -> Self {
        Self {
            phys,
            base,
            lock: Spinlock::new(()),
        }
    }

    fn select_locked(&self, selector: u16) {
        // fw_cfg MMIO 的 selector 寄存器按大端解释。当前平台是小端 CPU，
        // 写入前先做字节序转换，避免把 selector 高低字节反置。
        unsafe {
            write_volatile(
                (self.base + FW_CFG_MMIO_SELECTOR_OFFSET) as *mut u16,
                selector.to_be(),
            )
        };
    }

    fn read_data_byte(&self) -> u8 {
        unsafe { read_volatile((self.base + FW_CFG_MMIO_DATA_OFFSET) as *const u8) }
    }

    fn read_revision(&self) -> Result<u32, FwCfgError> {
        let mut raw = [0u8; 4];
        self.read_item(FW_CFG_REVISION_SELECTOR, &mut raw)?;
        Ok(u32::from_le_bytes(raw))
    }
}

impl FwCfgDevice for QemuFwCfgMmio {
    fn read_item(&self, selector: u16, out: &mut [u8]) -> Result<(), FwCfgError> {
        let _guard = self.lock.lock();
        self.select_locked(selector);
        for byte in out.iter_mut() {
            *byte = self.read_data_byte();
        }
        Ok(())
    }
}

struct QemuFwCfgBinding {
    handle: FwCfgHandle,
}

pub struct QemuFwCfgMmioDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl QemuFwCfgMmioDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_QEMU_FW_CFG_MMIO)
    }
}

impl PnpDriver for QemuFwCfgMmioDriver {
    fn name(&self) -> &'static str {
        "platform-qemu-fw-cfg-mmio"
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
        let info = platform_info(dev)?;
        let (phys, size) = info.first_mmio().ok_or(PnpError::ProbeFailed)?;
        if size < FW_CFG_MMIO_MIN_SIZE {
            return Err(PnpError::ProbeFailed);
        }
        let fwcfg = Arc::new(QemuFwCfgMmio::new(phys, (self.device_mmio_to_virt)(phys)));
        let mut signature = [0u8; 4];
        fwcfg
            .read_item(FW_CFG_SIGNATURE_SELECTOR, &mut signature)
            .map_err(map_fwcfg_error)?;
        if &signature != FW_CFG_SIGNATURE {
            return Err(PnpError::ProbeFailed);
        }
        let revision = fwcfg.read_revision().unwrap_or(0);
        let handle = fwcfg::install(fwcfg.clone()).map_err(map_fwcfg_error)?;
        dev.set_driver_data(Arc::new(QemuFwCfgBinding { handle }));
        log::printk!(
            "[fw_cfg] installed qemu fw_cfg mmio phys={:#x} revision={:#x}",
            fwcfg.phys,
            revision
        );
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<QemuFwCfgBinding>()
        {
            let _ = fwcfg::uninstall(binding.handle);
        }
    }
}

fn platform_info(dev: &Arc<PnpDevice>) -> Result<&PlatformDeviceInfo, PnpError> {
    dev.info
        .as_any()
        .downcast_ref::<PlatformDeviceInfo>()
        .ok_or(PnpError::InvalidState)
}

fn map_fwcfg_error(err: FwCfgError) -> PnpError {
    match err {
        FwCfgError::AlreadyInstalled => PnpError::NameConflict,
        FwCfgError::Invalid | FwCfgError::Io | FwCfgError::NotInstalled | FwCfgError::NotFound => {
            PnpError::ProbeFailed
        }
        FwCfgError::OutOfMemory => PnpError::OutOfMemory,
    }
}

struct QemuFwCfgMmioFactory;

impl DriverFactory for QemuFwCfgMmioFactory {
    fn name(&self) -> &'static str {
        "platform-qemu-fw-cfg-mmio"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(QemuFwCfgMmioDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(QemuFwCfgMmioFactory)).map(|_| ())
}

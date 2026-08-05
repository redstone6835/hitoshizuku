//! QEMU fw_cfg MMIO platform 驱动。
//!
//! fw_cfg 是固件向内核提供只读配置项的通道。这里实现 MMIO PIO/DMA 传输层，并把它
//! 安装到 [`general::dev::fwcfg`] 的通用接口；是否把某个配置项解释成 initrd、
//! SMBIOS 或其它数据，由更高层决定。

use alloc::sync::Arc;
use core::hint::spin_loop;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};

use vfs::sync::Spinlock;

use crate::dev::dma::{DmaBuffer, DmaContext, DmaDirection};
use crate::dev::fwcfg::{self, FwCfgDevice, FwCfgError};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, register_driver_factory,
};

const COMPAT_QEMU_FW_CFG_MMIO: &str = "qemu,fw-cfg-mmio";

const FW_CFG_SIGNATURE_SELECTOR: u16 = 0x0000;
const FW_CFG_REVISION_SELECTOR: u16 = 0x0001;
const FW_CFG_MMIO_DATA_OFFSET: usize = 0x00;
const FW_CFG_MMIO_SELECTOR_OFFSET: usize = 0x08;
const FW_CFG_MMIO_DMA_OFFSET: usize = 0x10;
const FW_CFG_MMIO_PIO_MIN_SIZE: usize = FW_CFG_MMIO_SELECTOR_OFFSET + core::mem::size_of::<u16>();
const FW_CFG_MMIO_DMA_MIN_SIZE: usize = FW_CFG_MMIO_DMA_OFFSET + core::mem::size_of::<u64>();
const FW_CFG_SIGNATURE: &[u8; 4] = b"QEMU";
const FW_CFG_VERSION_DMA: u32 = 1 << 1;
const FW_CFG_DMA_CTL_ERROR: u32 = 1 << 0;
const FW_CFG_DMA_CTL_READ: u32 = 1 << 1;
const FW_CFG_DMA_CTL_SELECT: u32 = 1 << 3;
const FW_CFG_DMA_SELECT_SHIFT: u32 = 16;
const FW_CFG_DMA_DESCRIPTOR_SIZE: usize = 16;
const FW_CFG_DMA_DESCRIPTOR_ALIGN: usize = 16;

struct QemuFwCfgMmio {
    phys: usize,
    base: usize,
    dma_context: DmaContext,
    dma_enabled: AtomicBool,
    lock: Spinlock<()>,
}

impl QemuFwCfgMmio {
    fn new(phys: usize, base: usize, dma_context: DmaContext) -> Self {
        Self {
            phys,
            base,
            dma_context,
            dma_enabled: AtomicBool::new(false),
            lock: Spinlock::new(()),
        }
    }

    fn select_locked(&self, selector: u16) {
        // fw_cfg MMIO 的 selector 寄存器按大端解释。当前平台是小端 CPU，
        // 写入前先做字节序转换，避免把 selector 高低字节反置。
        // Safety: probe 已确认并映射完整的 fw_cfg MMIO 寄存器窗口，选择器偏移和
        // 访问宽度由 fw_cfg MMIO 规范固定。
        unsafe {
            write_volatile(
                (self.base + FW_CFG_MMIO_SELECTOR_OFFSET) as *mut u16,
                selector.to_be(),
            )
        };
    }

    fn read_data_byte(&self) -> u8 {
        // Safety: 安全条件与 `select_locked` 相同，数据寄存器支持连续单字节易失读取。
        unsafe { read_volatile((self.base + FW_CFG_MMIO_DATA_OFFSET) as *const u8) }
    }

    fn read_revision(&self) -> Result<u32, FwCfgError> {
        let mut raw = [0u8; 4];
        self.read_item(FW_CFG_REVISION_SELECTOR, &mut raw)?;
        Ok(u32::from_le_bytes(raw))
    }

    fn read_item_pio_locked(&self, selector: u16, out: &mut [u8]) {
        self.select_locked(selector);
        for byte in out.iter_mut() {
            *byte = self.read_data_byte();
        }
    }

    fn submit_dma_descriptor(&self, descriptor_dma_addr: usize) {
        // fw_cfg MMIO DMA 地址寄存器按大端接收 64 位设备地址。
        // Safety: probe 已验证 DMA 寄存器完整位于固件声明的 MMIO 窗口内，
        // `descriptor_dma_addr` 来自该设备的 DmaContext 映射。
        unsafe {
            write_volatile(
                (self.base + FW_CFG_MMIO_DMA_OFFSET) as *mut u64,
                (descriptor_dma_addr as u64).to_be(),
            )
        };
    }

    fn read_item_dma_locked(&self, selector: u16, out: &mut [u8]) -> Result<(), FwCfgError> {
        if out.is_empty() {
            return Ok(());
        }
        let length = u32::try_from(out.len()).map_err(|_| FwCfgError::Invalid)?;
        let total = FW_CFG_DMA_DESCRIPTOR_SIZE
            .checked_add(out.len())
            .ok_or(FwCfgError::OutOfMemory)?;
        let mut buffer = DmaBuffer::new_in(
            self.dma_context.clone(),
            total,
            FW_CFG_DMA_DESCRIPTOR_ALIGN,
            DmaDirection::Bidirectional,
        )
        .map_err(|_| FwCfgError::OutOfMemory)?;
        let data_dma_addr = buffer
            .dma_addr()
            .checked_add(FW_CFG_DMA_DESCRIPTOR_SIZE)
            .ok_or(FwCfgError::Invalid)?;
        let control = FW_CFG_DMA_CTL_SELECT
            | FW_CFG_DMA_CTL_READ
            | (u32::from(selector) << FW_CFG_DMA_SELECT_SHIFT);
        let bytes = buffer.as_mut_slice();
        bytes[0..4].copy_from_slice(&control.to_be_bytes());
        bytes[4..8].copy_from_slice(&length.to_be_bytes());
        bytes[8..16].copy_from_slice(&(data_dma_addr as u64).to_be_bytes());
        buffer.sync_for_device();
        // fw_cfg DMA 规范要求 descriptor/data 在 doorbell MMIO 写之前对设备可见。
        // DmaContext 负责 cache 维护，HAL 强屏障负责普通内存与 MMIO 的硬件顺序。
        hal::memory::device_io_barrier();
        self.submit_dma_descriptor(buffer.dma_addr());

        // fw_cfg 没有取消一个已提交 DMA descriptor 的协议。超时后释放 buffer 会让
        // 仍在运行的设备写入已回收物理页，因此与 Linux 一样等待 control 完成；
        // 只有设备明确写回 ERROR 后才可安全释放并回退 PIO。
        loop {
            buffer.sync_for_cpu();
            hal::memory::device_io_barrier();
            // Safety: descriptor 起始地址按 16 字节分配并至少包含 4 字节；设备会异步
            // 改写 control，volatile 读取避免编译器把轮询折叠成一次普通 load。
            let control =
                u32::from_be(unsafe { read_volatile(buffer.as_slice().as_ptr().cast::<u32>()) });
            if control == 0 {
                // 完成标志本身先于 payload 发布。再次同步完整 buffer，并用强屏障
                // 保证后续普通 load 不越过设备完成观察点。
                hal::memory::device_io_barrier();
                buffer.sync_for_cpu();
                hal::memory::device_io_barrier();
                out.copy_from_slice(&buffer.as_slice()[FW_CFG_DMA_DESCRIPTOR_SIZE..total]);
                return Ok(());
            }
            if control & FW_CFG_DMA_CTL_ERROR != 0 {
                return Err(FwCfgError::Io);
            }
            spin_loop();
        }
    }

    fn enable_and_verify_dma(&self) -> bool {
        self.dma_enabled.store(true, Ordering::Release);
        let mut signature = [0u8; 4];
        let _guard = self.lock.lock();
        let valid = self
            .read_item_dma_locked(FW_CFG_SIGNATURE_SELECTOR, &mut signature)
            .is_ok()
            && &signature == FW_CFG_SIGNATURE;
        if !valid {
            self.dma_enabled.store(false, Ordering::Release);
        }
        valid
    }
}

impl FwCfgDevice for QemuFwCfgMmio {
    fn read_item(&self, selector: u16, out: &mut [u8]) -> Result<(), FwCfgError> {
        let _guard = self.lock.lock();
        if self.dma_enabled.load(Ordering::Acquire) {
            match self.read_item_dma_locked(selector, out) {
                Ok(()) => return Ok(()),
                Err(_) => {
                    // 一次 DMA 协议或映射失败后永久回退到 PIO，避免每次读取都重复
                    // 分配和超时；当前请求仍由 PIO 完成，保持 fw_cfg 可用性。
                    self.dma_enabled.store(false, Ordering::Release);
                    log::printk!("[fw_cfg] DMA transfer failed, falling back to PIO");
                }
            }
        }
        self.read_item_pio_locked(selector, out);
        Ok(())
    }
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
        let (phys, size) = info.first_mmio().ok_or(PnpError::missing(
            crate::dev::pnp::PnpResourceKind::Mmio,
            "fw_cfg mmio window missing",
        ))?;
        if size < FW_CFG_MMIO_PIO_MIN_SIZE {
            return Err(PnpError::malformed(
                crate::dev::pnp::PnpResourceKind::Mmio,
                "fw_cfg PIO register window too small",
            ));
        }
        let fwcfg = Arc::new(QemuFwCfgMmio::new(
            phys,
            (self.device_mmio_to_virt)(phys),
            info.dma_context(),
        ));
        let mut signature = [0u8; 4];
        fwcfg
            .read_item(FW_CFG_SIGNATURE_SELECTOR, &mut signature)
            .map_err(map_fwcfg_error)?;
        if &signature != FW_CFG_SIGNATURE {
            return Err(PnpError::malformed(
                crate::dev::pnp::PnpResourceKind::FwCfg,
                "fw_cfg signature mismatch",
            ));
        }
        let revision = fwcfg.read_revision().unwrap_or(0);
        let dma_enabled = size >= FW_CFG_MMIO_DMA_MIN_SIZE
            && revision & FW_CFG_VERSION_DMA != 0
            && fwcfg.enable_and_verify_dma();
        dev.reserve_owned_resources(1)?;
        let handle = fwcfg::install(fwcfg.clone()).map_err(map_fwcfg_error)?;
        if let Err(err) = dev.own_resource(fwcfg::pnp_resource(handle, "platform-fw-cfg")) {
            let _ = fwcfg::uninstall(handle);
            return Err(err);
        }
        log::printk!(
            "[fw_cfg] installed qemu fw_cfg mmio phys={:#x} revision={:#x} transport={}",
            fwcfg.phys,
            revision,
            if dma_enabled { "dma" } else { "pio" }
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

fn map_fwcfg_error(err: FwCfgError) -> PnpError {
    match err {
        FwCfgError::AlreadyInstalled => PnpError::NameConflict,
        FwCfgError::Invalid | FwCfgError::Io | FwCfgError::NotInstalled | FwCfgError::NotFound => {
            PnpError::registration_failed(
                crate::dev::pnp::PnpResourceKind::FwCfg,
                "fw_cfg install failed",
            )
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

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(QemuFwCfgMmioFactory))
}

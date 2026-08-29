//! QEMU fw_cfg MMIO/SystemIO platform 驱动。
//!
//! fw_cfg 是固件向内核提供只读配置项的通道。这里实现 MMIO PIO/DMA 和传统
//! SystemIO PIO 传输层，并把它安装到 [`general::dev::fwcfg`] 的通用接口；是否把
//! 某个配置项解释成 initrd、SMBIOS 或其它数据，由更高层决定。

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
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use general::StartAcpiIoOps;

const COMPAT_QEMU_FW_CFG_MMIO: &str = "qemu,fw-cfg-mmio";
const ACPI_HID_QEMU_FW_CFG: &str = "QEMU0002";

const FW_CFG_SIGNATURE_SELECTOR: u16 = 0x0000;
const FW_CFG_REVISION_SELECTOR: u16 = 0x0001;
const FW_CFG_MMIO_DATA_OFFSET: usize = 0x00;
const FW_CFG_MMIO_SELECTOR_OFFSET: usize = 0x08;
const FW_CFG_MMIO_DMA_OFFSET: usize = 0x10;
const FW_CFG_MMIO_PIO_MIN_SIZE: usize = FW_CFG_MMIO_SELECTOR_OFFSET + core::mem::size_of::<u16>();
const FW_CFG_MMIO_DMA_MIN_SIZE: usize = FW_CFG_MMIO_DMA_OFFSET + core::mem::size_of::<u64>();
const FW_CFG_IO_DATA_OFFSET: u16 = 1;
const FW_CFG_IO_MIN_SIZE: u16 = 2;
const FW_CFG_SIGNATURE: &[u8; 4] = b"QEMU";
const FW_CFG_VERSION_DMA: u32 = 1 << 1;
const FW_CFG_DMA_CTL_ERROR: u32 = 1 << 0;
const FW_CFG_DMA_CTL_READ: u32 = 1 << 1;
const FW_CFG_DMA_CTL_SELECT: u32 = 1 << 3;
const FW_CFG_DMA_SELECT_SHIFT: u32 = 16;
const FW_CFG_DMA_DESCRIPTOR_SIZE: usize = 16;
const FW_CFG_DMA_DESCRIPTOR_ALIGN: usize = 16;

// QEMU PC exposes one process-global selector/data register pair.  Keep the
// selector write and the complete data stream atomic even if duplicate
// firmware descriptions are probed concurrently.
static FW_CFG_SYSTEM_IO_LOCK: Spinlock<()> = Spinlock::new(());

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

struct QemuFwCfgSystemIo {
    selector_port: u16,
    data_port: u16,
    ops: StartAcpiIoOps,
}

impl QemuFwCfgSystemIo {
    fn new(selector_port: u16, size: u16, ops: StartAcpiIoOps) -> Option<Self> {
        let window_end = selector_port.checked_add(size.checked_sub(1)?)?;
        let data_port = selector_port.checked_add(FW_CFG_IO_DATA_OFFSET)?;
        (size >= FW_CFG_IO_MIN_SIZE && selector_port & 1 == 0 && data_port <= window_end).then_some(
            Self {
                selector_port,
                data_port,
                ops,
            },
        )
    }

    fn select_locked(&self, selector: u16) {
        // QEMU 把 I/O-port selector 定义为 16 位小端事务。这里传递数值本身，
        // 与 Linux qemu_fw_cfg_select() 的 iowrite16() 相同；MMIO 路径才需要
        // `to_be()`。在 x86 上 outw 的低字节先出现在 0x510，但 QEMU 按完整
        // 16 位 I/O 事务解码，不能把 selector 预先交换字节。
        (self.ops.write_u16)(self.selector_port, selector);
    }

    fn read_data_byte(&self) -> u8 {
        (self.ops.read_u8)(self.data_port)
    }
}

impl FwCfgDevice for QemuFwCfgSystemIo {
    fn read_item(&self, selector: u16, out: &mut [u8]) -> Result<(), FwCfgError> {
        let _guard = FW_CFG_SYSTEM_IO_LOCK.lock();
        self.select_locked(selector);
        for byte in out.iter_mut() {
            *byte = self.read_data_byte();
        }
        Ok(())
    }
}

pub struct QemuFwCfgDriver {
    device_mmio_to_virt: fn(usize) -> usize,
    system_io: Option<StartAcpiIoOps>,
}

impl QemuFwCfgDriver {
    const fn new(
        device_mmio_to_virt: fn(usize) -> usize,
        system_io: Option<StartAcpiIoOps>,
    ) -> Self {
        Self {
            device_mmio_to_virt,
            system_io,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_QEMU_FW_CFG_MMIO) || info.has_id(ACPI_HID_QEMU_FW_CFG)
    }

    fn probe_mmio(&self, dev: &Arc<PnpDevice>, info: &PlatformDeviceInfo) -> Result<(), PnpError> {
        let (phys, size) = info.first_mmio().ok_or(PnpError::missing(
            PnpResourceKind::Mmio,
            "fw_cfg mmio window missing",
        ))?;
        if size < FW_CFG_MMIO_PIO_MIN_SIZE {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "fw_cfg PIO register window too small",
            ));
        }
        let fwcfg = Arc::new(QemuFwCfgMmio::new(
            phys,
            (self.device_mmio_to_virt)(phys),
            info.dma_context(),
        ));
        let revision = verify_fwcfg(fwcfg.as_ref())?;
        let dma_enabled = size >= FW_CFG_MMIO_DMA_MIN_SIZE
            && revision & FW_CFG_VERSION_DMA != 0
            && fwcfg.enable_and_verify_dma();
        install_owned_fwcfg(dev, fwcfg.clone())?;
        log::printk!(
            "[fw_cfg] installed qemu fw_cfg mmio phys={:#x} revision={:#x} transport={}",
            fwcfg.phys,
            revision,
            if dma_enabled { "dma" } else { "pio" }
        );
        Ok(())
    }

    fn probe_system_io(
        &self,
        dev: &Arc<PnpDevice>,
        info: &PlatformDeviceInfo,
    ) -> Result<(), PnpError> {
        let (base, size) = info.first_io_port().ok_or(PnpError::missing(
            PnpResourceKind::IoPort,
            "fw_cfg SystemIO selector/data window missing",
        ))?;
        let ops = self.system_io.ok_or(PnpError::unsupported(
            "fw_cfg SystemIO port access callbacks",
        ))?;
        let fwcfg = Arc::new(QemuFwCfgSystemIo::new(base, size, ops).ok_or(
            PnpError::malformed(
                PnpResourceKind::IoPort,
                "fw_cfg SystemIO window must contain an aligned selector and data port",
            ),
        )?);
        let revision = verify_fwcfg(fwcfg.as_ref())?;
        // The ACPI resource describes selector/data only. The optional x86 DMA doorbell at
        // 0x514 is outside that ownership range, so this transport deliberately remains PIO.
        install_owned_fwcfg(dev, fwcfg)?;
        log::printk!(
            "[fw_cfg] installed qemu fw_cfg system-io selector={:#x} data={:#x} revision={:#x} transport=pio",
            base,
            base + FW_CFG_IO_DATA_OFFSET,
            revision
        );
        Ok(())
    }
}

impl PnpDriver for QemuFwCfgDriver {
    fn name(&self) -> &'static str {
        "platform-qemu-fw-cfg"
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
        if info.has_id(COMPAT_QEMU_FW_CFG_MMIO) {
            self.probe_mmio(dev, info)
        } else if info.has_id(ACPI_HID_QEMU_FW_CFG) {
            self.probe_system_io(dev, info)
        } else {
            Err(PnpError::InvalidState)
        }
    }

    fn remove(&self, _dev: &Arc<PnpDevice>) {}
}

fn verify_fwcfg(fwcfg: &dyn FwCfgDevice) -> Result<u32, PnpError> {
    let mut signature = [0u8; 4];
    fwcfg
        .read_item(FW_CFG_SIGNATURE_SELECTOR, &mut signature)
        .map_err(map_fwcfg_error)?;
    if &signature != FW_CFG_SIGNATURE {
        return Err(PnpError::malformed(
            PnpResourceKind::FwCfg,
            "fw_cfg signature mismatch",
        ));
    }
    let mut raw_revision = [0u8; 4];
    fwcfg
        .read_item(FW_CFG_REVISION_SELECTOR, &mut raw_revision)
        .map_err(map_fwcfg_error)?;
    Ok(u32::from_le_bytes(raw_revision))
}

fn install_owned_fwcfg(
    dev: &Arc<PnpDevice>,
    fwcfg_device: Arc<dyn FwCfgDevice>,
) -> Result<(), PnpError> {
    dev.reserve_owned_resources(1)?;
    let handle = fwcfg::install(fwcfg_device).map_err(map_fwcfg_error)?;
    if let Err(err) = dev.own_resource(fwcfg::pnp_resource(handle, "platform-fw-cfg")) {
        let _ = fwcfg::uninstall(handle);
        return Err(err);
    }
    Ok(())
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

struct QemuFwCfgFactory;

impl DriverFactory for QemuFwCfgFactory {
    fn name(&self) -> &'static str {
        "platform-qemu-fw-cfg"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(QemuFwCfgDriver::new(
            ctx.device_mmio_to_virt,
            ctx.system_io,
        )))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(QemuFwCfgFactory))
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use std::sync::Mutex;

    use crate::dev::dma::DmaContext;
    use crate::dev::platform::{DeviceMatchId, DeviceProperties, DeviceResource};

    use super::*;

    struct TestIo {
        selector: u16,
        offset: usize,
        selector_writes: Vec<(u16, u16)>,
        data_reads: Vec<u16>,
    }

    static TEST_IO: Mutex<TestIo> = Mutex::new(TestIo {
        selector: 0,
        offset: 0,
        selector_writes: Vec::new(),
        data_reads: Vec::new(),
    });

    fn test_read_u8(port: u16) -> u8 {
        let mut io = TEST_IO.lock().expect("test fw_cfg I/O lock poisoned");
        io.data_reads.push(port);
        let value = match io.selector {
            FW_CFG_SIGNATURE_SELECTOR => FW_CFG_SIGNATURE.get(io.offset).copied().unwrap_or(0),
            FW_CFG_REVISION_SELECTOR => [3, 0, 0, 0].get(io.offset).copied().unwrap_or(0),
            selector => (selector as u8).wrapping_add(io.offset as u8),
        };
        io.offset += 1;
        value
    }

    fn test_read_u16(_port: u16) -> u16 {
        panic!("fw_cfg SystemIO must read the data register one byte at a time")
    }

    fn test_read_u32(_port: u16) -> u32 {
        panic!("fw_cfg SystemIO must read the data register one byte at a time")
    }

    fn test_write_u8(_port: u16, _value: u8) {
        panic!("fw_cfg SystemIO selector must be written as one 16-bit transaction")
    }

    fn test_write_u16(port: u16, value: u16) {
        let mut io = TEST_IO.lock().expect("test fw_cfg I/O lock poisoned");
        io.selector = value;
        io.offset = 0;
        io.selector_writes.push((port, value));
    }

    fn test_write_u32(_port: u16, _value: u32) {
        panic!("fw_cfg SystemIO selector must be written as one 16-bit transaction")
    }

    const TEST_IO_OPS: StartAcpiIoOps = StartAcpiIoOps {
        read_u8: test_read_u8,
        read_u16: test_read_u16,
        read_u32: test_read_u32,
        write_u8: test_write_u8,
        write_u16: test_write_u16,
        write_u32: test_write_u32,
    };

    fn reset_test_io() {
        let mut io = TEST_IO.lock().expect("test fw_cfg I/O lock poisoned");
        io.selector = 0;
        io.offset = 0;
        io.selector_writes.clear();
        io.data_reads.clear();
    }

    fn platform_info(
        ids: Vec<DeviceMatchId>,
        resources: Vec<DeviceResource>,
    ) -> PlatformDeviceInfo {
        PlatformDeviceInfo {
            fw_name: "fwcfg".into(),
            fw_path: Some("\\_SB.FWCF".into()),
            fw_parent_path: None,
            ids,
            resources,
            irq_names: Vec::new(),
            properties: DeviceProperties::default(),
            fw_properties: Vec::new(),
            dma: DmaContext::default_coherent(),
            dtb_bindings: None,
            dtb_pcie_host: None,
            dtb_owned_nodes: None,
        }
    }

    #[test]
    fn system_io_transport_uses_native_selector_and_overlapping_data_port() {
        reset_test_io();
        let fwcfg =
            QemuFwCfgSystemIo::new(0x0510, 2, TEST_IO_OPS).expect("valid fw_cfg SystemIO window");
        assert_eq!(verify_fwcfg(&fwcfg), Ok(3));

        let mut data = [0u8; 3];
        fwcfg
            .read_item(0x1234, &mut data)
            .expect("test fw_cfg read");
        assert_eq!(data, [0x34, 0x35, 0x36]);

        let io = TEST_IO.lock().expect("test fw_cfg I/O lock poisoned");
        assert_eq!(
            io.selector_writes,
            [(0x0510, 0x0000), (0x0510, 0x0001), (0x0510, 0x1234)]
        );
        assert_eq!(io.data_reads, [0x0511; 11]);
    }

    #[test]
    fn system_io_transport_rejects_short_unaligned_or_wrapping_windows() {
        assert!(QemuFwCfgSystemIo::new(0x0510, 1, TEST_IO_OPS).is_none());
        assert!(QemuFwCfgSystemIo::new(0x0511, 2, TEST_IO_OPS).is_none());
        assert!(QemuFwCfgSystemIo::new(u16::MAX, 2, TEST_IO_OPS).is_none());
        assert!(QemuFwCfgSystemIo::new(0xfffe, 3, TEST_IO_OPS).is_none());
    }

    #[test]
    fn driver_matches_acpi_and_mmio_firmware_ids_only() {
        let acpi = platform_info(
            alloc::vec![DeviceMatchId::AcpiHid(ACPI_HID_QEMU_FW_CFG.into())],
            alloc::vec![DeviceResource::io_port(0x0510, 2)],
        );
        let dtb = platform_info(
            alloc::vec![DeviceMatchId::DtbCompatible(COMPAT_QEMU_FW_CFG_MMIO.into(),)],
            alloc::vec![DeviceResource::mmio(0x1000, 0x18)],
        );
        let unrelated = platform_info(
            alloc::vec![DeviceMatchId::AcpiHid(Box::<str>::from("PNP0501"))],
            Vec::new(),
        );
        assert!(QemuFwCfgDriver::matches_platform(&acpi));
        assert!(QemuFwCfgDriver::matches_platform(&dtb));
        assert!(!QemuFwCfgDriver::matches_platform(&unrelated));
    }
}

//! LS2K1000 SPI 控制器 + SPI-NOR flash platform ELM 驱动。
//!
//! 匹配工厂 DTB 的 `loongson,ls-spi` 控制器节点（reg 0x1fff0220 窗口
//! 0x10，clocks=<&clk 13>）与其 `jedec,spi-nor` 子节点（w25q64，
//! spi-max-frequency 30 MHz）。控制器与 flash 是同一个 ELM 模块里的两个
//! PnP 匹配：控制器 probe 时把 [`SpiMaster`] 放进模块内注册表（按固件
//! 路径索引），flash probe 时按父节点路径取回 master，避免跨驱动共享
//! 状态。
//!
//! 寄存器与位定义对照 Linux drivers/spi/spi-loongson-core.c：SPCR(0x00)
//! 的 SPE/CPOL/CPHA、SPSR(0x01) 的 RFEMPTY/WCOL/SPIF、FIFO(0x02)、
//! SPER(0x03) 时钟分频、SFCS(0x05) 片选。SPI-NOR 实现
//! [`general::dev::flash`] 的读写擦接口（JEDEC 0x03/0x02/0x20/0x06/0x05）。

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use general::dev::flash::{
    FlashCapabilities, FlashDevice, FlashDeviceV2, FlashEraseRegion, FlashError, FlashHandle,
    FlashIoError, FlashWindow, register_v2,
};
use general::dev::platform::PlatformDeviceInfo;
use general::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use vfs::sync::Spinlock;

const COMPAT_LS_SPI: &str = "loongson,ls-spi";
const COMPAT_JEDEC_SPI_NOR: &str = "jedec,spi-nor";

const PROP_CLOCKS: &str = "clocks";

// 寄存器偏移（8 位寄存器）。
const SPCR_REG: usize = 0x00;
const SPSR_REG: usize = 0x01;
const FIFO_REG: usize = 0x02;
const SPER_REG: usize = 0x03;
const PARA_REG: usize = 0x04;
const SFCS_REG: usize = 0x05;

const SPCR_CPHA: u8 = 1 << 2;
const SPCR_CPOL: u8 = 1 << 3;
const SPCR_SPE: u8 = 1 << 6;
const SPSR_RFEMPTY: u8 = 1 << 0;
const SPSR_WCOL: u8 = 1 << 6;
const SPSR_SPIF: u8 = 1 << 7;

/// SPI 时钟分频查找表（Linux loongson_spi_set_clk 的 rdiv）。
const CLK_RDIV: [u8; 12] = [0, 1, 4, 2, 3, 5, 6, 7, 8, 9, 10, 11];

/// SPI-NOR 指令。
const CMD_READ: u8 = 0x03;
const CMD_READ_ID: u8 = 0x9f;
const CMD_WRITE_ENABLE: u8 = 0x06;
const CMD_PAGE_PROGRAM: u8 = 0x02;
const CMD_SECTOR_ERASE: u8 = 0x20;
const CMD_READ_STATUS: u8 = 0x05;
const STATUS_WIP: u8 = 0x01;

/// w25q64 等常见 JEDEC 容量码（0x11=1Mb … 0x17=64Mb=8MB）。
fn nor_size_from_capacity(code: u8) -> Option<usize> {
    if (0x11..=0x20).contains(&code) {
        let mbit = 1usize << (code - 0x11);
        Some(mbit * 128 * 1024)
    } else {
        None
    }
}

const XFER_TIMEOUT_LOOPS: u32 = 10_000;

fn delay_ns(duration_ns: u64) {
    let deadline = hal::time::monotonic_ns().saturating_add(duration_ns);
    while hal::time::monotonic_ns() < deadline {
        core::hint::spin_loop();
    }
}

/// LS2K SPI 控制器实例。
pub struct SpiMaster {
    base: usize,
    clk_rate: u64,
}

impl SpiMaster {
    fn new(base: usize, clk_rate: u64) -> Self {
        Self { base, clk_rate }
    }

    fn read8(&self, offset: usize) -> u8 {
        // Safety: offset 是受控固定寄存器偏移，base 由 platform probe 映射。
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u8) }
    }

    fn write8(&self, offset: usize, value: u8) {
        // Safety: 同 read8，目标寄存器允许 8 位易失写入。
        unsafe { core::ptr::write_volatile((self.base + offset) as *mut u8, value) }
    }

    /// 控制器初始化（Linux loongson_spi_reginit）。
    pub fn reginit(&self) {
        self.write8(SPCR_REG, self.read8(SPCR_REG) & !SPCR_SPE);
        self.write8(SPSR_REG, SPSR_SPIF | SPSR_WCOL);
        self.write8(SPCR_REG, self.read8(SPCR_REG) | SPCR_SPE);
        self.set_clk(30_000_000);
    }

    /// 按目标频率设置分频（Linux loongson_spi_set_clk）。
    pub fn set_clk(&self, hz: u32) {
        if self.clk_rate == 0 || hz == 0 {
            return;
        }
        let div = ((self.clk_rate + u64::from(hz) - 1) / u64::from(hz)).clamp(2, 4096);
        let index = (usize::BITS - (div - 1).leading_zeros()) as usize;
        let div_tmp = CLK_RDIV.get(index).copied().unwrap_or(0);
        let spcr = div_tmp & 0b11;
        let sper = (div_tmp >> 2) & 0b11;
        self.write8(SPCR_REG, (self.read8(SPCR_REG) & !0b11) | spcr);
        self.write8(SPER_REG, (self.read8(SPER_REG) & !0b11) | sper);
    }

    pub fn cs_assert(&self, cs: u8) {
        let mask = (0x11u8) << cs;
        let value = self.read8(SFCS_REG) & !mask;
        self.write8(SFCS_REG, value | mask);
    }

    pub fn cs_deassert(&self, cs: u8) {
        let mask = (0x11u8) << cs;
        let value = self.read8(SFCS_REG) & !mask;
        self.write8(SFCS_REG, value | (0x01u8 << cs));
    }

    /// 8 位全双工传输（Linux loongson_spi_write_read_8bit）。
    pub fn transfer(&self, tx: &[u8], rx: &mut [u8]) -> Result<(), &'static str> {
        let count = tx.len().max(rx.len());
        for index in 0..count {
            let out = tx.get(index).copied().unwrap_or(0);
            self.write8(FIFO_REG, out);
            let mut waited = false;
            for _ in 0..XFER_TIMEOUT_LOOPS {
                if self.read8(SPSR_REG) & SPSR_RFEMPTY == 0 {
                    waited = true;
                    break;
                }
                delay_ns(100);
            }
            if !waited {
                return Err("LS2K SPI transfer timeout");
            }
            let input = self.read8(FIFO_REG);
            if let Some(slot) = rx.get_mut(index) {
                *slot = input;
            }
        }
        Ok(())
    }
}

/// SPI-NOR 设备的模块内 master 注册表（按控制器固件路径索引）。
struct SpiMasterRegistry {
    masters: Vec<(String, Arc<SpiMaster>)>,
}

static SPI_MASTERS: Spinlock<SpiMasterRegistry> = Spinlock::new(SpiMasterRegistry {
    masters: Vec::new(),
});

fn register_master(path: &str, master: Arc<SpiMaster>) {
    let mut registry = SPI_MASTERS.lock();
    registry.masters.retain(|(key, _)| key != path);
    registry.masters.push((path.to_owned(), master));
}

fn lookup_master(path: Option<&str>) -> Option<Arc<SpiMaster>> {
    let registry = SPI_MASTERS.lock();
    registry
        .masters
        .iter()
        .find(|(key, _)| Some(key.as_str()) == path)
        .map(|(_, master)| Arc::clone(master))
}

// ─────────────────────────── SPI-NOR flash ───────────────────────────

const SECTOR_SIZE: usize = 4096;
const PAGE_SIZE: usize = 256;

pub struct SpiNorFlash {
    master: Arc<SpiMaster>,
    size: usize,
    name: String,
    lock: Spinlock<()>,
}

impl SpiNorFlash {
    fn new(master: Arc<SpiMaster>, size: usize, name: String) -> Self {
        Self {
            master,
            size,
            name,
            lock: Spinlock::new(()),
        }
    }

    fn read_id(&self) -> Result<[u8; 3], &'static str> {
        let mut rx = [0u8; 3];
        self.master.cs_assert(0);
        let result = self
            .master
            .transfer(&[CMD_READ_ID], &mut [0u8; 1])
            .and_then(|_| self.master.transfer(&[], &mut rx));
        self.master.cs_deassert(0);
        result?;
        Ok(rx)
    }

    fn read_status(&self) -> u8 {
        let mut status = [0u8; 1];
        self.master.cs_assert(0);
        let _ = self
            .master
            .transfer(&[CMD_READ_STATUS], &mut [0u8; 1])
            .and_then(|_| self.master.transfer(&[], &mut status));
        self.master.cs_deassert(0);
        status[0]
    }

    fn wait_ready(&self) -> Result<(), FlashIoError> {
        for _ in 0..XFER_TIMEOUT_LOOPS {
            if self.read_status() & STATUS_WIP == 0 {
                return Ok(());
            }
            delay_ns(100_000);
        }
        Err(FlashIoError::Busy)
    }

    fn write_enable(&self) -> Result<(), FlashIoError> {
        self.master.cs_assert(0);
        let result = self.master.transfer(&[CMD_WRITE_ENABLE], &mut [0u8; 1]);
        self.master.cs_deassert(0);
        result.map_err(|_| FlashIoError::Io)
    }

    fn read_raw(&self, offset: usize, out: &mut [u8]) -> Result<(), FlashIoError> {
        let address = [
            ((offset >> 16) & 0xff) as u8,
            ((offset >> 8) & 0xff) as u8,
            (offset & 0xff) as u8,
        ];
        self.master.cs_assert(0);
        let result = self
            .master
            .transfer(&[CMD_READ], &mut [0u8; 1])
            .and_then(|_| self.master.transfer(&address, &mut [0u8; 3]))
            .and_then(|_| self.master.transfer(&[], out));
        self.master.cs_deassert(0);
        result.map_err(|_| FlashIoError::Io)
    }

    fn page_program(&self, offset: usize, data: &[u8]) -> Result<(), FlashIoError> {
        let address = [
            ((offset >> 16) & 0xff) as u8,
            ((offset >> 8) & 0xff) as u8,
            (offset & 0xff) as u8,
        ];
        self.write_enable()?;
        let mut tx = Vec::with_capacity(4 + data.len());
        tx.push(CMD_PAGE_PROGRAM);
        tx.extend_from_slice(&address);
        tx.extend_from_slice(data);
        self.master.cs_assert(0);
        let mut scratch = vec![0u8; tx.len()];
        let result = self.master.transfer(&tx, &mut scratch);
        self.master.cs_deassert(0);
        result.map_err(|_| FlashIoError::Io)?;
        self.wait_ready()
    }

    fn sector_erase(&self, offset: usize) -> Result<(), FlashIoError> {
        let address = [
            ((offset >> 16) & 0xff) as u8,
            ((offset >> 8) & 0xff) as u8,
            (offset & 0xff) as u8,
        ];
        self.write_enable()?;
        self.master.cs_assert(0);
        let result = self
            .master
            .transfer(&[CMD_SECTOR_ERASE], &mut [0u8; 1])
            .and_then(|_| self.master.transfer(&address, &mut [0u8; 3]));
        self.master.cs_deassert(0);
        result.map_err(|_| FlashIoError::Io)?;
        self.wait_ready()
    }
}

impl FlashDevice for SpiNorFlash {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> FlashCapabilities {
        FlashCapabilities {
            readable: true,
            writable: true,
            erasable: true,
        }
    }

    fn bank_width(&self) -> usize {
        1
    }

    fn window_count(&self) -> usize {
        0
    }

    fn window_at(&self, _index: usize) -> Option<FlashWindow> {
        None
    }

    fn read(&self, offset: usize, out: &mut [u8]) -> Result<(), FlashError> {
        if offset
            .checked_add(out.len())
            .is_none_or(|end| end > self.size)
        {
            return Err(FlashError::OutOfRange);
        }
        let _guard = self.lock.lock();
        self.read_raw(offset, out).map_err(|_| FlashError::Invalid)
    }
}

impl FlashDeviceV2 for SpiNorFlash {
    fn erase_region_count(&self) -> usize {
        1
    }

    fn erase_region_at(&self, index: usize) -> Option<FlashEraseRegion> {
        (index == 0).then_some(FlashEraseRegion {
            offset: 0,
            block_size: SECTOR_SIZE,
            block_count: self.size / SECTOR_SIZE,
        })
    }

    fn write(&self, offset: usize, data: &[u8]) -> Result<(), FlashIoError> {
        if offset
            .checked_add(data.len())
            .is_none_or(|end| end > self.size)
        {
            return Err(FlashIoError::OutOfRange);
        }
        let _guard = self.lock.lock();
        let mut position = offset;
        let mut remaining = data;
        while !remaining.is_empty() {
            let page_room = PAGE_SIZE - (position % PAGE_SIZE);
            let chunk = &remaining[..remaining.len().min(page_room)];
            self.page_program(position, chunk)?;
            position += chunk.len();
            remaining = &remaining[chunk.len()..];
        }
        Ok(())
    }

    fn erase(&self, offset: usize, len: usize) -> Result<(), FlashIoError> {
        if offset % SECTOR_SIZE != 0 || len % SECTOR_SIZE != 0 {
            return Err(FlashIoError::Invalid);
        }
        if offset.checked_add(len).is_none_or(|end| end > self.size) {
            return Err(FlashIoError::OutOfRange);
        }
        let _guard = self.lock.lock();
        let mut position = offset;
        while position < offset + len {
            self.sector_erase(position)?;
            position += SECTOR_SIZE;
        }
        Ok(())
    }
}

// ─────────────────────────── PnP 驱动 ───────────────────────────

struct SpiNorBinding {
    master: Arc<SpiMaster>,
    handle: Option<FlashHandle>,
}

pub struct Ls2kSpiDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl Ls2kSpiDriver {
    pub const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_controller(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_LS_SPI)
    }

    fn matches_flash(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_JEDEC_SPI_NOR)
    }

    fn acquire_clock_hz(&self, info: &PlatformDeviceInfo) -> Result<u64, PnpError> {
        let clock = info
            .acquire_dtb_resource_at(PROP_CLOCKS, 0)
            .map_err(general::dev::dt_provider::DtbProviderError::into_pnp_error)?;
        clock
            .control(general::dev::dt_provider::DtbResourceRequest::Enable)
            .map_err(general::dev::dt_provider::DtbProviderError::into_pnp_error)?;
        match clock
            .control(general::dev::dt_provider::DtbResourceRequest::GetRate)
            .map_err(general::dev::dt_provider::DtbProviderError::into_pnp_error)?
        {
            general::dev::dt_provider::DtbResourceReply::Value(hz) if hz != 0 => Ok(hz),
            _ => Err(PnpError::hardware_failure("spi clock has no rate")),
        }
    }

    fn probe_controller(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let Some((phys, size)) = info.first_mmio() else {
            return Err(PnpError::missing(PnpResourceKind::Mmio, "spi reg missing"));
        };
        if size < 0x10 {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "spi register window too small",
            ));
        }
        let clk_hz = self.acquire_clock_hz(info)?;
        let master = Arc::new(SpiMaster::new((self.device_mmio_to_virt)(phys), clk_hz));
        master.reginit();
        let path = info.fw_path.as_deref().unwrap_or(&dev.name).to_owned();
        register_master(&path, Arc::clone(&master));
        log::printk!(
            "[ls2k-spi] bound controller {} phys={:#x} clk={} Hz",
            dev.id,
            phys,
            clk_hz
        );
        dev.set_driver_data(Arc::new(SpiNorBinding {
            master,
            handle: None,
        }));
        Ok(())
    }

    fn probe_flash(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let parent_path = dev.parent().and_then(|parent| {
            parent
                .info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .and_then(|info| info.fw_path.as_deref())
                .map(str::to_owned)
        });
        let master = lookup_master(parent_path.as_deref()).ok_or(PnpError::ProbeDeferred)?;

        let flash = Arc::new(SpiNorFlash::new(
            Arc::clone(&master),
            0,
            String::from("spi-nor"),
        ));
        let id = flash
            .read_id()
            .map_err(|_| PnpError::hardware_failure("spi-nor id read failed"))?;
        let Some(size) = nor_size_from_capacity(id[2]) else {
            log::printk!(
                "[ls2k-spi] unknown SPI-NOR JEDEC id {:02x} {:02x} {:02x}",
                id[0],
                id[1],
                id[2]
            );
            return Err(PnpError::hardware_failure("unknown SPI-NOR device"));
        };
        let name = alloc::format!("{}_{:02x}{:02x}", "spi-nor", id[0], id[1]);
        let flash = Arc::new(SpiNorFlash::new(Arc::clone(&master), size, name.clone()));
        let handle = register_v2(
            Arc::clone(&flash) as Arc<dyn FlashDevice>,
            flash as Arc<dyn FlashDeviceV2>,
        )
        .map_err(|_| PnpError::OutOfMemory)?;

        log::printk!(
            "[ls2k-spi] bound spi-nor {} (jedec {:02x} {:02x} {:02x}) size={} bytes parent={:?}",
            dev.id,
            id[0],
            id[1],
            id[2],
            size,
            parent_path,
        );
        dev.set_driver_data(Arc::new(SpiNorBinding {
            master,
            handle: Some(handle),
        }));
        Ok(())
    }
}

impl PnpDriver for Ls2kSpiDriver {
    fn name(&self) -> &'static str {
        "platform-ls2k-spi"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        if !matches!(id, PnpId::Platform { .. }) {
            return false;
        }
        let Some(info) = info.as_any().downcast_ref::<PlatformDeviceInfo>() else {
            return false;
        };
        Self::matches_controller(info) || Self::matches_flash(info)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        if Self::matches_controller(info) {
            self.probe_controller(dev)
        } else {
            self.probe_flash(dev)
        }
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<SpiNorBinding>()
            && let Some(handle) = binding.handle
        {
            let _ = general::dev::flash::unregister_v2(handle);
        }
        log::printk!("[ls2k-spi] removed {}", dev.id);
    }
}

struct Ls2kSpiFactory;

impl DriverFactory for Ls2kSpiFactory {
    fn name(&self) -> &'static str {
        "platform-ls2k-spi"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Ls2kSpiDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(Ls2kSpiFactory))
}

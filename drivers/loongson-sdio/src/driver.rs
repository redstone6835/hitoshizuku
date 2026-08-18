//! LS2K1000 SDIO 主机与 SD/eMMC 块设备驱动。
//!
//! 初始化和 I/O 都通过标准 `BlockDriver` 发布。APB-DMA provider 可用时使用单个
//! DMA 中转缓冲；固件禁用或没有 DMA 提供方时使用同一缓冲执行 PIO。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::num::NonZeroU32;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering, fence};

use spin::Mutex;

use crate::alloc_mmc_dev_name;
use crate::dev::bio::{Bio, BioIoError, BioOp, BioReqError, SubmitError};
use crate::dev::block::{
    BlockAttributes, BlockClass, BlockDevice, BlockDeviceInit, BlockDriver, BlockFeatures,
    BlockGeometry, BlockLimits,
};
use crate::dev::dma::{DmaBuffer, DmaDirection};
use crate::dev::dt_provider::{
    DtbProviderError, DtbResourceLease, DtbResourceReply, DtbResourceRequest,
};
use crate::dev::function::BlockFunction;
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResource, PnpResourceKind, PnpResourceReleaseError,
    PnpResourceReleaseOrder, register_driver_factory,
};
use crate::protocol::{
    Command, DataDirection, ResponseType, card_status_has_error, emmc_sector_count,
    r6_relative_address, sd_sector_count, transfer_argument,
};
use crate::registers::{
    Ls2kSdioLayout, Ls2kSdioRegisters, command_control, data_control, dma_data_control, prescaler,
};

const COMPAT_LS2K_SDIO: &str = "loongson,ls2k_sdio";
const PROP_CLOCKS: &str = "clocks";
const PROP_DMAS: &str = "dmas";
const DMA_NAME: &str = "sdio_rw";

const BLOCK_SIZE: usize = 512;
const MAX_BLOCKS_PER_IO: u32 = 128;
const STAGING_BYTES: usize = BLOCK_SIZE * MAX_BLOCKS_PER_IO as usize;
const INIT_CLOCK_HZ: u64 = 400_000;
const TRANSFER_CLOCK_HZ: u64 = 25_000_000;
const COMMAND_TIMEOUT_NS: u64 = 1_000_000_000;
const DATA_TIMEOUT_NS: u64 = 10_000_000_000;
const CARD_INIT_RETRIES: usize = 100;
const CARD_INIT_RETRY_NS: u64 = 10_000_000;

const CONTROL_RESET: u32 = 1 << 8;
const CONTROL_FIFO_RESET: u32 = 1 << 1;
const CONTROL_CLOCK_ENABLE: u32 = 1;
const PRESCALER_REVERSE_CLOCK: u32 = 1 << 31;

const COMMAND_STATUS_CRC_FAILED: u32 = 1 << 12;
const COMMAND_STATUS_SENT: u32 = 1 << 11;
const COMMAND_STATUS_TIMEOUT: u32 = 1 << 10;
const COMMAND_STATUS_RESPONSE_FINISHED: u32 = 1 << 9;
const COMMAND_STATUS_CLEAR: u32 = COMMAND_STATUS_CRC_FAILED
    | COMMAND_STATUS_SENT
    | COMMAND_STATUS_TIMEOUT
    | COMMAND_STATUS_RESPONSE_FINISHED;

const DATA_STATUS_FIFO_FAILED: u32 = 1 << 8;
const DATA_STATUS_CRC_FAILED: u32 = (1 << 7) | (1 << 6);
const DATA_STATUS_TIMEOUT: u32 = 1 << 5;
const DATA_STATUS_FINISHED: u32 = 1 << 4;
const DATA_STATUS_CLEAR: u32 = 0x7ff;

const FIFO_TX_FULL: u32 = 1 << 11;
const FIFO_RX_LAST: u32 = 1 << 9;
const FIFO_RX_FULL: u32 = 1 << 8;
const FIFO_COUNT_MASK: u32 = 0x7f;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CardKind {
    Sd,
    Emmc,
}

#[derive(Clone, Copy, Debug)]
struct CardInfo {
    kind: CardKind,
    rca: u16,
    sectors: u64,
    high_capacity: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SdioError {
    Timeout,
    Crc,
    CardStatus,
    Protocol,
    Hardware,
    Unsupported,
}

impl SdioError {
    const fn bio(self) -> BioIoError {
        match self {
            Self::Timeout => BioIoError::Timeout,
            Self::Crc | Self::CardStatus | Self::Protocol => BioIoError::MediaError,
            Self::Hardware => BioIoError::Unavailable,
            Self::Unsupported => BioIoError::Unsupported,
        }
    }
}

struct SharedLease {
    lease: DtbResourceLease,
}

impl Drop for SharedLease {
    fn drop(&mut self) {
        let _ = self.lease.control(DtbResourceRequest::Disable);
    }
}

impl SharedLease {
    fn done(&self, request: DtbResourceRequest<'_>) -> Result<(), DtbProviderError> {
        match self.lease.control(request)? {
            DtbResourceReply::Done => Ok(()),
            _ => Err(DtbProviderError::HardwareFailure),
        }
    }

    fn value(&self, request: DtbResourceRequest<'_>) -> Result<u64, DtbProviderError> {
        match self.lease.control(request)? {
            DtbResourceReply::Value(value) => Ok(value),
            _ => Err(DtbProviderError::HardwareFailure),
        }
    }
}

struct SharedLeasePnpResource {
    lease: Option<Arc<SharedLease>>,
    label: &'static str,
}

impl SharedLeasePnpResource {
    fn new(lease: Arc<SharedLease>, label: &'static str) -> Self {
        Self {
            lease: Some(lease),
            label,
        }
    }
}

impl PnpResource for SharedLeasePnpResource {
    fn kind(&self) -> PnpResourceKind {
        PnpResourceKind::Other("dt-provider-lease")
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
        self.lease
            .as_ref()
            .ok_or_else(|| PnpResourceReleaseError::new(self.kind(), self.label, "lease missing"))?
            .lease
            .prepare_pnp_release()
            .map_err(|_| {
                PnpResourceReleaseError::new(self.kind(), self.label, "lease cannot be frozen")
            })
    }

    fn cancel_release(&self) {
        if let Some(lease) = self.lease.as_ref() {
            lease.lease.cancel_pnp_release();
        }
    }

    fn release_order(&self) -> PnpResourceReleaseOrder {
        PnpResourceReleaseOrder::Consumer
    }

    fn release(mut self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        drop(self.lease.take());
        Ok(())
    }
}

struct SdioHost {
    registers: Ls2kSdioRegisters,
    fifo_phys: u32,
    clock: Arc<SharedLease>,
    dma: Option<Arc<SharedLease>>,
    staging: DmaBuffer,
    card: CardInfo,
    bus_width: u8,
    stopped: bool,
}

impl SdioHost {
    fn initialize(
        layout: Ls2kSdioLayout,
        phys: usize,
        clock: Arc<SharedLease>,
        dma: Option<Arc<SharedLease>>,
        dma_context: crate::dev::dma::DmaContext,
    ) -> Result<Self, &'static str> {
        clock
            .done(DtbResourceRequest::Enable)
            .map_err(|_| "failed to enable SDIO clock")?;
        let input_hz = clock
            .value(DtbResourceRequest::GetRate)
            .map_err(|_| "failed to read SDIO clock")?;
        if input_hz == 0 {
            return Err("SDIO clock rate is zero");
        }
        let staging =
            DmaBuffer::new_in(dma_context, STAGING_BYTES, 32, DmaDirection::Bidirectional)
                .map_err(|_| "failed to allocate SDIO staging buffer")?;
        let fifo_phys = phys
            .checked_add(0x40)
            .and_then(|address| u32::try_from(address).ok())
            .ok_or("SDIO FIFO address exceeds APB-DMA width")?;
        let registers = layout.registers();
        write32(registers.interrupt_enable, 0);
        write32(registers.control, CONTROL_RESET);
        delay_ns(1_000_000);
        write32(registers.control, CONTROL_FIFO_RESET);
        let (initial_divisor, _) =
            prescaler(input_hz, INIT_CLOCK_HZ).ok_or("invalid SDIO initialization clock")?;
        write32(
            registers.prescaler,
            initial_divisor | PRESCALER_REVERSE_CLOCK,
        );
        write32(registers.control, CONTROL_FIFO_RESET | CONTROL_CLOCK_ENABLE);
        let mut host = Self {
            registers,
            fifo_phys,
            clock,
            dma,
            staging,
            card: CardInfo {
                kind: CardKind::Sd,
                rca: 0,
                sectors: 0,
                high_capacity: false,
            },
            bus_width: 1,
            stopped: false,
        };
        host.send_command(Command::new(0, 0, ResponseType::None, None), 0)
            .map_err(|_| "SDIO card reset command failed")?;
        host.card = match host.initialize_sd() {
            Ok(card) => card,
            Err(_) => {
                host.reset_command_path();
                host.bus_width = 1;
                host.send_command(Command::new(0, 0, ResponseType::None, None), 0)
                    .map_err(|_| "eMMC reset command failed")?;
                host.initialize_emmc()
                    .map_err(|_| "SDIO did not identify an SD or eMMC card")?
            }
        };
        let (transfer_divisor, _) =
            prescaler(input_hz, TRANSFER_CLOCK_HZ).ok_or("invalid SDIO transfer clock")?;
        write32(
            registers.prescaler,
            transfer_divisor | PRESCALER_REVERSE_CLOCK,
        );
        Ok(host)
    }

    fn initialize_sd(&mut self) -> Result<CardInfo, SdioError> {
        let version_two = self
            .send_command(Command::new(8, 0x1aa, ResponseType::R7, None), 0)
            .is_ok_and(|response| response[0] & 0xfff == 0x1aa);
        let mut ocr = 0;
        for _ in 0..CARD_INIT_RETRIES {
            self.send_command(Command::new(55, 0, ResponseType::R1, None), 0)?;
            ocr = self.send_command(
                Command::new(
                    41,
                    0x00ff_8000 | if version_two { 1 << 30 } else { 0 },
                    ResponseType::R3,
                    None,
                ),
                0,
            )?[0];
            if ocr & (1 << 31) != 0 {
                break;
            }
            delay_ns(CARD_INIT_RETRY_NS);
        }
        if ocr & (1 << 31) == 0 {
            return Err(SdioError::Timeout);
        }
        self.send_command(Command::new(2, 0, ResponseType::R2, None), 0)?;
        let rca_response = self.send_command(Command::new(3, 0, ResponseType::R6, None), 0)?[0];
        let rca = r6_relative_address(rca_response).map_err(|_| SdioError::CardStatus)?;
        let csd = self.send_command(
            Command::new(9, u32::from(rca) << 16, ResponseType::R2, None),
            0,
        )?;
        let sectors = sd_sector_count(csd).map_err(|_| SdioError::Protocol)?;
        self.send_command(
            Command::new(7, u32::from(rca) << 16, ResponseType::R1b, None),
            0,
        )?;
        self.send_command(
            Command::new(55, u32::from(rca) << 16, ResponseType::R1, None),
            0,
        )?;
        if self
            .send_command(Command::new(6, 2, ResponseType::R1, None), 0)
            .is_ok()
        {
            self.bus_width = 4;
        }
        let high_capacity = ocr & (1 << 30) != 0;
        if !high_capacity {
            self.send_command(Command::new(16, 512, ResponseType::R1, None), 0)?;
        }
        Ok(CardInfo {
            kind: CardKind::Sd,
            rca,
            sectors,
            high_capacity,
        })
    }

    fn initialize_emmc(&mut self) -> Result<CardInfo, SdioError> {
        let mut ocr = 0;
        for _ in 0..CARD_INIT_RETRIES {
            ocr = self.send_command(Command::new(1, 0x40ff_8000, ResponseType::R3, None), 0)?[0];
            if ocr & (1 << 31) != 0 {
                break;
            }
            delay_ns(CARD_INIT_RETRY_NS);
        }
        if ocr & (1 << 31) == 0 {
            return Err(SdioError::Timeout);
        }
        self.send_command(Command::new(2, 0, ResponseType::R2, None), 0)?;
        let rca = 1u16;
        self.send_command(
            Command::new(3, u32::from(rca) << 16, ResponseType::R1, None),
            0,
        )?;
        let csd = self.send_command(
            Command::new(9, u32::from(rca) << 16, ResponseType::R2, None),
            0,
        )?;
        let legacy_sectors = sd_sector_count(csd).ok();
        self.send_command(
            Command::new(7, u32::from(rca) << 16, ResponseType::R1b, None),
            0,
        )?;
        let sectors = match self.send_command(
            Command::new(8, 0, ResponseType::R1, Some(DataDirection::Read)),
            BLOCK_SIZE,
        ) {
            Ok(_) => emmc_sector_count(&self.staging.as_slice()[..BLOCK_SIZE]).ok(),
            Err(_) => None,
        }
        .or(legacy_sectors)
        .ok_or(SdioError::Protocol)?;
        let high_capacity = ocr & (1 << 30) != 0;
        if !high_capacity {
            self.send_command(Command::new(16, 512, ResponseType::R1, None), 0)?;
        }
        Ok(CardInfo {
            kind: CardKind::Emmc,
            rca,
            sectors,
            high_capacity,
        })
    }

    fn transfer(
        &mut self,
        lba: u64,
        blocks: u32,
        direction: DataDirection,
    ) -> Result<(), SdioError> {
        let argument =
            transfer_argument(lba, self.card.high_capacity).map_err(|_| SdioError::Protocol)?;
        let index = match (direction, blocks > 1) {
            (DataDirection::Read, false) => 17,
            (DataDirection::Read, true) => 18,
            (DataDirection::Write, false) => 24,
            (DataDirection::Write, true) => 25,
        };
        let bytes = usize::try_from(blocks)
            .ok()
            .and_then(|count| count.checked_mul(BLOCK_SIZE))
            .ok_or(SdioError::Protocol)?;
        let result = self.send_command(
            Command::new(index, argument, ResponseType::R1, Some(direction)),
            bytes,
        );
        if blocks > 1 {
            let stop = self.send_command(Command::new(12, 0, ResponseType::R1b, None), 0);
            result?;
            stop?;
        } else {
            result?;
        }
        if direction == DataDirection::Write {
            self.wait_card_ready()?;
        }
        Ok(())
    }

    fn wait_card_ready(&mut self) -> Result<(), SdioError> {
        let deadline = hal::time::monotonic_ns().saturating_add(COMMAND_TIMEOUT_NS);
        loop {
            let status = self.send_command(
                Command::new(13, u32::from(self.card.rca) << 16, ResponseType::R1, None),
                0,
            )?[0];
            let state = (status >> 9) & 0xf;
            if status & (1 << 8) != 0 && state == 4 {
                return Ok(());
            }
            if hal::time::monotonic_ns() >= deadline {
                return Err(SdioError::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    fn send_command(&mut self, command: Command, data_bytes: usize) -> Result<[u32; 4], SdioError> {
        if command.index > 63 || (command.data.is_some() != (data_bytes != 0)) {
            return Err(SdioError::Protocol);
        }
        let blocks = if data_bytes == 0 {
            0
        } else {
            if !data_bytes.is_multiple_of(BLOCK_SIZE) || data_bytes > self.staging.len() {
                return Err(SdioError::Protocol);
            }
            u32::try_from(data_bytes / BLOCK_SIZE).map_err(|_| SdioError::Protocol)?
        };
        self.reset_command_path();
        if let Some(direction) = command.data {
            write32(self.registers.block_size, BLOCK_SIZE as u32);
            write32(self.registers.data_timer, u32::MAX);
            let control = if self.dma.is_some() {
                dma_data_control(self.bus_width, blocks)
            } else {
                data_control(direction, self.bus_width, blocks)
            }
            .map_err(|_| SdioError::Protocol)?;
            write32(self.registers.data_control, control);
            self.prepare_dma(direction, data_bytes)?;
        } else {
            write32(self.registers.data_control, 0);
        }
        write32(self.registers.command_argument, command.argument);
        fence(Ordering::Release);
        write32(self.registers.command_control, command_control(command));

        let timeout = if data_bytes == 0 {
            COMMAND_TIMEOUT_NS
        } else {
            DATA_TIMEOUT_NS
        };
        let deadline = hal::time::monotonic_ns().saturating_add(timeout);
        let mut response_done = false;
        let mut transferred = 0usize;
        loop {
            let command_status = read32(self.registers.command_status);
            if command_status & COMMAND_STATUS_TIMEOUT != 0 {
                self.stop_dma();
                return Err(SdioError::Timeout);
            }
            if command_status & COMMAND_STATUS_CRC_FAILED != 0
                && command.response.requires_crc_check()
            {
                self.stop_dma();
                return Err(SdioError::Crc);
            }
            response_done |= if command.response.is_present() {
                command_status & COMMAND_STATUS_RESPONSE_FINISHED != 0
            } else {
                command_status & COMMAND_STATUS_SENT != 0
            };

            if let Some(direction) = command.data {
                if self.dma.is_none() {
                    transferred = self.pio_step(direction, transferred, data_bytes);
                }
                let data_status = read32(self.registers.data_status);
                if data_status & DATA_STATUS_TIMEOUT != 0 {
                    self.stop_dma();
                    return Err(SdioError::Timeout);
                }
                if data_status & (DATA_STATUS_FIFO_FAILED | DATA_STATUS_CRC_FAILED) != 0 {
                    self.stop_dma();
                    return Err(SdioError::Crc);
                }
                let payload_done = if self.dma.is_some() {
                    data_status & DATA_STATUS_FINISHED != 0
                } else {
                    transferred == data_bytes && data_status & DATA_STATUS_FINISHED != 0
                };
                if response_done && payload_done {
                    self.finish_dma(direction);
                    break;
                }
            } else if response_done {
                break;
            }
            if hal::time::monotonic_ns() >= deadline {
                self.stop_dma();
                return Err(SdioError::Timeout);
            }
            core::hint::spin_loop();
        }
        fence(Ordering::Acquire);
        let response = self.read_response();
        write32(self.registers.command_status, COMMAND_STATUS_CLEAR);
        write32(self.registers.data_status, DATA_STATUS_CLEAR);
        if command.response.has_card_status() && card_status_has_error(response[0]) {
            return Err(SdioError::CardStatus);
        }
        Ok(response)
    }

    fn prepare_dma(
        &mut self,
        direction: DataDirection,
        data_bytes: usize,
    ) -> Result<(), SdioError> {
        let Some(dma) = self.dma.as_ref() else {
            return Ok(());
        };
        if direction == DataDirection::Write {
            self.staging.sync_for_device();
        }
        let memory = self.staging.dma_addr() as u64;
        let config = [
            match direction {
                DataDirection::Read => 0,
                DataDirection::Write => 1,
            },
            memory as u32,
            (memory >> 32) as u32,
            self.fifo_phys,
            data_bytes as u32,
        ];
        dma.done(DtbResourceRequest::Configure(&config))
            .and_then(|_| dma.done(DtbResourceRequest::Enable))
            .map_err(|_| SdioError::Hardware)
    }

    fn finish_dma(&mut self, direction: DataDirection) {
        self.stop_dma();
        if direction == DataDirection::Read {
            self.staging.sync_for_cpu();
        }
    }

    fn stop_dma(&self) {
        if let Some(dma) = self.dma.as_ref() {
            let _ = dma.done(DtbResourceRequest::Disable);
        }
    }

    fn pio_step(&mut self, direction: DataDirection, mut offset: usize, end: usize) -> usize {
        let fifo_status = read32(self.registers.fifo_status);
        match direction {
            DataDirection::Read => {
                let available = usize::try_from(fifo_status & FIFO_COUNT_MASK).unwrap_or(0);
                if offset + 4 <= end
                    && (available >= 4 || fifo_status & (FIFO_RX_FULL | FIFO_RX_LAST) != 0)
                {
                    let word = read32(self.registers.fifo).to_le_bytes();
                    self.staging.as_mut_slice()[offset..offset + 4].copy_from_slice(&word);
                    offset += 4;
                }
            }
            DataDirection::Write => {
                if offset + 4 <= end && fifo_status & FIFO_TX_FULL == 0 {
                    let word = u32::from_le_bytes(
                        self.staging.as_slice()[offset..offset + 4]
                            .try_into()
                            .expect("PIO word has fixed length"),
                    );
                    write32(self.registers.fifo, word);
                    offset += 4;
                }
            }
        }
        offset
    }

    fn read_response(&self) -> [u32; 4] {
        self.registers.response.map(read32)
    }

    fn reset_command_path(&self) {
        self.stop_dma();
        write32(self.registers.command_control, 0);
        write32(self.registers.command_status, COMMAND_STATUS_CLEAR);
        write32(self.registers.data_status, DATA_STATUS_CLEAR);
        write32(self.registers.interrupt_status, u32::MAX);
    }

    fn shutdown(&mut self) {
        if self.stopped {
            return;
        }
        self.stop_dma();
        write32(self.registers.interrupt_enable, 0);
        write32(self.registers.command_control, 0);
        write32(self.registers.data_control, 0);
        write32(self.registers.control, CONTROL_RESET);
        let _ = self.clock.done(DtbResourceRequest::Disable);
        self.stopped = true;
    }
}

impl Drop for SdioHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct SdioBlockIo {
    host: Arc<Mutex<SdioHost>>,
    gone: Arc<AtomicBool>,
}

impl BlockDriver for SdioBlockIo {
    fn queue_bio(&self, mut bio: Bio) -> Result<(), (SubmitError, Bio)> {
        if self.gone.load(Ordering::Acquire) {
            return Err((SubmitError::DeviceGone, bio));
        }
        if bio.op.needs_data() && bio.range.blocks == 0 {
            return Err((SubmitError::InvalidRequest(BioReqError::EmptyRange), bio));
        }
        if bio.op.needs_data() && bio.range.blocks > MAX_BLOCKS_PER_IO {
            return Err((SubmitError::InvalidRequest(BioReqError::TooLarge), bio));
        }
        let bytes = bio.range.blocks as usize * BLOCK_SIZE;
        if bio.op.needs_data() && bio.buffer.len() != bytes {
            return Err((
                SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch),
                bio,
            ));
        }
        let result = {
            let mut host = self.host.lock();
            if host.stopped {
                Err(SdioError::Hardware)
            } else {
                match bio.op {
                    BioOp::Read => {
                        let result =
                            host.transfer(bio.range.lba, bio.range.blocks, DataDirection::Read);
                        if result.is_ok()
                            && !bio
                                .buffer
                                .copy_from_contiguous(&host.staging.as_slice()[..bytes])
                        {
                            Err(SdioError::Protocol)
                        } else {
                            result
                        }
                    }
                    BioOp::Write => {
                        if !bio
                            .buffer
                            .copy_to_contiguous(&mut host.staging.as_mut_slice()[..bytes])
                        {
                            Err(SdioError::Protocol)
                        } else {
                            host.transfer(bio.range.lba, bio.range.blocks, DataDirection::Write)
                        }
                    }
                    BioOp::Flush => host.wait_card_ready(),
                    BioOp::Discard | BioOp::WriteZeroes => Err(SdioError::Unsupported),
                }
            }
        };
        bio.complete(result.map_err(SdioError::bio));
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct SdioBinding {
    host: Arc<Mutex<SdioHost>>,
    block: Arc<BlockDevice>,
    gone: Arc<AtomicBool>,
}

struct LoongsonSdioDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl LoongsonSdioDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_LS2K_SDIO)
    }
}

impl PnpDriver for LoongsonSdioDriver {
    fn name(&self) -> &'static str {
        "platform-loongson-sdio"
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
        let (phys, size) = exact_mmio(info)?;
        let layout = Ls2kSdioLayout::new((self.device_mmio_to_virt)(phys), size).map_err(|_| {
            PnpError::malformed(PnpResourceKind::Mmio, "invalid LS2K SDIO register window")
        })?;
        Ls2kSdioLayout::new(phys, size).map_err(|_| {
            PnpError::malformed(PnpResourceKind::Mmio, "invalid LS2K SDIO physical window")
        })?;

        let clock = Arc::new(SharedLease {
            lease: info
                .acquire_dtb_resource_at(PROP_CLOCKS, 0)
                .map_err(DtbProviderError::into_pnp_error)?,
        });
        let dma = match info.acquire_named_dtb_resource(PROP_DMAS, DMA_NAME) {
            Ok(lease) => Some(Arc::new(SharedLease { lease })),
            Err(DtbProviderError::Disabled | DtbProviderError::Invalid) => None,
            Err(error) => return Err(error.into_pnp_error()),
        };
        dev.reserve_owned_resources(1 + usize::from(dma.is_some()))?;
        dev.own_resource(SharedLeasePnpResource::new(
            Arc::clone(&clock),
            "loongson-sdio-clock",
        ))?;
        if let Some(dma) = dma.as_ref() {
            dev.own_resource(SharedLeasePnpResource::new(
                Arc::clone(dma),
                "loongson-sdio-dma",
            ))?;
        }

        let host = SdioHost::initialize(layout, phys, clock, dma, info.dma_context()).map_err(
            |error| {
                log::error!("[loongson-sdio] probe failed for {}: {}", dev.name, error);
                PnpError::hardware_failure("LS2K SDIO initialization failed")
            },
        )?;
        let card = host.card;
        let host = Arc::new(Mutex::new(host));
        let gone = Arc::new(AtomicBool::new(false));
        let dev_name = alloc_mmc_dev_name(&dev.name).map_err(PnpError::from)?;
        let block = create_block_device(&dev_name, card, Arc::clone(&host), Arc::clone(&gone))?;
        dev.register_function(BlockFunction::with_projection_name_arc(
            &dev.name,
            &dev_name,
            Arc::clone(&block),
        ))?;
        log::printk!(
            "[loongson-sdio] {:?} sectors={} mode={} -> /dev/{}",
            card.kind,
            card.sectors,
            if host.lock().dma.is_some() {
                "dma"
            } else {
                "pio"
            },
            dev_name
        );
        dev.set_driver_data(Arc::new(SdioBinding { host, block, gone }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        let Some(data) = dev.take_driver_data() else {
            return;
        };
        let Ok(binding) = Arc::downcast::<SdioBinding>(data) else {
            return;
        };
        binding.gone.store(true, Ordering::Release);
        binding.block.mark_gone();
        binding.host.lock().shutdown();
    }
}

fn create_block_device(
    name: &str,
    card: CardInfo,
    host: Arc<Mutex<SdioHost>>,
    gone: Arc<AtomicBool>,
) -> Result<Arc<BlockDevice>, PnpError> {
    let sector = NonZeroU32::new(BLOCK_SIZE as u32).expect("MMC sector size is non-zero");
    let geometry = BlockGeometry::new(sector, sector, Some(card.sectors)).ok_or(
        PnpError::registration_failed(PnpResourceKind::Function, "invalid MMC geometry"),
    )?;
    let max_blocks = NonZeroU32::new(MAX_BLOCKS_PER_IO).expect("MMC BIO limit is non-zero");
    let limits = BlockLimits::new(Some(max_blocks), None, None)
        .expect("MMC BIO limit is valid")
        .with_data_segment_limits(NonZeroU32::new(1), NonZeroU32::new(STAGING_BYTES as u32));
    let io: Arc<dyn BlockDriver> = Arc::new(SdioBlockIo { host, gone });
    Ok(Arc::new(BlockDevice::new(
        BlockDeviceInit {
            name,
            subsystem: "mmc",
            class: BlockClass::Whole,
            geometry,
            limits,
            attributes: BlockAttributes::new(
                card.kind == CardKind::Sd,
                false,
                NonZeroU32::new(1),
                None,
            ),
            features: BlockFeatures::FLUSH,
        },
        io,
        None,
    )))
}

fn platform_info(dev: &Arc<PnpDevice>) -> Result<&PlatformDeviceInfo, PnpError> {
    dev.info
        .as_any()
        .downcast_ref::<PlatformDeviceInfo>()
        .ok_or(PnpError::InvalidState)
}

fn exact_mmio(info: &PlatformDeviceInfo) -> Result<(usize, usize), PnpError> {
    let mut windows = info.mmio_resources();
    let window = windows.next().ok_or(PnpError::missing(
        PnpResourceKind::Mmio,
        "LS2K SDIO register window missing",
    ))?;
    if windows.next().is_some() {
        return Err(PnpError::malformed(
            PnpResourceKind::Mmio,
            "LS2K SDIO requires exactly one register window",
        ));
    }
    Ok(window)
}

fn read32(address: usize) -> u32 {
    // Safety: 探测阶段已校验寄存器窗口，平台总线映射在设备移除前保持有效。
    unsafe { read_volatile(address as *const u32) }
}

fn write32(address: usize, value: u32) {
    // Safety: 与 read32 相同，地址指向当前 SDIO 控制器拥有的 32 位 MMIO 寄存器。
    unsafe { write_volatile(address as *mut u32, value) }
}

fn delay_ns(duration: u64) {
    let deadline = hal::time::monotonic_ns().saturating_add(duration);
    while hal::time::monotonic_ns() < deadline {
        core::hint::spin_loop();
    }
}

struct LoongsonSdioFactory;

impl DriverFactory for LoongsonSdioFactory {
    fn name(&self) -> &'static str {
        "platform-loongson-sdio"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(LoongsonSdioDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(LoongsonSdioFactory))
}

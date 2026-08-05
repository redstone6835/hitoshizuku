//! CFI NOR flash platform ELM 驱动。
//!
//! platform 层只负责提供固件声明的 MMIO 窗口；本 ELM 驱动通过 CFI query 发现
//! command set 与 erase geometry，并把读、写、擦除能力注册到通用 flash 接口。
//! 当前实现支持 QEMU 与常见 NOR 使用的 Intel/Sharp command set，未知 command set
//! 会在 probe 阶段拒绝，避免把普通内存写误当成 flash program。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::mem::size_of;
use core::ptr::{read_volatile, write_volatile};

use vfs::sync::Spinlock;

use crate::dev::flash::{
    self, FlashCapabilities, FlashDevice, FlashDeviceV2, FlashEraseRegion, FlashError,
    FlashIoError, FlashWindow,
};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, register_driver_factory,
};

const COMPAT_CFI_FLASH: &str = "cfi-flash";
const PROP_BANK_WIDTH: &str = "bank-width";
const PROP_DEVICE_WIDTH: &str = "device-width";
const PROP_READ_ONLY: &str = "read-only";
const PROP_BIG_ENDIAN: &str = "big-endian";
const PROP_LITTLE_ENDIAN: &str = "little-endian";
const PROP_NATIVE_ENDIAN: &str = "native-endian";

const CFI_QUERY_ADDRESS: usize = 0x55;
const CFI_QUERY_COMMAND: u8 = 0x98;
const CFI_QUERY_Q: usize = 0x10;
const CFI_QUERY_R: usize = 0x11;
const CFI_QUERY_Y: usize = 0x12;
const CFI_PRIMARY_COMMAND_SET: usize = 0x13;
const CFI_DEVICE_SIZE: usize = 0x27;
const CFI_INTERFACE_DESCRIPTION: usize = 0x28;
const CFI_ERASE_REGION_COUNT: usize = 0x2c;
const CFI_ERASE_REGION_TABLE: usize = 0x2d;
const CFI_ERASE_REGION_ENTRY_SIZE: usize = 4;
const CFI_COMMAND_SET_INTEL_EXTENDED: u16 = 0x0001;
const CFI_COMMAND_SET_INTEL_STANDARD: u16 = 0x0003;

const CFI_CMD_PROGRAM: u8 = 0x40;
const CFI_CMD_ERASE_SETUP: u8 = 0x20;
const CFI_CMD_ERASE_CONFIRM: u8 = 0xd0;
const CFI_CMD_CLEAR_STATUS: u8 = 0x50;
const CFI_CMD_READ_ARRAY: u8 = 0xff;
const CFI_STATUS_READY: u8 = 0x80;
const CFI_STATUS_ERROR_MASK: u8 = 0x3a;
const CFI_POLL_LIMIT: usize = 1_000_000;

struct MappedFlashWindow {
    phys: usize,
    base: usize,
    size: usize,
    device_width: usize,
    device_type: usize,
    endian: CfiEndian,
    erase_regions: Vec<FlashEraseRegion>,
}

struct CfiFlash {
    name: Box<str>,
    bank_width: usize,
    windows: Vec<MappedFlashWindow>,
    erase_regions: Vec<FlashEraseRegion>,
    writable: bool,
    lock: Spinlock<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CfiEndian {
    Native,
    Little,
    Big,
}

/// CFI probe 同时区分当前总线模式与芯片的最大接口宽度。
///
/// 例如 x16 芯片以 x8 模式接入时，`device_width=1`，但 CFI query
/// 地址仍按 `device_type=2` 缩放。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CfiProbeGeometry {
    device_width: usize,
    device_type: usize,
    interleave: usize,
}

impl CfiProbeGeometry {
    fn query_stride(self) -> Option<usize> {
        self.device_type.checked_mul(self.interleave)
    }
}

impl CfiEndian {
    const fn command_byte(self, device_width: usize) -> Option<usize> {
        if device_width == 0 {
            return None;
        }
        match self {
            Self::Little => Some(0),
            Self::Big => Some(device_width - 1),
            Self::Native if cfg!(target_endian = "little") => Some(0),
            Self::Native => Some(device_width - 1),
        }
    }
}

fn read_bus_value(
    base: usize,
    size: usize,
    offset: usize,
    width: usize,
) -> Result<u64, FlashIoError> {
    let end = offset.checked_add(width).ok_or(FlashIoError::OutOfRange)?;
    if end > size {
        return Err(FlashIoError::OutOfRange);
    }
    let address = base.checked_add(offset).ok_or(FlashIoError::OutOfRange)?;
    // Safety: `base..base+size` 是 platform probe 映射的 flash MMIO 窗口，边界检查
    // 已覆盖本次访问，且所有调用点都按 bank width 对齐 offset。
    let value = unsafe {
        match width {
            1 => u64::from(read_volatile(address as *const u8)),
            2 => u64::from(read_volatile(address as *const u16)),
            4 => u64::from(read_volatile(address as *const u32)),
            8 => read_volatile(address as *const u64),
            _ => return Err(FlashIoError::Invalid),
        }
    };
    Ok(value)
}

fn write_bus_value(
    base: usize,
    size: usize,
    offset: usize,
    width: usize,
    value: u64,
) -> Result<(), FlashIoError> {
    let end = offset.checked_add(width).ok_or(FlashIoError::OutOfRange)?;
    if end > size {
        return Err(FlashIoError::OutOfRange);
    }
    let address = base.checked_add(offset).ok_or(FlashIoError::OutOfRange)?;
    // Safety: 与 `read_bus_value` 相同；目标是 CFI command/data 窗口，访问宽度来自
    // 已校验的 DT `bank-width`。
    unsafe {
        match width {
            1 => write_volatile(address as *mut u8, value as u8),
            2 => write_volatile(address as *mut u16, value as u16),
            4 => write_volatile(address as *mut u32, value as u32),
            8 => write_volatile(address as *mut u64, value),
            _ => return Err(FlashIoError::Invalid),
        }
    }
    Ok(())
}

fn replicated_command(
    command: u8,
    bank_width: usize,
    device_width: usize,
    endian: CfiEndian,
) -> Option<u64> {
    let command_byte = endian.command_byte(device_width)?;
    if bank_width == 0
        || bank_width > size_of::<u64>()
        || device_width > bank_width
        || !bank_width.is_multiple_of(device_width)
    {
        return None;
    }
    let mut bytes = [0u8; 8];
    for lane in (0..bank_width).step_by(device_width) {
        bytes[lane + command_byte] = command;
    }
    Some(u64::from_ne_bytes(bytes))
}

fn query_byte_from_bus(
    value: u64,
    bank_width: usize,
    device_width: usize,
    endian: CfiEndian,
) -> Result<u8, FlashIoError> {
    if bank_width > size_of::<u64>()
        || device_width == 0
        || device_width > bank_width
        || !bank_width.is_multiple_of(device_width)
    {
        return Err(FlashIoError::Invalid);
    }
    let command_byte = endian
        .command_byte(device_width)
        .ok_or(FlashIoError::Invalid)?;
    let bytes = value.to_ne_bytes();
    let first = bytes[command_byte];
    for lane in (0..bank_width).step_by(device_width) {
        if bytes[lane + command_byte] != first {
            return Err(FlashIoError::Invalid);
        }
    }
    Ok(first)
}

impl MappedFlashWindow {
    fn read_bus(&self, offset: usize, bank_width: usize) -> Result<u64, FlashIoError> {
        read_bus_value(self.base, self.size, offset, bank_width)
    }

    fn write_bus(&self, offset: usize, bank_width: usize, value: u64) -> Result<(), FlashIoError> {
        write_bus_value(self.base, self.size, offset, bank_width, value)
    }

    fn write_command(
        &self,
        offset: usize,
        bank_width: usize,
        command: u8,
    ) -> Result<(), FlashIoError> {
        let value = replicated_command(command, bank_width, self.device_width, self.endian)
            .ok_or(FlashIoError::Invalid)?;
        self.write_bus(offset, bank_width, value)
    }

    fn reset(&self, bank_width: usize) {
        let _ = self.write_command(0, bank_width, CFI_CMD_READ_ARRAY);
    }

    fn poll_ready(&self, offset: usize, bank_width: usize) -> Result<(), FlashIoError> {
        for _ in 0..CFI_POLL_LIMIT {
            let status = self.read_bus(offset, bank_width)?;
            let mut ready = true;
            let bytes = status.to_ne_bytes();
            let command_byte = self
                .endian
                .command_byte(self.device_width)
                .ok_or(FlashIoError::Invalid)?;
            for lane in (0..bank_width).step_by(self.device_width) {
                let lane_status = bytes[lane + command_byte];
                if lane_status & CFI_STATUS_ERROR_MASK != 0 {
                    return Err(FlashIoError::Io);
                }
                ready &= lane_status & CFI_STATUS_READY != 0;
            }
            if ready {
                return Ok(());
            }
            spin_loop();
        }
        Err(FlashIoError::Busy)
    }

    fn program_word(
        &self,
        offset: usize,
        bank_width: usize,
        value: u64,
    ) -> Result<(), FlashIoError> {
        self.write_command(offset, bank_width, CFI_CMD_CLEAR_STATUS)?;
        self.write_command(offset, bank_width, CFI_CMD_PROGRAM)?;
        self.write_bus(offset, bank_width, value)?;
        let result = self.poll_ready(offset, bank_width);
        self.reset(bank_width);
        result
    }

    fn erase_block(&self, offset: usize, bank_width: usize) -> Result<(), FlashIoError> {
        self.write_command(offset, bank_width, CFI_CMD_CLEAR_STATUS)?;
        self.write_command(offset, bank_width, CFI_CMD_ERASE_SETUP)?;
        self.write_command(offset, bank_width, CFI_CMD_ERASE_CONFIRM)?;
        let result = self.poll_ready(offset, bank_width);
        self.reset(bank_width);
        result
    }

    fn erase_block_at(&self, offset: usize) -> Option<usize> {
        self.erase_regions.iter().find_map(|region| {
            let region_size = region.block_size.checked_mul(region.block_count)?;
            let end = region.offset.checked_add(region_size)?;
            if offset < region.offset || offset >= end {
                return None;
            }
            (offset - region.offset)
                .is_multiple_of(region.block_size)
                .then_some(region.block_size)
        })
    }
}

impl CfiFlash {
    fn total_size(&self) -> Option<usize> {
        self.windows
            .iter()
            .try_fold(0usize, |total, window| total.checked_add(window.size))
    }

    fn locate(&self, mut offset: usize) -> Option<(usize, usize, usize)> {
        for (index, window) in self.windows.iter().enumerate() {
            if offset < window.size {
                return Some((index, offset, window.size - offset));
            }
            offset -= window.size;
        }
        None
    }

    fn reset_all(&self) {
        for window in &self.windows {
            window.reset(self.bank_width);
        }
    }
}

impl FlashDevice for CfiFlash {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> FlashCapabilities {
        FlashCapabilities {
            readable: true,
            writable: self.writable,
            erasable: self.writable && !self.erase_regions.is_empty(),
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

        let _guard = self.lock.lock();
        self.reset_all();
        let mut done = 0usize;
        while done < out.len() {
            let (window_index, local_offset, available) =
                self.locate(offset).ok_or(FlashError::OutOfRange)?;
            let window = &self.windows[window_index];
            let addr = window
                .base
                .checked_add(local_offset)
                .ok_or(FlashError::OutOfRange)?;
            let count = available.min(out.len() - done);
            for index in 0..count {
                let byte_addr = addr.checked_add(index).ok_or(FlashError::OutOfRange)?;
                // Safety: `locate` 只返回 probe 时由固件声明并映射的 flash 窗口地址，
                // 上面的总长度和逐窗口边界检查保证本次单字节读取没有越界。
                out[done + index] = unsafe { read_volatile(byte_addr as *const u8) };
            }
            done += count;
            offset += count;
        }
        Ok(())
    }
}

impl FlashDeviceV2 for CfiFlash {
    fn erase_region_count(&self) -> usize {
        self.erase_regions.len()
    }

    fn erase_region_at(&self, index: usize) -> Option<FlashEraseRegion> {
        self.erase_regions.get(index).copied()
    }

    fn write(&self, mut offset: usize, data: &[u8]) -> Result<(), FlashIoError> {
        if !self.writable {
            return Err(FlashIoError::Unsupported);
        }
        let total_size = self.total_size().ok_or(FlashIoError::OutOfRange)?;
        let end = offset
            .checked_add(data.len())
            .ok_or(FlashIoError::OutOfRange)?;
        if end > total_size {
            return Err(FlashIoError::OutOfRange);
        }
        if data.is_empty() {
            return Ok(());
        }

        let _guard = self.lock.lock();
        self.reset_all();
        let result = (|| {
            let mut done = 0usize;
            while done < data.len() {
                let (window_index, local_offset, _) =
                    self.locate(offset).ok_or(FlashIoError::OutOfRange)?;
                let window = &self.windows[window_index];
                let word_offset = local_offset & !(self.bank_width - 1);
                let in_word = local_offset - word_offset;
                let count = (self.bank_width - in_word).min(data.len() - done);
                let old = window.read_bus(word_offset, self.bank_width)?;
                // `old` 是 native-endian volatile load 的数值；转成 native 字节序后
                // 数组下标才与 flash 窗口的递增字节地址一致。
                let mut bytes = old.to_ne_bytes();
                for index in 0..count {
                    let new = data[done + index];
                    let old = bytes[in_word + index];
                    if old & new != new {
                        return Err(FlashIoError::NeedsErase);
                    }
                    bytes[in_word + index] = new;
                }
                let value = u64::from_ne_bytes(bytes);
                window.program_word(word_offset, self.bank_width, value)?;
                done += count;
                offset += count;
            }
            Ok(())
        })();
        self.reset_all();
        result
    }

    fn erase(&self, mut offset: usize, len: usize) -> Result<(), FlashIoError> {
        if !self.writable || self.erase_regions.is_empty() {
            return Err(FlashIoError::Unsupported);
        }
        let total_size = self.total_size().ok_or(FlashIoError::OutOfRange)?;
        let end = offset.checked_add(len).ok_or(FlashIoError::OutOfRange)?;
        if end > total_size {
            return Err(FlashIoError::OutOfRange);
        }
        if len == 0 {
            return Ok(());
        }

        let _guard = self.lock.lock();
        self.reset_all();
        let result = (|| {
            let mut remaining = len;
            while remaining != 0 {
                let (window_index, local_offset, _) =
                    self.locate(offset).ok_or(FlashIoError::OutOfRange)?;
                let window = &self.windows[window_index];
                let block_size = window
                    .erase_block_at(local_offset)
                    .ok_or(FlashIoError::Invalid)?;
                if block_size > remaining {
                    return Err(FlashIoError::Invalid);
                }
                window.erase_block(local_offset, self.bank_width)?;
                offset += block_size;
                remaining -= block_size;
            }
            Ok(())
        })();
        self.reset_all();
        result
    }
}

fn cfi_query_byte(
    base: usize,
    size: usize,
    bank_width: usize,
    device_width: usize,
    endian: CfiEndian,
    query_stride: usize,
    index: usize,
) -> Result<u8, FlashIoError> {
    let offset = index
        .checked_mul(query_stride)
        .ok_or(FlashIoError::OutOfRange)?;
    query_byte_from_bus(
        read_bus_value(base, size, offset, bank_width)?,
        bank_width,
        device_width,
        endian,
    )
}

fn cfi_query_u16(
    base: usize,
    size: usize,
    bank_width: usize,
    device_width: usize,
    endian: CfiEndian,
    query_stride: usize,
    index: usize,
) -> Result<u16, FlashIoError> {
    let low = cfi_query_byte(
        base,
        size,
        bank_width,
        device_width,
        endian,
        query_stride,
        index,
    )?;
    let high = cfi_query_byte(
        base,
        size,
        bank_width,
        device_width,
        endian,
        query_stride,
        index + 1,
    )?;
    Ok(u16::from_le_bytes([low, high]))
}

fn write_geometry_command(
    base: usize,
    size: usize,
    offset: usize,
    bank_width: usize,
    device_width: usize,
    endian: CfiEndian,
    command: u8,
) -> Result<(), FlashIoError> {
    let value = replicated_command(command, bank_width, device_width, endian)
        .ok_or(FlashIoError::Invalid)?;
    write_bus_value(base, size, offset, bank_width, value)
}

fn reset_geometry(
    base: usize,
    size: usize,
    bank_width: usize,
    device_width: usize,
    endian: CfiEndian,
) {
    // CFI 通用 probe 同时发送 AMD/Fujitsu 的 0xf0 和 Intel/Sharp 的
    // 0xff。本驱动只在识别 Intel command set 后暴露擦写能力，但探测
    // 阶段仍要保证上一个候选几何留下的 query mode 被撤销。
    let _ = write_geometry_command(base, size, 0, bank_width, device_width, endian, 0xf0);
    let _ = write_geometry_command(
        base,
        size,
        0,
        bank_width,
        device_width,
        endian,
        CFI_CMD_READ_ARRAY,
    );
}

fn query_signature_present(
    base: usize,
    size: usize,
    bank_width: usize,
    device_width: usize,
    endian: CfiEndian,
    query_stride: usize,
) -> Result<bool, FlashIoError> {
    for (index, expected) in [
        (CFI_QUERY_Q, b'Q'),
        (CFI_QUERY_R, b'R'),
        (CFI_QUERY_Y, b'Y'),
    ] {
        match cfi_query_byte(
            base,
            size,
            bank_width,
            device_width,
            endian,
            query_stride,
            index,
        ) {
            Ok(value) if value == expected => {}
            Ok(_) | Err(FlashIoError::Invalid) => return Ok(false),
            Err(err) => return Err(err),
        }
    }
    Ok(true)
}

fn interface_supports_width(interface: u16, device_width: usize) -> bool {
    match interface {
        0x0000 => device_width == 1,
        0x0001 => device_width == 2,
        0x0002 => matches!(device_width, 1 | 2),
        0x0003 => device_width == 4,
        0x0004 => matches!(device_width, 2 | 4),
        _ => false,
    }
}

fn interface_supports_geometry(
    interface: u16,
    bank_width: usize,
    device_width: usize,
    device_width_was_declared: bool,
) -> bool {
    if interface_supports_width(interface, device_width) {
        return true;
    }
    // QEMU 为旧 machine 保留了一种 CFI01 布局：32-bit bank 只在最低
    // byte lane 返回 query 数据，geometry 也描述整个 bank，但 interface 仍填
    // x8/x16。Linux CFI probe 会把它当作单个 x32 bank 使用。只在 DT 没有
    // 明示 device-width、且完整 QRY 签名确实以 x32 几何响应时放行这一
    // 有界兼容形式；其它自相矛盾的 interface 仍 fail closed。
    !device_width_was_declared && bank_width == 4 && device_width == 4 && interface == 0x0002
}

fn probe_cfi_window(
    phys: usize,
    base: usize,
    size: usize,
    bank_width: usize,
    device_width_hint: Option<usize>,
    endian: CfiEndian,
) -> Result<MappedFlashWindow, FlashIoError> {
    for interleave in [4usize, 2, 1]
        .into_iter()
        .filter(|interleave| *interleave <= bank_width && bank_width.is_multiple_of(*interleave))
    {
        let device_width = bank_width / interleave;
        if device_width_hint.is_some_and(|hint| hint != device_width) {
            continue;
        }
        for device_type in [1usize, 2, 4]
            .into_iter()
            .filter(|device_type| *device_type >= device_width)
        {
            let geometry = CfiProbeGeometry {
                device_width,
                device_type,
                interleave,
            };
            let query_stride = geometry.query_stride().ok_or(FlashIoError::OutOfRange)?;
            let query_offset = CFI_QUERY_ADDRESS
                .checked_mul(query_stride)
                .ok_or(FlashIoError::OutOfRange)?;

            reset_geometry(base, size, bank_width, device_width, endian);
            if let Err(err) = write_geometry_command(
                base,
                size,
                query_offset,
                bank_width,
                device_width,
                endian,
                CFI_QUERY_COMMAND,
            ) {
                reset_geometry(base, size, bank_width, device_width, endian);
                return Err(err);
            }
            let signature =
                query_signature_present(base, size, bank_width, device_width, endian, query_stride);
            let signature = match signature {
                Ok(signature) => signature,
                Err(err) => {
                    reset_geometry(base, size, bank_width, device_width, endian);
                    return Err(err);
                }
            };
            if !signature {
                reset_geometry(base, size, bank_width, device_width, endian);
                continue;
            }

            let result = (|| {
                let command_set = cfi_query_u16(
                    base,
                    size,
                    bank_width,
                    device_width,
                    endian,
                    query_stride,
                    CFI_PRIMARY_COMMAND_SET,
                )?;

                if !matches!(
                    command_set,
                    CFI_COMMAND_SET_INTEL_EXTENDED | CFI_COMMAND_SET_INTEL_STANDARD
                ) {
                    return Err(FlashIoError::Unsupported);
                }
                let interface = cfi_query_u16(
                    base,
                    size,
                    bank_width,
                    device_width,
                    endian,
                    query_stride,
                    CFI_INTERFACE_DESCRIPTION,
                )?;
                if !interface_supports_geometry(
                    interface,
                    bank_width,
                    device_width,
                    device_width_hint.is_some(),
                ) {
                    return Err(FlashIoError::Unsupported);
                }

                let size_exponent = cfi_query_byte(
                    base,
                    size,
                    bank_width,
                    device_width,
                    endian,
                    query_stride,
                    CFI_DEVICE_SIZE,
                )? as u32;
                let per_device_size = 1usize
                    .checked_shl(size_exponent)
                    .ok_or(FlashIoError::OutOfRange)?;
                let interleave = bank_width / device_width;
                let described_size = per_device_size
                    .checked_mul(interleave)
                    .ok_or(FlashIoError::OutOfRange)?;
                if described_size != size {
                    // 当前一个 reg tuple 对应一个 CFI bank。大于一个 bank 的线性窗口
                    // 需要额外的 alias/chip 探测，在没有该保障前不允许擦写。
                    return Err(FlashIoError::Invalid);
                }

                let region_count = cfi_query_byte(
                    base,
                    size,
                    bank_width,
                    device_width,
                    endian,
                    query_stride,
                    CFI_ERASE_REGION_COUNT,
                )? as usize;
                if region_count == 0 {
                    return Err(FlashIoError::Unsupported);
                }
                let mut erase_regions = Vec::new();
                erase_regions
                    .try_reserve(region_count)
                    .map_err(|_| FlashIoError::OutOfMemory)?;
                let mut region_offset = 0usize;
                for region_index in 0..region_count {
                    let entry = CFI_ERASE_REGION_TABLE
                        .checked_add(
                            region_index
                                .checked_mul(CFI_ERASE_REGION_ENTRY_SIZE)
                                .ok_or(FlashIoError::OutOfRange)?,
                        )
                        .ok_or(FlashIoError::OutOfRange)?;
                    let block_count = usize::from(cfi_query_u16(
                        base,
                        size,
                        bank_width,
                        device_width,
                        endian,
                        query_stride,
                        entry,
                    )?) + 1;
                    let block_units = usize::from(cfi_query_u16(
                        base,
                        size,
                        bank_width,
                        device_width,
                        endian,
                        query_stride,
                        entry + 2,
                    )?);
                    let per_device_block_size = if block_units == 0 {
                        128
                    } else {
                        block_units
                            .checked_mul(256)
                            .ok_or(FlashIoError::OutOfRange)?
                    };
                    let block_size = per_device_block_size
                        .checked_mul(interleave)
                        .ok_or(FlashIoError::OutOfRange)?;
                    let region_size = block_size
                        .checked_mul(block_count)
                        .ok_or(FlashIoError::OutOfRange)?;
                    erase_regions.push(FlashEraseRegion {
                        offset: region_offset,
                        block_size,
                        block_count,
                    });
                    region_offset = region_offset
                        .checked_add(region_size)
                        .ok_or(FlashIoError::OutOfRange)?;
                }
                if region_offset != size {
                    return Err(FlashIoError::Invalid);
                }

                Ok(MappedFlashWindow {
                    phys,
                    base,
                    size,
                    device_width,
                    device_type,
                    endian,
                    erase_regions,
                })
            })();
            reset_geometry(base, size, bank_width, device_width, endian);
            return result;
        }
    }
    Err(FlashIoError::Invalid)
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
        let device_width_hint = flash_device_width(info, bank_width)?;
        let endian = flash_endian(info)?;
        let read_only = boolean_property(info, PROP_READ_ONLY)?;
        let mut windows = Vec::new();
        let mut erase_regions = Vec::new();
        let mut global_offset = 0usize;
        for (phys, size) in info.mmio_resources() {
            let base = (self.device_mmio_to_virt)(phys);
            if size == 0 || !size.is_multiple_of(bank_width) {
                return Err(PnpError::malformed(
                    crate::dev::pnp::PnpResourceKind::Mmio,
                    "cfi flash mmio window size is not bank aligned",
                ));
            }
            if !phys.is_multiple_of(bank_width) || !base.is_multiple_of(bank_width) {
                return Err(PnpError::malformed(
                    crate::dev::pnp::PnpResourceKind::Mmio,
                    "cfi flash mmio window base is not bank aligned",
                ));
            }
            windows.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
            let window = probe_cfi_window(phys, base, size, bank_width, device_width_hint, endian)
                .map_err(map_cfi_probe_error)?;
            erase_regions
                .try_reserve(window.erase_regions.len())
                .map_err(|_| PnpError::OutOfMemory)?;
            for region in &window.erase_regions {
                erase_regions.push(FlashEraseRegion {
                    offset: global_offset
                        .checked_add(region.offset)
                        .ok_or(PnpError::OutOfMemory)?,
                    block_size: region.block_size,
                    block_count: region.block_count,
                });
            }
            global_offset = global_offset
                .checked_add(size)
                .ok_or(PnpError::OutOfMemory)?;
            windows.push(window);
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
            erase_regions,
            writable: !read_only,
            lock: Spinlock::new(()),
        });
        dev.reserve_owned_resources(1)?;
        let handle = flash::register_v2(flash.clone(), flash.clone()).map_err(map_flash_error)?;
        if let Err(err) = dev.own_resource(flash::pnp_resource_v2(handle, "platform-cfi-flash")) {
            let _ = flash::unregister_v2(handle);
            return Err(err);
        }
        let detected_device_width = flash.windows[0].device_width;
        let detected_device_type = flash.windows[0].device_type;
        log::printk!(
            "[cfi-flash] registered {} windows={} bank-width={} first-device-width={} first-device-type={} first-interleave={} total={:#x} erase-regions={} writable={}",
            flash.name(),
            flash.window_count(),
            flash.bank_width(),
            detected_device_width,
            detected_device_type,
            flash.bank_width() / detected_device_width,
            flash.total_size().unwrap_or(0),
            flash.erase_region_count(),
            flash.capabilities().writable as usize
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
    let width = required_u32_property(info, PROP_BANK_WIDTH)? as usize;
    match width {
        1 | 2 | 4 => Ok(width),
        _ => Err(PnpError::malformed(
            crate::dev::pnp::PnpResourceKind::Flash,
            "invalid cfi flash bank-width",
        )),
    }
}

fn flash_device_width(
    info: &PlatformDeviceInfo,
    bank_width: usize,
) -> Result<Option<usize>, PnpError> {
    let Some(raw) = info.bytes_property(PROP_DEVICE_WIDTH) else {
        return Ok(None);
    };
    if raw.len() != size_of::<u32>() {
        return Err(malformed_flash_property(
            "cfi flash device-width must contain one u32 cell",
        ));
    }
    let width = info
        .u32_property(PROP_DEVICE_WIDTH)
        .ok_or_else(|| malformed_flash_property("invalid cfi flash device-width"))?
        as usize;
    if !matches!(width, 1 | 2) || width > bank_width || !bank_width.is_multiple_of(width) {
        return Err(malformed_flash_property(
            "cfi flash device-width does not divide bank-width",
        ));
    }
    Ok(Some(width))
}

fn flash_endian(info: &PlatformDeviceInfo) -> Result<CfiEndian, PnpError> {
    let big = boolean_property(info, PROP_BIG_ENDIAN)?;
    let little = boolean_property(info, PROP_LITTLE_ENDIAN)?;
    let native = boolean_property(info, PROP_NATIVE_ENDIAN)?;
    if usize::from(big) + usize::from(little) + usize::from(native) > 1 {
        return Err(malformed_flash_property(
            "conflicting cfi flash endian properties",
        ));
    }
    Ok(if big {
        CfiEndian::Big
    } else if little {
        CfiEndian::Little
    } else {
        CfiEndian::Native
    })
}

fn required_u32_property(info: &PlatformDeviceInfo, name: &str) -> Result<u32, PnpError> {
    let raw = info
        .bytes_property(name)
        .ok_or_else(|| malformed_flash_property("missing required cfi flash property"))?;
    if raw.len() != size_of::<u32>() {
        return Err(malformed_flash_property(
            "cfi flash integer property must contain one u32 cell",
        ));
    }
    info.u32_property(name)
        .ok_or_else(|| malformed_flash_property("invalid cfi flash integer property"))
}

fn boolean_property(info: &PlatformDeviceInfo, name: &str) -> Result<bool, PnpError> {
    let Some(raw) = info.bytes_property(name) else {
        return Ok(false);
    };
    if !raw.is_empty() {
        return Err(malformed_flash_property(
            "cfi flash boolean property must be empty",
        ));
    }
    Ok(true)
}

fn malformed_flash_property(message: &'static str) -> PnpError {
    PnpError::malformed(crate::dev::pnp::PnpResourceKind::Flash, message)
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

fn map_cfi_probe_error(err: FlashIoError) -> PnpError {
    match err {
        FlashIoError::OutOfMemory => PnpError::OutOfMemory,
        FlashIoError::Busy | FlashIoError::Io => {
            PnpError::hardware_failure("CFI query transport failed")
        }
        FlashIoError::Unsupported => PnpError::malformed(
            crate::dev::pnp::PnpResourceKind::Flash,
            "unsupported CFI command set or geometry",
        ),
        FlashIoError::Invalid | FlashIoError::OutOfRange | FlashIoError::NeedsErase => {
            PnpError::malformed(
                crate::dev::pnp::PnpResourceKind::Flash,
                "invalid CFI query data",
            )
        }
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

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(CfiFlashFactory))
}

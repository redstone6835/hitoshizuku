//! Platform-neutral firmware table parsers.
//!
//! This module deliberately contains only parsers for already selected firmware
//! data. Architecture-specific discovery, copying, storage, and ACPI-vs-DTB
//! policy stay in the architecture init code.

use alloc::vec::Vec;
use allocator::MemorySegment;

pub mod acpi;
pub mod dtb;
pub mod power;

#[derive(Clone, Copy, Debug)]
pub struct SerialPortInfo {
    pub name: &'static str,
    pub phys_addr: usize,
    /// 固件声明的寄存器窗口大小。没有该信息时保持 `None`，由最终 platform
    /// 资源保留未知大小，而不是在固件摘要层替调用方猜一个固定窗口。
    pub reg_size: Option<usize>,
    pub clock_hz: Option<u32>,
    /// 固件当前配置的串口波特率。驱动仍可在未声明时使用自身默认策略。
    pub baud: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
pub struct FirmwareTableMapping {
    pub physical_start: usize,
    pub virtual_start: usize,
    pub length: usize,
}

impl FirmwareTableMapping {
    pub const EMPTY: Self = Self {
        physical_start: 0,
        virtual_start: 0,
        length: 0,
    };

    #[inline]
    pub fn resolve(self, physical_address: usize, size: usize) -> Option<usize> {
        let requested_end = physical_address.checked_add(size)?;
        let mapping_end = self.physical_start.checked_add(self.length)?;
        if physical_address >= self.physical_start && requested_end <= mapping_end {
            Some(
                self.virtual_start
                    .checked_add(physical_address - self.physical_start)?,
            )
        } else {
            None
        }
    }
}

#[inline]
pub(crate) fn checksum_valid(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

#[inline]
pub(crate) fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

#[inline]
pub(crate) fn read_u64_le(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

pub(crate) fn normalize_segments(mut segments: Vec<MemorySegment>) -> Option<Vec<MemorySegment>> {
    if segments.is_empty() {
        return None;
    }

    segments.sort_unstable_by_key(|segment| segment.start);
    let mut merged: Vec<MemorySegment> = Vec::with_capacity(segments.len());
    for segment in segments {
        if segment.size == 0 {
            continue;
        }
        if let Some(last) = merged.last_mut() {
            let last_end = last.start.saturating_add(last.size);
            if last_end >= segment.start {
                let merged_end = last_end.max(segment.start.saturating_add(segment.size));
                last.size = merged_end.saturating_sub(last.start);
                continue;
            }
        }
        merged.push(segment);
    }

    (!merged.is_empty()).then_some(merged)
}

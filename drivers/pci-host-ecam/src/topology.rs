//! PCI 桥拓扑配置使用的纯资源模型与寄存器编码。

extern crate alloc;

use alloc::vec::Vec;

const PCI_IO_WINDOW_GRANULARITY: u64 = 1 << 12;
const PCI_MEMORY_WINDOW_GRANULARITY: u64 = 1 << 20;
const PCI_16BIT_IO_END: u64 = 1 << 16;
const PCI_32BIT_ADDRESS_END: u64 = 1 << 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PciResourceSpace {
    Io,
    Memory,
    PrefetchableMemory,
}

/// PCI 地址空间内的半开资源区间。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PciResourceRange {
    pub start: u64,
    pub end: u64,
}

impl PciResourceRange {
    pub(crate) fn new(start: u64, size: u64) -> Option<Self> {
        let end = start.checked_add(size)?;
        (size != 0).then_some(Self { start, end })
    }

    pub(crate) const fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    fn cover(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// 一条 bus 以及其全部后代实际启用资源的包络。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PciResourceEnvelope {
    io: Option<PciResourceRange>,
    memory: Option<PciResourceRange>,
    prefetchable: Option<PciResourceRange>,
}

impl PciResourceEnvelope {
    pub(crate) fn include(&mut self, space: PciResourceSpace, address: u64, size: u64) -> bool {
        let Some(range) = PciResourceRange::new(address, size) else {
            return false;
        };
        self.include_range(space, range);
        true
    }

    pub(crate) fn include_range(&mut self, space: PciResourceSpace, range: PciResourceRange) {
        let slot = self.slot_mut(space);
        *slot = Some(slot.map_or(range, |existing| existing.cover(range)));
    }

    pub(crate) const fn range(self, space: PciResourceSpace) -> Option<PciResourceRange> {
        match space {
            PciResourceSpace::Io => self.io,
            PciResourceSpace::Memory => self.memory,
            PciResourceSpace::PrefetchableMemory => self.prefetchable,
        }
    }

    pub(crate) fn clear(&mut self, space: PciResourceSpace) {
        *self.slot_mut(space) = None;
    }

    fn slot_mut(&mut self, space: PciResourceSpace) -> &mut Option<PciResourceRange> {
        match space {
            PciResourceSpace::Io => &mut self.io,
            PciResourceSpace::Memory => &mut self.memory,
            PciResourceSpace::PrefetchableMemory => &mut self.prefetchable,
        }
    }
}

/// 深度优先配置桥时使用的单调 bus number 分配器。
pub(crate) struct PciBusNumberAllocator {
    next: u16,
    end: u8,
}

impl PciBusNumberAllocator {
    pub(crate) const fn new(root: u8, end: u8) -> Self {
        Self {
            next: root as u16 + 1,
            end,
        }
    }

    pub(crate) fn allocate(&mut self) -> Option<u8> {
        if self.next > self.end as u16 {
            return None;
        }
        let bus = self.next as u8;
        self.next += 1;
        Some(bus)
    }

    pub(crate) const fn last_allocated(&self, root: u8) -> u8 {
        if self.next == root as u16 + 1 {
            root
        } else {
            (self.next - 1) as u8
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PciBridgeWindowError {
    AddressOverflow,
    IoWidth,
    MemoryWidth,
    PrefetchableWidth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PciBridgeIoWindow {
    pub base_low: u8,
    pub limit_low: u8,
    pub base_upper: u16,
    pub limit_upper: u16,
    pub forwarded: PciResourceRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PciBridgeMemoryWindow {
    pub base: u16,
    pub limit: u16,
    pub forwarded: PciResourceRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PciBridgePrefetchWindow {
    pub base: u16,
    pub limit: u16,
    pub base_upper: u32,
    pub limit_upper: u32,
    pub forwarded: PciResourceRange,
}

pub(crate) fn encode_bridge_io_window(
    range: PciResourceRange,
    supports_32bit: bool,
) -> Result<PciBridgeIoWindow, PciBridgeWindowError> {
    let forwarded = rounded_range(range, PCI_IO_WINDOW_GRANULARITY)?;
    if forwarded.end > PCI_32BIT_ADDRESS_END
        || (!supports_32bit && forwarded.end > PCI_16BIT_IO_END)
    {
        return Err(PciBridgeWindowError::IoWidth);
    }
    let limit = forwarded.end - 1;
    let address_type = u8::from(supports_32bit);
    Ok(PciBridgeIoWindow {
        base_low: ((forwarded.start >> 8) as u8 & 0xf0) | address_type,
        limit_low: ((limit >> 8) as u8 & 0xf0) | address_type,
        base_upper: (forwarded.start >> 16) as u16,
        limit_upper: (limit >> 16) as u16,
        forwarded,
    })
}

pub(crate) fn encode_bridge_memory_window(
    range: PciResourceRange,
) -> Result<PciBridgeMemoryWindow, PciBridgeWindowError> {
    let forwarded = rounded_range(range, PCI_MEMORY_WINDOW_GRANULARITY)?;
    if forwarded.end > PCI_32BIT_ADDRESS_END {
        return Err(PciBridgeWindowError::MemoryWidth);
    }
    let limit = forwarded.end - 1;
    Ok(PciBridgeMemoryWindow {
        base: (forwarded.start >> 16) as u16 & 0xfff0,
        limit: (limit >> 16) as u16 & 0xfff0,
        forwarded,
    })
}

pub(crate) fn encode_bridge_prefetch_window(
    range: PciResourceRange,
    supports_64bit: bool,
) -> Result<PciBridgePrefetchWindow, PciBridgeWindowError> {
    let forwarded = rounded_range(range, PCI_MEMORY_WINDOW_GRANULARITY)?;
    if !supports_64bit && forwarded.end > PCI_32BIT_ADDRESS_END {
        return Err(PciBridgeWindowError::PrefetchableWidth);
    }
    let limit = forwarded.end - 1;
    let address_type = u16::from(supports_64bit);
    Ok(PciBridgePrefetchWindow {
        base: ((forwarded.start >> 16) as u16 & 0xfff0) | address_type,
        limit: ((limit >> 16) as u16 & 0xfff0) | address_type,
        base_upper: (forwarded.start >> 32) as u32,
        limit_upper: (limit >> 32) as u32,
        forwarded,
    })
}

fn rounded_range(
    range: PciResourceRange,
    granularity: u64,
) -> Result<PciResourceRange, PciBridgeWindowError> {
    if range.start >= range.end {
        return Err(PciBridgeWindowError::AddressOverflow);
    }
    let start = range.start & !(granularity - 1);
    let end = align_up(range.end, granularity).ok_or(PciBridgeWindowError::AddressOverflow)?;
    Ok(PciResourceRange { start, end })
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

/// 同一上游 bus 上的兄弟桥不能发布互相重叠的 forwarding aperture。
pub(crate) struct PciBridgeApertureTracker {
    io: Vec<PciResourceRange>,
    memory: Vec<PciResourceRange>,
}

impl PciBridgeApertureTracker {
    pub(crate) const fn new() -> Self {
        Self {
            io: Vec::new(),
            memory: Vec::new(),
        }
    }

    pub(crate) fn reserve(&mut self, space: PciResourceSpace, range: PciResourceRange) -> bool {
        let ranges = match space {
            PciResourceSpace::Io => &mut self.io,
            PciResourceSpace::Memory | PciResourceSpace::PrefetchableMemory => &mut self.memory,
        };
        if ranges.iter().any(|existing| existing.overlaps(range)) {
            return false;
        }
        ranges.push(range);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_allocator_stays_inside_firmware_bus_range() {
        let mut buses = PciBusNumberAllocator::new(0x20, 0x22);
        assert_eq!(buses.allocate(), Some(0x21));
        assert_eq!(buses.allocate(), Some(0x22));
        assert_eq!(buses.last_allocated(0x20), 0x22);
        assert_eq!(buses.allocate(), None);
    }

    #[test]
    fn bridge_windows_round_outward_and_encode_inclusive_limits() {
        let io =
            encode_bridge_io_window(PciResourceRange::new(0x4120, 0x20).unwrap(), true).unwrap();
        assert_eq!(
            io.forwarded,
            PciResourceRange {
                start: 0x4000,
                end: 0x5000
            }
        );
        assert_eq!((io.base_low, io.limit_low), (0x41, 0x41));

        let memory =
            encode_bridge_memory_window(PciResourceRange::new(0x4012_0000, 0x2000).unwrap())
                .unwrap();
        assert_eq!(
            memory.forwarded,
            PciResourceRange {
                start: 0x4010_0000,
                end: 0x4020_0000,
            }
        );
        assert_eq!((memory.base, memory.limit), (0x4010, 0x4010));
    }

    #[test]
    fn prefetch_window_uses_upper_registers_for_64bit_resources() {
        let window = encode_bridge_prefetch_window(
            PciResourceRange::new(0x1_2000_0000, 0x20_0000).unwrap(),
            true,
        )
        .unwrap();
        assert_eq!(window.base & 0xf, 1);
        assert_eq!(window.limit & 0xf, 1);
        assert_eq!(window.base_upper, 1);
        assert_eq!(window.limit_upper, 1);
        assert_eq!(
            encode_bridge_prefetch_window(
                PciResourceRange::new(0x1_2000_0000, 0x20_0000).unwrap(),
                false,
            ),
            Err(PciBridgeWindowError::PrefetchableWidth)
        );
    }

    #[test]
    fn sibling_bridge_apertures_fail_closed_on_granularity_overlap() {
        let left = encode_bridge_memory_window(PciResourceRange::new(0x4000_0000, 0x1000).unwrap())
            .unwrap();
        let right =
            encode_bridge_memory_window(PciResourceRange::new(0x4008_0000, 0x1000).unwrap())
                .unwrap();
        let mut tracker = PciBridgeApertureTracker::new();
        assert!(tracker.reserve(PciResourceSpace::Memory, left.forwarded));
        assert!(!tracker.reserve(PciResourceSpace::Memory, right.forwarded));
    }

    #[test]
    fn resource_envelope_covers_nested_descendants() {
        let mut child = PciResourceEnvelope::default();
        assert!(child.include(PciResourceSpace::Memory, 0x5000_0000, 0x1000));
        assert!(child.include(PciResourceSpace::Memory, 0x5010_0000, 0x2000));
        assert_eq!(
            child.range(PciResourceSpace::Memory),
            Some(PciResourceRange {
                start: 0x5000_0000,
                end: 0x5010_2000,
            })
        );
    }
}

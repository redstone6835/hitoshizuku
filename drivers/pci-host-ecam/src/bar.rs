//! PCI host ELM 的 `ranges` 运行时窗口与 fallback BAR 分配器。
//!
//! 固件同时给出 PCI 子地址和 CPU 父地址；二者不能被压成一个“物理地址”。本模块
//! 保留完整映射，BAR 写回使用 PCI 地址，驱动映射则使用对应 CPU 地址。分配器在
//! 每段窗口维护已占用区间，先保留固件给出的可用 BAR，再从剩余空洞分配，避免
//! 启动期无条件改写已经工作的资源布局。

extern crate alloc;

use alloc::vec::Vec;

const PCI_32BIT_ADDRESS_END: u64 = 1u64 << 32;
const PCI_20BIT_ADDRESS_END: u64 = 1u64 << 20;
const PCI_20BIT_MEMORY_BAR_ADDRESS_MASK: u32 = 0x000f_fff0;
const PCI_BRIDGE_IO_GRANULARITY: u64 = 1 << 12;
const PCI_BRIDGE_MEMORY_GRANULARITY: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PciBarWindowSpace {
    Io,
    Memory,
    PrefetchableMemory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PciBarKind {
    Io,
    Memory { prefetchable: bool },
}

/// memory BAR 能表达的地址宽度。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PciBarAddressWidth {
    /// 旧式 PCI 1 MiB 以下 memory BAR。
    Bits20,
    Bits32,
    Bits64,
}

impl PciBarAddressWidth {
    const fn address_end(self) -> u64 {
        match self {
            Self::Bits20 => PCI_20BIT_ADDRESS_END,
            Self::Bits32 => PCI_32BIT_ADDRESS_END,
            Self::Bits64 => u64::MAX,
        }
    }
}

/// 从旧式 20-bit memory BAR 的探测值计算大小。
///
/// 位 31:20 不属于 BAR 地址字段，做二补数前必须按一处理；否则恰好占满低
/// 1 MiB 地址空间的 BAR 会被错误算成零大小。
pub(crate) fn probed_20bit_memory_bar_size(value: u32) -> u64 {
    let address_bits =
        (value & PCI_20BIT_MEMORY_BAR_ADDRESS_MASK) | !(PCI_20BIT_ADDRESS_END as u32 - 1);
    u64::from((!address_bits).wrapping_add(1))
}

/// 一段规范化的 PCI 子地址到 CPU 物理地址映射。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PciBarRuntimeWindow {
    pub space: PciBarWindowSpace,
    pub pci_start: u64,
    pub pci_end: u64,
    pub cpu_start: usize,
}

impl PciBarRuntimeWindow {
    pub(crate) fn new(
        space: PciBarWindowSpace,
        pci_start: u64,
        cpu_start: usize,
        size: usize,
    ) -> Option<Self> {
        let size = u64::try_from(size).ok()?;
        if size == 0 {
            return None;
        }
        let pci_end = pci_start.checked_add(size)?;
        cpu_start.checked_add(usize::try_from(size).ok()?)?;
        Some(Self {
            space,
            pci_start,
            pci_end,
            cpu_start,
        })
    }

    pub(crate) fn size(self) -> u64 {
        self.pci_end - self.pci_start
    }

    pub(crate) fn accepts(self, kind: PciBarKind) -> bool {
        match (self.space, kind) {
            (PciBarWindowSpace::Io, PciBarKind::Io) => true,
            (PciBarWindowSpace::Memory, PciBarKind::Memory { .. }) => true,
            (PciBarWindowSpace::PrefetchableMemory, PciBarKind::Memory { prefetchable: true }) => {
                true
            }
            _ => false,
        }
    }

    pub(crate) fn cpu_address(self, pci_address: u64, size: u64) -> Option<usize> {
        let end = pci_address.checked_add(size)?;
        if pci_address < self.pci_start || end > self.pci_end {
            return None;
        }
        let offset = usize::try_from(pci_address - self.pci_start).ok()?;
        self.cpu_start.checked_add(offset)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PciBarAllocation {
    pub pci_address: u64,
    pub cpu_address: usize,
    pub space: PciBarWindowSpace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PciBarReservedRange {
    start: u64,
    end: u64,
}

struct PciBarAllocationWindow {
    window: PciBarRuntimeWindow,
    reserved: Vec<PciBarReservedRange>,
    next: u64,
}

pub(crate) struct PciBarWindowAllocator {
    windows: Vec<PciBarAllocationWindow>,
}

impl PciBarWindowAllocator {
    pub(crate) fn new(windows: &[PciBarRuntimeWindow]) -> Self {
        let mut windows = windows
            .iter()
            .copied()
            .map(|window| PciBarAllocationWindow {
                window,
                reserved: Vec::new(),
                next: window.pci_start,
            })
            .collect::<Vec<_>>();
        windows.sort_by_key(|state| (state.window.space, state.window.pci_start));
        Self { windows }
    }

    pub(crate) fn allocate(
        &mut self,
        kind: PciBarKind,
        width: PciBarAddressWidth,
        size: u64,
        alignment: u64,
    ) -> Option<PciBarAllocation> {
        if !valid_request(size, alignment) {
            return None;
        }

        // Prefetchable BAR 优先进入同类型窗口；没有专用窗口时可以安全降级到普通
        // memory window。非 prefetchable BAR 不能反向占用专用 prefetchable 窗口。
        let passes = match kind {
            PciBarKind::Memory { prefetchable: true } => 2,
            _ => 1,
        };
        for pass in 0..passes {
            for state in &mut self.windows {
                if !window_matches_pass(state.window, kind, pass) {
                    continue;
                }
                let address_end = usable_address_end(state.window, kind, width);
                let Some(range) = first_free_range(state, size, alignment, address_end) else {
                    continue;
                };
                let cpu_address = state.window.cpu_address(range.start, size)?;
                insert_reserved_range(&mut state.reserved, range);
                state.next = range.end;
                return Some(PciBarAllocation {
                    pci_address: range.start,
                    cpu_address,
                    space: state.window.space,
                });
            }
        }
        None
    }

    /// 在开始配置一个桥后代前，把新分配游标推进到空闲 aperture 边界。
    pub(crate) fn begin_bridge_group(&mut self) {
        for state in &mut self.windows {
            let granularity = bridge_granularity(state.window.space);
            let Some(mut candidate) = align_up(state.next, granularity) else {
                state.next = state.window.pci_end;
                continue;
            };
            loop {
                let Some(end) = candidate.checked_add(granularity) else {
                    candidate = state.window.pci_end;
                    break;
                };
                let Some(conflict) = state.reserved.iter().find(|reserved| {
                    ranges_overlap(
                        **reserved,
                        PciBarReservedRange {
                            start: candidate,
                            end,
                        },
                    )
                }) else {
                    break;
                };
                let Some(next) = align_up(conflict.end, granularity) else {
                    candidate = state.window.pci_end;
                    break;
                };
                candidate = next;
            }
            state.next = candidate.min(state.window.pci_end);
        }
    }

    /// 结束一个桥后代后跳过当前 aperture 的尾部，隔离下一个兄弟桥或本地设备。
    pub(crate) fn end_bridge_group(&mut self) {
        for state in &mut self.windows {
            let granularity = bridge_granularity(state.window.space);
            state.next = align_up(state.next, granularity)
                .unwrap_or(state.window.pci_end)
                .min(state.window.pci_end);
        }
    }

    /// 尝试保留固件已经写入 BAR 的地址。
    ///
    /// 只有地址完整落在匹配的 host window、满足 BAR 对齐/位宽且没有与先前资源
    /// 重叠时才成功。调用方应先登记全部固件资源，再分配缺失资源。
    pub(crate) fn reserve(
        &mut self,
        kind: PciBarKind,
        width: PciBarAddressWidth,
        address: u64,
        size: u64,
        alignment: u64,
    ) -> Option<PciBarAllocation> {
        if !valid_request(size, alignment) || address & (alignment - 1) != 0 {
            return None;
        }
        let end = address.checked_add(size)?;
        let passes = match kind {
            PciBarKind::Memory { prefetchable: true } => 2,
            _ => 1,
        };
        for pass in 0..passes {
            for state in &mut self.windows {
                if !window_matches_pass(state.window, kind, pass)
                    || address < state.window.pci_start
                    || end > usable_address_end(state.window, kind, width)
                {
                    continue;
                }
                let range = PciBarReservedRange {
                    start: address,
                    end,
                };
                if state
                    .reserved
                    .iter()
                    .any(|existing| ranges_overlap(*existing, range))
                {
                    return None;
                }
                let cpu_address = state.window.cpu_address(address, size)?;
                insert_reserved_range(&mut state.reserved, range);
                return Some(PciBarAllocation {
                    pci_address: address,
                    cpu_address,
                    space: state.window.space,
                });
            }
        }
        None
    }
}

fn valid_request(size: u64, alignment: u64) -> bool {
    size != 0 && alignment != 0 && alignment.is_power_of_two()
}

const fn bridge_granularity(space: PciBarWindowSpace) -> u64 {
    match space {
        PciBarWindowSpace::Io => PCI_BRIDGE_IO_GRANULARITY,
        PciBarWindowSpace::Memory | PciBarWindowSpace::PrefetchableMemory => {
            PCI_BRIDGE_MEMORY_GRANULARITY
        }
    }
}

fn usable_address_end(
    window: PciBarRuntimeWindow,
    kind: PciBarKind,
    width: PciBarAddressWidth,
) -> u64 {
    let bar_end = match kind {
        PciBarKind::Io => PCI_32BIT_ADDRESS_END,
        PciBarKind::Memory { .. } => width.address_end(),
    };
    window.pci_end.min(bar_end)
}

fn first_free_range(
    state: &PciBarAllocationWindow,
    size: u64,
    alignment: u64,
    address_end: u64,
) -> Option<PciBarReservedRange> {
    let mut candidate = align_up(state.next.max(state.window.pci_start), alignment)?;
    for reserved in &state.reserved {
        let end = candidate.checked_add(size)?;
        if end <= reserved.start {
            break;
        }
        if candidate < reserved.end {
            candidate = align_up(reserved.end, alignment)?;
        }
    }
    let end = candidate.checked_add(size)?;
    (candidate >= state.window.pci_start && end <= address_end).then_some(PciBarReservedRange {
        start: candidate,
        end,
    })
}

fn insert_reserved_range(ranges: &mut Vec<PciBarReservedRange>, range: PciBarReservedRange) {
    let index = ranges
        .iter()
        .position(|existing| existing.start > range.start)
        .unwrap_or(ranges.len());
    ranges.insert(index, range);
}

fn ranges_overlap(left: PciBarReservedRange, right: PciBarReservedRange) -> bool {
    left.start < right.end && right.start < left.end
}

fn window_matches_pass(window: PciBarRuntimeWindow, kind: PciBarKind, pass: usize) -> bool {
    match kind {
        PciBarKind::Io => window.space == PciBarWindowSpace::Io,
        PciBarKind::Memory {
            prefetchable: false,
        } => window.space == PciBarWindowSpace::Memory,
        PciBarKind::Memory { prefetchable: true } => match pass {
            0 => window.space == PciBarWindowSpace::PrefetchableMemory,
            _ => window.space == PciBarWindowSpace::Memory,
        },
    }
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_20bit_bar_size_preserves_the_one_mib_boundary() {
        assert_eq!(probed_20bit_memory_bar_size(0x000f_f000), 0x1000);
        assert_eq!(probed_20bit_memory_bar_size(0x0000_0000), 0x10_0000);
        assert_eq!(probed_20bit_memory_bar_size(0x000f_fff0), 0x10);
    }

    #[test]
    fn io_window_preserves_pci_and_cpu_addresses() {
        let window =
            PciBarRuntimeWindow::new(PciBarWindowSpace::Io, 0x4000, 0x1800_4000, 0xc000).unwrap();
        assert_eq!(window.cpu_address(0x4100, 0x20), Some(0x1800_4100));
        assert_eq!(window.cpu_address(0xffff, 2), None);
    }

    #[test]
    fn allocator_assigns_io_and_memory_without_crossing_spaces() {
        let windows = [
            PciBarRuntimeWindow::new(PciBarWindowSpace::Io, 0x4000, 0x1800_4000, 0xc000).unwrap(),
            PciBarRuntimeWindow::new(PciBarWindowSpace::Memory, 0x4000_0000, 0x9000_0000, 0x10000)
                .unwrap(),
        ];
        let mut allocator = PciBarWindowAllocator::new(&windows);

        let io = allocator
            .allocate(PciBarKind::Io, PciBarAddressWidth::Bits32, 0x20, 0x20)
            .unwrap();
        assert_eq!(io.pci_address, 0x4000);
        assert_eq!(io.cpu_address, 0x1800_4000);

        let memory = allocator
            .allocate(
                PciBarKind::Memory {
                    prefetchable: false,
                },
                PciBarAddressWidth::Bits32,
                0x1000,
                0x1000,
            )
            .unwrap();
        assert_eq!(memory.pci_address, 0x4000_0000);
        assert_eq!(memory.cpu_address, 0x9000_0000);
    }

    #[test]
    fn shared_window_cursor_prevents_32_and_64_bit_overlap() {
        let windows = [PciBarRuntimeWindow::new(
            PciBarWindowSpace::Memory,
            0x4000_0000,
            0x4000_0000,
            0x10000,
        )
        .unwrap()];
        let mut allocator = PciBarWindowAllocator::new(&windows);
        let low = allocator
            .allocate(
                PciBarKind::Memory {
                    prefetchable: false,
                },
                PciBarAddressWidth::Bits32,
                0x1000,
                0x1000,
            )
            .unwrap();
        let wide = allocator
            .allocate(
                PciBarKind::Memory {
                    prefetchable: false,
                },
                PciBarAddressWidth::Bits64,
                0x4000,
                0x4000,
            )
            .unwrap();
        assert_eq!(low.pci_address, 0x4000_0000);
        assert_eq!(wide.pci_address, 0x4000_4000);
    }

    #[test]
    fn non_prefetchable_bar_does_not_use_prefetch_only_window() {
        let windows = [PciBarRuntimeWindow::new(
            PciBarWindowSpace::PrefetchableMemory,
            0x1_0000_0000,
            0x8000_0000,
            0x10000,
        )
        .unwrap()];
        let mut allocator = PciBarWindowAllocator::new(&windows);
        assert!(
            allocator
                .allocate(
                    PciBarKind::Memory {
                        prefetchable: false,
                    },
                    PciBarAddressWidth::Bits64,
                    0x1000,
                    0x1000,
                )
                .is_none()
        );
        assert!(
            allocator
                .allocate(
                    PciBarKind::Memory { prefetchable: true },
                    PciBarAddressWidth::Bits64,
                    0x1000,
                    0x1000,
                )
                .is_some()
        );
    }

    #[test]
    fn allocator_preserves_firmware_bar_and_uses_remaining_holes() {
        let windows = [PciBarRuntimeWindow::new(
            PciBarWindowSpace::Memory,
            0x4000_0000,
            0x8000_0000,
            0x10_0000,
        )
        .unwrap()];
        let mut allocator = PciBarWindowAllocator::new(&windows);
        let firmware = allocator
            .reserve(
                PciBarKind::Memory {
                    prefetchable: false,
                },
                PciBarAddressWidth::Bits32,
                0x4000_4000,
                0x4000,
                0x4000,
            )
            .unwrap();
        assert_eq!(firmware.cpu_address, 0x8000_4000);

        let before = allocator
            .allocate(
                PciBarKind::Memory {
                    prefetchable: false,
                },
                PciBarAddressWidth::Bits32,
                0x2000,
                0x2000,
            )
            .unwrap();
        let after = allocator
            .allocate(
                PciBarKind::Memory {
                    prefetchable: false,
                },
                PciBarAddressWidth::Bits32,
                0x4000,
                0x4000,
            )
            .unwrap();
        assert_eq!(before.pci_address, 0x4000_0000);
        assert_eq!(after.pci_address, 0x4000_8000);
    }

    #[test]
    fn reserve_rejects_overlap_misalignment_and_wrong_address_width() {
        let windows =
            [PciBarRuntimeWindow::new(PciBarWindowSpace::Memory, 0, 0, 0x2_0000_0000).unwrap()];
        let mut allocator = PciBarWindowAllocator::new(&windows);
        assert!(
            allocator
                .reserve(
                    PciBarKind::Memory {
                        prefetchable: false,
                    },
                    PciBarAddressWidth::Bits32,
                    0x2000,
                    0x1000,
                    0x1000,
                )
                .is_some()
        );
        assert!(
            allocator
                .reserve(
                    PciBarKind::Memory {
                        prefetchable: false,
                    },
                    PciBarAddressWidth::Bits32,
                    0x2800,
                    0x1000,
                    0x1000,
                )
                .is_none()
        );
        assert!(
            allocator
                .reserve(
                    PciBarKind::Memory {
                        prefetchable: false,
                    },
                    PciBarAddressWidth::Bits32,
                    0x1_0000_0000,
                    0x1000,
                    0x1000,
                )
                .is_none()
        );
        assert!(
            allocator
                .reserve(
                    PciBarKind::Memory {
                        prefetchable: false,
                    },
                    PciBarAddressWidth::Bits20,
                    0x10_0000,
                    0x1000,
                    0x1000,
                )
                .is_none()
        );
    }

    #[test]
    fn bridge_groups_skip_local_resources_and_end_on_aperture_boundary() {
        let windows = [PciBarRuntimeWindow::new(
            PciBarWindowSpace::Memory,
            0x4000_0000,
            0x4000_0000,
            0x40_0000,
        )
        .unwrap()];
        let mut allocator = PciBarWindowAllocator::new(&windows);
        allocator
            .reserve(
                PciBarKind::Memory {
                    prefetchable: false,
                },
                PciBarAddressWidth::Bits32,
                0x4000_0000,
                0x1000,
                0x1000,
            )
            .unwrap();

        allocator.begin_bridge_group();
        let child = allocator
            .allocate(
                PciBarKind::Memory {
                    prefetchable: false,
                },
                PciBarAddressWidth::Bits32,
                0x1000,
                0x1000,
            )
            .unwrap();
        assert_eq!(child.pci_address, 0x4010_0000);
        allocator.end_bridge_group();

        let sibling = allocator
            .allocate(
                PciBarKind::Memory {
                    prefetchable: false,
                },
                PciBarAddressWidth::Bits32,
                0x1000,
                0x1000,
            )
            .unwrap();
        assert_eq!(sibling.pci_address, 0x4020_0000);
    }
}

//! DT 内存描述到内核启动布局的策略层。
//!
//! [`fdt`] crate 保留无损的 `u128` 地址和动态保留请求；本模块只在真正进入
//! 当前平台的启动路径时把范围收窄为 [`MemorySegment`]，并统一处理 chosen
//! 限制、静态保留、动态分配以及内核镜像等额外避让范围。

use alloc::vec::Vec;

use allocator::{MemorySegment, NumaMemoryRange, PAGE_SIZE};
use fdt::{MemoryDescription, NodeId, PhysicalRange, ReservedMemory, ReservedMemoryPlacement};

use crate::StartMemoryRegion;

/// Linux 通用 reserved-memory 分配器在未指定 `alignment` 时使用 cache-line 对齐。
/// 当前支持的 LoongArch64 与 RISC-V 平台 cache line 均不大于 64 字节。
const DEFAULT_DYNAMIC_ALIGNMENT: usize = 64;

/// 已解析并完成动态放置的 `/reserved-memory` 节点。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbResolvedReservedMemory {
    /// 固件中的稳定请求描述，保留路径、phandle、用途和标志。
    pub request: ReservedMemory,
    /// 该节点最终占用的本机物理范围。
    pub ranges: Vec<MemorySegment>,
}

/// DT 约束全部应用后的启动内存布局。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbMemoryLayout {
    /// 可以交给物理页分配器的 RAM；DT 保留区已从中移除。
    pub usable_segments: Vec<MemorySegment>,
    /// FDT reservation block、静态节点和动态节点的合并保留范围。
    pub reserved_segments: Vec<MemorySegment>,
    /// 每个 `/reserved-memory` 子节点的稳定身份和最终范围。
    pub reserved_memory: Vec<DtbResolvedReservedMemory>,
    /// 必须从架构标准线性映射中排除的物理范围。
    pub no_map_segments: Vec<MemorySegment>,
}

/// 按 NUMA bank 边界切分、可直接交给 buddy 的物理内存布局。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbNumaMemoryLayout {
    pub usable_segments: Vec<MemorySegment>,
    pub numa_ranges: Vec<NumaMemoryRange>,
}

/// DT 内存描述无法转换为当前平台启动布局的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DtbMemoryLayoutError {
    /// 固件地址或长度不能由当前平台的 `usize` 无损表达。
    RangeNotRepresentable {
        /// 有节点来源时记录其稳定编号。
        node: Option<NodeId>,
        /// 对应的属性或范围来源。
        property: &'static str,
    },
    /// 本机范围的排他末端发生溢出。
    RangeOverflow {
        /// 有节点来源时记录其稳定编号。
        node: Option<NodeId>,
        /// 对应的属性或范围来源。
        property: &'static str,
    },
    /// 动态保留请求的长度为零。
    EmptyDynamicRequest { node: NodeId },
    /// 静态保留节点没有任何非空范围。
    EmptyStaticReservation { node: NodeId },
    /// 动态保留请求无法在 RAM 和 `alloc-ranges` 约束内满足。
    DynamicAllocationFailed { node: NodeId, size: usize },
    /// 架构声明的 `no-map` 粒度不是非零 2 次幂。
    InvalidNoMapGranule { granule: usize },
    /// `no-map` 向外对齐时地址溢出。
    NoMapAlignmentOverflow {
        range: MemorySegment,
        granule: usize,
    },
    /// 向外对齐后的空洞覆盖了内核镜像、initramfs 等仍需访问的对象。
    NoMapOverlapsProtected {
        range: MemorySegment,
        protected: MemorySegment,
    },
    /// 两个重叠 memory bank 对同一物理范围声明了不同 NUMA node。
    ConflictingNumaRange {
        range: MemorySegment,
        first: u32,
        second: u32,
    },
}

/// UEFI 内存图没有按 DTSpec 标注静态 reserved-memory 的错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbUefiReservationError {
    /// reserved-memory 子节点稳定编号。
    pub node: NodeId,
    /// 未被正确 EFI 类型完整覆盖的物理范围。
    pub range: MemorySegment,
    /// DTSpec 要求的 EFI memory type 数值。
    pub expected_efi_type: u32,
}

/// 把 DT `/memory` 节点展平为当前平台可表达的 RAM 段。
pub fn described_memory_segments(
    description: &MemoryDescription,
) -> Result<Vec<MemorySegment>, DtbMemoryLayoutError> {
    let mut segments = Vec::new();
    for bank in &description.memory_banks {
        for &range in &bank.ranges {
            if range.is_empty() {
                continue;
            }
            segments.push(native_range(range, Some(bank.node), "memory")?);
        }
    }
    normalize_checked(segments)
}

/// 把最终可用 RAM 按 DT NUMA bank 边界切分，并生成 allocator 标签。
///
/// buddy 会继续按 DMA zone 边界切分，但不会跨输入 segment 合并；因此这里必须先
/// 保留每个 NUMA 边界，确保一个 buddy segment 永远只属于零或一个节点。
pub fn split_numa_memory_segments(
    description: &MemoryDescription,
    usable_segments: Vec<MemorySegment>,
) -> Result<DtbNumaMemoryLayout, DtbMemoryLayoutError> {
    let mut tagged = Vec::new();
    for bank in &description.memory_banks {
        let Some(node_id) = bank.numa_node_id else {
            continue;
        };
        for &range in &bank.ranges {
            let native = native_range(range, Some(bank.node), "memory numa-node-id")?;
            if let Some(range) = page_normalized_segment(native)? {
                tagged.push(NumaMemoryRange { range, node_id });
            }
        }
    }
    for index in 0..tagged.len() {
        for other in &tagged[..index] {
            if segments_overlap(tagged[index].range, other.range)
                && tagged[index].node_id != other.node_id
            {
                let overlap_start = tagged[index].range.start.max(other.range.start);
                let overlap_end = tagged[index].range.end().min(other.range.end());
                return Err(DtbMemoryLayoutError::ConflictingNumaRange {
                    range: MemorySegment {
                        start: overlap_start,
                        size: overlap_end - overlap_start,
                    },
                    first: other.node_id,
                    second: tagged[index].node_id,
                });
            }
        }
    }

    let mut split = Vec::new();
    let mut numa_ranges = Vec::new();
    for segment in usable_segments {
        let Some(segment) = page_normalized_segment(segment)? else {
            continue;
        };
        let mut boundaries = Vec::from([segment.start, segment.end()]);
        for range in &tagged {
            let start = segment.start.max(range.range.start);
            let end = segment.end().min(range.range.end());
            if start < end {
                boundaries.push(start);
                boundaries.push(end);
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();

        for pair in boundaries.windows(2) {
            let range = MemorySegment {
                start: pair[0],
                size: pair[1] - pair[0],
            };
            if range.size == 0 {
                continue;
            }
            let node_id = tagged
                .iter()
                .find(|tagged| {
                    tagged.range.start <= range.start && tagged.range.end() >= range.end()
                })
                .map(|tagged| tagged.node_id);
            split.push(range);
            if let Some(node_id) = node_id {
                numa_ranges.push(NumaMemoryRange { range, node_id });
            }
        }
    }

    Ok(DtbNumaMemoryLayout {
        usable_segments: split,
        numa_ranges,
    })
}

/// 把 `/chosen/linux,usable-memory-range` 应用到已有 RAM 来源。
///
/// 该限制与 RAM 来源无关，因此直接 DT 启动和 UEFI GetMemoryMap 启动都应调用。
pub fn apply_chosen_usable_ranges(
    segments: Vec<MemorySegment>,
    description: &MemoryDescription,
) -> Result<Vec<MemorySegment>, DtbMemoryLayoutError> {
    let segments = normalize_checked(segments)?;
    if description.chosen_usable_ranges.is_empty() {
        return Ok(segments);
    }

    let mut limits = Vec::with_capacity(description.chosen_usable_ranges.len());
    for &range in &description.chosen_usable_ranges {
        if range.is_empty() {
            continue;
        }
        limits.push(native_range(range, None, "linux,usable-memory-range")?);
    }
    intersect_segments(&segments, &normalize_checked(limits)?)
}

/// 解析全部 DT 保留请求，并返回可交给页分配器的最终布局。
///
/// `additional_reserved` 用于内核镜像、外部 initramfs 等不在 DT 保留节点中的
/// 启动占用区。它们会参与动态请求选址，但仍由分配器的独立 reserved 参数记账，
/// 因而不会被合并进返回的 DT `reserved_segments`。
pub fn resolve_memory_layout(
    description: &MemoryDescription,
    base_memory: Vec<MemorySegment>,
    additional_reserved: &[MemorySegment],
) -> Result<DtbMemoryLayout, DtbMemoryLayoutError> {
    let base_memory = normalize_checked(base_memory)?;
    let additional_reserved = normalize_checked(additional_reserved.to_vec())?;
    let mut reserved_segments = Vec::new();
    for &range in &description.reservation_block_ranges {
        if range.is_empty() {
            continue;
        }
        reserved_segments.push(native_range(range, None, "memory reservation block")?);
    }

    let mut resolved_ranges: Vec<Option<Vec<MemorySegment>>> =
        description.reserved_memory.iter().map(|_| None).collect();

    // 所有静态区域必须先保留，避免后续动态请求落入一个尚未处理的 `reg` 范围。
    for (index, request) in description.reserved_memory.iter().enumerate() {
        let ReservedMemoryPlacement::Static(ranges) = &request.placement else {
            continue;
        };
        let mut native = Vec::with_capacity(ranges.len());
        for &range in ranges {
            if range.is_empty() {
                continue;
            }
            native.push(native_range(range, Some(request.node), "reg")?);
        }
        if native.is_empty() {
            return Err(DtbMemoryLayoutError::EmptyStaticReservation { node: request.node });
        }
        reserved_segments.extend(native.iter().copied());
        resolved_ranges[index] = Some(native);
    }

    reserved_segments = normalize_checked(reserved_segments)?;
    let mut usable_segments = subtract_segments(&base_memory, &reserved_segments)?;
    let mut allocation_space = subtract_segments(&usable_segments, &additional_reserved)?;

    // DTSpec 要求动态区域在所有静态区域完成保留之后分配。节点顺序作为稳定的
    // 请求顺序；单个 alloc-ranges 属性也按固件声明顺序尝试各窗口。
    for (index, request) in description.reserved_memory.iter().enumerate() {
        let ReservedMemoryPlacement::Dynamic {
            size,
            alignment,
            alloc_ranges,
        } = &request.placement
        else {
            continue;
        };
        let size = native_scalar(*size, Some(request.node), "size")?;
        if size == 0 {
            return Err(DtbMemoryLayoutError::EmptyDynamicRequest { node: request.node });
        }
        let alignment = match alignment {
            Some(0) | None => DEFAULT_DYNAMIC_ALIGNMENT,
            Some(value) => native_scalar(*value, Some(request.node), "alignment")?,
        };
        let mut windows = Vec::with_capacity(alloc_ranges.len());
        for &range in alloc_ranges {
            if range.is_empty() {
                continue;
            }
            windows.push(native_range(range, Some(request.node), "alloc-ranges")?);
        }

        let allocated = first_fit(&allocation_space, size, alignment, &windows).ok_or(
            DtbMemoryLayoutError::DynamicAllocationFailed {
                node: request.node,
                size,
            },
        )?;
        allocation_space = subtract_segments(&allocation_space, &[allocated])?;
        usable_segments = subtract_segments(&usable_segments, &[allocated])?;
        reserved_segments.push(allocated);
        resolved_ranges[index] = Some(alloc::vec![allocated]);
    }

    reserved_segments = normalize_checked(reserved_segments)?;
    let mut reserved_memory = Vec::with_capacity(description.reserved_memory.len());
    let mut no_map_segments = Vec::new();
    for (request, ranges) in description
        .reserved_memory
        .iter()
        .cloned()
        .zip(resolved_ranges.into_iter())
    {
        let ranges = ranges.expect("every valid reserved-memory request is resolved");
        if request.no_map {
            no_map_segments.extend(ranges.iter().copied());
        }
        reserved_memory.push(DtbResolvedReservedMemory { request, ranges });
    }

    Ok(DtbMemoryLayout {
        usable_segments,
        reserved_segments,
        reserved_memory,
        no_map_segments: normalize_checked(no_map_segments)?,
    })
}

/// 按架构可实现的最小页表粒度扩展 `no-map` 空洞。
///
/// 扩展部分也会从 `usable_segments` 移除并纳入合并保留范围，避免 buddy
/// 把一个已经不可映射的相邻页当成元数据或分配对象。`protected` 中的启动
/// 对象必须完全避开扩展空洞，否则启动必须 fail closed。
pub fn apply_no_map_granule(
    layout: &mut DtbMemoryLayout,
    granule: usize,
    protected: &[MemorySegment],
) -> Result<(), DtbMemoryLayoutError> {
    if granule == 0 || !granule.is_power_of_two() {
        return Err(DtbMemoryLayoutError::InvalidNoMapGranule { granule });
    }
    let mask = granule - 1;
    let mut expanded = Vec::with_capacity(layout.no_map_segments.len());
    for &range in &layout.no_map_segments {
        let exact_end = range
            .start
            .checked_add(range.size)
            .ok_or(DtbMemoryLayoutError::NoMapAlignmentOverflow { range, granule })?;
        let start = range.start & !mask;
        let end = exact_end
            .checked_add(mask)
            .ok_or(DtbMemoryLayoutError::NoMapAlignmentOverflow { range, granule })?
            & !mask;
        let aligned = MemorySegment {
            start,
            size: end - start,
        };
        for &protected in protected {
            if segments_overlap(aligned, protected) {
                return Err(DtbMemoryLayoutError::NoMapOverlapsProtected {
                    range: aligned,
                    protected,
                });
            }
        }
        expanded.push(aligned);
    }
    let expanded = normalize_checked(expanded)?;
    layout.usable_segments = subtract_segments(&layout.usable_segments, &expanded)?;
    layout.reserved_segments.extend(expanded.iter().copied());
    layout.reserved_segments = normalize_checked(core::mem::take(&mut layout.reserved_segments))?;
    layout.no_map_segments = expanded;
    Ok(())
}

/// 校验 UEFI memory map 对静态 `/reserved-memory` 的保护类型。
///
/// DTSpec 要求带 `no-map` 的静态区域使用 `EfiReservedMemoryType` (0)，其他
/// 静态区域使用 `EfiBootServicesData` (4)。动态请求由 OS 在退出 boot services
/// 后从普通可用内存中分配，不应预先对应一条专用保留描述符。
pub fn validate_uefi_reserved_memory(
    reserved_memory: &[DtbResolvedReservedMemory],
    memory_map: &[StartMemoryRegion],
) -> Result<(), DtbUefiReservationError> {
    for region in reserved_memory {
        if !matches!(region.request.placement, ReservedMemoryPlacement::Static(_)) {
            continue;
        }
        let expected_efi_type = if region.request.no_map { 0 } else { 4 };
        for &range in &region.ranges {
            if !efi_type_covers(memory_map, range, expected_efi_type) {
                return Err(DtbUefiReservationError {
                    node: region.request.node,
                    range,
                    expected_efi_type,
                });
            }
        }
    }
    Ok(())
}

fn efi_type_covers(
    memory_map: &[StartMemoryRegion],
    range: MemorySegment,
    expected_type: u32,
) -> bool {
    let Some(end) = range.start.checked_add(range.size) else {
        return false;
    };
    let mut cursor = range.start;
    while cursor < end {
        let next = memory_map
            .iter()
            .filter(|region| {
                region.source_type == Some(expected_type)
                    && region.range.start <= cursor
                    && cursor < region.range.end
            })
            .map(|region| region.range.end.min(end))
            .max();
        let Some(next) = next else {
            return false;
        };
        if next <= cursor {
            return false;
        }
        cursor = next;
    }
    true
}

fn native_scalar(
    value: u128,
    node: Option<NodeId>,
    property: &'static str,
) -> Result<usize, DtbMemoryLayoutError> {
    usize::try_from(value)
        .map_err(|_| DtbMemoryLayoutError::RangeNotRepresentable { node, property })
}

fn native_range(
    range: PhysicalRange,
    node: Option<NodeId>,
    property: &'static str,
) -> Result<MemorySegment, DtbMemoryLayoutError> {
    let start = native_scalar(range.address, node, property)?;
    let size = native_scalar(range.size, node, property)?;
    start
        .checked_add(size)
        .ok_or(DtbMemoryLayoutError::RangeOverflow { node, property })?;
    Ok(MemorySegment { start, size })
}

fn page_normalized_segment(
    segment: MemorySegment,
) -> Result<Option<MemorySegment>, DtbMemoryLayoutError> {
    let end =
        segment
            .start
            .checked_add(segment.size)
            .ok_or(DtbMemoryLayoutError::RangeOverflow {
                node: None,
                property: "NUMA memory range",
            })?;
    let start = segment
        .start
        .checked_add(PAGE_SIZE - 1)
        .map(|value| value & !(PAGE_SIZE - 1))
        .ok_or(DtbMemoryLayoutError::RangeOverflow {
            node: None,
            property: "NUMA memory range",
        })?;
    let end = end & !(PAGE_SIZE - 1);
    Ok((start < end).then_some(MemorySegment {
        start,
        size: end - start,
    }))
}

fn segments_overlap(left: MemorySegment, right: MemorySegment) -> bool {
    let Some(left_end) = left.start.checked_add(left.size) else {
        return true;
    };
    let Some(right_end) = right.start.checked_add(right.size) else {
        return true;
    };
    left.start < right_end && right.start < left_end
}

fn normalize_checked(
    mut segments: Vec<MemorySegment>,
) -> Result<Vec<MemorySegment>, DtbMemoryLayoutError> {
    segments.retain(|segment| segment.size != 0);
    for segment in &segments {
        segment
            .start
            .checked_add(segment.size)
            .ok_or(DtbMemoryLayoutError::RangeOverflow {
                node: None,
                property: "native memory map",
            })?;
    }
    segments.sort_unstable_by_key(|segment| segment.start);
    let mut merged: Vec<MemorySegment> = Vec::with_capacity(segments.len());
    for segment in segments {
        if let Some(last) = merged.last_mut() {
            let last_end = last.start + last.size;
            if last_end >= segment.start {
                let end = last_end.max(segment.start + segment.size);
                last.size = end - last.start;
                continue;
            }
        }
        merged.push(segment);
    }
    Ok(merged)
}

fn intersect_segments(
    lhs: &[MemorySegment],
    rhs: &[MemorySegment],
) -> Result<Vec<MemorySegment>, DtbMemoryLayoutError> {
    let lhs = normalize_checked(lhs.to_vec())?;
    let rhs = normalize_checked(rhs.to_vec())?;
    let mut result = Vec::new();
    let (mut left, mut right) = (0usize, 0usize);
    while left < lhs.len() && right < rhs.len() {
        let start = lhs[left].start.max(rhs[right].start);
        let end = (lhs[left].start + lhs[left].size).min(rhs[right].start + rhs[right].size);
        if start < end {
            result.push(MemorySegment {
                start,
                size: end - start,
            });
        }
        if lhs[left].start + lhs[left].size <= rhs[right].start + rhs[right].size {
            left += 1;
        } else {
            right += 1;
        }
    }
    normalize_checked(result)
}

fn subtract_segments(
    segments: &[MemorySegment],
    holes: &[MemorySegment],
) -> Result<Vec<MemorySegment>, DtbMemoryLayoutError> {
    let segments = normalize_checked(segments.to_vec())?;
    let holes = normalize_checked(holes.to_vec())?;
    if holes.is_empty() {
        return Ok(segments);
    }

    let mut result = Vec::new();
    for segment in segments {
        let end = segment.start + segment.size;
        let mut cursor = segment.start;
        for hole in &holes {
            let hole_end = hole.start + hole.size;
            if hole_end <= cursor {
                continue;
            }
            if hole.start >= end {
                break;
            }
            if cursor < hole.start {
                result.push(MemorySegment {
                    start: cursor,
                    size: hole.start - cursor,
                });
            }
            cursor = cursor.max(hole_end.min(end));
            if cursor == end {
                break;
            }
        }
        if cursor < end {
            result.push(MemorySegment {
                start: cursor,
                size: end - cursor,
            });
        }
    }
    normalize_checked(result)
}

fn first_fit(
    available: &[MemorySegment],
    size: usize,
    alignment: usize,
    windows: &[MemorySegment],
) -> Option<MemorySegment> {
    if size == 0 || alignment == 0 {
        return None;
    }

    let mut try_window = |window: MemorySegment| {
        let window_end = window.start.checked_add(window.size)?;
        for segment in available {
            let segment_end = segment.start.checked_add(segment.size)?;
            let low = segment.start.max(window.start);
            let high = segment_end.min(window_end);
            if low >= high {
                continue;
            }
            let start = align_up(low, alignment)?;
            let end = start.checked_add(size)?;
            if end <= high {
                return Some(MemorySegment { start, size });
            }
        }
        None
    };

    if windows.is_empty() {
        try_window(MemorySegment {
            start: 0,
            size: usize::MAX,
        })
    } else {
        windows.iter().copied().find_map(&mut try_window)
    }
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    let remainder = value % alignment;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(alignment - remainder)
    }
}

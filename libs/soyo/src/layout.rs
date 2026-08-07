//! 已校验 SOYO 段到用户地址空间的通用映射计划。

use alloc::vec::Vec;
use core::ops::Range;

use crate::error::{MalformedKind, ResourceKind, SoyoError};
use crate::metadata::SoyoMetadata;
use crate::registry::{PAGE_SIZE, SegmentKind};

/// 一个普通 SOYO 段在选定 image base 后的映射描述。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoyoMappedSegment {
    pub kind: SegmentKind,
    pub permissions: u16,
    pub virtual_start: u64,
    pub file_offset: u64,
    pub file_size: u64,
    pub memory_size: u64,
}

/// SOYO 运行时私有映射的规划输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoyoRuntimeLayoutInput {
    pub image_base: u64,
    pub image_virtual_size: u64,
    pub stack_top: u64,
    pub stack_size: u64,
    pub stack_guard_size: u64,
    pub tls_memory_size: u64,
    pub tls_alignment: u64,
    pub start_info_size: u64,
    pub user_lower_bound: u64,
}

/// 已验证且互不重叠的 SOYO 初始进程地址布局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoyoProcessLayout {
    pub image: Range<u64>,
    pub start_info: Range<u64>,
    pub tls: Option<Range<u64>>,
    pub initial_tls_size: u64,
    pub guard: Range<u64>,
    pub stack: Range<u64>,
}

/// 为普通映像段计算绝对用户地址；TLS template 由线程建立流程单独消费。
pub fn plan_mapped_segments(
    metadata: &SoyoMetadata,
    image_base: u64,
) -> Result<Vec<SoyoMappedSegment>, SoyoError> {
    if image_base % PAGE_SIZE != 0 {
        return Err(SoyoError::Malformed(MalformedKind::Alignment));
    }
    image_base
        .checked_add(metadata.header.image_virtual_size)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;

    let mut mapped = Vec::new();
    mapped
        .try_reserve_exact(metadata.segments.len())
        .map_err(|_| SoyoError::AllocationFailed(ResourceKind::Segments))?;
    for segment in &metadata.segments {
        if segment.kind == SegmentKind::TlsTemplate {
            continue;
        }
        let virtual_start = image_base
            .checked_add(segment.virtual_offset)
            .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
        virtual_start
            .checked_add(segment.memory_size)
            .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
        mapped.push(SoyoMappedSegment {
            kind: segment.kind,
            permissions: segment.permissions,
            virtual_start,
            file_offset: segment.file_offset,
            file_size: segment.file_size,
            memory_size: segment.memory_size,
        });
    }
    Ok(mapped)
}

/// 从 RuntimeInfo 和已编码 StartInfo 大小规划初始用户映射。
pub fn plan_runtime_layout(input: SoyoRuntimeLayoutInput) -> Result<SoyoProcessLayout, SoyoError> {
    if input.image_base % PAGE_SIZE != 0
        || input.image_virtual_size == 0
        || input.image_virtual_size % PAGE_SIZE != 0
        || input.stack_top % PAGE_SIZE != 0
        || input.stack_size == 0
        || input.stack_size % PAGE_SIZE != 0
        || input.stack_guard_size == 0
        || input.stack_guard_size % PAGE_SIZE != 0
        || input.start_info_size < native_abi::wire::START_INFO_SIZE as u64
        || input.start_info_size > 1024 * 1024
        || input.user_lower_bound % PAGE_SIZE != 0
    {
        return Err(SoyoError::Malformed(MalformedKind::Alignment));
    }
    let no_tls = input.tls_memory_size == 0 && input.tls_alignment == 0;
    let valid_tls = input.tls_memory_size != 0
        && input.tls_alignment.is_power_of_two()
        && (16..=PAGE_SIZE).contains(&input.tls_alignment);
    if !no_tls && !valid_tls {
        return Err(SoyoError::Malformed(MalformedKind::Alignment));
    }

    let image_end = input
        .image_base
        .checked_add(input.image_virtual_size)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
    let stack_start = input
        .stack_top
        .checked_sub(input.stack_size)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
    let guard_start = stack_start
        .checked_sub(input.stack_guard_size)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;

    let (tls, initial_tls_size, below_tls) = if no_tls {
        (None, 0, guard_start)
    } else {
        let initial_tls_size = align_up(input.tls_memory_size, input.tls_alignment)?;
        let mapped_tls_size = align_up(initial_tls_size, PAGE_SIZE)?;
        let tls_start = guard_start
            .checked_sub(mapped_tls_size)
            .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
        (Some(tls_start..guard_start), initial_tls_size, tls_start)
    };
    let mapped_start_info_size = align_up(input.start_info_size, PAGE_SIZE)?;
    let start_info_start = below_tls
        .checked_sub(mapped_start_info_size)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;

    if input.image_base < input.user_lower_bound
        || start_info_start < input.user_lower_bound
        || image_end > start_info_start
    {
        return Err(SoyoError::Malformed(MalformedKind::Range));
    }

    Ok(SoyoProcessLayout {
        image: input.image_base..image_end,
        start_info: start_info_start..below_tls,
        tls,
        initial_tls_size,
        guard: guard_start..stack_start,
        stack: stack_start..input.stack_top,
    })
}

fn align_up(value: u64, alignment: u64) -> Result<u64, SoyoError> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or(SoyoError::Malformed(MalformedKind::Range))
}

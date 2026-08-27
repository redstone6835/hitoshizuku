//! 已解码 SOYO metadata 的格式级不变量。

use core::cmp;

use native_abi::TargetArch;

use crate::error::{MalformedKind, ResourceKind, SoyoError};
use crate::metadata::{DirectoryEntry, ImageSegment, RuntimeInfo, SoyoHeader};
use crate::reader::{SoyoReadAt, SoyoReadError};
use crate::registry::{
    ArtifactKind, FeatureFlags, MAX_TLS_SIZE, RelocationKind, SegmentKind, SegmentPermissions,
};
use crate::source::{align_up, read_array, verify_zero_range};

pub(crate) fn validate_segments(
    segments: &[ImageSegment],
    header: &SoyoHeader,
) -> Result<(), SoyoError> {
    let mut previous_page_end = 0u64;
    let mut maximum_page_end = 0u64;
    let mut saw_code = false;
    let mut saw_tls = false;

    for segment in segments {
        let expected_permissions = match segment.kind {
            SegmentKind::Code => (SegmentPermissions::READ | SegmentPermissions::EXECUTE).bits(),
            SegmentKind::Rodata => SegmentPermissions::READ.bits(),
            SegmentKind::Data | SegmentKind::Bss | SegmentKind::TlsTemplate => {
                (SegmentPermissions::READ | SegmentPermissions::WRITE).bits()
            }
        };
        if segment.permissions != expected_permissions {
            return Err(SoyoError::Malformed(MalformedKind::Segment));
        }
        if segment.file_size > segment.memory_size || segment.memory_size == 0 {
            return Err(SoyoError::Malformed(MalformedKind::Segment));
        }

        if segment.kind == SegmentKind::TlsTemplate {
            if segment.memory_size > MAX_TLS_SIZE {
                return Err(SoyoError::ResourceExhausted(ResourceKind::TlsSize));
            }
            if saw_tls
                || segment.virtual_offset != 0
                || !segment.alignment.is_power_of_two()
                || !(16..=4096).contains(&segment.alignment)
                || (segment.file_size == 0 && segment.file_offset != 0)
                || (segment.file_size != 0 && segment.file_offset % segment.alignment != 0)
            {
                return Err(SoyoError::Malformed(MalformedKind::Segment));
            }
            saw_tls = true;
            continue;
        }
        if saw_tls || segment.virtual_offset % 4096 != 0 || segment.alignment != 4096 {
            return Err(SoyoError::Malformed(MalformedKind::Segment));
        }
        match segment.kind {
            SegmentKind::Code | SegmentKind::Rodata | SegmentKind::Data => {
                if segment.file_size == 0 || segment.file_offset % 4096 != 0 {
                    return Err(SoyoError::Malformed(MalformedKind::Segment));
                }
            }
            SegmentKind::Bss => {
                if segment.file_offset != 0 || segment.file_size != 0 {
                    return Err(SoyoError::Malformed(MalformedKind::Segment));
                }
            }
            SegmentKind::TlsTemplate => unreachable!(),
        }
        let raw_end = segment
            .virtual_offset
            .checked_add(segment.memory_size)
            .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
        let page_end = align_up(raw_end, 4096)?;
        if segment.virtual_offset < previous_page_end || page_end > header.image_virtual_size {
            return Err(SoyoError::Malformed(MalformedKind::Overlap));
        }
        previous_page_end = page_end;
        maximum_page_end = page_end;
        if segment.kind == SegmentKind::Code {
            saw_code = true;
            let code_file_end = segment
                .virtual_offset
                .checked_add(segment.file_size)
                .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
            if header.entry_offset < segment.virtual_offset || header.entry_offset >= code_file_end
            {
                continue;
            }
            let alignment = header.target_arch.instruction_alignment();
            if header.entry_offset % alignment != 0 {
                return Err(SoyoError::Malformed(MalformedKind::Alignment));
            }
        }
    }

    let entry_in_code = segments.iter().any(|segment| {
        if segment.kind != SegmentKind::Code {
            return false;
        }
        segment
            .virtual_offset
            .checked_add(segment.file_size)
            .is_some_and(|end| {
                header.entry_offset >= segment.virtual_offset && header.entry_offset < end
            })
    });
    let entry_valid = match header.artifact_kind {
        ArtifactKind::Executable => entry_in_code,
        ArtifactKind::SharedComponent => header.entry_offset == 0,
    };
    if !saw_code || !entry_valid || maximum_page_end != header.image_virtual_size {
        return Err(SoyoError::Malformed(MalformedKind::Segment));
    }
    if header.optional_features & FeatureFlags::STATIC_TLS.bits() != 0
        || saw_tls != (header.required_features & FeatureFlags::STATIC_TLS.bits() != 0)
    {
        return Err(SoyoError::Malformed(MalformedKind::Segment));
    }
    Ok(())
}

pub(crate) fn validate_segment_storage<R: SoyoReadAt>(
    source: &R,
    directory: &[DirectoryEntry],
    segments: &[ImageSegment],
    file_size: u64,
) -> Result<(), SoyoReadError<R::Error>> {
    let mut cursor = directory
        .last()
        .and_then(|entry| entry.file_offset.checked_add(entry.file_size))
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
    for segment in segments {
        if segment.file_size == 0 {
            continue;
        }
        let expected = align_up(cursor, segment.alignment)?;
        if segment.file_offset != expected {
            return Err(SoyoError::Malformed(MalformedKind::Ordering).into());
        }
        verify_zero_range(source, cursor, expected - cursor, file_size)?;
        let payload_end = segment
            .file_offset
            .checked_add(segment.file_size)
            .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
        cursor = if segment.kind == SegmentKind::TlsTemplate {
            payload_end
        } else {
            let storage_end = align_up(payload_end, 4096)?;
            verify_zero_range(source, payload_end, storage_end - payload_end, file_size)?;
            storage_end
        };
        if cursor > file_size {
            return Err(SoyoError::Malformed(MalformedKind::Range).into());
        }
    }
    if cursor != file_size {
        return Err(SoyoError::Malformed(MalformedKind::Ordering).into());
    }
    Ok(())
}

pub(crate) fn validate_relocation(
    kind: RelocationKind,
    target_segment_index: u32,
    target_offset: u64,
    source_segment_index: u32,
    addend: i64,
    segments: &[ImageSegment],
) -> Result<(), SoyoError> {
    let target = segments
        .get(target_segment_index as usize)
        .ok_or(SoyoError::Malformed(MalformedKind::Relocation))?;
    if !matches!(
        target.kind,
        SegmentKind::Rodata | SegmentKind::Data | SegmentKind::Bss
    ) || target_offset % 8 != 0
        || target_offset
            .checked_add(8)
            .is_none_or(|end| end > target.memory_size)
    {
        return Err(SoyoError::Malformed(MalformedKind::Relocation));
    }
    match kind {
        RelocationKind::ImageBase64 => {
            if source_segment_index != u32::MAX || addend < 0 {
                return Err(SoyoError::Malformed(MalformedKind::Relocation));
            }
            let value = addend as u64;
            let in_image = segments.iter().any(|segment| {
                if segment.kind == SegmentKind::TlsTemplate {
                    return false;
                }
                segment
                    .virtual_offset
                    .checked_add(segment.memory_size)
                    .is_some_and(|end| value >= segment.virtual_offset && value <= end)
            });
            if !in_image {
                return Err(SoyoError::Malformed(MalformedKind::Relocation));
            }
        }
        RelocationKind::SegmentBase64 => {
            let source = segments
                .get(source_segment_index as usize)
                .filter(|segment| segment.kind != SegmentKind::TlsTemplate)
                .ok_or(SoyoError::Malformed(MalformedKind::Relocation))?;
            if addend < 0 || addend as u64 > source.memory_size {
                return Err(SoyoError::Malformed(MalformedKind::Relocation));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_array(
    offset: u64,
    count: u32,
    entry_size: u16,
    runtime_flags: u64,
    flag: u64,
    segments: &[ImageSegment],
) -> Result<(), SoyoError> {
    if count == 0 {
        if offset != 0 || entry_size != 0 || runtime_flags & flag != 0 {
            return Err(SoyoError::Malformed(MalformedKind::Runtime));
        }
        return Ok(());
    }
    if count > 4096 || entry_size != 8 || runtime_flags & flag == 0 || offset % 8 != 0 {
        return Err(SoyoError::Malformed(MalformedKind::Runtime));
    }
    let size = u64::from(count)
        .checked_mul(8)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
    let end = offset
        .checked_add(size)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
    let in_rodata = segments.iter().any(|segment| {
        segment.kind == SegmentKind::Rodata
            && offset >= segment.virtual_offset
            && segment
                .virtual_offset
                .checked_add(segment.file_size)
                .is_some_and(|segment_end| end <= segment_end)
    });
    if !in_rodata {
        return Err(SoyoError::Malformed(MalformedKind::Runtime));
    }
    Ok(())
}

pub(crate) fn validate_array_entries<R: SoyoReadAt>(
    source: &R,
    runtime: &RuntimeInfo,
    segments: &[ImageSegment],
    target_arch: TargetArch,
    file_size: u64,
) -> Result<(), SoyoReadError<R::Error>> {
    validate_one_array_entries(
        source,
        runtime.init_array_offset,
        runtime.init_array_count,
        segments,
        target_arch,
        file_size,
    )?;
    validate_one_array_entries(
        source,
        runtime.fini_array_offset,
        runtime.fini_array_count,
        segments,
        target_arch,
        file_size,
    )
}

fn validate_one_array_entries<R: SoyoReadAt>(
    source: &R,
    offset: u64,
    count: u32,
    segments: &[ImageSegment],
    target_arch: TargetArch,
    file_size: u64,
) -> Result<(), SoyoReadError<R::Error>> {
    if count == 0 {
        return Ok(());
    }

    let array_size = u64::from(count)
        .checked_mul(8)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
    let array_segment = segments
        .iter()
        .find(|segment| {
            segment.kind == SegmentKind::Rodata
                && offset >= segment.virtual_offset
                && offset.checked_add(array_size).is_some_and(|end| {
                    segment
                        .virtual_offset
                        .checked_add(segment.file_size)
                        .is_some_and(|segment_end| end <= segment_end)
                })
        })
        .ok_or(SoyoError::Malformed(MalformedKind::Runtime))?;
    let array_file_offset = array_segment
        .file_offset
        .checked_add(offset - array_segment.virtual_offset)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
    let instruction_alignment = target_arch.instruction_alignment();

    for index in 0..count {
        let entry_file_offset = array_file_offset
            .checked_add(u64::from(index) * 8)
            .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
        let entry = u64::from_le_bytes(read_array::<R, 8>(source, entry_file_offset, file_size)?);
        let in_raw_code = segments.iter().any(|segment| {
            segment.kind == SegmentKind::Code
                && segment
                    .virtual_offset
                    .checked_add(segment.file_size)
                    .is_some_and(|end| entry >= segment.virtual_offset && entry < end)
        });
        if !in_raw_code || entry % instruction_alignment != 0 {
            return Err(SoyoError::Malformed(MalformedKind::Runtime).into());
        }
    }
    Ok(())
}

pub(crate) fn validate_string_table(strings: &[u8]) -> Result<(), SoyoError> {
    if strings.first().copied() != Some(0) {
        return Err(SoyoError::Malformed(MalformedKind::String));
    }
    Ok(())
}

pub(crate) fn validate_string_reference(strings: &[u8], offset: u32) -> Result<(), SoyoError> {
    if offset == 0 {
        return Ok(());
    }
    let start = offset as usize;
    if start >= strings.len() || strings.get(start.wrapping_sub(1)).copied() != Some(0) {
        return Err(SoyoError::Malformed(MalformedKind::String));
    }
    let tail = &strings[start..cmp::min(strings.len(), start.saturating_add(256))];
    let end = match tail.iter().position(|byte| *byte == 0) {
        Some(end) => end,
        None if tail.len() == 256 => {
            return Err(SoyoError::ResourceExhausted(ResourceKind::StringLength));
        }
        None => return Err(SoyoError::Malformed(MalformedKind::String)),
    };
    core::str::from_utf8(&tail[..end]).map_err(|_| SoyoError::Malformed(MalformedKind::String))?;
    Ok(())
}

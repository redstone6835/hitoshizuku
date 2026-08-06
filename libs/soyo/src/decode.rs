//! SOYO Wire 字段的逐项解码。

use alloc::vec::Vec;

use native_abi::{TargetArch, wire as native_wire};

use crate::error::{MalformedKind, ResourceKind, SoyoError, UnsupportedKind};
use crate::format::{validate_array, validate_relocation, validate_string_reference};
use crate::metadata::{
    AbiImport, CapabilityRequirement, DirectoryEntry, ImageSegment, Relocation, RuntimeInfo,
    SoyoHeader,
};
use crate::reader::{SoyoReadAt, SoyoReadError, SoyoReadLimits};
use crate::registry::{
    ArtifactKind, CapabilityFlags, DirectoryFlags, FORMAT_VERSION, FeatureFlags, HashAlgorithm,
    ImportFlags, MAX_CAPABILITIES, MAX_DIRECTORY_ENTRIES, MAX_IMAGE_SIZE, MAX_IMPORTS,
    MAX_RELOCATIONS, MAX_SEGMENTS, MAX_STRING_BYTES, RelocationKind, RuntimeFlags, SOYO_MAGIC,
    SegmentKind, TableType,
};
use crate::source::{
    align_up, all_zero, find_table, i64_at, range_within, read_bytes, u16_at, u32_at, u64_at,
    valid_alignment, verify_zero_range,
};
use crate::wire;

pub(crate) fn decode_header(
    bytes: &[u8; wire::HEADER_SIZE],
    actual_size: u64,
    limits: SoyoReadLimits,
) -> Result<SoyoHeader, SoyoError> {
    if bytes[wire::header::MAGIC..wire::header::MAGIC + 4] != SOYO_MAGIC {
        return Err(SoyoError::Malformed(MalformedKind::Header));
    }
    let format_version = u16_at(bytes, wire::header::FORMAT_VERSION);
    if format_version != FORMAT_VERSION {
        return Err(SoyoError::Unsupported(UnsupportedKind::FormatVersion(
            format_version,
        )));
    }
    if u16_at(bytes, wire::header::HEADER_SIZE) as usize != wire::HEADER_SIZE {
        return Err(SoyoError::Malformed(MalformedKind::Header));
    }
    let artifact_kind = u16_at(bytes, wire::header::ARTIFACT_KIND);
    if artifact_kind != ArtifactKind::Executable as u16 {
        return Err(SoyoError::Unsupported(UnsupportedKind::ArtifactKind(
            artifact_kind,
        )));
    }
    let target_arch_raw = u16_at(bytes, wire::header::TARGET_ARCH);
    let target_arch = match target_arch_raw {
        value if value == TargetArch::Riscv64 as u16 => TargetArch::Riscv64,
        value if value == TargetArch::LoongArch64 as u16 => TargetArch::LoongArch64,
        other => return Err(SoyoError::Unsupported(UnsupportedKind::TargetArch(other))),
    };
    let endian = bytes[wire::header::ENDIAN];
    if endian != 1 {
        return Err(SoyoError::Unsupported(UnsupportedKind::Endian(endian)));
    }
    let pointer_width = bytes[wire::header::POINTER_WIDTH];
    if pointer_width != 64 {
        return Err(SoyoError::Unsupported(UnsupportedKind::PointerWidth(
            pointer_width,
        )));
    }
    let abi_family = u16_at(bytes, wire::header::ABI_FAMILY);
    if abi_family == 0 || abi_family == u16::MAX {
        return Err(SoyoError::Malformed(MalformedKind::Header));
    }
    let hash_algorithm = u16_at(bytes, wire::header::HASH_ALGORITHM);
    if hash_algorithm != HashAlgorithm::Sha256 as u16 {
        return Err(SoyoError::Unsupported(UnsupportedKind::HashAlgorithm(
            hash_algorithm,
        )));
    }
    if u32_at(bytes, wire::header::FLAGS) != 0
        || u16_at(bytes, wire::header::RESERVED0) != 0
        || !all_zero(&bytes[wire::header::RESERVED1..wire::HEADER_SIZE])
    {
        return Err(SoyoError::Malformed(MalformedKind::Reserved));
    }

    let required_features = u64_at(bytes, wire::header::REQUIRED_FEATURES);
    let optional_features = u64_at(bytes, wire::header::OPTIONAL_FEATURES);
    let unknown_required = required_features & !FeatureFlags::KNOWN.bits();
    if unknown_required != 0 {
        return Err(SoyoError::Unsupported(UnsupportedKind::RequiredFeature(
            unknown_required,
        )));
    }
    if required_features & optional_features != 0 {
        return Err(SoyoError::Malformed(MalformedKind::Header));
    }

    if u64_at(bytes, wire::header::TABLE_OFFSET) != wire::HEADER_SIZE as u64
        || u16_at(bytes, wire::header::TABLE_ENTRY_SIZE) as usize != wire::DIRECTORY_ENTRY_SIZE
    {
        return Err(SoyoError::Malformed(MalformedKind::Header));
    }
    let table_count = u32_at(bytes, wire::header::TABLE_COUNT);
    if table_count == 0 {
        return Err(SoyoError::Malformed(MalformedKind::Header));
    }
    if table_count > limits.max_directory_entries.min(MAX_DIRECTORY_ENTRIES) {
        return Err(SoyoError::ResourceExhausted(ResourceKind::DirectoryCount));
    }

    let file_size = u64_at(bytes, wire::header::FILE_SIZE);
    if file_size != actual_size {
        return Err(SoyoError::Malformed(MalformedKind::Range));
    }
    let minimum_size = (wire::HEADER_SIZE as u64)
        .checked_add(
            u64::from(table_count)
                .checked_mul(wire::DIRECTORY_ENTRY_SIZE as u64)
                .ok_or(SoyoError::Malformed(MalformedKind::Range))?,
        )
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
    if file_size < minimum_size {
        return Err(SoyoError::Malformed(MalformedKind::Range));
    }

    let image_virtual_size = u64_at(bytes, wire::header::IMAGE_VIRTUAL_SIZE);
    if image_virtual_size == 0 || image_virtual_size % 4096 != 0 {
        return Err(SoyoError::Malformed(MalformedKind::Alignment));
    }
    if image_virtual_size > MAX_IMAGE_SIZE {
        return Err(SoyoError::ResourceExhausted(ResourceKind::ImageSize));
    }

    let mut build_id = [0; 32];
    build_id.copy_from_slice(&bytes[wire::header::BUILD_ID..wire::header::BUILD_ID + 32]);
    let mut content_hash = [0; 32];
    content_hash
        .copy_from_slice(&bytes[wire::header::CONTENT_HASH..wire::header::CONTENT_HASH + 32]);

    Ok(SoyoHeader {
        target_arch,
        abi_family,
        abi_epoch: u16_at(bytes, wire::header::ABI_EPOCH),
        required_features,
        optional_features,
        entry_offset: u64_at(bytes, wire::header::ENTRY_OFFSET),
        file_size,
        image_virtual_size,
        build_id,
        content_hash,
    })
}

pub(crate) fn decode_directory(
    bytes: &[u8],
    header: &SoyoHeader,
    limits: SoyoReadLimits,
) -> Result<Vec<DirectoryEntry>, SoyoError> {
    let mut directory = Vec::new();
    directory
        .try_reserve_exact(bytes.len() / wire::DIRECTORY_ENTRY_SIZE)
        .map_err(|_| SoyoError::AllocationFailed(ResourceKind::TableBytes))?;
    let mut previous_type = 0u16;
    let mut total_table_bytes = 0usize;

    for record in bytes.chunks_exact(wire::DIRECTORY_ENTRY_SIZE) {
        let table_type = u16_at(record, wire::directory::TABLE_TYPE);
        let flags = u16_at(record, wire::directory::FLAGS);
        let entry_size = u32_at(record, wire::directory::ENTRY_SIZE);
        let entry_count = u32_at(record, wire::directory::ENTRY_COUNT);
        let file_offset = u64_at(record, wire::directory::FILE_OFFSET);
        let file_size = u64_at(record, wire::directory::FILE_SIZE);
        let alignment = u64_at(record, wire::directory::ALIGNMENT);

        if table_type == 0 || table_type == u16::MAX {
            return Err(SoyoError::Malformed(MalformedKind::Header));
        }
        if table_type <= previous_type || entry_size == 0 || entry_count == 0 {
            return Err(SoyoError::Malformed(MalformedKind::Ordering));
        }
        previous_type = table_type;
        if flags & !DirectoryFlags::KNOWN.bits() != 0
            || u32_at(record, wire::directory::RESERVED0) != 0
            || u64_at(record, wire::directory::RESERVED1) != 0
        {
            return Err(SoyoError::Malformed(MalformedKind::Reserved));
        }
        if !valid_alignment(alignment) || file_offset % alignment != 0 {
            return Err(SoyoError::Malformed(MalformedKind::Alignment));
        }
        let expected_size = u64::from(entry_size)
            .checked_mul(u64::from(entry_count))
            .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
        if file_size != expected_size || !range_within(file_offset, file_size, header.file_size) {
            return Err(SoyoError::Malformed(MalformedKind::Range));
        }
        let size = usize::try_from(file_size)
            .map_err(|_| SoyoError::AllocationFailed(ResourceKind::TableBytes))?;
        if table_type <= TableType::RuntimeInfo as u16 {
            total_table_bytes = total_table_bytes
                .checked_add(size)
                .ok_or(SoyoError::ResourceExhausted(ResourceKind::TableBytes))?;
            if total_table_bytes > limits.max_table_bytes {
                return Err(SoyoError::ResourceExhausted(ResourceKind::TableBytes));
            }
        }

        directory.push(DirectoryEntry {
            table_type,
            flags,
            entry_size,
            entry_count,
            file_offset,
            file_size,
            alignment,
        });
    }
    Ok(directory)
}

pub(crate) fn validate_directory_shape(
    directory: &[DirectoryEntry],
    limits: SoyoReadLimits,
) -> Result<(), SoyoError> {
    for entry in directory {
        let standard = match entry.table_type {
            value if value == TableType::String as u16 => {
                Some((1, 1, limits.max_string_bytes.min(MAX_STRING_BYTES) as u32))
            }
            value if value == TableType::ImageSegment as u16 => Some((
                wire::IMAGE_SEGMENT_SIZE as u32,
                8,
                limits.max_segments.min(MAX_SEGMENTS),
            )),
            value if value == TableType::AbiImport as u16 => Some((
                wire::ABI_IMPORT_SIZE as u32,
                8,
                limits.max_imports.min(MAX_IMPORTS),
            )),
            value if value == TableType::CapabilityRequirement as u16 => Some((
                wire::CAPABILITY_REQUIREMENT_SIZE as u32,
                8,
                limits.max_capabilities.min(MAX_CAPABILITIES),
            )),
            value if value == TableType::Relocation as u16 => Some((
                wire::RELOCATION_SIZE as u32,
                8,
                limits.max_relocations.min(MAX_RELOCATIONS),
            )),
            value if value == TableType::RuntimeInfo as u16 => {
                Some((wire::RUNTIME_INFO_SIZE as u32, 8, 1))
            }
            _ => None,
        };
        if let Some((entry_size, alignment, max_count)) = standard {
            if entry.flags != DirectoryFlags::REQUIRED.bits()
                || entry.entry_size != entry_size
                || entry.alignment != alignment
            {
                return Err(SoyoError::Malformed(MalformedKind::Header));
            }
            if entry.entry_count > max_count {
                return Err(SoyoError::ResourceExhausted(resource_for_table(
                    entry.table_type,
                )));
            }
        } else if entry.flags & DirectoryFlags::REQUIRED.bits() != 0 {
            return Err(SoyoError::Unsupported(UnsupportedKind::RequiredTable(
                entry.table_type,
            )));
        }
    }

    for required in [
        TableType::String,
        TableType::ImageSegment,
        TableType::AbiImport,
        TableType::CapabilityRequirement,
        TableType::RuntimeInfo,
    ] {
        if find_table(directory, required as u16).is_none() {
            return Err(SoyoError::Malformed(MalformedKind::Header));
        }
    }
    Ok(())
}

pub(crate) fn validate_canonical_tables<R: SoyoReadAt>(
    source: &R,
    directory: &[DirectoryEntry],
    file_size: u64,
) -> Result<(), SoyoReadError<R::Error>> {
    let directory_bytes = (directory.len() as u64)
        .checked_mul(wire::DIRECTORY_ENTRY_SIZE as u64)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
    let mut cursor = (wire::HEADER_SIZE as u64)
        .checked_add(directory_bytes)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
    for entry in directory {
        let expected = align_up(cursor, entry.alignment)?;
        if entry.file_offset != expected {
            return Err(SoyoError::Malformed(MalformedKind::Ordering).into());
        }
        verify_zero_range(source, cursor, expected - cursor, file_size)?;
        cursor = expected
            .checked_add(entry.file_size)
            .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
    }
    Ok(())
}

pub(crate) fn read_standard_table<R: SoyoReadAt>(
    source: &R,
    directory: &[DirectoryEntry],
    table_type: TableType,
    file_size: u64,
    limits: SoyoReadLimits,
) -> Result<Vec<u8>, SoyoReadError<R::Error>> {
    let entry = find_table(directory, table_type as u16)
        .ok_or(SoyoError::Malformed(MalformedKind::Header))?;
    read_entry_bytes(source, entry, file_size, limits)
}

pub(crate) fn read_entry_bytes<R: SoyoReadAt>(
    source: &R,
    entry: &DirectoryEntry,
    file_size: u64,
    limits: SoyoReadLimits,
) -> Result<Vec<u8>, SoyoReadError<R::Error>> {
    read_bytes(
        source,
        entry.file_offset,
        entry.file_size,
        file_size,
        limits,
    )
}

pub(crate) fn decode_segments(bytes: &[u8]) -> Result<Vec<ImageSegment>, SoyoError> {
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(bytes.len() / wire::IMAGE_SEGMENT_SIZE)
        .map_err(|_| SoyoError::AllocationFailed(ResourceKind::Segments))?;
    for record in bytes.chunks_exact(wire::IMAGE_SEGMENT_SIZE) {
        if u32_at(record, wire::image_segment::FLAGS) != 0
            || u64_at(record, wire::image_segment::RESERVED0) != 0
            || u64_at(record, wire::image_segment::RESERVED1) != 0
        {
            return Err(SoyoError::Malformed(MalformedKind::Reserved));
        }
        let kind = match u16_at(record, wire::image_segment::KIND) {
            1 => SegmentKind::Code,
            2 => SegmentKind::Rodata,
            3 => SegmentKind::Data,
            4 => SegmentKind::Bss,
            5 => SegmentKind::TlsTemplate,
            _ => return Err(SoyoError::Malformed(MalformedKind::Segment)),
        };
        segments.push(ImageSegment {
            kind,
            permissions: u16_at(record, wire::image_segment::PERMISSIONS),
            virtual_offset: u64_at(record, wire::image_segment::VIRTUAL_OFFSET),
            file_offset: u64_at(record, wire::image_segment::FILE_OFFSET),
            file_size: u64_at(record, wire::image_segment::FILE_SIZE),
            memory_size: u64_at(record, wire::image_segment::MEMORY_SIZE),
            alignment: u64_at(record, wire::image_segment::ALIGNMENT),
        });
    }
    Ok(segments)
}

pub(crate) fn decode_imports(bytes: &[u8], strings: &[u8]) -> Result<Vec<AbiImport>, SoyoError> {
    let mut imports = Vec::new();
    imports
        .try_reserve_exact(bytes.len() / wire::ABI_IMPORT_SIZE)
        .map_err(|_| SoyoError::AllocationFailed(ResourceKind::Imports))?;
    let mut previous_operation = 0u32;
    for (index, record) in bytes.chunks_exact(wire::ABI_IMPORT_SIZE).enumerate() {
        let slot = u32_at(record, wire::abi_import::SLOT);
        let operation_id = u32_at(record, wire::abi_import::OPERATION_ID);
        let flags = u32_at(record, wire::abi_import::FLAGS);
        let name = u32_at(record, wire::abi_import::DIAGNOSTIC_NAME_OFFSET);
        if slot != index as u32
            || operation_id == 0
            || operation_id == u32::MAX
            || operation_id <= previous_operation
            || (flags != ImportFlags::REQUIRED.bits() && flags != ImportFlags::OPTIONAL.bits())
        {
            return Err(SoyoError::Malformed(MalformedKind::Import));
        }
        previous_operation = operation_id;
        if !all_zero(&record[wire::abi_import::RESERVED..wire::ABI_IMPORT_SIZE]) {
            return Err(SoyoError::Malformed(MalformedKind::Reserved));
        }
        validate_string_reference(strings, name)?;
        let mut signature_hash = [0; 32];
        signature_hash.copy_from_slice(
            &record[wire::abi_import::SIGNATURE_HASH..wire::abi_import::SIGNATURE_HASH + 32],
        );
        imports.push(AbiImport {
            slot,
            operation_id,
            flags,
            diagnostic_name_offset: name,
            signature_hash,
        });
    }
    Ok(imports)
}

pub(crate) fn decode_capabilities(
    bytes: &[u8],
    strings: &[u8],
) -> Result<Vec<CapabilityRequirement>, SoyoError> {
    let mut capabilities = Vec::new();
    capabilities
        .try_reserve_exact(bytes.len() / wire::CAPABILITY_REQUIREMENT_SIZE)
        .map_err(|_| SoyoError::AllocationFailed(ResourceKind::Capabilities))?;
    let mut previous_id = 0u32;
    for record in bytes.chunks_exact(wire::CAPABILITY_REQUIREMENT_SIZE) {
        let requirement_id = u32_at(record, wire::capability_requirement::REQUIREMENT_ID);
        let object_interface = u16_at(record, wire::capability_requirement::OBJECT_INTERFACE);
        let flags = u16_at(record, wire::capability_requirement::FLAGS);
        let name = u32_at(record, wire::capability_requirement::DIAGNOSTIC_NAME_OFFSET);
        if requirement_id == 0
            || requirement_id == u32::MAX
            || object_interface == 0
            || object_interface == u16::MAX
            || requirement_id <= previous_id
            || (flags != CapabilityFlags::REQUIRED.bits()
                && flags != CapabilityFlags::OPTIONAL.bits())
        {
            return Err(SoyoError::Malformed(MalformedKind::Capability));
        }
        previous_id = requirement_id;
        if u32_at(record, wire::capability_requirement::RESERVED0) != 0
            || !all_zero(
                &record[wire::capability_requirement::RESERVED1..wire::CAPABILITY_REQUIREMENT_SIZE],
            )
        {
            return Err(SoyoError::Malformed(MalformedKind::Reserved));
        }
        validate_string_reference(strings, name)?;
        capabilities.push(CapabilityRequirement {
            requirement_id,
            object_interface,
            flags,
            required_rights: u64_at(record, wire::capability_requirement::REQUIRED_RIGHTS),
            diagnostic_name_offset: name,
        });
    }
    Ok(capabilities)
}

pub(crate) fn decode_relocations(
    bytes: &[u8],
    segments: &[ImageSegment],
) -> Result<Vec<Relocation>, SoyoError> {
    let mut relocations = Vec::new();
    relocations
        .try_reserve_exact(bytes.len() / wire::RELOCATION_SIZE)
        .map_err(|_| SoyoError::AllocationFailed(ResourceKind::Relocations))?;
    let mut previous = None;
    for record in bytes.chunks_exact(wire::RELOCATION_SIZE) {
        if u16_at(record, wire::relocation::FLAGS) != 0
            || u32_at(record, wire::relocation::RESERVED0) != 0
            || u64_at(record, wire::relocation::RESERVED1) != 0
            || u64_at(record, wire::relocation::RESERVED2) != 0
        {
            return Err(SoyoError::Malformed(MalformedKind::Reserved));
        }
        let kind = match u16_at(record, wire::relocation::KIND) {
            1 => RelocationKind::ImageBase64,
            2 => RelocationKind::SegmentBase64,
            _ => return Err(SoyoError::Malformed(MalformedKind::Relocation)),
        };
        let target_segment_index = u32_at(record, wire::relocation::TARGET_SEGMENT_INDEX);
        let target_offset = u64_at(record, wire::relocation::TARGET_OFFSET);
        let source_segment_index = u32_at(record, wire::relocation::SOURCE_SEGMENT_INDEX);
        let addend = i64_at(record, wire::relocation::ADDEND);
        let key = (target_segment_index, target_offset);
        if previous.is_some_and(|value| value >= key) {
            return Err(SoyoError::Malformed(MalformedKind::Ordering));
        }
        previous = Some(key);
        validate_relocation(
            kind,
            target_segment_index,
            target_offset,
            source_segment_index,
            addend,
            segments,
        )?;
        relocations.push(Relocation {
            kind,
            target_segment_index,
            target_offset,
            source_segment_index,
            addend,
        });
    }
    Ok(relocations)
}

pub(crate) fn decode_runtime(
    bytes: &[u8],
    segments: &[ImageSegment],
    required_features: u64,
    optional_features: u64,
) -> Result<RuntimeInfo, SoyoError> {
    if bytes.len() != wire::RUNTIME_INFO_SIZE {
        return Err(SoyoError::Malformed(MalformedKind::Runtime));
    }
    if u16_at(bytes, wire::runtime_info::RESERVED0) != 0
        || u16_at(bytes, wire::runtime_info::RESERVED1) != 0
        || !all_zero(&bytes[wire::runtime_info::RESERVED2..wire::RUNTIME_INFO_SIZE])
    {
        return Err(SoyoError::Malformed(MalformedKind::Reserved));
    }
    let runtime = RuntimeInfo {
        stack_size: u64_at(bytes, wire::runtime_info::STACK_SIZE),
        stack_guard_size: u64_at(bytes, wire::runtime_info::STACK_GUARD_SIZE),
        runtime_flags: u64_at(bytes, wire::runtime_info::RUNTIME_FLAGS),
        init_array_offset: u64_at(bytes, wire::runtime_info::INIT_ARRAY_OFFSET),
        init_array_count: u32_at(bytes, wire::runtime_info::INIT_ARRAY_COUNT),
        init_array_entry_size: u16_at(bytes, wire::runtime_info::INIT_ARRAY_ENTRY_SIZE),
        fini_array_offset: u64_at(bytes, wire::runtime_info::FINI_ARRAY_OFFSET),
        fini_array_count: u32_at(bytes, wire::runtime_info::FINI_ARRAY_COUNT),
        fini_array_entry_size: u16_at(bytes, wire::runtime_info::FINI_ARRAY_ENTRY_SIZE),
        stack_alignment: u32_at(bytes, wire::runtime_info::STACK_ALIGNMENT),
        start_info_max_size: u32_at(bytes, wire::runtime_info::START_INFO_MAX_SIZE),
    };
    if runtime.stack_size < 64 * 1024
        || runtime.stack_size > 64 * 1024 * 1024
        || runtime.stack_size % 4096 != 0
        || runtime.stack_guard_size < 4096
        || runtime.stack_guard_size > 1024 * 1024
        || runtime.stack_guard_size % 4096 != 0
        || runtime.stack_alignment != 16
        || runtime.start_info_max_size < native_wire::START_INFO_SIZE as u32
        || runtime.start_info_max_size > 1024 * 1024
        || runtime.runtime_flags & !RuntimeFlags::KNOWN.bits() != 0
    {
        return Err(SoyoError::Malformed(MalformedKind::Runtime));
    }
    validate_array(
        runtime.init_array_offset,
        runtime.init_array_count,
        runtime.init_array_entry_size,
        runtime.runtime_flags,
        RuntimeFlags::RUN_INIT_ARRAY.bits(),
        segments,
    )?;
    validate_array(
        runtime.fini_array_offset,
        runtime.fini_array_count,
        runtime.fini_array_entry_size,
        runtime.runtime_flags,
        RuntimeFlags::RUN_FINI_ARRAY.bits(),
        segments,
    )?;
    let has_arrays = runtime.init_array_count != 0 || runtime.fini_array_count != 0;
    if optional_features & FeatureFlags::INIT_FINI_ARRAY.bits() != 0
        || has_arrays != (required_features & FeatureFlags::INIT_FINI_ARRAY.bits() != 0)
    {
        return Err(SoyoError::Malformed(MalformedKind::Runtime));
    }
    Ok(runtime)
}

fn resource_for_table(table_type: u16) -> ResourceKind {
    match table_type {
        value if value == TableType::String as u16 => ResourceKind::StringBytes,
        value if value == TableType::ImageSegment as u16 => ResourceKind::Segments,
        value if value == TableType::AbiImport as u16 => ResourceKind::Imports,
        value if value == TableType::CapabilityRequirement as u16 => ResourceKind::Capabilities,
        value if value == TableType::Relocation as u16 => ResourceKind::Relocations,
        _ => ResourceKind::TableBytes,
    }
}

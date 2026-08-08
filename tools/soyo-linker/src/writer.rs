//! 已链接映像的 canonical SOYO 编码与共享解析器自检。

use std::fmt;

use native_abi::{ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, operation, requirement};
use sha2::{Digest, Sha256};
use soyo::registry::{
    ArtifactKind, CapabilityFlags, DirectoryFlags, FORMAT_VERSION, FeatureFlags, HashAlgorithm,
    ImportFlags, MAX_FILE_SIZE, MAX_SEGMENTS, MAX_STRING_BYTES, PAGE_SIZE, RelocationKind,
    RuntimeFlags, SOYO_MAGIC, SegmentKind, SegmentPermissions, TableType,
};
use soyo::wire;
use soyo::{SliceSoyoReader, SoyoReadLimits, SoyoTargetPolicy, read_soyo, validate_soyo};

use crate::contract::ProgramContract;
use crate::link::{LinkSegment, LinkedImage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeErrorKind {
    InvalidLinkedImage,
    OutputTooLarge,
    SelfValidation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeError {
    kind: EncodeErrorKind,
    detail: String,
}

impl EncodeError {
    pub const fn kind(&self) -> EncodeErrorKind {
        self.kind
    }

    fn new(kind: EncodeErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for EncodeError {}

#[derive(Debug)]
struct EncodedTable {
    table_type: TableType,
    entry_size: u32,
    entry_count: u32,
    alignment: u64,
    file_offset: u64,
    bytes: Vec<u8>,
}

/// 把目标架构 relocation 已完成的映像编码为 SOYO。
pub fn encode_soyo(
    image: &LinkedImage,
    contract: &ProgramContract,
) -> Result<Vec<u8>, EncodeError> {
    validate_linked_image(image)?;
    let (strings, import_name_offsets, capability_name_offsets) = build_strings(contract)?;
    let mut tables = build_tables(
        image,
        contract,
        strings,
        &import_name_offsets,
        &capability_name_offsets,
    )?;
    layout_tables(&mut tables)?;
    let metadata_end = tables
        .last()
        .and_then(|table| table.file_offset.checked_add(table.bytes.len() as u64))
        .ok_or_else(|| invalid_image("缺少 SOYO metadata table"))?;
    let (segment_file_offsets, file_size) = layout_segment_payloads(image, metadata_end)?;
    encode_segment_table(image, &segment_file_offsets, &mut tables)?;

    if file_size > MAX_FILE_SIZE {
        return Err(EncodeError::new(
            EncodeErrorKind::OutputTooLarge,
            "SOYO 输出超过文件大小上限",
        ));
    }
    let file_size_usize = usize::try_from(file_size).map_err(|_| {
        EncodeError::new(
            EncodeErrorKind::OutputTooLarge,
            "SOYO 输出超过宿主 usize 范围",
        )
    })?;
    let mut output = vec![0; file_size_usize];
    encode_header(&mut output, image, &tables, file_size)?;
    encode_directory(&mut output, &tables);
    for table in &tables {
        write_bytes(&mut output, table.file_offset, &table.bytes)?;
    }
    for (segment, file_offset) in image.segments().iter().zip(segment_file_offsets) {
        if file_offset != 0 {
            write_bytes(&mut output, file_offset, segment.payload())?;
        }
    }
    rehash(&mut output);
    self_validate(&output, image.target_arch())?;
    Ok(output)
}

fn validate_linked_image(image: &LinkedImage) -> Result<(), EncodeError> {
    if image.segments().is_empty()
        || image.segments().len() > MAX_SEGMENTS as usize
        || image
            .segments()
            .first()
            .is_none_or(|segment| segment.kind() != SegmentKind::Code)
        || image.image_virtual_size() == 0
        || image.image_virtual_size() % PAGE_SIZE != 0
    {
        return Err(invalid_image("链接映像的段或虚拟尺寸无效"));
    }
    Ok(())
}

fn build_strings(contract: &ProgramContract) -> Result<(Vec<u8>, Vec<u32>, Vec<u32>), EncodeError> {
    let mut strings = vec![0];
    let mut import_offsets = Vec::with_capacity(contract.imports().len());
    for import in contract.imports() {
        let name = operation(import.operation)
            .expect("已归一化 operation 必须位于 registry")
            .name;
        import_offsets.push(push_string(&mut strings, name)?);
    }
    let mut capability_offsets = Vec::with_capacity(contract.capabilities().len());
    for capability in contract.capabilities() {
        let name = requirement(capability.requirement)
            .expect("已归一化 requirement 必须位于 registry")
            .name;
        capability_offsets.push(push_string(&mut strings, name)?);
    }
    Ok((strings, import_offsets, capability_offsets))
}

fn push_string(strings: &mut Vec<u8>, value: &str) -> Result<u32, EncodeError> {
    let offset = u32::try_from(strings.len()).map_err(|_| {
        EncodeError::new(EncodeErrorKind::OutputTooLarge, "SOYO 字符串表 offset 溢出")
    })?;
    strings.extend_from_slice(value.as_bytes());
    strings.push(0);
    if strings.len() > MAX_STRING_BYTES {
        return Err(EncodeError::new(
            EncodeErrorKind::OutputTooLarge,
            "SOYO 字符串表超过上限",
        ));
    }
    Ok(offset)
}

fn build_tables(
    image: &LinkedImage,
    contract: &ProgramContract,
    strings: Vec<u8>,
    import_name_offsets: &[u32],
    capability_name_offsets: &[u32],
) -> Result<Vec<EncodedTable>, EncodeError> {
    let segment_count = u32::try_from(image.segments().len())
        .map_err(|_| invalid_image("SOYO segment count 溢出"))?;
    let import_count = u32::try_from(contract.imports().len())
        .map_err(|_| invalid_image("SOYO import count 溢出"))?;
    let capability_count = u32::try_from(contract.capabilities().len())
        .map_err(|_| invalid_image("SOYO capability count 溢出"))?;
    let relocation_count = u32::try_from(image.runtime_relocations().len())
        .map_err(|_| invalid_image("SOYO relocation count 溢出"))?;

    let mut tables = vec![
        EncodedTable {
            table_type: TableType::String,
            entry_size: 1,
            entry_count: strings.len() as u32,
            alignment: 1,
            file_offset: 0,
            bytes: strings,
        },
        EncodedTable {
            table_type: TableType::ImageSegment,
            entry_size: wire::IMAGE_SEGMENT_SIZE as u32,
            entry_count: segment_count,
            alignment: 8,
            file_offset: 0,
            bytes: vec![0; image.segments().len() * wire::IMAGE_SEGMENT_SIZE],
        },
        EncodedTable {
            table_type: TableType::AbiImport,
            entry_size: wire::ABI_IMPORT_SIZE as u32,
            entry_count: import_count,
            alignment: 8,
            file_offset: 0,
            bytes: encode_imports(contract, import_name_offsets),
        },
        EncodedTable {
            table_type: TableType::CapabilityRequirement,
            entry_size: wire::CAPABILITY_REQUIREMENT_SIZE as u32,
            entry_count: capability_count,
            alignment: 8,
            file_offset: 0,
            bytes: encode_capabilities(contract, capability_name_offsets),
        },
    ];
    if relocation_count != 0 {
        tables.push(EncodedTable {
            table_type: TableType::Relocation,
            entry_size: wire::RELOCATION_SIZE as u32,
            entry_count: relocation_count,
            alignment: 8,
            file_offset: 0,
            bytes: encode_relocations(image),
        });
    }
    tables.push(EncodedTable {
        table_type: TableType::RuntimeInfo,
        entry_size: wire::RUNTIME_INFO_SIZE as u32,
        entry_count: 1,
        alignment: 8,
        file_offset: 0,
        bytes: encode_runtime(image, contract),
    });
    Ok(tables)
}

fn encode_imports(contract: &ProgramContract, name_offsets: &[u32]) -> Vec<u8> {
    let mut bytes = vec![0; contract.imports().len() * wire::ABI_IMPORT_SIZE];
    for (slot, (import, name_offset)) in contract.imports().iter().zip(name_offsets).enumerate() {
        let offset = slot * wire::ABI_IMPORT_SIZE;
        let spec = operation(import.operation).expect("已归一化 operation");
        put_u32(&mut bytes, offset + wire::abi_import::SLOT, slot as u32);
        put_u32(
            &mut bytes,
            offset + wire::abi_import::OPERATION_ID,
            spec.id as u32,
        );
        put_u32(
            &mut bytes,
            offset + wire::abi_import::FLAGS,
            if import.required {
                ImportFlags::REQUIRED.bits()
            } else {
                ImportFlags::OPTIONAL.bits()
            },
        );
        put_u32(
            &mut bytes,
            offset + wire::abi_import::DIAGNOSTIC_NAME_OFFSET,
            *name_offset,
        );
        bytes[offset + wire::abi_import::SIGNATURE_HASH
            ..offset + wire::abi_import::SIGNATURE_HASH + 32]
            .copy_from_slice(&spec.signature_hash);
    }
    bytes
}

fn encode_capabilities(contract: &ProgramContract, name_offsets: &[u32]) -> Vec<u8> {
    let mut bytes = vec![0; contract.capabilities().len() * wire::CAPABILITY_REQUIREMENT_SIZE];
    for (index, (capability, name_offset)) in
        contract.capabilities().iter().zip(name_offsets).enumerate()
    {
        let offset = index * wire::CAPABILITY_REQUIREMENT_SIZE;
        let spec = requirement(capability.requirement).expect("已归一化 requirement");
        put_u32(
            &mut bytes,
            offset + wire::capability_requirement::REQUIREMENT_ID,
            spec.id as u32,
        );
        put_u16(
            &mut bytes,
            offset + wire::capability_requirement::OBJECT_INTERFACE,
            spec.interface as u16,
        );
        put_u16(
            &mut bytes,
            offset + wire::capability_requirement::FLAGS,
            if capability.required {
                CapabilityFlags::REQUIRED.bits()
            } else {
                CapabilityFlags::OPTIONAL.bits()
            },
        );
        put_u64(
            &mut bytes,
            offset + wire::capability_requirement::REQUIRED_RIGHTS,
            capability.rights.bits(),
        );
        put_u32(
            &mut bytes,
            offset + wire::capability_requirement::DIAGNOSTIC_NAME_OFFSET,
            *name_offset,
        );
    }
    bytes
}

fn encode_relocations(image: &LinkedImage) -> Vec<u8> {
    let mut bytes = vec![0; image.runtime_relocations().len() * wire::RELOCATION_SIZE];
    for (index, relocation) in image.runtime_relocations().iter().enumerate() {
        let offset = index * wire::RELOCATION_SIZE;
        put_u16(
            &mut bytes,
            offset + wire::relocation::KIND,
            match relocation.kind {
                RelocationKind::ImageBase64 => RelocationKind::ImageBase64 as u16,
                RelocationKind::SegmentBase64 => RelocationKind::SegmentBase64 as u16,
            },
        );
        put_u32(
            &mut bytes,
            offset + wire::relocation::TARGET_SEGMENT_INDEX,
            relocation.target_segment_index,
        );
        put_u64(
            &mut bytes,
            offset + wire::relocation::TARGET_OFFSET,
            relocation.target_offset,
        );
        put_u32(
            &mut bytes,
            offset + wire::relocation::SOURCE_SEGMENT_INDEX,
            relocation.source_segment_index,
        );
        put_i64(
            &mut bytes,
            offset + wire::relocation::ADDEND,
            relocation.addend,
        );
    }
    bytes
}

fn encode_runtime(image: &LinkedImage, contract: &ProgramContract) -> Vec<u8> {
    let runtime = contract.runtime();
    let arrays = image.runtime_arrays();
    let mut bytes = vec![0; wire::RUNTIME_INFO_SIZE];
    put_u64(
        &mut bytes,
        wire::runtime_info::STACK_SIZE,
        runtime.stack_size,
    );
    put_u64(
        &mut bytes,
        wire::runtime_info::STACK_GUARD_SIZE,
        runtime.stack_guard_size,
    );
    let mut runtime_flags = 0;
    if arrays.init_count() != 0 {
        runtime_flags |= RuntimeFlags::RUN_INIT_ARRAY.bits();
        put_u64(
            &mut bytes,
            wire::runtime_info::INIT_ARRAY_OFFSET,
            arrays.init_offset(),
        );
        put_u32(
            &mut bytes,
            wire::runtime_info::INIT_ARRAY_COUNT,
            arrays.init_count(),
        );
        put_u16(&mut bytes, wire::runtime_info::INIT_ARRAY_ENTRY_SIZE, 8);
    }
    if arrays.fini_count() != 0 {
        runtime_flags |= RuntimeFlags::RUN_FINI_ARRAY.bits();
        put_u64(
            &mut bytes,
            wire::runtime_info::FINI_ARRAY_OFFSET,
            arrays.fini_offset(),
        );
        put_u32(
            &mut bytes,
            wire::runtime_info::FINI_ARRAY_COUNT,
            arrays.fini_count(),
        );
        put_u16(&mut bytes, wire::runtime_info::FINI_ARRAY_ENTRY_SIZE, 8);
    }
    put_u64(&mut bytes, wire::runtime_info::RUNTIME_FLAGS, runtime_flags);
    put_u32(&mut bytes, wire::runtime_info::STACK_ALIGNMENT, 16);
    put_u32(
        &mut bytes,
        wire::runtime_info::START_INFO_MAX_SIZE,
        runtime.start_info_max_size,
    );
    bytes
}

fn layout_tables(tables: &mut [EncodedTable]) -> Result<(), EncodeError> {
    let directory_size = tables
        .len()
        .checked_mul(wire::DIRECTORY_ENTRY_SIZE)
        .ok_or_else(|| output_too_large("SOYO directory 尺寸溢出"))?;
    let mut cursor = (wire::HEADER_SIZE + directory_size) as u64;
    for table in tables {
        cursor = align_up(cursor, table.alignment)?;
        table.file_offset = cursor;
        cursor = cursor
            .checked_add(table.bytes.len() as u64)
            .ok_or_else(|| output_too_large("SOYO metadata 尺寸溢出"))?;
    }
    Ok(())
}

fn layout_segment_payloads(
    image: &LinkedImage,
    metadata_end: u64,
) -> Result<(Vec<u64>, u64), EncodeError> {
    let mut cursor = metadata_end;
    let mut offsets = Vec::with_capacity(image.segments().len());
    for segment in image.segments() {
        if segment.payload().is_empty() {
            offsets.push(0);
            continue;
        }
        cursor = align_up(cursor, segment.alignment())?;
        offsets.push(cursor);
        cursor = cursor
            .checked_add(segment.payload().len() as u64)
            .ok_or_else(|| output_too_large("SOYO segment payload 尺寸溢出"))?;
        if segment.kind() != SegmentKind::TlsTemplate {
            cursor = align_up(cursor, PAGE_SIZE)?;
        }
    }
    Ok((offsets, cursor))
}

fn encode_segment_table(
    image: &LinkedImage,
    file_offsets: &[u64],
    tables: &mut [EncodedTable],
) -> Result<(), EncodeError> {
    let table = tables
        .iter_mut()
        .find(|table| table.table_type == TableType::ImageSegment)
        .ok_or_else(|| invalid_image("缺少 ImageSegment table"))?;
    for (index, (segment, file_offset)) in image.segments().iter().zip(file_offsets).enumerate() {
        let offset = index * wire::IMAGE_SEGMENT_SIZE;
        put_u16(
            &mut table.bytes,
            offset + wire::image_segment::KIND,
            segment.kind() as u16,
        );
        put_u16(
            &mut table.bytes,
            offset + wire::image_segment::PERMISSIONS,
            segment_permissions(segment).bits(),
        );
        put_u64(
            &mut table.bytes,
            offset + wire::image_segment::VIRTUAL_OFFSET,
            segment.virtual_offset(),
        );
        put_u64(
            &mut table.bytes,
            offset + wire::image_segment::FILE_OFFSET,
            *file_offset,
        );
        put_u64(
            &mut table.bytes,
            offset + wire::image_segment::FILE_SIZE,
            segment.payload().len() as u64,
        );
        put_u64(
            &mut table.bytes,
            offset + wire::image_segment::MEMORY_SIZE,
            segment.memory_size(),
        );
        put_u64(
            &mut table.bytes,
            offset + wire::image_segment::ALIGNMENT,
            segment.alignment(),
        );
    }
    Ok(())
}

fn segment_permissions(segment: &LinkSegment) -> SegmentPermissions {
    match segment.kind() {
        SegmentKind::Code => SegmentPermissions::READ | SegmentPermissions::EXECUTE,
        SegmentKind::Rodata => SegmentPermissions::READ,
        SegmentKind::Data | SegmentKind::Bss | SegmentKind::TlsTemplate => {
            SegmentPermissions::READ | SegmentPermissions::WRITE
        }
    }
}

fn encode_header(
    output: &mut [u8],
    image: &LinkedImage,
    tables: &[EncodedTable],
    file_size: u64,
) -> Result<(), EncodeError> {
    output[wire::header::MAGIC..wire::header::MAGIC + 4].copy_from_slice(&SOYO_MAGIC);
    put_u16(output, wire::header::FORMAT_VERSION, FORMAT_VERSION);
    put_u16(output, wire::header::HEADER_SIZE, wire::HEADER_SIZE as u16);
    put_u16(
        output,
        wire::header::ARTIFACT_KIND,
        ArtifactKind::Executable as u16,
    );
    put_u16(
        output,
        wire::header::TARGET_ARCH,
        image.target_arch() as u16,
    );
    output[wire::header::ENDIAN] = 1;
    output[wire::header::POINTER_WIDTH] = 64;
    put_u16(output, wire::header::ABI_FAMILY, ABI_FAMILY_MYGO_NATIVE);
    put_u16(output, wire::header::ABI_EPOCH, ABI_EPOCH);
    put_u16(
        output,
        wire::header::HASH_ALGORITHM,
        HashAlgorithm::Sha256 as u16,
    );
    if image
        .segments()
        .iter()
        .any(|segment| segment.kind() == SegmentKind::TlsTemplate)
    {
        put_u64(
            output,
            wire::header::REQUIRED_FEATURES,
            FeatureFlags::STATIC_TLS.bits(),
        );
    }
    if !image.runtime_arrays().is_empty() {
        let existing = u64::from_le_bytes(
            output[wire::header::REQUIRED_FEATURES..wire::header::REQUIRED_FEATURES + 8]
                .try_into()
                .expect("Header required feature 字段大小固定"),
        );
        put_u64(
            output,
            wire::header::REQUIRED_FEATURES,
            existing | FeatureFlags::INIT_FINI_ARRAY.bits(),
        );
    }
    put_u64(output, wire::header::ENTRY_OFFSET, image.entry_offset());
    put_u64(output, wire::header::TABLE_OFFSET, wire::HEADER_SIZE as u64);
    put_u32(
        output,
        wire::header::TABLE_COUNT,
        u32::try_from(tables.len()).map_err(|_| output_too_large("SOYO table count 溢出"))?,
    );
    put_u16(
        output,
        wire::header::TABLE_ENTRY_SIZE,
        wire::DIRECTORY_ENTRY_SIZE as u16,
    );
    put_u64(output, wire::header::FILE_SIZE, file_size);
    put_u64(
        output,
        wire::header::IMAGE_VIRTUAL_SIZE,
        image.image_virtual_size(),
    );
    Ok(())
}

fn encode_directory(output: &mut [u8], tables: &[EncodedTable]) {
    for (index, table) in tables.iter().enumerate() {
        let offset = wire::HEADER_SIZE + index * wire::DIRECTORY_ENTRY_SIZE;
        put_u16(
            output,
            offset + wire::directory::TABLE_TYPE,
            table.table_type as u16,
        );
        put_u16(
            output,
            offset + wire::directory::FLAGS,
            DirectoryFlags::REQUIRED.bits(),
        );
        put_u32(
            output,
            offset + wire::directory::ENTRY_SIZE,
            table.entry_size,
        );
        put_u32(
            output,
            offset + wire::directory::ENTRY_COUNT,
            table.entry_count,
        );
        put_u64(
            output,
            offset + wire::directory::FILE_OFFSET,
            table.file_offset,
        );
        put_u64(
            output,
            offset + wire::directory::FILE_SIZE,
            table.bytes.len() as u64,
        );
        put_u64(output, offset + wire::directory::ALIGNMENT, table.alignment);
    }
}

fn write_bytes(output: &mut [u8], file_offset: u64, bytes: &[u8]) -> Result<(), EncodeError> {
    let start = usize::try_from(file_offset)
        .map_err(|_| output_too_large("SOYO file offset 超过 usize"))?;
    let end = start
        .checked_add(bytes.len())
        .ok_or_else(|| output_too_large("SOYO file range 溢出"))?;
    output
        .get_mut(start..end)
        .ok_or_else(|| invalid_image("SOYO encoder 生成了越界文件范围"))?
        .copy_from_slice(bytes);
    Ok(())
}

fn rehash(output: &mut [u8]) {
    output[wire::header::BUILD_ID..wire::header::CONTENT_HASH + 32].fill(0);
    let digest: [u8; 32] = Sha256::digest(&*output).into();
    output[wire::header::BUILD_ID..wire::header::BUILD_ID + 32].copy_from_slice(&digest);
    output[wire::header::CONTENT_HASH..wire::header::CONTENT_HASH + 32].copy_from_slice(&digest);
}

fn self_validate(output: &[u8], target: native_abi::TargetArch) -> Result<(), EncodeError> {
    let metadata =
        read_soyo(&SliceSoyoReader::new(output), SoyoReadLimits::portable()).map_err(|error| {
            EncodeError::new(
                EncodeErrorKind::SelfValidation,
                format!("SOYO encoder 自检解析失败: {error:?}"),
            )
        })?;
    validate_soyo(&metadata, SoyoTargetPolicy::for_kernel(target)).map_err(|error| {
        EncodeError::new(
            EncodeErrorKind::SelfValidation,
            format!("SOYO encoder 自检绑定失败: {error:?}"),
        )
    })?;
    Ok(())
}

fn align_up(value: u64, alignment: u64) -> Result<u64, EncodeError> {
    if !alignment.is_power_of_two() {
        return Err(invalid_image("链接映像包含非 2 的幂对齐"));
    }
    value
        .checked_add(alignment - 1)
        .map(|sum| sum & !(alignment - 1))
        .ok_or_else(|| output_too_large("SOYO alignment 溢出"))
}

fn invalid_image(detail: impl Into<String>) -> EncodeError {
    EncodeError::new(EncodeErrorKind::InvalidLinkedImage, detail)
}

fn output_too_large(detail: impl Into<String>) -> EncodeError {
    EncodeError::new(EncodeErrorKind::OutputTooLarge, detail)
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_i64(bytes: &mut [u8], offset: usize, value: i64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

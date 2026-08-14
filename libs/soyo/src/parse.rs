//! SOYO 文件读取与校验流程编排。

use crate::component::validate_component_metadata;
use crate::decode::{
    decode_capabilities, decode_component, decode_directory, decode_header, decode_imports,
    decode_relocations, decode_runtime, decode_segments, read_optional_standard_table,
    read_standard_table, validate_canonical_tables, validate_directory_shape,
};
use crate::error::{MalformedKind, ResourceKind, SoyoError};
use crate::format::{
    validate_array_entries, validate_segment_storage, validate_segments, validate_string_table,
};
use crate::metadata::SoyoMetadata;
use crate::reader::{SoyoReadAt, SoyoReadError, SoyoReadLimits};
use crate::registry::{ArtifactKind, MAX_FILE_SIZE, TableType};
use crate::source::{checked_mul_u64, read_array, read_bytes, u32_at, verify_hash};
use crate::wire;

pub fn read_soyo<R: SoyoReadAt>(
    source: &R,
    limits: SoyoReadLimits,
) -> Result<SoyoMetadata, SoyoReadError<R::Error>> {
    let actual_size = source.len();
    if actual_size > MAX_FILE_SIZE {
        return Err(SoyoError::ResourceExhausted(ResourceKind::FileSize).into());
    }
    if actual_size < wire::HEADER_SIZE as u64 {
        return Err(SoyoError::Malformed(MalformedKind::Range).into());
    }

    let header_bytes = read_array::<R, { wire::HEADER_SIZE }>(source, 0, actual_size)?;
    let header = decode_header(&header_bytes, actual_size, limits)?;
    let directory_size = checked_mul_u64(
        u64::from(u32_at(&header_bytes, wire::header::TABLE_COUNT)),
        wire::DIRECTORY_ENTRY_SIZE as u64,
    )?;
    let directory_bytes = read_bytes(
        source,
        wire::HEADER_SIZE as u64,
        directory_size,
        actual_size,
        limits,
    )?;
    let directory = decode_directory(&directory_bytes, &header, limits)?;
    validate_directory_shape(&directory, header.artifact_kind, limits)?;
    validate_canonical_tables(source, &directory, actual_size)?;

    let strings = read_standard_table(source, &directory, TableType::String, actual_size, limits)?;
    validate_string_table(&strings)?;
    let segment_bytes = read_standard_table(
        source,
        &directory,
        TableType::ImageSegment,
        actual_size,
        limits,
    )?;
    let import_bytes = read_optional_standard_table(
        source,
        &directory,
        TableType::AbiImport,
        actual_size,
        limits,
    )?;
    let relocation_bytes = read_optional_standard_table(
        source,
        &directory,
        TableType::Relocation,
        actual_size,
        limits,
    )?;

    let segments = decode_segments(&segment_bytes)?;
    let imports = decode_imports(&import_bytes, &strings)?;
    let relocations = decode_relocations(&relocation_bytes, &segments)?;
    let (capabilities, runtime, component) = match header.artifact_kind {
        ArtifactKind::Executable => {
            let capability_bytes = read_standard_table(
                source,
                &directory,
                TableType::CapabilityRequirement,
                actual_size,
                limits,
            )?;
            let runtime_bytes = read_standard_table(
                source,
                &directory,
                TableType::RuntimeInfo,
                actual_size,
                limits,
            )?;
            let capabilities = decode_capabilities(&capability_bytes, &strings)?;
            let runtime = decode_runtime(
                &runtime_bytes,
                &segments,
                header.required_features,
                header.optional_features,
            )?;
            (capabilities, Some(runtime), None)
        }
        ArtifactKind::SharedComponent => {
            let capability_bytes = read_optional_standard_table(
                source,
                &directory,
                TableType::CapabilityRequirement,
                actual_size,
                limits,
            )?;
            let info = read_standard_table(
                source,
                &directory,
                TableType::ComponentInfo,
                actual_size,
                limits,
            )?;
            let dependencies = read_optional_standard_table(
                source,
                &directory,
                TableType::ComponentDependency,
                actual_size,
                limits,
            )?;
            let symbol_imports = read_optional_standard_table(
                source,
                &directory,
                TableType::SymbolImport,
                actual_size,
                limits,
            )?;
            let symbol_exports = read_standard_table(
                source,
                &directory,
                TableType::SymbolExport,
                actual_size,
                limits,
            )?;
            let dynamic_relocations = read_optional_standard_table(
                source,
                &directory,
                TableType::DynamicRelocation,
                actual_size,
                limits,
            )?;
            let signature = read_optional_standard_table(
                source,
                &directory,
                TableType::Signature,
                actual_size,
                limits,
            )?;
            let component = decode_component(
                &info,
                &dependencies,
                &symbol_imports,
                &symbol_exports,
                &dynamic_relocations,
                &signature,
            )?;
            let capabilities = decode_capabilities(&capability_bytes, &strings)?;
            validate_component_metadata(
                header.target_arch,
                header.image_virtual_size,
                &segments,
                &imports,
                &strings,
                &component,
            )?;
            (capabilities, None, Some(component))
        }
    };

    validate_segments(&segments, &header)?;
    validate_segment_storage(source, &directory, &segments, actual_size)?;
    if let Some(runtime) = &runtime {
        validate_array_entries(source, runtime, &segments, header.target_arch, actual_size)?;
    }
    verify_hash(source, &header, &directory, actual_size)?;

    Ok(SoyoMetadata {
        header,
        directory,
        strings,
        segments,
        imports,
        capabilities,
        relocations,
        runtime,
        component,
    })
}

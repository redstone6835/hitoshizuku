//! 已链接映像的 canonical SOYO 编码与共享解析器自检。

use std::fmt;

use ed25519_dalek::{Signer, SigningKey};
use native_abi::{ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, operation, requirement};
use sha2::{Digest, Sha256};
use soyo::registry::{
    ArtifactKind, CapabilityFlags, DirectoryFlags, FORMAT_VERSION, FeatureFlags, HashAlgorithm,
    ImportFlags, MAX_FILE_SIZE, MAX_SEGMENTS, MAX_STRING_BYTES, PAGE_SIZE, RelocationKind,
    RuntimeFlags, SOYO_MAGIC, SegmentKind, SegmentPermissions, TableType, DynamicRelocationKind,
};
use soyo::wire;
use soyo::{
    SliceSoyoReader, SoyoReadLimits, SoyoTargetPolicy, read_soyo, validate_component_soyo,
    validate_soyo,
};

use crate::contract::{ComponentContract, ContractCapability, ProgramContract};
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
    encode_header(&mut output, image, contract, &tables, file_size)?;
    encode_directory(&mut output, &tables);
    for table in &tables {
        write_bytes(&mut output, table.file_offset, &table.bytes)?;
    }
    for (segment, file_offset) in image.segments().iter().zip(segment_file_offsets) {
        if file_offset != 0 {
            write_bytes(&mut output, file_offset, segment.payload())?;
        }
    }
    rehash(&mut output, None);
    self_validate(&output, image.target_arch())?;
    Ok(output)
}

/// 把目标架构 relocation 已完成的映像编码为 shared component SOYO。
pub fn encode_component_soyo(
    image: &LinkedImage,
    contract: &ComponentContract,
) -> Result<Vec<u8>, EncodeError> {
    encode_component_soyo_with_key(image, contract, None)
}

/// 编码并以 Ed25519 私钥签署 shared component。
pub fn encode_signed_component_soyo(
    image: &LinkedImage,
    contract: &ComponentContract,
    signing_key: [u8; 32],
) -> Result<Vec<u8>, EncodeError> {
    let signing_key = SigningKey::from_bytes(&signing_key);
    encode_component_soyo_with_key(image, contract, Some(&signing_key))
}

fn encode_component_soyo_with_key(
    image: &LinkedImage,
    contract: &ComponentContract,
    signing_key: Option<&SigningKey>,
) -> Result<Vec<u8>, EncodeError> {
    validate_linked_image(image)?;
    let (
        strings,
        import_names,
        capability_names,
        dependency_names,
        symbol_import_names,
        symbol_export_names,
    ) = build_component_strings(contract)?;
    let mut tables = build_component_tables(
        image,
        contract,
        strings,
        &import_names,
        &capability_names,
        &dependency_names,
        &symbol_import_names,
        &symbol_export_names,
        signing_key,
    )?;
    layout_tables(&mut tables)?;
    let metadata_end = tables
        .last()
        .and_then(|table| table.file_offset.checked_add(table.bytes.len() as u64))
        .ok_or_else(|| invalid_image("缺少 SOYO component metadata table"))?;
    let (segment_file_offsets, file_size) = layout_segment_payloads(image, metadata_end)?;
    encode_segment_table(image, &segment_file_offsets, &mut tables)?;
    if file_size > MAX_FILE_SIZE {
        return Err(output_too_large("SOYO component 输出超过文件大小上限"));
    }
    let mut output = vec![
        0;
        usize::try_from(file_size)
            .map_err(|_| output_too_large("SOYO component 输出超过宿主 usize 范围"))?
    ];
    encode_component_header(&mut output, image, &tables, file_size)?;
    encode_directory(&mut output, &tables);
    for table in &tables {
        write_bytes(&mut output, table.file_offset, &table.bytes)?;
    }
    for (segment, file_offset) in image.segments().iter().zip(segment_file_offsets) {
        if file_offset != 0 {
            write_bytes(&mut output, file_offset, segment.payload())?;
        }
    }
    let signature_offset = tables
        .iter()
        .find(|table| table.table_type == TableType::Signature)
        .map(|table| table.file_offset);
    rehash(&mut output, signature_offset);
    if let (Some(signing_key), Some(signature_offset)) = (signing_key, signature_offset) {
        let mut content_hash = [0; 32];
        content_hash.copy_from_slice(
            &output[wire::header::CONTENT_HASH..wire::header::CONTENT_HASH + 32],
        );
        let signature = signing_key.sign(&soyo::signature_message(content_hash));
        write_bytes(
            &mut output,
            signature_offset + wire::signature::SIGNATURE as u64,
            &signature.to_bytes(),
        )?;
    }
    self_validate_component(&output)?;
    Ok(output)
}

type ComponentStringOffsets = (
    Vec<u8>,
    Vec<u32>,
    Vec<u32>,
    Vec<u32>,
    Vec<u32>,
    Vec<u32>,
);

fn build_component_strings(
    contract: &ComponentContract,
) -> Result<ComponentStringOffsets, EncodeError> {
    let mut strings = vec![0];
    let import_names = contract
        .imports()
        .iter()
        .map(|import| {
            let name = operation(import.operation)
                .expect("已归一化 component operation 必须位于 registry")
                .name;
            push_string(&mut strings, name)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let capability_names = contract
        .capabilities()
        .iter()
        .map(|capability| {
            let name = requirement(capability.requirement)
                .expect("已归一化 component requirement 必须位于 registry")
                .name;
            push_string(&mut strings, name)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let dependency_names = contract
        .dependencies()
        .iter()
        .map(|dependency| push_string(&mut strings, &dependency.name))
        .collect::<Result<Vec<_>, _>>()?;
    let symbol_import_names = contract
        .symbol_imports()
        .iter()
        .map(|import| push_string(&mut strings, &import.name))
        .collect::<Result<Vec<_>, _>>()?;
    let symbol_export_names = contract
        .symbol_exports()
        .iter()
        .map(|export| push_string(&mut strings, &export.name))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        strings,
        import_names,
        capability_names,
        dependency_names,
        symbol_import_names,
        symbol_export_names,
    ))
}

fn build_component_tables(
    image: &LinkedImage,
    contract: &ComponentContract,
    strings: Vec<u8>,
    import_names: &[u32],
    capability_names: &[u32],
    dependency_names: &[u32],
    symbol_import_names: &[u32],
    symbol_export_names: &[u32],
    signing_key: Option<&SigningKey>,
) -> Result<Vec<EncodedTable>, EncodeError> {
    let dynamic_relocations = encode_component_dynamic_relocations(image, contract)?;
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
            entry_count: image.segments().len() as u32,
            alignment: 8,
            file_offset: 0,
            bytes: vec![0; image.segments().len() * wire::IMAGE_SEGMENT_SIZE],
        },
    ];
    if !contract.imports().is_empty() {
        tables.push(EncodedTable {
            table_type: TableType::AbiImport,
            entry_size: wire::ABI_IMPORT_SIZE as u32,
            entry_count: contract.imports().len() as u32,
            alignment: 8,
            file_offset: 0,
            bytes: encode_component_imports(contract, import_names),
        });
    }
    if !contract.capabilities().is_empty() {
        tables.push(EncodedTable {
            table_type: TableType::CapabilityRequirement,
            entry_size: wire::CAPABILITY_REQUIREMENT_SIZE as u32,
            entry_count: contract.capabilities().len() as u32,
            alignment: 8,
            file_offset: 0,
            bytes: encode_capability_records(contract.capabilities(), capability_names),
        });
    }
    if !image.runtime_relocations().is_empty() {
        tables.push(EncodedTable {
            table_type: TableType::Relocation,
            entry_size: wire::RELOCATION_SIZE as u32,
            entry_count: image.runtime_relocations().len() as u32,
            alignment: 8,
            file_offset: 0,
            bytes: encode_relocations(image),
        });
    }
    tables.push(EncodedTable {
        table_type: TableType::ComponentInfo,
        entry_size: wire::COMPONENT_INFO_SIZE as u32,
        entry_count: 1,
        alignment: 8,
        file_offset: 0,
        bytes: encode_component_info(image, contract)?,
    });
    if !contract.dependencies().is_empty() {
        tables.push(EncodedTable {
            table_type: TableType::ComponentDependency,
            entry_size: wire::COMPONENT_DEPENDENCY_SIZE as u32,
            entry_count: contract.dependencies().len() as u32,
            alignment: 8,
            file_offset: 0,
            bytes: encode_component_dependencies(contract, dependency_names),
        });
    }
    if !contract.symbol_imports().is_empty() {
        tables.push(EncodedTable {
            table_type: TableType::SymbolImport,
            entry_size: wire::SYMBOL_IMPORT_SIZE as u32,
            entry_count: contract.symbol_imports().len() as u32,
            alignment: 8,
            file_offset: 0,
            bytes: encode_component_symbol_imports(contract, symbol_import_names),
        });
    }
    tables.push(EncodedTable {
        table_type: TableType::SymbolExport,
        entry_size: wire::SYMBOL_EXPORT_SIZE as u32,
        entry_count: contract.symbol_exports().len() as u32,
        alignment: 8,
        file_offset: 0,
        bytes: encode_component_symbol_exports(image, contract, symbol_export_names)?,
    });
    if !dynamic_relocations.is_empty() {
        tables.push(EncodedTable {
            table_type: TableType::DynamicRelocation,
            entry_size: wire::DYNAMIC_RELOCATION_SIZE as u32,
            entry_count: (dynamic_relocations.len() / wire::DYNAMIC_RELOCATION_SIZE) as u32,
            alignment: 8,
            file_offset: 0,
            bytes: dynamic_relocations,
        });
    }
    if let Some(signing_key) = signing_key {
        let public_key = signing_key.verifying_key().to_bytes();
        let key_id: [u8; 32] = Sha256::digest(public_key).into();
        let mut bytes = vec![0; wire::SIGNATURE_SIZE];
        bytes[wire::signature::KEY_ID..wire::signature::KEY_ID + 32]
            .copy_from_slice(&key_id);
        tables.push(EncodedTable {
            table_type: TableType::Signature,
            entry_size: wire::SIGNATURE_SIZE as u32,
            entry_count: 1,
            alignment: 8,
            file_offset: 0,
            bytes,
        });
    }
    tables.sort_by_key(|table| table.table_type as u16);
    Ok(tables)
}

fn encode_component_imports(contract: &ComponentContract, names: &[u32]) -> Vec<u8> {
    let mut bytes = vec![0; contract.imports().len() * wire::ABI_IMPORT_SIZE];
    for (slot, (import, name)) in contract.imports().iter().zip(names).enumerate() {
        let offset = slot * wire::ABI_IMPORT_SIZE;
        let spec = operation(import.operation).expect("已归一化 component operation");
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
            *name,
        );
        bytes[offset + wire::abi_import::SIGNATURE_HASH
            ..offset + wire::abi_import::SIGNATURE_HASH + 32]
            .copy_from_slice(&spec.signature_hash);
    }
    bytes
}

fn encode_component_info(
    image: &LinkedImage,
    contract: &ComponentContract,
) -> Result<Vec<u8>, EncodeError> {
    let mut bytes = vec![0; wire::COMPONENT_INFO_SIZE];
    bytes[wire::component_info::COMPONENT_ID..wire::component_info::COMPONENT_ID + 16]
        .copy_from_slice(&contract.component_id());
    bytes[wire::component_info::ABI_ID..wire::component_info::ABI_ID + 16]
        .copy_from_slice(&contract.abi_id());
    if let Some(init) = contract.init() {
        put_u64(
            &mut bytes,
            wire::component_info::INIT_OFFSET,
            code_symbol_offset(image, init)?,
        );
    }
    if let Some(fini) = contract.fini() {
        put_u64(
            &mut bytes,
            wire::component_info::FINI_OFFSET,
            code_symbol_offset(image, fini)?,
        );
    }
    let interface_count = contract
        .symbol_exports()
        .iter()
        .map(|export| export.interface_id)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    put_u32(
        &mut bytes,
        wire::component_info::INTERFACE_COUNT,
        interface_count as u32,
    );
    put_u64(
        &mut bytes,
        wire::component_info::CALL_STATE_SIZE,
        PAGE_SIZE,
    );
    Ok(bytes)
}

fn encode_component_dependencies(contract: &ComponentContract, names: &[u32]) -> Vec<u8> {
    let mut bytes = vec![0; contract.dependencies().len() * wire::COMPONENT_DEPENDENCY_SIZE];
    for (index, (dependency, name)) in contract.dependencies().iter().zip(names).enumerate() {
        let offset = index * wire::COMPONENT_DEPENDENCY_SIZE;
        bytes[offset + wire::component_dependency::COMPONENT_ID
            ..offset + wire::component_dependency::COMPONENT_ID + 16]
            .copy_from_slice(&dependency.component_id);
        bytes[offset + wire::component_dependency::ABI_ID
            ..offset + wire::component_dependency::ABI_ID + 16]
            .copy_from_slice(&dependency.abi_id);
        if let Some(hash) = dependency.content_hash {
            bytes[offset + wire::component_dependency::CONTENT_HASH
                ..offset + wire::component_dependency::CONTENT_HASH + 32]
                .copy_from_slice(&hash);
            put_u32(&mut bytes, offset + wire::component_dependency::FLAGS, 1);
        }
        put_u32(
            &mut bytes,
            offset + wire::component_dependency::DIAGNOSTIC_NAME_OFFSET,
            *name,
        );
    }
    bytes
}

fn encode_component_symbol_imports(contract: &ComponentContract, names: &[u32]) -> Vec<u8> {
    let mut bytes = vec![0; contract.symbol_imports().len() * wire::SYMBOL_IMPORT_SIZE];
    for (index, (import, name)) in contract.symbol_imports().iter().zip(names).enumerate() {
        let offset = index * wire::SYMBOL_IMPORT_SIZE;
        put_u32(
            &mut bytes,
            offset + wire::symbol_import::DEPENDENCY_INDEX,
            import.dependency_index,
        );
        put_u32(&mut bytes, offset + wire::symbol_import::FLAGS, 1);
        bytes[offset + wire::symbol_import::INTERFACE_ID
            ..offset + wire::symbol_import::INTERFACE_ID + 16]
            .copy_from_slice(&import.interface_id);
        bytes[offset + wire::symbol_import::SYMBOL_ID
            ..offset + wire::symbol_import::SYMBOL_ID + 16]
            .copy_from_slice(&import.symbol_id);
        bytes[offset + wire::symbol_import::SIGNATURE_HASH
            ..offset + wire::symbol_import::SIGNATURE_HASH + 32]
            .copy_from_slice(&import.signature_hash);
        put_u32(
            &mut bytes,
            offset + wire::symbol_import::DIAGNOSTIC_NAME_OFFSET,
            *name,
        );
    }
    bytes
}

fn encode_component_symbol_exports(
    image: &LinkedImage,
    contract: &ComponentContract,
    names: &[u32],
) -> Result<Vec<u8>, EncodeError> {
    let mut bytes = vec![0; contract.symbol_exports().len() * wire::SYMBOL_EXPORT_SIZE];
    for (index, (export, name)) in contract.symbol_exports().iter().zip(names).enumerate() {
        let offset = index * wire::SYMBOL_EXPORT_SIZE;
        bytes[offset + wire::symbol_export::INTERFACE_ID
            ..offset + wire::symbol_export::INTERFACE_ID + 16]
            .copy_from_slice(&export.interface_id);
        bytes[offset + wire::symbol_export::SYMBOL_ID
            ..offset + wire::symbol_export::SYMBOL_ID + 16]
            .copy_from_slice(&export.symbol_id);
        bytes[offset + wire::symbol_export::SIGNATURE_HASH
            ..offset + wire::symbol_export::SIGNATURE_HASH + 32]
            .copy_from_slice(&export.signature_hash);
        put_u64(
            &mut bytes,
            offset + wire::symbol_export::ENTRY_OFFSET,
            code_symbol_offset(image, &export.symbol)?,
        );
        put_u32(
            &mut bytes,
            offset + wire::symbol_export::DIAGNOSTIC_NAME_OFFSET,
            *name,
        );
    }
    Ok(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ComponentRelocation {
    target_segment_index: u32,
    target_offset: u64,
    kind: DynamicRelocationKind,
    source_index: u32,
}

fn encode_component_dynamic_relocations(
    image: &LinkedImage,
    contract: &ComponentContract,
) -> Result<Vec<u8>, EncodeError> {
    let tls_segment_index = image
        .segments()
        .iter()
        .position(|segment| segment.kind() == SegmentKind::TlsTemplate);
    if tls_segment_index.is_some() != contract.tls_offset_symbol().is_some() {
        return Err(invalid_image(
            "component TLS_TEMPLATE 与 tls_offset_symbol 必须同时存在",
        ));
    }
    let mut relocations = Vec::with_capacity(
        contract.imports().len()
            + contract.symbol_imports().len()
            + usize::from(tls_segment_index.is_some()),
    );
    for (source_index, import) in contract.imports().iter().enumerate() {
        let (target_segment_index, target_offset) =
            writable_symbol_target(image, &import.slot_symbol, 8)?;
        relocations.push(ComponentRelocation {
            target_segment_index,
            target_offset,
            kind: DynamicRelocationKind::AbiSlot64,
            source_index: source_index as u32,
        });
    }
    for (source_index, import) in contract.symbol_imports().iter().enumerate() {
        let (target_segment_index, target_offset) =
            writable_symbol_target(image, &import.binding_symbol, 32)?;
        relocations.push(ComponentRelocation {
            target_segment_index,
            target_offset,
            kind: DynamicRelocationKind::InterfaceGate,
            source_index: source_index as u32,
        });
    }
    if let (Some(source_index), Some(symbol)) =
        (tls_segment_index, contract.tls_offset_symbol())
    {
        let (target_segment_index, target_offset) = writable_symbol_target(image, symbol, 8)?;
        relocations.push(ComponentRelocation {
            target_segment_index,
            target_offset,
            kind: DynamicRelocationKind::TlsOffset64,
            source_index: source_index as u32,
        });
    }
    relocations.sort_by_key(|relocation| (relocation.target_segment_index, relocation.target_offset));
    if relocations.windows(2).any(|pair| {
        (pair[0].target_segment_index, pair[0].target_offset)
            == (pair[1].target_segment_index, pair[1].target_offset)
    }) {
        return Err(invalid_image("component dynamic relocation target 重复"));
    }
    let mut bytes = vec![0; relocations.len() * wire::DYNAMIC_RELOCATION_SIZE];
    for (index, relocation) in relocations.iter().enumerate() {
        let offset = index * wire::DYNAMIC_RELOCATION_SIZE;
        put_u16(
            &mut bytes,
            offset + wire::dynamic_relocation::KIND,
            relocation.kind as u16,
        );
        put_u32(
            &mut bytes,
            offset + wire::dynamic_relocation::TARGET_SEGMENT_INDEX,
            relocation.target_segment_index,
        );
        put_u64(
            &mut bytes,
            offset + wire::dynamic_relocation::TARGET_OFFSET,
            relocation.target_offset,
        );
        put_u32(
            &mut bytes,
            offset + wire::dynamic_relocation::SOURCE_INDEX,
            relocation.source_index,
        );
    }
    Ok(bytes)
}

fn code_symbol_offset(image: &LinkedImage, name: &str) -> Result<u64, EncodeError> {
    let symbol = image
        .symbol(name)
        .ok_or_else(|| invalid_image(format!("找不到 component CODE symbol {name}")))?;
    let segment_index = symbol
        .segment_index()
        .ok_or_else(|| invalid_image(format!("component symbol {name} 没有 segment")))?;
    if image.segments()[segment_index].kind() != SegmentKind::Code {
        return Err(invalid_image(format!("component symbol {name} 不在 CODE")));
    }
    match symbol.value() {
        crate::link::SymbolValue::Image(value) => Ok(value),
        _ => Err(invalid_image(format!("component symbol {name} 不是映像地址"))),
    }
}

fn writable_symbol_target(
    image: &LinkedImage,
    name: &str,
    minimum_size: u64,
) -> Result<(u32, u64), EncodeError> {
    let symbol = image
        .symbol(name)
        .ok_or_else(|| invalid_image(format!("找不到 component binding symbol {name}")))?;
    let segment_index = symbol
        .segment_index()
        .ok_or_else(|| invalid_image(format!("binding symbol {name} 没有 segment")))?;
    let segment = &image.segments()[segment_index];
    if !matches!(segment.kind(), SegmentKind::Data | SegmentKind::Bss)
        || symbol.size() < minimum_size
    {
        return Err(invalid_image(format!(
            "binding symbol {name} 必须是至少 {minimum_size} 字节的 DATA/BSS 对象"
        )));
    }
    let value = match symbol.value() {
        crate::link::SymbolValue::Image(value) => value,
        _ => return Err(invalid_image(format!("binding symbol {name} 不是映像地址"))),
    };
    let target_offset = value
        .checked_sub(segment.virtual_offset())
        .ok_or_else(|| invalid_image(format!("binding symbol {name} 位于 segment 之外")))?;
    if target_offset % 8 != 0
        || target_offset
            .checked_add(minimum_size)
            .is_none_or(|end| end > segment.memory_size())
    {
        return Err(invalid_image(format!("binding symbol {name} 范围或对齐无效")));
    }
    Ok((segment_index as u32, target_offset))
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
            bytes: encode_capability_records(contract.capabilities(), capability_name_offsets),
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

fn encode_capability_records(
    capabilities: &[ContractCapability],
    name_offsets: &[u32],
) -> Vec<u8> {
    let mut bytes = vec![0; capabilities.len() * wire::CAPABILITY_REQUIREMENT_SIZE];
    for (index, (capability, name_offset)) in
        capabilities.iter().zip(name_offsets).enumerate()
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
    contract: &ProgramContract,
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
    if contract
        .imports()
        .iter()
        .any(|import| import.operation == native_abi::OperationId::ComponentLoad)
    {
        let existing = u64::from_le_bytes(
            output[wire::header::REQUIRED_FEATURES..wire::header::REQUIRED_FEATURES + 8]
                .try_into()
                .expect("Header required feature 字段大小固定"),
        );
        put_u64(
            output,
            wire::header::REQUIRED_FEATURES,
            existing | FeatureFlags::DYNAMIC_COMPONENTS.bits(),
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

fn encode_component_header(
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
        ArtifactKind::SharedComponent as u16,
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
    put_u64(output, wire::header::ENTRY_OFFSET, 0);
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

fn rehash(output: &mut [u8], signature_offset: Option<u64>) {
    output[wire::header::BUILD_ID..wire::header::CONTENT_HASH + 32].fill(0);
    if let Some(signature_offset) = signature_offset {
        let start = signature_offset as usize + wire::signature::SIGNATURE;
        output[start..start + 64].fill(0);
    }
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

fn self_validate_component(output: &[u8]) -> Result<(), EncodeError> {
    let metadata =
        read_soyo(&SliceSoyoReader::new(output), SoyoReadLimits::portable()).map_err(|error| {
            EncodeError::new(
                EncodeErrorKind::SelfValidation,
                format!("SOYO component encoder 自检解析失败: {error:?}"),
            )
        })?;
    validate_component_soyo(
        &metadata,
        SoyoTargetPolicy::for_kernel(metadata.header.target_arch),
    )
    .map_err(|error| {
        EncodeError::new(
            EncodeErrorKind::SelfValidation,
            format!("SOYO component encoder 自检绑定失败: {error:?}"),
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

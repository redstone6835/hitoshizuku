//! EKI 原生镜像元数据解析。
//!
//! EKI 是 ELM 原生镜像格式。本模块只把 EKI v1 元数据展开为 EBI 协议对象；
//! 代码段映射、重定位和入口调用仍属于后续原生执行器。

use alloc::string::String;
use alloc::vec::Vec;
use core::str;

use crate::ebi::{
    ELM_EBI_ABI_VERSION, ELM_EBI_MAX_DEPENDENCIES, ELM_EBI_MAX_EXPORTS,
    ELM_EBI_MAX_EXTENSION_POINTS, ELM_EBI_MAX_EXTENSIONS, ELM_EBI_MAX_IMPORTS,
    ELM_EBI_MAX_PROVIDER_PORTS, ELM_EBI_MAX_SEGMENTS, ELM_EBI_NAME_LEN, ELM_EBI_SYMBOL_NAME_LEN,
    ElmEbiArch, ElmEbiDependencyDecl, ElmEbiEntry, ElmEbiExportDecl, ElmEbiExtensionDecl,
    ElmEbiExtensionPointDecl, ElmEbiImportDecl, ElmEbiLifecycleHookDecl, ElmEbiLifecycleHookKind,
    ElmEbiLifecycleHooks, ElmEbiLoadStatus, ElmEbiMenuDecl, ElmEbiProviderPortDecl,
    ElmEbiRustHookSignature, ElmEbiSegment, ElmEbiSegmentKind, ElmEbiTarget, ElmEbiUnit,
};
use crate::manifest::{ElmKind, ElmManifest, ElmName, ElmVersion};
use crate::menu::{
    ELM_MENU_DESCRIPTION_LEN, ELM_MENU_LABEL_LEN, ELM_MENU_ROUTE_LEN, ElmMenuItemKind,
};
use crate::mgr::{ELM_MGR_RELATION_POINT_LEN, ELM_NEXUS_CONTRACT_LEN};
use crate::nexus::{FlowDirection, FlowMode};
use crate::ports::ElmPortAccessPolicy;

pub const ELM_EKI_MAGIC: [u8; 8] = *b"ELM_EKI\0";
pub const ELM_EKI_FORMAT_VERSION: u16 = 1;
pub const ELM_EKI_HEADER_SIZE: usize = 64;
pub const ELM_EKI_BLOCK_DESC_SIZE: usize = 48;
pub const ELM_EKI_MAX_BLOCKS: usize = 64;
pub const ELM_EKI_MANIFEST_NAME_LEN: usize = 128;
pub const ELM_EKI_MANIFEST_VERSION_LEN: usize = 64;
pub const ELM_EKI_ENTRY_SYMBOL_LEN: usize = 128;
pub const ELM_EKI_BLOCK_FLAG_REQUIRED: u32 = 1 << 0;

const EKI_MANIFEST_BLOCK_SIZE: usize =
    16 + ELM_EKI_MANIFEST_NAME_LEN + ELM_EKI_MANIFEST_VERSION_LEN;
const EKI_MENU_BLOCK_SIZE: usize =
    16 + ELM_MENU_LABEL_LEN + ELM_MENU_DESCRIPTION_LEN + ELM_MENU_ROUTE_LEN;
const EKI_ENTRY_BLOCK_SIZE: usize = 8 + ELM_EKI_ENTRY_SYMBOL_LEN;
const EKI_TABLE_HEADER_SIZE: usize = 8;
const EKI_SEGMENT_RECORD_SIZE: usize = 32;
const EKI_DEPENDENCY_RECORD_SIZE: usize = 8 + ELM_EBI_NAME_LEN + ELM_NEXUS_CONTRACT_LEN;
const EKI_EXTENSION_POINT_RECORD_SIZE: usize =
    8 + ELM_MGR_RELATION_POINT_LEN + ELM_NEXUS_CONTRACT_LEN;
const EKI_EXTENSION_RECORD_SIZE: usize =
    8 + ELM_EBI_NAME_LEN + ELM_MGR_RELATION_POINT_LEN + ELM_NEXUS_CONTRACT_LEN;
const EKI_PROVIDER_PORT_RECORD_SIZE: usize = 24 + ELM_NEXUS_CONTRACT_LEN;
const EKI_SYMBOL_RECORD_SIZE: usize = 16 + ELM_EBI_SYMBOL_NAME_LEN + ELM_NEXUS_CONTRACT_LEN;
const EKI_LIFECYCLE_HOOK_RECORD_SIZE: usize = 20 + ELM_EBI_SYMBOL_NAME_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmEkiBlockKind {
    Manifest = 1,
    Menu = 2,
    Entry = 3,
    Segments = 4,
    Code = 5,
    ReadOnlyData = 6,
    Data = 7,
    Bss = 8,
    Relocation = 9,
    Imports = 10,
    Exports = 11,
    Notes = 12,
    Signature = 13,
    Dependencies = 14,
    ExtensionPoints = 15,
    Extensions = 16,
    ProviderPorts = 17,
    LifecycleHooks = 18,
}

impl ElmEkiBlockKind {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Manifest),
            2 => Some(Self::Menu),
            3 => Some(Self::Entry),
            4 => Some(Self::Segments),
            5 => Some(Self::Code),
            6 => Some(Self::ReadOnlyData),
            7 => Some(Self::Data),
            8 => Some(Self::Bss),
            9 => Some(Self::Relocation),
            10 => Some(Self::Imports),
            11 => Some(Self::Exports),
            12 => Some(Self::Notes),
            13 => Some(Self::Signature),
            14 => Some(Self::Dependencies),
            15 => Some(Self::ExtensionPoints),
            16 => Some(Self::Extensions),
            17 => Some(Self::ProviderPorts),
            18 => Some(Self::LifecycleHooks),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmEkiHeader {
    pub magic: [u8; 8],
    pub format_version: u16,
    pub ebi_abi_version: u16,
    pub header_size: u32,
    pub file_size: u64,
    pub block_table_offset: u64,
    pub image_hash_offset: u64,
    pub arch: u32,
    pub min_core_version: u16,
    pub flags: u16,
    pub block_count: u32,
    pub image_hash_size: u32,
    pub reserved: [u8; 8],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmEkiBlockDesc {
    pub kind: u32,
    pub flags: u32,
    pub offset: u64,
    pub file_size: u64,
    pub mem_size: u64,
    pub align: u64,
    pub checksum: u32,
    pub reserved: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EkiSegmentDecl {
    kind: ElmEbiSegmentKind,
    flags: u32,
    file_size: u64,
    mem_size: u64,
    align: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EkiPayloadSegment {
    kind: ElmEbiSegmentKind,
    file_size: u64,
    mem_size: u64,
    align: u64,
    source_index: u32,
    source_offset: u64,
    content_hash: u64,
}

pub fn parse_eki_ebi_unit(bytes: &[u8]) -> Result<ElmEbiUnit, ElmEbiLoadStatus> {
    let header = parse_header(bytes)?;
    validate_header(bytes, &header)?;

    let mut manifest = None;
    let mut menu = None;
    let mut entry = None;
    let mut segment_decls = None;
    let mut payload_segments = Vec::new();
    let mut dependencies = None;
    let mut extension_points = None;
    let mut extensions = None;
    let mut provider_ports = None;
    let mut imports = None;
    let mut exports = None;
    let mut lifecycle_hooks = None;

    for index in 0..header.block_count as usize {
        let desc_offset = header.block_table_offset as usize + index * ELM_EKI_BLOCK_DESC_SIZE;
        let desc = parse_block_desc(bytes, desc_offset)?;
        validate_block_desc(bytes, &desc)?;

        let Some(kind) = ElmEkiBlockKind::from_raw(desc.kind) else {
            if desc.flags & ELM_EKI_BLOCK_FLAG_REQUIRED != 0 {
                return Err(ElmEbiLoadStatus::InvalidUnit);
            }
            continue;
        };
        let payload = block_payload(bytes, &desc)?;
        match kind {
            ElmEkiBlockKind::Manifest => {
                if manifest.is_some() {
                    return Err(ElmEbiLoadStatus::InvalidManifest);
                }
                manifest = Some(parse_manifest(payload)?);
            }
            ElmEkiBlockKind::Menu => {
                if menu.is_some() {
                    return Err(ElmEbiLoadStatus::InvalidMenu);
                }
                menu = Some(parse_menu(payload)?);
            }
            ElmEkiBlockKind::Entry => {
                if entry.is_some() {
                    return Err(ElmEbiLoadStatus::InvalidSegment);
                }
                entry = Some(parse_entry(payload)?);
            }
            ElmEkiBlockKind::Segments => {
                if segment_decls.is_some() {
                    return Err(ElmEbiLoadStatus::InvalidSegment);
                }
                segment_decls = Some(parse_segments(payload)?);
            }
            ElmEkiBlockKind::Code
            | ElmEkiBlockKind::ReadOnlyData
            | ElmEkiBlockKind::Data
            | ElmEkiBlockKind::Bss
            | ElmEkiBlockKind::Relocation
            | ElmEkiBlockKind::Notes => {
                payload_segments.push(parse_payload_segment(kind, &desc, index as u32, payload)?);
            }
            ElmEkiBlockKind::Imports => {
                if imports.is_some() {
                    return Err(ElmEbiLoadStatus::InvalidManifest);
                }
                imports = Some(parse_imports(payload)?);
            }
            ElmEkiBlockKind::Exports => {
                if exports.is_some() {
                    return Err(ElmEbiLoadStatus::InvalidManifest);
                }
                exports = Some(parse_exports(payload)?);
            }
            ElmEkiBlockKind::LifecycleHooks => {
                if lifecycle_hooks.is_some() {
                    return Err(ElmEbiLoadStatus::InvalidManifest);
                }
                lifecycle_hooks = Some(parse_lifecycle_hooks(payload)?);
            }
            ElmEkiBlockKind::Dependencies => {
                if dependencies.is_some() {
                    return Err(ElmEbiLoadStatus::InvalidManifest);
                }
                dependencies = Some(parse_dependencies(payload)?);
            }
            ElmEkiBlockKind::ExtensionPoints => {
                if extension_points.is_some() {
                    return Err(ElmEbiLoadStatus::InvalidManifest);
                }
                extension_points = Some(parse_extension_points(payload)?);
            }
            ElmEkiBlockKind::Extensions => {
                if extensions.is_some() {
                    return Err(ElmEbiLoadStatus::InvalidManifest);
                }
                extensions = Some(parse_extensions(payload)?);
            }
            ElmEkiBlockKind::ProviderPorts => {
                if provider_ports.is_some() {
                    return Err(ElmEbiLoadStatus::InvalidManifest);
                }
                provider_ports = Some(parse_provider_ports(payload)?);
            }
            _ => {}
        }
    }

    let manifest = manifest.ok_or(ElmEbiLoadStatus::InvalidManifest)?;
    let arch = ElmEbiArch::from_raw(header.arch).ok_or(ElmEbiLoadStatus::InvalidTarget)?;
    let mut target = ElmEbiTarget::new(arch);
    target.min_core_version = header.min_core_version;

    let mut unit = ElmEbiUnit::new(manifest, target);
    if let Some(menu) = menu {
        unit = unit.with_menu(menu);
    }
    if let Some(entry) = entry {
        unit = unit.with_entry(entry);
    }
    for segment in resolve_eki_segments(segment_decls.as_deref(), &payload_segments)? {
        unit = unit.with_segment(segment);
    }
    if let Some(dependencies) = dependencies {
        for dependency in dependencies {
            unit = unit.with_dependency(dependency);
        }
    }
    if let Some(extension_points) = extension_points {
        for point in extension_points {
            unit = unit.with_extension_point(point);
        }
    }
    if let Some(extensions) = extensions {
        for extension in extensions {
            unit = unit.with_extension(extension);
        }
    }
    if let Some(provider_ports) = provider_ports {
        for provider in provider_ports {
            unit = unit.with_provider_port(provider);
        }
    }
    if let Some(imports) = imports {
        for import in imports {
            unit = unit.with_import(import);
        }
    }
    if let Some(exports) = exports {
        for export in exports {
            unit = unit.with_export(export);
        }
    }
    if let Some(hooks) = lifecycle_hooks {
        unit = unit.with_lifecycle_hooks(hooks);
    }
    unit.validate(ElmEbiArch::Any)?;
    Ok(unit)
}

fn parse_header(bytes: &[u8]) -> Result<ElmEkiHeader, ElmEbiLoadStatus> {
    if bytes.len() < ELM_EKI_HEADER_SIZE {
        return Err(ElmEbiLoadStatus::InvalidUnit);
    }
    let mut magic = [0u8; 8];
    magic.copy_from_slice(read_bytes(bytes, 0, 8)?);
    let mut reserved = [0u8; 8];
    reserved.copy_from_slice(read_bytes(bytes, 56, 8)?);
    Ok(ElmEkiHeader {
        magic,
        format_version: read_u16(bytes, 8)?,
        ebi_abi_version: read_u16(bytes, 10)?,
        header_size: read_u32(bytes, 12)?,
        file_size: read_u64(bytes, 16)?,
        block_table_offset: read_u64(bytes, 24)?,
        image_hash_offset: read_u64(bytes, 32)?,
        arch: read_u32(bytes, 40)?,
        min_core_version: read_u16(bytes, 44)?,
        flags: read_u16(bytes, 46)?,
        block_count: read_u32(bytes, 48)?,
        image_hash_size: read_u32(bytes, 52)?,
        reserved,
    })
}

fn validate_header(bytes: &[u8], header: &ElmEkiHeader) -> Result<(), ElmEbiLoadStatus> {
    if header.magic != ELM_EKI_MAGIC
        || header.format_version != ELM_EKI_FORMAT_VERSION
        || header.header_size as usize != ELM_EKI_HEADER_SIZE
        || header.file_size as usize != bytes.len()
        || header.block_count == 0
        || header.block_count as usize > ELM_EKI_MAX_BLOCKS
        || header.min_core_version == 0
        || header.flags != 0
        || header.reserved.iter().any(|byte| *byte != 0)
    {
        return Err(ElmEbiLoadStatus::InvalidUnit);
    }
    if header.ebi_abi_version != ELM_EBI_ABI_VERSION {
        return Err(ElmEbiLoadStatus::UnsupportedAbi);
    }
    if ElmEbiArch::from_raw(header.arch).is_none() {
        return Err(ElmEbiLoadStatus::InvalidTarget);
    }
    let table_size = (header.block_count as usize)
        .checked_mul(ELM_EKI_BLOCK_DESC_SIZE)
        .ok_or(ElmEbiLoadStatus::InvalidUnit)?;
    checked_range(bytes, header.block_table_offset as usize, table_size)?;
    if header.image_hash_size != 0 {
        checked_range(
            bytes,
            header.image_hash_offset as usize,
            header.image_hash_size as usize,
        )?;
    }
    Ok(())
}

fn parse_block_desc(bytes: &[u8], offset: usize) -> Result<ElmEkiBlockDesc, ElmEbiLoadStatus> {
    Ok(ElmEkiBlockDesc {
        kind: read_u32(bytes, offset)?,
        flags: read_u32(bytes, offset + 4)?,
        offset: read_u64(bytes, offset + 8)?,
        file_size: read_u64(bytes, offset + 16)?,
        mem_size: read_u64(bytes, offset + 24)?,
        align: read_u64(bytes, offset + 32)?,
        checksum: read_u32(bytes, offset + 40)?,
        reserved: read_u32(bytes, offset + 44)?,
    })
}

fn validate_block_desc(bytes: &[u8], desc: &ElmEkiBlockDesc) -> Result<(), ElmEbiLoadStatus> {
    if desc.reserved != 0 || desc.checksum != 0 {
        return Err(ElmEbiLoadStatus::InvalidUnit);
    }
    if desc.flags & !ELM_EKI_BLOCK_FLAG_REQUIRED != 0 {
        return Err(ElmEbiLoadStatus::InvalidUnit);
    }
    if desc.align != 0 && !desc.align.is_power_of_two() {
        return Err(ElmEbiLoadStatus::InvalidSegment);
    }
    checked_range(bytes, desc.offset as usize, desc.file_size as usize)?;
    Ok(())
}

fn block_payload<'a>(
    bytes: &'a [u8],
    desc: &ElmEkiBlockDesc,
) -> Result<&'a [u8], ElmEbiLoadStatus> {
    read_bytes(bytes, desc.offset as usize, desc.file_size as usize)
}

fn parse_manifest(payload: &[u8]) -> Result<ElmManifest, ElmEbiLoadStatus> {
    if payload.len() != EKI_MANIFEST_BLOCK_SIZE {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    let kind = ElmKind::from_raw(read_u32(payload, 0)?).ok_or(ElmEbiLoadStatus::InvalidManifest)?;
    let flags = read_u32(payload, 4)?;
    let name_len = read_u16(payload, 8)? as usize;
    let version_len = read_u16(payload, 10)? as usize;
    let reserved = read_u32(payload, 12)?;
    if flags != 0
        || reserved != 0
        || name_len > ELM_EKI_MANIFEST_NAME_LEN
        || version_len > ELM_EKI_MANIFEST_VERSION_LEN
    {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    let name_start = 16;
    let version_start = name_start + ELM_EKI_MANIFEST_NAME_LEN;
    let name = fixed_str(payload, name_start, name_len)?;
    let version = fixed_str(payload, version_start, version_len)?;
    Ok(ElmManifest::new(
        ElmName::new(name).map_err(|_| ElmEbiLoadStatus::InvalidManifest)?,
        ElmVersion::new(version).map_err(|_| ElmEbiLoadStatus::InvalidManifest)?,
        kind,
    ))
}

fn parse_menu(payload: &[u8]) -> Result<ElmEbiMenuDecl, ElmEbiLoadStatus> {
    if payload.len() != EKI_MENU_BLOCK_SIZE {
        return Err(ElmEbiLoadStatus::InvalidMenu);
    }
    let kind =
        ElmMenuItemKind::from_raw(read_u32(payload, 0)?).ok_or(ElmEbiLoadStatus::InvalidMenu)?;
    let flags = read_u32(payload, 4)?;
    let label_len = read_u16(payload, 8)? as usize;
    let description_len = read_u16(payload, 10)? as usize;
    let route_len = read_u16(payload, 12)? as usize;
    let reserved = read_u16(payload, 14)?;
    if reserved != 0
        || label_len > ELM_MENU_LABEL_LEN
        || description_len > ELM_MENU_DESCRIPTION_LEN
        || route_len > ELM_MENU_ROUTE_LEN
    {
        return Err(ElmEbiLoadStatus::InvalidMenu);
    }
    let label_start = 16;
    let description_start = label_start + ELM_MENU_LABEL_LEN;
    let route_start = description_start + ELM_MENU_DESCRIPTION_LEN;
    Ok(ElmEbiMenuDecl::new(
        kind,
        flags,
        fixed_str(payload, label_start, label_len)?,
        fixed_str(payload, description_start, description_len)?,
        fixed_str(payload, route_start, route_len)?,
    ))
}

fn parse_entry(payload: &[u8]) -> Result<ElmEbiEntry, ElmEbiLoadStatus> {
    if payload.len() != EKI_ENTRY_BLOCK_SIZE {
        return Err(ElmEbiLoadStatus::InvalidSegment);
    }
    let symbol_len = read_u16(payload, 0)? as usize;
    if read_u16(payload, 2)? != 0
        || read_u32(payload, 4)? != 0
        || symbol_len > ELM_EKI_ENTRY_SYMBOL_LEN
    {
        return Err(ElmEbiLoadStatus::InvalidSegment);
    }
    Ok(ElmEbiEntry::new(fixed_str(payload, 8, symbol_len)?))
}

fn parse_segments(payload: &[u8]) -> Result<Vec<EkiSegmentDecl>, ElmEbiLoadStatus> {
    let count = parse_table_count(payload, ELM_EBI_MAX_SEGMENTS, EKI_SEGMENT_RECORD_SIZE)?;
    let mut segments = Vec::new();
    for index in 0..count {
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_SEGMENT_RECORD_SIZE;
        let kind = ElmEbiSegmentKind::from_raw(read_u32(payload, offset)?)
            .ok_or(ElmEbiLoadStatus::InvalidSegment)?;
        let flags = read_u32(payload, offset + 4)?;
        let file_size = read_u64(payload, offset + 8)?;
        let mem_size = read_u64(payload, offset + 16)?;
        let align = read_u64(payload, offset + 24)?;
        segments.push(EkiSegmentDecl {
            kind,
            flags,
            file_size,
            mem_size,
            align,
        });
    }
    Ok(segments)
}

fn parse_payload_segment(
    kind: ElmEkiBlockKind,
    desc: &ElmEkiBlockDesc,
    source_index: u32,
    payload: &[u8],
) -> Result<EkiPayloadSegment, ElmEbiLoadStatus> {
    let kind = segment_kind_from_block(kind).ok_or(ElmEbiLoadStatus::InvalidSegment)?;
    if desc.file_size > desc.mem_size {
        return Err(ElmEbiLoadStatus::InvalidSegment);
    }
    match kind {
        ElmEbiSegmentKind::Bss => {
            if desc.file_size != 0 || !payload.is_empty() || desc.mem_size == 0 {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
        }
        ElmEbiSegmentKind::Data => {
            if desc.file_size == 0 || desc.mem_size == 0 {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
        }
        ElmEbiSegmentKind::Code
        | ElmEbiSegmentKind::ReadOnlyData
        | ElmEbiSegmentKind::Relocation
        | ElmEbiSegmentKind::Note => {
            if desc.file_size == 0 || desc.file_size != desc.mem_size {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
        }
    }
    Ok(EkiPayloadSegment {
        kind,
        file_size: desc.file_size,
        mem_size: desc.mem_size,
        align: desc.align,
        source_index,
        source_offset: desc.offset,
        content_hash: if matches!(kind, ElmEbiSegmentKind::Bss) {
            0
        } else {
            stable_payload_hash(payload)
        },
    })
}

fn resolve_eki_segments(
    decls: Option<&[EkiSegmentDecl]>,
    payloads: &[EkiPayloadSegment],
) -> Result<Vec<ElmEbiSegment>, ElmEbiLoadStatus> {
    let Some(decls) = decls else {
        return if payloads.is_empty() {
            Ok(Vec::new())
        } else {
            Err(ElmEbiLoadStatus::InvalidSegment)
        };
    };
    if decls.len() != payloads.len() {
        return Err(ElmEbiLoadStatus::InvalidSegment);
    }
    let mut segments = Vec::new();
    for (decl, payload) in decls.iter().zip(payloads.iter()) {
        if decl.kind != payload.kind
            || decl.file_size != payload.file_size
            || decl.mem_size != payload.mem_size
            || decl.align != payload.align
        {
            return Err(ElmEbiLoadStatus::InvalidSegment);
        }
        segments.push(ElmEbiSegment::from_payload(
            payload.kind,
            decl.flags,
            payload.file_size,
            payload.mem_size,
            payload.align,
            payload.source_index,
            payload.source_offset,
            payload.content_hash,
        ));
    }
    Ok(segments)
}

fn segment_kind_from_block(kind: ElmEkiBlockKind) -> Option<ElmEbiSegmentKind> {
    match kind {
        ElmEkiBlockKind::Code => Some(ElmEbiSegmentKind::Code),
        ElmEkiBlockKind::ReadOnlyData => Some(ElmEbiSegmentKind::ReadOnlyData),
        ElmEkiBlockKind::Data => Some(ElmEbiSegmentKind::Data),
        ElmEkiBlockKind::Bss => Some(ElmEbiSegmentKind::Bss),
        ElmEkiBlockKind::Relocation => Some(ElmEbiSegmentKind::Relocation),
        ElmEkiBlockKind::Notes => Some(ElmEbiSegmentKind::Note),
        _ => None,
    }
}

fn parse_dependencies(payload: &[u8]) -> Result<Vec<ElmEbiDependencyDecl>, ElmEbiLoadStatus> {
    let count = parse_table_count(
        payload,
        ELM_EBI_MAX_DEPENDENCIES,
        EKI_DEPENDENCY_RECORD_SIZE,
    )?;
    let mut dependencies = Vec::new();
    for index in 0..count {
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_DEPENDENCY_RECORD_SIZE;
        let name_len = read_u16(payload, offset)? as usize;
        let contract_len = read_u16(payload, offset + 2)? as usize;
        if read_u32(payload, offset + 4)? != 0 {
            return Err(ElmEbiLoadStatus::InvalidManifest);
        }
        let name_start = offset + 8;
        let contract_start = name_start + ELM_EBI_NAME_LEN;
        dependencies.push(ElmEbiDependencyDecl::new(
            fixed_str(payload, name_start, name_len)?,
            fixed_str(payload, contract_start, contract_len)?,
        )?);
    }
    Ok(dependencies)
}

fn parse_extension_points(
    payload: &[u8],
) -> Result<Vec<ElmEbiExtensionPointDecl>, ElmEbiLoadStatus> {
    let count = parse_table_count(
        payload,
        ELM_EBI_MAX_EXTENSION_POINTS,
        EKI_EXTENSION_POINT_RECORD_SIZE,
    )?;
    let mut points = Vec::new();
    for index in 0..count {
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_EXTENSION_POINT_RECORD_SIZE;
        let point_len = read_u16(payload, offset)? as usize;
        let contract_len = read_u16(payload, offset + 2)? as usize;
        if read_u32(payload, offset + 4)? != 0 {
            return Err(ElmEbiLoadStatus::InvalidManifest);
        }
        let point_start = offset + 8;
        let contract_start = point_start + ELM_MGR_RELATION_POINT_LEN;
        points.push(ElmEbiExtensionPointDecl::new(
            fixed_str(payload, point_start, point_len)?,
            fixed_str(payload, contract_start, contract_len)?,
        )?);
    }
    Ok(points)
}

fn parse_extensions(payload: &[u8]) -> Result<Vec<ElmEbiExtensionDecl>, ElmEbiLoadStatus> {
    let count = parse_table_count(payload, ELM_EBI_MAX_EXTENSIONS, EKI_EXTENSION_RECORD_SIZE)?;
    let mut extensions = Vec::new();
    for index in 0..count {
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_EXTENSION_RECORD_SIZE;
        let target_len = read_u16(payload, offset)? as usize;
        let point_len = read_u16(payload, offset + 2)? as usize;
        let contract_len = read_u16(payload, offset + 4)? as usize;
        if read_u16(payload, offset + 6)? != 0 {
            return Err(ElmEbiLoadStatus::InvalidManifest);
        }
        let target_start = offset + 8;
        let point_start = target_start + ELM_EBI_NAME_LEN;
        let contract_start = point_start + ELM_MGR_RELATION_POINT_LEN;
        extensions.push(ElmEbiExtensionDecl::new(
            fixed_str(payload, target_start, target_len)?,
            fixed_str(payload, point_start, point_len)?,
            fixed_str(payload, contract_start, contract_len)?,
        )?);
    }
    Ok(extensions)
}

fn parse_provider_ports(payload: &[u8]) -> Result<Vec<ElmEbiProviderPortDecl>, ElmEbiLoadStatus> {
    let count = parse_table_count(
        payload,
        ELM_EBI_MAX_PROVIDER_PORTS,
        EKI_PROVIDER_PORT_RECORD_SIZE,
    )?;
    let mut providers = Vec::new();
    for index in 0..count {
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_PROVIDER_PORT_RECORD_SIZE;
        let access = ElmPortAccessPolicy::from_raw(read_u32(payload, offset)?)
            .ok_or(ElmEbiLoadStatus::InvalidManifest)?;
        let direction = FlowDirection::from_raw(read_u32(payload, offset + 4)?)
            .ok_or(ElmEbiLoadStatus::InvalidManifest)?;
        let mode = FlowMode::from_raw(read_u32(payload, offset + 8)?)
            .ok_or(ElmEbiLoadStatus::InvalidManifest)?;
        let flags = read_u32(payload, offset + 12)?;
        let contract_len = read_u16(payload, offset + 16)? as usize;
        if read_u16(payload, offset + 18)? != 0 || read_u32(payload, offset + 20)? != 0 {
            return Err(ElmEbiLoadStatus::InvalidManifest);
        }
        let contract_start = offset + 24;
        providers.push(ElmEbiProviderPortDecl::new(
            fixed_str(payload, contract_start, contract_len)?,
            access,
            direction,
            mode,
            flags,
        )?);
    }
    Ok(providers)
}

fn parse_imports(payload: &[u8]) -> Result<Vec<ElmEbiImportDecl>, ElmEbiLoadStatus> {
    let count = parse_table_count(payload, ELM_EBI_MAX_IMPORTS, EKI_SYMBOL_RECORD_SIZE)?;
    let mut imports = Vec::new();
    for index in 0..count {
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_SYMBOL_RECORD_SIZE;
        let (name, contract, version, flags) = parse_symbol_record(payload, offset)?;
        imports.push(ElmEbiImportDecl::new(name, contract, version, flags)?);
    }
    Ok(imports)
}

fn parse_exports(payload: &[u8]) -> Result<Vec<ElmEbiExportDecl>, ElmEbiLoadStatus> {
    let count = parse_table_count(payload, ELM_EBI_MAX_EXPORTS, EKI_SYMBOL_RECORD_SIZE)?;
    let mut exports = Vec::new();
    for index in 0..count {
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_SYMBOL_RECORD_SIZE;
        let (name, contract, version, flags) = parse_symbol_record(payload, offset)?;
        exports.push(ElmEbiExportDecl::new(name, contract, version, flags)?);
    }
    Ok(exports)
}

fn parse_lifecycle_hooks(payload: &[u8]) -> Result<ElmEbiLifecycleHooks, ElmEbiLoadStatus> {
    let count = parse_table_count(payload, 2, EKI_LIFECYCLE_HOOK_RECORD_SIZE)?;
    if count != 2 {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    let mut initialize = None;
    let mut finalize = None;
    for index in 0..count {
        let offset = EKI_TABLE_HEADER_SIZE + index * EKI_LIFECYCLE_HOOK_RECORD_SIZE;
        let hook = parse_lifecycle_hook_record(payload, offset)?;
        match hook.kind {
            ElmEbiLifecycleHookKind::Initialize => {
                if initialize.replace(hook).is_some() {
                    return Err(ElmEbiLoadStatus::InvalidManifest);
                }
            }
            ElmEbiLifecycleHookKind::Finalize => {
                if finalize.replace(hook).is_some() {
                    return Err(ElmEbiLoadStatus::InvalidManifest);
                }
            }
        }
    }
    ElmEbiLifecycleHooks::new(
        initialize.ok_or(ElmEbiLoadStatus::InvalidManifest)?,
        finalize.ok_or(ElmEbiLoadStatus::InvalidManifest)?,
    )
}

fn parse_lifecycle_hook_record(
    payload: &[u8],
    offset: usize,
) -> Result<ElmEbiLifecycleHookDecl, ElmEbiLoadStatus> {
    let kind = ElmEbiLifecycleHookKind::from_raw(read_u32(payload, offset)?)
        .ok_or(ElmEbiLoadStatus::InvalidManifest)?;
    let flags = read_u32(payload, offset + 4)?;
    let rust_abi_version = read_u16(payload, offset + 8)?;
    let signature = ElmEbiRustHookSignature::from_raw(read_u16(payload, offset + 10)?)
        .ok_or(ElmEbiLoadStatus::InvalidManifest)?;
    let symbol_len = read_u16(payload, offset + 12)? as usize;
    if read_u16(payload, offset + 14)? != 0 || read_u32(payload, offset + 16)? != 0 {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    ElmEbiLifecycleHookDecl::new(
        kind,
        fixed_str(payload, offset + 20, symbol_len)?,
        rust_abi_version,
        signature,
        flags,
    )
}

fn parse_symbol_record(
    payload: &[u8],
    offset: usize,
) -> Result<(String, String, u32, u32), ElmEbiLoadStatus> {
    let version = read_u32(payload, offset)?;
    let flags = read_u32(payload, offset + 4)?;
    let name_len = read_u16(payload, offset + 8)? as usize;
    let contract_len = read_u16(payload, offset + 10)? as usize;
    if read_u32(payload, offset + 12)? != 0 {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    let name_start = offset + 16;
    let contract_start = name_start + ELM_EBI_SYMBOL_NAME_LEN;
    Ok((
        fixed_str(payload, name_start, name_len)?,
        fixed_str(payload, contract_start, contract_len)?,
        version,
        flags,
    ))
}

fn stable_payload_hash(payload: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in payload {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn parse_table_count(
    payload: &[u8],
    max_count: usize,
    record_size: usize,
) -> Result<usize, ElmEbiLoadStatus> {
    if payload.len() < EKI_TABLE_HEADER_SIZE {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    let count = read_u32(payload, 0)? as usize;
    let reserved = read_u32(payload, 4)?;
    if reserved != 0 || count > max_count {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    let expected = EKI_TABLE_HEADER_SIZE
        .checked_add(
            count
                .checked_mul(record_size)
                .ok_or(ElmEbiLoadStatus::InvalidManifest)?,
        )
        .ok_or(ElmEbiLoadStatus::InvalidManifest)?;
    if payload.len() != expected {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    Ok(count)
}

fn fixed_str(bytes: &[u8], offset: usize, len: usize) -> Result<String, ElmEbiLoadStatus> {
    let raw = read_bytes(bytes, offset, len)?;
    let value = str::from_utf8(raw).map_err(|_| ElmEbiLoadStatus::InvalidUnit)?;
    Ok(String::from(value))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ElmEbiLoadStatus> {
    Ok(u16::from_le_bytes(
        read_bytes(bytes, offset, 2)?
            .try_into()
            .map_err(|_| ElmEbiLoadStatus::InvalidUnit)?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ElmEbiLoadStatus> {
    Ok(u32::from_le_bytes(
        read_bytes(bytes, offset, 4)?
            .try_into()
            .map_err(|_| ElmEbiLoadStatus::InvalidUnit)?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ElmEbiLoadStatus> {
    Ok(u64::from_le_bytes(
        read_bytes(bytes, offset, 8)?
            .try_into()
            .map_err(|_| ElmEbiLoadStatus::InvalidUnit)?,
    ))
}

fn read_bytes(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8], ElmEbiLoadStatus> {
    checked_range(bytes, offset, len)
}

fn checked_range(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8], ElmEbiLoadStatus> {
    let end = offset
        .checked_add(len)
        .ok_or(ElmEbiLoadStatus::InvalidUnit)?;
    bytes.get(offset..end).ok_or(ElmEbiLoadStatus::InvalidUnit)
}

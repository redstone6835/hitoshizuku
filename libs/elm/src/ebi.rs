//! EBI 镜像固定格式和纯解析器。

use alloc::vec::Vec;
use core::str;

use crate::manifest::{ElmKind, ElmManifest, ElmName, ElmVersion};
use crate::menu::ElmMenuItemKind;

pub const ELM_EBI_MAGIC: u32 = 0x3149_4245;
pub const ELM_EBI_ABI_VERSION: u16 = 1;
pub const ELM_EBI_MAX_IMAGE_SIZE: usize = 64 * 1024;
pub const ELM_EBI_MAX_SECTIONS: usize = 16;

pub const ELM_EBI_MANIFEST_NAME_LEN: usize = 64;
pub const ELM_EBI_MANIFEST_VERSION_LEN: usize = 32;
pub const ELM_EBI_MANIFEST_LABEL_LEN: usize = 64;
pub const ELM_EBI_MANIFEST_DESCRIPTION_LEN: usize = 128;
pub const ELM_EBI_MANIFEST_ROUTE_LEN: usize = 64;

pub const ELM_EBI_MANIFEST_FLAG_MENU_ITEM: u32 = 1 << 0;
pub const ELM_EBI_MANIFEST_FLAG_NATIVE_ENTRY: u32 = 1 << 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmEbiArch {
    Any = 0,
    Riscv64 = 1,
    LoongArch64 = 2,
}

impl ElmEbiArch {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Any),
            1 => Some(Self::Riscv64),
            2 => Some(Self::LoongArch64),
            _ => None,
        }
    }

    pub const fn matches(self, expected: Self) -> bool {
        matches!(self, Self::Any) || matches!(expected, Self::Any) || self as u32 == expected as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmEbiSectionKind {
    Manifest = 1,
    Code = 2,
    Data = 3,
    Relocation = 4,
    Note = 5,
}

impl ElmEbiSectionKind {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Manifest),
            2 => Some(Self::Code),
            3 => Some(Self::Data),
            4 => Some(Self::Relocation),
            5 => Some(Self::Note),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ElmEbiLoadStatus {
    Ok = 0,
    InvalidImage = -1,
    UnsupportedAbi = -2,
    ImageTooLarge = -3,
    SectionOutOfBounds = -4,
    ArchMismatch = -5,
    InvalidManifest = -6,
    UnsupportedSection = -7,
    NativeCodeTodo = -4096,
    RuntimeRejected = -4097,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmEbiHeader {
    pub magic: u32,
    pub abi_version: u16,
    pub header_size: u16,
    pub image_size: u32,
    pub arch: u32,
    pub section_count: u16,
    pub section_entry_size: u16,
    pub section_table_offset: u32,
    pub manifest_offset: u32,
    pub manifest_size: u32,
    pub flags: u32,
}

pub const ELM_EBI_HEADER_SIZE: usize = core::mem::size_of::<ElmEbiHeader>();

impl ElmEbiHeader {
    pub const fn new(
        image_size: u32,
        arch: ElmEbiArch,
        section_count: u16,
        section_table_offset: u32,
        manifest_offset: u32,
        manifest_size: u32,
        flags: u32,
    ) -> Self {
        Self {
            magic: ELM_EBI_MAGIC,
            abi_version: ELM_EBI_ABI_VERSION,
            header_size: ELM_EBI_HEADER_SIZE as u16,
            image_size,
            arch: arch as u32,
            section_count,
            section_entry_size: ELM_EBI_SECTION_HEADER_SIZE as u16,
            section_table_offset,
            manifest_offset,
            manifest_size,
            flags,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmEbiSectionHeader {
    pub kind: u32,
    pub offset: u32,
    pub size: u32,
    pub flags: u32,
}

pub const ELM_EBI_SECTION_HEADER_SIZE: usize = core::mem::size_of::<ElmEbiSectionHeader>();

impl ElmEbiSectionHeader {
    pub const fn new(kind: ElmEbiSectionKind, offset: u32, size: u32, flags: u32) -> Self {
        Self {
            kind: kind as u32,
            offset,
            size,
            flags,
        }
    }

    pub const fn section_kind(&self) -> Option<ElmEbiSectionKind> {
        ElmEbiSectionKind::from_raw(self.kind)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmEbiManifestRecord {
    pub name_len: u16,
    pub version_len: u16,
    pub kind: u32,
    pub flags: u32,
    pub menu_kind: u32,
    pub menu_flags: u32,
    pub menu_label_len: u16,
    pub menu_description_len: u16,
    pub menu_route_len: u16,
    pub reserved: u16,
    pub name: [u8; ELM_EBI_MANIFEST_NAME_LEN],
    pub version: [u8; ELM_EBI_MANIFEST_VERSION_LEN],
    pub menu_label: [u8; ELM_EBI_MANIFEST_LABEL_LEN],
    pub menu_description: [u8; ELM_EBI_MANIFEST_DESCRIPTION_LEN],
    pub menu_route: [u8; ELM_EBI_MANIFEST_ROUTE_LEN],
}

impl ElmEbiManifestRecord {
    pub fn new(name: &str, version: &str, kind: ElmKind) -> Self {
        let mut out = Self {
            name_len: 0,
            version_len: 0,
            kind: elm_kind_code(kind),
            flags: 0,
            menu_kind: ElmMenuItemKind::Status as u32,
            menu_flags: 0,
            menu_label_len: 0,
            menu_description_len: 0,
            menu_route_len: 0,
            reserved: 0,
            name: [0; ELM_EBI_MANIFEST_NAME_LEN],
            version: [0; ELM_EBI_MANIFEST_VERSION_LEN],
            menu_label: [0; ELM_EBI_MANIFEST_LABEL_LEN],
            menu_description: [0; ELM_EBI_MANIFEST_DESCRIPTION_LEN],
            menu_route: [0; ELM_EBI_MANIFEST_ROUTE_LEN],
        };
        out.name_len = copy_str(name, &mut out.name) as u16;
        out.version_len = copy_str(version, &mut out.version) as u16;
        out
    }

    pub fn with_menu_item(
        mut self,
        kind: ElmMenuItemKind,
        flags: u32,
        label: &str,
        description: &str,
        route: &str,
    ) -> Self {
        self.flags |= ELM_EBI_MANIFEST_FLAG_MENU_ITEM;
        self.menu_kind = kind as u32;
        self.menu_flags = flags;
        self.menu_label_len = copy_str(label, &mut self.menu_label) as u16;
        self.menu_description_len = copy_str(description, &mut self.menu_description) as u16;
        self.menu_route_len = copy_str(route, &mut self.menu_route) as u16;
        self
    }

    pub const fn has_menu_item(&self) -> bool {
        self.flags & ELM_EBI_MANIFEST_FLAG_MENU_ITEM != 0
    }

    pub const fn requests_native_entry(&self) -> bool {
        self.flags & ELM_EBI_MANIFEST_FLAG_NATIVE_ENTRY != 0
    }

    pub fn name_str(&self) -> Result<&str, ElmEbiLoadStatus> {
        fixed_str(&self.name, self.name_len as usize)
    }

    pub fn version_str(&self) -> Result<&str, ElmEbiLoadStatus> {
        fixed_str(&self.version, self.version_len as usize)
    }

    pub fn menu_label_str(&self) -> Result<&str, ElmEbiLoadStatus> {
        fixed_str(&self.menu_label, self.menu_label_len as usize)
    }

    pub fn menu_description_str(&self) -> Result<&str, ElmEbiLoadStatus> {
        fixed_str(&self.menu_description, self.menu_description_len as usize)
    }

    pub fn menu_route_str(&self) -> Result<&str, ElmEbiLoadStatus> {
        fixed_str(&self.menu_route, self.menu_route_len as usize)
    }

    pub fn menu_item_kind(&self) -> Result<ElmMenuItemKind, ElmEbiLoadStatus> {
        ElmMenuItemKind::from_raw(self.menu_kind).ok_or(ElmEbiLoadStatus::InvalidManifest)
    }

    pub fn to_manifest(&self) -> Result<ElmManifest, ElmEbiLoadStatus> {
        let name = ElmName::new(self.name_str()?).map_err(|_| ElmEbiLoadStatus::InvalidManifest)?;
        let version =
            ElmVersion::new(self.version_str()?).map_err(|_| ElmEbiLoadStatus::InvalidManifest)?;
        let kind = elm_kind_from_code(self.kind).ok_or(ElmEbiLoadStatus::InvalidManifest)?;
        Ok(ElmManifest::new(name, version, kind))
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmLoadCellResponse {
    pub cell_id: u64,
    pub status: i32,
    pub final_state: u32,
    pub reason: u32,
    pub reserved: u32,
}

impl ElmLoadCellResponse {
    pub const fn new(
        status: ElmEbiLoadStatus,
        cell_id: u64,
        final_state: u32,
        reason: u32,
    ) -> Self {
        Self {
            cell_id,
            status: status as i32,
            final_state,
            reason,
            reserved: 0,
        }
    }

    pub const fn failed(status: ElmEbiLoadStatus) -> Self {
        Self::new(status, 0, 0, 0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiImage {
    pub header: ElmEbiHeader,
    pub manifest: ElmEbiManifestRecord,
    pub sections: Vec<ElmEbiSectionHeader>,
    pub has_native_code: bool,
}

impl ElmEbiImage {
    pub fn parse(bytes: &[u8], expected_arch: ElmEbiArch) -> Result<Self, ElmEbiLoadStatus> {
        if bytes.len() < ELM_EBI_HEADER_SIZE {
            return Err(ElmEbiLoadStatus::InvalidImage);
        }
        if bytes.len() > ELM_EBI_MAX_IMAGE_SIZE {
            return Err(ElmEbiLoadStatus::ImageTooLarge);
        }

        let header = read_header(bytes)?;
        if header.magic != ELM_EBI_MAGIC {
            return Err(ElmEbiLoadStatus::InvalidImage);
        }
        if header.abi_version != ELM_EBI_ABI_VERSION {
            return Err(ElmEbiLoadStatus::UnsupportedAbi);
        }
        if header.header_size as usize != ELM_EBI_HEADER_SIZE {
            return Err(ElmEbiLoadStatus::InvalidImage);
        }
        if header.image_size as usize != bytes.len() {
            return Err(ElmEbiLoadStatus::InvalidImage);
        }
        let arch = ElmEbiArch::from_raw(header.arch).ok_or(ElmEbiLoadStatus::InvalidImage)?;
        if !arch.matches(expected_arch) {
            return Err(ElmEbiLoadStatus::ArchMismatch);
        }
        if header.section_count as usize > ELM_EBI_MAX_SECTIONS {
            return Err(ElmEbiLoadStatus::InvalidImage);
        }
        if header.section_entry_size as usize != ELM_EBI_SECTION_HEADER_SIZE {
            return Err(ElmEbiLoadStatus::InvalidImage);
        }

        let sections = read_sections(bytes, &header)?;
        let manifest = read_manifest(bytes, &header)?;
        validate_manifest(&manifest)?;

        let mut has_native_code = manifest.requests_native_entry();
        for section in &sections {
            match section.section_kind() {
                Some(ElmEbiSectionKind::Code) | Some(ElmEbiSectionKind::Relocation) => {
                    has_native_code = true;
                }
                Some(_) => {}
                None => return Err(ElmEbiLoadStatus::UnsupportedSection),
            }
        }

        Ok(Self {
            header,
            manifest,
            sections,
            has_native_code,
        })
    }
}

pub const fn elm_kind_code(kind: ElmKind) -> u32 {
    match kind {
        ElmKind::Manager => 1,
        ElmKind::Service => 2,
        ElmKind::Driver => 3,
        ElmKind::Extension => 4,
        ElmKind::Filesystem => 5,
        ElmKind::Network => 6,
        ElmKind::Debug => 7,
        ElmKind::Other => 255,
    }
}

pub const fn elm_kind_from_code(code: u32) -> Option<ElmKind> {
    match code {
        1 => Some(ElmKind::Manager),
        2 => Some(ElmKind::Service),
        3 => Some(ElmKind::Driver),
        4 => Some(ElmKind::Extension),
        5 => Some(ElmKind::Filesystem),
        6 => Some(ElmKind::Network),
        7 => Some(ElmKind::Debug),
        255 => Some(ElmKind::Other),
        _ => None,
    }
}

fn read_header(bytes: &[u8]) -> Result<ElmEbiHeader, ElmEbiLoadStatus> {
    Ok(ElmEbiHeader {
        magic: read_u32(bytes, 0)?,
        abi_version: read_u16(bytes, 4)?,
        header_size: read_u16(bytes, 6)?,
        image_size: read_u32(bytes, 8)?,
        arch: read_u32(bytes, 12)?,
        section_count: read_u16(bytes, 16)?,
        section_entry_size: read_u16(bytes, 18)?,
        section_table_offset: read_u32(bytes, 20)?,
        manifest_offset: read_u32(bytes, 24)?,
        manifest_size: read_u32(bytes, 28)?,
        flags: read_u32(bytes, 32)?,
    })
}

fn read_sections(
    bytes: &[u8],
    header: &ElmEbiHeader,
) -> Result<Vec<ElmEbiSectionHeader>, ElmEbiLoadStatus> {
    let count = header.section_count as usize;
    let table_offset = header.section_table_offset as usize;
    let table_size = count
        .checked_mul(ELM_EBI_SECTION_HEADER_SIZE)
        .ok_or(ElmEbiLoadStatus::InvalidImage)?;
    checked_range(bytes.len(), table_offset, table_size)?;

    let mut sections = Vec::new();
    for index in 0..count {
        let offset = table_offset + index * ELM_EBI_SECTION_HEADER_SIZE;
        let section = ElmEbiSectionHeader {
            kind: read_u32(bytes, offset)?,
            offset: read_u32(bytes, offset + 4)?,
            size: read_u32(bytes, offset + 8)?,
            flags: read_u32(bytes, offset + 12)?,
        };
        section
            .section_kind()
            .ok_or(ElmEbiLoadStatus::UnsupportedSection)?;
        checked_range(bytes.len(), section.offset as usize, section.size as usize)?;
        sections.push(section);
    }
    Ok(sections)
}

fn read_manifest(
    bytes: &[u8],
    header: &ElmEbiHeader,
) -> Result<ElmEbiManifestRecord, ElmEbiLoadStatus> {
    let offset = header.manifest_offset as usize;
    let size = header.manifest_size as usize;
    if size < core::mem::size_of::<ElmEbiManifestRecord>() {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    checked_range(bytes.len(), offset, size)?;
    let raw = &bytes[offset..offset + core::mem::size_of::<ElmEbiManifestRecord>()];

    let mut manifest = ElmEbiManifestRecord::new("", "", ElmKind::Other);
    manifest.name_len = read_u16(raw, 0)?;
    manifest.version_len = read_u16(raw, 2)?;
    manifest.kind = read_u32(raw, 4)?;
    manifest.flags = read_u32(raw, 8)?;
    manifest.menu_kind = read_u32(raw, 12)?;
    manifest.menu_flags = read_u32(raw, 16)?;
    manifest.menu_label_len = read_u16(raw, 20)?;
    manifest.menu_description_len = read_u16(raw, 22)?;
    manifest.menu_route_len = read_u16(raw, 24)?;
    manifest.reserved = read_u16(raw, 26)?;

    let mut cursor = 28;
    copy_array(raw, &mut cursor, &mut manifest.name)?;
    copy_array(raw, &mut cursor, &mut manifest.version)?;
    copy_array(raw, &mut cursor, &mut manifest.menu_label)?;
    copy_array(raw, &mut cursor, &mut manifest.menu_description)?;
    copy_array(raw, &mut cursor, &mut manifest.menu_route)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &ElmEbiManifestRecord) -> Result<(), ElmEbiLoadStatus> {
    manifest.to_manifest()?;
    if manifest.has_menu_item() {
        manifest.menu_item_kind()?;
        if manifest.menu_label_str()?.is_empty() || manifest.menu_route_str()?.is_empty() {
            return Err(ElmEbiLoadStatus::InvalidManifest);
        }
        manifest.menu_description_str()?;
    }
    Ok(())
}

fn checked_range(total: usize, offset: usize, size: usize) -> Result<(), ElmEbiLoadStatus> {
    let end = offset
        .checked_add(size)
        .ok_or(ElmEbiLoadStatus::SectionOutOfBounds)?;
    if end <= total {
        Ok(())
    } else {
        Err(ElmEbiLoadStatus::SectionOutOfBounds)
    }
}

fn fixed_str(bytes: &[u8], len: usize) -> Result<&str, ElmEbiLoadStatus> {
    if len > bytes.len() {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    str::from_utf8(&bytes[..len]).map_err(|_| ElmEbiLoadStatus::InvalidManifest)
}

fn copy_str(src: &str, dst: &mut [u8]) -> usize {
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&bytes[..n]);
    n
}

fn copy_array<const N: usize>(
    src: &[u8],
    cursor: &mut usize,
    dst: &mut [u8; N],
) -> Result<(), ElmEbiLoadStatus> {
    checked_range(src.len(), *cursor, N)?;
    dst.copy_from_slice(&src[*cursor..*cursor + N]);
    *cursor += N;
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ElmEbiLoadStatus> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or(ElmEbiLoadStatus::InvalidImage)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ElmEbiLoadStatus> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or(ElmEbiLoadStatus::InvalidImage)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

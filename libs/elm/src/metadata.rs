//! ELM Rust 编译期元数据协议。
//!
//! attribute 宏把规范记录写入非装载段 `.elm.meta`，宿主工具只解析本协议，
//! 不依赖 Rust 符号修饰规则，也不从函数名猜测 EBI 拓扑。

use alloc::vec::Vec;
use core::str;

pub const ELM_RUST_METADATA_MAGIC: [u8; 8] = *b"ELMMETA1";
pub const ELM_RUST_METADATA_VERSION: u16 = 1;
pub const ELM_RUST_METADATA_HEADER_SIZE: usize = 32;
pub const ELM_RUST_METADATA_FIELD_HEADER_SIZE: usize = 8;
pub const ELM_RUST_METADATA_ALIGNMENT: usize = 8;
pub const ELM_RUST_METADATA_MAX_RECORD_SIZE: usize = 64 * 1024;

pub const ELM_META_FIELD_SYMBOL: u16 = 1;
pub const ELM_META_FIELD_HOOK_KIND: u16 = 2;
pub const ELM_META_FIELD_NAME: u16 = 3;
pub const ELM_META_FIELD_CONTRACT: u16 = 4;
pub const ELM_META_FIELD_MIN_VERSION: u16 = 5;
pub const ELM_META_FIELD_MAX_VERSION: u16 = 6;
pub const ELM_META_FIELD_VERSION: u16 = 7;
pub const ELM_META_FIELD_FLAGS: u16 = 8;
pub const ELM_META_FIELD_ACCESS: u16 = 9;
pub const ELM_META_FIELD_DIRECTION: u16 = 10;
pub const ELM_META_FIELD_MODE: u16 = 11;
pub const ELM_META_FIELD_TARGET: u16 = 12;
pub const ELM_META_FIELD_POINT: u16 = 13;
pub const ELM_META_FIELD_STAGE: u16 = 14;
pub const ELM_META_FIELD_PRIORITY: u16 = 15;
pub const ELM_META_FIELD_HANDLER_CONTRACT: u16 = 16;
pub const ELM_META_FIELD_PAYLOAD_CONTRACT: u16 = 17;
pub const ELM_META_FIELD_WIRE_SIZE: u16 = 18;
pub const ELM_META_FIELD_STAGES: u16 = 19;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ElmRustMetadataKind {
    Lifecycle = 1,
    Entry = 2,
    Provider = 3,
    ProviderSnapshot = 4,
    Export = 5,
    Import = 6,
    ExtensionPoint = 7,
    Extension = 8,
    Payload = 9,
}

impl ElmRustMetadataKind {
    pub const fn from_raw(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::Lifecycle),
            2 => Some(Self::Entry),
            3 => Some(Self::Provider),
            4 => Some(Self::ProviderSnapshot),
            5 => Some(Self::Export),
            6 => Some(Self::Import),
            7 => Some(Self::ExtensionPoint),
            8 => Some(Self::Extension),
            9 => Some(Self::Payload),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ElmRustMetadataValueKind {
    Utf8 = 1,
    U32 = 2,
    I32 = 3,
    U64 = 4,
    Bool = 5,
}

impl ElmRustMetadataValueKind {
    pub const fn from_raw(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::Utf8),
            2 => Some(Self::U32),
            3 => Some(Self::I32),
            4 => Some(Self::U64),
            5 => Some(Self::Bool),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElmRustMetadataError {
    Truncated,
    InvalidMagic,
    UnsupportedVersion,
    InvalidKind,
    InvalidHeader,
    InvalidRecordSize,
    InvalidChecksum,
    InvalidField,
    DuplicateOrUnsortedField,
    InvalidUtf8,
    NonZeroPadding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmRustMetadataField<'a> {
    pub tag: u16,
    pub kind: ElmRustMetadataValueKind,
    pub bytes: &'a [u8],
}

impl<'a> ElmRustMetadataField<'a> {
    pub fn utf8(self) -> Result<&'a str, ElmRustMetadataError> {
        if self.kind != ElmRustMetadataValueKind::Utf8 || self.bytes.contains(&0) {
            return Err(ElmRustMetadataError::InvalidField);
        }
        str::from_utf8(self.bytes).map_err(|_| ElmRustMetadataError::InvalidUtf8)
    }

    pub fn u32(self) -> Result<u32, ElmRustMetadataError> {
        if self.kind != ElmRustMetadataValueKind::U32 || self.bytes.len() != 4 {
            return Err(ElmRustMetadataError::InvalidField);
        }
        Ok(u32::from_le_bytes(
            self.bytes
                .try_into()
                .map_err(|_| ElmRustMetadataError::InvalidField)?,
        ))
    }

    pub fn i32(self) -> Result<i32, ElmRustMetadataError> {
        if self.kind != ElmRustMetadataValueKind::I32 || self.bytes.len() != 4 {
            return Err(ElmRustMetadataError::InvalidField);
        }
        Ok(i32::from_le_bytes(
            self.bytes
                .try_into()
                .map_err(|_| ElmRustMetadataError::InvalidField)?,
        ))
    }

    pub fn u64(self) -> Result<u64, ElmRustMetadataError> {
        if self.kind != ElmRustMetadataValueKind::U64 || self.bytes.len() != 8 {
            return Err(ElmRustMetadataError::InvalidField);
        }
        Ok(u64::from_le_bytes(
            self.bytes
                .try_into()
                .map_err(|_| ElmRustMetadataError::InvalidField)?,
        ))
    }

    pub fn boolean(self) -> Result<bool, ElmRustMetadataError> {
        if self.kind != ElmRustMetadataValueKind::Bool || self.bytes.len() != 1 {
            return Err(ElmRustMetadataError::InvalidField);
        }
        match self.bytes[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ElmRustMetadataError::InvalidField),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmRustMetadataRecord<'a> {
    pub kind: ElmRustMetadataKind,
    pub flags: u32,
    pub fields: Vec<ElmRustMetadataField<'a>>,
}

impl<'a> ElmRustMetadataRecord<'a> {
    pub fn field(&self, tag: u16) -> Option<ElmRustMetadataField<'a>> {
        self.fields.iter().find(|field| field.tag == tag).copied()
    }

    pub fn require_field(
        &self,
        tag: u16,
    ) -> Result<ElmRustMetadataField<'a>, ElmRustMetadataError> {
        self.field(tag).ok_or(ElmRustMetadataError::InvalidField)
    }
}

pub fn parse_rust_metadata_section(
    bytes: &[u8],
) -> Result<Vec<ElmRustMetadataRecord<'_>>, ElmRustMetadataError> {
    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        while bytes.get(offset) == Some(&0) {
            offset += 1;
        }
        if offset == bytes.len() {
            break;
        }
        let header = bytes
            .get(offset..offset + ELM_RUST_METADATA_HEADER_SIZE)
            .ok_or(ElmRustMetadataError::Truncated)?;
        if header[..8] != ELM_RUST_METADATA_MAGIC {
            return Err(ElmRustMetadataError::InvalidMagic);
        }
        let version = read_u16(header, 8)?;
        if version != ELM_RUST_METADATA_VERSION {
            return Err(ElmRustMetadataError::UnsupportedVersion);
        }
        let kind = ElmRustMetadataKind::from_raw(read_u16(header, 10)?)
            .ok_or(ElmRustMetadataError::InvalidKind)?;
        let header_size = read_u16(header, 12)? as usize;
        let field_count = read_u16(header, 14)? as usize;
        let record_size = read_u32(header, 16)? as usize;
        let flags = read_u32(header, 20)?;
        let checksum = read_u32(header, 24)?;
        if header_size != ELM_RUST_METADATA_HEADER_SIZE || read_u32(header, 28)? != 0 {
            return Err(ElmRustMetadataError::InvalidHeader);
        }
        if record_size < header_size
            || record_size > ELM_RUST_METADATA_MAX_RECORD_SIZE
            || record_size % ELM_RUST_METADATA_ALIGNMENT != 0
        {
            return Err(ElmRustMetadataError::InvalidRecordSize);
        }
        let record = bytes
            .get(offset..offset + record_size)
            .ok_or(ElmRustMetadataError::Truncated)?;
        let payload = &record[header_size..];
        if crc32(payload) != checksum {
            return Err(ElmRustMetadataError::InvalidChecksum);
        }
        let mut fields = Vec::new();
        fields
            .try_reserve_exact(field_count)
            .map_err(|_| ElmRustMetadataError::InvalidRecordSize)?;
        let mut field_offset = header_size;
        let mut previous_tag = 0u16;
        for _ in 0..field_count {
            let field_header = record
                .get(field_offset..field_offset + ELM_RUST_METADATA_FIELD_HEADER_SIZE)
                .ok_or(ElmRustMetadataError::Truncated)?;
            let tag = read_u16(field_header, 0)?;
            let value_kind = ElmRustMetadataValueKind::from_raw(read_u16(field_header, 2)?)
                .ok_or(ElmRustMetadataError::InvalidField)?;
            let value_len = read_u32(field_header, 4)? as usize;
            if tag == 0 || tag <= previous_tag {
                return Err(ElmRustMetadataError::DuplicateOrUnsortedField);
            }
            previous_tag = tag;
            let value_start = field_offset + ELM_RUST_METADATA_FIELD_HEADER_SIZE;
            let value_end = value_start
                .checked_add(value_len)
                .ok_or(ElmRustMetadataError::InvalidField)?;
            let padded_end = align_up(value_end, ELM_RUST_METADATA_ALIGNMENT)
                .ok_or(ElmRustMetadataError::InvalidField)?;
            let value = record
                .get(value_start..value_end)
                .ok_or(ElmRustMetadataError::Truncated)?;
            let padding = record
                .get(value_end..padded_end)
                .ok_or(ElmRustMetadataError::Truncated)?;
            if padding.iter().any(|byte| *byte != 0) {
                return Err(ElmRustMetadataError::NonZeroPadding);
            }
            validate_value(value_kind, value)?;
            fields.push(ElmRustMetadataField {
                tag,
                kind: value_kind,
                bytes: value,
            });
            field_offset = padded_end;
        }
        if field_offset != record_size {
            return Err(ElmRustMetadataError::InvalidRecordSize);
        }
        records.push(ElmRustMetadataRecord {
            kind,
            flags,
            fields,
        });
        offset = offset
            .checked_add(record_size)
            .ok_or(ElmRustMetadataError::InvalidRecordSize)?;
    }
    Ok(records)
}

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn validate_value(
    kind: ElmRustMetadataValueKind,
    bytes: &[u8],
) -> Result<(), ElmRustMetadataError> {
    match kind {
        ElmRustMetadataValueKind::Utf8 => {
            if bytes.is_empty() || bytes.contains(&0) || str::from_utf8(bytes).is_err() {
                return Err(ElmRustMetadataError::InvalidUtf8);
            }
        }
        ElmRustMetadataValueKind::U32 | ElmRustMetadataValueKind::I32 => {
            if bytes.len() != 4 {
                return Err(ElmRustMetadataError::InvalidField);
            }
        }
        ElmRustMetadataValueKind::U64 => {
            if bytes.len() != 8 {
                return Err(ElmRustMetadataError::InvalidField);
            }
        }
        ElmRustMetadataValueKind::Bool => {
            if !matches!(bytes, [0] | [1]) {
                return Err(ElmRustMetadataError::InvalidField);
            }
        }
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ElmRustMetadataError> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or(ElmRustMetadataError::Truncated)?
            .try_into()
            .map_err(|_| ElmRustMetadataError::Truncated)?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ElmRustMetadataError> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(ElmRustMetadataError::Truncated)?
            .try_into()
            .map_err(|_| ElmRustMetadataError::Truncated)?,
    ))
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    if align == 0 || !align.is_power_of_two() {
        return None;
    }
    match value.checked_add(align - 1) {
        Some(value) => Some(value & !(align - 1)),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn parses_canonical_metadata_record() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&ELM_META_FIELD_SYMBOL.to_le_bytes());
        payload.extend_from_slice(&(ElmRustMetadataValueKind::Utf8 as u16).to_le_bytes());
        payload.extend_from_slice(&4u32.to_le_bytes());
        payload.extend_from_slice(b"hook");
        payload.extend_from_slice(&[0; 4]);

        let mut record = Vec::new();
        record.extend_from_slice(&ELM_RUST_METADATA_MAGIC);
        record.extend_from_slice(&ELM_RUST_METADATA_VERSION.to_le_bytes());
        record.extend_from_slice(&(ElmRustMetadataKind::Lifecycle as u16).to_le_bytes());
        record.extend_from_slice(&(ELM_RUST_METADATA_HEADER_SIZE as u16).to_le_bytes());
        record.extend_from_slice(&1u16.to_le_bytes());
        record.extend_from_slice(
            &((ELM_RUST_METADATA_HEADER_SIZE + payload.len()) as u32).to_le_bytes(),
        );
        record.extend_from_slice(&0u32.to_le_bytes());
        record.extend_from_slice(&crc32(&payload).to_le_bytes());
        record.extend_from_slice(&0u32.to_le_bytes());
        record.extend_from_slice(&payload);

        let records = parse_rust_metadata_section(&record).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, ElmRustMetadataKind::Lifecycle);
        assert_eq!(
            records[0]
                .require_field(ELM_META_FIELD_SYMBOL)
                .unwrap()
                .utf8(),
            Ok("hook")
        );
    }

    #[test]
    fn rejects_corrupted_metadata_record() {
        let mut record = vec![0; ELM_RUST_METADATA_HEADER_SIZE];
        record[..8].copy_from_slice(&ELM_RUST_METADATA_MAGIC);
        record[8..10].copy_from_slice(&ELM_RUST_METADATA_VERSION.to_le_bytes());
        record[10..12].copy_from_slice(&(ElmRustMetadataKind::Entry as u16).to_le_bytes());
        record[12..14].copy_from_slice(&(ELM_RUST_METADATA_HEADER_SIZE as u16).to_le_bytes());
        record[16..20].copy_from_slice(&(ELM_RUST_METADATA_HEADER_SIZE as u32).to_le_bytes());
        record[24..28].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            parse_rust_metadata_section(&record),
            Err(ElmRustMetadataError::InvalidChecksum)
        );
    }
}

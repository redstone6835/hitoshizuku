//! ELM Rust 编译期元数据协议。
//!
//! attribute 宏把规范记录写入非装载段 `.elm.meta`，宿主工具只解析本协议，
//! 不依赖 Rust 符号修饰规则，也不从函数名猜测 EBI 拓扑。
//!
//! 每条 `ELMMETA1` 记录具有固定头、记录 kind、已排序字段、payload CRC32 和八字节零填充。
//! parser 拒绝重复/乱序字段、未知 value kind、非零 padding、非法 UTF-8、越界长度和错误
//! checksum。`.elm.meta` 不属于运行时映射，打包器读取后将其规范化为 EBI 声明。
//!
//! 该协议是 Rust attribute 与 `elm-tools` 之间的构建协议，不是模块运行时 API，也不能被
//! 模块用来在装载后修改自身拓扑。

use alloc::vec::Vec;
use core::str;

/// `ELM_RUST_METADATA_MAGIC` 的固定魔数；解析器必须先校验该值，再解释后续布局。
pub const ELM_RUST_METADATA_MAGIC: [u8; 8] = *b"ELMMETA1";
/// `ELM_RUST_METADATA_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_RUST_METADATA_VERSION: u16 = 1;
/// `ELM_RUST_METADATA_HEADER_SIZE` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_RUST_METADATA_HEADER_SIZE: usize = 32;
/// `ELM_RUST_METADATA_FIELD_HEADER_SIZE` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_RUST_METADATA_FIELD_HEADER_SIZE: usize = 8;
/// `ELM_RUST_METADATA_ALIGNMENT` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_RUST_METADATA_ALIGNMENT: usize = 8;
/// `ELM_RUST_METADATA_MAX_RECORD_SIZE` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_RUST_METADATA_MAX_RECORD_SIZE: usize = 64 * 1024;

/// `.elm.meta` 字段表中标识 `symbol` 属性的稳定 tag。
pub const ELM_META_FIELD_SYMBOL: u16 = 1;
/// `.elm.meta` 字段表中标识 `hook_kind` 属性的稳定 tag。
pub const ELM_META_FIELD_HOOK_KIND: u16 = 2;
/// `ELM_META_FIELD_NAME` 的规范 identifier 或契约名称；比较时使用完整字节串而不是截断哈希。
pub const ELM_META_FIELD_NAME: u16 = 3;
/// `ELM_META_FIELD_CONTRACT` 的规范 identifier 或契约名称；比较时使用完整字节串而不是截断哈希。
pub const ELM_META_FIELD_CONTRACT: u16 = 4;
/// `ELM_META_FIELD_MIN_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_META_FIELD_MIN_VERSION: u16 = 5;
/// `ELM_META_FIELD_MAX_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_META_FIELD_MAX_VERSION: u16 = 6;
/// `ELM_META_FIELD_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_META_FIELD_VERSION: u16 = 7;
/// `.elm.meta` 字段表中标识 `flags` 属性的稳定 tag。
pub const ELM_META_FIELD_FLAGS: u16 = 8;
/// `.elm.meta` 字段表中标识 `access` 属性的稳定 tag。
pub const ELM_META_FIELD_ACCESS: u16 = 9;
/// `.elm.meta` 字段表中标识 `direction` 属性的稳定 tag。
pub const ELM_META_FIELD_DIRECTION: u16 = 10;
/// `.elm.meta` 字段表中标识 `mode` 属性的稳定 tag。
pub const ELM_META_FIELD_MODE: u16 = 11;
/// `.elm.meta` 字段表中标识 `target` 属性的稳定 tag。
pub const ELM_META_FIELD_TARGET: u16 = 12;
/// `.elm.meta` 字段表中标识 `point` 属性的稳定 tag。
pub const ELM_META_FIELD_POINT: u16 = 13;
/// `.elm.meta` 字段表中标识 `stage` 属性的稳定 tag。
pub const ELM_META_FIELD_STAGE: u16 = 14;
/// `.elm.meta` 字段表中标识 `priority` 属性的稳定 tag。
pub const ELM_META_FIELD_PRIORITY: u16 = 15;
/// `ELM_META_FIELD_HANDLER_CONTRACT` 的规范 identifier 或契约名称；比较时使用完整字节串而不是截断哈希。
pub const ELM_META_FIELD_HANDLER_CONTRACT: u16 = 16;
/// `ELM_META_FIELD_PAYLOAD_CONTRACT` 的规范 identifier 或契约名称；比较时使用完整字节串而不是截断哈希。
pub const ELM_META_FIELD_PAYLOAD_CONTRACT: u16 = 17;
/// `ELM_META_FIELD_WIRE_SIZE` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_META_FIELD_WIRE_SIZE: u16 = 18;
/// `.elm.meta` 字段表中标识 `stages` 属性的稳定 tag。
pub const ELM_META_FIELD_STAGES: u16 = 19;
/// `.elm.meta` 字段表中标识规范 Rust 函数指针签名的稳定 tag。
///
/// 该字段只用于直接固定 import/export 和内核直接符号。构建工具必须对完整 UTF-8 字节串
/// 计算 SHA-256，并把摘要写入 EBI；运行时不得仅凭函数名或版本接受裸地址。
pub const ELM_META_FIELD_RUST_ABI: u16 = 27;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
/// `ElmRustMetadataKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmRustMetadataKind {
    /// `Lifecycle` 表示 `ElmRustMetadataKind` 的对象类别：`lifecycle`。
    Lifecycle = 1,
    /// `Entry` 表示 `ElmRustMetadataKind` 的对象类别：`entry`。
    Entry = 2,
    /// `Provider` 表示 `ElmRustMetadataKind` 的对象类别：`provider`。
    Provider = 3,
    /// `ProviderSnapshot` 表示 `ElmRustMetadataKind` 的对象类别：`provider snapshot`。
    ProviderSnapshot = 4,
    /// `Export` 表示 `ElmRustMetadataKind` 的对象类别：`export`。
    Export = 5,
    /// `Import` 表示 `ElmRustMetadataKind` 的对象类别：`import`。
    Import = 6,
    /// `ExtensionPoint` 表示 `ElmRustMetadataKind` 的对象类别：`extension point`。
    ExtensionPoint = 7,
    /// `Extension` 表示 `ElmRustMetadataKind` 的对象类别：`extension`。
    Extension = 8,
    /// `Payload` 表示 `ElmRustMetadataKind` 的对象类别：`payload`。
    Payload = 9,
    /// `Module` 表示当前镜像唯一的统一模块描述符。
    Module = 18,
}

impl ElmRustMetadataKind {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
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
            18 => Some(Self::Module),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
/// `ElmRustMetadataValueKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmRustMetadataValueKind {
    /// `Utf8` 表示 `ElmRustMetadataValueKind` 的对象类别：`utf8`。
    Utf8 = 1,
    /// `U32` 表示 `ElmRustMetadataValueKind` 的对象类别：`u32`。
    U32 = 2,
    /// `I32` 表示 `ElmRustMetadataValueKind` 的对象类别：`i32`。
    I32 = 3,
    /// `U64` 表示 `ElmRustMetadataValueKind` 的对象类别：`u64`。
    U64 = 4,
    /// `Bool` 表示 `ElmRustMetadataValueKind` 的对象类别：`bool`。
    Bool = 5,
}

impl ElmRustMetadataValueKind {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
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
/// `ElmRustMetadataError` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmRustMetadataError {
    /// `Truncated` 表示 `ElmRustMetadataError` 的错误：`truncated`。
    Truncated,
    /// `InvalidMagic` 表示 `ElmRustMetadataError` 的错误：`invalid magic`。
    InvalidMagic,
    /// `UnsupportedVersion` 表示 `ElmRustMetadataError` 的错误：`unsupported version`。
    UnsupportedVersion,
    /// `InvalidKind` 表示 `ElmRustMetadataError` 的错误：`invalid kind`。
    InvalidKind,
    /// `InvalidHeader` 表示 `ElmRustMetadataError` 的错误：`invalid header`。
    InvalidHeader,
    /// `InvalidRecordSize` 表示 `ElmRustMetadataError` 的错误：`invalid record size`。
    InvalidRecordSize,
    /// `InvalidChecksum` 表示 `ElmRustMetadataError` 的错误：`invalid checksum`。
    InvalidChecksum,
    /// `InvalidField` 表示 `ElmRustMetadataError` 的错误：`invalid field`。
    InvalidField,
    /// `DuplicateOrUnsortedField` 表示 `ElmRustMetadataError` 的错误：`duplicate or unsorted field`。
    DuplicateOrUnsortedField,
    /// `InvalidUtf8` 表示 `ElmRustMetadataError` 的错误：`invalid utf8`。
    InvalidUtf8,
    /// `NonZeroPadding` 表示 `ElmRustMetadataError` 的错误：`non zero padding`。
    NonZeroPadding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 一条 `.elm.meta` 记录中已验证 tag、value kind 和原始值的字段视图。
pub struct ElmRustMetadataField<'a> {
    /// `tag` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub tag: u16,
    /// 该记录、资源或关系的类别编码。
    pub kind: ElmRustMetadataValueKind,
    /// `bytes` 保存所属对象声明或快照中的有序记录集合。
    pub bytes: &'a [u8],
}

impl<'a> ElmRustMetadataField<'a> {
    /// 执行 `utf8` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn utf8(self) -> Result<&'a str, ElmRustMetadataError> {
        if self.kind != ElmRustMetadataValueKind::Utf8 || self.bytes.contains(&0) {
            return Err(ElmRustMetadataError::InvalidField);
        }
        str::from_utf8(self.bytes).map_err(|_| ElmRustMetadataError::InvalidUtf8)
    }

    /// 执行 `u32` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `i32` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `u64` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `boolean` 定义的模型或协议操作；返回值反映校验后的结果。
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
/// `ElmRustMetadataRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmRustMetadataRecord<'a> {
    /// 该记录、资源或关系的类别编码。
    pub kind: ElmRustMetadataKind,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 该元数据记录已经按 tag 排序并验证的字段集合。
    pub fields: Vec<ElmRustMetadataField<'a>>,
}

impl<'a> ElmRustMetadataRecord<'a> {
    /// 执行 `field` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn field(&self, tag: u16) -> Option<ElmRustMetadataField<'a>> {
        self.fields.iter().find(|field| field.tag == tag).copied()
    }

    /// 执行 `require_field` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn require_field(
        &self,
        tag: u16,
    ) -> Result<ElmRustMetadataField<'a>, ElmRustMetadataError> {
        self.field(tag).ok_or(ElmRustMetadataError::InvalidField)
    }
}

/// 执行 `parse_rust_metadata_section` 定义的模型或协议操作；返回值反映校验后的结果。
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

/// 执行 `crc32` 定义的模型或协议操作；返回值反映校验后的结果。
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

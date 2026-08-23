//! 通用 V1 结构校验原语。

use core::mem::size_of;

use crate::{LanguageHandle, LanguageRuntimeStatus};

/// 可由固定布局结构报告的校验失败原因。
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageValidationError {
    /// ABI 版本不匹配。
    AbiVersion = 1,
    /// 结构尺寸不匹配。
    StructSize = 2,
    /// flags 设置了未知位。
    Flags = 3,
    /// 保留字段非零。
    Reserved = 4,
    /// ID 为零。
    Identifier = 5,
    /// 句柄无效。
    Handle = 6,
    /// owner cell 或 generation 无效。
    Owner = 7,
    /// 载荷长度超过固定容量。
    PayloadLength = 8,
    /// 请求状态值未知。
    State = 9,
    /// 名称不是合法的固定 ASCII 标识符。
    Name = 10,
    /// 容量字段为零或超过协议上限。
    Capacity = 11,
}

/// ABI 结构校验的结果别名。
pub type ValidationResult = Result<(), LanguageValidationError>;

impl LanguageValidationError {
    /// 将校验原因映射到稳定的运行时状态码。
    pub const fn status(self) -> LanguageRuntimeStatus {
        match self {
            Self::AbiVersion => LanguageRuntimeStatus::ABI_MISMATCH,
            Self::StructSize => LanguageRuntimeStatus::SIZE_MISMATCH,
            Self::Flags => LanguageRuntimeStatus::FLAGS_INVALID,
            Self::Reserved => LanguageRuntimeStatus::RESERVED_NONZERO,
            Self::Identifier => LanguageRuntimeStatus::INVALID_ID,
            Self::Handle => LanguageRuntimeStatus::HANDLE_INVALID,
            Self::Owner => LanguageRuntimeStatus::OWNER_MISMATCH,
            Self::PayloadLength => LanguageRuntimeStatus::PAYLOAD_TOO_LARGE,
            Self::State => LanguageRuntimeStatus::BAD_STATE,
            Self::Name => LanguageRuntimeStatus::INVALID_ARGUMENT,
            Self::Capacity => LanguageRuntimeStatus::NO_CAPACITY,
        }
    }
}

/// 验证通用的 ABI 版本和精确结构尺寸。
pub fn validate_header(
    abi_version: u16,
    struct_size: u16,
    expected_version: u16,
    expected_size: usize,
) -> ValidationResult {
    if abi_version != expected_version {
        return Err(LanguageValidationError::AbiVersion);
    }
    if struct_size as usize != expected_size || expected_size > u16::MAX as usize {
        return Err(LanguageValidationError::StructSize);
    }
    Ok(())
}

/// 验证一个 `repr(C)` 类型的 ABI 版本、尺寸和自身布局。
pub fn validate_typed_header<T>(
    abi_version: u16,
    struct_size: u16,
    expected_version: u16,
) -> ValidationResult {
    validate_header(abi_version, struct_size, expected_version, size_of::<T>())
}

/// 验证 flags 没有设置 V1 未定义的位。
pub const fn validate_flags(flags: u32, mask: u32) -> ValidationResult {
    if flags & !mask == 0 {
        Ok(())
    } else {
        Err(LanguageValidationError::Flags)
    }
}

/// 验证必须为零的保留字段。
pub const fn validate_reserved(value: u64) -> ValidationResult {
    if value == 0 {
        Ok(())
    } else {
        Err(LanguageValidationError::Reserved)
    }
}

/// 验证非零运行时 ID。
pub const fn validate_identifier(value: u64) -> ValidationResult {
    if value != 0 {
        Ok(())
    } else {
        Err(LanguageValidationError::Identifier)
    }
}

/// 验证 opaque 句柄的 slot 和 generation。
pub const fn validate_handle(handle: LanguageHandle) -> ValidationResult {
    if handle.is_valid() {
        Ok(())
    } else {
        Err(LanguageValidationError::Handle)
    }
}

/// 验证 owner 的 cell/generation 均为非零。
pub const fn validate_owner(cell_id: u64, generation: u64) -> ValidationResult {
    if cell_id != 0 && generation != 0 {
        Ok(())
    } else {
        Err(LanguageValidationError::Owner)
    }
}

/// 验证 wire 中声明的 owner 与受信任调用上下文完全一致。
///
/// `expected_*` 必须来自 ELM managed call 上下文，不能来自同一个不可信 payload。
pub const fn validate_expected_owner(
    cell_id: u64,
    generation: u64,
    expected_cell_id: u64,
    expected_generation: u64,
) -> ValidationResult {
    if cell_id == expected_cell_id
        && generation == expected_generation
        && cell_id != 0
        && generation != 0
    {
        Ok(())
    } else {
        Err(LanguageValidationError::Owner)
    }
}

/// 验证固定内联载荷长度。
pub const fn validate_payload_length(length: u16, capacity: usize) -> ValidationResult {
    if (length as usize) <= capacity {
        Ok(())
    } else {
        Err(LanguageValidationError::PayloadLength)
    }
}

/// 验证固定 ASCII 名称及其尾部零填充。
pub fn validate_name(name: &[u8], length: u16) -> ValidationResult {
    let length = length as usize;
    if length == 0 || length > name.len() {
        return Err(LanguageValidationError::Name);
    }
    if name[..length]
        .iter()
        .any(|byte| !matches!(*byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(LanguageValidationError::Name);
    }
    if name[length..].iter().any(|byte| *byte != 0) {
        return Err(LanguageValidationError::Name);
    }
    Ok(())
}

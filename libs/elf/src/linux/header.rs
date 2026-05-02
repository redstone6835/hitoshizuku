//! ELF Ehdr 校验。
//!
//! 只做"形状校验"：magic / class / data。`e_type` 和 `e_machine` 在 [`super::parse`]
//! 里读到再单独判定，因为 `Arch::Unknown` 也算解析成功（loader 决定是否拒绝）。

use crate::error::ElfError;

use super::raw::{
    EI_CLASS, EI_DATA, EI_MAG0, EI_MAG3, ELFCLASS64, ELFDATA2LSB, ELFMAG, ET_DYN, ET_EXEC,
};

/// 校验 e_ident 字段。
pub(super) fn validate_ident(ident: &[u8; 16]) -> Result<(), ElfError> {
    if ident[EI_MAG0..=EI_MAG3] != ELFMAG {
        return Err(ElfError::BadMagic);
    }
    if ident[EI_CLASS] != ELFCLASS64 {
        return Err(ElfError::UnsupportedClass);
    }
    if ident[EI_DATA] != ELFDATA2LSB {
        return Err(ElfError::UnsupportedData);
    }
    Ok(())
}

/// 接受 `ET_EXEC` 与 `ET_DYN`，其它一律拒绝（relocatable / core dump 不在
/// loader 范围内）。
pub(super) fn accept_type(ty: u16) -> Result<(), ElfError> {
    match ty {
        ET_EXEC | ET_DYN => Ok(()),
        other => Err(ElfError::UnsupportedType(other)),
    }
}

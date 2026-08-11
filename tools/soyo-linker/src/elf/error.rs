use std::fmt;
use std::path::{Path, PathBuf};

/// ELF 对象读取失败的稳定分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfErrorKind {
    FileTooLarge,
    Truncated,
    InvalidMagic,
    UnsupportedClass,
    UnsupportedEndian,
    UnsupportedVersion,
    UnsupportedType,
    UnsupportedMachine,
    InvalidHeader,
    TooManySections,
    InvalidSectionTable,
    InvalidSection,
    InvalidSectionLink,
    InvalidString,
    TooManySymbols,
    InvalidSymbolTable,
    InvalidSymbol,
    TooManyRelocations,
    InvalidRelocation,
}

/// 带输入路径的 ELF 对象错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfError {
    path: PathBuf,
    kind: ElfErrorKind,
}

impl ElfError {
    pub(crate) fn new(path: &Path, kind: ElfErrorKind) -> Self {
        Self {
            path: path.to_path_buf(),
            kind,
        }
    }

    pub const fn kind(&self) -> ElfErrorKind {
        self.kind
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Display for ElfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path.display(), self.kind)
    }
}

impl fmt::Display for ElfErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::FileTooLarge => "ELF 对象超过大小上限",
            Self::Truncated => "ELF 对象被截断",
            Self::InvalidMagic => "ELF magic 无效",
            Self::UnsupportedClass => "只支持 ELF64",
            Self::UnsupportedEndian => "只支持小端 ELF",
            Self::UnsupportedVersion => "ELF 版本不受支持",
            Self::UnsupportedType => "只支持 ET_REL 对象",
            Self::UnsupportedMachine => "ELF 目标架构不受支持",
            Self::InvalidHeader => "ELF Header 无效",
            Self::TooManySections => "ELF section 数量超过上限",
            Self::InvalidSectionTable => "ELF section table 无效",
            Self::InvalidSection => "ELF section 无效",
            Self::InvalidSectionLink => "ELF section link 无效",
            Self::InvalidString => "ELF string table 无效",
            Self::TooManySymbols => "ELF symbol 数量超过上限",
            Self::InvalidSymbolTable => "ELF symbol table 无效",
            Self::InvalidSymbol => "ELF symbol 无效",
            Self::TooManyRelocations => "ELF relocation 数量超过上限",
            Self::InvalidRelocation => "ELF relocation 无效",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ElfError {}

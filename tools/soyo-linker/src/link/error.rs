use std::fmt;
use std::path::PathBuf;

use crate::elf::ElfError;

/// 静态链接失败的稳定分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkErrorKind {
    MalformedObject,
    TargetMismatch,
    InvalidSection,
    InvalidSectionAlignment,
    WritableExecutableSection,
    UnsupportedSection,
    WeakSymbol,
    CommonSymbol,
    InvalidSymbol,
    DuplicateSymbol,
    UndefinedSymbol,
    EntryNotFound,
    EntryNotCode,
    ImageTooLarge,
    InvalidRelocation,
    UnsupportedRelocation,
    RelocationOverflow,
}

/// 包含输入位置和诊断上下文的链接错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkError {
    kind: LinkErrorKind,
    path: Option<PathBuf>,
    detail: String,
}

impl LinkError {
    pub(crate) fn new(kind: LinkErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            path: None,
            detail: detail.into(),
        }
    }

    pub(crate) fn in_object(
        kind: LinkErrorKind,
        path: impl Into<PathBuf>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            path: Some(path.into()),
            detail: detail.into(),
        }
    }

    pub(crate) fn from_elf(error: ElfError) -> Self {
        Self {
            kind: LinkErrorKind::MalformedObject,
            path: Some(error.path().to_path_buf()),
            detail: error.kind().to_string(),
        }
    }

    pub const fn kind(&self) -> LinkErrorKind {
        self.kind
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for LinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            write!(formatter, "{}: {}", path.display(), self.detail)
        } else {
            formatter.write_str(&self.detail)
        }
    }
}

impl std::error::Error for LinkError {}

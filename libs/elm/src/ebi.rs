//! EBI 二进制装载接口协议。
//!
//! EBI 不是文件格式。未来的 `soyo` 文件解析器需要把文件内容转换成
//! 这里定义的协议对象，ELM Core 只消费这些对象，不理解容器布局。

use alloc::string::String;
use alloc::vec::Vec;

use crate::manifest::ElmManifest;
use crate::menu::{
    ELM_MENU_DESCRIPTION_LEN, ELM_MENU_LABEL_LEN, ELM_MENU_ROUTE_LEN, ElmMenuItemKind,
};

pub const ELM_EBI_ABI_VERSION: u16 = 1;
pub const ELM_EBI_MAX_SEGMENTS: usize = 32;

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
pub enum ElmEbiSegmentKind {
    Code = 1,
    ReadOnlyData = 2,
    Data = 3,
    Bss = 4,
    Relocation = 5,
    Note = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ElmEbiLoadStatus {
    Ok = 0,
    InvalidUnit = -1,
    UnsupportedAbi = -2,
    InvalidTarget = -3,
    InvalidSegment = -4,
    ArchMismatch = -5,
    InvalidManifest = -6,
    InvalidMenu = -7,
    NativeCodeTodo = -4096,
    RuntimeRejected = -4097,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiTarget {
    pub arch: ElmEbiArch,
    pub abi_version: u16,
    pub min_core_version: u16,
}

impl ElmEbiTarget {
    pub const fn new(arch: ElmEbiArch) -> Self {
        Self {
            arch,
            abi_version: ELM_EBI_ABI_VERSION,
            min_core_version: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiSegment {
    pub kind: ElmEbiSegmentKind,
    pub size: u64,
    pub flags: u32,
}

impl ElmEbiSegment {
    pub const fn new(kind: ElmEbiSegmentKind, size: u64, flags: u32) -> Self {
        Self { kind, size, flags }
    }

    pub const fn requires_native_loader(&self) -> bool {
        matches!(
            self.kind,
            ElmEbiSegmentKind::Code | ElmEbiSegmentKind::Relocation
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiEntry {
    pub symbol: String,
}

impl ElmEbiEntry {
    pub fn new(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiMenuDecl {
    pub kind: ElmMenuItemKind,
    pub flags: u32,
    pub label: String,
    pub description: String,
    pub route: String,
}

impl ElmEbiMenuDecl {
    pub fn new(
        kind: ElmMenuItemKind,
        flags: u32,
        label: impl Into<String>,
        description: impl Into<String>,
        route: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            flags,
            label: label.into(),
            description: description.into(),
            route: route.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiUnit {
    pub manifest: ElmManifest,
    pub target: ElmEbiTarget,
    pub menu: Option<ElmEbiMenuDecl>,
    pub segments: Vec<ElmEbiSegment>,
    pub entry: Option<ElmEbiEntry>,
}

impl ElmEbiUnit {
    pub fn new(manifest: ElmManifest, target: ElmEbiTarget) -> Self {
        Self {
            manifest,
            target,
            menu: None,
            segments: Vec::new(),
            entry: None,
        }
    }

    pub fn with_menu(mut self, menu: ElmEbiMenuDecl) -> Self {
        self.menu = Some(menu);
        self
    }

    pub fn with_segment(mut self, segment: ElmEbiSegment) -> Self {
        self.segments.push(segment);
        self
    }

    pub fn with_entry(mut self, entry: ElmEbiEntry) -> Self {
        self.entry = Some(entry);
        self
    }

    pub fn validate(&self, expected_arch: ElmEbiArch) -> Result<(), ElmEbiLoadStatus> {
        if self.target.abi_version != ELM_EBI_ABI_VERSION {
            return Err(ElmEbiLoadStatus::UnsupportedAbi);
        }
        if !self.target.arch.matches(expected_arch) {
            return Err(ElmEbiLoadStatus::ArchMismatch);
        }
        if self.target.min_core_version == 0 {
            return Err(ElmEbiLoadStatus::InvalidTarget);
        }
        if self.segments.len() > ELM_EBI_MAX_SEGMENTS {
            return Err(ElmEbiLoadStatus::InvalidSegment);
        }
        for segment in &self.segments {
            if segment.size == 0 {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
        }
        if let Some(entry) = &self.entry {
            if entry.symbol.is_empty() {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
        }
        if let Some(menu) = &self.menu {
            validate_menu(menu)?;
        }
        Ok(())
    }

    pub fn has_native_code(&self) -> bool {
        self.entry.is_some()
            || self
                .segments
                .iter()
                .any(ElmEbiSegment::requires_native_loader)
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

fn validate_menu(menu: &ElmEbiMenuDecl) -> Result<(), ElmEbiLoadStatus> {
    if menu.label.is_empty()
        || menu.label.len() > ELM_MENU_LABEL_LEN
        || menu.description.len() > ELM_MENU_DESCRIPTION_LEN
        || menu.route.is_empty()
        || menu.route.len() > ELM_MENU_ROUTE_LEN
    {
        return Err(ElmEbiLoadStatus::InvalidMenu);
    }
    Ok(())
}

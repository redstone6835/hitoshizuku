//! EBI 二进制装载接口协议。
//!
//! EBI 不是文件格式。EKI、未来的 soyo profile、启动期内建对象或测试内存对象
//! 都可以作为 EBI Source 产出这里定义的协议对象；ELM Core 只消费这些对象，
//! 不理解任何具体镜像或容器布局。

use alloc::string::String;
use alloc::vec::Vec;

use crate::manifest::{ElmManifest, ElmName};
use crate::menu::{
    ELM_MENU_DESCRIPTION_LEN, ELM_MENU_LABEL_LEN, ELM_MENU_ROUTE_LEN, ElmMenuItemKind,
};
use crate::mgr::{ELM_MGR_RELATION_POINT_LEN, ELM_NEXUS_CONTRACT_LEN};
use crate::nexus::{FlowContract, FlowDirection, FlowMode};
use crate::ports::ElmPortAccessPolicy;

pub const ELM_EBI_ABI_VERSION: u16 = 1;
pub const ELM_EBI_SOURCE_ABI_VERSION: u16 = 1;
pub const ELM_EBI_MAX_SEGMENTS: usize = 32;
pub const ELM_EBI_MAX_DEPENDENCIES: usize = 16;
pub const ELM_EBI_MAX_EXTENSION_POINTS: usize = 16;
pub const ELM_EBI_MAX_EXTENSIONS: usize = 16;
pub const ELM_EBI_MAX_PROVIDER_PORTS: usize = 16;
pub const ELM_EBI_MAX_IMPORTS: usize = 64;
pub const ELM_EBI_MAX_EXPORTS: usize = 64;
pub const ELM_EBI_NAME_LEN: usize = 128;
pub const ELM_EBI_SYMBOL_NAME_LEN: usize = 128;
pub const ELM_EBI_SOURCE_FLAG_NONE: u32 = 0;
pub const ELM_EBI_SEGMENT_SOURCE_NONE: u32 = u32::MAX;
pub const ELM_EBI_SEGMENT_FLAG_READ: u32 = 1 << 0;
pub const ELM_EBI_SEGMENT_FLAG_WRITE: u32 = 1 << 1;
pub const ELM_EBI_SEGMENT_FLAG_EXECUTE: u32 = 1 << 2;
pub const ELM_EBI_SEGMENT_FLAG_ZERO_FILL: u32 = 1 << 3;
pub const ELM_EBI_SEGMENT_FLAG_RELOCATION_INPUT: u32 = 1 << 4;
pub const ELM_EBI_SYMBOL_FLAG_NONE: u32 = 0;
pub const ELM_EBI_RUST_ABI_VERSION: u16 = 1;
pub const ELM_EBI_HOOK_FLAG_NONE: u32 = 0;
pub const ELM_EBI_HOOK_ON_INITIALIZE: &str = "on_initialize";
pub const ELM_EBI_HOOK_ON_FINALIZE: &str = "on_finalize";

const ELM_EBI_SEGMENT_FLAG_MASK: u32 = ELM_EBI_SEGMENT_FLAG_READ
    | ELM_EBI_SEGMENT_FLAG_WRITE
    | ELM_EBI_SEGMENT_FLAG_EXECUTE
    | ELM_EBI_SEGMENT_FLAG_ZERO_FILL
    | ELM_EBI_SEGMENT_FLAG_RELOCATION_INPUT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ElmEbiSourceKind {
    Eki = 1,
    SoyoProfile = 2,
    Builtin = 3,
    Memory = 4,
    Remote = 5,
}

impl ElmEbiSourceKind {
    pub const fn from_raw(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::Eki),
            2 => Some(Self::SoyoProfile),
            3 => Some(Self::Builtin),
            4 => Some(Self::Memory),
            5 => Some(Self::Remote),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmEbiSourceRequest {
    pub abi_version: u16,
    pub source_kind: u16,
    pub flags: u32,
    pub payload_len: u32,
    pub reserved: u32,
}

impl ElmEbiSourceRequest {
    pub const fn new(kind: ElmEbiSourceKind, payload_len: u32) -> Self {
        Self {
            abi_version: ELM_EBI_SOURCE_ABI_VERSION,
            source_kind: kind as u16,
            flags: ELM_EBI_SOURCE_FLAG_NONE,
            payload_len,
            reserved: 0,
        }
    }
}

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
#[repr(u32)]
pub enum ElmEbiLifecycleHookKind {
    Initialize = 1,
    Finalize = 2,
}

impl ElmEbiLifecycleHookKind {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Initialize),
            2 => Some(Self::Finalize),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ElmEbiRustHookSignature {
    ContextResult = 1,
}

impl ElmEbiRustHookSignature {
    pub const fn from_raw(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::ContextResult),
            _ => None,
        }
    }
}

impl ElmEbiSegmentKind {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Code),
            2 => Some(Self::ReadOnlyData),
            3 => Some(Self::Data),
            4 => Some(Self::Bss),
            5 => Some(Self::Relocation),
            6 => Some(Self::Note),
            _ => None,
        }
    }
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
    pub file_size: u64,
    pub mem_size: u64,
    pub align: u64,
    pub source_index: u32,
    pub source_offset: u64,
    pub content_hash: u64,
}

impl ElmEbiSegment {
    pub const fn new(kind: ElmEbiSegmentKind, size: u64, flags: u32) -> Self {
        let effective_flags = if flags == 0 {
            default_segment_flags(kind)
        } else {
            flags
        };
        let file_size = if matches!(kind, ElmEbiSegmentKind::Bss) {
            0
        } else {
            size
        };
        Self {
            kind,
            size,
            flags: effective_flags,
            file_size,
            mem_size: size,
            align: 0,
            source_index: ELM_EBI_SEGMENT_SOURCE_NONE,
            source_offset: 0,
            content_hash: 0,
        }
    }

    pub const fn from_payload(
        kind: ElmEbiSegmentKind,
        flags: u32,
        file_size: u64,
        mem_size: u64,
        align: u64,
        source_index: u32,
        source_offset: u64,
        content_hash: u64,
    ) -> Self {
        let effective_flags = if flags == 0 {
            default_segment_flags(kind)
        } else {
            flags
        };
        Self {
            kind,
            size: mem_size,
            flags: effective_flags,
            file_size,
            mem_size,
            align,
            source_index,
            source_offset,
            content_hash,
        }
    }

    pub const fn requires_native_loader(&self) -> bool {
        matches!(
            self.kind,
            ElmEbiSegmentKind::Code | ElmEbiSegmentKind::Relocation
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiImportDecl {
    pub name: String,
    pub contract: FlowContract,
    pub version: u32,
    pub flags: u32,
}

impl ElmEbiImportDecl {
    pub fn new(
        name: impl Into<String>,
        contract: impl Into<String>,
        version: u32,
        flags: u32,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let name = name.into();
        validate_symbol_name(&name)?;
        Ok(Self {
            name,
            contract: FlowContract::new(contract.into())
                .map_err(|_| ElmEbiLoadStatus::InvalidManifest)?,
            version,
            flags,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiExportDecl {
    pub name: String,
    pub contract: FlowContract,
    pub version: u32,
    pub flags: u32,
}

impl ElmEbiExportDecl {
    pub fn new(
        name: impl Into<String>,
        contract: impl Into<String>,
        version: u32,
        flags: u32,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let name = name.into();
        validate_symbol_name(&name)?;
        Ok(Self {
            name,
            contract: FlowContract::new(contract.into())
                .map_err(|_| ElmEbiLoadStatus::InvalidManifest)?,
            version,
            flags,
        })
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
pub struct ElmEbiLifecycleHookDecl {
    pub kind: ElmEbiLifecycleHookKind,
    pub symbol: String,
    pub rust_abi_version: u16,
    pub signature: ElmEbiRustHookSignature,
    pub flags: u32,
}

impl ElmEbiLifecycleHookDecl {
    pub fn new(
        kind: ElmEbiLifecycleHookKind,
        symbol: impl Into<String>,
        rust_abi_version: u16,
        signature: ElmEbiRustHookSignature,
        flags: u32,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let symbol = symbol.into();
        validate_symbol_name(&symbol)?;
        Ok(Self {
            kind,
            symbol,
            rust_abi_version,
            signature,
            flags,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiLifecycleHooks {
    pub initialize: ElmEbiLifecycleHookDecl,
    pub finalize: ElmEbiLifecycleHookDecl,
}

impl ElmEbiLifecycleHooks {
    pub fn new(
        initialize: ElmEbiLifecycleHookDecl,
        finalize: ElmEbiLifecycleHookDecl,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let hooks = Self {
            initialize,
            finalize,
        };
        validate_lifecycle_hooks(Some(&hooks))?;
        Ok(hooks)
    }

    pub fn rust_context_result_v1() -> Self {
        Self {
            initialize: ElmEbiLifecycleHookDecl {
                kind: ElmEbiLifecycleHookKind::Initialize,
                symbol: String::from(ELM_EBI_HOOK_ON_INITIALIZE),
                rust_abi_version: ELM_EBI_RUST_ABI_VERSION,
                signature: ElmEbiRustHookSignature::ContextResult,
                flags: ELM_EBI_HOOK_FLAG_NONE,
            },
            finalize: ElmEbiLifecycleHookDecl {
                kind: ElmEbiLifecycleHookKind::Finalize,
                symbol: String::from(ELM_EBI_HOOK_ON_FINALIZE),
                rust_abi_version: ELM_EBI_RUST_ABI_VERSION,
                signature: ElmEbiRustHookSignature::ContextResult,
                flags: ELM_EBI_HOOK_FLAG_NONE,
            },
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
pub struct ElmEbiDependencyDecl {
    pub provider_name: String,
    pub contract: FlowContract,
}

impl ElmEbiDependencyDecl {
    pub fn new(
        provider_name: impl Into<String>,
        contract: impl Into<String>,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let provider_name = provider_name.into();
        validate_ebi_name(&provider_name)?;
        Ok(Self {
            provider_name,
            contract: FlowContract::new(contract.into())
                .map_err(|_| ElmEbiLoadStatus::InvalidManifest)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiExtensionPointDecl {
    pub point: String,
    pub contract: FlowContract,
}

impl ElmEbiExtensionPointDecl {
    pub fn new(
        point: impl Into<String>,
        contract: impl Into<String>,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let point = point.into();
        validate_ebi_point(&point)?;
        Ok(Self {
            point,
            contract: FlowContract::new(contract.into())
                .map_err(|_| ElmEbiLoadStatus::InvalidManifest)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiExtensionDecl {
    pub target_name: String,
    pub point: String,
    pub contract: FlowContract,
}

impl ElmEbiExtensionDecl {
    pub fn new(
        target_name: impl Into<String>,
        point: impl Into<String>,
        contract: impl Into<String>,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let target_name = target_name.into();
        let point = point.into();
        validate_ebi_name(&target_name)?;
        validate_ebi_point(&point)?;
        Ok(Self {
            target_name,
            point,
            contract: FlowContract::new(contract.into())
                .map_err(|_| ElmEbiLoadStatus::InvalidManifest)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiProviderPortDecl {
    pub contract: FlowContract,
    pub access: ElmPortAccessPolicy,
    pub direction: FlowDirection,
    pub mode: FlowMode,
    pub flags: u32,
}

impl ElmEbiProviderPortDecl {
    pub fn new(
        contract: impl Into<String>,
        access: ElmPortAccessPolicy,
        direction: FlowDirection,
        mode: FlowMode,
        flags: u32,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let contract =
            FlowContract::new(contract.into()).map_err(|_| ElmEbiLoadStatus::InvalidManifest)?;
        Ok(Self {
            contract,
            access,
            direction,
            mode,
            flags,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiUnit {
    pub manifest: ElmManifest,
    pub target: ElmEbiTarget,
    pub menu: Option<ElmEbiMenuDecl>,
    pub segments: Vec<ElmEbiSegment>,
    pub entry: Option<ElmEbiEntry>,
    pub dependencies: Vec<ElmEbiDependencyDecl>,
    pub extension_points: Vec<ElmEbiExtensionPointDecl>,
    pub extensions: Vec<ElmEbiExtensionDecl>,
    pub provider_ports: Vec<ElmEbiProviderPortDecl>,
    pub imports: Vec<ElmEbiImportDecl>,
    pub exports: Vec<ElmEbiExportDecl>,
    pub lifecycle_hooks: Option<ElmEbiLifecycleHooks>,
}

impl ElmEbiUnit {
    pub fn new(manifest: ElmManifest, target: ElmEbiTarget) -> Self {
        Self {
            manifest,
            target,
            menu: None,
            segments: Vec::new(),
            entry: None,
            dependencies: Vec::new(),
            extension_points: Vec::new(),
            extensions: Vec::new(),
            provider_ports: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
            lifecycle_hooks: None,
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

    pub fn with_dependency(mut self, dependency: ElmEbiDependencyDecl) -> Self {
        self.dependencies.push(dependency);
        self
    }

    pub fn with_extension_point(mut self, point: ElmEbiExtensionPointDecl) -> Self {
        self.extension_points.push(point);
        self
    }

    pub fn with_extension(mut self, extension: ElmEbiExtensionDecl) -> Self {
        self.extensions.push(extension);
        self
    }

    pub fn with_provider_port(mut self, provider: ElmEbiProviderPortDecl) -> Self {
        self.provider_ports.push(provider);
        self
    }

    pub fn with_import(mut self, import: ElmEbiImportDecl) -> Self {
        self.imports.push(import);
        self
    }

    pub fn with_export(mut self, export: ElmEbiExportDecl) -> Self {
        self.exports.push(export);
        self
    }

    pub fn with_lifecycle_hooks(mut self, hooks: ElmEbiLifecycleHooks) -> Self {
        self.lifecycle_hooks = Some(hooks);
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
        if self.dependencies.len() > ELM_EBI_MAX_DEPENDENCIES
            || self.extension_points.len() > ELM_EBI_MAX_EXTENSION_POINTS
            || self.extensions.len() > ELM_EBI_MAX_EXTENSIONS
            || self.provider_ports.len() > ELM_EBI_MAX_PROVIDER_PORTS
            || self.imports.len() > ELM_EBI_MAX_IMPORTS
            || self.exports.len() > ELM_EBI_MAX_EXPORTS
        {
            return Err(ElmEbiLoadStatus::InvalidManifest);
        }
        for segment in &self.segments {
            validate_segment(segment)?;
        }
        if let Some(entry) = &self.entry {
            if validate_symbol_name(&entry.symbol).is_err() {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
        }
        if let Some(menu) = &self.menu {
            validate_menu(menu)?;
        }
        for dependency in &self.dependencies {
            validate_ebi_name(&dependency.provider_name)?;
            validate_contract_len(&dependency.contract)?;
        }
        for point in &self.extension_points {
            validate_ebi_point(&point.point)?;
            validate_contract_len(&point.contract)?;
        }
        for extension in &self.extensions {
            validate_ebi_name(&extension.target_name)?;
            validate_ebi_point(&extension.point)?;
            validate_contract_len(&extension.contract)?;
        }
        for provider in &self.provider_ports {
            if provider.flags != 0 {
                return Err(ElmEbiLoadStatus::InvalidManifest);
            }
            validate_contract_len(&provider.contract)?;
        }
        for import in &self.imports {
            validate_symbol_decl(&import.name, &import.contract, import.flags)?;
        }
        for export in &self.exports {
            validate_symbol_decl(&export.name, &export.contract, export.flags)?;
        }
        validate_lifecycle_hooks(self.lifecycle_hooks.as_ref())?;
        Ok(())
    }

    pub fn has_native_code(&self) -> bool {
        self.lifecycle_hooks.is_some()
            || self.entry.is_some()
            || self
                .segments
                .iter()
                .any(ElmEbiSegment::requires_native_loader)
    }
}

pub const fn default_segment_flags(kind: ElmEbiSegmentKind) -> u32 {
    match kind {
        ElmEbiSegmentKind::Code => ELM_EBI_SEGMENT_FLAG_READ | ELM_EBI_SEGMENT_FLAG_EXECUTE,
        ElmEbiSegmentKind::ReadOnlyData | ElmEbiSegmentKind::Note => ELM_EBI_SEGMENT_FLAG_READ,
        ElmEbiSegmentKind::Data => ELM_EBI_SEGMENT_FLAG_READ | ELM_EBI_SEGMENT_FLAG_WRITE,
        ElmEbiSegmentKind::Bss => {
            ELM_EBI_SEGMENT_FLAG_READ | ELM_EBI_SEGMENT_FLAG_WRITE | ELM_EBI_SEGMENT_FLAG_ZERO_FILL
        }
        ElmEbiSegmentKind::Relocation => {
            ELM_EBI_SEGMENT_FLAG_READ | ELM_EBI_SEGMENT_FLAG_RELOCATION_INPUT
        }
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

fn validate_ebi_name(name: &str) -> Result<(), ElmEbiLoadStatus> {
    if name.len() > ELM_EBI_NAME_LEN || ElmName::new(name).is_err() {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    Ok(())
}

fn validate_ebi_point(point: &str) -> Result<(), ElmEbiLoadStatus> {
    if point.is_empty()
        || point.len() > ELM_MGR_RELATION_POINT_LEN
        || !point.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    Ok(())
}

fn validate_segment(segment: &ElmEbiSegment) -> Result<(), ElmEbiLoadStatus> {
    if segment.size == 0
        || segment.mem_size == 0
        || segment.size != segment.mem_size
        || segment.file_size > segment.mem_size
        || segment.flags & !ELM_EBI_SEGMENT_FLAG_MASK != 0
        || (segment.align != 0 && !segment.align.is_power_of_two())
    {
        return Err(ElmEbiLoadStatus::InvalidSegment);
    }
    match segment.kind {
        ElmEbiSegmentKind::Code => {
            if segment.file_size == 0
                || segment.file_size != segment.mem_size
                || segment.flags & ELM_EBI_SEGMENT_FLAG_EXECUTE == 0
                || segment.flags & ELM_EBI_SEGMENT_FLAG_WRITE != 0
                || segment.flags & ELM_EBI_SEGMENT_FLAG_ZERO_FILL != 0
            {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
        }
        ElmEbiSegmentKind::ReadOnlyData | ElmEbiSegmentKind::Note => {
            if segment.file_size == 0
                || segment.file_size != segment.mem_size
                || segment.flags & (ELM_EBI_SEGMENT_FLAG_WRITE | ELM_EBI_SEGMENT_FLAG_EXECUTE) != 0
                || segment.flags & ELM_EBI_SEGMENT_FLAG_READ == 0
            {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
        }
        ElmEbiSegmentKind::Data => {
            if segment.file_size == 0
                || segment.flags & ELM_EBI_SEGMENT_FLAG_WRITE == 0
                || segment.flags & ELM_EBI_SEGMENT_FLAG_EXECUTE != 0
                || segment.flags & ELM_EBI_SEGMENT_FLAG_ZERO_FILL != 0
            {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
        }
        ElmEbiSegmentKind::Bss => {
            if segment.file_size != 0
                || segment.content_hash != 0
                || segment.flags & ELM_EBI_SEGMENT_FLAG_ZERO_FILL == 0
                || segment.flags & ELM_EBI_SEGMENT_FLAG_EXECUTE != 0
            {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
        }
        ElmEbiSegmentKind::Relocation => {
            if segment.file_size == 0
                || segment.file_size != segment.mem_size
                || segment.flags & ELM_EBI_SEGMENT_FLAG_RELOCATION_INPUT == 0
                || segment.flags & (ELM_EBI_SEGMENT_FLAG_WRITE | ELM_EBI_SEGMENT_FLAG_EXECUTE) != 0
            {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
        }
    }
    Ok(())
}

fn validate_symbol_decl(
    name: &str,
    contract: &FlowContract,
    flags: u32,
) -> Result<(), ElmEbiLoadStatus> {
    validate_symbol_name(name)?;
    validate_contract_len(contract)?;
    if flags != ELM_EBI_SYMBOL_FLAG_NONE {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    Ok(())
}

fn validate_lifecycle_hooks(hooks: Option<&ElmEbiLifecycleHooks>) -> Result<(), ElmEbiLoadStatus> {
    let Some(hooks) = hooks else {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    };
    validate_lifecycle_hook(
        &hooks.initialize,
        ElmEbiLifecycleHookKind::Initialize,
        ELM_EBI_HOOK_ON_INITIALIZE,
    )?;
    validate_lifecycle_hook(
        &hooks.finalize,
        ElmEbiLifecycleHookKind::Finalize,
        ELM_EBI_HOOK_ON_FINALIZE,
    )?;
    Ok(())
}

fn validate_lifecycle_hook(
    hook: &ElmEbiLifecycleHookDecl,
    expected_kind: ElmEbiLifecycleHookKind,
    expected_symbol: &str,
) -> Result<(), ElmEbiLoadStatus> {
    if hook.kind != expected_kind
        || hook.symbol != expected_symbol
        || hook.rust_abi_version != ELM_EBI_RUST_ABI_VERSION
        || hook.signature != ElmEbiRustHookSignature::ContextResult
        || hook.flags != ELM_EBI_HOOK_FLAG_NONE
    {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    validate_symbol_name(&hook.symbol)?;
    Ok(())
}

fn validate_symbol_name(name: &str) -> Result<(), ElmEbiLoadStatus> {
    if name.is_empty()
        || name.len() > ELM_EBI_SYMBOL_NAME_LEN
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'@' | b':')
        })
    {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    Ok(())
}

fn validate_contract_len(contract: &FlowContract) -> Result<(), ElmEbiLoadStatus> {
    if contract.as_str().len() > ELM_NEXUS_CONTRACT_LEN {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    Ok(())
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

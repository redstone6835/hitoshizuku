//! EBI 二进制装载接口协议。
//!
//! EBI 不是文件格式。EKI、未来的投影产物、启动期内建对象或测试内存对象
//! 都可以通过 EBI Source 产出这里定义的协议对象；ELM Core 只消费这些对象，
//! 不理解任何具体镜像或容器布局。当前内核把 EKI 投影能力归属到内建 `eki`
//! 子单元，而不是让根管理器或 Core 直接拥有某种文件格式。

use alloc::string::String;
use alloc::vec::Vec;

pub use crate::ebi_wire::*;
use crate::elmapi::{
    ELM_API_MAX_COMPATIBLE_VERSIONS, ELM_API_ROOT_IMPORT_CONTRACT, ELM_API_ROOT_IMPORT_NAME,
};
use crate::manifest::{ElmManifest, ElmName};
use crate::menu::{
    ELM_MENU_DESCRIPTION_LEN, ELM_MENU_LABEL_LEN, ELM_MENU_ROUTE_LEN, ElmMenuItemKind,
};
use crate::mgr::{ELM_MGR_RELATION_POINT_LEN, ELM_NEXUS_CONTRACT_LEN};
use crate::nexus::{FlowContract, FlowDirection, FlowMode};
use crate::proof::{ElmEbiProofV1, ElmRustAbiFingerprintV1, canonical_ebi_digest};
use crate::wire::{ElmMixinMode, ElmPortAccessPolicy};

pub const ELM_EBI_ABI_VERSION: u16 = 1;
pub const ELM_EBI_MAX_SEGMENTS: usize = 32;
pub const ELM_EBI_MAX_DEPENDENCIES: usize = 16;
pub const ELM_EBI_MAX_EXTENSION_POINTS: usize = 16;
pub const ELM_EBI_MAX_EXTENSIONS: usize = 16;
pub const ELM_EBI_MAX_PROVIDER_PORTS: usize = 16;
pub const ELM_EBI_MAX_IMPORTS: usize = 64;
pub const ELM_EBI_MAX_EXPORTS: usize = 64;
pub const ELM_EBI_MAX_SYMBOL_LOCATIONS: usize = 128;
pub const ELM_EBI_MAX_RELOCATIONS: usize = 512;
pub const ELM_EBI_NAME_LEN: usize = 128;
pub const ELM_EBI_SYMBOL_NAME_LEN: usize = 128;
pub const ELM_EBI_SEGMENT_SOURCE_NONE: u32 = u32::MAX;
pub const ELM_EBI_SEGMENT_FLAG_READ: u32 = 1 << 0;
pub const ELM_EBI_SEGMENT_FLAG_WRITE: u32 = 1 << 1;
pub const ELM_EBI_SEGMENT_FLAG_EXECUTE: u32 = 1 << 2;
pub const ELM_EBI_SEGMENT_FLAG_ZERO_FILL: u32 = 1 << 3;
pub const ELM_EBI_SEGMENT_FLAG_RELOCATION_INPUT: u32 = 1 << 4;
pub const ELM_EBI_SYMBOL_FLAG_NONE: u32 = 0;
pub const ELM_EBI_IMPORT_FLAG_OPTIONAL: u32 = 1 << 0;
pub const ELM_EBI_IMPORT_FLAG_MANAGED: u32 = 1 << 1;
pub const ELM_EBI_IMPORT_FLAG_DIRECT_PINNED: u32 = 1 << 2;
pub const ELM_EBI_IMPORT_FLAG_ALLOW_ANCESTOR: u32 = 1 << 3;
pub const ELM_EBI_IMPORT_FLAG_ALLOW_BUILTIN: u32 = 1 << 4;
pub const ELM_EBI_IMPORT_FLAGS_MASK: u32 = ELM_EBI_IMPORT_FLAG_OPTIONAL
    | ELM_EBI_IMPORT_FLAG_MANAGED
    | ELM_EBI_IMPORT_FLAG_DIRECT_PINNED
    | ELM_EBI_IMPORT_FLAG_ALLOW_ANCESTOR
    | ELM_EBI_IMPORT_FLAG_ALLOW_BUILTIN;
pub const ELM_EBI_EXPORT_FLAG_MANAGED: u32 = 1 << 0;
pub const ELM_EBI_EXPORT_FLAG_DIRECT_PINNED: u32 = 1 << 1;
pub const ELM_EBI_EXPORT_FLAG_PRIVATE: u32 = 1 << 2;
pub const ELM_EBI_EXPORT_FLAG_DEPENDENCY: u32 = 1 << 3;
pub const ELM_EBI_EXPORT_FLAG_SUBTREE: u32 = 1 << 4;
pub const ELM_EBI_EXPORT_FLAGS_MASK: u32 = ELM_EBI_EXPORT_FLAG_MANAGED
    | ELM_EBI_EXPORT_FLAG_DIRECT_PINNED
    | ELM_EBI_EXPORT_FLAG_PRIVATE
    | ELM_EBI_EXPORT_FLAG_DEPENDENCY
    | ELM_EBI_EXPORT_FLAG_SUBTREE;
pub const ELM_EBI_SYMBOL_LOCATION_FLAG_NONE: u32 = 0;
pub const ELM_EBI_RELOCATION_FLAG_NONE: u32 = 0;
pub const ELM_EBI_RUST_ABI_VERSION: u16 = 1;
pub const ELM_EBI_HOOK_FLAG_NONE: u32 = 0;
pub const ELM_EBI_HOOK_ON_INITIALIZE: &str = "on_initialize";
pub const ELM_EBI_HOOK_ON_FINALIZE: &str = "on_finalize";
pub const ELM_EBI_HOOK_ON_QUIESCE: &str = "on_quiesce";
pub const ELM_EBI_HOOK_ON_PAUSE: &str = "on_pause";
pub const ELM_EBI_HOOK_ON_RESUME: &str = "on_resume";
pub const ELM_EBI_HOOK_ON_MIGRATE_EXPORT: &str = "on_migrate_export";
pub const ELM_EBI_HOOK_ON_MIGRATE_IMPORT: &str = "on_migrate_import";
pub const ELM_EBI_HOOK_ON_MIGRATE_ABORT: &str = "on_migrate_abort";
pub const ELM_MIGRATION_STATE_MAX: usize = 64 * 1024;

const ELM_EBI_SEGMENT_FLAG_MASK: u32 = ELM_EBI_SEGMENT_FLAG_READ
    | ELM_EBI_SEGMENT_FLAG_WRITE
    | ELM_EBI_SEGMENT_FLAG_EXECUTE
    | ELM_EBI_SEGMENT_FLAG_ZERO_FILL
    | ELM_EBI_SEGMENT_FLAG_RELOCATION_INPUT;

/// 内建 `eki` 子单元提供的 EKI -> EBI 投影源标识。
///
/// ELM Core 只按 Projection Source 协议调用该标识，不识别 EKI 文件格式本身。
pub const ELM_EKI_PROJECTION_SOURCE_ID: u64 = 0x454b_4900_0000_0001;

/// Projection Source 的随机访问输入。
///
/// Core 只提供只读 reader，不暴露镜像会话或具体容器的存储方式。投影器必须按
/// `read_at` 读取输入，因此内联载荷、分段会话以及后续其他来源使用同一条协议。
pub trait ElmImageReader {
    fn len(&self) -> u64;

    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<(), ElmEbiLoadStatus>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read_all(&self, max_len: usize) -> Result<Vec<u8>, ElmEbiLoadStatus> {
        let len = usize::try_from(self.len()).map_err(|_| ElmEbiLoadStatus::InvalidUnit)?;
        if len > max_len {
            return Err(ElmEbiLoadStatus::InvalidUnit);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
        bytes.resize(len, 0);
        self.read_at(0, &mut bytes)?;
        Ok(bytes)
    }
}

pub struct ElmSliceImageReader<'a> {
    bytes: &'a [u8],
}

impl<'a> ElmSliceImageReader<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
}

impl ElmImageReader for ElmSliceImageReader<'_> {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<(), ElmEbiLoadStatus> {
        let start = usize::try_from(offset).map_err(|_| ElmEbiLoadStatus::InvalidUnit)?;
        let end = start
            .checked_add(output.len())
            .ok_or(ElmEbiLoadStatus::InvalidUnit)?;
        let source = self
            .bytes
            .get(start..end)
            .ok_or(ElmEbiLoadStatus::InvalidUnit)?;
        output.copy_from_slice(source);
        Ok(())
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
    MigrateExport = 3,
    MigrateImport = 4,
    MigrateAbort = 5,
}

impl ElmEbiLifecycleHookKind {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Initialize),
            2 => Some(Self::Finalize),
            3 => Some(Self::MigrateExport),
            4 => Some(Self::MigrateImport),
            5 => Some(Self::MigrateAbort),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmEbiRelocationKind {
    ImageBase64 = 1,
    SegmentBase64 = 2,
    SymbolAbs64 = 3,
    SymbolRel32 = 4,
    SymbolRel64 = 5,
    ImportAbs64 = 6,
    ImportRel32 = 7,
    ImportRel64 = 8,
}

impl ElmEbiRelocationKind {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::ImageBase64),
            2 => Some(Self::SegmentBase64),
            3 => Some(Self::SymbolAbs64),
            4 => Some(Self::SymbolRel32),
            5 => Some(Self::SymbolRel64),
            6 => Some(Self::ImportAbs64),
            7 => Some(Self::ImportRel32),
            8 => Some(Self::ImportRel64),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiTarget {
    pub arch: ElmEbiArch,
    pub abi_version: u16,
    pub min_core_version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiApiCompatibility {
    pub root_import_index: u32,
    pub required_features: u64,
    pub compatible_versions: Vec<u16>,
}

impl ElmEbiApiCompatibility {
    pub fn new(
        root_import_index: u32,
        required_features: u64,
        compatible_versions: impl IntoIterator<Item = u16>,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let compatible_versions: Vec<_> = compatible_versions.into_iter().collect();
        let value = Self {
            root_import_index,
            required_features,
            compatible_versions,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ElmEbiLoadStatus> {
        if self.compatible_versions.is_empty()
            || self.compatible_versions.len() > ELM_API_MAX_COMPATIBLE_VERSIONS
            || self.compatible_versions.iter().any(|version| *version == 0)
        {
            return Err(ElmEbiLoadStatus::InvalidTarget);
        }
        if self
            .compatible_versions
            .windows(2)
            .any(|versions| versions[0] >= versions[1])
        {
            return Err(ElmEbiLoadStatus::InvalidTarget);
        }
        Ok(())
    }

    pub fn select_highest_common(&self, supported: &[u16]) -> Option<u16> {
        self.compatible_versions
            .iter()
            .rev()
            .copied()
            .find(|version| supported.contains(version))
    }
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
    pub min_version: u32,
    pub max_version: u32,
    pub flags: u32,
}

impl ElmEbiImportDecl {
    pub fn new(
        name: impl Into<String>,
        contract: impl Into<String>,
        version: u32,
        flags: u32,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let (min_version, max_version) = if version == 0 {
            (1, u32::MAX)
        } else {
            (version, version)
        };
        Self::new_range(name, contract, min_version, max_version, flags)
    }

    pub fn new_range(
        name: impl Into<String>,
        contract: impl Into<String>,
        min_version: u32,
        max_version: u32,
        flags: u32,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let name = name.into();
        validate_symbol_name(&name)?;
        let value = Self {
            name,
            contract: FlowContract::new(contract.into())
                .map_err(|_| ElmEbiLoadStatus::InvalidManifest)?,
            min_version,
            max_version,
            flags,
        };
        validate_import_decl(&value)?;
        Ok(value)
    }

    pub const fn accepts_version(&self, version: u32) -> bool {
        version >= self.min_version && version <= self.max_version
    }

    pub const fn is_optional(&self) -> bool {
        self.flags & ELM_EBI_IMPORT_FLAG_OPTIONAL != 0
    }

    pub const fn is_managed(&self) -> bool {
        self.flags & ELM_EBI_IMPORT_FLAG_MANAGED != 0
    }

    pub const fn is_direct_pinned(&self) -> bool {
        self.flags & (ELM_EBI_IMPORT_FLAG_MANAGED | ELM_EBI_IMPORT_FLAG_DIRECT_PINNED) == 0
            || self.flags & ELM_EBI_IMPORT_FLAG_DIRECT_PINNED != 0
    }

    pub const fn allows_ancestor(&self) -> bool {
        self.flags & (ELM_EBI_IMPORT_FLAG_ALLOW_ANCESTOR | ELM_EBI_IMPORT_FLAG_ALLOW_BUILTIN) == 0
            || self.flags & ELM_EBI_IMPORT_FLAG_ALLOW_ANCESTOR != 0
    }

    pub const fn allows_builtin(&self) -> bool {
        self.flags & (ELM_EBI_IMPORT_FLAG_ALLOW_ANCESTOR | ELM_EBI_IMPORT_FLAG_ALLOW_BUILTIN) == 0
            || self.flags & ELM_EBI_IMPORT_FLAG_ALLOW_BUILTIN != 0
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
        let value = Self {
            name,
            contract: FlowContract::new(contract.into())
                .map_err(|_| ElmEbiLoadStatus::InvalidManifest)?,
            version,
            flags,
        };
        validate_export_decl(&value)?;
        Ok(value)
    }

    pub const fn is_managed(&self) -> bool {
        self.flags & ELM_EBI_EXPORT_FLAG_MANAGED != 0
    }

    pub const fn is_direct_pinned(&self) -> bool {
        self.flags & (ELM_EBI_EXPORT_FLAG_MANAGED | ELM_EBI_EXPORT_FLAG_DIRECT_PINNED) == 0
            || self.flags & ELM_EBI_EXPORT_FLAG_DIRECT_PINNED != 0
    }

    pub const fn visible_to_dependency(&self) -> bool {
        self.flags
            & (ELM_EBI_EXPORT_FLAG_PRIVATE
                | ELM_EBI_EXPORT_FLAG_DEPENDENCY
                | ELM_EBI_EXPORT_FLAG_SUBTREE)
            == 0
            || self.flags & ELM_EBI_EXPORT_FLAG_DEPENDENCY != 0
    }

    pub const fn visible_to_subtree(&self) -> bool {
        self.flags & ELM_EBI_EXPORT_FLAG_SUBTREE != 0
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
pub struct ElmEbiSegmentPayload {
    pub segment_index: u32,
    pub source_index: u32,
    pub kind: ElmEbiSegmentKind,
    pub file_size: u64,
    pub mem_size: u64,
    pub bytes: Vec<u8>,
}

impl ElmEbiSegmentPayload {
    pub fn new(
        segment_index: u32,
        source_index: u32,
        kind: ElmEbiSegmentKind,
        file_size: u64,
        mem_size: u64,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            segment_index,
            source_index,
            kind,
            file_size,
            mem_size,
            bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiSymbolLocationDecl {
    pub name: String,
    pub segment_index: u32,
    pub offset: u64,
    pub size: u64,
    pub flags: u32,
}

impl ElmEbiSymbolLocationDecl {
    pub fn new(
        name: impl Into<String>,
        segment_index: u32,
        offset: u64,
        size: u64,
        flags: u32,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let name = name.into();
        validate_symbol_name(&name)?;
        if flags != ELM_EBI_SYMBOL_LOCATION_FLAG_NONE || size == 0 {
            return Err(ElmEbiLoadStatus::InvalidManifest);
        }
        Ok(Self {
            name,
            segment_index,
            offset,
            size,
            flags,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiRelocationDecl {
    pub kind: ElmEbiRelocationKind,
    pub flags: u32,
    pub target_segment_index: u32,
    pub target_offset: u64,
    pub value_index: u32,
    pub addend: i64,
}

impl ElmEbiRelocationDecl {
    pub fn new(
        kind: ElmEbiRelocationKind,
        flags: u32,
        target_segment_index: u32,
        target_offset: u64,
        value_index: u32,
        addend: i64,
    ) -> Result<Self, ElmEbiLoadStatus> {
        if flags != ELM_EBI_RELOCATION_FLAG_NONE {
            return Err(ElmEbiLoadStatus::InvalidSegment);
        }
        Ok(Self {
            kind,
            flags,
            target_segment_index,
            target_offset,
            value_index,
            addend,
        })
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
    pub migrate_export: Option<ElmEbiLifecycleHookDecl>,
    pub migrate_import: Option<ElmEbiLifecycleHookDecl>,
    pub migrate_abort: Option<ElmEbiLifecycleHookDecl>,
}

impl ElmEbiLifecycleHooks {
    pub fn new(
        initialize: ElmEbiLifecycleHookDecl,
        finalize: ElmEbiLifecycleHookDecl,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let hooks = Self {
            initialize,
            finalize,
            migrate_export: None,
            migrate_import: None,
            migrate_abort: None,
        };
        validate_lifecycle_hooks(Some(&hooks))?;
        Ok(hooks)
    }

    pub fn with_migration_hooks(
        mut self,
        migrate_export: Option<ElmEbiLifecycleHookDecl>,
        migrate_import: Option<ElmEbiLifecycleHookDecl>,
        migrate_abort: Option<ElmEbiLifecycleHookDecl>,
    ) -> Result<Self, ElmEbiLoadStatus> {
        self.migrate_export = migrate_export;
        self.migrate_import = migrate_import;
        self.migrate_abort = migrate_abort;
        validate_lifecycle_hooks(Some(&self))?;
        Ok(self)
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
            migrate_export: None,
            migrate_import: None,
            migrate_abort: None,
        }
    }

    pub fn rust_context_result_v1_with_migration() -> Self {
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
            migrate_export: Some(ElmEbiLifecycleHookDecl {
                kind: ElmEbiLifecycleHookKind::MigrateExport,
                symbol: String::from(ELM_EBI_HOOK_ON_MIGRATE_EXPORT),
                rust_abi_version: ELM_EBI_RUST_ABI_VERSION,
                signature: ElmEbiRustHookSignature::ContextResult,
                flags: ELM_EBI_HOOK_FLAG_NONE,
            }),
            migrate_import: Some(ElmEbiLifecycleHookDecl {
                kind: ElmEbiLifecycleHookKind::MigrateImport,
                symbol: String::from(ELM_EBI_HOOK_ON_MIGRATE_IMPORT),
                rust_abi_version: ELM_EBI_RUST_ABI_VERSION,
                signature: ElmEbiRustHookSignature::ContextResult,
                flags: ELM_EBI_HOOK_FLAG_NONE,
            }),
            migrate_abort: Some(ElmEbiLifecycleHookDecl {
                kind: ElmEbiLifecycleHookKind::MigrateAbort,
                symbol: String::from(ELM_EBI_HOOK_ON_MIGRATE_ABORT),
                rust_abi_version: ELM_EBI_RUST_ABI_VERSION,
                signature: ElmEbiRustHookSignature::ContextResult,
                flags: ELM_EBI_HOOK_FLAG_NONE,
            }),
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
    pub mode: ElmMixinMode,
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
            mode: ElmMixinMode::Chain,
        })
    }

    pub const fn with_mode(mut self, mode: ElmMixinMode) -> Self {
        self.mode = mode;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiExtensionDecl {
    pub target_name: String,
    pub point: String,
    pub contract: FlowContract,
    pub handler_contract: FlowContract,
    pub priority: i32,
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
        let contract =
            FlowContract::new(contract.into()).map_err(|_| ElmEbiLoadStatus::InvalidManifest)?;
        Ok(Self {
            target_name,
            point,
            handler_contract: contract.clone(),
            contract,
            priority: 0,
        })
    }

    pub fn with_handler_contract(
        mut self,
        handler_contract: impl Into<String>,
    ) -> Result<Self, ElmEbiLoadStatus> {
        self.handler_contract = FlowContract::new(handler_contract.into())
            .map_err(|_| ElmEbiLoadStatus::InvalidManifest)?;
        Ok(self)
    }

    pub const fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiProviderPortDecl {
    pub contract: FlowContract,
    pub access: ElmPortAccessPolicy,
    pub direction: FlowDirection,
    pub mode: FlowMode,
    pub flags: u32,
    pub handler_symbol: Option<String>,
    pub snapshot_symbol: Option<String>,
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
            handler_symbol: None,
            snapshot_symbol: None,
        })
    }

    pub fn with_handler_symbol(
        mut self,
        symbol: impl Into<String>,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let symbol = symbol.into();
        validate_symbol_name(&symbol)?;
        self.handler_symbol = Some(symbol);
        Ok(self)
    }

    pub fn with_snapshot_symbol(
        mut self,
        symbol: impl Into<String>,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let symbol = symbol.into();
        validate_symbol_name(&symbol)?;
        self.snapshot_symbol = Some(symbol);
        Ok(self)
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
    pub api_compatibility: Option<ElmEbiApiCompatibility>,
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
            api_compatibility: None,
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

    pub fn with_api_compatibility(mut self, compatibility: ElmEbiApiCompatibility) -> Self {
        self.api_compatibility = Some(compatibility);
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
            validate_contract_len(&extension.handler_contract)?;
        }
        for provider in &self.provider_ports {
            if provider.flags != 0 {
                return Err(ElmEbiLoadStatus::InvalidManifest);
            }
            validate_contract_len(&provider.contract)?;
            if let Some(symbol) = &provider.handler_symbol {
                validate_symbol_name(symbol)?;
            }
            if let Some(symbol) = &provider.snapshot_symbol {
                validate_symbol_name(symbol)?;
            }
        }
        for import in &self.imports {
            validate_import_decl(import)?;
        }
        for export in &self.exports {
            validate_export_decl(export)?;
        }
        if let Some(compatibility) = &self.api_compatibility {
            compatibility.validate()?;
            let root_index = usize::try_from(compatibility.root_import_index)
                .map_err(|_| ElmEbiLoadStatus::InvalidTarget)?;
            let root = self
                .imports
                .get(root_index)
                .ok_or(ElmEbiLoadStatus::InvalidTarget)?;
            if root.name != ELM_API_ROOT_IMPORT_NAME
                || root.contract.as_str() != ELM_API_ROOT_IMPORT_CONTRACT
                || root.min_version != 1
                || root.max_version != u32::MAX
            {
                return Err(ElmEbiLoadStatus::InvalidTarget);
            }
        }
        validate_lifecycle_hooks(self.lifecycle_hooks.as_ref())?;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmEbiImage {
    pub unit: ElmEbiUnit,
    pub payloads: Vec<ElmEbiSegmentPayload>,
    pub symbol_locations: Vec<ElmEbiSymbolLocationDecl>,
    pub relocations: Vec<ElmEbiRelocationDecl>,
    pub abi_fingerprint: Option<ElmRustAbiFingerprintV1>,
    pub proof: Option<ElmEbiProofV1>,
}

impl ElmEbiImage {
    pub fn new(unit: ElmEbiUnit) -> Self {
        Self {
            unit,
            payloads: Vec::new(),
            symbol_locations: Vec::new(),
            relocations: Vec::new(),
            abi_fingerprint: None,
            proof: None,
        }
    }

    pub fn with_payload(mut self, payload: ElmEbiSegmentPayload) -> Self {
        self.payloads.push(payload);
        self
    }

    pub fn with_symbol_location(mut self, symbol: ElmEbiSymbolLocationDecl) -> Self {
        self.symbol_locations.push(symbol);
        self
    }

    pub fn with_relocation(mut self, relocation: ElmEbiRelocationDecl) -> Self {
        self.relocations.push(relocation);
        self
    }

    pub fn with_abi_fingerprint(mut self, fingerprint: ElmRustAbiFingerprintV1) -> Self {
        self.abi_fingerprint = Some(fingerprint);
        self
    }

    pub fn with_proof(mut self, proof: ElmEbiProofV1) -> Self {
        self.proof = Some(proof);
        self
    }

    pub fn validate(&self, expected_arch: ElmEbiArch) -> Result<(), ElmEbiLoadStatus> {
        self.unit.validate(expected_arch)?;
        if self.symbol_locations.len() > ELM_EBI_MAX_SYMBOL_LOCATIONS
            || self.relocations.len() > ELM_EBI_MAX_RELOCATIONS
        {
            return Err(ElmEbiLoadStatus::InvalidManifest);
        }
        for payload in &self.payloads {
            let Some(segment) = self.unit.segments.get(payload.segment_index as usize) else {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            };
            if payload.kind != segment.kind
                || payload.source_index != segment.source_index
                || payload.file_size != segment.file_size
                || payload.mem_size != segment.mem_size
                || payload.bytes.len() as u64 != payload.file_size
            {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
            if matches!(payload.kind, ElmEbiSegmentKind::Bss) && !payload.bytes.is_empty() {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
        }
        for symbol in &self.symbol_locations {
            validate_symbol_name(&symbol.name)?;
            if symbol.flags != ELM_EBI_SYMBOL_LOCATION_FLAG_NONE || symbol.size == 0 {
                return Err(ElmEbiLoadStatus::InvalidManifest);
            }
            let Some(segment) = self.unit.segments.get(symbol.segment_index as usize) else {
                return Err(ElmEbiLoadStatus::InvalidManifest);
            };
            let Some(end) = symbol.offset.checked_add(symbol.size) else {
                return Err(ElmEbiLoadStatus::InvalidManifest);
            };
            if end > segment.mem_size {
                return Err(ElmEbiLoadStatus::InvalidManifest);
            }
        }
        if let (true, Some(hooks)) = (self.has_code_segment(), &self.unit.lifecycle_hooks) {
            let init = self.symbol_location(&hooks.initialize.symbol);
            let fini = self.symbol_location(&hooks.finalize.symbol);
            if !matches!(init, Some(symbol) if self.symbol_is_code(symbol))
                || !matches!(fini, Some(symbol) if self.symbol_is_code(symbol))
            {
                return Err(ElmEbiLoadStatus::InvalidManifest);
            }
        }
        if let (true, Some(entry)) = (self.has_code_segment(), &self.unit.entry) {
            if !matches!(self.symbol_location(&entry.symbol), Some(symbol) if self.symbol_is_code(symbol))
            {
                return Err(ElmEbiLoadStatus::InvalidManifest);
            }
        }
        for relocation in &self.relocations {
            if relocation.flags != ELM_EBI_RELOCATION_FLAG_NONE {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
            let Some(target) = self
                .unit
                .segments
                .get(relocation.target_segment_index as usize)
            else {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            };
            let width = relocation_width(relocation.kind);
            let Some(end) = relocation.target_offset.checked_add(width) else {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            };
            if end > target.mem_size || matches!(target.kind, ElmEbiSegmentKind::Relocation) {
                return Err(ElmEbiLoadStatus::InvalidSegment);
            }
            match relocation.kind {
                ElmEbiRelocationKind::ImageBase64 => {}
                ElmEbiRelocationKind::SegmentBase64 => {
                    if self
                        .unit
                        .segments
                        .get(relocation.value_index as usize)
                        .is_none()
                    {
                        return Err(ElmEbiLoadStatus::InvalidSegment);
                    }
                }
                ElmEbiRelocationKind::SymbolAbs64
                | ElmEbiRelocationKind::SymbolRel32
                | ElmEbiRelocationKind::SymbolRel64 => {
                    if self
                        .symbol_locations
                        .get(relocation.value_index as usize)
                        .is_none()
                    {
                        return Err(ElmEbiLoadStatus::InvalidSegment);
                    }
                }
                ElmEbiRelocationKind::ImportAbs64
                | ElmEbiRelocationKind::ImportRel32
                | ElmEbiRelocationKind::ImportRel64 => {
                    if self
                        .unit
                        .imports
                        .get(relocation.value_index as usize)
                        .is_none()
                    {
                        return Err(ElmEbiLoadStatus::InvalidSegment);
                    }
                }
            }
        }
        for (import_index, import) in self.unit.imports.iter().enumerate() {
            if !import.is_managed() {
                continue;
            }
            let mut relocation_count = 0usize;
            for relocation in self.relocations.iter().filter(|relocation| {
                relocation.value_index == import_index as u32
                    && matches!(
                        relocation.kind,
                        ElmEbiRelocationKind::ImportAbs64
                            | ElmEbiRelocationKind::ImportRel32
                            | ElmEbiRelocationKind::ImportRel64
                    )
            }) {
                relocation_count += 1;
                if relocation.kind != ElmEbiRelocationKind::ImportAbs64
                    || relocation.target_offset & 7 != 0
                    || !self
                        .unit
                        .segments
                        .get(relocation.target_segment_index as usize)
                        .is_some_and(|segment| {
                            matches!(
                                segment.kind,
                                ElmEbiSegmentKind::Data | ElmEbiSegmentKind::Bss
                            )
                        })
                {
                    return Err(ElmEbiLoadStatus::InvalidTarget);
                }
            }
            if relocation_count == 0 {
                return Err(ElmEbiLoadStatus::InvalidTarget);
            }
        }
        if let Some(compatibility) = &self.unit.api_compatibility {
            let root_relocations: Vec<_> = self
                .relocations
                .iter()
                .filter(|relocation| {
                    relocation.value_index == compatibility.root_import_index
                        && matches!(
                            relocation.kind,
                            ElmEbiRelocationKind::ImportAbs64
                                | ElmEbiRelocationKind::ImportRel32
                                | ElmEbiRelocationKind::ImportRel64
                        )
                })
                .collect();
            if root_relocations.len() != 1
                || root_relocations[0].kind != ElmEbiRelocationKind::ImportAbs64
                || !self
                    .unit
                    .segments
                    .get(root_relocations[0].target_segment_index as usize)
                    .is_some_and(|segment| {
                        matches!(
                            segment.kind,
                            ElmEbiSegmentKind::Data | ElmEbiSegmentKind::Bss
                        )
                    })
            {
                return Err(ElmEbiLoadStatus::InvalidTarget);
            }
        }
        if let Some(fingerprint) = &self.abi_fingerprint {
            fingerprint.validate()?;
        }
        if let Some(proof) = &self.proof {
            proof.validate_shape()?;
            if proof.subject_digest != canonical_ebi_digest(self) {
                return Err(ElmEbiLoadStatus::RuntimeRejected);
            }
        }
        Ok(())
    }

    pub fn symbol_location(&self, name: &str) -> Option<&ElmEbiSymbolLocationDecl> {
        self.symbol_locations
            .iter()
            .find(|symbol| symbol.name == name)
    }

    fn symbol_is_code(&self, symbol: &ElmEbiSymbolLocationDecl) -> bool {
        self.unit
            .segments
            .get(symbol.segment_index as usize)
            .map(|segment| matches!(segment.kind, ElmEbiSegmentKind::Code))
            .unwrap_or(false)
    }

    pub fn has_code_segment(&self) -> bool {
        self.unit
            .segments
            .iter()
            .any(|segment| matches!(segment.kind, ElmEbiSegmentKind::Code))
    }
}

pub const fn relocation_width(kind: ElmEbiRelocationKind) -> u64 {
    match kind {
        ElmEbiRelocationKind::ImageBase64
        | ElmEbiRelocationKind::SegmentBase64
        | ElmEbiRelocationKind::SymbolAbs64
        | ElmEbiRelocationKind::SymbolRel64
        | ElmEbiRelocationKind::ImportAbs64
        | ElmEbiRelocationKind::ImportRel64 => 8,
        ElmEbiRelocationKind::SymbolRel32 | ElmEbiRelocationKind::ImportRel32 => 4,
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

fn validate_import_decl(import: &ElmEbiImportDecl) -> Result<(), ElmEbiLoadStatus> {
    validate_symbol_name(&import.name)?;
    validate_contract_len(&import.contract)?;
    if import.min_version == 0
        || import.max_version < import.min_version
        || import.flags & !ELM_EBI_IMPORT_FLAGS_MASK != 0
        || import.flags & ELM_EBI_IMPORT_FLAG_MANAGED != 0
            && import.flags & ELM_EBI_IMPORT_FLAG_DIRECT_PINNED != 0
    {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    Ok(())
}

fn validate_export_decl(export: &ElmEbiExportDecl) -> Result<(), ElmEbiLoadStatus> {
    validate_symbol_name(&export.name)?;
    validate_contract_len(&export.contract)?;
    let visibility = export.flags
        & (ELM_EBI_EXPORT_FLAG_PRIVATE
            | ELM_EBI_EXPORT_FLAG_DEPENDENCY
            | ELM_EBI_EXPORT_FLAG_SUBTREE);
    if export.version == 0
        || export.flags & !ELM_EBI_EXPORT_FLAGS_MASK != 0
        || export.flags & ELM_EBI_EXPORT_FLAG_MANAGED != 0
            && export.flags & ELM_EBI_EXPORT_FLAG_DIRECT_PINNED != 0
        || visibility.count_ones() > 1
    {
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
    if let Some(hook) = &hooks.migrate_export {
        validate_lifecycle_hook(
            hook,
            ElmEbiLifecycleHookKind::MigrateExport,
            ELM_EBI_HOOK_ON_MIGRATE_EXPORT,
        )?;
    }
    if let Some(hook) = &hooks.migrate_import {
        validate_lifecycle_hook(
            hook,
            ElmEbiLifecycleHookKind::MigrateImport,
            ELM_EBI_HOOK_ON_MIGRATE_IMPORT,
        )?;
    }
    if let Some(hook) = &hooks.migrate_abort {
        validate_lifecycle_hook(
            hook,
            ElmEbiLifecycleHookKind::MigrateAbort,
            ELM_EBI_HOOK_ON_MIGRATE_ABORT,
        )?;
    }
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

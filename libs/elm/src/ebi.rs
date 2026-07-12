//! EBI 二进制装载接口协议。
//!
//! EBI 不是文件格式。EKI、未来的投影产物、启动期内建对象或测试内存对象
//! 都可以通过 EBI Source 产出这里定义的协议对象；ELM Core 只消费这些对象，
//! 不理解任何具体镜像或容器布局。当前内核把 EKI 投影能力归属到内建 `eki`
//! 子单元，而不是让根管理器或 Core 直接拥有某种文件格式。
//!
//! 一个 [`ElmEbiUnit`] 汇总 manifest、目标、API 兼容性、生命周期、菜单、依赖、段、重定位、
//! 符号位置、provider、import/export、扩展点和 extension。`validate` 系列逻辑必须在分配
//! 可执行内存或调用模块代码前检查数量上限、名称和契约、段权限、范围溢出、重定位宽度、
//! 符号存在性以及必需生命周期钩子。
//!
//! 原生 payload 通过 [`ElmImageReader`] 按稳定索引读取。EBI 对象只描述“装载器应该得到
//! 什么”，不规定容器如何压缩、索引、签名或存储这些字节；来源证明和容器完整性在投影层
//! 验证，规范 EBI 摘要与 Rust ABI 指纹在 Core 接收时再次验证。

use alloc::string::String;
use alloc::vec::Vec;

pub use crate::ebi_wire::*;
use crate::elmapi::{
    ELM_API_MAX_COMPATIBLE_VERSIONS, ELM_API_ROOT_IMPORT_CONTRACT, ELM_API_ROOT_IMPORT_NAME,
    ELM_KERNEL_API_LAYOUT_HASH_LEN, is_valid_kernel_api_identifier,
};
use crate::manifest::{ElmManifest, ElmName};
use crate::menu::{
    ELM_MENU_DESCRIPTION_LEN, ELM_MENU_LABEL_LEN, ELM_MENU_ROUTE_LEN, ElmMenuItemKind,
};
use crate::mgr::{ELM_MGR_RELATION_POINT_LEN, ELM_NEXUS_CONTRACT_LEN};
use crate::nexus::{FlowContract, FlowDirection, FlowMode};
use crate::proof::{ElmEbiProofV1, ElmRustAbiFingerprintV1, canonical_ebi_digest};
use crate::wire::{ElmMixinMode, ElmPortAccessPolicy};

/// `ELM_EBI_ABI_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_EBI_ABI_VERSION: u16 = 1;
/// `ELM_EBI_MAX_SEGMENTS` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_EBI_MAX_SEGMENTS: usize = 32;
/// `ELM_EBI_MAX_DEPENDENCIES` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_EBI_MAX_DEPENDENCIES: usize = 16;
/// `ELM_EBI_MAX_EXTENSION_POINTS` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_EBI_MAX_EXTENSION_POINTS: usize = 16;
/// `ELM_EBI_MAX_EXTENSIONS` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_EBI_MAX_EXTENSIONS: usize = 16;
/// `ELM_EBI_MAX_PROVIDER_PORTS` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_EBI_MAX_PROVIDER_PORTS: usize = 16;
/// `ELM_EBI_MAX_IMPORTS` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_EBI_MAX_IMPORTS: usize = 64;
/// `ELM_EBI_MAX_EXPORTS` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_EBI_MAX_EXPORTS: usize = 64;
/// 单个 ELM 可以声明的 Kernel API 命名空间依赖上限。
pub const ELM_EBI_MAX_KERNEL_API_REQUIREMENTS: usize = 64;
/// `ELM_EBI_MAX_SYMBOL_LOCATIONS` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_EBI_MAX_SYMBOL_LOCATIONS: usize = 128;
/// `ELM_EBI_MAX_RELOCATIONS` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_EBI_MAX_RELOCATIONS: usize = 512;
/// `ELM_EBI_NAME_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_EBI_NAME_LEN: usize = 128;
/// `ELM_EBI_SYMBOL_NAME_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_EBI_SYMBOL_NAME_LEN: usize = 128;
/// 表示该 EBI 段没有外部 payload 来源，通常用于 BSS 零填充段。
pub const ELM_EBI_SEGMENT_SOURCE_NONE: u32 = u32::MAX;
/// `ELM_EBI_SEGMENT_FLAG_READ` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_SEGMENT_FLAG_READ: u32 = 1 << 0;
/// `ELM_EBI_SEGMENT_FLAG_WRITE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_SEGMENT_FLAG_WRITE: u32 = 1 << 1;
/// `ELM_EBI_SEGMENT_FLAG_EXECUTE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_SEGMENT_FLAG_EXECUTE: u32 = 1 << 2;
/// `ELM_EBI_SEGMENT_FLAG_ZERO_FILL` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_SEGMENT_FLAG_ZERO_FILL: u32 = 1 << 3;
/// `ELM_EBI_SEGMENT_FLAG_RELOCATION_INPUT` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_SEGMENT_FLAG_RELOCATION_INPUT: u32 = 1 << 4;
/// `ELM_EBI_SYMBOL_FLAG_NONE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_SYMBOL_FLAG_NONE: u32 = 0;
/// `ELM_EBI_IMPORT_FLAG_OPTIONAL` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_IMPORT_FLAG_OPTIONAL: u32 = 1 << 0;
/// `ELM_EBI_IMPORT_FLAG_MANAGED` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_IMPORT_FLAG_MANAGED: u32 = 1 << 1;
/// `ELM_EBI_IMPORT_FLAG_DIRECT_PINNED` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_IMPORT_FLAG_DIRECT_PINNED: u32 = 1 << 2;
/// `ELM_EBI_IMPORT_FLAG_ALLOW_ANCESTOR` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_IMPORT_FLAG_ALLOW_ANCESTOR: u32 = 1 << 3;
/// `ELM_EBI_IMPORT_FLAG_ALLOW_BUILTIN` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_IMPORT_FLAG_ALLOW_BUILTIN: u32 = 1 << 4;
/// `ELM_EBI_IMPORT_FLAGS_MASK` 定义当前版本认可的全部标志位；输入包含掩码外位时必须拒绝或按调用契约报错。
pub const ELM_EBI_IMPORT_FLAGS_MASK: u32 = ELM_EBI_IMPORT_FLAG_OPTIONAL
    | ELM_EBI_IMPORT_FLAG_MANAGED
    | ELM_EBI_IMPORT_FLAG_DIRECT_PINNED
    | ELM_EBI_IMPORT_FLAG_ALLOW_ANCESTOR
    | ELM_EBI_IMPORT_FLAG_ALLOW_BUILTIN;
/// `ELM_EBI_EXPORT_FLAG_MANAGED` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_EXPORT_FLAG_MANAGED: u32 = 1 << 0;
/// `ELM_EBI_EXPORT_FLAG_DIRECT_PINNED` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_EXPORT_FLAG_DIRECT_PINNED: u32 = 1 << 1;
/// `ELM_EBI_EXPORT_FLAG_PRIVATE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_EXPORT_FLAG_PRIVATE: u32 = 1 << 2;
/// `ELM_EBI_EXPORT_FLAG_DEPENDENCY` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_EXPORT_FLAG_DEPENDENCY: u32 = 1 << 3;
/// `ELM_EBI_EXPORT_FLAG_SUBTREE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_EXPORT_FLAG_SUBTREE: u32 = 1 << 4;
/// `ELM_EBI_EXPORT_FLAGS_MASK` 定义当前版本认可的全部标志位；输入包含掩码外位时必须拒绝或按调用契约报错。
pub const ELM_EBI_EXPORT_FLAGS_MASK: u32 = ELM_EBI_EXPORT_FLAG_MANAGED
    | ELM_EBI_EXPORT_FLAG_DIRECT_PINNED
    | ELM_EBI_EXPORT_FLAG_PRIVATE
    | ELM_EBI_EXPORT_FLAG_DEPENDENCY
    | ELM_EBI_EXPORT_FLAG_SUBTREE;
/// `ELM_EBI_SYMBOL_LOCATION_FLAG_NONE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_SYMBOL_LOCATION_FLAG_NONE: u32 = 0;
/// `ELM_EBI_RELOCATION_FLAG_NONE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_RELOCATION_FLAG_NONE: u32 = 0;
/// `ELM_EBI_RUST_ABI_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_EBI_RUST_ABI_VERSION: u16 = 1;
/// `ELM_EBI_HOOK_FLAG_NONE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_HOOK_FLAG_NONE: u32 = 0;
/// 生命周期 hook presence mask 中表示 `initialize` 钩子已声明的位。
pub const ELM_EBI_HOOK_ON_INITIALIZE: &str = "on_initialize";
/// 生命周期 hook presence mask 中表示 `finalize` 钩子已声明的位。
pub const ELM_EBI_HOOK_ON_FINALIZE: &str = "on_finalize";
/// 生命周期 hook presence mask 中表示 `quiesce` 钩子已声明的位。
pub const ELM_EBI_HOOK_ON_QUIESCE: &str = "on_quiesce";
/// 生命周期 hook presence mask 中表示 `pause` 钩子已声明的位。
pub const ELM_EBI_HOOK_ON_PAUSE: &str = "on_pause";
/// 生命周期 hook presence mask 中表示 `resume` 钩子已声明的位。
pub const ELM_EBI_HOOK_ON_RESUME: &str = "on_resume";
/// 生命周期 hook presence mask 中表示 `migrate_export` 钩子已声明的位。
pub const ELM_EBI_HOOK_ON_MIGRATE_EXPORT: &str = "on_migrate_export";
/// 生命周期 hook presence mask 中表示 `migrate_import` 钩子已声明的位。
pub const ELM_EBI_HOOK_ON_MIGRATE_IMPORT: &str = "on_migrate_import";
/// 生命周期 hook presence mask 中表示 `migrate_abort` 钩子已声明的位。
pub const ELM_EBI_HOOK_ON_MIGRATE_ABORT: &str = "on_migrate_abort";
/// `ELM_MIGRATION_STATE_MAX` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
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
    /// 返回当前视图包含的有效记录或字节数量。
    fn len(&self) -> u64;

    /// 读取 `at`，并验证游标、长度和返回记录边界。
    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<(), ElmEbiLoadStatus>;

    /// 判断当前视图是否不含任何有效记录。
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 读取 `all`，并验证游标、长度和返回记录边界。
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

/// 以借用字节切片实现 [`ElmImageReader`] 的内存镜像读取器。
pub struct ElmSliceImageReader<'a> {
    bytes: &'a [u8],
}

impl<'a> ElmSliceImageReader<'a> {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// `ElmEbiArch` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmEbiArch {
    /// `Any` 是 `ElmEbiArch` 中的稳定判别值，表示 `any`。
    Any = 0,
    /// `Riscv64` 是 `ElmEbiArch` 中的稳定判别值，表示 `riscv64`。
    Riscv64 = 1,
    /// `LoongArch64` 是 `ElmEbiArch` 中的稳定判别值，表示 `loong arch64`。
    LoongArch64 = 2,
}

impl ElmEbiArch {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Any),
            1 => Some(Self::Riscv64),
            2 => Some(Self::LoongArch64),
            _ => None,
        }
    }

    /// 执行 `matches` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn matches(self, expected: Self) -> bool {
        matches!(self, Self::Any) || matches!(expected, Self::Any) || self as u32 == expected as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// `ElmEbiSegmentKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmEbiSegmentKind {
    /// `Code` 表示 `ElmEbiSegmentKind` 的对象类别：`code`。
    Code = 1,
    /// `ReadOnlyData` 表示 `ElmEbiSegmentKind` 的对象类别：`read only data`。
    ReadOnlyData = 2,
    /// `Data` 表示 `ElmEbiSegmentKind` 的对象类别：`data`。
    Data = 3,
    /// `Bss` 表示 `ElmEbiSegmentKind` 的对象类别：`bss`。
    Bss = 4,
    /// `Relocation` 表示 `ElmEbiSegmentKind` 的对象类别：`relocation`。
    Relocation = 5,
    /// `Note` 表示 `ElmEbiSegmentKind` 的对象类别：`note`。
    Note = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// `ElmEbiLifecycleHookKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmEbiLifecycleHookKind {
    /// `Initialize` 表示 `ElmEbiLifecycleHookKind` 的对象类别：`initialize`。
    Initialize = 1,
    /// `Finalize` 表示 `ElmEbiLifecycleHookKind` 的对象类别：`finalize`。
    Finalize = 2,
    /// `MigrateExport` 表示 `ElmEbiLifecycleHookKind` 的对象类别：`migrate export`。
    MigrateExport = 3,
    /// `MigrateImport` 表示 `ElmEbiLifecycleHookKind` 的对象类别：`migrate import`。
    MigrateImport = 4,
    /// `MigrateAbort` 表示 `ElmEbiLifecycleHookKind` 的对象类别：`migrate abort`。
    MigrateAbort = 5,
}

impl ElmEbiLifecycleHookKind {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
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
/// `ElmEbiRustHookSignature` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmEbiRustHookSignature {
    /// `ContextResult` 是 `ElmEbiRustHookSignature` 中的稳定判别值，表示 `context result`。
    ContextResult = 1,
}

impl ElmEbiRustHookSignature {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
    pub const fn from_raw(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::ContextResult),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// `ElmEbiRelocationKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmEbiRelocationKind {
    /// `ImageBase64` 表示 `ElmEbiRelocationKind` 的对象类别：`image base64`。
    ImageBase64 = 1,
    /// `SegmentBase64` 表示 `ElmEbiRelocationKind` 的对象类别：`segment base64`。
    SegmentBase64 = 2,
    /// `SymbolAbs64` 表示 `ElmEbiRelocationKind` 的对象类别：`symbol abs64`。
    SymbolAbs64 = 3,
    /// `SymbolRel32` 表示 `ElmEbiRelocationKind` 的对象类别：`symbol rel32`。
    SymbolRel32 = 4,
    /// `SymbolRel64` 表示 `ElmEbiRelocationKind` 的对象类别：`symbol rel64`。
    SymbolRel64 = 5,
    /// `ImportAbs64` 表示 `ElmEbiRelocationKind` 的对象类别：`import abs64`。
    ImportAbs64 = 6,
    /// `ImportRel32` 表示 `ElmEbiRelocationKind` 的对象类别：`import rel32`。
    ImportRel32 = 7,
    /// `ImportRel64` 表示 `ElmEbiRelocationKind` 的对象类别：`import rel64`。
    ImportRel64 = 8,
}

impl ElmEbiRelocationKind {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
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
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
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
/// EBI 单元要求的目标架构、Rust ABI 版本和目标特性约束。
pub struct ElmEbiTarget {
    /// `arch` 是所属枚举的稳定判别值；未知值必须拒绝。
    pub arch: ElmEbiArch,
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `min_core_version` 是该对象、ABI 或契约的版本值，用于装载和协商兼容性。
    pub min_core_version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 镜像声明可接受的 ELM API 版本范围与必需 feature 位。
pub struct ElmEbiApiCompatibility {
    /// `root_import_index` 是所属表中的零基索引；使用前必须验证小于对应记录数量。
    pub root_import_index: u32,
    /// 镜像要求运行时必须提供的 ELM API feature 位。
    pub required_features: u64,
    /// 调用方或镜像声明可接受的 API 版本列表。
    pub compatible_versions: Vec<u16>,
}

impl ElmEbiApiCompatibility {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 验证当前对象及其关联记录满足全部结构、范围和关系不变量。
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

    /// 执行 `select_highest_common` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn select_highest_common(&self, supported: &[u16]) -> Option<u16> {
        self.compatible_versions
            .iter()
            .rev()
            .copied()
            .find(|version| supported.contains(version))
    }
}

impl ElmEbiTarget {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(arch: ElmEbiArch) -> Self {
        Self {
            arch,
            abi_version: ELM_EBI_ABI_VERSION,
            min_core_version: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 一个待装载内存段的 kind、权限、对齐、虚拟布局和 payload 引用。
pub struct ElmEbiSegment {
    /// 该记录、资源或关系的类别编码。
    pub kind: ElmEbiSegmentKind,
    /// `size` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub size: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `file_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub file_size: u64,
    /// `mem_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub mem_size: u64,
    /// `align` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub align: u64,
    /// `source_index` 是所属表中的零基索引；使用前必须验证小于对应记录数量。
    pub source_index: u32,
    /// `source_offset` 是相对于所属块、段或文件起点的字节偏移；与长度相加前必须检查溢出。
    pub source_offset: u64,
    /// `content_hash` 保存对应对象的完整性摘要；安全决策必须按声明算法验证完整字节。
    pub content_hash: u64,
}

impl ElmEbiSegment {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 执行 `from_payload` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `requires_native_loader` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn requires_native_loader(&self) -> bool {
        matches!(
            self.kind,
            ElmEbiSegmentKind::Code | ElmEbiSegmentKind::Relocation
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// ELM 对外部 export 的名称、契约、版本范围、模式和作用域需求声明。
pub struct ElmEbiImportDecl {
    /// 对象的固定长度名称缓冲区；实际字符串以首个零字节结束。
    pub name: String,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: FlowContract,
    /// 调用方可接受的最低版本，包含该端点。
    pub min_version: u32,
    /// 调用方可接受的最高版本，包含该端点。
    pub max_version: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
}

impl ElmEbiImportDecl {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 执行 `new_range` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `accepts_version` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn accepts_version(&self, version: u32) -> bool {
        version >= self.min_version && version <= self.max_version
    }

    /// 执行 `is_optional` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn is_optional(&self) -> bool {
        self.flags & ELM_EBI_IMPORT_FLAG_OPTIONAL != 0
    }

    /// 执行 `is_managed` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn is_managed(&self) -> bool {
        self.flags & ELM_EBI_IMPORT_FLAG_MANAGED != 0
    }

    /// 执行 `is_direct_pinned` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn is_direct_pinned(&self) -> bool {
        self.flags & (ELM_EBI_IMPORT_FLAG_MANAGED | ELM_EBI_IMPORT_FLAG_DIRECT_PINNED) == 0
            || self.flags & ELM_EBI_IMPORT_FLAG_DIRECT_PINNED != 0
    }

    /// 执行 `allows_ancestor` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn allows_ancestor(&self) -> bool {
        self.flags & (ELM_EBI_IMPORT_FLAG_ALLOW_ANCESTOR | ELM_EBI_IMPORT_FLAG_ALLOW_BUILTIN) == 0
            || self.flags & ELM_EBI_IMPORT_FLAG_ALLOW_ANCESTOR != 0
    }

    /// 执行 `allows_builtin` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn allows_builtin(&self) -> bool {
        self.flags & (ELM_EBI_IMPORT_FLAG_ALLOW_ANCESTOR | ELM_EBI_IMPORT_FLAG_ALLOW_BUILTIN) == 0
            || self.flags & ELM_EBI_IMPORT_FLAG_ALLOW_BUILTIN != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// ELM 向其他单元公开的名称、契约、版本、符号和可见范围声明。
pub struct ElmEbiExportDecl {
    /// 对象的固定长度名称缓冲区；实际字符串以首个零字节结束。
    pub name: String,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: FlowContract,
    /// 该对象或契约的版本号。
    pub version: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
}

impl ElmEbiExportDecl {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 执行 `is_managed` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn is_managed(&self) -> bool {
        self.flags & ELM_EBI_EXPORT_FLAG_MANAGED != 0
    }

    /// 执行 `is_direct_pinned` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn is_direct_pinned(&self) -> bool {
        self.flags & (ELM_EBI_EXPORT_FLAG_MANAGED | ELM_EBI_EXPORT_FLAG_DIRECT_PINNED) == 0
            || self.flags & ELM_EBI_EXPORT_FLAG_DIRECT_PINNED != 0
    }

    /// 执行 `visible_to_dependency` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn visible_to_dependency(&self) -> bool {
        self.flags
            & (ELM_EBI_EXPORT_FLAG_PRIVATE
                | ELM_EBI_EXPORT_FLAG_DEPENDENCY
                | ELM_EBI_EXPORT_FLAG_SUBTREE)
            == 0
            || self.flags & ELM_EBI_EXPORT_FLAG_DEPENDENCY != 0
    }

    /// 执行 `visible_to_subtree` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn visible_to_subtree(&self) -> bool {
        self.flags & ELM_EBI_EXPORT_FLAG_SUBTREE != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 单元激活后可选 entry 的原生符号和调用约束。
pub struct ElmEbiEntry {
    /// 镜像符号表中的规范符号名称或符号引用。
    pub symbol: String,
}

impl ElmEbiEntry {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub fn new(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// EBI 段对应的来源索引、偏移、长度或零填充 payload 描述。
pub struct ElmEbiSegmentPayload {
    /// `segment_index` 是所属表中的零基索引；使用前必须验证小于对应记录数量。
    pub segment_index: u32,
    /// `source_index` 是所属表中的零基索引；使用前必须验证小于对应记录数量。
    pub source_index: u32,
    /// 该记录、资源或关系的类别编码。
    pub kind: ElmEbiSegmentKind,
    /// `file_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub file_size: u64,
    /// `mem_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub mem_size: u64,
    /// `bytes` 保存所属对象声明或快照中的有序记录集合。
    pub bytes: Vec<u8>,
}

impl ElmEbiSegmentPayload {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// 一个规范符号在指定段内的偏移、尺寸和 flags 映射。
pub struct ElmEbiSymbolLocationDecl {
    /// 对象的固定长度名称缓冲区；实际字符串以首个零字节结束。
    pub name: String,
    /// `segment_index` 是所属表中的零基索引；使用前必须验证小于对应记录数量。
    pub segment_index: u32,
    /// `offset` 是相对于所属块、段或文件起点的字节偏移；与长度相加前必须检查溢出。
    pub offset: u64,
    /// `size` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub size: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
}

impl ElmEbiSymbolLocationDecl {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// 装载器对段、符号、import 或 image base 应用的显式重定位记录。
pub struct ElmEbiRelocationDecl {
    /// 该记录、资源或关系的类别编码。
    pub kind: ElmEbiRelocationKind,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `target_segment_index` 是所属表中的零基索引；使用前必须验证小于对应记录数量。
    pub target_segment_index: u32,
    /// `target_offset` 是相对于所属块、段或文件起点的字节偏移；与长度相加前必须检查溢出。
    pub target_offset: u64,
    /// `value_index` 是所属表中的零基索引；使用前必须验证小于对应记录数量。
    pub value_index: u32,
    /// 重定位计算使用的有符号加数。
    pub addend: i64,
}

impl ElmEbiRelocationDecl {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// 单个生命周期阶段对应的符号、签名种类和 flags 声明。
pub struct ElmEbiLifecycleHookDecl {
    /// 该记录、资源或关系的类别编码。
    pub kind: ElmEbiLifecycleHookKind,
    /// 镜像符号表中的规范符号名称或符号引用。
    pub symbol: String,
    /// `rust_abi_version` 是该对象、ABI 或契约的版本值，用于装载和协商兼容性。
    pub rust_abi_version: u16,
    /// 覆盖规范 EBI 摘要的签名字节。
    pub signature: ElmEbiRustHookSignature,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
}

impl ElmEbiLifecycleHookDecl {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// 必需 initialize/finalize 与全部可选暂停、恢复、迁移钩子的集合。
pub struct ElmEbiLifecycleHooks {
    /// 执行 `initialize` 操作的受控回调；调用方必须遵守所属表的生命周期和故障边界。
    pub initialize: ElmEbiLifecycleHookDecl,
    /// 执行 `finalize` 操作的受控回调；调用方必须遵守所属表的生命周期和故障边界。
    pub finalize: ElmEbiLifecycleHookDecl,
    /// 执行 `migrate_export` 操作的受控回调；调用方必须遵守所属表的生命周期和故障边界。
    pub migrate_export: Option<ElmEbiLifecycleHookDecl>,
    /// 执行 `migrate_import` 操作的受控回调；调用方必须遵守所属表的生命周期和故障边界。
    pub migrate_import: Option<ElmEbiLifecycleHookDecl>,
    /// 执行 `migrate_abort` 操作的受控回调；调用方必须遵守所属表的生命周期和故障边界。
    pub migrate_abort: Option<ElmEbiLifecycleHookDecl>,
}

impl ElmEbiLifecycleHooks {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 设置 `migration_hooks` 并返回更新后的值，便于构建器式初始化。
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

    /// 执行 `rust_context_result_v1` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `rust_context_result_v1_with_migration` 定义的模型或协议操作；返回值反映校验后的结果。
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
/// EBI 中一个 elm-mgr 菜单项的展示、路由和 action 声明。
pub struct ElmEbiMenuDecl {
    /// 该记录、资源或关系的类别编码。
    pub kind: ElmMenuItemKind,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 供管理界面展示的短标签，实际长度受固定缓冲区限制。
    pub label: String,
    /// 供管理和诊断界面展示的说明文本。
    pub description: String,
    /// 菜单或管理入口使用的稳定路由 identifier。
    pub route: String,
}

impl ElmEbiMenuDecl {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// 对另一 ELM 名称、版本范围和可选性的依赖声明。
pub struct ElmEbiDependencyDecl {
    /// `provider_name` 是固定长度规范名称；实际字符串在首个零字节处结束。
    pub provider_name: String,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: FlowContract,
}

impl ElmEbiDependencyDecl {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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
/// ELM 对一个版本化 Kernel API 命名空间的装载前依赖声明。
pub struct ElmEbiKernelApiRequirement {
    /// 命名空间 identifier。
    pub identifier: String,
    /// 当前 v1 使用的精确函数表版本。
    pub version: u16,
    /// 模块实际需要的能力位。
    pub required_capabilities: u64,
    /// 对应版本函数表的规范布局 SHA-256。
    pub layout_hash: [u8; ELM_KERNEL_API_LAYOUT_HASH_LEN],
}

impl ElmEbiKernelApiRequirement {
    /// 构造并校验一个 Kernel API 依赖声明。
    pub fn new(
        identifier: impl Into<String>,
        version: u16,
        required_capabilities: u64,
        layout_hash: [u8; ELM_KERNEL_API_LAYOUT_HASH_LEN],
    ) -> Result<Self, ElmEbiLoadStatus> {
        let requirement = Self {
            identifier: identifier.into(),
            version,
            required_capabilities,
            layout_hash,
        };
        validate_kernel_api_requirement(&requirement)?;
        Ok(requirement)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 目标单元公开的补缀点名称、契约、阶段和组合模式声明。
pub struct ElmEbiExtensionPointDecl {
    /// 补缀点的完整 identifier，通常包含阶段后缀。
    pub point: String,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: FlowContract,
    /// 端口、绑定或扩展点采用的并发/分发模式编码。
    pub mode: ElmMixinMode,
}

impl ElmEbiExtensionPointDecl {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 设置 `mode` 并返回更新后的值，便于构建器式初始化。
    pub const fn with_mode(mut self, mode: ElmMixinMode) -> Self {
        self.mode = mode;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 当前单元附着到目标 ELM 补缀点的 handler、优先级和载荷契约声明。
pub struct ElmEbiExtensionDecl {
    /// `target_name` 是固定长度规范名称；实际字符串在首个零字节处结束。
    pub target_name: String,
    /// 补缀点的完整 identifier，通常包含阶段后缀。
    pub point: String,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: FlowContract,
    /// mixin/provider 处理器自身的调用契约。
    pub handler_contract: FlowContract,
    /// 同一扩展点中的调度优先级；排序规则由扩展运行时定义。
    pub priority: i32,
}

impl ElmEbiExtensionDecl {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 设置 `handler_contract` 并返回更新后的值，便于构建器式初始化。
    pub fn with_handler_contract(
        mut self,
        handler_contract: impl Into<String>,
    ) -> Result<Self, ElmEbiLoadStatus> {
        self.handler_contract = FlowContract::new(handler_contract.into())
            .map_err(|_| ElmEbiLoadStatus::InvalidManifest)?;
        Ok(self)
    }

    /// 设置 `priority` 并返回更新后的值，便于构建器式初始化。
    pub const fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 原生 provider 端口的契约、访问策略、方向、模式和处理符号声明。
pub struct ElmEbiProviderPortDecl {
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: FlowContract,
    /// 端口的访问范围策略编码。
    pub access: ElmPortAccessPolicy,
    /// 端口的数据流方向编码。
    pub direction: FlowDirection,
    /// 端口、绑定或扩展点采用的并发/分发模式编码。
    pub mode: FlowMode,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 实现该处理器的原生导出符号。
    pub handler_symbol: Option<String>,
    /// 实现 provider 快照入口的原生导出符号。
    pub snapshot_symbol: Option<String>,
}

impl ElmEbiProviderPortDecl {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 设置 `handler_symbol` 并返回更新后的值，便于构建器式初始化。
    pub fn with_handler_symbol(
        mut self,
        symbol: impl Into<String>,
    ) -> Result<Self, ElmEbiLoadStatus> {
        let symbol = symbol.into();
        validate_symbol_name(&symbol)?;
        self.handler_symbol = Some(symbol);
        Ok(self)
    }

    /// 设置 `snapshot_symbol` 并返回更新后的值，便于构建器式初始化。
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
/// 一个来源投影出的完整 EBI 语义单元，是 ELM Core 的唯一装载输入。
pub struct ElmEbiUnit {
    /// 经解析和验证的 ELM manifest。
    pub manifest: ElmManifest,
    /// 关系、重定位或调用的目标对象。
    pub target: ElmEbiTarget,
    /// 该单元声明的菜单项或菜单元数据。
    pub menu: Option<ElmEbiMenuDecl>,
    /// EBI 单元声明的内存段集合。
    pub segments: Vec<ElmEbiSegment>,
    /// 执行 `entry` 操作的受控回调；调用方必须遵守所属表的生命周期和故障边界。
    pub entry: Option<ElmEbiEntry>,
    /// 该单元声明的必需或可选依赖集合。
    pub dependencies: Vec<ElmEbiDependencyDecl>,
    /// 该单元在执行任何原生代码前必须取得的 Kernel API 命名空间集合。
    pub kernel_api_requirements: Vec<ElmEbiKernelApiRequirement>,
    /// 该单元允许其他 ELM 附着的补缀点集合。
    pub extension_points: Vec<ElmEbiExtensionPointDecl>,
    /// 该单元声明的 extension/mixin 附着集合。
    pub extensions: Vec<ElmEbiExtensionDecl>,
    /// `provider_ports` 保存所属对象声明或快照中的有序记录集合。
    pub provider_ports: Vec<ElmEbiProviderPortDecl>,
    /// 该单元声明的 import 集合。
    pub imports: Vec<ElmEbiImportDecl>,
    /// 该单元公开的 export 集合。
    pub exports: Vec<ElmEbiExportDecl>,
    /// 该单元声明并已验证符号位置的生命周期钩子集合。
    pub lifecycle_hooks: Option<ElmEbiLifecycleHooks>,
    /// 镜像声明的 ELM API 版本和能力兼容范围。
    pub api_compatibility: Option<ElmEbiApiCompatibility>,
}

impl ElmEbiUnit {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub fn new(manifest: ElmManifest, target: ElmEbiTarget) -> Self {
        Self {
            manifest,
            target,
            menu: None,
            segments: Vec::new(),
            entry: None,
            dependencies: Vec::new(),
            kernel_api_requirements: Vec::new(),
            extension_points: Vec::new(),
            extensions: Vec::new(),
            provider_ports: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
            lifecycle_hooks: None,
            api_compatibility: None,
        }
    }

    /// 设置 `menu` 并返回更新后的值，便于构建器式初始化。
    pub fn with_menu(mut self, menu: ElmEbiMenuDecl) -> Self {
        self.menu = Some(menu);
        self
    }

    /// 设置 `segment` 并返回更新后的值，便于构建器式初始化。
    pub fn with_segment(mut self, segment: ElmEbiSegment) -> Self {
        self.segments.push(segment);
        self
    }

    /// 设置 `entry` 并返回更新后的值，便于构建器式初始化。
    pub fn with_entry(mut self, entry: ElmEbiEntry) -> Self {
        self.entry = Some(entry);
        self
    }

    /// 设置 `dependency` 并返回更新后的值，便于构建器式初始化。
    pub fn with_dependency(mut self, dependency: ElmEbiDependencyDecl) -> Self {
        self.dependencies.push(dependency);
        self
    }

    /// 添加一个装载前 Kernel API 依赖声明。
    pub fn with_kernel_api_requirement(mut self, requirement: ElmEbiKernelApiRequirement) -> Self {
        self.kernel_api_requirements.push(requirement);
        self
    }

    /// 设置 `extension_point` 并返回更新后的值，便于构建器式初始化。
    pub fn with_extension_point(mut self, point: ElmEbiExtensionPointDecl) -> Self {
        self.extension_points.push(point);
        self
    }

    /// 设置 `extension` 并返回更新后的值，便于构建器式初始化。
    pub fn with_extension(mut self, extension: ElmEbiExtensionDecl) -> Self {
        self.extensions.push(extension);
        self
    }

    /// 设置 `provider_port` 并返回更新后的值，便于构建器式初始化。
    pub fn with_provider_port(mut self, provider: ElmEbiProviderPortDecl) -> Self {
        self.provider_ports.push(provider);
        self
    }

    /// 设置 `import` 并返回更新后的值，便于构建器式初始化。
    pub fn with_import(mut self, import: ElmEbiImportDecl) -> Self {
        self.imports.push(import);
        self
    }

    /// 设置 `export` 并返回更新后的值，便于构建器式初始化。
    pub fn with_export(mut self, export: ElmEbiExportDecl) -> Self {
        self.exports.push(export);
        self
    }

    /// 设置 `lifecycle_hooks` 并返回更新后的值，便于构建器式初始化。
    pub fn with_lifecycle_hooks(mut self, hooks: ElmEbiLifecycleHooks) -> Self {
        self.lifecycle_hooks = Some(hooks);
        self
    }

    /// 设置 `api_compatibility` 并返回更新后的值，便于构建器式初始化。
    pub fn with_api_compatibility(mut self, compatibility: ElmEbiApiCompatibility) -> Self {
        self.api_compatibility = Some(compatibility);
        self
    }

    /// 验证当前对象及其关联记录满足全部结构、范围和关系不变量。
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
            || self.kernel_api_requirements.len() > ELM_EBI_MAX_KERNEL_API_REQUIREMENTS
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
        for requirement in &self.kernel_api_requirements {
            validate_kernel_api_requirement(requirement)?;
        }
        if self.kernel_api_requirements.windows(2).any(|items| {
            items[0].identifier >= items[1].identifier
                || (items[0].identifier == items[1].identifier
                    && items[0].version >= items[1].version)
        }) {
            return Err(ElmEbiLoadStatus::InvalidManifest);
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

    /// 执行 `has_native_code` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn has_native_code(&self) -> bool {
        self.entry.is_some()
            || self
                .segments
                .iter()
                .any(ElmEbiSegment::requires_native_loader)
    }
}

fn validate_kernel_api_requirement(
    requirement: &ElmEbiKernelApiRequirement,
) -> Result<(), ElmEbiLoadStatus> {
    if !is_valid_kernel_api_identifier(&requirement.identifier)
        || requirement.version == 0
        || requirement.layout_hash == [0; ELM_KERNEL_API_LAYOUT_HASH_LEN]
    {
        return Err(ElmEbiLoadStatus::InvalidManifest);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 完整 EBI 单元及其 payload reader、来源元数据和证明信息的组合。
pub struct ElmEbiImage {
    /// `unit` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub unit: ElmEbiUnit,
    /// 镜像各段或对象对应的 payload 数据集合。
    pub payloads: Vec<ElmEbiSegmentPayload>,
    /// 符号名称到段内位置的已验证映射集合。
    pub symbol_locations: Vec<ElmEbiSymbolLocationDecl>,
    /// 装载器必须应用且已经通过范围检查的重定位集合。
    pub relocations: Vec<ElmEbiRelocationDecl>,
    /// 用于拒绝 Rust ABI、目标特性或布局不兼容镜像的完整指纹。
    pub abi_fingerprint: Option<ElmRustAbiFingerprintV1>,
    /// 证明链、签名和来源身份信息。
    pub proof: Option<ElmEbiProofV1>,
}

impl ElmEbiImage {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 设置 `payload` 并返回更新后的值，便于构建器式初始化。
    pub fn with_payload(mut self, payload: ElmEbiSegmentPayload) -> Self {
        self.payloads.push(payload);
        self
    }

    /// 设置 `symbol_location` 并返回更新后的值，便于构建器式初始化。
    pub fn with_symbol_location(mut self, symbol: ElmEbiSymbolLocationDecl) -> Self {
        self.symbol_locations.push(symbol);
        self
    }

    /// 设置 `relocation` 并返回更新后的值，便于构建器式初始化。
    pub fn with_relocation(mut self, relocation: ElmEbiRelocationDecl) -> Self {
        self.relocations.push(relocation);
        self
    }

    /// 设置 `abi_fingerprint` 并返回更新后的值，便于构建器式初始化。
    pub fn with_abi_fingerprint(mut self, fingerprint: ElmRustAbiFingerprintV1) -> Self {
        self.abi_fingerprint = Some(fingerprint);
        self
    }

    /// 设置 `proof` 并返回更新后的值，便于构建器式初始化。
    pub fn with_proof(mut self, proof: ElmEbiProofV1) -> Self {
        self.proof = Some(proof);
        self
    }

    /// 验证当前对象及其关联记录满足全部结构、范围和关系不变量。
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

    /// 执行 `symbol_location` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `has_code_segment` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn has_code_segment(&self) -> bool {
        self.unit
            .segments
            .iter()
            .any(|segment| matches!(segment.kind, ElmEbiSegmentKind::Code))
    }
}

/// 执行 `relocation_width` 定义的模型或协议操作；返回值反映校验后的结果。
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

/// 执行 `default_segment_flags` 定义的模型或协议操作；返回值反映校验后的结果。
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

//! JSON manifest 到格式无关程序契约的归一化边界。

use std::fmt;

use native_abi::{
    OPERATIONS, OperationId, REQUIREMENTS, RequirementId, Rights, right_by_name,
    wire as native_wire,
};
use serde::Deserialize;
use soyo::registry::{MAX_CAPABILITIES, MAX_IMPORTS, PAGE_SIZE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractErrorKind {
    InvalidJson,
    InvalidEntry,
    InvalidVersion,
    TooManyImports,
    TooManyCapabilities,
    UnknownOperation,
    DuplicateOperation,
    UnknownRequirement,
    DuplicateRequirement,
    UnknownRight,
    DuplicateRight,
    RightsExceeded,
    MissingCapability,
    InvalidRuntime,
    InvalidIdentity,
    DuplicateDependency,
    MissingDependency,
    DuplicateSymbol,
    InvalidSymbol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractError {
    kind: ContractErrorKind,
    detail: String,
}

impl ContractError {
    pub const fn kind(&self) -> ContractErrorKind {
        self.kind
    }

    fn new(kind: ContractErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ContractError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractImport {
    pub operation: OperationId,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractCapability {
    pub requirement: RequirementId,
    pub rights: Rights,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeContract {
    pub stack_size: u64,
    pub stack_guard_size: u64,
    pub start_info_max_size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramContract {
    entry: String,
    imports: Vec<ContractImport>,
    capabilities: Vec<ContractCapability>,
    runtime: RuntimeContract,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentContractImport {
    pub operation: OperationId,
    pub required: bool,
    pub slot_symbol: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentContractDependency {
    pub component_id: [u8; 16],
    pub abi_id: [u8; 16],
    pub content_hash: Option<[u8; 32]>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentContractSymbolImport {
    pub dependency_index: u32,
    pub interface_id: [u8; 16],
    pub symbol_id: [u8; 16],
    pub signature_hash: [u8; 32],
    pub binding_symbol: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentContractSymbolExport {
    pub interface_id: [u8; 16],
    pub symbol_id: [u8; 16],
    pub signature_hash: [u8; 32],
    pub symbol: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentContract {
    component_id: [u8; 16],
    abi_id: [u8; 16],
    init: Option<String>,
    fini: Option<String>,
    tls_offset_symbol: Option<String>,
    imports: Vec<ComponentContractImport>,
    capabilities: Vec<ContractCapability>,
    dependencies: Vec<ComponentContractDependency>,
    symbol_imports: Vec<ComponentContractSymbolImport>,
    symbol_exports: Vec<ComponentContractSymbolExport>,
}

impl ComponentContract {
    pub const fn component_id(&self) -> [u8; 16] {
        self.component_id
    }

    pub const fn abi_id(&self) -> [u8; 16] {
        self.abi_id
    }

    pub fn init(&self) -> Option<&str> {
        self.init.as_deref()
    }

    pub fn fini(&self) -> Option<&str> {
        self.fini.as_deref()
    }

    pub fn tls_offset_symbol(&self) -> Option<&str> {
        self.tls_offset_symbol.as_deref()
    }

    pub fn imports(&self) -> &[ComponentContractImport] {
        &self.imports
    }

    pub fn capabilities(&self) -> &[ContractCapability] {
        &self.capabilities
    }

    pub fn dependencies(&self) -> &[ComponentContractDependency] {
        &self.dependencies
    }

    pub fn symbol_imports(&self) -> &[ComponentContractSymbolImport] {
        &self.symbol_imports
    }

    pub fn symbol_exports(&self) -> &[ComponentContractSymbolExport] {
        &self.symbol_exports
    }
}

impl ProgramContract {
    pub fn entry(&self) -> &str {
        &self.entry
    }

    pub fn imports(&self) -> &[ContractImport] {
        &self.imports
    }

    pub fn capabilities(&self) -> &[ContractCapability] {
        &self.capabilities
    }

    pub const fn runtime(&self) -> RuntimeContract {
        self.runtime
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    manifest_version: u16,
    abi_epoch: u16,
    entry: String,
    imports: Vec<ManifestImport>,
    capabilities: Vec<ManifestCapability>,
    runtime: ManifestRuntime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestImport {
    operation: String,
    required: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestCapability {
    requirement: String,
    rights: Vec<String>,
    required: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestRuntime {
    stack_size: u64,
    stack_guard_size: u64,
    start_info_max_size: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComponentManifest {
    manifest_version: u16,
    abi_epoch: u16,
    component_id: String,
    abi_id: String,
    #[serde(default)]
    init: Option<String>,
    #[serde(default)]
    fini: Option<String>,
    #[serde(default)]
    tls_offset_symbol: Option<String>,
    #[serde(default)]
    imports: Vec<ComponentManifestImport>,
    #[serde(default)]
    capabilities: Vec<ManifestCapability>,
    #[serde(default)]
    dependencies: Vec<ComponentManifestDependency>,
    #[serde(default)]
    symbol_imports: Vec<ComponentManifestSymbolImport>,
    symbol_exports: Vec<ComponentManifestSymbolExport>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComponentManifestImport {
    operation: String,
    required: bool,
    slot_symbol: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComponentManifestDependency {
    component_id: String,
    abi_id: String,
    #[serde(default)]
    content_hash: Option<String>,
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComponentManifestSymbolImport {
    dependency_component_id: String,
    interface_id: String,
    symbol_id: String,
    signature_hash: String,
    binding_symbol: String,
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComponentManifestSymbolExport {
    interface_id: String,
    symbol_id: String,
    signature_hash: String,
    symbol: String,
    name: String,
}

pub fn parse_manifest(source: &str) -> Result<ProgramContract, ContractError> {
    let manifest: Manifest = serde_json::from_str(source).map_err(|error| {
        ContractError::new(
            ContractErrorKind::InvalidJson,
            format!("manifest JSON 无效: {error}"),
        )
    })?;
    if manifest.manifest_version != 1 || manifest.abi_epoch != native_abi::ABI_EPOCH {
        return Err(ContractError::new(
            ContractErrorKind::InvalidVersion,
            "manifest_version 必须为 1 且 abi_epoch 必须匹配 Native ABI",
        ));
    }
    validate_entry(&manifest.entry)?;
    let imports = normalize_imports(manifest.imports)?;
    let capabilities = normalize_capabilities(manifest.capabilities, false)?;
    let runtime = normalize_runtime(manifest.runtime)?;
    Ok(ProgramContract {
        entry: manifest.entry,
        imports,
        capabilities,
        runtime,
    })
}

pub fn parse_component_manifest(source: &str) -> Result<ComponentContract, ContractError> {
    let manifest: ComponentManifest = serde_json::from_str(source).map_err(|error| {
        ContractError::new(
            ContractErrorKind::InvalidJson,
            format!("component manifest JSON 无效: {error}"),
        )
    })?;
    if manifest.manifest_version != 1 || manifest.abi_epoch != native_abi::ABI_EPOCH {
        return Err(ContractError::new(
            ContractErrorKind::InvalidVersion,
            "manifest_version 必须为 1 且 abi_epoch 必须匹配 Native ABI",
        ));
    }
    let component_id = parse_hex::<16>(&manifest.component_id, "component_id")?;
    let abi_id = parse_hex::<16>(&manifest.abi_id, "abi_id")?;
    if component_id == [0; 16] || abi_id == [0; 16] {
        return Err(ContractError::new(
            ContractErrorKind::InvalidIdentity,
            "component_id 与 abi_id 不能为零",
        ));
    }
    for entry in manifest
        .init
        .iter()
        .chain(manifest.fini.iter())
        .chain(manifest.tls_offset_symbol.iter())
    {
        validate_symbol_name(entry)?;
    }
    let imports = normalize_component_imports(manifest.imports)?;
    let capabilities = normalize_capabilities(manifest.capabilities, true)?;
    let dependencies = normalize_dependencies(manifest.dependencies)?;
    let symbol_imports = normalize_symbol_imports(manifest.symbol_imports, &dependencies)?;
    let symbol_exports = normalize_symbol_exports(manifest.symbol_exports)?;
    Ok(ComponentContract {
        component_id,
        abi_id,
        init: manifest.init,
        fini: manifest.fini,
        tls_offset_symbol: manifest.tls_offset_symbol,
        imports,
        capabilities,
        dependencies,
        symbol_imports,
        symbol_exports,
    })
}

fn normalize_component_imports(
    imports: Vec<ComponentManifestImport>,
) -> Result<Vec<ComponentContractImport>, ContractError> {
    if imports.len() > MAX_IMPORTS as usize {
        return Err(ContractError::new(
            ContractErrorKind::TooManyImports,
            "imports 数量超过 SOYO 上限",
        ));
    }
    let mut normalized = Vec::with_capacity(imports.len());
    for import in imports {
        validate_symbol_name(&import.slot_symbol)?;
        let spec = OPERATIONS
            .iter()
            .find(|spec| spec.name == import.operation)
            .ok_or_else(|| {
                ContractError::new(
                    ContractErrorKind::UnknownOperation,
                    format!("未知 Native ABI operation {}", import.operation),
                )
            })?;
        if normalized.iter().any(|item: &ComponentContractImport| {
            item.operation == spec.id || item.slot_symbol == import.slot_symbol
        }) {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateOperation,
                format!("重复 operation 或 slot symbol {}", import.operation),
            ));
        }
        normalized.push(ComponentContractImport {
            operation: spec.id,
            required: import.required,
            slot_symbol: import.slot_symbol,
        });
    }
    normalized.sort_by_key(|import| import.operation as u32);
    Ok(normalized)
}

fn normalize_dependencies(
    dependencies: Vec<ComponentManifestDependency>,
) -> Result<Vec<ComponentContractDependency>, ContractError> {
    if dependencies.len() > soyo::registry::MAX_COMPONENT_DEPENDENCIES as usize {
        return Err(ContractError::new(
            ContractErrorKind::TooManyCapabilities,
            "component dependencies 数量超过 SOYO 上限",
        ));
    }
    let mut normalized = Vec::with_capacity(dependencies.len());
    for dependency in dependencies {
        let component_id = parse_hex::<16>(&dependency.component_id, "dependency component_id")?;
        let abi_id = parse_hex::<16>(&dependency.abi_id, "dependency abi_id")?;
        if component_id == [0; 16] || abi_id == [0; 16] {
            return Err(ContractError::new(
                ContractErrorKind::InvalidIdentity,
                "dependency identity 不能为零",
            ));
        }
        let content_hash = dependency
            .content_hash
            .as_deref()
            .map(|value| parse_hex::<32>(value, "dependency content_hash"))
            .transpose()?;
        validate_diagnostic_name(&dependency.name)?;
        if normalized.iter().any(|item: &ComponentContractDependency| {
            item.component_id == component_id && item.abi_id == abi_id
        }) {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateDependency,
                "重复 component dependency",
            ));
        }
        normalized.push(ComponentContractDependency {
            component_id,
            abi_id,
            content_hash,
            name: dependency.name,
        });
    }
    normalized.sort_by_key(|dependency| (dependency.component_id, dependency.abi_id));
    Ok(normalized)
}

fn normalize_symbol_imports(
    imports: Vec<ComponentManifestSymbolImport>,
    dependencies: &[ComponentContractDependency],
) -> Result<Vec<ComponentContractSymbolImport>, ContractError> {
    if imports.len() > soyo::registry::MAX_SYMBOL_IMPORTS as usize {
        return Err(ContractError::new(
            ContractErrorKind::TooManyImports,
            "symbol imports 数量超过 SOYO 上限",
        ));
    }
    let mut normalized = Vec::with_capacity(imports.len());
    for import in imports {
        let dependency_id = parse_hex::<16>(
            &import.dependency_component_id,
            "symbol import dependency_component_id",
        )?;
        let dependency_index = dependencies
            .iter()
            .position(|dependency| dependency.component_id == dependency_id)
            .ok_or_else(|| {
                ContractError::new(
                    ContractErrorKind::MissingDependency,
                    "symbol import 引用了未声明 dependency",
                )
            })? as u32;
        let interface_id = parse_nonzero_hex::<16>(&import.interface_id, "interface_id")?;
        let symbol_id = parse_nonzero_hex::<16>(&import.symbol_id, "symbol_id")?;
        let signature_hash =
            parse_nonzero_hex::<32>(&import.signature_hash, "signature_hash")?;
        validate_symbol_name(&import.binding_symbol)?;
        validate_diagnostic_name(&import.name)?;
        if normalized.iter().any(|item: &ComponentContractSymbolImport| {
            (item.dependency_index, item.interface_id, item.symbol_id)
                == (dependency_index, interface_id, symbol_id)
                || item.binding_symbol == import.binding_symbol
        }) {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateSymbol,
                "重复 symbol import 或 binding symbol",
            ));
        }
        normalized.push(ComponentContractSymbolImport {
            dependency_index,
            interface_id,
            symbol_id,
            signature_hash,
            binding_symbol: import.binding_symbol,
            name: import.name,
        });
    }
    normalized.sort_by_key(|import| (import.dependency_index, import.interface_id, import.symbol_id));
    Ok(normalized)
}

fn normalize_symbol_exports(
    exports: Vec<ComponentManifestSymbolExport>,
) -> Result<Vec<ComponentContractSymbolExport>, ContractError> {
    if exports.is_empty() || exports.len() > soyo::registry::MAX_SYMBOL_EXPORTS as usize {
        return Err(ContractError::new(
            ContractErrorKind::InvalidSymbol,
            "symbol exports 数量必须位于 SOYO 上限内且不能为空",
        ));
    }
    let mut normalized = Vec::with_capacity(exports.len());
    for export in exports {
        let interface_id = parse_nonzero_hex::<16>(&export.interface_id, "interface_id")?;
        let symbol_id = parse_nonzero_hex::<16>(&export.symbol_id, "symbol_id")?;
        let signature_hash =
            parse_nonzero_hex::<32>(&export.signature_hash, "signature_hash")?;
        validate_symbol_name(&export.symbol)?;
        validate_diagnostic_name(&export.name)?;
        if normalized.iter().any(|item: &ComponentContractSymbolExport| {
            (item.interface_id, item.symbol_id) == (interface_id, symbol_id)
                || item.symbol == export.symbol
        }) {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateSymbol,
                "重复 symbol export 或 ELF symbol",
            ));
        }
        normalized.push(ComponentContractSymbolExport {
            interface_id,
            symbol_id,
            signature_hash,
            symbol: export.symbol,
            name: export.name,
        });
    }
    normalized.sort_by_key(|export| (export.interface_id, export.symbol_id));
    Ok(normalized)
}

fn parse_nonzero_hex<const N: usize>(
    value: &str,
    field: &str,
) -> Result<[u8; N], ContractError> {
    let parsed = parse_hex(value, field)?;
    if parsed == [0; N] {
        return Err(ContractError::new(
            ContractErrorKind::InvalidIdentity,
            format!("{field} 不能为零"),
        ));
    }
    Ok(parsed)
}

fn parse_hex<const N: usize>(value: &str, field: &str) -> Result<[u8; N], ContractError> {
    if value.len() != N * 2 || !value.is_ascii() {
        return Err(ContractError::new(
            ContractErrorKind::InvalidIdentity,
            format!("{field} 必须是 {} 位十六进制", N * 2),
        ));
    }
    let mut output = [0; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0]).ok_or_else(|| {
            ContractError::new(
                ContractErrorKind::InvalidIdentity,
                format!("{field} 包含非十六进制字符"),
            )
        })?;
        let low = hex_nibble(pair[1]).ok_or_else(|| {
            ContractError::new(
                ContractErrorKind::InvalidIdentity,
                format!("{field} 包含非十六进制字符"),
            )
        })?;
        output[index] = high << 4 | low;
    }
    Ok(output)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn validate_symbol_name(name: &str) -> Result<(), ContractError> {
    if name.is_empty() || name.len() > 255 || name.as_bytes().contains(&0) {
        return Err(ContractError::new(
            ContractErrorKind::InvalidSymbol,
            "ELF symbol 必须是 1..=255 字节的非 NUL 名称",
        ));
    }
    Ok(())
}

fn validate_diagnostic_name(name: &str) -> Result<(), ContractError> {
    if name.len() > 255 || name.as_bytes().contains(&0) {
        return Err(ContractError::new(
            ContractErrorKind::InvalidSymbol,
            "诊断名称必须是不超过 255 字节的非 NUL 字符串",
        ));
    }
    Ok(())
}

fn validate_entry(entry: &str) -> Result<(), ContractError> {
    if entry.is_empty() || entry.len() > 255 || entry.as_bytes().contains(&0) {
        return Err(ContractError::new(
            ContractErrorKind::InvalidEntry,
            "entry 必须是 1..=255 字节的非 NUL 符号名",
        ));
    }
    Ok(())
}

fn normalize_imports(imports: Vec<ManifestImport>) -> Result<Vec<ContractImport>, ContractError> {
    if imports.is_empty() || imports.len() > MAX_IMPORTS as usize {
        return Err(ContractError::new(
            ContractErrorKind::TooManyImports,
            "imports 数量必须位于 SOYO 限制内且不能为空",
        ));
    }
    let mut normalized = Vec::with_capacity(imports.len());
    for import in imports {
        let spec = OPERATIONS
            .iter()
            .find(|spec| spec.name == import.operation)
            .ok_or_else(|| {
                ContractError::new(
                    ContractErrorKind::UnknownOperation,
                    format!("未知 Native ABI operation {}", import.operation),
                )
            })?;
        if normalized
            .iter()
            .any(|item: &ContractImport| item.operation == spec.id)
        {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateOperation,
                format!("重复声明 Native ABI operation {}", spec.name),
            ));
        }
        normalized.push(ContractImport {
            operation: spec.id,
            required: import.required,
        });
    }
    normalized.sort_by_key(|import| import.operation as u32);
    Ok(normalized)
}

fn normalize_capabilities(
    capabilities: Vec<ManifestCapability>,
    allow_empty: bool,
) -> Result<Vec<ContractCapability>, ContractError> {
    if capabilities.len() > MAX_CAPABILITIES as usize || !allow_empty && capabilities.is_empty() {
        return Err(ContractError::new(
            ContractErrorKind::TooManyCapabilities,
            "capabilities 数量必须位于 SOYO 限制内且不能为空",
        ));
    }
    let mut normalized = Vec::with_capacity(capabilities.len());
    for capability in capabilities {
        let spec = REQUIREMENTS
            .iter()
            .find(|spec| spec.name == capability.requirement)
            .ok_or_else(|| {
                ContractError::new(
                    ContractErrorKind::UnknownRequirement,
                    format!("未知 capability requirement {}", capability.requirement),
                )
            })?;
        if normalized
            .iter()
            .any(|item: &ContractCapability| item.requirement == spec.id)
        {
            return Err(ContractError::new(
                ContractErrorKind::DuplicateRequirement,
                format!("重复声明 capability requirement {}", spec.name),
            ));
        }
        let mut rights = Rights::NONE;
        for name in capability.rights {
            let right = right_by_name(&name).ok_or_else(|| {
                ContractError::new(
                    ContractErrorKind::UnknownRight,
                    format!("未知 capability right {name}"),
                )
            })?;
            if right.right.is_subset_of(rights) {
                return Err(ContractError::new(
                    ContractErrorKind::DuplicateRight,
                    format!("capability {} 重复声明 right {name}", spec.name),
                ));
            }
            rights |= right.right;
        }
        if !rights.is_subset_of(spec.max_rights) {
            return Err(ContractError::new(
                ContractErrorKind::RightsExceeded,
                format!("capability {} 请求了超出 registry 的权限", spec.name),
            ));
        }
        normalized.push(ContractCapability {
            requirement: spec.id,
            rights,
            required: capability.required,
        });
    }
    normalized.sort_by_key(|capability| capability.requirement as u32);
    Ok(normalized)
}

fn normalize_runtime(runtime: ManifestRuntime) -> Result<RuntimeContract, ContractError> {
    if !(64 * 1024..=64 * 1024 * 1024).contains(&runtime.stack_size)
        || runtime.stack_size % PAGE_SIZE != 0
        || !(PAGE_SIZE..=1024 * 1024).contains(&runtime.stack_guard_size)
        || runtime.stack_guard_size % PAGE_SIZE != 0
        || runtime.start_info_max_size < native_wire::START_INFO_SIZE as u32
        || runtime.start_info_max_size > 1024 * 1024
    {
        return Err(ContractError::new(
            ContractErrorKind::InvalidRuntime,
            "runtime 栈、护栅或 StartInfo 大小不符合 SOYO 约束",
        ));
    }
    Ok(RuntimeContract {
        stack_size: runtime.stack_size,
        stack_guard_size: runtime.stack_guard_size,
        start_info_max_size: runtime.start_info_max_size,
    })
}

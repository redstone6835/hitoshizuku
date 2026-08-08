//! JSON manifest 到格式无关程序契约的归一化边界。

use std::fmt;

use native_abi::{
    OPERATIONS, OperationId, REQUIREMENTS, RequirementId, Rights, operation, requirement,
    right_by_name, wire as native_wire,
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
    let capabilities = normalize_capabilities(manifest.capabilities)?;
    validate_operation_authority(&imports, &capabilities)?;
    let runtime = normalize_runtime(manifest.runtime)?;
    Ok(ProgramContract {
        entry: manifest.entry,
        imports,
        capabilities,
        runtime,
    })
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
) -> Result<Vec<ContractCapability>, ContractError> {
    if capabilities.is_empty() || capabilities.len() > MAX_CAPABILITIES as usize {
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

fn validate_operation_authority(
    imports: &[ContractImport],
    capabilities: &[ContractCapability],
) -> Result<(), ContractError> {
    for import in imports.iter().filter(|import| import.required) {
        let spec = operation(import.operation).expect("已从 registry 归一化 operation");
        let Some(interface) = spec.interface else {
            continue;
        };
        let satisfied = capabilities
            .iter()
            .filter(|capability| capability.required)
            .filter_map(|capability| {
                requirement(capability.requirement).map(|spec| (capability, spec))
            })
            .any(|(capability, requirement)| {
                requirement.interface == interface
                    && spec.required_rights.is_subset_of(capability.rights)
            });
        if !satisfied {
            return Err(ContractError::new(
                ContractErrorKind::MissingCapability,
                format!(
                    "required operation {} 缺少匹配的 required capability",
                    spec.name
                ),
            ));
        }
    }
    Ok(())
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

use std::collections::{BTreeMap, BTreeSet};

use elm::{
    ELM_API_FEATURES_V1, ELM_API_ROOT_IMPORT_CONTRACT, ELM_API_ROOT_IMPORT_NAME,
    ELM_API_ROOT_SLOT_SYMBOL, ELM_API_VERSION_V1, ELM_EBI_EXPORT_FLAG_DIRECT_PINNED,
    ELM_EBI_IMPORT_FLAG_DIRECT_PINNED, ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL, ELM_META_FIELD_ACCESS,
    ELM_META_FIELD_CONTRACT, ELM_META_FIELD_DIRECTION, ELM_META_FIELD_FLAGS,
    ELM_META_FIELD_HANDLER_CONTRACT, ELM_META_FIELD_MAX_VERSION, ELM_META_FIELD_MIN_VERSION,
    ELM_META_FIELD_MODE, ELM_META_FIELD_NAME, ELM_META_FIELD_PAYLOAD_CONTRACT,
    ELM_META_FIELD_POINT, ELM_META_FIELD_PRIORITY, ELM_META_FIELD_RUST_ABI, ELM_META_FIELD_STAGE,
    ELM_META_FIELD_SYMBOL, ELM_META_FIELD_TARGET, ELM_META_FIELD_VERSION, ELM_META_FIELD_WIRE_SIZE,
    ELM_MODULE_DESCRIPTOR_SYMBOL, ElmKernelMixinKind, ElmMixinMode, ElmPortAccessPolicy,
    ElmRustMetadataKind, ElmRustMetadataRecord, FlowDirection, FlowMode,
    parse_rust_metadata_section, sha256,
};

pub fn retain_linked_kernel_symbol_imports(
    imports: &mut Vec<ImportSpec>,
    mut is_linked: impl FnMut(&str) -> bool,
) {
    imports.retain(|import| {
        import.flags & ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL == 0
            || import.slot_symbol.as_deref().is_some_and(&mut is_linked)
    });
}

#[derive(Debug, Clone)]
pub struct NativeMetadata {
    pub module_descriptor: String,
    pub imports: Vec<ImportSpec>,
    pub exports: Vec<ExportSpec>,
    pub providers: Vec<ProviderSpec>,
    pub extension_points: Vec<ExtensionPointSpec>,
    pub extensions: Vec<ExtensionSpec>,
    pub kernel_mixins: Vec<KernelMixinSpec>,
    pub api_root_import_index: u32,
    pub api_versions: Vec<u16>,
    pub api_required_features: u64,
}

#[derive(Debug, Clone)]
pub struct ImportSpec {
    pub slot_symbol: Option<String>,
    pub name: String,
    pub contract: String,
    pub min_version: u32,
    pub max_version: u32,
    pub flags: u32,
    pub rust_abi_hash: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct ExportSpec {
    pub symbol: String,
    pub name: String,
    pub contract: String,
    pub version: u32,
    pub flags: u32,
    pub rust_abi_hash: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct ProviderSpec {
    pub contract: String,
    pub access: ElmPortAccessPolicy,
    pub direction: FlowDirection,
    pub mode: FlowMode,
    pub flags: u32,
    pub handler_symbol: String,
    pub snapshot_symbol: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExtensionPointSpec {
    pub point: String,
    pub contract: String,
    pub mode: ElmMixinMode,
}

#[derive(Debug, Clone)]
pub struct ExtensionSpec {
    pub target: String,
    pub point: String,
    pub contract: String,
    pub handler_contract: String,
    pub priority: i32,
}

#[derive(Debug, Clone)]
pub struct KernelMixinSpec {
    pub target_api: String,
    pub selector: String,
    pub handler_symbol: String,
    pub kind: ElmKernelMixinKind,
    pub flags: u16,
    pub priority: i32,
    pub handler_abi_hash: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct PayloadSpec {
    pub contract: String,
    pub wire_size: u32,
}

#[derive(Debug, Clone)]
struct SnapshotSpec {
    contract: String,
    symbol: String,
}

impl NativeMetadata {
    pub fn parse(section: &[u8]) -> Result<Self, String> {
        let records = parse_rust_metadata_section(section)
            .map_err(|err| format!("解析 .elm.meta 失败: {err:?}"))?;
        if records.is_empty() {
            return Err(".elm.meta 不包含任何 ELM Rust 元数据记录".to_string());
        }
        let mut module_descriptor = None;
        let mut imports = vec![ImportSpec {
            slot_symbol: Some(ELM_API_ROOT_SLOT_SYMBOL.to_string()),
            name: ELM_API_ROOT_IMPORT_NAME.to_string(),
            contract: ELM_API_ROOT_IMPORT_CONTRACT.to_string(),
            min_version: u32::from(ELM_API_VERSION_V1),
            max_version: u32::MAX,
            flags: 0,
            rust_abi_hash: [0; 32],
        }];
        let mut exports = Vec::new();
        let mut providers = Vec::new();
        let mut snapshots = Vec::new();
        let mut extension_points = Vec::new();
        let mut extensions = Vec::new();
        let mut kernel_mixins = Vec::new();
        let mut payloads = Vec::new();
        for record in &records {
            if record.flags != 0 {
                return Err(format!("元数据记录 {:?} 使用了未知 flags", record.kind));
            }
            match record.kind {
                ElmRustMetadataKind::Module => {
                    expect_fields(record, &[ELM_META_FIELD_SYMBOL])?;
                    let symbol = field_string(record, ELM_META_FIELD_SYMBOL)?;
                    if symbol != ELM_MODULE_DESCRIPTOR_SYMBOL {
                        return Err(format!("未知统一模块描述符符号 {symbol}"));
                    }
                    if module_descriptor.replace(symbol).is_some() {
                        return Err("一个 ELM 只能声明一个 #[elm::module]".to_string());
                    }
                }
                ElmRustMetadataKind::Lifecycle | ElmRustMetadataKind::Entry => {
                    return Err(
                        "独立生命周期与 entry 元数据已废止；请使用 #[elm::module] 和 ElmModule trait"
                            .to_string(),
                    );
                }
                ElmRustMetadataKind::Provider => {
                    expect_fields(
                        record,
                        &[
                            ELM_META_FIELD_SYMBOL,
                            ELM_META_FIELD_CONTRACT,
                            ELM_META_FIELD_FLAGS,
                            ELM_META_FIELD_ACCESS,
                            ELM_META_FIELD_DIRECTION,
                            ELM_META_FIELD_MODE,
                        ],
                    )?;
                    providers.push(ProviderSpec {
                        handler_symbol: field_string(record, ELM_META_FIELD_SYMBOL)?,
                        contract: field_string(record, ELM_META_FIELD_CONTRACT)?,
                        flags: field_u32(record, ELM_META_FIELD_FLAGS)?,
                        access: ElmPortAccessPolicy::from_raw(field_u32(
                            record,
                            ELM_META_FIELD_ACCESS,
                        )?)
                        .ok_or_else(|| "provider access 元数据无效".to_string())?,
                        direction: FlowDirection::from_raw(field_u32(
                            record,
                            ELM_META_FIELD_DIRECTION,
                        )?)
                        .ok_or_else(|| "provider direction 元数据无效".to_string())?,
                        mode: FlowMode::from_raw(field_u32(record, ELM_META_FIELD_MODE)?)
                            .ok_or_else(|| "provider mode 元数据无效".to_string())?,
                        snapshot_symbol: None,
                    });
                }
                ElmRustMetadataKind::ProviderSnapshot => {
                    expect_fields(record, &[ELM_META_FIELD_SYMBOL, ELM_META_FIELD_CONTRACT])?;
                    snapshots.push(SnapshotSpec {
                        symbol: field_string(record, ELM_META_FIELD_SYMBOL)?,
                        contract: field_string(record, ELM_META_FIELD_CONTRACT)?,
                    });
                }
                ElmRustMetadataKind::Export => {
                    let flags = field_u32(record, ELM_META_FIELD_FLAGS)?;
                    let direct = flags & ELM_EBI_EXPORT_FLAG_DIRECT_PINNED != 0;
                    let expected: &[u16] = if direct {
                        &[
                            ELM_META_FIELD_SYMBOL,
                            ELM_META_FIELD_NAME,
                            ELM_META_FIELD_CONTRACT,
                            ELM_META_FIELD_VERSION,
                            ELM_META_FIELD_FLAGS,
                            ELM_META_FIELD_RUST_ABI,
                        ]
                    } else {
                        &[
                            ELM_META_FIELD_SYMBOL,
                            ELM_META_FIELD_NAME,
                            ELM_META_FIELD_CONTRACT,
                            ELM_META_FIELD_VERSION,
                            ELM_META_FIELD_FLAGS,
                        ]
                    };
                    expect_fields(record, expected)?;
                    exports.push(ExportSpec {
                        symbol: field_string(record, ELM_META_FIELD_SYMBOL)?,
                        name: field_string(record, ELM_META_FIELD_NAME)?,
                        contract: field_string(record, ELM_META_FIELD_CONTRACT)?,
                        version: field_u32(record, ELM_META_FIELD_VERSION)?,
                        flags,
                        rust_abi_hash: rust_abi_hash(record, direct)?,
                    });
                }
                ElmRustMetadataKind::Import => {
                    let flags = field_u32(record, ELM_META_FIELD_FLAGS)?;
                    let direct = flags
                        & (ELM_EBI_IMPORT_FLAG_DIRECT_PINNED | ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL)
                        != 0;
                    let expected: &[u16] = if direct {
                        &[
                            ELM_META_FIELD_SYMBOL,
                            ELM_META_FIELD_NAME,
                            ELM_META_FIELD_CONTRACT,
                            ELM_META_FIELD_MIN_VERSION,
                            ELM_META_FIELD_MAX_VERSION,
                            ELM_META_FIELD_FLAGS,
                            ELM_META_FIELD_RUST_ABI,
                        ]
                    } else {
                        &[
                            ELM_META_FIELD_SYMBOL,
                            ELM_META_FIELD_NAME,
                            ELM_META_FIELD_CONTRACT,
                            ELM_META_FIELD_MIN_VERSION,
                            ELM_META_FIELD_MAX_VERSION,
                            ELM_META_FIELD_FLAGS,
                        ]
                    };
                    expect_fields(record, expected)?;
                    imports.push(ImportSpec {
                        slot_symbol: Some(field_string(record, ELM_META_FIELD_SYMBOL)?),
                        name: field_string(record, ELM_META_FIELD_NAME)?,
                        contract: field_string(record, ELM_META_FIELD_CONTRACT)?,
                        min_version: field_u32(record, ELM_META_FIELD_MIN_VERSION)?,
                        max_version: field_u32(record, ELM_META_FIELD_MAX_VERSION)?,
                        flags,
                        rust_abi_hash: rust_abi_hash(record, direct)?,
                    });
                }
                ElmRustMetadataKind::ExtensionPoint => {
                    expect_fields(
                        record,
                        &[
                            ELM_META_FIELD_CONTRACT,
                            ELM_META_FIELD_MODE,
                            ELM_META_FIELD_POINT,
                            ELM_META_FIELD_STAGE,
                            ELM_META_FIELD_PAYLOAD_CONTRACT,
                        ],
                    )?;
                    let contract = field_string(record, ELM_META_FIELD_CONTRACT)?;
                    if field_string(record, ELM_META_FIELD_PAYLOAD_CONTRACT)? != contract {
                        return Err(
                            "mixin point 的 payload contract 与 point contract 不一致".to_string()
                        );
                    }
                    let stage = checked_stage(field_u32(record, ELM_META_FIELD_STAGE)?)?;
                    let point = field_string(record, ELM_META_FIELD_POINT)?;
                    validate_stage_point(&point, stage)?;
                    let mode = ElmMixinMode::from_raw(field_u32(record, ELM_META_FIELD_MODE)?)
                        .ok_or_else(|| "mixin point mode 元数据无效".to_string())?;
                    if mode != expected_stage_mode(stage) {
                        return Err(format!("mixin point {point} 的 stage 与 mode 不一致"));
                    }
                    extension_points.push(ExtensionPointSpec {
                        point,
                        contract,
                        mode,
                    });
                }
                ElmRustMetadataKind::Extension => {
                    expect_fields(
                        record,
                        &[
                            ELM_META_FIELD_CONTRACT,
                            ELM_META_FIELD_TARGET,
                            ELM_META_FIELD_POINT,
                            ELM_META_FIELD_STAGE,
                            ELM_META_FIELD_PRIORITY,
                            ELM_META_FIELD_HANDLER_CONTRACT,
                            ELM_META_FIELD_PAYLOAD_CONTRACT,
                        ],
                    )?;
                    let contract = field_string(record, ELM_META_FIELD_CONTRACT)?;
                    if field_string(record, ELM_META_FIELD_PAYLOAD_CONTRACT)? != contract {
                        return Err(
                            "mixin 的 payload contract 与 point contract 不一致".to_string()
                        );
                    }
                    let stage = checked_stage(field_u32(record, ELM_META_FIELD_STAGE)?)?;
                    let point = field_string(record, ELM_META_FIELD_POINT)?;
                    validate_stage_point(&point, stage)?;
                    extensions.push(ExtensionSpec {
                        target: field_string(record, ELM_META_FIELD_TARGET)?,
                        point,
                        contract,
                        handler_contract: field_string(record, ELM_META_FIELD_HANDLER_CONTRACT)?,
                        priority: field_i32(record, ELM_META_FIELD_PRIORITY)?,
                    });
                }
                ElmRustMetadataKind::KernelMixin => {
                    expect_fields(
                        record,
                        &[
                            ELM_META_FIELD_SYMBOL,
                            ELM_META_FIELD_NAME,
                            ELM_META_FIELD_FLAGS,
                            ELM_META_FIELD_MODE,
                            ELM_META_FIELD_POINT,
                            ELM_META_FIELD_PRIORITY,
                            ELM_META_FIELD_RUST_ABI,
                        ],
                    )?;
                    let kind_raw = u16::try_from(field_u32(record, ELM_META_FIELD_MODE)?)
                        .map_err(|_| "内核 Mixin kind 超出 u16".to_string())?;
                    let kind = ElmKernelMixinKind::from_raw(kind_raw)
                        .ok_or_else(|| "内核 Mixin kind 无效".to_string())?;
                    let flags = u16::try_from(field_u32(record, ELM_META_FIELD_FLAGS)?)
                        .map_err(|_| "内核 Mixin flags 超出 u16".to_string())?;
                    if flags != kind.required_flags() {
                        return Err("内核 Mixin kind 与 flags 不一致".to_string());
                    }
                    let handler_abi = field_string(record, ELM_META_FIELD_RUST_ABI)?;
                    if handler_abi != kernel_symbols::KERNEL_MIXIN_HANDLER_RUST_ABI_V1 {
                        return Err("内核 Mixin handler ABI 不符合 v1".to_string());
                    }
                    kernel_mixins.push(KernelMixinSpec {
                        target_api: field_string(record, ELM_META_FIELD_NAME)?,
                        selector: field_string(record, ELM_META_FIELD_POINT)?,
                        handler_symbol: field_string(record, ELM_META_FIELD_SYMBOL)?,
                        kind,
                        flags,
                        priority: field_i32(record, ELM_META_FIELD_PRIORITY)?,
                        handler_abi_hash: sha256(handler_abi.as_bytes()),
                    });
                }
                ElmRustMetadataKind::Payload => {
                    expect_fields(
                        record,
                        &[ELM_META_FIELD_PAYLOAD_CONTRACT, ELM_META_FIELD_WIRE_SIZE],
                    )?;
                    let wire_size = field_u32(record, ELM_META_FIELD_WIRE_SIZE)?;
                    if wire_size > 256 {
                        return Err("ELM payload 元数据超过 v1 的 256 字节上限".to_string());
                    }
                    payloads.push(PayloadSpec {
                        contract: field_string(record, ELM_META_FIELD_PAYLOAD_CONTRACT)?,
                        wire_size,
                    });
                }
            }
        }
        validate_and_sort(
            &mut imports,
            &mut exports,
            &mut providers,
            snapshots,
            &mut extension_points,
            &mut extensions,
            &mut kernel_mixins,
            &mut payloads,
        )?;
        let module_descriptor =
            module_descriptor.ok_or_else(|| "ELM 必须且只能声明一个 #[elm::module]".to_string())?;
        Ok(Self {
            module_descriptor,
            imports,
            exports,
            providers,
            extension_points,
            extensions,
            kernel_mixins,
            api_root_import_index: 0,
            api_versions: vec![ELM_API_VERSION_V1],
            api_required_features: ELM_API_FEATURES_V1,
        })
    }

    pub fn symbol_names(&self) -> Vec<String> {
        let mut names = BTreeSet::new();
        names.insert(self.module_descriptor.clone());
        for import in &self.imports {
            if let Some(slot_symbol) = &import.slot_symbol {
                names.insert(slot_symbol.clone());
            }
        }
        for export in &self.exports {
            names.insert(export.symbol.clone());
        }
        for provider in &self.providers {
            names.insert(provider.handler_symbol.clone());
            if let Some(snapshot) = &provider.snapshot_symbol {
                names.insert(snapshot.clone());
            }
        }
        for mixin in &self.kernel_mixins {
            names.insert(mixin.handler_symbol.clone());
        }
        names.into_iter().collect()
    }
}

fn validate_and_sort(
    imports: &mut Vec<ImportSpec>,
    exports: &mut Vec<ExportSpec>,
    providers: &mut Vec<ProviderSpec>,
    snapshots: Vec<SnapshotSpec>,
    extension_points: &mut Vec<ExtensionPointSpec>,
    extensions: &mut Vec<ExtensionSpec>,
    kernel_mixins: &mut Vec<KernelMixinSpec>,
    payloads: &mut Vec<PayloadSpec>,
) -> Result<(), String> {
    imports[1..].sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.contract.cmp(&right.contract))
            .then_with(|| left.slot_symbol.cmp(&right.slot_symbol))
    });
    if imports.windows(2).any(|items| {
        items[0].name == items[1].name
            && items[0].contract == items[1].contract
            && items[0].min_version == items[1].min_version
            && items[0].max_version == items[1].max_version
    }) {
        return Err("重复 native import".to_string());
    }
    exports.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.contract.cmp(&right.contract))
            .then_with(|| left.version.cmp(&right.version))
    });
    if let Some(export) = exports.iter().find(|export| export.symbol != export.name) {
        return Err(format!(
            "native export {} 的符号必须与导出名称完全一致",
            export.name
        ));
    }
    if exports.windows(2).any(|items| {
        items[0].name == items[1].name
            && items[0].contract == items[1].contract
            && items[0].version == items[1].version
    }) {
        return Err("重复 native export".to_string());
    }
    let mut snapshot_by_contract = BTreeMap::new();
    for snapshot in snapshots {
        if snapshot_by_contract
            .insert(snapshot.contract.clone(), snapshot.symbol)
            .is_some()
        {
            return Err(format!("provider {} 重复声明 snapshot", snapshot.contract));
        }
    }
    providers.sort_by(|left, right| left.contract.cmp(&right.contract));
    if providers
        .windows(2)
        .any(|items| items[0].contract == items[1].contract)
    {
        return Err("同一 ELM 内 provider contract 必须唯一".to_string());
    }
    for provider in providers.iter_mut() {
        provider.snapshot_symbol = snapshot_by_contract.remove(&provider.contract);
    }
    if let Some(contract) = snapshot_by_contract.keys().next() {
        return Err(format!("snapshot {contract} 没有对应的 #[elm::provider]"));
    }
    extension_points.sort_by(|left, right| left.point.cmp(&right.point));
    if extension_points
        .windows(2)
        .any(|items| items[0].point == items[1].point)
    {
        return Err("重复 mixin point".to_string());
    }
    extensions.sort_by(|left, right| {
        left.target
            .cmp(&right.target)
            .then_with(|| left.point.cmp(&right.point))
            .then_with(|| right.priority.cmp(&left.priority))
    });
    if extensions
        .windows(2)
        .any(|items| items[0].target == items[1].target && items[0].point == items[1].point)
    {
        return Err("同一 ELM 不能重复挂接同一个目标补缀点".to_string());
    }
    kernel_mixins.sort_by(|left, right| {
        left.target_api
            .cmp(&right.target_api)
            .then_with(|| left.selector.cmp(&right.selector))
            .then_with(|| right.priority.cmp(&left.priority))
            .then_with(|| left.handler_symbol.cmp(&right.handler_symbol))
    });
    if kernel_mixins
        .windows(2)
        .any(|items| items[0].handler_symbol == items[1].handler_symbol)
    {
        return Err("重复内核 Mixin handler symbol".to_string());
    }
    for mixin in kernel_mixins.iter() {
        if mixin.target_api.is_empty()
            || mixin.target_api.len() > elm::ELM_EBI_SYMBOL_NAME_LEN
            || mixin.selector.is_empty()
            || mixin.selector.len() > elm::ELM_EBI_KERNEL_MIXIN_SELECTOR_LEN
            || mixin.handler_symbol.is_empty()
            || mixin.handler_symbol.len() > elm::ELM_EBI_SYMBOL_NAME_LEN
        {
            return Err("内核 Mixin 元数据字段超过 EBI v1 上限".to_string());
        }
    }
    payloads.sort_by(|left, right| left.contract.cmp(&right.contract));
    if payloads
        .windows(2)
        .any(|items| items[0].contract == items[1].contract)
    {
        return Err("重复 payload contract".to_string());
    }
    for point in extension_points.iter() {
        if !payloads
            .iter()
            .any(|payload| payload.contract == point.contract)
        {
            return Err(format!(
                "mixin point {} 缺少对应的 #[elm::payload(\"{}\")] 类型",
                point.point, point.contract
            ));
        }
    }
    for extension in extensions {
        let Some(payload) = payloads
            .iter()
            .find(|payload| payload.contract == extension.contract)
        else {
            return Err(format!(
                "mixin {} 缺少对应的 #[elm::payload(\"{}\")] 类型",
                extension.point, extension.contract
            ));
        };
        if payload.wire_size > 256 {
            return Err(format!("mixin {} 的固定帧超过 256 字节", extension.point));
        }
        let Some(provider) = providers
            .iter()
            .find(|provider| provider.contract == extension.handler_contract)
        else {
            return Err(format!(
                "mixin {} 缺少 handler provider {}",
                extension.point, extension.handler_contract
            ));
        };
        if provider.access != ElmPortAccessPolicy::ExtensionOnly
            || provider.direction != FlowDirection::Control
            || provider.mode != FlowMode::Shared
            || provider.flags != 0
        {
            return Err(format!(
                "mixin {} 的 handler provider {} 不符合 extension-only/control/shared 约束",
                extension.point, extension.handler_contract
            ));
        }
    }
    Ok(())
}

fn checked_stage(stage: u32) -> Result<u32, String> {
    if (1..=4).contains(&stage) {
        Ok(stage)
    } else {
        Err(format!("未知 mixin stage={stage}"))
    }
}

fn validate_stage_point(point: &str, stage: u32) -> Result<(), String> {
    let suffix = match stage {
        1 => ".ingress",
        2 => ".substitute",
        3 => ".egress",
        4 => ".observe",
        _ => return Err(format!("未知 mixin stage={stage}")),
    };
    if point.len() > suffix.len() && point.ends_with(suffix) {
        Ok(())
    } else {
        Err(format!("mixin point {point} 与 stage={stage} 不一致"))
    }
}

fn expected_stage_mode(stage: u32) -> ElmMixinMode {
    match stage {
        2 => ElmMixinMode::Exclusive,
        4 => ElmMixinMode::Observer,
        _ => ElmMixinMode::Chain,
    }
}

fn expect_fields(record: &ElmRustMetadataRecord<'_>, expected: &[u16]) -> Result<(), String> {
    if record.fields.len() != expected.len()
        || !expected
            .iter()
            .all(|tag| record.fields.iter().any(|field| field.tag == *tag))
    {
        return Err(format!("元数据记录 {:?} 的字段集合不符合 v1", record.kind));
    }
    Ok(())
}

fn field_string(record: &ElmRustMetadataRecord<'_>, tag: u16) -> Result<String, String> {
    record
        .require_field(tag)
        .map_err(|_| format!("元数据 {:?} 缺少字段 {tag}", record.kind))?
        .utf8()
        .map(str::to_string)
        .map_err(|_| format!("元数据 {:?} 字段 {tag} 不是 UTF-8", record.kind))
}

fn rust_abi_hash(record: &ElmRustMetadataRecord<'_>, required: bool) -> Result<[u8; 32], String> {
    if !required {
        return Ok([0; 32]);
    }
    let signature = field_string(record, ELM_META_FIELD_RUST_ABI)?;
    if signature.len() > 4096
        || signature.bytes().any(|byte| byte.is_ascii_whitespace())
        || !(signature.starts_with("fn(") || signature.starts_with("unsafefn("))
        || !signature.contains(")->")
    {
        return Err("直接固定符号使用了非规范 Rust ABI 签名".to_string());
    }
    Ok(sha256(signature.as_bytes()))
}

fn field_u32(record: &ElmRustMetadataRecord<'_>, tag: u16) -> Result<u32, String> {
    record
        .require_field(tag)
        .map_err(|_| format!("元数据 {:?} 缺少字段 {tag}", record.kind))?
        .u32()
        .map_err(|_| format!("元数据 {:?} 字段 {tag} 不是 u32", record.kind))
}

fn field_i32(record: &ElmRustMetadataRecord<'_>, tag: u16) -> Result<i32, String> {
    record
        .require_field(tag)
        .map_err(|_| format!("元数据 {:?} 缺少字段 {tag}", record.kind))?
        .i32()
        .map_err(|_| format!("元数据 {:?} 字段 {tag} 不是 i32", record.kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_metadata_is_rejected() {
        assert!(
            NativeMetadata::parse(&[])
                .unwrap_err()
                .contains("不包含任何")
        );
    }

    #[test]
    fn export_symbol_must_match_export_name() {
        let mut imports = vec![ImportSpec {
            slot_symbol: Some(ELM_API_ROOT_SLOT_SYMBOL.to_string()),
            name: ELM_API_ROOT_IMPORT_NAME.to_string(),
            contract: ELM_API_ROOT_IMPORT_CONTRACT.to_string(),
            min_version: 1,
            max_version: u32::MAX,
            flags: 0,
            rust_abi_hash: [0; 32],
        }];
        let mut exports = vec![ExportSpec {
            symbol: "hidden_symbol".to_string(),
            name: "public_symbol".to_string(),
            contract: "test.export@1".to_string(),
            version: 1,
            flags: 0,
            rust_abi_hash: [0; 32],
        }];
        let error = validate_and_sort(
            &mut imports,
            &mut exports,
            &mut Vec::new(),
            Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap_err();

        assert!(error.contains("符号必须与导出名称完全一致"));
    }

    #[test]
    fn mixin_stage_requires_canonical_point_suffix() {
        assert!(validate_stage_point("test.point.ingress", 1).is_ok());
        assert!(validate_stage_point("test.point.egress", 1).is_err());
        assert_eq!(expected_stage_mode(2), ElmMixinMode::Exclusive);
        assert_eq!(expected_stage_mode(4), ElmMixinMode::Observer);
    }

    #[test]
    fn unreferenced_kernel_symbol_metadata_is_not_projected() {
        let mut imports = vec![
            ImportSpec {
                slot_symbol: Some(ELM_API_ROOT_SLOT_SYMBOL.to_string()),
                name: ELM_API_ROOT_IMPORT_NAME.to_string(),
                contract: ELM_API_ROOT_IMPORT_CONTRACT.to_string(),
                min_version: 1,
                max_version: u32::MAX,
                flags: 0,
                rust_abi_hash: [0; 32],
            },
            ImportSpec {
                slot_symbol: Some("__elm_kernel_symbol_used".to_string()),
                name: "sched.used".to_string(),
                contract: "kernel.sched.used@1".to_string(),
                min_version: 1,
                max_version: 1,
                flags: ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL,
                rust_abi_hash: [1; 32],
            },
            ImportSpec {
                slot_symbol: Some("__elm_kernel_symbol_unused".to_string()),
                name: "sched.unused".to_string(),
                contract: "kernel.sched.unused@1".to_string(),
                min_version: 1,
                max_version: 1,
                flags: ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL,
                rust_abi_hash: [2; 32],
            },
        ];

        retain_linked_kernel_symbol_imports(&mut imports, |symbol| {
            symbol == "__elm_kernel_symbol_used"
        });

        assert_eq!(imports.len(), 2);
        assert_eq!(
            imports[0].slot_symbol.as_deref(),
            Some(ELM_API_ROOT_SLOT_SYMBOL)
        );
        assert_eq!(
            imports[1].slot_symbol.as_deref(),
            Some("__elm_kernel_symbol_used")
        );
    }
}

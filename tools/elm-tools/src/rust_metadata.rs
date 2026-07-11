use std::collections::{BTreeMap, BTreeSet};

use elm::{
    ELM_API_FEATURES_V1, ELM_API_ROOT_IMPORT_CONTRACT, ELM_API_ROOT_IMPORT_NAME,
    ELM_API_ROOT_SLOT_SYMBOL, ELM_API_VERSION_V1, ELM_META_FIELD_ACCESS, ELM_META_FIELD_CONTRACT,
    ELM_META_FIELD_DIRECTION, ELM_META_FIELD_FLAGS, ELM_META_FIELD_HANDLER_CONTRACT,
    ELM_META_FIELD_HOOK_KIND, ELM_META_FIELD_MAX_VERSION, ELM_META_FIELD_MIN_VERSION,
    ELM_META_FIELD_MODE, ELM_META_FIELD_NAME, ELM_META_FIELD_PAYLOAD_CONTRACT,
    ELM_META_FIELD_POINT, ELM_META_FIELD_PRIORITY, ELM_META_FIELD_STAGE, ELM_META_FIELD_SYMBOL,
    ELM_META_FIELD_TARGET, ELM_META_FIELD_VERSION, ELM_META_FIELD_WIRE_SIZE, ElmMixinMode,
    ElmPortAccessPolicy, ElmRustMetadataKind, ElmRustMetadataRecord, FlowDirection, FlowMode,
    parse_rust_metadata_section,
};

#[derive(Debug, Clone)]
pub struct NativeMetadata {
    pub lifecycle: Vec<LifecycleSpec>,
    pub entry: Option<String>,
    pub imports: Vec<ImportSpec>,
    pub exports: Vec<ExportSpec>,
    pub providers: Vec<ProviderSpec>,
    pub extension_points: Vec<ExtensionPointSpec>,
    pub extensions: Vec<ExtensionSpec>,
    pub api_root_import_index: u32,
    pub api_versions: Vec<u16>,
    pub api_required_features: u64,
}

#[derive(Debug, Clone)]
pub struct LifecycleSpec {
    pub kind: u32,
    pub symbol: String,
}

#[derive(Debug, Clone)]
pub struct ImportSpec {
    pub slot_symbol: String,
    pub name: String,
    pub contract: String,
    pub min_version: u32,
    pub max_version: u32,
    pub flags: u32,
}

#[derive(Debug, Clone)]
pub struct ExportSpec {
    pub symbol: String,
    pub name: String,
    pub contract: String,
    pub version: u32,
    pub flags: u32,
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
        let mut lifecycle = Vec::new();
        let mut entry = None;
        let mut imports = vec![ImportSpec {
            slot_symbol: ELM_API_ROOT_SLOT_SYMBOL.to_string(),
            name: ELM_API_ROOT_IMPORT_NAME.to_string(),
            contract: ELM_API_ROOT_IMPORT_CONTRACT.to_string(),
            min_version: u32::from(ELM_API_VERSION_V1),
            max_version: u32::MAX,
            flags: 0,
        }];
        let mut exports = Vec::new();
        let mut providers = Vec::new();
        let mut snapshots = Vec::new();
        let mut extension_points = Vec::new();
        let mut extensions = Vec::new();
        let mut payloads = Vec::new();
        for record in &records {
            if record.flags != 0 {
                return Err(format!("元数据记录 {:?} 使用了未知 flags", record.kind));
            }
            match record.kind {
                ElmRustMetadataKind::Lifecycle => {
                    expect_fields(record, &[ELM_META_FIELD_SYMBOL, ELM_META_FIELD_HOOK_KIND])?;
                    let kind = field_u32(record, ELM_META_FIELD_HOOK_KIND)?;
                    let symbol = field_string(record, ELM_META_FIELD_SYMBOL)?;
                    validate_lifecycle_symbol(kind, &symbol)?;
                    lifecycle.push(LifecycleSpec { kind, symbol });
                }
                ElmRustMetadataKind::Entry => {
                    expect_fields(record, &[ELM_META_FIELD_SYMBOL])?;
                    let symbol = field_string(record, ELM_META_FIELD_SYMBOL)?;
                    if entry.replace(symbol).is_some() {
                        return Err("一个 ELM 只能声明一个 #[elm::entry]".to_string());
                    }
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
                    expect_fields(
                        record,
                        &[
                            ELM_META_FIELD_SYMBOL,
                            ELM_META_FIELD_NAME,
                            ELM_META_FIELD_CONTRACT,
                            ELM_META_FIELD_VERSION,
                            ELM_META_FIELD_FLAGS,
                        ],
                    )?;
                    exports.push(ExportSpec {
                        symbol: field_string(record, ELM_META_FIELD_SYMBOL)?,
                        name: field_string(record, ELM_META_FIELD_NAME)?,
                        contract: field_string(record, ELM_META_FIELD_CONTRACT)?,
                        version: field_u32(record, ELM_META_FIELD_VERSION)?,
                        flags: field_u32(record, ELM_META_FIELD_FLAGS)?,
                    });
                }
                ElmRustMetadataKind::Import => {
                    expect_fields(
                        record,
                        &[
                            ELM_META_FIELD_SYMBOL,
                            ELM_META_FIELD_NAME,
                            ELM_META_FIELD_CONTRACT,
                            ELM_META_FIELD_MIN_VERSION,
                            ELM_META_FIELD_MAX_VERSION,
                            ELM_META_FIELD_FLAGS,
                        ],
                    )?;
                    imports.push(ImportSpec {
                        slot_symbol: field_string(record, ELM_META_FIELD_SYMBOL)?,
                        name: field_string(record, ELM_META_FIELD_NAME)?,
                        contract: field_string(record, ELM_META_FIELD_CONTRACT)?,
                        min_version: field_u32(record, ELM_META_FIELD_MIN_VERSION)?,
                        max_version: field_u32(record, ELM_META_FIELD_MAX_VERSION)?,
                        flags: field_u32(record, ELM_META_FIELD_FLAGS)?,
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
            &mut lifecycle,
            &mut imports,
            &mut exports,
            &mut providers,
            snapshots,
            &mut extension_points,
            &mut extensions,
            &mut payloads,
        )?;
        Ok(Self {
            lifecycle,
            entry,
            imports,
            exports,
            providers,
            extension_points,
            extensions,
            api_root_import_index: 0,
            api_versions: vec![ELM_API_VERSION_V1],
            api_required_features: ELM_API_FEATURES_V1,
        })
    }

    pub fn symbol_names(&self) -> Vec<String> {
        let mut names = BTreeSet::new();
        for hook in &self.lifecycle {
            names.insert(hook.symbol.clone());
        }
        if let Some(entry) = &self.entry {
            names.insert(entry.clone());
        }
        for import in &self.imports {
            names.insert(import.slot_symbol.clone());
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
        names.into_iter().collect()
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_and_sort(
    lifecycle: &mut Vec<LifecycleSpec>,
    imports: &mut Vec<ImportSpec>,
    exports: &mut Vec<ExportSpec>,
    providers: &mut Vec<ProviderSpec>,
    snapshots: Vec<SnapshotSpec>,
    extension_points: &mut Vec<ExtensionPointSpec>,
    extensions: &mut Vec<ExtensionSpec>,
    payloads: &mut Vec<PayloadSpec>,
) -> Result<(), String> {
    lifecycle.sort_by_key(|hook| hook.kind);
    for kind in [1, 2] {
        if lifecycle.iter().filter(|hook| hook.kind == kind).count() != 1 {
            return Err(format!("ELM 必须且只能声明一个 lifecycle hook kind={kind}"));
        }
    }
    if lifecycle
        .windows(2)
        .any(|hooks| hooks[0].kind == hooks[1].kind)
    {
        return Err("重复 lifecycle hook".to_string());
    }
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

fn validate_lifecycle_symbol(kind: u32, symbol: &str) -> Result<(), String> {
    let expected = match kind {
        1 => "on_initialize",
        2 => "on_finalize",
        3 => "on_migrate_export",
        4 => "on_migrate_import",
        5 => "on_migrate_abort",
        6 => "on_quiesce",
        7 => "on_pause",
        8 => "on_resume",
        _ => return Err(format!("未知 lifecycle hook kind={kind}")),
    };
    if symbol == expected {
        Ok(())
    } else {
        Err(format!("lifecycle kind={kind} 必须导出符号 {expected}"))
    }
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
        let mut lifecycle = vec![
            LifecycleSpec {
                kind: 1,
                symbol: "on_initialize".to_string(),
            },
            LifecycleSpec {
                kind: 2,
                symbol: "on_finalize".to_string(),
            },
        ];
        let mut imports = vec![ImportSpec {
            slot_symbol: ELM_API_ROOT_SLOT_SYMBOL.to_string(),
            name: ELM_API_ROOT_IMPORT_NAME.to_string(),
            contract: ELM_API_ROOT_IMPORT_CONTRACT.to_string(),
            min_version: 1,
            max_version: u32::MAX,
            flags: 0,
        }];
        let mut exports = vec![ExportSpec {
            symbol: "hidden_symbol".to_string(),
            name: "public_symbol".to_string(),
            contract: "test.export@1".to_string(),
            version: 1,
            flags: 0,
        }];
        let error = validate_and_sort(
            &mut lifecycle,
            &mut imports,
            &mut exports,
            &mut Vec::new(),
            Vec::new(),
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
}

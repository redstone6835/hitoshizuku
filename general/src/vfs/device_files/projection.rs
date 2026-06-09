//! 设备文件投影快照。
//!
//! dev core 目前仍通过 `DevNodeSpec` 描述 `/dev` 投影；本模块是 VFS 层集中理解
//! 该声明的唯一位置。devtmpfs/sysfs/procfs 只消费这里生成的只读快照，避免多个
//! 文件系统各自解析底层 function 的节点声明。

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::dev::enumerate::DEVICES;
use crate::dev::function::{CustomDevNodeKind, DevNodeSpec};

/// `/dev` 投影节点的通用类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceFileProjectionKind {
    Char,
    Block,
    Symlink,
    CustomChar,
    CustomBlock,
    CustomFile,
    CustomDirectory,
}

impl DeviceFileProjectionKind {
    /// 面向诊断视图的稳定短名称。
    pub const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::Char => "char",
            Self::Block => "block",
            Self::Symlink => "symlink",
            Self::CustomChar => "custom-char",
            Self::CustomBlock => "custom-block",
            Self::CustomFile => "custom-file",
            Self::CustomDirectory => "custom-dir",
        }
    }

    /// 该节点是否能在 `/sys/class/<class>` 中作为设备文件 class 成员展示。
    pub const fn has_device_class(self) -> bool {
        matches!(self, Self::Char | Self::Block | Self::CustomChar | Self::CustomBlock)
    }
}

/// 单个 function 声明的 `/dev` 投影节点快照。
#[derive(Clone, Debug)]
pub struct DeviceFileProjectionEntry {
    class_name: &'static str,
    function_name: String,
    node_name: String,
    target: Option<String>,
    kind: DeviceFileProjectionKind,
}

impl DeviceFileProjectionEntry {
    pub fn class_name(&self) -> &'static str {
        self.class_name
    }

    pub fn function_name(&self) -> &str {
        &self.function_name
    }

    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    pub fn kind(&self) -> DeviceFileProjectionKind {
        self.kind
    }

    pub fn diagnostic_len(&self) -> usize {
        self.kind
            .diagnostic_name()
            .len()
            .saturating_add(1)
            .saturating_add(self.node_name.len())
            .saturating_add(
                self.target
                    .as_ref()
                    .map(|target| "->".len().saturating_add(target.len()))
                    .unwrap_or(0),
            )
    }

    pub fn write_diagnostic(&self, out: &mut String) {
        let _ = write!(out, "{}:{}", self.kind.diagnostic_name(), self.node_name);
        if let Some(target) = self.target.as_deref() {
            let _ = write!(out, "->{target}");
        }
    }
}

/// 单个 function 的 `/dev` 投影快照。
#[derive(Clone, Debug)]
pub struct DeviceFunctionProjectionSnapshot {
    class_name: &'static str,
    function_name: String,
    nodes: Vec<DeviceFileProjectionEntry>,
}

impl DeviceFunctionProjectionSnapshot {
    pub fn class_name(&self) -> &'static str {
        self.class_name
    }

    pub fn function_name(&self) -> &str {
        &self.function_name
    }

    pub fn nodes(&self) -> &[DeviceFileProjectionEntry] {
        &self.nodes
    }
}

/// 收集当前 function registry 的设备文件投影快照。
pub fn collect_function_projection_snapshots() -> Vec<DeviceFunctionProjectionSnapshot> {
    let mut out = Vec::new();
    for func in DEVICES.functions.try_list().unwrap_or_default() {
        let class_name = func.class_id().as_str();
        let Some(function_name) = fallible_string(func.dev_name()) else {
            continue;
        };
        let Some(nodes) = func.devnodes() else {
            push_function_snapshot(&mut out, class_name, function_name, Vec::new());
            continue;
        };
        let mut entries = Vec::new();
        for node in nodes.nodes() {
            let Some(entry) = projection_entry(class_name, &function_name, node) else {
                continue;
            };
            if entries.try_reserve(1).is_err() {
                break;
            }
            entries.push(entry);
        }
        push_function_snapshot(&mut out, class_name, function_name, entries);
    }
    out
}

/// 渲染 function registry 的 `/dev` 投影诊断表。
///
/// procfs/sysfs 都不应该各自解释 `DevNodeSpec`。本函数把 class、function 名称和
/// devtmpfs 投影声明统一格式化为调试文本；低内存时返回已生成前缀，避免诊断
/// 视图影响设备对象生命周期。
pub fn render_function_projection_diagnostics() -> String {
    let mut out = String::new();
    if out.try_reserve("class\tname\tdevnodes\n".len()).is_err() {
        return out;
    }
    out.push_str("class\tname\tdevnodes\n");
    for func in collect_function_projection_snapshots() {
        let nodes = func.nodes();
        let devnode_len = projection_list_len(nodes).unwrap_or(1);
        let line_reserve = func
            .class_name()
            .len()
            .saturating_add(func.function_name().len())
            .saturating_add(devnode_len)
            .saturating_add(3);
        if out.try_reserve(line_reserve).is_err() {
            return out;
        }
        let _ = write!(out, "{}\t{}\t", func.class_name(), func.function_name());
        if !nodes.is_empty() {
            for (idx, node) in nodes.iter().enumerate() {
                if idx != 0 {
                    out.push(',');
                }
                node.write_diagnostic(&mut out);
            }
        } else {
            out.push('-');
        }
        out.push('\n');
    }
    out
}

fn projection_list_len(nodes: &[DeviceFileProjectionEntry]) -> Option<usize> {
    if nodes.is_empty() {
        return None;
    }
    let names_len = nodes
        .iter()
        .fold(0usize, |acc, node| acc.saturating_add(node.diagnostic_len()));
    Some(names_len.saturating_add(nodes.len().saturating_sub(1)))
}

fn push_function_snapshot(
    out: &mut Vec<DeviceFunctionProjectionSnapshot>,
    class_name: &'static str,
    function_name: String,
    nodes: Vec<DeviceFileProjectionEntry>,
) {
    if out.try_reserve(1).is_err() {
        return;
    }
    out.push(DeviceFunctionProjectionSnapshot {
        class_name,
        function_name,
        nodes,
    });
}

fn projection_entry(
    class_name: &'static str,
    function_name: &str,
    node: &DevNodeSpec,
) -> Option<DeviceFileProjectionEntry> {
    let (node_name, target, kind) = match node {
        DevNodeSpec::Char { name, .. } => (name.as_ref(), None, DeviceFileProjectionKind::Char),
        DevNodeSpec::Block { name, .. } => (name.as_ref(), None, DeviceFileProjectionKind::Block),
        DevNodeSpec::Symlink { name, target } => {
            (name.as_ref(), Some(target.as_ref()), DeviceFileProjectionKind::Symlink)
        }
        DevNodeSpec::Custom(spec) => (spec.name(), None, custom_kind(spec.kind())),
    };
    Some(DeviceFileProjectionEntry {
        class_name,
        function_name: fallible_string(function_name)?,
        node_name: fallible_string(node_name)?,
        target: target.and_then(fallible_string),
        kind,
    })
}

fn custom_kind(kind: CustomDevNodeKind) -> DeviceFileProjectionKind {
    match kind {
        CustomDevNodeKind::CharDevice => DeviceFileProjectionKind::CustomChar,
        CustomDevNodeKind::BlockDevice => DeviceFileProjectionKind::CustomBlock,
        CustomDevNodeKind::RegularFile => DeviceFileProjectionKind::CustomFile,
        CustomDevNodeKind::Directory => DeviceFileProjectionKind::CustomDirectory,
    }
}

fn fallible_string(value: &str) -> Option<String> {
    let mut out = String::new();
    out.try_reserve(value.len()).ok()?;
    out.push_str(value);
    Some(out)
}

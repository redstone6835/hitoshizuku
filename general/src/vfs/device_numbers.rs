//! POSIX `dev_t` projection for device-like VFS nodes.
//!
//! This registry is intentionally part of the VFS compatibility layer.  The
//! core device model remains keyed by PnP identity and typed device objects,
//! not by major/minor numbers.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vfs::stat::DevId;
use vfs::sync::Spinlock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PosixDeviceKind {
    Char,
    Block,
}

#[derive(Clone, Debug)]
pub struct PosixDeviceRecord {
    pub kind: PosixDeviceKind,
    pub node_name: String,
    pub display_name: String,
    pub major_name: String,
    pub rdev: DevId,
}

#[derive(Clone, Debug)]
pub struct PosixMajorSummary {
    pub kind: PosixDeviceKind,
    pub major: u32,
    pub display_name: String,
}

struct PosixDeviceRegistry {
    next_char_private: Option<PrivateDynamicCursor>,
    next_block_private: Option<PrivateDynamicCursor>,
    records: Vec<PosixDeviceRecord>,
}

impl PosixDeviceRegistry {
    const fn new() -> Self {
        Self {
            next_char_private: Some(PrivateDynamicCursor::new(PRIVATE_DYNAMIC_MAJOR_START, 0)),
            next_block_private: Some(PrivateDynamicCursor::new(PRIVATE_DYNAMIC_MAJOR_START, 0)),
            records: Vec::new(),
        }
    }
}

static POSIX_DEVICES: Spinlock<PosixDeviceRegistry> = Spinlock::new(PosixDeviceRegistry::new());

// POSIX dev_t 仅是 stat/mknod/procfs 等接口的兼容投影；底层设备模型不以主次设备号寻址。
const PRIVATE_DYNAMIC_MAJOR_START: u32 = 240;

#[derive(Clone, Copy)]
struct PrivateDynamicCursor {
    major: u32,
    minor: u32,
}

impl PrivateDynamicCursor {
    const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }
}

pub fn register_char(node_name: &str, display_name: &str) -> Option<DevId> {
    register(PosixDeviceKind::Char, node_name, display_name)
}

pub fn register_block(node_name: &str, display_name: &str) -> Option<DevId> {
    register(PosixDeviceKind::Block, node_name, display_name)
}

fn register(kind: PosixDeviceKind, node_name: &str, display_name: &str) -> Option<DevId> {
    let mut registry = POSIX_DEVICES.lock();
    if let Some(record) = registry
        .records
        .iter()
        .find(|record| record.node_name == node_name)
    {
        return (record.kind == kind).then_some(record.rdev);
    }

    let (rdev, major_name) = match well_known_rdev(kind, node_name, display_name) {
        Some((rdev, major_name)) => (rdev, major_name.to_string()),
        None => allocate_private_rdev(&mut registry, kind)
            .map(|rdev| (rdev, display_name.to_string()))?,
    };

    if registry
        .records
        .iter()
        .any(|record| record.kind == kind && record.rdev == rdev)
    {
        return None;
    }

    registry.records.push(PosixDeviceRecord {
        kind,
        node_name: node_name.to_string(),
        display_name: display_name.to_string(),
        major_name,
        rdev,
    });
    Some(rdev)
}

fn allocate_private_rdev(
    registry: &mut PosixDeviceRegistry,
    kind: PosixDeviceKind,
) -> Option<DevId> {
    let next_private = match kind {
        PosixDeviceKind::Char => &mut registry.next_char_private,
        PosixDeviceKind::Block => &mut registry.next_block_private,
    };
    let cursor = (*next_private)?;
    *next_private = advance_private_cursor(cursor);
    Some(DevId::new(cursor.major, cursor.minor))
}

fn advance_private_cursor(cursor: PrivateDynamicCursor) -> Option<PrivateDynamicCursor> {
    match cursor.minor.checked_add(1) {
        Some(minor) => Some(PrivateDynamicCursor::new(cursor.major, minor)),
        None => cursor
            .major
            .checked_add(1)
            .map(|major| PrivateDynamicCursor::new(major, 0)),
    }
}

fn well_known_rdev(
    kind: PosixDeviceKind,
    node_name: &str,
    display_name: &str,
) -> Option<(DevId, &'static str)> {
    if kind != PosixDeviceKind::Char {
        return None;
    }

    let node_leaf = node_name.rsplit('/').next().unwrap_or(node_name);
    let name = match node_leaf {
        "null" | "zero" | "random" | "urandom" | "console" => node_leaf,
        _ => display_name,
    };
    match name {
        // Linux/POSIX well-known 字符设备号：1 是 mem 主设备，minor 区分 null/zero/random。
        "null" => Some((DevId::new(1, 3), "mem")),
        "zero" => Some((DevId::new(1, 5), "mem")),
        "random" => Some((DevId::new(1, 8), "mem")),
        "urandom" => Some((DevId::new(1, 9), "mem")),
        // Linux well-known 控制台设备号，供 /dev/console 和 stat(2) 兼容使用。
        "console" => Some((DevId::new(5, 1), "console")),
        _ => None,
    }
}

pub fn unregister_node(node_name: &str) {
    POSIX_DEVICES
        .lock()
        .records
        .retain(|record| record.node_name != node_name);
}

pub fn lookup_node(node_name: &str) -> Option<PosixDeviceRecord> {
    POSIX_DEVICES
        .lock()
        .records
        .iter()
        .find(|record| record.node_name == node_name)
        .cloned()
}

pub fn lookup_rdev(kind: PosixDeviceKind, rdev: DevId) -> Option<PosixDeviceRecord> {
    // 这是 POSIX 兼容层的快照查询，只能回答已投影的 VFS 节点；
    // 不能把 rdev 当作底层设备模型的反向入口或稳定设备身份。
    POSIX_DEVICES
        .lock()
        .records
        .iter()
        .find(|record| record.kind == kind && record.rdev == rdev)
        .cloned()
}

pub fn lookup_char(display_name: &str) -> Option<DevId> {
    lookup(PosixDeviceKind::Char, display_name)
}

pub fn lookup_block(display_name: &str) -> Option<DevId> {
    lookup(PosixDeviceKind::Block, display_name)
}

fn lookup(kind: PosixDeviceKind, display_name: &str) -> Option<DevId> {
    POSIX_DEVICES
        .lock()
        .records
        .iter()
        .find(|record| record.kind == kind && record.display_name == display_name)
        .map(|record| record.rdev)
}

pub fn lookup_char_record(display_name: &str) -> Option<PosixDeviceRecord> {
    lookup_record(PosixDeviceKind::Char, display_name)
}

pub fn lookup_block_record(display_name: &str) -> Option<PosixDeviceRecord> {
    lookup_record(PosixDeviceKind::Block, display_name)
}

fn lookup_record(kind: PosixDeviceKind, display_name: &str) -> Option<PosixDeviceRecord> {
    POSIX_DEVICES
        .lock()
        .records
        .iter()
        .find(|record| record.kind == kind && record.display_name == display_name)
        .cloned()
}

pub fn records() -> Vec<PosixDeviceRecord> {
    POSIX_DEVICES.lock().records.clone()
}

pub fn major_summaries(kind: PosixDeviceKind) -> Vec<PosixMajorSummary> {
    let mut summaries = Vec::new();
    for record in POSIX_DEVICES
        .lock()
        .records
        .iter()
        .filter(|record| record.kind == kind)
    {
        if summaries.iter().any(|summary: &PosixMajorSummary| {
            summary.kind == kind
                && summary.major == record.rdev.major
                && summary.display_name == record.major_name
        }) {
            continue;
        }
        summaries.push(PosixMajorSummary {
            kind,
            major: record.rdev.major,
            display_name: record.major_name.clone(),
        });
    }
    summaries.sort_by(|a, b| {
        a.major
            .cmp(&b.major)
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    summaries
}

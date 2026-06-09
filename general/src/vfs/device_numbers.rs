//! POSIX `dev_t` 兼容投影。
//!
//! 这个注册表只属于 VFS 兼容层：`stat(2)`、`/proc/devices`、`/sys/dev/*`
//! 需要主次设备号，但底层设备模型仍然以 PnP identity 和 typed device object
//! 为准，不能通过 `major/minor` 反向寻址硬件设备。

use alloc::string::String;
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

impl PosixDeviceRecord {
    fn try_clone_record(&self) -> Option<Self> {
        Some(Self {
            kind: self.kind,
            node_name: fallible_string_from(&self.node_name)?,
            display_name: fallible_string_from(&self.display_name)?,
            major_name: fallible_string_from(&self.major_name)?,
            rdev: self.rdev,
        })
    }
}

#[derive(Clone, Debug)]
pub struct PosixMajorSummary {
    pub kind: PosixDeviceKind,
    pub major: u32,
    pub display_name: String,
}

impl PosixMajorSummary {
    fn try_new(kind: PosixDeviceKind, major: u32, display_name: &str) -> Option<Self> {
        Some(Self {
            kind,
            major,
            display_name: fallible_string_from(display_name)?,
        })
    }
}

struct PosixDeviceRegistry {
    next_char_private: Option<PrivateDynamicCursor>,
    next_block_private: Option<PrivateDynamicCursor>,
    well_known: Vec<PosixDeviceNumberPolicy>,
    records: Vec<PosixDeviceRecord>,
}

impl PosixDeviceRegistry {
    const fn new() -> Self {
        Self {
            next_char_private: Some(PrivateDynamicCursor::new(PRIVATE_DYNAMIC_MAJOR_START, 0)),
            next_block_private: Some(PrivateDynamicCursor::new(PRIVATE_DYNAMIC_MAJOR_START, 0)),
            well_known: Vec::new(),
            records: Vec::new(),
        }
    }
}

static POSIX_DEVICES: Spinlock<PosixDeviceRegistry> = Spinlock::new(PosixDeviceRegistry::new());

// POSIX dev_t 仅是 stat/mknod/procfs 等接口的兼容投影；底层设备模型不以主次设备号寻址。
// TODO(posix-compat): private dynamic major 起点仍是兼容层固定策略。后续应作为
// device number allocator policy 的可配置字段安装，而不是写死在分配器本体中。
const PRIVATE_DYNAMIC_MAJOR_START: u32 = 240;

/// 兼容层声明的传统 POSIX 设备号策略。
///
/// 该策略只按 `/dev` 投影节点名匹配，不读取底层设备的固件名或 driver 名。这样
/// 固件节点即使叫作 `console`，也不会在未投影为 `/dev/console` 时获得控制台
/// 设备号，避免 POSIX 兼容策略反向污染底层设备身份。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PosixDeviceNumberPolicy {
    kind: PosixDeviceKind,
    node_name: &'static str,
    major: u32,
    minor: u32,
    major_name: &'static str,
}

impl PosixDeviceNumberPolicy {
    pub const fn char(
        node_name: &'static str,
        major: u32,
        minor: u32,
        major_name: &'static str,
    ) -> Self {
        Self {
            kind: PosixDeviceKind::Char,
            node_name,
            major,
            minor,
            major_name,
        }
    }

    pub const fn block(
        node_name: &'static str,
        major: u32,
        minor: u32,
        major_name: &'static str,
    ) -> Self {
        Self {
            kind: PosixDeviceKind::Block,
            node_name,
            major,
            minor,
            major_name,
        }
    }

    fn rdev(self) -> DevId {
        DevId::new(self.major, self.minor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PosixDevicePolicyError {
    Invalid,
    OutOfMemory,
    AlreadyRegistered,
    RdevConflict,
}

/// 注册一个 well-known POSIX 设备号策略。
///
/// 该接口属于 VFS 兼容层初始化，不属于底层设备注册流程。重复注册完全相同的
/// policy 视为幂等；同一节点名或同一 `dev_t` 被不同策略抢占时返回错误。
pub fn register_device_number_policy(
    policy: PosixDeviceNumberPolicy,
) -> Result<(), PosixDevicePolicyError> {
    if policy.node_name.is_empty() || policy.major_name.is_empty() {
        return Err(PosixDevicePolicyError::Invalid);
    }

    let mut registry = POSIX_DEVICES.lock();
    if let Some(existing) = registry
        .well_known
        .iter()
        .find(|entry| entry.kind == policy.kind && entry.node_name == policy.node_name)
    {
        if *existing == policy {
            return Ok(());
        }
        return Err(PosixDevicePolicyError::AlreadyRegistered);
    }
    if registry
        .well_known
        .iter()
        .any(|entry| entry.kind == policy.kind && entry.rdev() == policy.rdev())
    {
        return Err(PosixDevicePolicyError::RdevConflict);
    }
    if let Some(record) = registry
        .records
        .iter()
        .find(|record| record.kind == policy.kind && record.node_name == policy.node_name)
    {
        if record.rdev != policy.rdev() || record.major_name != policy.major_name {
            return Err(PosixDevicePolicyError::AlreadyRegistered);
        }
    }
    if registry.records.iter().any(|record| {
        record.kind == policy.kind
            && record.rdev == policy.rdev()
            && record.node_name != policy.node_name
    }) {
        return Err(PosixDevicePolicyError::RdevConflict);
    }

    registry
        .well_known
        .try_reserve(1)
        .map_err(|_| PosixDevicePolicyError::OutOfMemory)?;
    registry.well_known.push(policy);
    Ok(())
}

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

    let mut private_rdev = false;
    let (rdev, major_name) = match well_known_rdev(&registry, kind, node_name) {
        Some((rdev, major_name)) => (rdev, major_name),
        None => {
            private_rdev = true;
            (next_private_rdev(&registry, kind)?, display_name)
        }
    };

    if registry
        .records
        .iter()
        .any(|record| record.kind == kind && record.rdev == rdev)
    {
        return None;
    }

    push_record(
        &mut registry,
        kind,
        node_name,
        display_name,
        major_name,
        rdev,
    )?;
    if private_rdev {
        advance_private_rdev(&mut registry, kind);
    }
    Some(rdev)
}

fn fallible_string_from(value: &str) -> Option<String> {
    let mut out = String::new();
    out.try_reserve(value.len()).ok()?;
    out.push_str(value);
    Some(out)
}

fn push_record(
    registry: &mut PosixDeviceRegistry,
    kind: PosixDeviceKind,
    node_name: &str,
    display_name: &str,
    major_name: &str,
    rdev: DevId,
) -> Option<()> {
    // POSIX 设备号表位于兼容层，启动期和热插拔路径都可能调用。这里不用
    // `Vec::push`/`String::from` 的隐式分配路径，避免低内存时把普通注册失败
    // 放大成内核 panic。
    registry.records.try_reserve(1).ok()?;
    let node_name = fallible_string_from(node_name)?;
    let display_name = fallible_string_from(display_name)?;
    let major_name = fallible_string_from(major_name)?;
    registry.records.push(PosixDeviceRecord {
        kind,
        node_name,
        display_name,
        major_name,
        rdev,
    });
    Some(())
}

fn next_private_rdev(registry: &PosixDeviceRegistry, kind: PosixDeviceKind) -> Option<DevId> {
    let cursor = match kind {
        PosixDeviceKind::Char => registry.next_char_private,
        PosixDeviceKind::Block => registry.next_block_private,
    }?;
    Some(DevId::new(cursor.major, cursor.minor))
}

fn advance_private_rdev(registry: &mut PosixDeviceRegistry, kind: PosixDeviceKind) {
    let next_private = match kind {
        PosixDeviceKind::Char => &mut registry.next_char_private,
        PosixDeviceKind::Block => &mut registry.next_block_private,
    };
    if let Some(cursor) = *next_private {
        *next_private = advance_private_cursor(cursor);
    }
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
    registry: &PosixDeviceRegistry,
    kind: PosixDeviceKind,
    node_name: &str,
) -> Option<(DevId, &'static str)> {
    registry
        .well_known
        .iter()
        .find(|entry| entry.kind == kind && entry.node_name == node_name)
        .map(|entry| (entry.rdev(), entry.major_name))
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
        .and_then(PosixDeviceRecord::try_clone_record)
}

pub fn lookup_rdev(kind: PosixDeviceKind, rdev: DevId) -> Option<PosixDeviceRecord> {
    // 这是 POSIX 兼容层的快照查询，只能回答已投影的 VFS 节点；
    // 不能把 rdev 当作底层设备模型的反向入口或稳定设备身份。
    POSIX_DEVICES
        .lock()
        .records
        .iter()
        .find(|record| record.kind == kind && record.rdev == rdev)
        .and_then(PosixDeviceRecord::try_clone_record)
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
        .and_then(PosixDeviceRecord::try_clone_record)
}

pub fn records() -> Vec<PosixDeviceRecord> {
    try_records().unwrap_or_default()
}

/// 返回 POSIX 设备投影记录的 fallible 快照。
///
/// 诊断文件系统读取 `/sys/dev/*` 或 `/proc/devices` 时使用该接口，低内存下可以
/// 降级为空/不完整视图，而不影响底层设备对象和 devtmpfs inode 的真实生命周期。
pub fn try_records() -> Option<Vec<PosixDeviceRecord>> {
    let registry = POSIX_DEVICES.lock();
    let mut out = Vec::new();
    out.try_reserve(registry.records.len()).ok()?;
    for record in &registry.records {
        out.push(record.try_clone_record()?);
    }
    Some(out)
}

pub fn major_summaries(kind: PosixDeviceKind) -> Vec<PosixMajorSummary> {
    try_major_summaries(kind).unwrap_or_default()
}

/// 返回指定 POSIX 设备类别的 major 汇总。
///
/// 这是 `/proc/devices` 兼容视图专用快照，不提供 `major -> device` 的底层反向
/// 寻址能力。分配失败时返回 `None`，调用方应按空视图处理。
pub fn try_major_summaries(kind: PosixDeviceKind) -> Option<Vec<PosixMajorSummary>> {
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
        summaries.try_reserve(1).ok()?;
        summaries.push(PosixMajorSummary::try_new(
            kind,
            record.rdev.major,
            &record.major_name,
        )?);
    }
    summaries.sort_by(|a, b| {
        a.major
            .cmp(&b.major)
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    Some(summaries)
}

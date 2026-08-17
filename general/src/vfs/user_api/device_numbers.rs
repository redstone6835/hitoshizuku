//! 用户 ABI `dev_t` 投影。
//!
//! 这个注册表只属于 VFS 用户接口层：`stat(2)`、`/proc/devices`、`/sys/dev/*`
//! 需要主次设备号，但底层设备模型仍以 PnP identity 和 typed device object 为准，
//! 不能通过 `major/minor` 反向寻址硬件设备。

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use vfs::stat::DevId;
use vfs::sync::Spinlock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceNumberKind {
    Char,
    Block,
}

#[derive(Clone, Debug)]
pub struct DeviceNumberRecord {
    pub kind: DeviceNumberKind,
    pub node_name: String,
    pub display_name: String,
    pub major_name: String,
    pub rdev: DevId,
}

impl DeviceNumberRecord {
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
pub struct DeviceMajorSummary {
    pub kind: DeviceNumberKind,
    pub major: u32,
    pub display_name: String,
}

impl DeviceMajorSummary {
    fn try_new(kind: DeviceNumberKind, major: u32, display_name: &str) -> Option<Self> {
        Some(Self {
            kind,
            major,
            display_name: fallible_string_from(display_name)?,
        })
    }
}

struct DeviceNumberRegistry {
    next_char_private: Option<PrivateDynamicCursor>,
    next_block_private: Option<PrivateDynamicCursor>,
    next_misc_minor: Option<u32>,
    well_known: Vec<DeviceNumberPolicy>,
    records: Vec<DeviceNumberRecord>,
    /// 动态分配的已释放 (major, minor),优先复用(Linux idr 语义)。
    freed: BTreeSet<(u32, u32)>,
}

impl DeviceNumberRegistry {
    const fn new() -> Self {
        Self {
            next_char_private: Some(PrivateDynamicCursor::new(PRIVATE_DYNAMIC_MAJOR_START, 0)),
            next_block_private: Some(PrivateDynamicCursor::new(PRIVATE_DYNAMIC_MAJOR_START, 0)),
            next_misc_minor: Some(0),
            well_known: Vec::new(),
            records: Vec::new(),
            freed: BTreeSet::new(),
        }
    }
}

static DEVICE_NUMBERS: Spinlock<DeviceNumberRegistry> = Spinlock::new(DeviceNumberRegistry::new());

// `dev_t` 仅是 stat/mknod/procfs 等接口的用户可见投影；底层设备模型不以主次设备号寻址。
// private dynamic major 起点属于用户 ABI 策略。集中在本模块内可以避免散落到
// devtmpfs/sysfs/procfs 或底层设备对象中。
const PRIVATE_DYNAMIC_MAJOR_START: u32 = 240;
const MISC_MAJOR: u32 = 10;
const MISC_MAJOR_NAME: &str = "misc";

/// 用户 ABI 层声明的传统设备号策略。
///
/// 该策略只按 `/dev` 投影节点名匹配，不读取底层设备的固件名或 driver 名。这样
/// 固件节点即使叫作 `console`，也不会在未投影为 `/dev/console` 时获得控制台
/// 设备号，避免用户 ABI 策略反向污染底层设备身份。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceNumberPolicy {
    kind: DeviceNumberKind,
    node_name: &'static str,
    major: u32,
    minor: u32,
    major_name: &'static str,
}

impl DeviceNumberPolicy {
    pub const fn char(
        node_name: &'static str,
        major: u32,
        minor: u32,
        major_name: &'static str,
    ) -> Self {
        Self {
            kind: DeviceNumberKind::Char,
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
            kind: DeviceNumberKind::Block,
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
pub enum DeviceNumberPolicyError {
    Invalid,
    OutOfMemory,
    AlreadyRegistered,
    RdevConflict,
}

/// 注册一个 well-known 设备号策略。
///
/// 该接口属于 VFS 用户接口层初始化，不属于底层设备注册流程。重复注册完全相同的
/// policy 视为幂等；同一节点名或同一 `dev_t` 被不同策略抢占时返回错误。
pub fn register_device_number_policy(
    policy: DeviceNumberPolicy,
) -> Result<(), DeviceNumberPolicyError> {
    if policy.node_name.is_empty() || policy.major_name.is_empty() {
        return Err(DeviceNumberPolicyError::Invalid);
    }

    let mut registry = DEVICE_NUMBERS.lock();
    if let Some(existing) = registry
        .well_known
        .iter()
        .find(|entry| entry.kind == policy.kind && entry.node_name == policy.node_name)
    {
        if *existing == policy {
            return Ok(());
        }
        return Err(DeviceNumberPolicyError::AlreadyRegistered);
    }
    if registry
        .well_known
        .iter()
        .any(|entry| entry.kind == policy.kind && entry.rdev() == policy.rdev())
    {
        return Err(DeviceNumberPolicyError::RdevConflict);
    }
    if let Some(record) = registry
        .records
        .iter()
        .find(|record| record.kind == policy.kind && record.node_name == policy.node_name)
    {
        if record.rdev != policy.rdev() || record.major_name != policy.major_name {
            return Err(DeviceNumberPolicyError::AlreadyRegistered);
        }
    }
    if registry.records.iter().any(|record| {
        record.kind == policy.kind
            && record.rdev == policy.rdev()
            && record.node_name != policy.node_name
    }) {
        return Err(DeviceNumberPolicyError::RdevConflict);
    }

    registry
        .well_known
        .try_reserve(1)
        .map_err(|_| DeviceNumberPolicyError::OutOfMemory)?;
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
    register(DeviceNumberKind::Char, node_name, display_name)
}

/// Register a Linux misc character device with a dynamically allocated minor.
///
/// Misc devices share character major 10. The minor number is an ABI-facing
/// projection detail and is not used for reverse lookup into the device model.
pub fn register_misc_char(node_name: &str, display_name: &str) -> Option<DevId> {
    let mut registry = DEVICE_NUMBERS.lock();
    if let Some(record) = registry
        .records
        .iter()
        .find(|record| record.node_name == node_name)
    {
        return (record.kind == DeviceNumberKind::Char
            && record.rdev.major == MISC_MAJOR
            && record.major_name == MISC_MAJOR_NAME)
            .then_some(record.rdev);
    }

    let minor = next_misc_minor(&registry)?;
    let rdev = DevId::new(MISC_MAJOR, minor);
    push_record(
        &mut registry,
        DeviceNumberKind::Char,
        node_name,
        display_name,
        MISC_MAJOR_NAME,
        rdev,
    )?;
    registry.next_misc_minor = minor.checked_add(1);
    Some(rdev)
}

/// pts slave 的主号(Linux 一致:136)。
pub const PTY_MAJOR: u32 = 136;
const PTY_MAJOR_NAME: &str = "pts";

fn next_pty_minor(registry: &DeviceNumberRegistry) -> Option<u32> {
    if let Some(&(_, minor)) = registry.freed.iter().find(|(major, _)| *major == PTY_MAJOR) {
        return Some(minor);
    }
    let mut minor = 0u32;
    loop {
        if registry.records.iter().all(|record| {
            record.kind != DeviceNumberKind::Char
                || record.rdev.major != PTY_MAJOR
                || record.rdev.minor != minor
        }) {
            return Some(minor);
        }
        minor = minor.checked_add(1)?;
    }
}

/// 登记一个 pts slave 的呈现设备号(136:N)。
///
/// 节点名使用合成键 `pts/<N>`;devpts 节点在独立挂载中,devtmpfs 的
/// mknod 反查不会命中它(符合"设备号只是呈现层"的边界)。
pub fn register_pty(index: u32) -> Option<DevId> {
    let node_name = alloc::format!("pts/{index}");
    let mut registry = DEVICE_NUMBERS.lock();
    if let Some(record) = registry
        .records
        .iter()
        .find(|record| record.node_name == node_name)
    {
        return (record.kind == DeviceNumberKind::Char && record.rdev.major == PTY_MAJOR)
            .then_some(record.rdev);
    }
    let minor = next_pty_minor(&registry)?;
    let rdev = DevId::new(PTY_MAJOR, minor);
    push_record(
        &mut registry,
        DeviceNumberKind::Char,
        &node_name,
        &node_name,
        PTY_MAJOR_NAME,
        rdev,
    )?;
    Some(rdev)
}

/// 注销 pts slave 呈现设备号(配对销毁时)。
pub fn unregister_pty(index: u32) {
    let node_name = alloc::format!("pts/{index}");
    unregister_node(&node_name);
}

/// 查询 pts slave 的呈现设备号(devpts 节点 inode 用)。
pub fn pty_rdev(index: u32) -> DevId {
    let node_name = alloc::format!("pts/{index}");
    lookup_node(&node_name)
        .map(|record| record.rdev)
        .unwrap_or_else(|| DevId::new(PTY_MAJOR, index))
}

pub fn register_block(node_name: &str, display_name: &str) -> Option<DevId> {
    register(DeviceNumberKind::Block, node_name, display_name)
}

fn register(kind: DeviceNumberKind, node_name: &str, display_name: &str) -> Option<DevId> {
    let mut registry = DEVICE_NUMBERS.lock();
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
    registry.freed.remove(&(rdev.major, rdev.minor));
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
    registry: &mut DeviceNumberRegistry,
    kind: DeviceNumberKind,
    node_name: &str,
    display_name: &str,
    major_name: &str,
    rdev: DevId,
) -> Option<()> {
    // 设备号表位于用户接口层，启动期和热插拔路径都可能调用。这里不用
    // `Vec::push`/`String::from` 的隐式分配路径，避免低内存时把普通注册失败
    // 放大成内核 panic。
    {
        // 设备号记录会在节点解绑时释放；全局表扩容后的容量则由内核继续复用。
        let _accounting = allocator::suspend_implicit_allocation_accounting()?;
        registry.records.try_reserve(1).ok()?;
    }
    let node_name = fallible_string_from(node_name)?;
    let display_name = fallible_string_from(display_name)?;
    let major_name = fallible_string_from(major_name)?;
    registry.records.push(DeviceNumberRecord {
        kind,
        node_name,
        display_name,
        major_name,
        rdev,
    });
    Some(())
}

fn next_private_rdev(registry: &DeviceNumberRegistry, kind: DeviceNumberKind) -> Option<DevId> {
    let cursor = match kind {
        DeviceNumberKind::Char => registry.next_char_private,
        DeviceNumberKind::Block => registry.next_block_private,
    }?;
    if let Some(&(major, minor)) = registry
        .freed
        .iter()
        .find(|(major, _)| *major >= PRIVATE_DYNAMIC_MAJOR_START)
    {
        return Some(DevId::new(major, minor));
    }
    Some(DevId::new(cursor.major, cursor.minor))
}

fn advance_private_rdev(registry: &mut DeviceNumberRegistry, kind: DeviceNumberKind) {
    let next_private = match kind {
        DeviceNumberKind::Char => &mut registry.next_char_private,
        DeviceNumberKind::Block => &mut registry.next_block_private,
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

fn next_misc_minor(registry: &DeviceNumberRegistry) -> Option<u32> {
    if let Some(&(_, minor)) = registry
        .freed
        .iter()
        .find(|(major, _)| *major == MISC_MAJOR)
    {
        return Some(minor);
    }
    let mut minor = registry.next_misc_minor?;
    loop {
        if registry.records.iter().all(|record| {
            record.kind != DeviceNumberKind::Char
                || record.rdev.major != MISC_MAJOR
                || record.rdev.minor != minor
        }) {
            return Some(minor);
        }
        minor = minor.checked_add(1)?;
    }
}

fn well_known_rdev(
    registry: &DeviceNumberRegistry,
    kind: DeviceNumberKind,
    node_name: &str,
) -> Option<(DevId, &'static str)> {
    registry
        .well_known
        .iter()
        .find(|entry| entry.kind == kind && entry.node_name == node_name)
        .map(|entry| (entry.rdev(), entry.major_name))
}

pub fn unregister_node(node_name: &str) {
    let mut registry = DEVICE_NUMBERS.lock();
    let Some(index) = registry
        .records
        .iter()
        .position(|record| record.node_name == node_name)
    else {
        return;
    };
    let record = registry.records.swap_remove(index);
    // well-known 策略的设备号由策略表固定,不进入复用池;动态分配的
    // (misc/private/pts)释放后回收,保证热插拔同名设备拿到原次号。
    let is_well_known = registry.well_known.iter().any(|policy| {
        policy.kind == record.kind
            && policy.node_name == record.node_name
            && policy.rdev() == record.rdev
    });
    if !is_well_known {
        registry
            .freed
            .insert((record.rdev.major, record.rdev.minor));
    }
}

pub fn lookup_node(node_name: &str) -> Option<DeviceNumberRecord> {
    DEVICE_NUMBERS
        .lock()
        .records
        .iter()
        .find(|record| record.node_name == node_name)
        .and_then(DeviceNumberRecord::try_clone_record)
}

pub fn lookup_rdev(kind: DeviceNumberKind, rdev: DevId) -> Option<DeviceNumberRecord> {
    // 这是用户 ABI 投影层的快照查询，只能回答已投影的 VFS 节点；
    // 不能把 rdev 当作底层设备模型的反向入口或稳定设备身份。
    DEVICE_NUMBERS
        .lock()
        .records
        .iter()
        .find(|record| record.kind == kind && record.rdev == rdev)
        .and_then(DeviceNumberRecord::try_clone_record)
}

pub fn lookup_char(display_name: &str) -> Option<DevId> {
    lookup(DeviceNumberKind::Char, display_name)
}

pub fn lookup_block(display_name: &str) -> Option<DevId> {
    lookup(DeviceNumberKind::Block, display_name)
}

fn lookup(kind: DeviceNumberKind, display_name: &str) -> Option<DevId> {
    DEVICE_NUMBERS
        .lock()
        .records
        .iter()
        .find(|record| record.kind == kind && record.display_name == display_name)
        .map(|record| record.rdev)
}

pub fn lookup_char_record(display_name: &str) -> Option<DeviceNumberRecord> {
    lookup_record(DeviceNumberKind::Char, display_name)
}

pub fn lookup_block_record(display_name: &str) -> Option<DeviceNumberRecord> {
    lookup_record(DeviceNumberKind::Block, display_name)
}

fn lookup_record(kind: DeviceNumberKind, display_name: &str) -> Option<DeviceNumberRecord> {
    DEVICE_NUMBERS
        .lock()
        .records
        .iter()
        .find(|record| record.kind == kind && record.display_name == display_name)
        .and_then(DeviceNumberRecord::try_clone_record)
}

pub fn records() -> Vec<DeviceNumberRecord> {
    try_records().unwrap_or_default()
}

/// 返回设备号投影记录的 fallible 快照。
///
/// 诊断文件系统读取 `/sys/dev/*` 或 `/proc/devices` 时使用该接口，低内存下可以
/// 降级为空/不完整视图，而不影响底层设备对象和 devtmpfs inode 的真实生命周期。
pub fn try_records() -> Option<Vec<DeviceNumberRecord>> {
    let registry = DEVICE_NUMBERS.lock();
    let mut out = Vec::new();
    out.try_reserve(registry.records.len()).ok()?;
    for record in &registry.records {
        out.push(record.try_clone_record()?);
    }
    Some(out)
}

pub fn major_summaries(kind: DeviceNumberKind) -> Vec<DeviceMajorSummary> {
    try_major_summaries(kind).unwrap_or_default()
}

/// 返回指定设备号类别的 major 汇总。
///
/// 这是 `/proc/devices` 兼容视图专用快照，不提供 `major -> device` 的底层反向
/// 寻址能力。分配失败时返回 `None`，调用方应按空视图处理。
pub fn try_major_summaries(kind: DeviceNumberKind) -> Option<Vec<DeviceMajorSummary>> {
    let mut summaries = Vec::new();
    for record in DEVICE_NUMBERS
        .lock()
        .records
        .iter()
        .filter(|record| record.kind == kind)
    {
        if summaries.iter().any(|summary: &DeviceMajorSummary| {
            summary.kind == kind
                && summary.major == record.rdev.major
                && summary.display_name == record.major_name
        }) {
            continue;
        }
        summaries.try_reserve(1).ok()?;
        summaries.push(DeviceMajorSummary::try_new(
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

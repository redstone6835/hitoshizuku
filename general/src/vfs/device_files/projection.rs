//! 设备文件投影快照。
//!
//! 本模块是 VFS 层集中生成和理解 `/dev` 投影的唯一位置。dev core 只暴露
//! typed function；devtmpfs/sysfs/procfs 只消费这里生成的只读快照，避免多个
//! 文件系统各自 downcast 或解释底层 function。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::Write;

use vfs::error::VfsError;
use vfs::sync::Spinlock;

use crate::dev::block::BlockDevice;
use crate::dev::char::CharDevice;
use crate::dev::enumerate::DEVICES;
use crate::dev::function::{
    BlockFunction, CharFunction, DeviceClassId, DeviceFunction, FunctionRegistry, function_as,
};
use crate::dev::rtc::RtcFunction;
use crate::vfs::device_files::rtc::RtcDevNodeEndpoint;
use crate::vfs::device_files::spec::{
    CustomDevNodeKind, CustomDevNodeSpec, DevNodeSet, DevNodeSpec, fallible_box_str,
};
use crate::vfs::user_api::device_numbers::{self, DeviceNumberKind};

/// 设备文件投影状态。
///
/// 状态表只记录 VFS/user ABI 视图的发布结果，不参与底层 function 生命周期。
/// 这样 devtmpfs 绑定失败可以被 procfs/sysfs 诊断，而不会反向破坏 PnP probe
/// 或虚拟设备注册事务。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceFileProjectionStateKind {
    Pending,
    Bound,
    Unbound,
    Failed,
}

impl DeviceFileProjectionStateKind {
    pub const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Bound => "bound",
            Self::Unbound => "unbound",
            Self::Failed => "failed",
        }
    }
}

/// 单个 function 的投影状态快照。
#[derive(Clone, Debug)]
pub struct DeviceFileProjectionStateSnapshot {
    class_name: &'static str,
    function_name: String,
    state: DeviceFileProjectionStateKind,
    errno: Option<i32>,
}

impl DeviceFileProjectionStateSnapshot {
    pub fn class_name(&self) -> &'static str {
        self.class_name
    }

    pub fn function_name(&self) -> &str {
        &self.function_name
    }

    pub fn state(&self) -> DeviceFileProjectionStateKind {
        self.state
    }

    pub fn errno(&self) -> Option<i32> {
        self.errno
    }
}

struct DeviceFileProjectionStateRecord {
    class_name: &'static str,
    function_name: String,
    state: DeviceFileProjectionStateKind,
    errno: Option<i32>,
}

static PROJECTION_STATES: Spinlock<Vec<DeviceFileProjectionStateRecord>> =
    Spinlock::new(Vec::new());

#[derive(Clone)]
struct PublishedDevNodeRecord {
    class_name: &'static str,
    function_name: String,
    nodes: DevNodeSet,
}

#[derive(Clone)]
struct PublishedDevNodeSnapshot {
    class_id: DeviceClassId,
    nodes: DevNodeSet,
}

static PUBLISHED_DEVNODES: Spinlock<Vec<PublishedDevNodeRecord>> = Spinlock::new(Vec::new());

/// function 设备文件投影构造函数。
///
/// 返回 `None` 表示当前 projector 不认识该 function；返回 `Some` 表示已经生成
/// 该 function 的完整 VFS 节点集合。投影构造不得修改底层设备生命周期，只能
/// 读取 typed function 并生成用户态名字空间声明。
pub type DeviceFileProjectorBuild = fn(&dyn DeviceFunction) -> Result<Option<DevNodeSet>, VfsError>;

/// VFS 设备文件 projector 声明。
///
/// projector registry 是 dev core 与 `/dev` 投影之间的扩展点。新增设备文件类型
/// 通过注册 projector 接入，不需要修改 devtmpfs/sysfs/procfs，也不需要把
/// `DevNodeSpec` 放回底层 function trait。
#[derive(Clone, Copy)]
pub struct DeviceFileProjector {
    owner: &'static str,
    name: &'static str,
    build: DeviceFileProjectorBuild,
}

impl DeviceFileProjector {
    pub const fn new(
        owner: &'static str,
        name: &'static str,
        build: DeviceFileProjectorBuild,
    ) -> Self {
        Self { owner, name, build }
    }

    pub const fn owner(self) -> &'static str {
        self.owner
    }

    pub const fn name(self) -> &'static str {
        self.name
    }

    fn build(self, func: &dyn DeviceFunction) -> Result<Option<DevNodeSet>, VfsError> {
        (self.build)(func)
    }
}

/// projector 注册结果。
///
/// `inserted=false` 表示同一 owner/name 已存在，本次只是幂等确认。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceFileProjectorRegistration {
    inserted: bool,
}

impl DeviceFileProjectorRegistration {
    pub const fn inserted(self) -> bool {
        self.inserted
    }
}

static DEVICE_FILE_PROJECTORS: Spinlock<Vec<DeviceFileProjector>> = Spinlock::new(Vec::new());

/// 注册一个 VFS function 设备文件 projector。
///
/// 同一 owner/name 重复注册视为幂等；同名 projector 被不同 owner 抢占时返回
/// `AlreadyExists`，避免两个适配器对同一种投影语义竞争解释权。
pub fn register_device_file_projector(
    projector: DeviceFileProjector,
) -> Result<DeviceFileProjectorRegistration, VfsError> {
    if projector.owner().is_empty() || projector.name().is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    let mut projectors = DEVICE_FILE_PROJECTORS.lock();
    if let Some(existing) = projectors
        .iter()
        .find(|entry| entry.name() == projector.name())
    {
        if existing.owner() == projector.owner() {
            return Ok(DeviceFileProjectorRegistration { inserted: false });
        }
        return Err(VfsError::AlreadyExists);
    }
    projectors
        .try_reserve(1)
        .map_err(|_| VfsError::OutOfMemory)?;
    projectors.push(projector);
    Ok(DeviceFileProjectorRegistration { inserted: true })
}

/// 注销一个 VFS function 设备文件 projector。
///
/// 注销只影响后续 function 投影生成；已经绑定到 devtmpfs 的节点仍由 function
/// unregister/remove 路径按节点集合解绑。该接口用于让可选 VFS 适配层具备完整的
/// 生命周期模型，而不是只能在启动期单向注册。
pub fn unregister_device_file_projector(owner: &'static str, name: &str) -> Result<(), VfsError> {
    let mut projectors = DEVICE_FILE_PROJECTORS.lock();
    let Some(index) = projectors
        .iter()
        .position(|entry| entry.owner() == owner && entry.name() == name)
    else {
        return Err(VfsError::NotFound);
    };
    projectors.remove(index);
    Ok(())
}

/// 注册当前 VFS 已支持的内建设备文件 projector。
///
/// 该函数只安装 VFS 侧投影规则，不注册底层设备或 devtmpfs 节点。启动期在
/// function 事件订阅前调用它，保证已经存在或随后出现的 function 都能被投影。
pub fn register_builtin_device_file_projectors() -> Result<(), VfsError> {
    for projector in BUILTIN_DEVICE_FILE_PROJECTORS.iter().copied() {
        register_device_file_projector(projector)?;
    }
    Ok(())
}

const BUILTIN_PROJECTOR_OWNER: &str = "device-files";
const BUILTIN_DEVICE_FILE_PROJECTORS: &[DeviceFileProjector] = &[
    DeviceFileProjector::new(BUILTIN_PROJECTOR_OWNER, "char", char_function_devnodes),
    DeviceFileProjector::new(BUILTIN_PROJECTOR_OWNER, "block", block_function_devnodes),
    DeviceFileProjector::new(BUILTIN_PROJECTOR_OWNER, "rtc", rtc_function_devnodes),
];

/// 标记一个 function 的 devtmpfs 投影进入 pending。
pub fn mark_projection_pending(func: &dyn DeviceFunction) {
    update_projection_state(func, DeviceFileProjectionStateKind::Pending, None);
}

/// 标记一个 function 的 devtmpfs 投影已成功绑定。
pub fn mark_projection_bound(func: &dyn DeviceFunction) {
    update_projection_state(func, DeviceFileProjectionStateKind::Bound, None);
}

/// 标记一个 function 的 devtmpfs 投影已解绑。
pub fn mark_projection_unbound(func: &dyn DeviceFunction) {
    update_projection_state(func, DeviceFileProjectionStateKind::Unbound, None);
}

/// 标记一个 function 的 devtmpfs 投影失败。
pub fn mark_projection_failed(func: &dyn DeviceFunction, err: VfsError) {
    let errno = err.to_errno().as_i32();
    update_projection_state(func, DeviceFileProjectionStateKind::Failed, Some(errno));
}

/// 记录一个 function 已经成功发布到 devtmpfs 的节点集合。
///
/// 这是 VFS 用户态名字空间的事实快照。sysfs/procfs/mount source 读取这里，避免
/// projector 规则变化后把“当前理论投影”误当作已经存在的 `/dev` 节点。
pub fn remember_published_devnodes(
    func: &dyn DeviceFunction,
    nodes: &DevNodeSet,
) -> Result<(), VfsError> {
    let Some(function_name) = fallible_string(func.dev_name()) else {
        return Err(VfsError::NoSpace);
    };
    let class_name = func.class_id().as_str();
    let mut published = PUBLISHED_DEVNODES.lock();
    if let Some(existing) = published
        .iter_mut()
        .find(|record| record.class_name == class_name && record.function_name == function_name)
    {
        existing.nodes = nodes.clone();
        return Ok(());
    }
    {
        // 已发布记录随 function 注销而释放，但全局表扩容后的容量会由内核长期复用。
        let _accounting =
            allocator::suspend_implicit_allocation_accounting().ok_or(VfsError::OutOfMemory)?;
        published
            .try_reserve(1)
            .map_err(|_| VfsError::OutOfMemory)?;
    }
    published.push(PublishedDevNodeRecord {
        class_name,
        function_name,
        nodes: nodes.clone(),
    });
    Ok(())
}

/// 查询一个 function 当前实际发布到 devtmpfs 的节点集合。
pub fn published_devnodes_for_function(func: &dyn DeviceFunction) -> Option<DevNodeSet> {
    let class_name = func.class_id().as_str();
    let function_name = func.dev_name();
    PUBLISHED_DEVNODES
        .lock()
        .iter()
        .find(|record| record.class_name == class_name && record.function_name == function_name)
        .map(|record| record.nodes.clone())
}

/// 移除并返回一个 function 的已发布节点快照。
pub fn forget_published_devnodes(func: &dyn DeviceFunction) -> Option<DevNodeSet> {
    let class_name = func.class_id().as_str();
    let function_name = func.dev_name();
    let mut published = PUBLISHED_DEVNODES.lock();
    let index = published.iter().position(|record| {
        record.class_name == class_name && record.function_name == function_name
    })?;
    Some(published.remove(index).nodes)
}

fn update_projection_state(
    func: &dyn DeviceFunction,
    state: DeviceFileProjectionStateKind,
    errno: Option<i32>,
) {
    let function_name = func.dev_name();
    let class_name = func.class_id().as_str();
    let mut states = PROJECTION_STATES.lock();
    if let Some(existing) = states
        .iter_mut()
        .find(|record| record.class_name == class_name && record.function_name == function_name)
    {
        existing.state = state;
        existing.errno = errno;
        return;
    }
    // 状态表是内核维护的诊断历史；状态键和扩容容量都会跨 function 注销保留，
    // 因而不能计入触发注册事件的动态 ELM。
    let Some(_accounting) = allocator::suspend_implicit_allocation_accounting() else {
        return;
    };
    let Some(function_name) = fallible_string(function_name) else {
        return;
    };
    if states.try_reserve(1).is_err() {
        return;
    }
    states.push(DeviceFileProjectionStateRecord {
        class_name,
        function_name,
        state,
        errno,
    });
}

/// 收集当前已知的 function 投影状态快照。
pub fn collect_projection_state_snapshots() -> Vec<DeviceFileProjectionStateSnapshot> {
    let states = PROJECTION_STATES.lock();
    let mut out = Vec::new();
    if out.try_reserve(states.len()).is_err() {
        return out;
    }
    out.extend(states.iter().filter_map(|record| {
        Some(DeviceFileProjectionStateSnapshot {
            class_name: record.class_name,
            function_name: fallible_string(&record.function_name)?,
            state: record.state,
            errno: record.errno,
        })
    }));
    out
}

fn projection_status_for(
    states: &[DeviceFileProjectionStateSnapshot],
    class_name: &'static str,
    function_name: &str,
) -> (DeviceFileProjectionStateKind, Option<i32>) {
    states
        .iter()
        .find(|state| state.class_name == class_name && state.function_name == function_name)
        .map(|state| (state.state(), state.errno()))
        .unwrap_or((DeviceFileProjectionStateKind::Pending, None))
}

/// 一个已声明到 devtmpfs 的字符设备投影。
///
/// `node_name` 只是用户态兼容层路径，不能作为底层设备身份；底层对象仍由 `dev`
/// 和所属 function 描述。sysfs/procfs 这类视图需要关联 `rdev` 时，应先按
/// `node_name` 查询兼容层 registry，再回到 typed device object。
#[derive(Clone)]
pub struct CharDevNodeProjection {
    class_id: DeviceClassId,
    node_name: Box<str>,
    dev: CharDevice,
}

impl CharDevNodeProjection {
    pub fn class_id(&self) -> DeviceClassId {
        self.class_id
    }

    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    pub fn dev(&self) -> &CharDevice {
        &self.dev
    }
}

/// 一个已声明到 devtmpfs 的块设备投影。
#[derive(Clone)]
pub struct BlockDevNodeProjection {
    class_id: DeviceClassId,
    node_name: Box<str>,
    dev: Arc<BlockDevice>,
}

/// 已经发布并分配 `dev_t` 的字符设备节点快照。
///
/// 这是 sysfs/procfs 这类用户视图需要的完整事实：节点来自 devtmpfs 发布表，
/// `rdev/display_name` 来自用户 ABI 设备号表，typed device 仍作为 VFS 打开对象。
#[derive(Clone)]
pub struct PublishedCharDevNode {
    class_id: DeviceClassId,
    node_name: Box<str>,
    display_name: String,
    rdev: vfs::stat::DevId,
    dev: CharDevice,
}

impl PublishedCharDevNode {
    pub fn class_id(&self) -> DeviceClassId {
        self.class_id
    }

    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn rdev(&self) -> vfs::stat::DevId {
        self.rdev
    }

    pub fn dev(&self) -> &CharDevice {
        &self.dev
    }
}

/// 已经发布并分配 `dev_t` 的块设备节点快照。
#[derive(Clone)]
pub struct PublishedBlockDevNode {
    class_id: DeviceClassId,
    node_name: Box<str>,
    display_name: String,
    rdev: vfs::stat::DevId,
    dev: Arc<BlockDevice>,
}

/// 已发布设备节点到 sysfs class 的映射。
#[derive(Clone, Debug)]
pub struct PublishedDevNodeClass {
    node_name: String,
    class_name: &'static str,
}

impl PublishedDevNodeClass {
    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    pub fn class_name(&self) -> &'static str {
        self.class_name
    }
}

impl PublishedBlockDevNode {
    pub fn class_id(&self) -> DeviceClassId {
        self.class_id
    }

    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn rdev(&self) -> vfs::stat::DevId {
        self.rdev
    }

    pub fn dev(&self) -> &Arc<BlockDevice> {
        &self.dev
    }
}

impl BlockDevNodeProjection {
    pub fn class_id(&self) -> DeviceClassId {
        self.class_id
    }

    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    pub fn dev(&self) -> &Arc<BlockDevice> {
        &self.dev
    }
}

/// 由 typed function 生成 `/dev` 投影节点集合。
///
/// 这是 dev core 与 VFS 用户态名字空间之间的边界：底层 function 不再携带
/// `DevNodeSpec`，VFS 在这里按 function 类型生成节点声明。未来新增设备文件类型
/// 应扩展本层或注册新的 projector，而不是把 inode、设备号或用户 ABI 放回 dev。
pub fn devnodes_for_function(func: &dyn DeviceFunction) -> Result<Option<DevNodeSet>, VfsError> {
    let projectors = device_file_projector_snapshot()?;
    let mut merged = Vec::new();
    for projector in projectors {
        let Some(nodes) = projector.build(func)? else {
            continue;
        };
        merged
            .try_reserve(nodes.nodes().len())
            .map_err(|_| VfsError::OutOfMemory)?;
        merged.extend(nodes.into_nodes());
    }
    DevNodeSet::try_new(merged)
}

fn device_file_projector_snapshot() -> Result<Vec<DeviceFileProjector>, VfsError> {
    let projectors = DEVICE_FILE_PROJECTORS.lock();
    let mut out = Vec::new();
    out.try_reserve(projectors.len())
        .map_err(|_| VfsError::OutOfMemory)?;
    out.extend(projectors.iter().copied());
    Ok(out)
}

fn char_function_devnodes(func: &dyn DeviceFunction) -> Result<Option<DevNodeSet>, VfsError> {
    let Some(ch) = function_as::<CharFunction>(func) else {
        return Ok(None);
    };
    let dev = ch.dev();
    let mut nodes = Vec::new();
    nodes.try_reserve(1).map_err(|_| VfsError::NoSpace)?;
    nodes.push(DevNodeSpec::Char {
        name: fallible_box_str(ch.projection_name())?,
        dev: dev.clone(),
    });
    DevNodeSet::try_new(nodes)
}

fn block_function_devnodes(func: &dyn DeviceFunction) -> Result<Option<DevNodeSet>, VfsError> {
    let Some(block) = function_as::<BlockFunction>(func) else {
        return Ok(None);
    };
    let dev = block.dev();
    let mut nodes = Vec::new();
    nodes.try_reserve(8).map_err(|_| VfsError::NoSpace)?;
    nodes.push(DevNodeSpec::Block {
        name: fallible_box_str(block.projection_name())?,
        dev: Arc::clone(&dev),
    });
    // 整盘设备扫描分区表并投影 /dev/<disk>p<part> 节点（如 vd0p1）。
    // 分区扫描带缓存,仅在首次投影时读一次磁盘。
    for part in crate::dev::partition::partitions_of(&dev) {
        let name =
            crate::dev::partition::partition_disk_name(block.projection_name(), part.number());
        if name.is_empty() {
            continue;
        }
        nodes.push(DevNodeSpec::Block {
            name: fallible_box_str(&name)?,
            dev: part.dev(),
        });
    }
    DevNodeSet::try_new(nodes)
}

fn rtc_function_devnodes(func: &dyn DeviceFunction) -> Result<Option<DevNodeSet>, VfsError> {
    let Some(func) = function_as::<RtcFunction>(func) else {
        return Ok(None);
    };
    let dev = func.dev();
    let mut nodes = Vec::new();
    let payload: Arc<dyn Any + Send + Sync> = Arc::new(RtcDevNodeEndpoint::new(Arc::clone(&dev)));
    nodes.try_reserve(1).map_err(|_| VfsError::OutOfMemory)?;
    nodes.push(DevNodeSpec::custom(CustomDevNodeSpec::try_new(
        dev.name(),
        CustomDevNodeKind::CharDevice,
        payload,
    )?));
    DevNodeSet::try_new(nodes)
}

pub fn char_device_from_function(func: &dyn DeviceFunction) -> Option<CharDevice> {
    let nodes = devnodes_for_function(func).ok()??;
    nodes.nodes().iter().find_map(|node| match node {
        DevNodeSpec::Char { dev, .. } => Some(dev.clone()),
        DevNodeSpec::Block { .. } | DevNodeSpec::Symlink { .. } | DevNodeSpec::Custom(_) => None,
    })
}

pub fn block_device_from_function(func: &dyn DeviceFunction) -> Option<Arc<BlockDevice>> {
    let nodes = devnodes_for_function(func).ok()??;
    nodes.nodes().iter().find_map(|node| match node {
        DevNodeSpec::Block { dev, .. } => Some(Arc::clone(dev)),
        DevNodeSpec::Char { .. } | DevNodeSpec::Symlink { .. } | DevNodeSpec::Custom(_) => None,
    })
}

/// 列出当前 function registry 中活动字符设备的 `/dev` 投影。
///
/// 这个 helper 只暴露“function 声明了哪些 VFS 节点”这一层事实，不把设备号或
/// sysfs 目录名塞回底层设备模型。
pub fn active_char_devnode_projections(functions: &FunctionRegistry) -> Vec<CharDevNodeProjection> {
    let mut out = Vec::new();
    for snapshot in published_devnode_snapshots(functions) {
        for node in snapshot.nodes.nodes() {
            if let DevNodeSpec::Char { name, dev } = node {
                if dev.is_active() {
                    if out.try_reserve(1).is_err() {
                        return out;
                    }
                    out.push(CharDevNodeProjection {
                        class_id: snapshot.class_id,
                        node_name: name.clone(),
                        dev: dev.clone(),
                    });
                }
            }
        }
    }
    out
}

/// 列出当前已发布且仍可用的字符设备节点，附带用户 ABI 设备号记录。
pub fn published_char_devnodes(functions: &FunctionRegistry) -> Vec<PublishedCharDevNode> {
    let mut out = Vec::new();
    for projection in active_char_devnode_projections(functions) {
        let Some(record) = device_numbers::lookup_node(projection.node_name()) else {
            continue;
        };
        if record.kind != DeviceNumberKind::Char {
            continue;
        }
        let dev = projection.dev().clone();
        if !dev.is_active() {
            continue;
        }
        if out.try_reserve(1).is_err() {
            return out;
        }
        out.push(PublishedCharDevNode {
            class_id: projection.class_id(),
            node_name: projection.node_name.clone(),
            display_name: record.display_name,
            rdev: record.rdev,
            dev,
        });
    }
    out
}

/// 列出当前 function registry 中活动块设备的 `/dev` 投影。
pub fn active_block_devnode_projections(
    functions: &FunctionRegistry,
) -> Vec<BlockDevNodeProjection> {
    let mut out = Vec::new();
    for snapshot in published_devnode_snapshots(functions) {
        for node in snapshot.nodes.nodes() {
            if let DevNodeSpec::Block { name, dev } = node {
                if dev.is_active() {
                    if out.try_reserve(1).is_err() {
                        return out;
                    }
                    out.push(BlockDevNodeProjection {
                        class_id: snapshot.class_id,
                        node_name: name.clone(),
                        dev: Arc::clone(dev),
                    });
                }
            }
        }
    }
    out
}

/// 列出当前已发布且仍可用的块设备节点，附带用户 ABI 设备号记录。
pub fn published_block_devnodes(functions: &FunctionRegistry) -> Vec<PublishedBlockDevNode> {
    let mut out = Vec::new();
    for projection in active_block_devnode_projections(functions) {
        let Some(record) = device_numbers::lookup_node(projection.node_name()) else {
            continue;
        };
        if record.kind != DeviceNumberKind::Block {
            continue;
        }
        let dev = Arc::clone(projection.dev());
        if !dev.is_active() {
            continue;
        }
        if out.try_reserve(1).is_err() {
            return out;
        }
        out.push(PublishedBlockDevNode {
            class_id: projection.class_id(),
            node_name: projection.node_name.clone(),
            display_name: record.display_name,
            rdev: record.rdev,
            dev,
        });
    }
    out
}

/// 收集已发布设备节点对应的 sysfs class 映射。
///
/// 哪些 `/dev` 投影可以进入 `/sys/class` 是 device_files 投影语义的一部分；sysfs
/// 只消费这里产出的映射，避免在文件系统代码里重复解释 projection kind。
pub fn published_devnode_classes() -> Vec<PublishedDevNodeClass> {
    let mut out = Vec::new();
    for snapshot in published_devnode_snapshots(&DEVICES.functions) {
        for node in snapshot.nodes.nodes() {
            if !node_has_device_class(node) {
                continue;
            }
            let Some(node_name) = fallible_string(node.name()) else {
                continue;
            };
            if out.try_reserve(1).is_err() {
                return out;
            }
            out.push(PublishedDevNodeClass {
                node_name,
                class_name: snapshot.class_id.as_str(),
            });
        }
    }
    out
}

fn node_has_device_class(node: &DevNodeSpec) -> bool {
    match node {
        DevNodeSpec::Char { .. } | DevNodeSpec::Block { .. } => true,
        DevNodeSpec::Symlink { .. } => false,
        DevNodeSpec::Custom(spec) => matches!(
            spec.kind(),
            CustomDevNodeKind::CharDevice | CustomDevNodeKind::BlockDevice
        ),
    }
}

/// 列出当前仍处于 active 状态的字符设备节点。
pub fn active_char_devices(functions: &FunctionRegistry) -> Vec<CharDevice> {
    let mut out = Vec::new();
    for projection in active_char_devnode_projections(functions) {
        let dev = projection.dev().clone();
        if !dev.is_active() {
            continue;
        }
        if out.try_reserve(1).is_err() {
            return out;
        }
        out.push(dev);
    }
    out
}

/// 列出当前仍处于 active 状态的块设备节点。
pub fn active_block_devices(functions: &FunctionRegistry) -> Vec<Arc<BlockDevice>> {
    let mut out = Vec::new();
    for projection in active_block_devnode_projections(functions) {
        let dev = Arc::clone(projection.dev());
        if !dev.is_active() {
            continue;
        }
        if out.try_reserve(1).is_err() {
            return out;
        }
        out.push(dev);
    }
    out
}

/// 按固件名查找字符设备。
///
/// 控制台选择路径使用固件名（例如 DTB stdout-path 指向的串口节点名），而不是
/// `/dev` 下的节点名，因此这里扫描所有字符 function 的 `fw_name()`。
pub fn find_char_device_by_fw_name(
    functions: &FunctionRegistry,
    fw_name: &str,
) -> Option<CharDevice> {
    active_char_devices(functions)
        .into_iter()
        .find(|dev| dev.fw_name() == fw_name)
}

/// 按 `/dev` 节点名查找块设备。
pub fn lookup_block_device_by_node(
    functions: &FunctionRegistry,
    dev_name: &str,
) -> Option<Arc<BlockDevice>> {
    published_devnode_snapshots(functions)
        .into_iter()
        .filter_map(|snapshot| {
            let nodes = snapshot.nodes;
            nodes.nodes().iter().find_map(|node| match node {
                DevNodeSpec::Block { name, dev } if name.as_ref() == dev_name => {
                    Some(Arc::clone(dev))
                }
                _ => None,
            })
        })
        .find(|dev| dev.is_active())
}

fn published_devnode_snapshots(functions: &FunctionRegistry) -> Vec<PublishedDevNodeSnapshot> {
    let published = published_devnode_records_snapshot();
    let mut out = Vec::new();
    if out.try_reserve(published.len()).is_err() {
        return out;
    }
    for record in published {
        let class_id = DeviceClassId::new(record.class_name);
        let known = functions.lookup(class_id, &record.function_name).is_some();
        if !known {
            continue;
        }
        out.push(PublishedDevNodeSnapshot {
            class_id,
            nodes: record.nodes,
        });
    }
    out
}

fn published_devnode_records_snapshot() -> Vec<PublishedDevNodeRecord> {
    let published = PUBLISHED_DEVNODES.lock();
    let mut out = Vec::new();
    if out.try_reserve(published.len()).is_err() {
        return out;
    }
    // 先复制已发布表再查询 function registry，避免诊断路径和注册/注销事件路径
    // 在不同锁顺序下形成隐蔽死锁。低内存时返回已复制前缀；诊断视图会保守降级，
    // 真实 devtmpfs 节点生命周期不受影响。
    out.extend(published.iter().cloned());
    out
}

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
        matches!(
            self,
            Self::Char | Self::Block | Self::CustomChar | Self::CustomBlock
        )
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
    error: Option<i32>,
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

    pub fn error(&self) -> Option<i32> {
        self.error
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
        if let Some(nodes) = published_devnodes_for_function(func.as_ref()) {
            let entries = projection_entries(class_name, &function_name, &nodes);
            push_function_snapshot(&mut out, class_name, function_name, entries, None);
            continue;
        }
        let nodes = match devnodes_for_function(func.as_ref()) {
            Ok(Some(nodes)) => nodes,
            Ok(None) => {
                push_function_snapshot(&mut out, class_name, function_name, Vec::new(), None);
                continue;
            }
            Err(err) => {
                push_function_snapshot(
                    &mut out,
                    class_name,
                    function_name,
                    Vec::new(),
                    Some(err.to_errno().as_i32()),
                );
                continue;
            }
        };
        let entries = projection_entries(class_name, &function_name, &nodes);
        push_function_snapshot(&mut out, class_name, function_name, entries, None);
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
    append_function_projection_diagnostics(&mut out);
    out
}

/// 渲染 function registry 的设备文件投影诊断表到指定缓冲区。
///
/// procfs 和 sysfs 都需要展示同一组 VFS 投影状态；把格式化逻辑集中在这里，
/// 可以保证 `/proc/device-functions` 与 `/sys/kernel/device_functions` 不会因
/// 各自扫描 function registry 而出现字段不一致。
pub fn append_function_projection_diagnostics(out: &mut String) {
    if out
        .try_reserve("class\tname\tstatus\terrno\tdevnodes\n".len())
        .is_err()
    {
        return;
    }
    out.push_str("class\tname\tstatus\terrno\tdevnodes\n");
    append_function_projection_rows(out);
}

fn append_function_projection_rows(out: &mut String) {
    let states = collect_projection_state_snapshots();
    for func in collect_function_projection_snapshots() {
        let nodes = func.nodes();
        let devnode_len = projection_list_len(nodes).unwrap_or(1);
        let (status, errno) = projection_row_status(
            &states,
            func.class_name(),
            func.function_name(),
            func.error(),
        );
        let line_reserve = func
            .class_name()
            .len()
            .saturating_add(func.function_name().len())
            .saturating_add(status.diagnostic_name().len())
            .saturating_add(16)
            .saturating_add(devnode_len)
            .saturating_add(6);
        if out.try_reserve(line_reserve).is_err() {
            return;
        }
        write_function_projection_row(out, &func, status, errno);
    }
}

fn projection_row_status(
    states: &[DeviceFileProjectionStateSnapshot],
    class_name: &'static str,
    function_name: &str,
    build_error: Option<i32>,
) -> (DeviceFileProjectionStateKind, Option<i32>) {
    if let Some(error) = build_error {
        return (DeviceFileProjectionStateKind::Failed, Some(error));
    }
    projection_status_for(states, class_name, function_name)
}

fn write_function_projection_row(
    out: &mut String,
    func: &DeviceFunctionProjectionSnapshot,
    status: DeviceFileProjectionStateKind,
    errno: Option<i32>,
) {
    let _ = write!(
        out,
        "{}\t{}\t{}\t",
        func.class_name(),
        func.function_name(),
        status.diagnostic_name()
    );
    if let Some(errno) = errno {
        let _ = write!(out, "{errno}\t");
    } else {
        out.push_str("-\t");
    }
    if !func.nodes().is_empty() {
        for (idx, node) in func.nodes().iter().enumerate() {
            if idx != 0 {
                out.push(',');
            }
            node.write_diagnostic(out);
        }
    } else {
        out.push('-');
    }
    out.push('\n');
}

fn projection_list_len(nodes: &[DeviceFileProjectionEntry]) -> Option<usize> {
    if nodes.is_empty() {
        return None;
    }
    let names_len = nodes.iter().fold(0usize, |acc, node| {
        acc.saturating_add(node.diagnostic_len())
    });
    Some(names_len.saturating_add(nodes.len().saturating_sub(1)))
}

fn push_function_snapshot(
    out: &mut Vec<DeviceFunctionProjectionSnapshot>,
    class_name: &'static str,
    function_name: String,
    nodes: Vec<DeviceFileProjectionEntry>,
    error: Option<i32>,
) {
    if out.try_reserve(1).is_err() {
        return;
    }
    out.push(DeviceFunctionProjectionSnapshot {
        class_name,
        function_name,
        nodes,
        error,
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
        DevNodeSpec::Symlink { name, target } => (
            name.as_ref(),
            Some(target.as_ref()),
            DeviceFileProjectionKind::Symlink,
        ),
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

fn projection_entries(
    class_name: &'static str,
    function_name: &str,
    nodes: &DevNodeSet,
) -> Vec<DeviceFileProjectionEntry> {
    let mut entries = Vec::new();
    for node in nodes.nodes() {
        let Some(entry) = projection_entry(class_name, function_name, node) else {
            continue;
        };
        if entries.try_reserve(1).is_err() {
            break;
        }
        entries.push(entry);
    }
    entries
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::function::CharFunction;

    #[test]
    fn serial_functions_publish_only_the_native_uart_name() {
        register_builtin_device_file_projectors().expect("builtin projectors");
        let function = CharFunction::with_projection_name(
            "serial@3f8",
            "uart0",
            crate::dev::char::CharDevice::null(),
        );
        let nodes = devnodes_for_function(&function)
            .expect("projection should succeed")
            .expect("character projector should produce a node");
        assert_eq!(nodes.nodes().len(), 1);
        assert_eq!(nodes.nodes()[0].name(), "uart0");
    }
}

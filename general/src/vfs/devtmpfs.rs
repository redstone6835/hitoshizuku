//! devtmpfs — 设备临时文件系统
//!
//! # 设计要点
//!
//! 设备节点的 inode 直接持有设备对象引用（`CharDevice` 或 `Arc<BlockDevice>`），
//! 而非设备名称字符串。`open()` 时零查找：已在绑定时解析，运行时直接调用。
//!
//! ```text
//! bind_char("uart0", dev: CharDevice)
//!   └─ 创建 Inode，InodeOps = DevCharOps { dev, tty }
//!         └─ open() → 直接访问 dev              // 无查找，直接构造
//!
//! bind_block("disk/root", dev: Arc<BlockDevice>)
//!   └─ 创建 Inode，InodeOps = DevBlockOps { dev: Arc<BlockDevice> }
//!         └─ open() → 直接访问 dev              // 无查找，直接构造
//!
//! bind_symlink("disk/by-name/root", "../root")
//!   └─ 创建 Symlink Inode，InodeOps = DevSymlinkOps { target: "../root" }
//!         └─ path lookup → readlink() → 按标准相对链接规则继续解析
//! ```
//!
//! # 文件系统结构
//!
//! devtmpfs 是一棵普通目录树。每个目录 inode 维护本级
//! `name → Arc<Inode>` 的 `BTreeMap`，作为 `lookup` 和 `readdir` 的数据源。
//! 设备驱动可以声明主节点、目录化节点或符号链接节点；devtmpfs 不内建任何固定
//! 设备别名。
//!
//! 整个文件系统通过 `mount -t devtmpfs` 挂载到 `/dev`，之后通过
//! [`DevTmpfsSuperblockOps::bind_char`] / [`DevTmpfsSuperblockOps::bind_block`] /
//! [`DevTmpfsSuperblockOps::bind_symlink`] / [`DevTmpfsSuperblockOps::bind_node`]
//! 动态增删节点。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use errno::Errno;
use sched::operation;
use vfs::cred::{Credentials, Gid, Uid};
use vfs::dentry::{Dentry, SmallStr};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::mount::MountFlags;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{FsDriver, FsDriverFlags, Superblock, SuperblockOps};
use vfs::sync::Spinlock;

use crate::dev::bio::{BioBuffer, BioError, BioOp, BlockRange};
use crate::dev::block::{BlockDevice, BlockFeatures};
use crate::dev::char::{CharDevice, CharIoError};
use crate::dev::control::{BlockControlRequest, BlockControlResponse, BlockIoHints, ControlError};
use crate::dev::tty::{self, TtyCore};
use crate::dev::enumerate::{
    DEVICES, DeviceFunctionEvent, DeviceFunctionEventKind, subscribe_function_events,
};
use crate::dev::function::DeviceFunction;
use crate::vfs::device_files::projection::{
    devnodes_for_function, forget_published_devnodes, mark_projection_bound,
    mark_projection_failed, mark_projection_pending, mark_projection_unbound,
    published_devnodes_for_function, remember_published_devnodes,
};
use crate::vfs::device_files::spec::{
    CustomDevNodeKind, CustomDevNodeNumbering, CustomDevNodeSpec, DevNodeSet, DevNodeSpec,
};
use crate::vfs::user_api::block_device::{BlockDeviceIoctlContext, handle_block_ioctl};
use crate::vfs::user_api::tty::{
    TtyIoctlContext, TtyIoctlState, UserTermios, handle_tty_ioctl,
};

// ───────── 全局实例计数器 ─────────

static DEVTMPFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);

static DEVTMPFS_SINGLETON_SB: Spinlock<Option<&'static Arc<Superblock>>> = Spinlock::new(None);
// 无 PnP backing 的内核服务由 VFS device_files 层注册静态投影。devtmpfs 只维护
// 这张声明表和事务绑定逻辑，不直接知道 null/zero/random/loop-control 等具体设备。
static STATIC_DEV_NODES: Spinlock<Vec<DevTmpfsStaticNode>> = Spinlock::new(Vec::new());
static CUSTOM_DEVNODE_ADAPTERS: Spinlock<Vec<DevTmpfsCustomNodeAdapter>> =
    Spinlock::new(Vec::new());

/// 自定义 devtmpfs 节点适配器构造函数。
///
/// 返回 `Ok(None)` 表示当前适配器不认识该 payload；返回 `Ok(Some(_))` 表示
/// 已成功把 typed endpoint 转换成 VFS inode 操作对象；返回 `Err(_)` 表示
/// payload 类型匹配但元数据或运行状态不合法。
pub type DevTmpfsCustomNodeBuild =
    fn(&CustomDevNodeSpec) -> VfsResult<Option<Arc<dyn InodeOps + Send + Sync>>>;

/// 自定义节点适配器声明。
///
/// devtmpfs 本体不应该知道 RTC、GPU、专用控制设备等具体类型。每个 VFS 兼容
/// 适配器通过本结构注册一个 typed payload 解释器，devtmpfs 只负责按注册顺序
/// 分发，避免每新增一种设备都修改核心文件系统代码。
#[derive(Clone, Copy)]
pub struct DevTmpfsCustomNodeAdapter {
    owner: &'static str,
    name: &'static str,
    build: DevTmpfsCustomNodeBuild,
}

/// 自定义 devtmpfs 节点适配器注册结果。
///
/// `inserted=false` 表示同一 owner/name 已经登记过，本次只是幂等确认。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevTmpfsCustomNodeAdapterRegistration {
    inserted: bool,
}

impl DevTmpfsCustomNodeAdapterRegistration {
    pub const fn inserted(self) -> bool {
        self.inserted
    }
}

impl DevTmpfsCustomNodeAdapter {
    pub const fn new(
        owner: &'static str,
        name: &'static str,
        build: DevTmpfsCustomNodeBuild,
    ) -> Self {
        Self { owner, name, build }
    }

    pub const fn owner(self) -> &'static str {
        self.owner
    }

    pub const fn name(self) -> &'static str {
        self.name
    }

    fn build(self, spec: &CustomDevNodeSpec) -> VfsResult<Option<Arc<dyn InodeOps + Send + Sync>>> {
        (self.build)(spec)
    }
}

/// 注册一个自定义 devtmpfs 节点适配器。
///
/// 同一 owner/name 重复注册视为幂等，便于 DTB/ACPI 或测试路径重复执行启动期
/// 初始化；不同 owner 复用同一 adapter name 会被拒绝，避免两个适配器抢占同一
/// payload 命名空间。
pub fn register_custom_devnode_adapter(
    adapter: DevTmpfsCustomNodeAdapter,
) -> VfsResult<DevTmpfsCustomNodeAdapterRegistration> {
    if adapter.owner().is_empty() || adapter.name().is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    let mut adapters = CUSTOM_DEVNODE_ADAPTERS.lock();
    if let Some(existing) = adapters.iter().find(|entry| entry.name() == adapter.name()) {
        if existing.owner() == adapter.owner() {
            return Ok(DevTmpfsCustomNodeAdapterRegistration { inserted: false });
        }
        return Err(VfsError::AlreadyExists);
    }
    adapters.try_reserve(1).map_err(|_| VfsError::OutOfMemory)?;
    adapters.push(adapter);
    Ok(DevTmpfsCustomNodeAdapterRegistration { inserted: true })
}

/// 注销一个自定义 devtmpfs 节点适配器。
///
/// 该操作只移除后续 custom 节点解析能力，不会主动删除已经创建的 inode；驱动
/// 或 PnP remove 仍应通过节点解绑路径处理已存在的 `/dev` 投影。
pub fn unregister_custom_devnode_adapter(owner: &'static str, name: &str) -> VfsResult<()> {
    let mut adapters = CUSTOM_DEVNODE_ADAPTERS.lock();
    let Some(index) = adapters
        .iter()
        .position(|entry| entry.owner() == owner && entry.name() == name)
    else {
        return Err(VfsError::NotFound);
    };
    adapters.remove(index);
    Ok(())
}

/// devtmpfs 静态节点声明。
///
/// 静态节点用于没有 PnP backing device、但又必须出现在 `/dev` 的基础设备。
/// 声明只保存构造器，真正的 inode 仍通过 [`DevNodeSpec`] 进入统一绑定路径，
/// 避免 devtmpfs 为每种特殊设备增加分支。
#[derive(Clone, Copy)]
pub struct DevTmpfsStaticNode {
    owner: &'static str,
    name: &'static str,
    build: fn() -> VfsResult<DevNodeSpec>,
}

impl DevTmpfsStaticNode {
    /// 构造一个静态节点声明。
    ///
    /// `owner` 是声明来源的稳定名字，用于让同一组件重复初始化时幂等返回，同时
    /// 仍能发现两个不同组件抢占同一个 `/dev` 名称的真实冲突。
    pub const fn new(
        owner: &'static str,
        name: &'static str,
        build: fn() -> VfsResult<DevNodeSpec>,
    ) -> Self {
        Self { owner, name, build }
    }

    pub const fn name(self) -> &'static str {
        self.name
    }

    pub const fn owner(self) -> &'static str {
        self.owner
    }
}

/// 静态 devtmpfs 节点注册结果。
///
/// `inserted=false` 表示同一 owner/name 已经登记过，本次只是幂等确认。批量注册
/// 需要这个信息来避免失败回滚时误删早已存在的节点。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevTmpfsStaticNodeRegistration {
    inserted: bool,
}

impl DevTmpfsStaticNodeRegistration {
    pub const fn inserted(self) -> bool {
        self.inserted
    }
}

fn bind_static_node(ops: &DevTmpfsSuperblockOps, node: DevTmpfsStaticNode) -> VfsResult<()> {
    let spec = (node.build)()?;
    ops.bind_node(&spec)
}

fn remove_static_dev_node_record(owner: &'static str, name: &str) -> Option<DevTmpfsStaticNode> {
    let mut nodes = STATIC_DEV_NODES.lock();
    let Some(index) = nodes
        .iter()
        .position(|existing| existing.owner == owner && existing.name == name)
    else {
        return None;
    };
    Some(nodes.remove(index))
}

fn restore_static_dev_node_record(node: DevTmpfsStaticNode) -> VfsResult<()> {
    let mut nodes = STATIC_DEV_NODES.lock();
    if let Some(existing) = nodes.iter().find(|existing| existing.name == node.name) {
        if existing.owner == node.owner {
            return Ok(());
        }
        return Err(VfsError::AlreadyExists);
    }
    nodes.try_reserve(1).map_err(|_| VfsError::NoSpace)?;
    nodes.push(node);
    Ok(())
}

/// 注册一个非 PnP 静态 devtmpfs 节点。
///
/// 如果 devtmpfs 已经安装到 PnP bridge，注册会立即把节点补进现有 superblock；
/// 否则节点会留在注册表中，首次 mount devtmpfs 时批量绑定。调用方不需要关心
/// DTB/ACPI 等启动路径的先后顺序。
pub fn register_static_dev_node(
    node: DevTmpfsStaticNode,
) -> VfsResult<DevTmpfsStaticNodeRegistration> {
    split_devtmpfs_path(node.name)?;
    {
        let mut nodes = STATIC_DEV_NODES.lock();
        if let Some(existing) = nodes.iter().find(|existing| existing.name == node.name) {
            if existing.owner == node.owner {
                return Ok(DevTmpfsStaticNodeRegistration { inserted: false });
            }
            return Err(VfsError::AlreadyExists);
        }
        nodes.try_reserve(1).map_err(|_| VfsError::NoSpace)?;
        nodes.push(node);
    }

    if let Some(sb) = mounted_devtmpfs_sb() {
        let ops = sb
            .downcast_ops::<DevTmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
        if let Err(err) = bind_static_node(ops, node) {
            STATIC_DEV_NODES
                .lock()
                .retain(|existing| existing.owner != node.owner || existing.name != node.name);
            return Err(err);
        }
    }

    Ok(DevTmpfsStaticNodeRegistration { inserted: true })
}

/// 批量注册同一组件声明的一组静态 devtmpfs 节点。
///
/// 如果中途失败，只回滚本轮实际新插入的节点；已经存在且由同一 owner 声明的节点
/// 保持不动。这样重复初始化、部分重试和未来可卸载组件都能共享同一套事务语义。
pub fn register_static_dev_nodes(nodes: &[DevTmpfsStaticNode]) -> VfsResult<()> {
    let mut inserted: Vec<DevTmpfsStaticNode> = Vec::new();
    for node in nodes.iter().copied() {
        match register_static_dev_node(node) {
            Ok(registration) => {
                if registration.inserted() {
                    if inserted.try_reserve(1).is_err() {
                        let _ = unregister_static_dev_node(node.owner(), node.name());
                        for registered in inserted.iter().rev().copied() {
                            let _ =
                                unregister_static_dev_node(registered.owner(), registered.name());
                        }
                        return Err(VfsError::NoSpace);
                    }
                    inserted.push(node);
                }
            }
            Err(err) => {
                for registered in inserted.iter().rev().copied() {
                    let _ = unregister_static_dev_node(registered.owner(), registered.name());
                }
                return Err(err);
            }
        }
    }
    Ok(())
}

/// 注销一个非 PnP 静态 devtmpfs 节点。
///
/// 这给未来可卸载的内核内建服务提供对称生命周期：注册表先移除声明，再从当前
/// devtmpfs 单例中删除节点。不存在的节点按 `NotFound` 返回，避免调用方误以为
/// 已完成解绑。
pub fn unregister_static_dev_node(owner: &'static str, name: &str) -> VfsResult<()> {
    split_devtmpfs_path(name)?;
    let Some(node) = remove_static_dev_node_record(owner, name) else {
        return Err(VfsError::NotFound);
    };

    if let Some(sb) = mounted_devtmpfs_sb() {
        let ops = sb
            .downcast_ops::<DevTmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
        match ops.unbind(name) {
            Ok(()) | Err(VfsError::NotFound) => {}
            Err(err) => {
                let _ = restore_static_dev_node_record(node);
                return Err(err);
            }
        }
    }

    Ok(())
}

const DEVTMPFS_NAME_MAX: usize = 255;

/// devtmpfs 节点元数据策略。
///
/// 默认权限、属主、块大小和轮询能力属于 VFS 用户接口策略，不属于底层设备身份。
/// 先集中在这个结构内，避免 inode 创建路径散落魔数；后续挂载参数或投影 registry
/// 可以生成新的 policy 并替换调用点。
#[derive(Clone, Copy)]
struct DevTmpfsNodePolicy {
    dir_mode: FileMode,
    symlink_mode: FileMode,
    device_mode: FileMode,
    regular_mode: FileMode,
    uid: Uid,
    gid: Gid,
    block_size: u32,
    char_poll: PollEvents,
    max_blocks_per_io: u32,
}

impl DevTmpfsNodePolicy {
    const fn standard() -> Self {
        Self {
            dir_mode: FileMode::new(0o755),
            symlink_mode: FileMode::new(0o777),
            device_mode: FileMode::new(0o600),
            regular_mode: FileMode::new(0o644),
            uid: Uid::ROOT,
            gid: Gid::ROOT,
            block_size: 512,
            char_poll: PollEvents::POLLIN.with(PollEvents::POLLOUT),
            max_blocks_per_io: 1024,
        }
    }

    fn custom_mode(self, kind: CustomDevNodeKind) -> FileMode {
        match kind {
            CustomDevNodeKind::CharDevice | CustomDevNodeKind::BlockDevice => self.device_mode,
            CustomDevNodeKind::RegularFile => self.regular_mode,
            CustomDevNodeKind::Directory => self.dir_mode,
        }
    }

    fn custom_nlink(self, kind: CustomDevNodeKind) -> u32 {
        match kind {
            // 目录 inode 自身和父目录中的入口各占一个链接计数。自定义目录目前只
            // 作为空目录投影，后续若有可枚举子项，应由对应 InodeOps 维护。
            CustomDevNodeKind::Directory => 2,
            CustomDevNodeKind::CharDevice
            | CustomDevNodeKind::BlockDevice
            | CustomDevNodeKind::RegularFile => 1,
        }
    }
}

const DEVTMPFS_STANDARD_POLICY: DevTmpfsNodePolicy = DevTmpfsNodePolicy::standard();

/// 按节点名注册的设备节点权限(与 Linux devtmpfs 的 devnode 回调对应)。
///
/// 设备身份与权限策略分离:驱动声明功能,投影层按名查策略;未注册的
/// 节点使用默认 0600 root:root(Linux devtmpfs 默认)。
#[derive(Clone, Copy, Debug)]
pub struct DevNodePolicy {
    pub mode: FileMode,
    pub uid: Uid,
    pub gid: Gid,
}

impl DevNodePolicy {
    pub const fn new(mode: u16) -> Self {
        Self {
            mode: FileMode::new(mode),
            uid: Uid::ROOT,
            gid: Gid::ROOT,
        }
    }
}

static NODE_POLICIES: Spinlock<BTreeMap<String, DevNodePolicy>> = Spinlock::new(BTreeMap::new());

/// 注册节点权限策略(幂等;同名覆盖)。
pub fn register_node_policy(name: &str, policy: DevNodePolicy) -> VfsResult<()> {
    let mut out = String::new();
    out.try_reserve(name.len()).map_err(|_| VfsError::NoSpace)?;
    out.push_str(name);
    NODE_POLICIES.lock().insert(out, policy);
    Ok(())
}

fn node_policy(name: &str) -> DevNodePolicy {
    NODE_POLICIES
        .lock()
        .get(name)
        .copied()
        .unwrap_or(DevNodePolicy::new(0o600))
}

fn devtmpfs_fallible_string(value: &str) -> VfsResult<String> {
    let mut out = String::new();
    out.try_reserve(value.len())
        .map_err(|_| VfsError::NoSpace)?;
    out.push_str(value);
    Ok(out)
}

fn devtmpfs_fallible_smallstr(value: &str) -> VfsResult<SmallStr> {
    let bytes = value.as_bytes();
    if bytes.len() <= 23 {
        let mut buf = [0u8; 23];
        buf[..bytes.len()].copy_from_slice(bytes);
        return Ok(SmallStr::Inline {
            len: bytes.len() as u8,
            buf,
        });
    }
    Ok(SmallStr::Heap(devtmpfs_fallible_string(value)?))
}

fn validate_devtmpfs_component(name: &str) -> VfsResult<()> {
    if name.is_empty() || name.contains('/') || name.contains('\0') || name == "." || name == ".." {
        return Err(VfsError::InvalidArgument);
    }
    if name.len() > DEVTMPFS_NAME_MAX {
        return Err(VfsError::NameTooLong);
    }
    Ok(())
}

fn split_devtmpfs_path(path: &str) -> VfsResult<Vec<&str>> {
    if path.is_empty() || path.starts_with('/') || path.ends_with('/') || path.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }

    let mut components = Vec::new();
    for component in path.split('/') {
        validate_devtmpfs_component(component)?;
        components.try_reserve(1).map_err(|_| VfsError::NoSpace)?;
        components.push(component);
    }
    if components.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    Ok(components)
}

fn validate_symlink_target(target: &str) -> VfsResult<()> {
    if target.is_empty() || target.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }
    Ok(())
}

/// 安装 function registry 到 devtmpfs 的投影订阅。
///
/// 安装后，任何带 [`DevNodeSpec`] 的 function 注册/注销都会触发 devtmpfs 在当前
/// superblock 中创建或删除对应 `/dev` 节点。该路径只是用户态名字空间投影：
/// function 的所有权、probe/remove 事务仍由 dev core/PnP core 管理，投影失败只会
/// 记录日志，不会回滚底层设备注册。
pub fn install_function_projection(dev_sb: Arc<Superblock>) -> VfsResult<()> {
    let (dev_sb, _) = publish_devtmpfs_sb(dev_sb);
    dev_sb
        .downcast_ops::<DevTmpfsSuperblockOps>()
        .ok_or(VfsError::InvalidArgument)?;

    let subscription =
        subscribe_function_events("devtmpfs", "function-devnodes", devtmpfs_function_event)
            .map_err(|_| VfsError::NoSpace)?;
    if subscription.inserted() {
        let functions = DEVICES.functions.try_list().ok_or(VfsError::NoSpace)?;
        for func in functions {
            match devnodes_for_function(func.as_ref()) {
                Ok(Some(nodes)) => {
                    // 允许 function 在 devtmpfs 之前完成底层注册；投影首次安装后补齐
                    // 用户可见节点投影。单个节点失败只影响该 function 的用户态入口，
                    // 不能阻止 devtmpfs 本身挂载或反向破坏底层设备生命周期。
                    handle_projected_devnodes(
                        func.as_ref(),
                        DeviceFunctionEventKind::Registered,
                        &nodes,
                    );
                }
                Ok(None) => {}
                Err(err) => mark_projection_failed(func.as_ref(), err),
            }
        }
    }
    Ok(())
}

fn mounted_devtmpfs_sb() -> Option<Arc<Superblock>> {
    let guard = DEVTMPFS_SINGLETON_SB.lock();
    guard.as_ref().map(|sb| Arc::clone(*sb))
}

/// 将 function 声明的 `/dev` 投影绑定到当前 devtmpfs 单例。
///
/// 该函数只服务于 devtmpfs 自己的投影事件处理；调用方必须已经完成底层
/// function registry 的注册事务，devtmpfs 不反向拥有设备对象。
fn bind_projected_devnodes(nodes: &DevNodeSet) -> VfsResult<()> {
    let sb = mounted_devtmpfs_sb().ok_or(VfsError::NoDevice)?;
    let ops = sb
        .downcast_ops::<DevTmpfsSuperblockOps>()
        .ok_or(VfsError::InvalidArgument)?;
    ops.bind_nodes(nodes)
}

fn bind_and_remember_projected_devnodes(
    func: &dyn DeviceFunction,
    nodes: &DevNodeSet,
) -> VfsResult<()> {
    bind_projected_devnodes(nodes)?;
    if let Err(err) = remember_bound_projection_nodes(func, nodes) {
        let _ = unbind_projected_devnodes(nodes);
        return Err(err);
    }
    Ok(())
}

/// 解绑 function 声明的 `/dev` 投影。
fn unbind_projected_devnodes(nodes: &DevNodeSet) -> VfsResult<()> {
    let sb = mounted_devtmpfs_sb().ok_or(VfsError::NoDevice)?;
    let ops = sb
        .downcast_ops::<DevTmpfsSuperblockOps>()
        .ok_or(VfsError::InvalidArgument)?;
    ops.unbind_nodes(nodes)
}

fn publish_devtmpfs_sb(dev_sb: Arc<Superblock>) -> (Arc<Superblock>, bool) {
    let mut guard = DEVTMPFS_SINGLETON_SB.lock();
    if let Some(existing) = guard.as_ref() {
        return (Arc::clone(*existing), false);
    }

    // devtmpfs 是全局设备名字空间的投影，superblock 生命周期等同内核生命周期。
    // 这里泄露一个 Arc 作为单例锚点，后续 mount/bridge/static node 注册都只克隆它。
    let leaked: &'static Arc<Superblock> = Box::leak(Box::new(dev_sb));
    *guard = Some(leaked);
    (Arc::clone(leaked), true)
}

fn devtmpfs_function_event(event: &DeviceFunctionEvent) {
    match event.kind() {
        DeviceFunctionEventKind::Registered => {
            match devnodes_for_function(event.function().as_ref()) {
                Ok(Some(nodes)) => {
                    handle_projected_devnodes(event.function().as_ref(), event.kind(), &nodes)
                }
                Ok(None) => {}
                Err(err) => mark_projection_failed(event.function().as_ref(), err),
            }
        }
        DeviceFunctionEventKind::Unregistered => {
            let Some(nodes) = bound_projection_nodes(event.function().as_ref()) else {
                mark_projection_unbound(event.function().as_ref());
                return;
            };
            handle_projected_devnodes(event.function().as_ref(), event.kind(), &nodes);
        }
    }
}

fn bound_projection_nodes(func: &dyn DeviceFunction) -> Option<DevNodeSet> {
    published_devnodes_for_function(func)
}

fn forget_bound_projection_nodes(func: &dyn DeviceFunction) {
    let _ = forget_published_devnodes(func);
}

fn handle_projected_devnodes(
    func: &dyn DeviceFunction,
    kind: DeviceFunctionEventKind,
    nodes: &DevNodeSet,
) {
    match kind {
        DeviceFunctionEventKind::Registered => mark_projection_pending(func),
        DeviceFunctionEventKind::Unregistered => {}
    }
    let result = match kind {
        DeviceFunctionEventKind::Registered => bind_and_remember_projected_devnodes(func, nodes),
        DeviceFunctionEventKind::Unregistered => unbind_projected_devnodes(nodes),
    };
    log_projection_result(func, kind, result);
}

fn log_projection_result(
    func: &dyn DeviceFunction,
    kind: DeviceFunctionEventKind,
    result: VfsResult<()>,
) {
    if result.is_ok() {
        match kind {
            DeviceFunctionEventKind::Registered => mark_projection_bound(func),
            DeviceFunctionEventKind::Unregistered => {
                forget_bound_projection_nodes(func);
                mark_projection_unbound(func)
            }
        }
        return;
    }

    if let Err(err) = result {
        match kind {
            DeviceFunctionEventKind::Registered => mark_projection_failed(func, err),
            DeviceFunctionEventKind::Unregistered if err == VfsError::NotFound => {
                forget_bound_projection_nodes(func);
                mark_projection_unbound(func)
            }
            DeviceFunctionEventKind::Unregistered => mark_projection_failed(func, err),
        }
        // 投影层失败不能破坏 dev core 生命周期。这里保留启动/热拔诊断，后续 sysfs
        // 可读取更结构化的 projection 状态。
        log::debug!(
            "[devtmpfs] function projection {:?} failed: {:?}",
            kind,
            err
        );
    }
}

fn remember_bound_projection_nodes(func: &dyn DeviceFunction, nodes: &DevNodeSet) -> VfsResult<()> {
    remember_published_devnodes(func, nodes)
}

// ───────── 字符设备 FileOps（内联适配器） ─────────

fn map_char_err(e: CharIoError) -> VfsError {
    match e {
        CharIoError::NoSpace => VfsError::NoSpace,
        CharIoError::HardwareError => VfsError::Io,
        CharIoError::Unavailable => VfsError::NoDevice,
        CharIoError::Interrupted => VfsError::Interrupted,
        CharIoError::Timeout => VfsError::TimedOut,
    }
}

fn map_control_errno(e: ControlError) -> Errno {
    match e {
        ControlError::Unsupported => Errno::ENOTTY,
        ControlError::Invalid => Errno::EINVAL,
        ControlError::NoDevice => Errno::ENODEV,
        ControlError::Busy => Errno::EBUSY,
        ControlError::Io => Errno::EIO,
        ControlError::Permission => Errno::EPERM,
    }
}

fn map_control_vfs(e: ControlError) -> VfsError {
    match e {
        ControlError::Unsupported => VfsError::NotSupported,
        ControlError::Invalid => VfsError::InvalidArgument,
        ControlError::NoDevice => VfsError::NoDevice,
        ControlError::Busy => VfsError::DeviceBusy,
        ControlError::Io => VfsError::Io,
        ControlError::Permission => VfsError::PermissionDenied,
    }
}

struct CharDevFileOps {
    dev: CharDevice,
    nonblock: AtomicBool,
    tty: Option<Arc<TtyCore>>,
}

impl CharDevFileOps {
    fn new(dev: CharDevice, nonblock: bool, tty: Option<Arc<TtyCore>>) -> Self {
        crate::dev::tty::vt::note_vt_opened(&dev, 1);
        crate::dev::tty::pty::note_pty_opened(&dev, 1);
        Self {
            dev,
            nonblock: AtomicBool::new(nonblock),
            tty,
        }
    }

    fn is_tty(&self) -> bool {
        self.tty.is_some() && self.dev.is_tty()
    }

    fn current_or_stored_pgrp(&self) -> Result<i32, Errno> {
        match self.tty.as_deref() {
            Some(tty) => tty.current_or_stored_pgrp(),
            None => operation::getpgid(0),
        }
    }

    fn remember_reader_pgrp(&self) {
        if let Some(tty) = self.tty.as_deref() {
            tty.remember_reader_pgrp();
        }
    }

    fn map_tty_err(err: tty::TtyIoError) -> VfsError {
        match err {
            tty::TtyIoError::WouldBlock => VfsError::WouldBlock,
            tty::TtyIoError::Interrupted => VfsError::Interrupted,
            tty::TtyIoError::NoSpace => VfsError::NoSpace,
            tty::TtyIoError::Io => VfsError::Io,
            tty::TtyIoError::NoDevice => VfsError::NoDevice,
            tty::TtyIoError::TimedOut => VfsError::TimedOut,
            tty::TtyIoError::Unsupported => VfsError::NotSupported,
            tty::TtyIoError::Busy => VfsError::DeviceBusy,
            tty::TtyIoError::Invalid => VfsError::InvalidArgument,
        }
    }

    fn read_tty_canonical(
        &self,
        tty: &TtyCore,
        buf: &mut [u8],
        termios: UserTermios,
    ) -> VfsResult<usize> {
        tty.read_tty_canonical(buf, termios).map_err(Self::map_tty_err)
    }

    fn read_tty_raw(&self, tty: &TtyCore, buf: &mut [u8], termios: UserTermios) -> VfsResult<usize> {
        tty.read_tty_raw(buf, termios).map_err(Self::map_tty_err)
    }

    fn write_tty_bytes(&self, tty: &TtyCore, buf: &[u8], termios: UserTermios) -> VfsResult<()> {
        tty.write_tty_bytes(buf, termios).map_err(Self::map_tty_err)
    }
}

impl TtyIoctlContext for CharDevFileOps {
    fn current_or_stored_pgrp(&self) -> Result<i32, Errno> {
        self.current_or_stored_pgrp()
    }

    fn session_id(&self) -> Result<i32, Errno> {
        operation::getsid(0)
    }
}

/// 从已打开的 TTY 中主动拉取输入，供 timer tick 路径调用。
///
/// 串口中断只能说明底层 FIFO 有字节，不能替代终端行规程。若前台程序
/// 没有调用 `read()`（例如 `sleep`），Ctrl-C/Ctrl-\ /Ctrl-Z 仍必须被
/// 终端识别并投递给前台进程组；因此这里在 tick 上做一次有界 drain。
/// 非规范模式下普通字节会进入 TTY pending 队列，由之后的 read() 取走；
/// 控制字符则立即处理，避免 raw-mode shell 启动前台程序后 Ctrl-C 滞留。
pub fn poll_tty_input() {
    // VT 串口输入模式(console=ttyN):物理控制台字节属于活动 VT,由 VT 泵
    // 消费;此时不能按通用路径 drain 控制台核心,否则同一 FIFO 会被
    // console/uart 的行规程与活动 VT 竞争读取。其余终端(pty slave 等)
    // 仍需要 tick 泵推进其行规程。
    if let Some(manager) = crate::dev::tty::vt::VtManager::global()
        && manager.pump_console()
    {
        // VT 串口输入模式:物理控制台字节已由 VT 泵消费;此时不能按通用路径
        // drain 控制台核心,否则同一 FIFO 会被 console/uart 的行规程与活动
        // VT 竞争读取。其余终端(pty slave 等)仍需要 tick 泵推进。
        let console_name = manager
            .console_device()
            .map(|dev| alloc::string::String::from(dev.fw_name()));
        for tty in tty::active_tty_cores() {
            if !tty.is_active() {
                continue;
            }
            if console_name.as_deref() == Some(tty.name()) {
                continue;
            }
            let termios = tty.termios();
            tty.drain_tty_input(termios);
        }
        return;
    }
    for tty in tty::active_tty_cores() {
        if !tty.is_active() {
            continue;
        }
        let termios = tty.termios();
        tty.drain_tty_input(termios);
    }
}

impl FileOps for CharDevFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if buf.is_empty() || !self.is_tty() {
            return self.dev.read(buf).map_err(map_char_err);
        }
        let Some(tty) = self.tty.as_deref() else {
            return self.dev.read(buf).map_err(map_char_err);
        };
        // O_NONBLOCK 只影响没有完整输入时是否等待，不能绕过 TTY 行规程。
        // Ctrl-C/Ctrl-D 等控制字符必须先经过 ISIG/ICANON 处理，再由 syscall
        // 层把 WouldBlock 按文件状态转换成 EAGAIN 或阻塞等待。
        self.remember_reader_pgrp();
        let termios = tty.termios();
        if termios.canonical() {
            self.read_tty_canonical(tty, buf, termios)
        } else {
            self.read_tty_raw(tty, buf, termios)
        }
    }
    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        if self.nonblock.load(Ordering::Acquire) {
            return self.dev.write(buf).map_err(map_char_err);
        }
        let Some(tty) = self.tty.as_deref() else {
            self.dev.write_all(buf).map_err(map_char_err)?;
            return Ok(buf.len());
        };
        let termios = tty.termios();
        self.write_tty_bytes(tty, buf, termios)?;
        Ok(buf.len())
    }
    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> {
        self.dev.flush().map_err(map_char_err)
    }
    fn poll(&self, _interest: PollEvents) -> PollEvents {
        if !self.dev.is_active() {
            return PollEvents::POLLERR.with(PollEvents::POLLHUP);
        }
        if let Some(tty) = self.tty.as_deref() {
            let line_readable = tty.has_ready_input();
            // 规范模式同样要暴露底层 FIFO 的“有字节可取”状态。阻塞 read()
            // 在无完整行时会先返回 WouldBlock，再由 syscall 层按 poll() 等待；
            // 若这里只看行缓冲，UART 字节永远不会被重新拉进行规程，shell 会像
            // 串口输入失效一样卡住。
            let dev_readable = tty.poll_read();
            let readable = line_readable || dev_readable;
            return if readable {
                PollEvents::POLLIN.with(PollEvents::POLLOUT)
            } else {
                PollEvents::POLLOUT
            };
        }
        DEVTMPFS_STANDARD_POLICY.char_poll
    }

    fn poll_add_waiter(&self, task: &Arc<sched::Task>, interest: PollEvents) -> bool {
        let want_read = interest.has(PollEvents::POLLIN) || interest.has(PollEvents::POLLPRI);
        let want_write = interest.has(PollEvents::POLLOUT);
        if !want_read && !want_write {
            return false;
        }
        self.dev.poll_add_waiter(task, want_read, want_write)
    }

    fn poll_remove_waiter(&self, task: &Arc<sched::Task>) {
        self.dev.poll_remove_waiter(task);
    }

    fn is_epollable(&self) -> bool {
        true
    }

    fn set_status_flags(&self, flags: OpenOptions) {
        self.nonblock.store(flags.nonblock, Ordering::Release);
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        if !self.dev.is_active() {
            return Err(Errno::ENODEV);
        }
        // VT 设备优先走 VT/KD ioctl 表;非 VT 命令回落 TTY 表。
        if let Some(vt) = crate::dev::tty::vt::vt_from_char_device(&self.dev) {
            if let Some(result) = crate::dev::tty::vt::handle_vt_ioctl(&vt, cmd, arg)? {
                return Ok(result);
            }
        }
        let Some(tty) = self.tty.as_deref() else {
            return Err(Errno::ENOTTY);
        };

        handle_tty_ioctl(tty, self, cmd, arg)
    }
    fn release(&self) {
        crate::dev::tty::vt::note_vt_opened(&self.dev, -1);
        crate::dev::tty::pty::note_pty_opened(&self.dev, -1);
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── mknod 节点 InodeOps ─────────

/// 用户 mknod 创建的设备节点:open 时按投影名委托给已绑定设备。
///
/// 未绑定设备时按设备号反查(呈现层索引);仍未命中返回 ENXIO。
struct MknodInodeOps {
    target_name: String,
}

impl InodeOps for MknodInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        opts: &OpenOptions,
        cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let sb = mounted_devtmpfs_sb().ok_or(VfsError::NoDevice)?;
        let sb_ops = sb
            .downcast_ops::<DevTmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
        let target = sb_ops
            .lookup_node_at(&self.target_name)
            .map_err(|_| VfsError::NoSuchDeviceOrAddress)?;
        if let Some(ops) = target.downcast_ops::<DevCharOps>() {
            return ops.open(&target, opts, cred);
        }
        if let Some(ops) = target.downcast_ops::<DevBlockOps>() {
            return ops.open(&target, opts, cred);
        }
        Err(VfsError::NoSuchDeviceOrAddress)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── 字符设备 InodeOps ─────────

/// 字符设备节点的操作对象。
///
/// 直接持有 `CharDev` 句柄。句柄内部共享 active/gone 状态，设备解绑后旧 inode
/// 和已打开 fd 都会通过同一状态停止访问底层驱动。
struct DevCharOps {
    dev: CharDevice,
    tty: Option<Arc<TtyCore>>,
}

impl DevCharOps {
    fn dev(&self) -> CharDevice {
        self.dev.clone()
    }
}

impl InodeOps for DevCharOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        if !self.dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        char_dev_file_ops(self.dev.clone(), opts.nonblock)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// 构造字符设备的 FileOps(带共享行规程)。
///
/// devtmpfs 节点与 devpts 节点共用;`tty` 实例按设备 fw_name 共享,
/// 因此同一终端的多个节点/多次打开拿到同一行规程状态。
pub(crate) fn char_dev_file_ops(
    dev: CharDevice,
    nonblock: bool,
) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
    if !dev.is_active() {
        return Err(VfsError::NoDevice);
    }
    let tty = tty::shared_tty_core(&dev);
    Ok(Box::new(CharDevFileOps::new(dev, nonblock, tty)))
}

// ───────── 块设备 InodeOps ─────────

/// 块设备节点的操作对象。
///
/// 持有 `Arc<BlockDev>`，`open()` 时同样无需查找。
struct DevBlockOps {
    dev: Arc<BlockDevice>,
}

struct BlockDevFileOps {
    dev: Arc<BlockDevice>,
    sync_writes: bool,
    direct: bool,
}

fn map_bio_err(err: BioError) -> VfsError {
    match err {
        BioError::Submit(s) => match s {
            crate::dev::bio::SubmitError::Unsupported => VfsError::NotSupported,
            crate::dev::bio::SubmitError::ReadOnly => VfsError::ReadOnlyFilesystem,
            crate::dev::bio::SubmitError::QueueFull => VfsError::WouldBlock,
            crate::dev::bio::SubmitError::DeviceGone => VfsError::NoDevice,
            crate::dev::bio::SubmitError::OutOfMemory => VfsError::OutOfMemory,
            crate::dev::bio::SubmitError::InvalidRequest(_) => VfsError::InvalidArgument,
        },
        BioError::Io(i) => match i {
            crate::dev::bio::BioIoError::MediaError => VfsError::Io,
            crate::dev::bio::BioIoError::Unavailable => VfsError::NoDevice,
            crate::dev::bio::BioIoError::Timeout => VfsError::TimedOut,
            crate::dev::bio::BioIoError::ReadOnly => VfsError::ReadOnlyFilesystem,
            crate::dev::bio::BioIoError::Unsupported => VfsError::NotSupported,
        },
    }
}

fn map_bio_errno(err: BioError) -> Errno {
    match err {
        BioError::Submit(s) => match s {
            crate::dev::bio::SubmitError::Unsupported => Errno::ENOTTY,
            crate::dev::bio::SubmitError::ReadOnly => Errno::EROFS,
            crate::dev::bio::SubmitError::QueueFull => Errno::EAGAIN,
            crate::dev::bio::SubmitError::DeviceGone => Errno::ENODEV,
            crate::dev::bio::SubmitError::OutOfMemory => Errno::ENOMEM,
            crate::dev::bio::SubmitError::InvalidRequest(_) => Errno::EINVAL,
        },
        BioError::Io(i) => match i {
            crate::dev::bio::BioIoError::MediaError => Errno::EIO,
            crate::dev::bio::BioIoError::Unavailable => Errno::ENODEV,
            crate::dev::bio::BioIoError::Timeout => Errno::ETIMEDOUT,
            crate::dev::bio::BioIoError::ReadOnly => Errno::EROFS,
            crate::dev::bio::BioIoError::Unsupported => Errno::ENOTTY,
        },
    }
}

fn boxed_zeroed(len: usize) -> VfsResult<Box<[u8]>> {
    let mut data = Vec::new();
    data.try_reserve(len).map_err(|_| VfsError::OutOfMemory)?;
    data.resize(len, 0);
    Ok(data.into_boxed_slice())
}

fn block_range_for_io(dev: &BlockDevice, offset: u64, len: usize) -> VfsResult<Option<BlockRange>> {
    if len == 0 {
        return Ok(None);
    }
    if offset == u64::MAX {
        return Err(VfsError::InvalidArgument);
    }
    let block_size = dev.geometry().logical_block_size().get() as u64;
    let len_u64 = u64::try_from(len).map_err(|_| VfsError::InvalidArgument)?;
    if !offset.is_multiple_of(block_size) || !len_u64.is_multiple_of(block_size) {
        return Err(VfsError::InvalidArgument);
    }
    let blocks = len_u64 / block_size;
    let blocks = u32::try_from(blocks).map_err(|_| VfsError::InvalidArgument)?;
    Ok(Some(BlockRange {
        lba: offset / block_size,
        blocks,
    }))
}

fn block_capacity_remaining(dev: &Arc<BlockDevice>, offset: u64, len: usize) -> usize {
    let capacity = match dev.control(BlockControlRequest::GetCapacityBytes) {
        Ok(BlockControlResponse::U64(value)) => Some(value),
        _ => dev.geometry().capacity_bytes(),
    };
    let Some(capacity) = capacity else {
        return len;
    };
    if offset >= capacity {
        return 0;
    }
    let remaining = capacity - offset;
    len.min(remaining as usize)
}

fn max_blocks_per_io(dev: &BlockDevice) -> u32 {
    dev.limits()
        .max_blocks_per_io()
        .map(|n| n.get())
        .unwrap_or_else(|| DEVTMPFS_STANDARD_POLICY.max_blocks_per_io)
        .max(1)
}

fn block_io_hints(dev: &Arc<BlockDevice>) -> Result<BlockIoHints, Errno> {
    match dev
        .control(BlockControlRequest::GetIoHints)
        .map_err(map_control_errno)?
    {
        BlockControlResponse::IoHints(hints) => Ok(hints),
        _ => Err(Errno::EINVAL),
    }
}

impl BlockDeviceIoctlContext for BlockDevFileOps {
    fn control(&self, req: BlockControlRequest) -> Result<BlockControlResponse, Errno> {
        self.dev.control(req).map_err(map_control_errno)
    }

    fn io_hints(&self) -> Result<BlockIoHints, Errno> {
        block_io_hints(&self.dev)
    }

    fn submit_range(&self, op: BioOp, range: BlockRange) -> Result<(), Errno> {
        // ioctl 层已经把用户 ABI 的字节区间转换为块范围；这里仅提交 typed BIO，
        // 不解析 ioctl number，也不接触用户指针。
        self.dev
            .submit_bio_wait(op, range, BioBuffer::None)
            .map(|_| ())
            .map_err(map_bio_errno)
    }
}

fn block_read_into(dev: &Arc<BlockDevice>, lba: u64, blocks: u32, buf: &mut [u8]) -> VfsResult<()> {
    // 块设备文件整块读的热路径直接复用调用者缓冲，避免先分配 BIO owned buffer
    // 再复制到 VFS buffer。生命周期由同步提交接口收束，驱动完成后函数才返回。
    dev.submit_bio_wait_borrowed_read(BlockRange { lba, blocks }, buf)
        .map_err(map_bio_err)
}

fn block_write_from(dev: &Arc<BlockDevice>, lba: u64, blocks: u32, buf: &[u8]) -> VfsResult<()> {
    // 与读路径对称：整块写直接借用调用者缓冲提交，去掉 boxed_copy 和一次 memcpy。
    dev.submit_bio_wait_borrowed_write(BlockRange { lba, blocks }, buf)
        .map_err(map_bio_err)
}

fn flush_if_supported(dev: &Arc<BlockDevice>) -> VfsResult<()> {
    if !dev.features().contains(BlockFeatures::FLUSH) {
        return Ok(());
    }
    dev.submit_bio_wait(
        BioOp::Flush,
        BlockRange { lba: 0, blocks: 0 },
        BioBuffer::None,
    )
    .map_err(map_bio_err)?;
    Ok(())
}

impl FileOps for BlockDevFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let len = block_capacity_remaining(&self.dev, offset, buf.len());
        if len == 0 {
            return Ok(0);
        }
        let block_size = self.dev.geometry().logical_block_size().get() as usize;
        let mut done = 0usize;

        if self.direct {
            let Some(range) = block_range_for_io(&self.dev, offset, len)? else {
                return Ok(0);
            };
            let mut lba = range.lba;
            let mut remaining_blocks = range.blocks as usize;
            let max_blocks = max_blocks_per_io(&self.dev) as usize;
            while remaining_blocks != 0 {
                let blocks = remaining_blocks.min(max_blocks).min(u32::MAX as usize) as u32;
                let bytes = blocks as usize * block_size;
                block_read_into(&self.dev, lba, blocks, &mut buf[done..done + bytes])?;
                done += bytes;
                lba += blocks as u64;
                remaining_blocks -= blocks as usize;
            }
            return Ok(done);
        }

        if offset.is_multiple_of(block_size as u64) && len.is_multiple_of(block_size) {
            let mut lba = offset / block_size as u64;
            let mut remaining_blocks = len / block_size;
            let max_blocks = max_blocks_per_io(&self.dev) as usize;
            while remaining_blocks != 0 {
                let blocks = remaining_blocks.min(max_blocks).min(u32::MAX as usize) as u32;
                let bytes = blocks as usize * block_size;
                block_read_into(&self.dev, lba, blocks, &mut buf[done..done + bytes])?;
                done += bytes;
                lba += blocks as u64;
                remaining_blocks -= blocks as usize;
            }
            return Ok(done);
        }

        let mut scratch = boxed_zeroed(block_size)?;
        while done < len {
            let abs = offset.saturating_add(done as u64);
            let lba = abs / block_size as u64;
            let in_block = (abs % block_size as u64) as usize;
            let take = (block_size - in_block).min(len - done);
            block_read_into(&self.dev, lba, 1, &mut scratch)?;
            buf[done..done + take].copy_from_slice(&scratch[in_block..in_block + take]);
            done += take;
        }
        Ok(done)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let len = block_capacity_remaining(&self.dev, offset, buf.len());
        if len == 0 {
            return Ok(0);
        }
        let block_size = self.dev.geometry().logical_block_size().get() as usize;
        let mut done = 0usize;

        if self.direct {
            let Some(range) = block_range_for_io(&self.dev, offset, len)? else {
                return Ok(0);
            };
            let mut lba = range.lba;
            let mut remaining_blocks = range.blocks as usize;
            let max_blocks = max_blocks_per_io(&self.dev) as usize;
            while remaining_blocks != 0 {
                let blocks = remaining_blocks.min(max_blocks).min(u32::MAX as usize) as u32;
                let bytes = blocks as usize * block_size;
                block_write_from(&self.dev, lba, blocks, &buf[done..done + bytes])?;
                done += bytes;
                lba += blocks as u64;
                remaining_blocks -= blocks as usize;
            }
        } else if offset.is_multiple_of(block_size as u64) && len.is_multiple_of(block_size) {
            let mut lba = offset / block_size as u64;
            let mut remaining_blocks = len / block_size;
            let max_blocks = max_blocks_per_io(&self.dev) as usize;
            while remaining_blocks != 0 {
                let blocks = remaining_blocks.min(max_blocks).min(u32::MAX as usize) as u32;
                let bytes = blocks as usize * block_size;
                block_write_from(&self.dev, lba, blocks, &buf[done..done + bytes])?;
                done += bytes;
                lba += blocks as u64;
                remaining_blocks -= blocks as usize;
            }
        } else {
            let mut scratch = boxed_zeroed(block_size)?;
            while done < len {
                let abs = offset.saturating_add(done as u64);
                let lba = abs / block_size as u64;
                let in_block = (abs % block_size as u64) as usize;
                let take = (block_size - in_block).min(len - done);
                block_read_into(&self.dev, lba, 1, &mut scratch)?;
                scratch[in_block..in_block + take].copy_from_slice(&buf[done..done + take]);
                block_write_from(&self.dev, lba, 1, &scratch)?;
                done += take;
            }
        }
        if self.sync_writes {
            flush_if_supported(&self.dev)?;
        }
        Ok(done)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        if !self.dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        flush_if_supported(&self.dev)?;
        Ok(())
    }

    fn poll(&self, _interest: PollEvents) -> PollEvents {
        if !self.dev.is_active() {
            return PollEvents::POLLERR.with(PollEvents::POLLHUP);
        }
        PollEvents::POLLIN.with(PollEvents::POLLOUT)
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        if !self.dev.is_active() {
            return Err(Errno::ENODEV);
        }
        if let Some(result) =
            crate::vfs::device_files::loop_device::try_loop_block_ioctl(&self.dev, cmd, arg)
        {
            return result;
        }

        handle_block_ioctl(self, cmd, arg)
    }

    fn release(&self) {
        self.dev.release_file();
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl InodeOps for DevBlockOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        if !self.dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        self.dev.open_file().map_err(map_control_vfs)?;
        Ok(Box::new(BlockDevFileOps {
            dev: Arc::clone(&self.dev),
            sync_writes: opts.sync,
            direct: opts.direct,
        }))
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// 从 devtmpfs 块设备 inode 中恢复底层块设备对象。
///
/// 这是给 blockfs 挂载源解析使用的窄接口：调用方仍然通过 VFS 解析路径和符号链接，
/// 只有最终确认 inode 属于 devtmpfs 块设备节点后，才取出其内联保存的设备对象。
pub fn block_device_from_inode(inode: &Inode) -> Option<Arc<BlockDevice>> {
    if inode.kind() != FileType::BlockDevice {
        return None;
    }
    let ops = inode.downcast_ops::<DevBlockOps>()?;
    let dev = Arc::clone(&ops.dev);
    dev.is_active().then_some(dev)
}

// ───────── 符号链接 InodeOps ─────────

/// devtmpfs 符号链接节点的操作对象。
///
/// 只保存链接目标文本。相对目标由 VFS path walker 按“链接所在目录”继续解析。
struct DevSymlinkOps {
    target: String,
}

fn custom_devnode_file_type(kind: CustomDevNodeKind) -> FileType {
    match kind {
        CustomDevNodeKind::CharDevice => FileType::CharDevice,
        CustomDevNodeKind::BlockDevice => FileType::BlockDevice,
        CustomDevNodeKind::RegularFile => FileType::Regular,
        CustomDevNodeKind::Directory => FileType::Directory,
    }
}

fn custom_devnode_ops(spec: &CustomDevNodeSpec) -> VfsResult<Arc<dyn InodeOps + Send + Sync>> {
    // dev core 只保存 opaque payload；这里按注册的 VFS 适配器顺序解释 payload。
    // devtmpfs 不直接 downcast 到具体设备 endpoint，从而保持核心文件系统对
    // 后续设备类别开放。先复制适配器快照再调用构造函数，避免 VFS 适配器内部再
    // 注册节点或访问 devtmpfs 时和全局 adapter 表形成锁顺序依赖。
    let adapters = CUSTOM_DEVNODE_ADAPTERS.lock().clone();
    for adapter in adapters {
        if let Some(ops) = adapter.build(spec)? {
            return Ok(ops);
        }
    }
    Err(VfsError::InvalidArgument)
}

impl InodeOps for DevSymlinkOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn readlink(&self, inode: &Inode) -> VfsResult<String> {
        if inode.kind() != FileType::Symlink {
            return Err(VfsError::InvalidArgument);
        }
        Ok(self.target.clone())
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Err(VfsError::InvalidArgument)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── 目录 InodeOps ─────────

/// devtmpfs 目录操作对象。
///
/// 每个目录只维护本级 `name → Arc<Inode>` 映射。设备节点的批量增删通过
/// [`DevTmpfsSuperblockOps`] 对外暴露，普通符号链接和目录创建也走 VFS 标准入口。
pub struct DevDirOps {
    pub(crate) children: Spinlock<BTreeMap<String, Arc<Inode>>>,
}

impl DevDirOps {
    fn new() -> Self {
        Self {
            children: Spinlock::new(BTreeMap::new()),
        }
    }

    /// 返回当前子节点的快照：`(user_name, Arc<Inode>)` 列表。
    pub fn try_children_snapshot(&self) -> VfsResult<alloc::vec::Vec<(String, Arc<Inode>)>> {
        let children = self.children.lock();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve(children.len())
            .map_err(|_| VfsError::NoSpace)?;
        for (name, inode) in children.iter() {
            snapshot.push((devtmpfs_fallible_string(name)?, Arc::clone(inode)));
        }
        Ok(snapshot)
    }

    /// 返回当前子节点的快照：`(user_name, Arc<Inode>)` 列表。
    pub fn children_snapshot(&self) -> alloc::vec::Vec<(String, Arc<Inode>)> {
        self.try_children_snapshot().unwrap_or_default()
    }
}

impl InodeOps for DevDirOps {
    fn lookup(&self, _inode: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        self.children
            .lock()
            .get(name)
            .cloned()
            .ok_or(VfsError::NotFound)
    }

    fn mknod(
        &self,
        dir: &Inode,
        name: &str,
        kind: FileType,
        mode: FileMode,
        dev: DevId,
        cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        if !matches!(kind, FileType::CharDevice | FileType::BlockDevice) {
            // devtmpfs 只投影设备节点;其他 mknod 类型不支持。
            return Err(VfsError::NotSupported);
        }
        validate_devtmpfs_component(name)?;

        let sb = dir.superblock().ok_or(VfsError::InvalidArgument)?;
        let sb_ops = sb
            .downcast_ops::<DevTmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;

        // 设备号只作呈现层键:经 device_numbers 反查投影节点名,open 时
        // 委托给已绑定设备;未登记的设备号在 open 时返回 ENXIO(Linux 语义)。
        let node_kind = match kind {
            FileType::CharDevice => super::user_api::device_numbers::DeviceNumberKind::Char,
            _ => super::user_api::device_numbers::DeviceNumberKind::Block,
        };
        let Some(record) = super::user_api::device_numbers::lookup_rdev(node_kind, dev) else {
            // 允许先建节点后绑定设备:open 时再解析。
            let inode = sb_ops.new_mknod_inode(name, kind, mode, dev, cred, name)?;
            return sb_ops.insert_mknod(dir, name, inode);
        };

        let inode = sb_ops.new_mknod_inode(name, kind, mode, dev, cred, &record.node_name)?;
        sb_ops.insert_mknod(dir, name, inode)
    }

    fn mkdir(
        &self,
        dir: &Inode,
        name: &str,
        mode: FileMode,
        cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        validate_devtmpfs_component(name)?;

        let sb = dir.superblock().ok_or(VfsError::InvalidArgument)?;
        let sb_ops = sb
            .downcast_ops::<DevTmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
        let inode = sb_ops.new_dir_inode(mode, cred.euid, cred.egid)?;

        let mut children = self.children.lock();
        if children.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        children.insert(devtmpfs_fallible_string(name)?, Arc::clone(&inode));
        drop(children);

        dir.inc_nlink();
        dir.touch_mtime();
        dir.touch_ctime();
        Ok(inode)
    }

    fn symlink(
        &self,
        dir: &Inode,
        name: &str,
        target: &str,
        cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        validate_devtmpfs_component(name)?;
        validate_symlink_target(target)?;

        let sb = dir.superblock().ok_or(VfsError::InvalidArgument)?;
        let sb_ops = sb
            .downcast_ops::<DevTmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
        let inode = sb_ops.new_symlink_inode(target, cred.euid, cred.egid)?;

        let mut children = self.children.lock();
        if children.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        children.insert(devtmpfs_fallible_string(name)?, Arc::clone(&inode));
        drop(children);

        dir.touch_mtime();
        dir.touch_ctime();
        Ok(inode)
    }

    fn rmdir(&self, dir: &Inode, name: &str, child: &Inode) -> VfsResult<()> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        if child.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }

        let child_ops = child
            .downcast_ops::<DevDirOps>()
            .ok_or(VfsError::InvalidArgument)?;
        if !child_ops.children.lock().is_empty() {
            return Err(VfsError::DirectoryNotEmpty);
        }

        let mut children = self.children.lock();
        let existing = children.get(name).ok_or(VfsError::NotFound)?;
        if existing.fs_id() != child.fs_id() || existing.ino() != child.ino() {
            return Err(VfsError::NotFound);
        }
        let removed = children.remove(name).ok_or(VfsError::NotFound)?;
        drop(children);

        dir.dec_nlink();
        dir.touch_mtime();
        dir.touch_ctime();
        removed.set_nlink(0);
        removed.touch_ctime();
        Ok(())
    }

    fn unlink(&self, dir: &Inode, name: &str, child: &Inode) -> VfsResult<()> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        if child.kind() == FileType::Directory {
            return Err(VfsError::IsADirectory);
        }
        if child.kind() != FileType::Symlink {
            return Err(VfsError::OperationNotPermitted);
        }

        let mut children = self.children.lock();
        let existing = children.get(name).ok_or(VfsError::NotFound)?;
        if existing.fs_id() != child.fs_id() || existing.ino() != child.ino() {
            return Err(VfsError::NotFound);
        }
        let removed = children.remove(name).ok_or(VfsError::NotFound)?;
        drop(children);

        dir.touch_mtime();
        dir.touch_ctime();
        removed.set_nlink(0);
        removed.touch_ctime();
        Ok(())
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let children = self.children.lock();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve(children.len())
            .map_err(|_| VfsError::NoSpace)?;
        for (name, inode) in children.iter() {
            snapshot.push(DirEntry {
                ino: inode.ino(),
                name: devtmpfs_fallible_smallstr(name)?,
                kind: inode.kind(),
            });
        }
        Ok(Box::new(DevRootFile { snapshot }))
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── 根目录 FileOps ─────────

struct DevRootFile {
    snapshot: alloc::vec::Vec<DirEntry>,
}

impl FileOps for DevRootFile {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }

    fn readdir(
        &self,
        pos: u64,
        sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        let start = pos as usize;
        for (i, entry) in self.snapshot.iter().enumerate().skip(start) {
            if sink(entry.clone()).is_break() {
                return Ok(i as u64);
            }
        }
        Ok(self.snapshot.len() as u64)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, interest: PollEvents) -> PollEvents {
        // 根目录是设备表快照，目录项读取不会等待设备事件。
        PollEvents::READ_WRITE_READY.intersect(interest)
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── SuperblockOps ─────────

/// devtmpfs 超级块操作对象。
///
/// 同时提供公开的 `bind_char` / `bind_block` / `bind_symlink` / `unbind` API，
/// 让设备驱动或兼容层在设备注册/注销时同步更新 `/dev` 下的节点。
pub struct DevTmpfsSuperblockOps {
    next_ino: AtomicU64,
    /// 超级块弱引用，创建 Inode 时需要
    sb: vfs::sync::Spinlock<Option<alloc::sync::Weak<Superblock>>>,
    /// 本 devtmpfs 实例实际创建过的 `dev_t` 投影节点。
    ///
    /// 符号链接、普通文件、目录以及未注册 `dev_t` 的节点解绑时不能无条件清理
    /// device_numbers registry，否则会误删其他兼容层入口留下的记录。
    numbered_nodes: Spinlock<Vec<String>>,
    /// 由 devtmpfs 自动补出来的中间目录路径。
    ///
    /// 只有这类目录会在节点解绑后自动收缩；用户显式 `mkdir` 的目录不进入此表，
    /// 避免文件系统帮用户删除自己创建的空目录。
    implicit_dirs: Spinlock<Vec<String>>,
}

impl DevTmpfsSuperblockOps {
    fn alloc_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    fn fs_id(&self) -> Option<FsId> {
        self.sb.lock().as_ref()?.upgrade().map(|sb| sb.fs_id)
    }

    fn sb_weak(&self) -> Option<alloc::sync::Weak<Superblock>> {
        self.sb.lock().clone()
    }

    fn remember_numbered_node(&self, name: &str) -> VfsResult<()> {
        let mut nodes = self.numbered_nodes.lock();
        if nodes.iter().any(|existing| existing == name) {
            return Ok(());
        }
        nodes.try_reserve(1).map_err(|_| VfsError::NoSpace)?;
        nodes.push(devtmpfs_fallible_string(name)?);
        Ok(())
    }

    fn forget_numbered_node(&self, name: &str) -> bool {
        let mut nodes = self.numbered_nodes.lock();
        let Some(index) = nodes.iter().position(|existing| existing == name) else {
            return false;
        };
        nodes.remove(index);
        true
    }

    fn rollback_numbered_node(&self, name: &str) {
        if self.forget_numbered_node(name) {
            super::user_api::device_numbers::unregister_node(name);
        }
    }

    fn remember_implicit_dir(&self, path: &str) -> VfsResult<()> {
        let mut dirs = self.implicit_dirs.lock();
        if dirs.iter().any(|existing| existing == path) {
            return Ok(());
        }
        dirs.try_reserve(1).map_err(|_| VfsError::NoSpace)?;
        dirs.push(devtmpfs_fallible_string(path)?);
        Ok(())
    }

    fn is_implicit_dir(&self, path: &str) -> bool {
        self.implicit_dirs
            .lock()
            .iter()
            .any(|existing| existing == path)
    }

    fn forget_implicit_dir(&self, path: &str) -> bool {
        let mut dirs = self.implicit_dirs.lock();
        let Some(index) = dirs.iter().position(|existing| existing == path) else {
            return false;
        };
        dirs.remove(index);
        true
    }

    fn rollback_implicit_dirs(&self, paths: &[String]) {
        for path in paths.iter().rev() {
            let _ = self.remove_implicit_dir_path(path);
        }
    }

    fn join_components(components: &[&str]) -> VfsResult<String> {
        let mut path = String::new();
        for component in components {
            if !path.is_empty() {
                path.push('/');
            }
            path.try_reserve(component.len())
                .map_err(|_| VfsError::NoSpace)?;
            path.push_str(component);
        }
        Ok(path)
    }

    fn lookup_dir_at_components(&self, components: &[&str]) -> VfsResult<Arc<Inode>> {
        let mut dir_inode = self.root_inode()?;
        for component in components {
            let dir_ops = dir_inode
                .downcast_ops::<DevDirOps>()
                .ok_or(VfsError::NotADirectory)?;
            let next = dir_ops
                .children
                .lock()
                .get(*component)
                .cloned()
                .ok_or(VfsError::NotFound)?;
            if next.kind() != FileType::Directory {
                return Err(VfsError::NotADirectory);
            }
            dir_inode = next;
        }
        Ok(dir_inode)
    }

    fn remove_implicit_dir_path(&self, path: &str) -> VfsResult<bool> {
        if !self.is_implicit_dir(path) {
            return Ok(false);
        }

        let components = split_devtmpfs_path(path)?;
        let name = components
            .last()
            .copied()
            .ok_or(VfsError::InvalidArgument)?;
        let parent = if components.len() == 1 {
            self.root_inode()?
        } else {
            self.lookup_dir_at_components(&components[..components.len() - 1])?
        };
        let parent_ops = parent
            .downcast_ops::<DevDirOps>()
            .ok_or(VfsError::NotADirectory)?;
        let child = {
            let children = parent_ops.children.lock();
            children.get(name).cloned()
        };
        let Some(child) = child else {
            return Ok(false);
        };
        if child.kind() != FileType::Directory {
            return Ok(false);
        }
        let child_ops = child
            .downcast_ops::<DevDirOps>()
            .ok_or(VfsError::InvalidArgument)?;
        if !child_ops.children.lock().is_empty() {
            return Ok(false);
        }

        let removed = {
            let mut children = parent_ops.children.lock();
            children.remove(name)
        };
        let Some(removed) = removed else {
            return Ok(false);
        };
        if removed.ino() != child.ino() || removed.fs_id() != child.fs_id() {
            let mut children = parent_ops.children.lock();
            children.insert(devtmpfs_fallible_string(name)?, removed);
            return Ok(false);
        }

        parent.dec_nlink();
        parent.touch_mtime();
        parent.touch_ctime();
        removed.set_nlink(0);
        removed.touch_ctime();
        if let Some(sb) = removed.superblock() {
            sb.remove_inode(removed.ino());
        }
        self.invalidate_path_dcache(path);
        let _ = self.forget_implicit_dir(path);
        Ok(true)
    }

    fn prune_implicit_dir_chain(&self, components: &[&str]) {
        if components.is_empty() {
            return;
        }
        for len in (1..=components.len()).rev() {
            let Ok(path) = Self::join_components(&components[..len]) else {
                return;
            };
            match self.remove_implicit_dir_path(&path) {
                Ok(true) => continue,
                Ok(false) | Err(_) => return,
            }
        }
    }

    fn root_inode(&self) -> VfsResult<Arc<Inode>> {
        self.sb
            .lock()
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .map(|sb| Arc::clone(&sb.root_inode))
            .ok_or(VfsError::InvalidArgument)
    }

    fn invalidate_path_dcache(&self, path: &str) {
        let Some(sb) = self.sb.lock().as_ref().and_then(|weak| weak.upgrade()) else {
            return;
        };

        let mut parent = Arc::clone(&sb.root_dentry);
        let mut components = path.split('/').peekable();
        while let Some(component) = components.next() {
            let Some(dentry) = vfs::DCACHE.get(&parent, component) else {
                return;
            };
            if components.peek().is_none() {
                vfs::DCACHE.invalidate_dentry(&dentry);
                dentry.invalidate();
                return;
            }
            if !dentry.is_positive() {
                return;
            }
            parent = dentry;
        }
    }

    fn new_dir_inode(&self, mode: FileMode, uid: Uid, gid: Gid) -> VfsResult<Arc<Inode>> {
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;

        let now = Timespec::now();
        let meta = InodeMeta {
            size: 0,
            nlink: 2,
            mode,
            uid,
            gid,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        Ok(Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            FileType::Directory,
            DevId::new(0, 0),
            DEVTMPFS_STANDARD_POLICY.block_size,
            None,
            meta,
            Arc::new(DevDirOps::new()),
            sb_weak,
        ))
    }

    fn new_symlink_inode(&self, target: &str, uid: Uid, gid: Gid) -> VfsResult<Arc<Inode>> {
        validate_symlink_target(target)?;
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;

        let now = Timespec::now();
        let meta = InodeMeta {
            size: target.len() as u64,
            nlink: 1,
            mode: DEVTMPFS_STANDARD_POLICY.symlink_mode,
            uid,
            gid,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        Ok(Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            FileType::Symlink,
            DevId::new(0, 0),
            DEVTMPFS_STANDARD_POLICY.block_size,
            None,
            meta,
            Arc::new(DevSymlinkOps {
                target: devtmpfs_fallible_string(target)?,
            }),
            sb_weak,
        ))
    }

    fn new_custom_inode(&self, spec: &CustomDevNodeSpec, rdev: DevId) -> VfsResult<Arc<Inode>> {
        let policy = node_policy(spec.name());
        split_devtmpfs_path(spec.name())?;
        let block_size = DEVTMPFS_STANDARD_POLICY.block_size;
        let nlink = DEVTMPFS_STANDARD_POLICY.custom_nlink(spec.kind());
        if block_size == 0 || nlink == 0 {
            return Err(VfsError::InvalidArgument);
        }
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;
        let kind = custom_devnode_file_type(spec.kind());
        let ops = custom_devnode_ops(spec)?;

        let now = Timespec::now();
        let meta = InodeMeta {
            size: 0,
            nlink,
            // 设备节点优先使用按名登记的节点策略(如 ptmx 0666);目录/普通
            // 文件保持标准策略。
            mode: if matches!(spec.kind(), CustomDevNodeKind::CharDevice | CustomDevNodeKind::BlockDevice) {
                policy.mode
            } else {
                DEVTMPFS_STANDARD_POLICY.custom_mode(spec.kind())
            },
            uid: if matches!(spec.kind(), CustomDevNodeKind::CharDevice | CustomDevNodeKind::BlockDevice) {
                policy.uid
            } else {
                DEVTMPFS_STANDARD_POLICY.uid
            },
            gid: if matches!(spec.kind(), CustomDevNodeKind::CharDevice | CustomDevNodeKind::BlockDevice) {
                policy.gid
            } else {
                DEVTMPFS_STANDARD_POLICY.gid
            },
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        Ok(Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            kind,
            rdev,
            block_size,
            None,
            meta,
            ops,
            sb_weak,
        ))
    }

    fn ensure_parent_dir(&self, components: &[&str]) -> VfsResult<Arc<Inode>> {
        let mut dir_inode = self.root_inode()?;
        let mut current_path = String::new();
        let mut created_paths: Vec<String> = Vec::new();

        for component in &components[..components.len().saturating_sub(1)] {
            if !current_path.is_empty() {
                current_path.push('/');
            }
            if let Err(_) = current_path.try_reserve(component.len()) {
                self.rollback_implicit_dirs(&created_paths);
                return Err(VfsError::NoSpace);
            }
            current_path.push_str(component);

            let dir_ops = dir_inode
                .downcast_ops::<DevDirOps>()
                .ok_or(VfsError::NotADirectory)?;
            let mut created = false;
            let next = {
                let mut children = dir_ops.children.lock();
                if let Some(existing) = children.get(*component).cloned() {
                    existing
                } else {
                    let child = self.new_dir_inode(
                        DEVTMPFS_STANDARD_POLICY.dir_mode,
                        DEVTMPFS_STANDARD_POLICY.uid,
                        DEVTMPFS_STANDARD_POLICY.gid,
                    )?;
                    let path = match devtmpfs_fallible_string(&current_path) {
                        Ok(path) => path,
                        Err(err) => {
                            self.rollback_implicit_dirs(&created_paths);
                            return Err(err);
                        }
                    };
                    if let Err(err) = self.remember_implicit_dir(&path) {
                        self.rollback_implicit_dirs(&created_paths);
                        return Err(err);
                    }
                    let key = match devtmpfs_fallible_string(component) {
                        Ok(key) => key,
                        Err(err) => {
                            let _ = self.forget_implicit_dir(&path);
                            self.rollback_implicit_dirs(&created_paths);
                            return Err(err);
                        }
                    };
                    children.insert(key, Arc::clone(&child));
                    created = true;
                    created_paths.push(path);
                    child
                }
            };

            if next.kind() != FileType::Directory {
                self.rollback_implicit_dirs(&created_paths);
                return Err(VfsError::NotADirectory);
            }
            if created {
                dir_inode.inc_nlink();
                dir_inode.touch_mtime();
                dir_inode.touch_ctime();
                self.invalidate_path_dcache(&current_path);
            }
            dir_inode = next;
        }

        Ok(dir_inode)
    }

    fn lookup_parent_dir(&self, components: &[&str]) -> VfsResult<Arc<Inode>> {
        let mut dir_inode = self.root_inode()?;
        for component in &components[..components.len().saturating_sub(1)] {
            let dir_ops = dir_inode
                .downcast_ops::<DevDirOps>()
                .ok_or(VfsError::NotADirectory)?;
            let next = dir_ops
                .children
                .lock()
                .get(*component)
                .cloned()
                .ok_or(VfsError::NotFound)?;
            if next.kind() != FileType::Directory {
                return Err(VfsError::NotADirectory);
            }
            dir_inode = next;
        }
        Ok(dir_inode)
    }

    fn insert_node_at(&self, path: &str, inode: Arc<Inode>) -> VfsResult<()> {
        let components = split_devtmpfs_path(path)?;
        let name = components
            .last()
            .copied()
            .ok_or(VfsError::InvalidArgument)?;
        let parent = match self.ensure_parent_dir(&components) {
            Ok(parent) => parent,
            Err(err) => return Err(err),
        };
        let parent_ops = parent
            .downcast_ops::<DevDirOps>()
            .ok_or(VfsError::NotADirectory)?;

        let mut children = parent_ops.children.lock();
        if children.contains_key(name) {
            self.prune_implicit_dir_chain(&components[..components.len().saturating_sub(1)]);
            return Err(VfsError::AlreadyExists);
        }
        children.insert(devtmpfs_fallible_string(name)?, inode);
        drop(children);

        parent.touch_mtime();
        parent.touch_ctime();
        self.invalidate_path_dcache(path);
        Ok(())
    }

    fn remove_node_at(&self, path: &str) -> VfsResult<Arc<Inode>> {
        let components = split_devtmpfs_path(path)?;
        let name = components
            .last()
            .copied()
            .ok_or(VfsError::InvalidArgument)?;
        let parent = self.lookup_parent_dir(&components)?;
        let parent_ops = parent
            .downcast_ops::<DevDirOps>()
            .ok_or(VfsError::NotADirectory)?;

        let mut children = parent_ops.children.lock();
        let (owned_name, inode) = children.remove_entry(name).ok_or(VfsError::NotFound)?;
        drop(children);

        if inode.kind() == FileType::Directory {
            let dir_ops = inode
                .downcast_ops::<DevDirOps>()
                .ok_or(VfsError::InvalidArgument)?;
            if !dir_ops.children.lock().is_empty() {
                let mut children = parent_ops.children.lock();
                children.insert(owned_name, Arc::clone(&inode));
                return Err(VfsError::DirectoryNotEmpty);
            }
            parent.dec_nlink();
        }

        parent.touch_mtime();
        parent.touch_ctime();
        inode.set_nlink(0);
        inode.touch_ctime();
        if let Some(sb) = inode.superblock() {
            sb.remove_inode(inode.ino());
        }
        self.invalidate_path_dcache(path);
        self.prune_implicit_dir_chain(&components[..components.len().saturating_sub(1)]);
        Ok(inode)
    }

    fn lookup_node_at(&self, path: &str) -> VfsResult<Arc<Inode>> {
        let components = split_devtmpfs_path(path)?;
        let name = components
            .last()
            .copied()
            .ok_or(VfsError::InvalidArgument)?;
        let parent = self.lookup_parent_dir(&components)?;
        let parent_ops = parent
            .downcast_ops::<DevDirOps>()
            .ok_or(VfsError::NotADirectory)?;
        parent_ops
            .children
            .lock()
            .get(name)
            .cloned()
            .ok_or(VfsError::NotFound)
    }

    /// 将字符设备绑定到 devtmpfs 相对路径。
    ///
    /// - `user_name`：用户空间可见的相对路径（如 `"console"` 或 `"tty/serial0"`）
    /// - `dev`：已注册的字符设备对象（直接存入 inode，不再保存名称）
    pub fn bind_char(&self, user_name: &str, dev: CharDevice) -> VfsResult<()> {
        split_devtmpfs_path(user_name)?;
        if !dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        // 节点已存在且设备号一致时幂等复用(initramfs 预建节点/重复绑定)。
        // register_char 对同名已登记记录返回其原设备号,因此这里同时完成
        // 复用判定与(必要时)设备号登记。
        if self.lookup_node_at(user_name).is_ok() {
            let expected = super::user_api::device_numbers::register_char(user_name, dev.fw_name())
                .ok_or(VfsError::NoSpace)?;
            let existing_rdev =
                super::user_api::device_numbers::lookup_node(user_name).map(|record| record.rdev);
            return if existing_rdev == Some(expected) {
                Ok(())
            } else {
                Err(VfsError::AlreadyExists)
            };
        }
        let rdev = super::user_api::device_numbers::register_char(user_name, dev.fw_name())
            .ok_or(VfsError::NoSpace)?;
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;
        let policy = node_policy(user_name);

        let now = Timespec::now();
        let meta = InodeMeta {
            size: 0,
            nlink: 1,
            mode: policy.mode,
            uid: policy.uid,
            gid: policy.gid,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        let tty = tty::shared_tty_core(&dev);
        let ops = Arc::new(DevCharOps { dev, tty });
        let inode = Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            FileType::CharDevice,
            rdev,
            DEVTMPFS_STANDARD_POLICY.block_size,
            None,
            meta,
            ops,
            sb_weak,
        );

        if let Err(err) = self.insert_node_at(user_name, inode) {
            super::user_api::device_numbers::unregister_node(user_name);
            return Err(err);
        }
        if let Err(err) = self.remember_numbered_node(user_name) {
            let _ = self.remove_node_at(user_name);
            super::user_api::device_numbers::unregister_node(user_name);
            return Err(err);
        }
        Ok(())
    }

    /// 绑定已经注册的静态节点。
    ///
    /// mount 时调用一次，用于把早于 devtmpfs 出现的非 PnP 节点批量投影到
    /// 当前 superblock。运行期后注册的静态节点由 [`register_static_dev_node`]
    /// 直接补绑。
    fn bind_registered_static_nodes(&self) -> VfsResult<()> {
        let mut bound: Vec<&'static str> = Vec::new();
        // 复制静态节点声明后再执行绑定，保证节点构造路径不会在持有全局静态
        // 注册表锁时回调到 VFS 或 dev core。
        let nodes = STATIC_DEV_NODES.lock().clone();
        for node in nodes {
            if let Err(err) = bind_static_node(self, node) {
                for name in bound.iter().rev() {
                    let _ = self.unbind(name);
                }
                return Err(err);
            }
            bound.push(node.name());
        }
        Ok(())
    }

    /// 将块设备绑定到 devtmpfs 相对路径。
    ///
    /// - `user_name`：用户空间可见的相对路径（如 `"block/root"`）
    /// - `dev`：已注册的块设备对象（`Arc` 直接存入 inode）
    pub fn bind_block(&self, user_name: &str, dev: Arc<BlockDevice>) -> VfsResult<()> {
        split_devtmpfs_path(user_name)?;
        if !dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        if self.lookup_node_at(user_name).is_ok() {
            let expected = super::user_api::device_numbers::register_block(user_name, dev.name())
                .ok_or(VfsError::NoSpace)?;
            let existing_rdev =
                super::user_api::device_numbers::lookup_node(user_name).map(|record| record.rdev);
            return if existing_rdev == Some(expected) {
                Ok(())
            } else {
                Err(VfsError::AlreadyExists)
            };
        }
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;
        let rdev = super::user_api::device_numbers::register_block(user_name, dev.name())
            .ok_or(VfsError::NoSpace)?;
        let policy = node_policy(user_name);

        let now = Timespec::now();
        let meta = InodeMeta {
            size: 0,
            nlink: 1,
            mode: policy.mode,
            uid: policy.uid,
            gid: policy.gid,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        let ops = Arc::new(DevBlockOps { dev });
        let inode = Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            FileType::BlockDevice,
            rdev,
            DEVTMPFS_STANDARD_POLICY.block_size,
            None,
            meta,
            ops,
            sb_weak,
        );

        if let Err(err) = self.insert_node_at(user_name, inode) {
            super::user_api::device_numbers::unregister_node(user_name);
            return Err(err);
        }
        if let Err(err) = self.remember_numbered_node(user_name) {
            let _ = self.remove_node_at(user_name);
            super::user_api::device_numbers::unregister_node(user_name);
            return Err(err);
        }
        Ok(())
    }

    /// 在 devtmpfs 相对路径上创建一个符号链接节点。
    ///
    /// `target` 按标准符号链接文本保存，不在创建时验证目标是否存在。相对目标会按
    /// VFS path walker 的规则以链接所在目录为基准继续解析。
    pub fn bind_symlink(&self, user_name: &str, target: &str) -> VfsResult<()> {
        split_devtmpfs_path(user_name)?;
        let inode = self.new_symlink_inode(
            target,
            DEVTMPFS_STANDARD_POLICY.uid,
            DEVTMPFS_STANDARD_POLICY.gid,
        )?;
        self.insert_node_at(user_name, inode)
    }

    /// 绑定一个自定义 devtmpfs 节点。
    ///
    /// 自定义节点的底层 function 只提交 opaque payload；这里作为 VFS 用户接口
    /// 适配层负责解释 payload、分配兼容 `dev_t` 并创建 inode。
    /// 构造一个用户 mknod 的设备节点 inode。
    ///
    /// open 时按 `target_name`(已登记投影)或设备号(open 时反查)解析设备。
    fn new_mknod_inode(
        &self,
        name: &str,
        kind: FileType,
        mode: FileMode,
        dev: DevId,
        cred: &Credentials,
        target_name: &str,
    ) -> VfsResult<Arc<Inode>> {
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;
        let now = Timespec::now();
        let meta = InodeMeta {
            size: 0,
            nlink: 1,
            mode,
            uid: cred.euid,
            gid: cred.egid,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };
        let ops = Arc::new(MknodInodeOps {
            target_name: devtmpfs_fallible_string(target_name)?,
        });
        Ok(Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            kind,
            dev,
            DEVTMPFS_STANDARD_POLICY.block_size,
            None,
            meta,
            ops,
            sb_weak,
        ))
    }

    /// 把 mknod 节点插入父目录。
    fn insert_mknod(&self, dir: &Inode, name: &str, inode: Arc<Inode>) -> VfsResult<Arc<Inode>> {
        let parent_ops = dir
            .downcast_ops::<DevDirOps>()
            .ok_or(VfsError::InvalidArgument)?;
        let mut children = parent_ops.children.lock();
        if children.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        children.insert(devtmpfs_fallible_string(name)?, Arc::clone(&inode));
        drop(children);
        dir.inc_nlink();
        dir.touch_mtime();
        dir.touch_ctime();
        Ok(inode)
    }

    pub fn bind_custom(&self, spec: &CustomDevNodeSpec) -> VfsResult<()> {
        split_devtmpfs_path(spec.name())?;
        if self.lookup_node_at(spec.name()).is_ok() {
            return Err(VfsError::AlreadyExists);
        }

        // custom 节点只从 dev core 接收通用类别和 opaque payload；兼容设备号
        // 是 devtmpfs/stat/proc/sysfs 这条用户 ABI 投影链路的状态，不能由底层
        // function 指定或反向影响设备身份。
        let (rdev, registered_rdev) = match spec.kind() {
            CustomDevNodeKind::CharDevice => (
                match spec.numbering() {
                    CustomDevNodeNumbering::Default => {
                        super::user_api::device_numbers::register_char(spec.name(), spec.name())
                    }
                    CustomDevNodeNumbering::MiscChar => {
                        super::user_api::device_numbers::register_misc_char(
                            spec.name(),
                            spec.name(),
                        )
                    }
                }
                .ok_or(VfsError::NoSpace)?,
                true,
            ),
            CustomDevNodeKind::BlockDevice => {
                if spec.numbering() != CustomDevNodeNumbering::Default {
                    return Err(VfsError::InvalidArgument);
                }
                (
                    super::user_api::device_numbers::register_block(spec.name(), spec.name())
                        .ok_or(VfsError::NoSpace)?,
                    true,
                )
            }
            CustomDevNodeKind::RegularFile | CustomDevNodeKind::Directory => {
                if spec.numbering() != CustomDevNodeNumbering::Default {
                    return Err(VfsError::InvalidArgument);
                }
                (DevId::new(0, 0), false)
            }
        };
        let inode = match self.new_custom_inode(spec, rdev) {
            Ok(inode) => inode,
            Err(err) => {
                if registered_rdev {
                    super::user_api::device_numbers::unregister_node(spec.name());
                }
                return Err(err);
            }
        };
        if let Err(err) = self.insert_node_at(spec.name(), inode) {
            if registered_rdev {
                super::user_api::device_numbers::unregister_node(spec.name());
            }
            return Err(err);
        }
        if registered_rdev && let Err(err) = self.remember_numbered_node(spec.name()) {
            let _ = self.remove_node_at(spec.name());
            super::user_api::device_numbers::unregister_node(spec.name());
            return Err(err);
        }
        Ok(())
    }

    /// 绑定一个通用 devtmpfs 节点规格。
    pub fn bind_node(&self, node: &DevNodeSpec) -> VfsResult<()> {
        match node {
            DevNodeSpec::Char { name, dev } => self.bind_char(name, dev.clone()),
            DevNodeSpec::Block { name, dev } => self.bind_block(name, Arc::clone(dev)),
            DevNodeSpec::Symlink { name, target } => self.bind_symlink(name, target),
            DevNodeSpec::Custom(spec) => self.bind_custom(spec),
        }
    }

    /// 批量绑定一个 function 声明的 devtmpfs 节点集合。
    ///
    /// 任一节点创建失败时，已经创建的节点会按逆序回滚。这样 PnP 注册要么完整暴露
    /// 一个 function 的全部节点，要么不留下半完成名字空间状态。
    pub fn bind_nodes(&self, nodes: &DevNodeSet) -> VfsResult<()> {
        let mut bound: Vec<&str> = Vec::new();
        for node in nodes.nodes() {
            if bound.try_reserve(1).is_err() {
                for name in bound.iter().rev() {
                    let _ = self.unbind(name);
                }
                return Err(VfsError::NoSpace);
            }
            if let Err(err) = self.bind_node(node) {
                for name in bound.iter().rev() {
                    let _ = self.unbind(name);
                }
                return Err(err);
            }
            bound.push(node.name());
        }
        Ok(())
    }

    /// 解除设备绑定，删除 devtmpfs 中的相对路径节点。
    pub fn unbind(&self, user_name: &str) -> VfsResult<()> {
        self.remove_node_at(user_name)?;
        self.rollback_numbered_node(user_name);
        Ok(())
    }

    /// 批量解绑一个 function 声明的 devtmpfs 节点集合。
    pub fn unbind_nodes(&self, nodes: &DevNodeSet) -> VfsResult<()> {
        let mut last_error = None;
        for node in nodes.nodes().iter().rev() {
            match self.unbind(node.name()) {
                Ok(()) | Err(VfsError::NotFound) => {}
                Err(err) => last_error = Some(err),
            }
        }
        if let Some(err) = last_error {
            Err(err)
        } else {
            Ok(())
        }
    }

    /// 根据 devtmpfs 相对路径恢复其绑定的字符设备对象。
    pub fn char_dev(&self, user_name: &str) -> Option<CharDevice> {
        let inode = self.lookup_node_at(user_name).ok()?;
        let ops = inode.downcast_ops::<DevCharOps>()?;
        let dev = ops.dev();
        dev.is_active().then_some(dev)
    }

    /// 根据 devtmpfs 相对路径恢复其绑定的块设备对象。
    pub fn block_dev(&self, user_name: &str) -> Option<Arc<BlockDevice>> {
        let inode = self.lookup_node_at(user_name).ok()?;
        block_device_from_inode(&inode)
    }
}

impl SuperblockOps for DevTmpfsSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn retain_dentries_without_mounts(&self) -> bool {
        true
    }

    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Ok(FsStat {
            fs_type: 0x444f4445, // "devt" 魔数
            block_size: DEVTMPFS_STANDARD_POLICY.block_size as u64,
            total_blocks: 0,
            free_blocks: 0,
            avail_blocks: 0,
            total_inodes: self.next_ino.load(Ordering::Relaxed),
            free_inodes: 0,
            fs_id: sb.fs_id.raw(),
            name_max: DEVTMPFS_NAME_MAX as u32,
        })
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }

    fn remount(&self, _sb: &Arc<Superblock>, _flags: MountFlags) -> VfsResult<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── FsDriver ─────────

/// devtmpfs 文件系统驱动。
///
/// 通过 `mount` 方法创建超级块，返回的 `Arc<Superblock>` 的
/// `ops` 字段可通过 `downcast_ops::<DevTmpfsSuperblockOps>()` 取回，
/// 供驱动调用 `bind_char` / `bind_block` / `bind_symlink` 或批量节点 API。
///
/// # 典型初始化流程
///
/// ```rust,ignore
/// // 1. 挂载 devtmpfs 到 /dev
/// let sb = FS_REGISTRY.find("devtmpfs").unwrap().mount(None, "")?;
/// mount_ns.mount(&dev_dentry, &dev_mount, sb.clone(), MountFlags::empty())?;
///
/// // 2. 驱动注册后绑定设备
/// let ops = sb.downcast_ops::<DevTmpfsSuperblockOps>().unwrap();
/// ops.bind_char("console", char_dev)?;       // 直接绑定对象引用
/// ops.bind_block("block/root", block_dev)?;  // 目录化块设备节点
/// ops.bind_symlink("disk/root", "../block/root")?; // 可选符号链接投影
/// ```
/// 解析 devtmpfs 挂载选项(与 Linux 同键名)。
///
/// 只消费 `mode=/uid=/gid=`;其余常见选项接受但忽略。
fn parse_devtmpfs_mount_options(data: &str) -> VfsResult<FileMode> {
    let mut mode = DEVTMPFS_STANDARD_POLICY.dir_mode;
    for item in data.split(',').filter(|item| !item.is_empty()) {
        let (key, value) = item.split_once('=').unwrap_or((item, ""));
        match key {
            "mode" => mode = FileMode::new(parse_octal_mount_mode(value)?),
            "uid" | "gid" | "nosuid" | "nodev" | "noexec" | "rw" | "ro" | "defaults" => {}
            _ => return Err(VfsError::InvalidArgument),
        }
    }
    Ok(mode)
}

fn parse_octal_mount_mode(value: &str) -> VfsResult<u16> {
    if value.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    let mut result = 0u16;
    for byte in value.bytes() {
        if !(b'0'..=b'7').contains(&byte) {
            return Err(VfsError::InvalidArgument);
        }
        result = result
            .checked_mul(8)
            .and_then(|value| value.checked_add((byte - b'0') as u16))
            .ok_or(VfsError::InvalidArgument)?;
    }
    Ok(result)
}

pub struct DevTmpfsDriver;

impl FsDriver for DevTmpfsDriver {
    fn name(&self) -> &'static str {
        "devtmpfs"
    }

    fn flags(&self) -> FsDriverFlags {
        FsDriverFlags::NODEV.with(FsDriverFlags::SINGLE)
    }

    fn mount(&self, _dev: Option<&str>, data: &str) -> VfsResult<Arc<Superblock>> {
        // devtmpfs 是内核设备树的用户可见投影，不能像 tmpfs 一样每次 mount
        // 都创建空实例。启动期 PnP bridge 安装后，用户态再次挂载 devtmpfs
        // 应复用同一个 superblock，否则会覆盖掉已经绑定的 console/uart/vd0 等节点。
        if let Some(sb) = mounted_devtmpfs_sb() {
            return Ok(sb);
        }
        let mount_root_mode = parse_devtmpfs_mount_options(data)?;

        let fs_id = FsId::new(DEVTMPFS_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));

        let root_ops = Arc::new(DevDirOps::new());

        // 只构造一个 DevTmpfsSuperblockOps 实例，move 进 new_cyclic 闭包，
        // 写入 weak ref 后再 Box 化存入 Superblock。外层不再持有任何引用，
        // 后续通过 sb.downcast_ops::<DevTmpfsSuperblockOps>() 访问。
        let sb_ops = DevTmpfsSuperblockOps {
            next_ino: AtomicU64::new(2),
            sb: vfs::sync::Spinlock::new(None),
            numbered_nodes: Spinlock::new(Vec::new()),
            implicit_dirs: Spinlock::new(Vec::new()),
        };

        let sb = Superblock::new(move |weak_sb| {
            sb_ops.sb.lock().replace(weak_sb.clone());

            let now = Timespec::now();
            let root_meta = InodeMeta {
                size: 0,
                nlink: 2,
                mode: mount_root_mode,
                uid: DEVTMPFS_STANDARD_POLICY.uid,
                gid: DEVTMPFS_STANDARD_POLICY.gid,
                atime: now,
                mtime: now,
                ctime: now,
                blocks: 0,
            };

            let root_inode = Inode::new(
                InodeId { fs_id, ino: 1 },
                FileType::Directory,
                DevId::new(0, 0),
                DEVTMPFS_STANDARD_POLICY.block_size,
                None,
                root_meta,
                Arc::clone(&root_ops) as Arc<dyn InodeOps + Send + Sync>,
                weak_sb.clone(),
            );

            let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));

            Superblock {
                fs_type: "devtmpfs",
                fs_id,
                dev_id: None,
                block_size: DEVTMPFS_STANDARD_POLICY.block_size,
                name_max: DEVTMPFS_NAME_MAX as u32,
                root_inode,
                root_dentry,
                inode_cache: vfs::superblock::InodeCache::new(),
                ops: Box::new(sb_ops),
                self_weak: weak_sb,
            }
        });

        let ops = sb
            .downcast_ops::<DevTmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
        ops.bind_registered_static_nodes()?;

        let (sb, _) = publish_devtmpfs_sb(sb);
        Ok(sb)
    }

    fn kill_sb(&self, _sb: Arc<Superblock>) {}

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── 虚拟终端节点投影 ─────────

const VT_DEVNODE_OWNER: &'static str = "vt-devnode";

/// tty0 / VT console 节点:open 时解析为当前活动 VT。
///
/// 与 Linux 的 `/dev/tty0` 语义一致:它是活动 VT 的别名,切换后重新打开
/// 即得到新活动 VT。
struct VtZeroInodeOps {
    manager: &'static crate::dev::tty::VtManager,
}

impl InodeOps for VtZeroInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let Some(fg) = self.manager.fg_vt() else {
            return Err(VfsError::NoDevice);
        };
        let Some(dev) = fg.char_device() else {
            return Err(VfsError::NoDevice);
        };
        if !dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        let tty = tty::shared_tty_core(&dev);
        Ok(Box::new(CharDevFileOps::new(dev, opts.nonblock, tty)))
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn vt_zero_node_build(
    spec: &CustomDevNodeSpec,
) -> VfsResult<Option<Arc<dyn InodeOps + Send + Sync>>> {
    // 适配器按注册顺序逐个尝试;不属于自己的 spec 返回 None 交给下一个。
    let payload = spec.payload();
    let Some(manager) = payload.as_ref().downcast_ref::<&'static crate::dev::tty::VtManager>()
    else {
        return Ok(None);
    };
    Ok(Some(Arc::new(VtZeroInodeOps { manager: *manager })))
}

fn bind_vt_zero_node(
    dev_ops: &DevTmpfsSuperblockOps,
    name: &'static str,
    manager: &'static crate::dev::tty::VtManager,
) -> VfsResult<()> {
    register_custom_devnode_adapter(DevTmpfsCustomNodeAdapter::new(
        VT_DEVNODE_OWNER,
        name,
        vt_zero_node_build,
    ))?;
    let payload: Arc<dyn core::any::Any + Send + Sync> = Arc::new(manager);
    let spec = CustomDevNodeSpec::try_new(name, CustomDevNodeKind::CharDevice, payload)?;
    dev_ops.bind_custom(&spec)
}

/// `/dev/ptmx` 节点:open 时分配 pty 对并返回 master。
pub fn register_pty_devnode() -> VfsResult<()> {
    register_custom_devnode_adapter(DevTmpfsCustomNodeAdapter::new(
        "ptmx-devnode",
        "ptmx",
        ptmx_node_build,
    ))?;
    register_static_dev_node(DevTmpfsStaticNode::new(
        "ptmx-devnode",
        "ptmx",
        build_ptmx_node,
    ))?;
    Ok(())
}

fn build_ptmx_node() -> VfsResult<DevNodeSpec> {
    let payload: Arc<dyn core::any::Any + Send + Sync> = Arc::new(());
    Ok(DevNodeSpec::custom(CustomDevNodeSpec::try_new(
        "ptmx",
        CustomDevNodeKind::CharDevice,
        payload,
    )?))
}

fn map_pty_open_err(err: Errno) -> VfsError {
    match err {
        Errno::ENOMEM => VfsError::OutOfMemory,
        Errno::EAGAIN => VfsError::NoSpace,
        _ => VfsError::NoDevice,
    }
}

fn ptmx_node_build(
    spec: &CustomDevNodeSpec,
) -> VfsResult<Option<Arc<dyn InodeOps + Send + Sync>>> {
    if spec.name() != "ptmx" {
        return Ok(None);
    }
    Ok(Some(Arc::new(PtyMasterInodeOps)))
}

struct PtyMasterInodeOps;

impl InodeOps for PtyMasterInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        crate::dev::tty::pty::open_ptmx(opts.nonblock).map_err(map_pty_open_err)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// 安装虚拟终端节点投影:tty0(活动 VT 别名)+ tty1..tty7。
///
/// `bind_console` 为 true 时(`console=ttyN`)把 `/dev/console` 重绑为
/// 活动 VT 别名;否则 console 保持指向物理串口。节点创建与 VT 管理器
/// 安装分离,便于启动期按 console= 参数决定输入路由与 console 绑定。
pub fn install_virtual_terminal_nodes(
    dev_ops: &DevTmpfsSuperblockOps,
    manager: &'static crate::dev::tty::VtManager,
    bind_console: bool,
) -> VfsResult<()> {
    bind_vt_zero_node(dev_ops, "tty0", manager)?;
    for index in 1..crate::dev::tty::vt::VT_COUNT {
        let Some(vt) = manager.vt(index as u8) else {
            continue;
        };
        let Some(dev) = vt.char_device() else {
            continue;
        };
        dev_ops.bind_char(&vt.name(), dev)?;
    }
    if bind_console {
        let _ = dev_ops.unbind("console");
        bind_vt_zero_node(dev_ops, "console", manager)?;
    }
    Ok(())
}

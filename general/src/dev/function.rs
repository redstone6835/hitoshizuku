//! 开放设备 function 注册表。
//!
//! 一个物理 PnP 设备可以暴露一个或多个 function。字符设备和块设备只是当前
//! VFS 兼容层已经支持的两种 function；PnP core 只保存 `DeviceFunction`
//! trait object，不关心 function 最终会不会在 `/dev` 下形成节点。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use spin::mutex::Mutex;
use vfs::cred::{Gid, Uid};
use vfs::inode::InodeOps;
use vfs::stat::{DevId, FileMode, FileType};

use crate::dev::block::BlockDevice;
use crate::dev::char::CharDevice;

/// function 的内部类别标识。
///
/// 该标识只用于 registry 唯一性和按类查询，不代表 PnP core 理解字符/块设备语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceClassId(&'static str);

impl DeviceClassId {
    pub const CHAR: Self = Self("char");
    pub const BLOCK: Self = Self("block");
    pub const RTC: Self = Self("rtc");

    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// 自定义 devtmpfs 节点规格。
///
/// 新设备类型如果需要在 `/dev` 暴露特殊节点，不应修改 devtmpfs 的核心 match
/// 逻辑，而应构造一个携带自身 [`InodeOps`] 的自定义节点。devtmpfs 只负责路径、
/// inode 号、元数据和目录树管理，具体 open/ioctl/read/write 语义由该 ops 决定。
#[derive(Clone)]
pub struct CustomDevNodeSpec {
    name: Box<str>,
    kind: FileType,
    rdev: DevId,
    block_size: u32,
    mode: FileMode,
    uid: Uid,
    gid: Gid,
    size: u64,
    blocks: u64,
    nlink: u32,
    ops: Arc<dyn InodeOps + Send + Sync>,
}

impl CustomDevNodeSpec {
    pub fn new(name: &str, kind: FileType, ops: Arc<dyn InodeOps + Send + Sync>) -> Self {
        Self {
            name: name.into(),
            kind,
            rdev: DevId::new(0, 0),
            block_size: 512,
            mode: FileMode::new(0o660),
            uid: Uid::ROOT,
            gid: Gid::ROOT,
            size: 0,
            blocks: 0,
            nlink: 1,
            ops,
        }
    }

    pub fn with_rdev(mut self, rdev: DevId) -> Self {
        self.rdev = rdev;
        self
    }

    pub fn with_block_size(mut self, block_size: u32) -> Self {
        self.block_size = block_size;
        self
    }

    pub fn with_mode(mut self, mode: FileMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_owner(mut self, uid: Uid, gid: Gid) -> Self {
        self.uid = uid;
        self.gid = gid;
        self
    }

    pub fn with_size(mut self, size: u64, blocks: u64) -> Self {
        self.size = size;
        self.blocks = blocks;
        self
    }

    pub fn with_nlink(mut self, nlink: u32) -> Self {
        self.nlink = nlink;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> FileType {
        self.kind
    }

    pub fn rdev(&self) -> DevId {
        self.rdev
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn mode(&self) -> FileMode {
        self.mode
    }

    pub fn uid(&self) -> Uid {
        self.uid
    }

    pub fn gid(&self) -> Gid {
        self.gid
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    pub fn nlink(&self) -> u32 {
        self.nlink
    }

    pub fn ops(&self) -> Arc<dyn InodeOps + Send + Sync> {
        Arc::clone(&self.ops)
    }
}

/// devtmpfs 需要创建的兼容层设备节点。
///
/// 枚举变体直接携带 VFS 打开节点时需要的对象，因此 devtmpfs 不需要再把
/// `DeviceFunction` downcast 回 `CharFunction` 或 `BlockFunction`。
/// 这里描述的是 POSIX/VFS 命名空间里的投影，不是底层硬件 identity；底层身份
/// 仍由 PnP id、function `class_id + dev_name` 和具体 typed device object 表达。
///
/// 标记为 `#[non_exhaustive]` 以便未来扩展新 VFS 节点类型时保持 API 兼容；
/// 新设备类别如果没有 `/dev` 节点（网络等）应通过 `devnode() → None` 表达；
/// 如果确实需要 `/dev` 投影，优先使用 [`DevNodeSpec::Custom`] 携带自己的
/// [`InodeOps`]，而不是继续向 devtmpfs core 增加设备类型分支。
#[non_exhaustive]
#[derive(Clone)]
pub enum DevNodeSpec {
    Char {
        name: Box<str>,
        dev: CharDevice,
    },
    Block {
        name: Box<str>,
        dev: Arc<BlockDevice>,
    },
    Symlink {
        name: Box<str>,
        target: Box<str>,
    },
    Custom(CustomDevNodeSpec),
}

impl DevNodeSpec {
    pub fn name(&self) -> &str {
        match self {
            Self::Char { name, .. } | Self::Block { name, .. } | Self::Symlink { name, .. } => name,
            Self::Custom(spec) => spec.name(),
        }
    }

    pub fn custom(spec: CustomDevNodeSpec) -> Self {
        Self::Custom(spec)
    }
}

/// 一个 function 在 devtmpfs 中需要投影出的节点集合。
///
/// 旧接口里 `function 名称 == /dev 节点名 == 解绑键`，这会把设备身份和 VFS
/// 名字空间耦合在一起。节点集合把这三件事拆开：function 仍由 `class_id +
/// dev_name` 唯一标识，devtmpfs 只消费这里声明的路径投影。
#[derive(Clone)]
pub struct DevNodeSet {
    nodes: Vec<DevNodeSpec>,
}

impl DevNodeSet {
    pub fn single(node: DevNodeSpec) -> Self {
        let mut nodes = Vec::new();
        nodes.push(node);
        Self { nodes }
    }

    pub fn new(nodes: Vec<DevNodeSpec>) -> Option<Self> {
        if nodes.is_empty() {
            None
        } else {
            Some(Self { nodes })
        }
    }

    pub fn nodes(&self) -> &[DevNodeSpec] {
        &self.nodes
    }
}

pub trait DeviceFunction: Send + Sync {
    /// function 类别，同一类别下 `dev_name` 必须唯一。
    fn class_id(&self) -> DeviceClassId;
    /// function registry key 的名称；`/dev` 节点由 [`DevNodeSpec`]/[`DevNodeSet`] 决定。
    fn dev_name(&self) -> &str;
    /// 标记 function 已不可用，使旧句柄尽快停止访问底层设备。
    fn mark_gone(&self);
    /// 等待正在进行的 I/O 排空；没有异步 I/O 的 function 可以使用默认空实现。
    fn drain_io(&self) {}
    /// 返回该 function 需要暴露到 devtmpfs 的节点规格。
    fn devnode(&self) -> Option<DevNodeSpec> {
        None
    }
    /// 返回该 function 需要暴露到 devtmpfs 的节点集合。
    ///
    /// 默认把旧的单节点接口提升为集合，已有驱动无需立即迁移；需要多个路径、别名
    /// 或符号链接的驱动可以只覆盖此方法。
    fn devnodes(&self) -> Option<DevNodeSet> {
        self.devnode().map(DevNodeSet::single)
    }
    /// 向下转型支持。新设备类型通过此方法提供类型恢复路径。
    fn as_any(&self) -> &dyn core::any::Any;
}

/// 字符设备 function。
///
/// 它把 [`CharDevice`] 包装成通用 [`DeviceFunction`]，供 PnP core 和 devtmpfs
/// 统一处理。
pub struct CharFunction {
    dev_name: Box<str>,
    devnode_name: Box<str>,
    dev: CharDevice,
}

impl CharFunction {
    /// 创建一个字符设备 function。
    pub fn new(dev_name: &str, dev: CharDevice) -> Self {
        Self::with_devnode(dev_name, dev_name, dev)
    }

    /// 创建一个字符设备 function，并显式指定 `/dev` 投影名。
    ///
    /// `dev_name` 是设备 registry key，`devnode_name` 是 POSIX/VFS 兼容层路径名；
    /// 两者允许相同，但底层设备抽象不再强制绑定二者。
    pub fn with_devnode(dev_name: &str, devnode_name: &str, dev: CharDevice) -> Self {
        Self {
            dev_name: dev_name.into(),
            devnode_name: devnode_name.into(),
            dev,
        }
    }

    /// 返回内部字符设备句柄。
    pub fn dev(&self) -> CharDevice {
        self.dev.clone()
    }
}

impl DeviceFunction for CharFunction {
    fn class_id(&self) -> DeviceClassId {
        DeviceClassId::CHAR
    }

    fn dev_name(&self) -> &str {
        &self.dev_name
    }

    fn mark_gone(&self) {
        self.dev.mark_gone();
    }

    fn devnode(&self) -> Option<DevNodeSpec> {
        Some(DevNodeSpec::Char {
            name: self.devnode_name.clone(),
            dev: self.dev(),
        })
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// 块设备 function。
pub struct BlockFunction {
    dev_name: Box<str>,
    devnode_name: Box<str>,
    dev: Arc<BlockDevice>,
}

impl BlockFunction {
    /// 创建一个块设备 function。
    pub fn new(dev_name: &str, dev: Arc<BlockDevice>) -> Self {
        Self::with_devnode(dev_name, dev_name, dev)
    }

    /// 创建一个块设备 function，并显式指定 `/dev` 投影名。
    ///
    /// `dev_name` 是 PnP/function registry key，`devnode_name` 只用于 devtmpfs
    /// 暴露 POSIX 块设备节点，避免把底层设备身份和 `/dev` 命名策略混在一起。
    pub fn with_devnode(dev_name: &str, devnode_name: &str, dev: Arc<BlockDevice>) -> Self {
        Self {
            dev_name: dev_name.into(),
            devnode_name: devnode_name.into(),
            dev,
        }
    }

    /// 返回内部块设备对象。
    pub fn dev(&self) -> Arc<BlockDevice> {
        Arc::clone(&self.dev)
    }
}

impl DeviceFunction for BlockFunction {
    fn class_id(&self) -> DeviceClassId {
        DeviceClassId::BLOCK
    }

    fn dev_name(&self) -> &str {
        &self.dev_name
    }

    fn mark_gone(&self) {
        self.dev.mark_gone();
    }

    fn drain_io(&self) {
        self.dev.drain();
    }

    fn devnode(&self) -> Option<DevNodeSpec> {
        Some(DevNodeSpec::Block {
            name: self.devnode_name.clone(),
            dev: self.dev(),
        })
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// 通用类型化查询。
///
/// 把 `&dyn DeviceFunction` 向下转型为具体的 function 类型。返回 `None`
/// 表示类型不匹配。
///
/// # 示例
///
/// ```rust,ignore
/// // 网络设备：function_as::<NetFunction>(func).map(|nf| nf.dev())
/// // 块设备：  function_as::<BlockFunction>(func).map(|bf| bf.dev())
/// ```
///
/// 这是为新设备类型提供的"开闭原则"路径——新增设备类型不需要改任何 core
/// 文件，只需实现 `DeviceFunction::as_any` 后即可被外部代码 downcast 取出。
pub fn function_as<T: 'static>(func: &dyn DeviceFunction) -> Option<&T> {
    func.as_any().downcast_ref::<T>()
}

pub fn char_device_from_function(func: &dyn DeviceFunction) -> Option<CharDevice> {
    func.devnodes()?.nodes().iter().find_map(|node| match node {
        DevNodeSpec::Char { dev, .. } => Some(dev.clone()),
        DevNodeSpec::Block { .. } | DevNodeSpec::Symlink { .. } | DevNodeSpec::Custom(_) => None,
    })
}

pub fn block_device_from_function(func: &dyn DeviceFunction) -> Option<Arc<BlockDevice>> {
    func.devnodes()?.nodes().iter().find_map(|node| match node {
        DevNodeSpec::Block { dev, .. } => Some(Arc::clone(dev)),
        DevNodeSpec::Char { .. } | DevNodeSpec::Symlink { .. } | DevNodeSpec::Custom(_) => None,
    })
}

/// 列出当前仍处于 active 状态的字符设备节点。
///
/// 这是 VFS/procfs/sysfs 兼容层使用的查询 helper。调用方不需要知道 function
/// 的具体 Rust 类型，只需要消费返回的 `CharDevice`。
pub fn active_char_devices(functions: &FunctionRegistry) -> Vec<CharDevice> {
    functions
        .list()
        .into_iter()
        .filter_map(|func| char_device_from_function(func.as_ref()))
        .filter(CharDevice::is_active)
        .collect()
}

/// 列出当前仍处于 active 状态的块设备节点。
pub fn active_block_devices(functions: &FunctionRegistry) -> Vec<Arc<BlockDevice>> {
    functions
        .list()
        .into_iter()
        .filter_map(|func| block_device_from_function(func.as_ref()))
        .filter(|dev| dev.is_active())
        .collect()
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
    functions
        .list()
        .into_iter()
        .filter_map(|func| {
            func.devnodes()?.nodes().iter().find_map(|node| match node {
                DevNodeSpec::Block { name, dev } if name.as_ref() == dev_name => {
                    Some(Arc::clone(dev))
                }
                _ => None,
            })
        })
        .find(|dev| dev.is_active())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionRegistryError {
    /// 同类别同名 function 已存在。
    NameExists,
    /// 扩容或分配失败。
    OutOfMemory,
}

/// 全局开放设备 function 注册表。
///
/// 注册表只保存 `Arc<dyn DeviceFunction>`，不持有具体字符/块类型。兼容层如
/// procfs/sysfs/devtmpfs 通过 helper 函数读取 `DevNodeSpec`。
pub struct FunctionRegistry {
    functions: Mutex<Vec<Arc<dyn DeviceFunction>>>,
}

impl FunctionRegistry {
    /// 创建空注册表。
    pub const fn new() -> Self {
        Self {
            functions: Mutex::new(Vec::new()),
        }
    }

    /// 插入一个 function。
    ///
    /// 同一 `class_id + dev_name` 组合必须唯一。
    pub fn push(&self, func: Arc<dyn DeviceFunction>) -> Result<(), FunctionRegistryError> {
        {
            let mut list = self.functions.lock();
            if list.iter().any(|existing| {
                existing.class_id() == func.class_id() && existing.dev_name() == func.dev_name()
            }) {
                return Err(FunctionRegistryError::NameExists);
            }
            if list.len() < list.capacity() {
                list.push(func);
                return Ok(());
            }
        }

        loop {
            let initial_len = self.functions.lock().len();
            let needed = initial_len
                .checked_add(1)
                .ok_or(FunctionRegistryError::OutOfMemory)?;
            let mut replacement = Vec::new();
            replacement
                .try_reserve(needed)
                .map_err(|_| FunctionRegistryError::OutOfMemory)?;

            let mut list = self.functions.lock();
            if list.iter().any(|existing| {
                existing.class_id() == func.class_id() && existing.dev_name() == func.dev_name()
            }) {
                return Err(FunctionRegistryError::NameExists);
            }
            if list.len() < list.capacity() {
                list.push(func);
                return Ok(());
            }
            let needed = list
                .len()
                .checked_add(1)
                .ok_or(FunctionRegistryError::OutOfMemory)?;
            if needed > replacement.capacity() {
                continue;
            }
            replacement.extend(list.iter().cloned());
            replacement.push(Arc::clone(&func));
            let old = core::mem::replace(&mut *list, replacement);
            drop(list);
            drop(old);
            return Ok(());
        }
    }

    /// 删除指定类别和名称的 function。
    pub fn remove(
        &self,
        class_id: DeviceClassId,
        dev_name: &str,
    ) -> Option<Arc<dyn DeviceFunction>> {
        let func = {
            let mut list = self.functions.lock();
            let pos = list
                .iter()
                .position(|func| func.class_id() == class_id && func.dev_name() == dev_name)?;
            list.swap_remove(pos)
        };
        func.mark_gone();
        func.drain_io();
        Some(func)
    }

    /// 查找指定类别和名称的 function。
    pub fn lookup(
        &self,
        class_id: DeviceClassId,
        dev_name: &str,
    ) -> Option<Arc<dyn DeviceFunction>> {
        self.functions
            .lock()
            .iter()
            .find(|func| func.class_id() == class_id && func.dev_name() == dev_name)
            .cloned()
    }

    /// 返回所有 function 的快照。
    pub fn list(&self) -> Vec<Arc<dyn DeviceFunction>> {
        self.functions.lock().iter().cloned().collect()
    }

    /// 返回指定类别 function 的快照。
    pub fn list_by_class(&self, class_id: DeviceClassId) -> Vec<Arc<dyn DeviceFunction>> {
        self.functions
            .lock()
            .iter()
            .filter(|func| func.class_id() == class_id)
            .cloned()
            .collect()
    }
}

impl Default for FunctionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

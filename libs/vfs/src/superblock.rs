//! 超级块（Superblock）：文件系统实例的顶层元数据容器。
//!
//! 每次将一个文件系统（或设备）挂载到挂载点时，VFS 层会要求对应的 FS 驱动
//! 创建一个 [`Superblock`] 实例，代表该挂载的文件系统。超级块：
//!
//! - 持有文件系统级别的元数据（块大小、最大文件名长度等）；
//! - 跟踪所有在内存中存活的 Inode（通过弱引用），用于同步和回收；
//! - 提供文件系统级别的操作（`sync_fs`、`statfs`、`remount` 等）；
//! - 是 [`crate::vfs::mount::Mount`] 的核心组成部分。
//!
//! ### 超级块与挂载标志的分离
//!
//! 挂载标志（RDONLY、NOSUID 等）描述的是"这次挂载"的属性，而不是文件系统本身
//! 的属性——同一个文件系统镜像可以在不同命名空间中以不同标志挂载（如一处只读、
//! 另一处读写）。因此，挂载标志属于 [`crate::vfs::mount::Mount`]，而不是
//! `Superblock`。写操作前检查只读限制时，应读取 `Mount::flags`。
//!
//! ### 文件系统驱动注册
//!
//! 每种文件系统（ext4、tmpfs、procfs 等）通过实现 [`FsDriver`] trait 注册到
//! 全局 [`FsRegistry`] 中。挂载时，VFS 层按名称（如 `"ext4"`）查找对应驱动，
//! 调用 `mount` 方法创建 Superblock。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::inode::Inode;
use crate::vfs::mount::MountFlags;
use crate::vfs::stat::{DevId, FsId, FsStat};
use crate::vfs::sync::Spinlock;

// ── 分片 Inode 缓存 ─────────────────────────────────────────────────────────

/// Inode 缓存分片数量。
const INODE_CACHE_SHARDS: usize = 8;
const INODE_CACHE_SHARD_MASK: usize = INODE_CACHE_SHARDS - 1;

/// 分片 Inode 缓存，参照 [`crate::vfs::dentry::DentryCache`] 的分片设计。
///
/// 8 个独立分片各有自己的 `Spinlock`，按 `ino % 8` 选片，降低多核并发
/// 访问同一文件系统时的锁竞争。
pub struct InodeCache {
    shards: [Spinlock<BTreeMap<u64, Weak<Inode>>>; INODE_CACHE_SHARDS],
}

impl InodeCache {
    /// 构造空缓存。
    pub const fn new() -> Self {
        Self {
            shards: [
                Spinlock::new(BTreeMap::new()),
                Spinlock::new(BTreeMap::new()),
                Spinlock::new(BTreeMap::new()),
                Spinlock::new(BTreeMap::new()),
                Spinlock::new(BTreeMap::new()),
                Spinlock::new(BTreeMap::new()),
                Spinlock::new(BTreeMap::new()),
                Spinlock::new(BTreeMap::new()),
            ],
        }
    }

    #[inline]
    fn shard(&self, ino: u64) -> &Spinlock<BTreeMap<u64, Weak<Inode>>> {
        &self.shards[(ino as usize) & INODE_CACHE_SHARD_MASK]
    }

    /// 查找给定 ino 对应的 Inode。
    pub fn find(&self, ino: u64) -> Option<Arc<Inode>> {
        let mut cache = self.shard(ino).lock();
        if let Some(weak) = cache.get(&ino) {
            if let Some(arc) = weak.upgrade() {
                return Some(arc);
            }
            cache.remove(&ino);
        }
        None
    }

    /// 插入 Inode。若同 ino 已有存活实例，返回已有实例。
    pub fn insert(&self, inode: Arc<Inode>) -> Arc<Inode> {
        use alloc::collections::btree_map::Entry;
        let ino = inode.id.ino;
        let mut cache = self.shard(ino).lock();
        match cache.entry(ino) {
            Entry::Occupied(mut e) => {
                if let Some(existing) = e.get().upgrade() {
                    return existing;
                }
                *e.get_mut() = Arc::downgrade(&inode);
                inode
            }
            Entry::Vacant(e) => {
                e.insert(Arc::downgrade(&inode));
                inode
            }
        }
    }

    /// 移除指定 ino 的条目。
    pub fn remove(&self, ino: u64) {
        self.shard(ino).lock().remove(&ino);
    }

    /// 清理所有分片中的失效弱引用。
    pub fn gc(&self) {
        for shard in &self.shards {
            shard.lock().retain(|_, weak| weak.strong_count() > 0);
        }
    }
}

impl Default for InodeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// 超级块，代表一个已挂载的文件系统实例。
pub struct Superblock {
    /// 文件系统类型名称（如 `"ext4"`、`"tmpfs"`），只读。
    pub fs_type: &'static str,

    /// 文件系统实例标识符，挂载时由 VFS 层分配，用于跨 FS 操作的同 FS 检查。
    ///
    /// 对块设备文件系统，通常将 `DevId` 编码为 `u64`；对内存文件系统，
    /// 由内核分配单调递增的唯一 ID。
    pub fs_id: FsId,

    /// 底层块设备的设备号，仅对块设备文件系统有意义（对应 `stat(2)` 的 `st_dev`）。
    ///
    /// 内存文件系统（tmpfs、procfs 等）此字段为 `None`；`stat()` 遇到 `None`
    /// 时回退到以 `fs_id.raw()` 为 `st_dev`（与 Linux 行为一致）。
    pub dev_id: Option<DevId>,

    /// 文件系统的基本块大小（字节）。所有块 I/O 操作以此为单位。
    pub block_size: u32,

    /// 文件名最大字节数（不含 NUL 终止符，对应 POSIX `NAME_MAX`）。
    pub name_max: u32,

    /// 文件系统根目录的 Inode，在 `FsDriver::mount` 时创建并填入。
    pub root_inode: Arc<Inode>,

    /// 文件系统根目录的 Dentry，与 `root_inode` 对应。
    ///
    /// `MountNamespace::mount()` 在建立挂载点时需要此 Dentry 作为被挂载文件系统的
    /// `mount_root`，以供路径解析进入该挂载后继续向下遍历。
    pub root_dentry: Arc<crate::vfs::dentry::Dentry>,

    /// 已加载到内存的 Inode 弱引用表（8 分片），以 `InodeId.ino` 为键。
    ///
    /// 弱引用确保 Superblock 不会阻止 Inode 的正常回收；当外部的
    /// `Arc<Inode>` 全部 drop 后，弱引用自动失效，下次 GC 时被清除。
    ///
    /// 分片设计参照 [`crate::vfs::dentry::DentryCache`]：8 个独立分片各有自己的
    /// `Spinlock`，按 `ino % 8` 选片，降低多核并发访问同一文件系统时的锁竞争。
    pub inode_cache: InodeCache,

    /// 文件系统特定操作的实现，由 `FsDriver::mount` 注入。
    pub ops: Box<dyn SuperblockOps + Send + Sync>,

    /// 指向自身的弱引用。
    ///
    /// 使 `Superblock` 的方法（如 `statfs`、`sync`、`remount`）能在不持有
    /// 外部 `Arc<Superblock>` 的情况下，将 `&Arc<Self>` 传给 `SuperblockOps`
    /// 的各回调方法。由 [`Superblock::new`] 通过 `Arc::new_cyclic` 在构造时填入。
    pub self_weak: Weak<Self>,
}

impl Superblock {
    /// 构造一个新的 `Superblock`，同时填入指向自身的弱引用。
    ///
    /// 必须通过此函数构造，以确保 `self_weak` 字段被正确初始化。
    /// 使用 `Arc::new_cyclic` 保证弱引用在 `Arc` 完全分配之前就能获取到。
    ///
    /// # 参数
    ///
    /// `init` 接收一个 `Weak<Superblock>`（尚未升级），返回完整填充的 `Superblock`
    /// 值（除 `self_weak` 外的所有字段）。`self_weak` 由此函数自动填入。
    pub fn new<F>(init: F) -> Arc<Self>
    where
        F: FnOnce(Weak<Self>) -> Self,
    {
        Arc::new_cyclic(|weak| {
            let mut sb = init(weak.clone());
            sb.self_weak = weak.clone();
            sb
        })
    }

    /// 从 Inode 缓存中查找给定 ino 编号对应的 Inode。
    ///
    /// 缓存命中时直接返回，避免重复创建 Inode 对象（保证同一 ino 在内存中
    /// 只存在一个 `Arc<Inode>` 实例，确保元数据的一致性视图）。
    pub fn find_inode(&self, ino: u64) -> Option<Arc<Inode>> {
        self.inode_cache.find(ino)
    }

    /// 将新创建的 Inode 插入缓存。若同 ino 已有存活实例，返回已有实例并丢弃
    /// 传入的新实例（确保唯一性）。
    pub fn insert_inode(&self, inode: Arc<Inode>) -> Arc<Inode> {
        self.inode_cache.insert(inode)
    }

    /// 从缓存中移除指定 ino 的条目（Inode 被 evict 时调用）。
    pub fn remove_inode(&self, ino: u64) {
        self.inode_cache.remove(ino);
    }

    /// 返回当前超级块的统计信息（`statfs`）。
    ///
    /// 通过 `self_weak` 升级得到 `Arc<Superblock>`，传给 `SuperblockOps::statfs`。
    pub fn statfs(&self) -> VfsResult<FsStat> {
        let arc = self.self_weak.upgrade().ok_or(VfsError::InvalidArgument)?;
        self.ops.statfs(&arc)
    }

    /// 将所有脏 Inode 的元数据和数据刷入底层存储（`sync(2)` 或 `syncfs(2)`）。
    ///
    /// 通过 `self_weak` 升级得到 `Arc<Superblock>`，传给 `SuperblockOps::sync_fs`。
    pub fn sync(&self) -> VfsResult<()> {
        let arc = self.self_weak.upgrade().ok_or(VfsError::InvalidArgument)?;
        self.ops.sync_fs(&arc)
    }

    /// 以新标志重新挂载文件系统（`MS_REMOUNT`），如只读→读写升级或反向降级。
    ///
    /// 注意：挂载标志存储在 [`crate::vfs::mount::Mount::flags`] 中，`remount`
    /// 需要同时更新那里；此处的调用由 VFS 层在更新 Mount 标志前/后协调。
    ///
    /// 通过 `self_weak` 升级得到 `Arc<Superblock>`，传给 `SuperblockOps::remount`。
    pub fn remount(&self, new_flags: MountFlags) -> VfsResult<()> {
        let arc = self.self_weak.upgrade().ok_or(VfsError::InvalidArgument)?;
        self.ops.remount(&arc, new_flags)
    }

    /// 清理所有分片中的失效弱引用（GC），释放 BTreeMap 占用的内存。
    pub fn gc_inode_cache(&self) {
        self.inode_cache.gc();
    }

    /// 将超级块操作对象向下转型为具体 FS 驱动类型 `T`。
    ///
    /// 镜像 [`crate::vfs::inode::Inode::downcast_ops`] 和
    /// [`crate::dev::char::CharDev::downcast_driver`] 的语义：
    ///
    /// ```rust,ignore
    /// if let Some(tmpfs) = sb.downcast_ops::<TmpfsSuperblockOps>() {
    ///     tmpfs.set_size_limit(64 * 1024 * 1024);
    /// }
    /// ```
    pub fn downcast_ops<T: SuperblockOps + 'static>(&self) -> Option<&T> {
        self.ops.as_any().downcast_ref::<T>()
    }
}

/// 文件系统特定的超级块操作接口。
pub trait SuperblockOps {
    /// 分配一个新的 Inode 编号并在磁盘（或内存）上预留空间。
    ///
    /// 返回的 Inode 应当已加入超级块的 inode_cache，但尚未插入任何目录。
    /// 调用方（`InodeOps::create` 等）负责将其链接到目录后设置 `nlink ≥ 1`。
    fn alloc_inode(&self, sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>>;

    /// 将 Inode 的脏数据（内容块）和元数据写回底层存储。
    ///
    /// 由 `fsync`、`sync_fs` 等路径调用。对内存文件系统（tmpfs 等），此操作
    /// 为空，因为"存储"就是内存本身。
    fn write_inode(&self, inode: &Arc<Inode>) -> VfsResult<()>;

    /// 返回 `&dyn Any`，用于向下转型到具体 FS 驱动的超级块操作类型。
    ///
    /// 实现者只需写 `fn as_any(&self) -> &dyn Any { self }`。
    fn as_any(&self) -> &dyn core::any::Any;

    /// 返回文件系统全局统计信息（块使用量、inode 数量等）。
    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat>;

    /// 将超级块自身的元数据（如块位图、inode 位图）刷盘。
    fn sync_fs(&self, sb: &Arc<Superblock>) -> VfsResult<()>;

    /// 以新标志重新挂载，更新超级块的只读/读写状态等属性。
    fn remount(&self, sb: &Arc<Superblock>, new_flags: MountFlags) -> VfsResult<()>;
}

// ── 文件系统驱动注册与查找 ───────────────────────────────────────────────────

/// 文件系统驱动接口。
///
/// 每种文件系统实现此 trait，并通过 [`FsRegistry::register`] 注册到内核。
/// 挂载时，VFS 层通过 `FsRegistry::find` 按名称定位驱动，调用 `mount` 创建
/// `Superblock`。
pub trait FsDriver: Send + Sync {
    /// 文件系统类型名称（如 `"ext4"`），用于 `mount -t <name>` 匹配。
    fn name(&self) -> &'static str;

    /// 文件系统的能力标志。
    fn flags(&self) -> FsDriverFlags;

    /// 探测挂载源是否属于本文件系统。
    ///
    /// 只有带 [`FsDriverFlags::AUTO_DETECT`] 的驱动需要覆盖该方法；显式
    /// `mount -t <name>` 仍直接调用 [`FsDriver::mount`]。
    fn probe(&self, _dev: Option<&str>) -> FsProbe {
        FsProbe::None
    }

    /// 挂载文件系统，创建并返回 Superblock。
    ///
    /// - `dev`：块设备路径（网络文件系统或内存文件系统传 `None`）；
    /// - `data`：文件系统特定的挂载选项字符串（如 ext4 的 `"errors=remount-ro"`）。
    ///
    /// 驱动**不**接收 `mount_flags`，挂载标志完全由 VFS 层（`MountNamespace::mount`）
    /// 管理并存入 `Mount::flags`，驱动只负责创建 `Superblock` 本身。
    fn mount(&self, dev: Option<&str>, data: &str) -> VfsResult<Arc<Superblock>>;

    /// 强制卸载：释放所有缓存，写回脏数据，使超级块进入无效状态。
    ///
    /// VFS 层保证在调用此方法前，该文件系统上已无打开的文件和待处理的路径引用。
    fn kill_sb(&self, sb: Arc<Superblock>);

    /// 返回 `&dyn Any`，用于向下转型到具体驱动类型。
    ///
    /// 实现者只需写 `fn as_any(&self) -> &dyn Any { self }`。
    fn as_any(&self) -> &dyn core::any::Any;
}

/// 文件系统驱动能力标志，描述文件系统的固有特性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FsDriverFlags(pub u32);

impl FsDriverFlags {
    /// 文件系统不需要块设备（如 tmpfs、procfs、sysfs）。
    pub const NODEV: Self = Self(1 << 0);
    /// 文件系统不支持写操作（如 iso9660 只读镜像）。
    pub const RDONLY: Self = Self(1 << 1);
    /// 同一类型文件系统只能挂载一次（如 sysfs、procfs）。
    pub const SINGLE: Self = Self(1 << 2);
    /// 文件系统挂载源是块设备。
    pub const BLOCK: Self = Self(1 << 3);
    /// 允许在未指定文件系统类型时参与自动探测。
    pub const AUTO_DETECT: Self = Self(1 << 4);

    pub const fn has(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
    pub const fn with(self, flag: Self) -> Self {
        Self(self.0 | flag.0)
    }
    pub const fn without(self, flag: Self) -> Self {
        Self(self.0 & !flag.0)
    }
    pub const fn raw(self) -> u32 {
        self.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// 文件系统自动探测结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FsProbe {
    /// 明确不是该文件系统。
    None,
    /// 可能匹配，但证据不足，应在强匹配驱动之后尝试。
    Weak,
    /// 明确匹配，可以优先尝试挂载。
    Strong,
}

// ── 无锁文件系统驱动注册表 ──────────────────────────────────────────────────────

/// 内核支持的文件系统驱动最大数量。
///
/// 此上限是有意为之：该注册表专门用于启动期注册的文件系统驱动（ext4、tmpfs、
/// procfs 等），数量有限。无锁位图是注册机制的核心；单个 `AtomicU32` 组成
/// 32 位位图，决定上界。
pub const MAX_FS_DRIVERS: usize = 32;

const ZERO_BUCKET: AtomicU32 = AtomicU32::new(0);

// ── 名称哈希索引 ────────────────────────────────────────────────────────────────

/// 哈希表容量，必须为 2 的幂且 ≥ 2×MAX_FS_DRIVERS（50% 最大负载因子）。
const FS_HASH_SIZE: usize = 64;

/// FNV-1a 哈希（64 位）。无依赖，纯计算，适用于短字符串。
#[inline]
fn fnv1a(bytes: &[u8]) -> usize {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h as usize
}

/// 文件系统名称 → `FsRegistry` 槽位索引的无锁开放寻址哈希表。
///
/// # 并发模型
///
/// 与 [`crate::dev::char::DtbPathIndex`] 完全对称：
/// - 桶存储：`0` 为空哨兵；`slot_idx + 1` 为有效条目。
/// - 插入：CAS(0 → slot_idx+1, Release)。
/// - 查找：Acquire 读桶 → 直接读 `slots[idx]` → 比对 `name()`。
struct FsNameIndex {
    buckets: [AtomicU32; FS_HASH_SIZE],
}

impl FsNameIndex {
    const fn new() -> Self {
        Self {
            buckets: [ZERO_BUCKET; FS_HASH_SIZE],
        }
    }

    /// 将 `name → slot_idx` 插入哈希表。
    fn insert(&self, name: &str, slot_idx: usize) {
        let start = fnv1a(name.as_bytes()) & (FS_HASH_SIZE - 1);
        for probe in 0..FS_HASH_SIZE {
            let b = (start + probe) & (FS_HASH_SIZE - 1);
            if self.buckets[b]
                .compare_exchange(0, slot_idx as u32 + 1, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
        // 理论上不会到达：FS_HASH_SIZE(64) 是 MAX_FS_DRIVERS(32) 的两倍
    }

    /// 查找 `name` 对应的槽位索引。
    fn find(&self, name: &str, slots: *const Option<FsDriverEntry>) -> Option<FsDriverEntry> {
        let start = fnv1a(name.as_bytes()) & (FS_HASH_SIZE - 1);
        for probe in 0..FS_HASH_SIZE {
            let b = (start + probe) & (FS_HASH_SIZE - 1);
            let val = self.buckets[b].load(Ordering::Acquire);
            if val == 0 {
                return None;
            }
            let slot_idx = (val - 1) as usize;
            // Safety: val != 0 → 桶已由 Release CAS 发布，slots[slot_idx] 写入可见。
            let opt_entry = unsafe { core::ptr::read(slots.add(slot_idx)) };
            if let Some(entry) = opt_entry
                && entry.driver.name() == name
            {
                return Some(entry);
            }
        }
        None
    }
}

/// 一个已注册的文件系统驱动条目。
///
/// `&'static dyn FsDriver` 是 `Copy`（引用天然 `Copy`），与
/// [`crate::dev::char::CharDev`] 的 `&'static dyn CharDriver` 设计对齐。
#[derive(Clone, Copy)]
pub struct FsDriverEntry {
    /// 驱动引用。
    pub driver: &'static dyn FsDriver,
}

/// 全局文件系统驱动注册表（无锁，多生产者 SMP 安全）。
///
/// 固定大小数组存储，`AtomicUsize` 分配唯一槽位，
/// `AtomicU32` 位图标记已就绪的槽位，[`FsNameIndex`] 提供 O(1) 期望的名称查找。
///
/// 仅用于启动期注册的文件系统驱动，数量有限（通常 < 20）。
pub struct FsRegistry {
    slots: UnsafeCell<[Option<FsDriverEntry>; MAX_FS_DRIVERS]>,
    /// fetch_add 分配唯一槽位索引。
    reserved_idx: AtomicUsize,
    /// 位图：第 i 位为 1 表示 slots[i] 已完整写入。
    ready_mask: AtomicU32,
    /// 名称哈希索引。
    name_index: FsNameIndex,
}

unsafe impl Sync for FsRegistry {}
unsafe impl Send for FsRegistry {}

#[kernel_symbols::export]
impl FsRegistry {
    /// 构造空注册表。
    pub const fn new() -> Self {
        Self {
            slots: UnsafeCell::new([None; MAX_FS_DRIVERS]),
            reserved_idx: AtomicUsize::new(0),
            ready_mask: AtomicU32::new(0),
            name_index: FsNameIndex::new(),
        }
    }

    /// 注册一个文件系统驱动（无锁多生产者安全）。
    ///
    /// 成功返回分配到的索引，注册表已满时返回 `Err`。
    /// 不检查重名——调用方应确保不重复注册同名驱动。
    #[kernel_symbols::export(
        name = "vfs.superblock.FsRegistry.register",
        contract = "kernel.vfs.filesystem-registry@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE,
        retained_args = 1 << 1
    )]
    pub fn register(&self, driver: &'static dyn FsDriver) -> VfsResult<usize> {
        // 1. 原子抢占唯一槽位
        let idx = self.reserved_idx.fetch_add(1, Ordering::Relaxed);
        if idx >= MAX_FS_DRIVERS {
            self.reserved_idx.fetch_sub(1, Ordering::Relaxed);
            return Err(VfsError::NoSpace);
        }

        let entry = FsDriverEntry { driver };

        // 2. 通过裸指针写入单个元素
        let slot_ptr = self.slots.get() as *mut Option<FsDriverEntry>;
        unsafe {
            slot_ptr.add(idx).write(Some(entry));
        }

        // 3. Release：保证槽位写入对后续 Acquire 加载 ready_mask 的读者可见
        self.ready_mask.fetch_or(1u32 << idx, Ordering::Release);

        // 4. 插入名称哈希索引
        self.name_index.insert(driver.name(), idx);

        Ok(idx)
    }

    /// 按名称查找已注册驱动（O(1) 期望，哈希索引）。
    #[kernel_symbols::export(
        name = "vfs.superblock.FsRegistry.find",
        contract = "kernel.vfs.filesystem-registry@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_QUERY,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_MODULE_BORROW
    )]
    pub fn find(&self, name: &str) -> Option<&'static dyn FsDriver> {
        self.name_index
            .find(name, self.slots.get() as *const Option<FsDriverEntry>)
            .map(|entry| entry.driver)
    }

    /// 已就绪的驱动数量。
    #[inline]
    pub fn len(&self) -> usize {
        self.ready_mask.load(Ordering::Acquire).count_ones() as usize
    }

    /// 注册表是否为空。
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ready_mask.load(Ordering::Acquire) == 0
    }

    /// 返回迭代器。快照当前 ready_mask，之后才就绪的驱动不会出现在本次迭代中。
    #[inline]
    pub fn iter(&self) -> FsRegistryIter<'_> {
        let snapshot = self.ready_mask.load(Ordering::Acquire);
        FsRegistryIter {
            registry: self,
            index: 0,
            snapshot_mask: snapshot,
        }
    }

    /// 返回所有已注册文件系统的名称列表（用于 `/proc/filesystems`）。
    #[kernel_symbols::export(
        name = "vfs.superblock.FsRegistry.list_names",
        contract = "kernel.vfs.filesystem-registry@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_QUERY,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn list_names(&self) -> Vec<String> {
        self.iter().map(|e| String::from(e.driver.name())).collect()
    }
}

impl Default for FsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// [`FsRegistry`] 的迭代器。
///
/// 按值产出 `FsDriverEntry`（`Copy`），完全避免从 `UnsafeCell` 创建共享引用。
pub struct FsRegistryIter<'a> {
    registry: &'a FsRegistry,
    index: u32,
    snapshot_mask: u32,
}

impl Iterator for FsRegistryIter<'_> {
    type Item = FsDriverEntry;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < MAX_FS_DRIVERS as u32 {
            let i = self.index as usize;
            self.index += 1;

            if (self.snapshot_mask & (1u32 << i)) != 0 {
                let slot_ptr = self.registry.slots.get() as *const Option<FsDriverEntry>;
                let opt_entry = unsafe { core::ptr::read(slot_ptr.add(i)) };
                if let Some(entry) = opt_entry {
                    return Some(entry);
                }
            }
        }
        None
    }
}

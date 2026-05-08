//! 挂载点（Mount）与挂载命名空间（Mount Namespace）。
//!
//! 在 Unix 文件系统模型中，"挂载"是将一个文件系统的根目录叠加（覆盖）到目录树
//! 某个节点上的操作。挂载之后，访问该节点的路径将进入被挂载文件系统，而不是
//! 原来的目录内容。
//!
//! ### 挂载命名空间与容器隔离
//!
//! Linux 3.8 引入了挂载命名空间（mount namespace，`CLONE_NEWNS` 标志），允许
//! 不同进程组看到不同的文件系统视图。容器技术（Docker、Podman 等）正是利用此
//! 机制为每个容器提供独立的 `/` 树，而不影响宿主机。
//!
//! 每个 [`MountNamespace`] 持有一套独立的挂载树，同一物理设备可以在不同命名空间
//! 中以不同方式挂载（甚至以不同的只读/读写标志）。
//!
//! ### 挂载树结构
//!
//! - [`Mount`] 是挂载树的节点：它将一个 [`Superblock`] 绑定到一个 Dentry（挂载点），
//!   并记录其在树中的父挂载（`parent`）和子挂载列表（`children`）；
//! - 根挂载的 `parent` 为 `None`，对应文件系统命名空间的 `/`；
//! - 挂载树的遍历由 [`crate::vfs::path`] 中的路径解析代码完成。
//!
//! ### "挂载点是否繁忙"的判断
//!
//! 卸载前必须确认挂载点上没有活跃的文件描述符或路径引用。[`Mount::open_count`]
//! 是一个引用计数器，在文件打开/关闭、路径解析进入/离开挂载时相应递增/递减。
//! `is_busy()` 通过检查此计数器实现，无需扫描整个进程文件描述符表。

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::vfs::DCACHE;
use crate::vfs::dentry::Dentry;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::superblock::Superblock;
use crate::vfs::sync::Spinlock;

/// 挂载标志，对应 `mount(2)` 的 `mountflags` 参数。
///
/// 这些标志描述文件系统以何种约束挂载，影响该挂载上所有文件操作的行为。
/// 注意：挂载标志属于挂载实例（[`Mount`]），而非文件系统本身（[`Superblock`]）。
/// 同一文件系统镜像在不同命名空间可以以不同标志挂载，只读检查应读取 `Mount::flags`。
/// 挂载标志，描述文件系统以何种约束挂载。
///
/// ### 位编码原则
///
/// 位编号顺序分配（0、1、2…），**与 Linux `MS_*` 宏的数值完全解耦**。
/// Linux ABI（`mount(2)` 的 `mountflags` 参数）到本结构的转换必须在 `arch/`
/// 层的 syscall 入口完成（`decode_mount_flags`），VFS 内部只使用命名常量。
///
/// 扩展原则：新增标志追加下一个空闲位，**不得**为匹配 Linux 数值而跳过位，
/// 避免破坏已有位的语义映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MountFlags(pub(crate) u32);

impl MountFlags {
    /// 以只读方式挂载；所有写操作返回 `EROFS`。
    pub const RDONLY: Self = Self(1 << 0);
    /// 禁止 setuid/setgid 可执行文件的特权提升。
    pub const NOSUID: Self = Self(1 << 1);
    /// 禁止访问设备文件；对普通文件系统通常建议开启。
    pub const NODEV: Self = Self(1 << 2);
    /// 禁止执行文件；适用于数据分区（如 `/tmp`）。
    pub const NOEXEC: Self = Self(1 << 3);
    /// 同步写入：所有写操作立即落盘。
    pub const SYNCHRONOUS: Self = Self(1 << 4);
    /// 禁止更新访问时间 atime，提升读密集型工作负载性能。
    pub const NOATIME: Self = Self(1 << 5);
    /// 仅对目录不更新 atime。
    pub const NODIRATIME: Self = Self(1 << 6);
    /// 绑定挂载：将已有目录树挂载到另一挂载点。
    pub const BIND: Self = Self(1 << 7);
    /// 递归绑定挂载：将挂载树下所有子挂载也一并绑定。
    pub const REC: Self = Self(1 << 8);

    pub const fn has(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
    pub const fn with(self, flag: Self) -> Self {
        Self(self.0 | flag.0)
    }
    pub const fn without(self, flag: Self) -> Self {
        Self(self.0 & !flag.0)
    }
    /// 返回原始位字，仅供 ABI 序列化边界使用（如 `/proc/mounts` 标志列输出）。
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// 判断是否只读挂载。
    pub const fn is_rdonly(self) -> bool {
        self.has(Self::RDONLY)
    }
}

/// 挂载点在挂载树中的位置信息，由自旋锁保护以支持 `pivot_root` 原子修改。
pub struct MountLocation {
    /// 挂载点 Dentry：在父文件系统（或根）中，此挂载"遮盖"的目录节点。
    pub mountpoint: Arc<Dentry>,
    /// 父挂载（弱引用，避免循环）。根挂载的 `parent` 为 `None`。
    pub parent: Option<Weak<Mount>>,
}

/// 挂载点：一个文件系统实例在命名空间中的落脚处。
///
/// `Mount` 将 [`Superblock`]（文件系统实例）绑定到 [`Dentry`]（挂载点位置），
/// 并记录挂载树中的父子关系，使得路径解析能够正确跨越挂载边界。
pub struct Mount {
    /// 被挂载的文件系统实例（含根 inode、块大小、驱动操作等）。
    pub superblock: Arc<Superblock>,

    /// 挂载位置（挂载点 Dentry + 父挂载），由自旋锁保护。
    ///
    /// 使用锁而非裸字段，以支持 `pivot_root` 原子地修改挂载点和父挂载关系，
    /// 避免原先 `Arc::get_mut` 方案因引用计数不为 1 而始终失败的问题。
    pub location: Spinlock<MountLocation>,

    /// 被挂载文件系统的根 Dentry。
    ///
    /// 路径解析进入此挂载时，以此 Dentry 为新的"当前目录"继续向下解析。
    pub mount_root: Arc<Dentry>,

    /// 挂载标志，控制此挂载点的访问限制。
    ///
    /// 使用 `AtomicU32` 替代 `Spinlock<MountFlags>`：读（`is_rdonly()`）是路径解析
    /// 极热路径，写（`remount`）极少。原子操作消除锁开销。
    pub flags: AtomicU32,

    /// 子挂载列表（强引用），由自旋锁保护。
    pub children: Spinlock<Vec<Arc<Mount>>>,

    /// 活跃引用计数：当前在此挂载上存活的文件描述符与路径引用之和。
    pub open_count: AtomicUsize,
}

impl Mount {
    /// 构造一个新的挂载节点。
    pub fn new(
        superblock: Arc<Superblock>,
        mountpoint: Arc<Dentry>,
        mount_root: Arc<Dentry>,
        flags: MountFlags,
        parent: Option<Weak<Mount>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            superblock,
            location: Spinlock::new(MountLocation { mountpoint, parent }),
            mount_root,
            flags: AtomicU32::new(flags.raw()),
            children: Spinlock::new(Vec::new()),
            open_count: AtomicUsize::new(0),
        })
    }

    /// 返回挂载点 Dentry 的克隆（持有 location 锁期间复制，锁随即释放）。
    pub fn mountpoint(&self) -> Arc<Dentry> {
        Arc::clone(&self.location.lock().mountpoint)
    }

    /// 判断此挂载点是否只读。
    pub fn is_rdonly(&self) -> bool {
        MountFlags(self.flags.load(Ordering::Relaxed)).is_rdonly()
    }

    /// 检查挂载点是否可写，不可写时返回 `ReadOnlyFilesystem` 错误。
    pub fn check_writable(&self) -> VfsResult<()> {
        if self.is_rdonly() {
            Err(VfsError::ReadOnlyFilesystem)
        } else {
            Ok(())
        }
    }

    /// 获取当前挂载标志的快照。
    pub fn flags_snapshot(&self) -> MountFlags {
        MountFlags(self.flags.load(Ordering::Acquire))
    }

    /// 原子替换挂载标志（用于 `remount`）。
    pub fn set_flags(&self, new_flags: MountFlags) {
        self.flags.store(new_flags.raw(), Ordering::Release);
    }

    /// 添加一个子挂载。
    pub fn add_child(&self, child: Arc<Mount>) {
        self.children.lock().push(child);
    }

    /// 移除指定子挂载（通过指针比较定位）。
    pub fn remove_child(&self, child: &Arc<Mount>) {
        let mut children = self.children.lock();
        if let Some(pos) = children.iter().position(|c| Arc::ptr_eq(c, child)) {
            children.swap_remove(pos);
        }
    }

    /// 递增活跃引用计数（路径解析进入此挂载或打开文件时调用）。
    ///
    /// 使用 `Release` 语义，确保递增对后续 `is_busy(Acquire)` 可见。
    /// 若使用 `Relaxed`，卸载线程的 `Acquire` 加载可能看不到此增量，
    /// 导致 `is_busy()` 误判为零从而允许不安全的卸载。
    pub fn inc_open(&self) {
        self.open_count.fetch_add(1, Ordering::Release);
    }

    /// 递减活跃引用计数（路径解析离开此挂载或关闭文件时调用）。
    ///
    /// 使用 `Release` 语义确保递减操作对后续 `is_busy()` 的 `Acquire` 读可见，
    /// 防止卸载代码在引用尚未完全释放时误判为"不繁忙"。
    pub fn dec_open(&self) {
        self.open_count.fetch_sub(1, Ordering::Release);
    }

    /// 检查此挂载点上是否仍有活跃引用，用于卸载前的安全检查。
    ///
    /// 返回 `true` 表示仍有打开的文件或进行中的路径解析，卸载不安全。
    pub fn is_busy(&self) -> bool {
        self.open_count.load(Ordering::Acquire) > 0
    }
}

/// 挂载命名空间内部数据，由单把 `Spinlock` 保护一致性。
///
/// 合并 `mounts` 扁平列表与 `by_mountpoint`/`by_root` 索引到同一个锁下，
/// 消除 `mount()`/`umount()`/`pivot_root()` 需要依次获取三把锁的不一致窗口。
struct MountData {
    /// 扁平化的 Mount 列表（便于 `umount -a` 遍历）。
    mounts: Vec<Arc<Mount>>,
    /// mountpoint Dentry 地址 → 覆盖在其上的 Mount 列表（后来的在后面）。
    by_mountpoint: BTreeMap<usize, Vec<Arc<Mount>>>,
    /// mount_root Dentry 地址 → Mount。
    by_root: BTreeMap<usize, Arc<Mount>>,
}

impl MountData {
    /// 将 mount 加入扁平列表和索引。
    fn add(&mut self, mount: &Arc<Mount>) {
        let mp_ptr = Arc::as_ptr(&mount.location.lock().mountpoint) as usize;
        let root_ptr = Arc::as_ptr(&mount.mount_root) as usize;
        self.mounts.push(Arc::clone(mount));
        self.by_mountpoint
            .entry(mp_ptr)
            .or_default()
            .push(Arc::clone(mount));
        self.by_root.insert(root_ptr, Arc::clone(mount));
    }

    /// 将 mount 从索引中移除（不从 mounts 列表移除，调用方自行处理）。
    fn index_remove(&mut self, mount: &Arc<Mount>) {
        let mp_ptr = Arc::as_ptr(&mount.location.lock().mountpoint) as usize;
        let root_ptr = Arc::as_ptr(&mount.mount_root) as usize;
        if let Some(list) = self.by_mountpoint.get_mut(&mp_ptr) {
            list.retain(|m| !Arc::ptr_eq(m, mount));
            if list.is_empty() {
                self.by_mountpoint.remove(&mp_ptr);
            }
        }
        self.by_root.remove(&root_ptr);
    }
}

/// 挂载命名空间：进程可见的挂载树视图。
///
/// 每个进程（或进程组，在使用 `CLONE_NEWNS` 创建新命名空间之前）共享同一个
/// `MountNamespace`。`clone(CLONE_NEWNS)` 创建子命名空间时，父命名空间的挂载
/// 树被拷贝（写时复制），之后的挂载/卸载操作在各自命名空间中独立进行。
///
/// ### 单锁一致性
///
/// `mounts` 扁平列表与 `by_mountpoint`/`by_root` 索引合并在 `MountData` 中，
/// 由单把 `Spinlock` 保护。`mount()`/`umount()`/`pivot_root()` 在同一临界区内
/// 同步更新列表和索引，消除多锁方案的不一致窗口。
pub struct MountNamespace {
    /// 命名空间的唯一 ID，用于日志和 `/proc/mounts` 等调试接口。
    pub id: u64,

    /// 命名空间的根挂载（对应文件系统 `/`）。
    ///
    /// 用 `Spinlock` 包裹以支持 `pivot_root` 原子替换根挂载，无需重建整个命名空间。
    pub root: Spinlock<Arc<Mount>>,

    /// 挂载数据（扁平列表 + 索引），单锁保护。
    data: Spinlock<MountData>,
}

impl MountNamespace {
    /// 构造一个新的挂载命名空间，以 `root_mount` 为根。
    pub fn new(id: u64, root_mount: Arc<Mount>) -> Arc<Self> {
        let mp_ptr = Arc::as_ptr(&root_mount.location.lock().mountpoint) as usize;
        let root_ptr = Arc::as_ptr(&root_mount.mount_root) as usize;

        let mut by_mp = BTreeMap::new();
        by_mp.insert(mp_ptr, alloc::vec![Arc::clone(&root_mount)]);
        let mut by_rt = BTreeMap::new();
        by_rt.insert(root_ptr, Arc::clone(&root_mount));

        Arc::new(Self {
            id,
            root: Spinlock::new(Arc::clone(&root_mount)),
            data: Spinlock::new(MountData {
                mounts: alloc::vec![root_mount],
                by_mountpoint: by_mp,
                by_root: by_rt,
            }),
        })
    }

    /// 在指定挂载点 Dentry 上执行挂载操作，将 `superblock` 挂载到 `mountpoint`。
    ///
    /// - 检查 `mountpoint` 是否已有挂载（嵌套挂载是允许的，后来的覆盖前者）；
    /// - 在挂载树中为新挂载找到正确的父 `Mount`；
    /// - 将新 `Mount` 插入父的 `children` 列表和扁平的 `mounts` 列表。
    pub fn mount(
        &self,
        mountpoint: Arc<Dentry>,
        superblock: Arc<Superblock>,
        flags: MountFlags,
    ) -> VfsResult<Arc<Mount>> {
        // 找父 mount：包含 mountpoint 所在文件系统的 mount（通过 superblock 匹配）。
        //
        // 不能用 lookup_mount(&mountpoint)——那会返回"叠加在 mountpoint 上的 mount"，
        // 即 mountpoint 作为挂载点时其上方已有的挂载；而父 mount 应是 mountpoint 所属
        // 文件系统对应的 mount（即 mountpoint 的 inode 所在的 superblock 对应的 mount）。
        //
        // 例：Mount A (sb=SA) 挂载于 "/"，目录 /mnt 属于 SA；此时再在 /mnt 挂载 Mount B：
        //   - lookup_mount("/mnt") 找不到任何 mount（/mnt 尚未被挂载），返回 None → 根 mount（正确）
        //   - 再次在 /mnt 挂载 Mount C：
        //     · lookup_mount("/mnt") 返回 Mount B（错误——B 是叠在 /mnt 上的 mount，不是 C 的父）
        //     · 正确父应为 Mount A（/mnt 的 inode 属于 SA，对应 Mount A）
        let mountpoint_sb = mountpoint
            .inode()
            .and_then(|inode| inode.superblock.upgrade());
        let parent_mount = {
            let data = self.data.lock();
            data.mounts
                .iter()
                .find(|m| {
                    mountpoint_sb
                        .as_ref()
                        .is_some_and(|sb| Arc::ptr_eq(&m.superblock, sb))
                })
                .cloned()
        }
        .unwrap_or_else(|| Arc::clone(&self.root.lock()));

        let mount_root = Arc::clone(&superblock.root_dentry);
        let new_mount = Mount::new(
            superblock,
            Arc::clone(&mountpoint),
            mount_root,
            flags,
            Some(Arc::downgrade(&parent_mount)),
        );

        parent_mount.add_child(Arc::clone(&new_mount));
        self.data.lock().add(&new_mount);
        Ok(new_mount)
    }

    /// 卸载 `mountpoint` 上最上层的挂载。
    ///
    /// - 若挂载点仍有引用（打开的文件、子挂载），默认返回 `VfsError::DeviceBusy`；
    /// - `force` 为 `true` 时强制卸载（`umount -f`），等待所有引用释放或强制中断；
    /// - 卸载成功后从 `mounts` 和父的 `children` 中移除该 `Mount`。
    pub fn umount(&self, mountpoint: &Arc<Dentry>, force: bool) -> VfsResult<()> {
        let mut data = self.data.lock();
        let pos = data
            .mounts
            .iter()
            .rposition(|m| Arc::ptr_eq(&m.location.lock().mountpoint, mountpoint))
            .ok_or(VfsError::InvalidArgument)?;
        let mount = Arc::clone(&data.mounts[pos]);

        if !force && mount.is_busy() {
            return Err(VfsError::DeviceBusy);
        }
        if !force && !mount.children.lock().is_empty() {
            return Err(VfsError::DeviceBusy);
        }

        let removed_roots: Vec<Arc<Dentry>>;
        if force {
            let mut to_remove: Vec<Arc<Mount>> = Vec::new();
            let mut queue: alloc::collections::VecDeque<Arc<Mount>> =
                alloc::collections::VecDeque::new();
            queue.push_back(Arc::clone(&mount));
            while let Some(m) = queue.pop_front() {
                for child in m.children.lock().iter() {
                    queue.push_back(Arc::clone(child));
                }
                to_remove.push(m);
            }
            removed_roots = to_remove
                .iter()
                .map(|mount| Arc::clone(&mount.mount_root))
                .collect();
            let remove_ptrs: Vec<usize> =
                to_remove.iter().map(|m| Arc::as_ptr(m) as usize).collect();
            data.mounts
                .retain(|m| !remove_ptrs.contains(&(Arc::as_ptr(m) as usize)));
            for m in &to_remove {
                data.index_remove(m);
            }
        } else {
            removed_roots = alloc::vec![Arc::clone(&mount.mount_root)];
            data.index_remove(&mount);
            data.mounts.swap_remove(pos);
        }

        // 从父 mount 的 children 列表移除
        if let Some(weak_parent) = &mount.location.lock().parent
            && let Some(parent) = weak_parent.upgrade()
        {
            parent.remove_child(&mount);
        }
        drop(data);

        for root in removed_roots {
            DCACHE.invalidate_subtree(&root);
        }
        Ok(())
    }

    /// 在命名空间中查找 `mount_root` 与给定 dentry 相同（Arc 指针相等）的 Mount。
    ///
    /// 用于 `clone(CLONE_NEWNS)` 后，在新命名空间中找到与旧命名空间同一文件系统
    /// 对应的 Mount（两者共享同一 `mount_root` Arc，因为 `clone_namespace` 用的是
    /// `Arc::clone`）。
    ///
    /// O(log n) 通过 `by_root` 索引查找。
    pub fn find_mount_for_root(&self, mount_root: &Arc<Dentry>) -> Option<Arc<Mount>> {
        let root_ptr = Arc::as_ptr(mount_root) as usize;
        self.data.lock().by_root.get(&root_ptr).cloned()
    }

    /// 若 `dentry` 是某个文件系统的根 Dentry（`mount_root`），返回对应的挂载点
    /// Dentry（用于 `..` 跨越挂载边界向上回溯到父文件系统）。
    ///
    /// 与 [`lookup_mount`] 的区别：`lookup_mount` 是"进入"挂载（给定挂载点返回被
    /// 挂载 FS 的根），此方法是"退出"挂载（给定挂载根返回其在父 FS 中的落脚点）。
    ///
    /// O(log n) 通过 `by_root` 索引查找。
    pub fn find_mountpoint(&self, dentry: &Arc<Dentry>) -> Option<Arc<Dentry>> {
        let root_ptr = Arc::as_ptr(dentry) as usize;
        self.data
            .lock()
            .by_root
            .get(&root_ptr)
            .map(|m| Arc::clone(&m.location.lock().mountpoint))
    }

    /// 查找覆盖在 `dentry` 上的最顶层挂载。
    ///
    /// 若 `dentry` 是某个挂载点，返回覆盖在其上的最顶层 [`Mount`]；
    /// 若无挂载覆盖，返回 `None`。
    ///
    /// 返回 `Arc<Mount>` 而非 `Arc<Dentry>`，使调用方能够：
    /// 1. 通过 `mount.mount_root` 获取被挂载文件系统的根 Dentry；
    /// 2. 检查 `mount.flags`（如 RDONLY）；
    /// 3. 维护 `mount.open_count`。
    ///
    /// O(log n) 通过 `by_mountpoint` 索引查找，取列表最后一个（最顶层）。
    pub fn lookup_mount(&self, dentry: &Arc<Dentry>) -> Option<Arc<Mount>> {
        let mp_ptr = Arc::as_ptr(dentry) as usize;
        self.data
            .lock()
            .by_mountpoint
            .get(&mp_ptr)
            .and_then(|list| list.last().cloned())
    }

    /// 创建当前命名空间的拷贝（用于 `clone(CLONE_NEWNS)`）。
    ///
    /// 深拷贝挂载树结构（创建新的 `Mount` 节点，但共享同一批 `Superblock`），
    /// 使得两个命名空间的后续挂载操作互不影响。
    ///
    /// 两遍扫描算法：
    /// 1. 第一遍：为每个旧 Mount 创建对应的新 Mount（复制 superblock/mountpoint/flags）；
    /// 2. 第二遍：重建父子关系（新 parent = 旧 parent 对应的新 Mount）。
    pub fn clone_namespace(&self) -> VfsResult<Arc<MountNamespace>> {
        use alloc::collections::BTreeMap;
        let data = self.data.lock();

        // 用旧 Mount 指针地址映射到新 Mount
        let mut old_to_new: BTreeMap<usize, Arc<Mount>> = BTreeMap::new();

        // 第一遍：创建新 Mount 节点（location.parent 暂设 None，第二遍填）
        for old in data.mounts.iter() {
            let old_loc = old.location.lock();
            let new_mount = Arc::new(Mount {
                superblock: Arc::clone(&old.superblock),
                location: Spinlock::new(MountLocation {
                    mountpoint: Arc::clone(&old_loc.mountpoint),
                    parent: None, // 第二遍填充
                }),
                mount_root: Arc::clone(&old.mount_root),
                flags: AtomicU32::new(old.flags.load(Ordering::Relaxed)),
                children: Spinlock::new(Vec::new()),
                open_count: AtomicUsize::new(0),
            });
            drop(old_loc);
            old_to_new.insert(Arc::as_ptr(old) as usize, new_mount);
        }

        // 第二遍：重建父子关系
        // 先收集需要设置的 (child_ptr, parent_ptr) 对，避免同时借用 old_to_new
        let mut parent_links: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
        for old in data.mounts.iter() {
            if let Some(weak_parent) = &old.location.lock().parent
                && let Some(old_parent) = weak_parent.upgrade()
            {
                parent_links.push((Arc::as_ptr(old) as usize, Arc::as_ptr(&old_parent) as usize));
            }
        }
        for (child_ptr, parent_ptr) in parent_links {
            let new_parent = match old_to_new.get(&parent_ptr) {
                Some(p) => Arc::clone(p),
                None => continue,
            };
            // 先在引用计数为 1 时（仅 map 持有）独占修改 location.parent 字段，
            // 再克隆 Arc 用于 add_child——顺序不能颠倒。
            {
                let entry = match old_to_new.get_mut(&child_ptr) {
                    Some(e) => e,
                    None => continue,
                };
                if let Some(m) = Arc::get_mut(entry) {
                    m.location.lock().parent = Some(Arc::downgrade(&new_parent));
                }
            }
            let new_child = match old_to_new.get(&child_ptr) {
                Some(c) => Arc::clone(c),
                None => continue,
            };
            new_parent.add_child(new_child);
        }

        let new_root_ptr = Arc::as_ptr(&self.root.lock()) as usize;
        let new_root = old_to_new
            .get(&new_root_ptr)
            .cloned()
            .ok_or(VfsError::InvalidArgument)?;

        let new_mounts: Vec<Arc<Mount>> = old_to_new.into_values().collect();

        // 重建索引
        let mut by_mp = BTreeMap::new();
        let mut by_rt = BTreeMap::new();
        for m in &new_mounts {
            let mp_ptr = Arc::as_ptr(&m.location.lock().mountpoint) as usize;
            let root_ptr = Arc::as_ptr(&m.mount_root) as usize;
            by_mp
                .entry(mp_ptr)
                .or_insert_with(Vec::new)
                .push(Arc::clone(m));
            by_rt.insert(root_ptr, Arc::clone(m));
        }

        let new_ns = Arc::new(MountNamespace {
            id: self.id.wrapping_add(1),
            root: Spinlock::new(Arc::clone(&new_root)),
            data: Spinlock::new(MountData {
                mounts: new_mounts,
                by_mountpoint: by_mp,
                by_root: by_rt,
            }),
        });

        Ok(new_ns)
    }

    /// 执行 `pivot_root`：将新根（`new_root`）设为命名空间根，旧根移到 `put_old`。
    ///
    /// 这是容器初始化的关键步骤，使容器进程完全无法访问宿主机文件系统。
    ///
    /// ### 前置条件（调用方需验证）
    ///
    /// 1. `new_root` 必须是某个挂载点的 `mount_root`（即它自身是被挂载 FS 的根 Dentry）；
    /// 2. `put_old` 必须位于 `new_root` 所在文件系统的目录树之下；
    /// 3. `new_root` 和 `put_old` 必须属于同一个命名空间（即 `self`）；
    /// 4. 调用方持有 `CAP_SYS_ADMIN`。
    ///
    /// ### 操作步骤（参考 Linux `do_pivot_root()`）
    ///
    /// ```text
    /// 设 old_root_mount = 当前命名空间根 Mount
    ///    new_mount       = new_root 对应的 Mount（即以 new_root 为 mount_root 的那个）
    ///
    /// 1. 找到 new_mount（mountpoint 指向 new_root 父目录下某个 dentry，
    ///    mount_root == new_root）
    /// 2. 将 old_root_mount 的 mountpoint 改为 put_old，parent 改为 new_mount
    /// 3. 将 new_mount 从其父 mount 的 children 中移除（它即将成为根）
    /// 4. 更新 self.root = new_mount
    /// ```
    pub fn pivot_root(&self, new_root: Arc<Dentry>, put_old: Arc<Dentry>) -> VfsResult<()> {
        let mut data = self.data.lock();

        // 1. 找 new_mount：mount_root 指针等于 new_root 的 Mount
        let new_mount = data
            .mounts
            .iter()
            .find(|m| Arc::ptr_eq(&m.mount_root, &new_root))
            .cloned()
            .ok_or(VfsError::InvalidArgument)?;

        // 2. 取得旧根 mount
        let old_root_mount = Arc::clone(&self.root.lock());

        // 3. 不允许 new_mount 就是旧根
        if Arc::ptr_eq(&new_mount, &old_root_mount) {
            return Err(VfsError::InvalidArgument);
        }

        // 4. put_old 不能已经是某个 busy mount 的挂载点
        if data
            .mounts
            .iter()
            .any(|m| Arc::ptr_eq(&m.location.lock().mountpoint, &put_old) && m.is_busy())
        {
            return Err(VfsError::DeviceBusy);
        }

        // 5. 从 new_mount 的旧父 mount 的 children 列表和旧挂载点索引中移除 new_mount。
        let weak_old_parent = new_mount.location.lock().parent.clone();
        if let Some(weak_parent) = weak_old_parent
            && let Some(old_parent) = weak_parent.upgrade()
        {
            old_parent.remove_child(&new_mount);
        }
        data.index_remove(&new_mount);
        {
            let mut loc = new_mount.location.lock();
            loc.mountpoint = Arc::clone(&new_root);
            loc.parent = None;
        }
        {
            let mp_ptr = Arc::as_ptr(&new_root) as usize;
            let root_ptr = Arc::as_ptr(&new_mount.mount_root) as usize;
            data.by_mountpoint
                .entry(mp_ptr)
                .or_default()
                .push(Arc::clone(&new_mount));
            data.by_root.insert(root_ptr, Arc::clone(&new_mount));
        }

        // 6. 将旧根 mount 移到 put_old 下面，同步更新索引
        data.index_remove(&old_root_mount);
        {
            let mut loc = old_root_mount.location.lock();
            loc.mountpoint = Arc::clone(&put_old);
            loc.parent = Some(Arc::downgrade(&new_mount));
        }
        // 手动重新加入索引（不能用 add() 因为不需要再 push 到 mounts 列表）
        {
            let mp_ptr = Arc::as_ptr(&put_old) as usize;
            let root_ptr = Arc::as_ptr(&old_root_mount.mount_root) as usize;
            data.by_mountpoint
                .entry(mp_ptr)
                .or_default()
                .push(Arc::clone(&old_root_mount));
            data.by_root.insert(root_ptr, Arc::clone(&old_root_mount));
        }

        // 7. 将旧根 mount 加入 new_mount 的 children
        new_mount.add_child(Arc::clone(&old_root_mount));

        // 8. 更新命名空间根
        drop(data);
        *self.root.lock() = new_mount;

        Ok(())
    }

    /// 将当前所有挂载信息格式化为 `/proc/mounts` 格式字符串（用于 procfs）。
    pub fn dump_mounts(&self) -> alloc::string::String {
        let visible_root = Arc::clone(&self.root.lock().mount_root);
        let data = self.data.lock();
        let mut out = alloc::string::String::new();
        for m in data.mounts.iter() {
            let fs_type = m.superblock.fs_type;
            let fs_id = m.superblock.fs_id.raw();
            let mp_path = m
                .location
                .lock()
                .mountpoint
                .full_path(&visible_root)
                .unwrap_or_else(|| alloc::string::String::from("?"));
            let rw = if m.is_rdonly() { "ro" } else { "rw" };
            out.push_str(&alloc::format!(
                "{:#x} {} {} {}\n",
                fs_id,
                fs_type,
                mp_path,
                rw
            ));
        }
        out
    }
}

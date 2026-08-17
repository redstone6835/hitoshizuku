//! 本模块定义了 VFS 层的 Inode 抽象，是整个虚拟文件系统中最核心的数据结构。
//!
//! Inode（index node，索引节点）源自 Unix 文件系统的经典概念。每个 Inode 代表
//! 文件系统中一个具体的对象——可以是普通文件、目录、符号链接、字符设备节点、
//! 块设备节点、FIFO 管道或 Unix 域套接字。Inode 与文件名无关：同一个 Inode
//! 可以被多个目录项（Dentry）通过硬链接引用，但底层的数据和元数据只有一份。
//!
//! 本模块的设计遵循以下原则：
//!
//! 在所有权管理方面，Inode 通过 Arc<Inode> 实现共享所有权。内核中的多个子系统
//! 会同时持有对同一 Inode 的引用：Dentry 缓存中的正向条目持有 Arc<Inode>，
//! Superblock 的 inode_cache 持有 Weak<Inode>（弱引用，不阻止回收），打开的
//! File 对象也持有 Arc<Inode>。这种设计确保了只要有任何一方仍在使用某个 Inode，
//! 它就不会被释放。
//!
//! 在磁盘资源回收方面，本模块采用“两阶段回收”而非在 `unlink` 现场立即释放资源。
//! 当文件被 unlink 或目录被 rmdir 后，如果该 Inode 的硬链接计数（nlink）降为零，
//! VFS 操作层会先将该 Inode 从命名空间和 inode cache 中摘除，标记为"待回收"；
//! 真正的 `InodeOps::evict` 则延迟到最后一个强引用释放时由 `Drop` 触发。这样可以
//! 同时满足两点：
//! - 已删除对象不会再通过路径或 inode cache 被重新发现；
//! - 仍被打开文件持有的对象不会被过早回收，符合 Unix `unlink-but-open` 语义。
//!
//! 在并发保护方面，Inode 将字段分为不可变和可变两类。id（全局唯一标识符）、
//! kind（文件类型）、rdev（设备号）、blksize（I/O 块大小）以及 stat_dev
//! （对外暴露给用户空间的 st_dev）在创建后永远不变，可以在任意时刻无锁读取。
//! size、nlink、权限位、时间戳等会随文件操作而变化的元数据被聚合到 InodeMeta
//! 结构体中，通过 Spinlock 保护；其中 size 和 nlink 还会同步镜像到原子字段，
//! 供 `lseek(SEEK_END)`、`O_APPEND`、unlink/link 等热路径无锁读取。这种拆分
//! 使得类型检查、inode 号读取等高频只读操作完全不需要获取锁，而最常见的元数据
//! 只读访问也能尽量避开总锁。
//!
//! 在操作分离方面，Inode 的元数据存储与文件系统特定的操作逻辑完全解耦。每个
//! Inode 持有一个 `Arc<dyn InodeOps + Send + Sync>` trait object，由具体的文件
//! 系统驱动（ext4、tmpfs、procfs 等）在创建 Inode 时注入。VFS 层通过这个 trait
//! object 调用 lookup、create、unlink 等操作，无需了解底层文件系统的任何细节。
//! InodeOps 的所有方法接收 &Inode 而非 &Arc<Inode>，防止驱动随意克隆 Arc 引用
//! 导致引用计数管理混乱。
//!
//! 在循环引用防护方面，Inode 通过 Weak<Superblock> 持有对所属超级块的反向引用。
//! 由于 Superblock 的 inode_cache 已经通过 Arc<Inode> 或 Weak<Inode> 持有对
//! Inode 的正向引用，如果 Inode 也用 Arc 指回 Superblock 就会形成引用环，导致
//! 两者都无法被释放。使用 Weak 引用打破了这个环：当 Superblock 被卸载时，所有
//! Inode 中的 Weak 引用自动失效，不会阻止 Superblock 的内存回收。

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::vfs::cred::{Credentials, Gid, Uid};
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::file::OpenOptions;
use crate::vfs::stat::{DevId, FileMode, FileStat, FileType, FsId, Timespec};
use crate::vfs::superblock::Superblock;

/// Inode 在内核中的全局唯一标识符。
///
/// 由文件系统实例标识符 fs_id 和该文件系统内部的 inode 编号 ino 组成。即使两个
/// 不同的文件系统各自都有编号为 1 的 inode，它们的 InodeId 也不会冲突，因为
/// fs_id 不同。VFS 层在执行 rename 和 link 等跨目录操作时，通过比较 fs_id 来
/// 判断源和目标是否位于同一个文件系统，不在同一文件系统上的硬链接和重命名会被
/// 拒绝并返回 CrossDevice 错误。
///
/// 需要注意的是，fs_id 是 VFS 内部用于"同一文件系统"判断的键，与 stat(2) 系统
/// 调用返回给用户空间的 st_dev 字段不同。st_dev 来自 Superblock 的 dev_id 字段，
/// 对于块设备文件系统是实际的设备号，对于内存文件系统则由 fs_id 合成。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InodeId {
    /// 文件系统实例标识符，在挂载时由 VFS 层分配，用于区分不同的文件系统实例。
    pub fs_id: FsId,
    /// 该 Inode 在其所属文件系统中的唯一编号，由文件系统驱动分配和管理。
    pub ino: u64,
}

/// Inode 的动态元数据集合。
///
/// 这些字段在 Inode 创建之后仍然会发生变化：文件被写入时 size 和 mtime 更新，
/// chmod 修改 mode，chown 修改 uid 和 gid，unlink 递减 nlink，truncate 改变
/// size 和 blocks。所有这些可变字段被聚合到一个结构体中，整体受 Spinlock 保护。
///
/// 将可变字段与不可变字段分离的好处在于：读取文件类型（kind）、inode 编号（id）、
/// 设备号（rdev）等不可变信息时完全不需要获取锁，只有真正需要读写 size、nlink
/// 或时间戳时才进入临界区，从而缩短了锁的持有时间并降低了多核竞争。
#[derive(Clone, Copy)]
pub struct InodeMeta {
    /// 文件的字节大小。对于普通文件，这是实际的数据长度；对于目录，其语义由具体
    /// 文件系统定义（有些文件系统将其设为目录项占用的总字节数，有些设为固定值）。
    pub size: u64,
    /// 硬链接计数。每当一个新的目录项指向该 Inode 时 nlink 加一，目录项被删除时
    /// 减一。当 nlink 降为零且没有任何进程打开该文件时，文件系统驱动应当回收该
    /// Inode 占用的所有磁盘资源（数据块和 inode 位图条目）。
    pub nlink: u32,
    /// 文件的访问权限位，包括所有者、所属组和其他用户的读写执行权限，以及 setuid、
    /// setgid 和 sticky 等特殊位。
    pub mode: FileMode,
    /// 文件所有者的用户标识符。
    pub uid: Uid,
    /// 文件所属组的组标识符。
    pub gid: Gid,
    /// 最后访问时间。读取文件内容时更新（但许多文件系统出于性能考虑会延迟更新
    /// 或通过 noatime 挂载选项完全禁用）。
    pub atime: Timespec,
    /// 最后内容修改时间。写入文件数据或 truncate 改变文件大小时更新。
    pub mtime: Timespec,
    /// 最后元数据变更时间。任何修改 Inode 元数据的操作（chmod、chown、link、
    /// unlink 等）都会更新此字段，包括那些同时更新 mtime 的操作。
    pub ctime: Timespec,
    /// 已分配的 512 字节逻辑块数，对应 stat(2) 的 st_blocks 字段。这个值包含
    /// 文件系统为存储文件数据和元数据而分配的所有块，但不包含稀疏文件中的空洞
    /// 部分（空洞不占用实际磁盘空间，读取时返回全零）。
    pub blocks: u64,
}

const STATE_LIVE: u8 = 0;
const STATE_ORPHANED: u8 = 1;
const STATE_EVICTED: u8 = 2;
/// 私有文件页缓存身份；0 保留为“不可缓存”，耗尽后永久停止分配新身份。
static NEXT_PRIVATE_PAGE_CACHE_ID: AtomicUsize = AtomicUsize::new(1);

const DATA_MUTATION_BITS: u32 = 16;
const DATA_MUTATION_MASK: u64 = (1 << DATA_MUTATION_BITS) - 1;
const DATA_GENERATION_SHIFT: u32 = DATA_MUTATION_BITS;
const DATA_CACHE_DISABLED: u64 = 1 << 63;
const DATA_GENERATION_MAX: u64 = (DATA_CACHE_DISABLED - 1) >> DATA_GENERATION_SHIFT;

const fn data_state(generation: u64, active: usize, disabled: bool) -> u64 {
    debug_assert!(generation <= DATA_GENERATION_MAX);
    debug_assert!(active <= DATA_MUTATION_MASK as usize);
    (generation << DATA_GENERATION_SHIFT)
        | active as u64
        | if disabled { DATA_CACHE_DISABLED } else { 0 }
}

const fn data_state_generation(state: u64) -> u64 {
    (state & !DATA_CACHE_DISABLED) >> DATA_GENERATION_SHIFT
}

const fn data_state_active(state: u64) -> usize {
    (state & DATA_MUTATION_MASK) as usize
}

const fn data_state_disabled(state: u64) -> bool {
    state & DATA_CACHE_DISABLED != 0
}

const fn finish_data_mutation_state(state: u64) -> Option<u64> {
    let active = data_state_active(state);
    if active == 0 {
        return None;
    }
    let generation = data_state_generation(state);
    let disabled = data_state_disabled(state) || generation == DATA_GENERATION_MAX;
    let generation = if generation == DATA_GENERATION_MAX {
        generation
    } else {
        generation + 1
    };
    Some(data_state(generation, active - 1, disabled))
}

fn allocate_private_page_cache_id() -> usize {
    NEXT_PRIVATE_PAGE_CACHE_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .unwrap_or(0)
}

/// 根据文件系统实例标识符和超级块设备号推导 `stat(2)` 使用的 `st_dev` 值。
///
/// 对块设备文件系统，优先使用真实底层设备号；对纯内存文件系统，则回退到由
/// `fs_id` 合成的稳定虚拟设备号。该值在一次挂载生命周期内不会变化，因此适合
/// 在 Inode 创建时直接缓存到只读字段中。
pub(crate) fn derive_stat_dev(fs_id: FsId, dev_id: Option<DevId>) -> DevId {
    dev_id.unwrap_or_else(|| DevId::new((fs_id.raw() >> 32) as u32, fs_id.raw() as u32))
}

/// 文件系统对象的核心内存表示。
///
/// 每个 Inode 实例对应磁盘（或内存文件系统中）的一个唯一文件系统对象。Inode 与
/// 文件名完全无关——文件名由 Dentry（目录项）管理，一个 Inode 可以被零个、一个
/// 或多个 Dentry 引用（分别对应已删除的文件、普通文件和有多个硬链接的文件）。
///
/// 当文件被 unlink 或目录被 rmdir 导致 nlink 降为零时，VFS 操作层会先将该
/// Inode 标记为"待回收"并从 inode cache 中移除；真正的 `InodeOps::evict`
/// 由 `Drop` 在最后一个强引用释放时触发。这样，已打开但已从命名空间删除的文件
/// 仍可继续通过 fd 访问，而磁盘资源只会在最后一个引用消失后才被释放。
pub struct Inode {
    /// 全局唯一标识符，由文件系统实例 ID 和 inode 编号组成。在 Inode 创建时确定，
    /// 之后永远不会改变。
    pub(crate) id: InodeId,

    /// 内核生命周期内不复用的私有文件页缓存身份。
    private_page_cache_id: usize,

    /// 文件类型（普通文件、目录、符号链接、字符设备、块设备、FIFO 或套接字）。
    /// 在 Inode 创建时确定，之后永远不会改变。将文件类型作为顶层不可变字段而非
    /// 放在 InodeMeta 锁内，使得路径解析中频繁进行的类型检查（例如判断是否为
    /// 目录以决定能否继续向下查找）完全不需要获取锁。
    pub(crate) kind: FileType,

    /// 设备文件的设备号，对应 stat(2) 的 st_rdev 字段。只有当 kind 为
    /// CharDevice 或 BlockDevice 时此字段才有意义，其他类型的文件此字段为零值。
    /// 在 Inode 创建时由 mknod 操作设定，之后永远不会改变。
    pub(crate) rdev: DevId,

    /// 文件系统建议的最优 I/O 块大小（字节），对应 stat(2) 的 st_blksize 字段。
    /// 通常等于文件系统的逻辑块大小（例如 ext4 默认为 4096），由文件系统驱动在
    /// 创建 Inode 时填入，之后永远不会改变。应用程序可以参考此值来选择 read/write
    /// 的缓冲区大小以获得最佳 I/O 性能。
    pub(crate) blksize: u32,

    /// 对应 stat(2) 的 `st_dev` 字段，在 Inode 创建时计算并缓存。
    ///
    /// 该值在一次挂载生命周期内保持稳定：块设备文件系统使用真实设备号，内存文件系统
    /// 使用由 fs_id 推导出的虚拟设备号。将其缓存为只读字段后，`stat`/`fstat`
    /// 热路径不再需要为了同一份信息反复访问 Superblock。
    stat_dev: DevId,

    /// 可变元数据集合，受 Spinlock 保护。所有需要在 Inode 生命周期内修改的字段
    /// 都聚合在这个结构体中，通过一把锁统一保护，避免为每个字段单独加锁带来的
    /// 复杂性和潜在的锁序问题。
    meta: crate::vfs::sync::Spinlock<InodeMeta>,

    /// `meta.size` 的原子镜像，用于无锁读取文件长度热路径。
    cached_size: AtomicU64,

    /// `meta.nlink` 的原子镜像，用于 link/unlink/evict 相关热路径。
    cached_nlink: AtomicU32,

    /// 文件代际、活跃修改数和永久禁用标志的原子快照。
    ///
    /// 三者必须由读者一致观察；合并后稳定代际查询只需一次 Acquire，写者仍以
    /// 单次 AcqRel 更新发布开始和结束状态。
    data_state: AtomicU64,

    /// 普通文件的写打开与执行映像排斥状态。
    ///
    /// 正值表示当前持有写访问的打开文件描述数量，负值表示当前引用该 inode 的
    /// 执行映像数量，零表示空闲。两类访问通过同一个原子量切换，保证并发
    /// `open(O_WRONLY)` 与 `execve` 不会同时成功。
    exec_write_state: AtomicIsize,

    /// 文件系统驱动提供的操作实现。VFS 层通过这个 trait object 调用 lookup、
    /// create、read、write 等操作，实现对不同文件系统（ext4、tmpfs、procfs 等）
    /// 的透明访问。通过 Arc 共享操作对象，使同一类 inode 可以复用同一套方法实现，
    /// 避免为每个 inode 单独进行一次 Box 分配。
    pub(crate) ops: Arc<dyn InodeOps + Send + Sync>,

    /// 指向所属超级块的弱引用。使用 Weak 而非 Arc 是为了打破引用环：Superblock
    /// 的 inode_cache 通过 Weak<Inode> 持有对 Inode 的引用，而 Inode 如果也用
    /// Arc 指回 Superblock 就会形成双向强引用环，导致两者都无法被释放。Weak 引用
    /// 在 Superblock 被卸载后自动失效，upgrade() 会返回 None。
    pub(crate) superblock: Weak<Superblock>,

    /// 该 inode 是否可能携带扩展属性（xattr 快速路径提示）。
    ///
    /// 为 `false` 时权限检查无需读取 xattr 块；由驱动在装载（如 extfs 的
    /// `i_file_acl != 0`）或首次 setxattr 时置位。对象随 inode 生命周期
    /// 失效，无陈旧风险。
    has_xattrs: AtomicBool,

    /// 生命周期状态：
    /// - LIVE: 仍在命名空间中可达；
    /// - ORPHANED: 已从命名空间摘除，等待最后一个强引用释放；
    /// - EVICTED: 已执行底层资源回收。
    lifecycle: AtomicU8,
}

/// 文件内容发布区间的 RAII guard。只对显式声明支持私有页缓存的普通文件激活。
pub(crate) struct InodeDataMutation<'a> {
    inode: &'a Inode,
    active: bool,
}

impl Drop for InodeDataMutation<'_> {
    fn drop(&mut self) {
        if self.active {
            self.inode.end_data_mutation_raw();
        }
    }
}

impl Inode {
    /// 构造一个新的 Inode 并返回其 Arc 引用。
    ///
    /// 调用方需要提供所有不可变字段（id、kind、rdev、blksize）、初始元数据、
    /// 文件系统驱动的操作实现、所属文件系统对外暴露的设备号信息，以及所属超级块的
    /// 弱引用。构造过程中会自动推导并缓存 `stat_dev`。构造完成后，Inode 的不可变
    /// 字段就被冻结，后续只能通过 meta 锁修改可变元数据。
    pub fn new(
        id: InodeId,
        kind: FileType,
        rdev: DevId,
        blksize: u32,
        dev_id: Option<DevId>,
        meta: InodeMeta,
        ops: Arc<dyn InodeOps + Send + Sync>,
        superblock: Weak<Superblock>,
    ) -> Arc<Self> {
        // 只有显式支持稳定内容代际的普通文件才会进入全局私有页缓存。目录、设备
        // 以及未实现该协议的文件不应争用全局 ID 分配原子的 cache line。
        let private_page_cache_id =
            if kind == FileType::Regular && ops.supports_private_page_cache() {
                allocate_private_page_cache_id()
            } else {
                0
            };
        Arc::new(Self {
            id,
            private_page_cache_id,
            kind,
            rdev,
            blksize,
            stat_dev: derive_stat_dev(id.fs_id, dev_id),
            meta: crate::vfs::sync::Spinlock::new(meta),
            cached_size: AtomicU64::new(meta.size),
            cached_nlink: AtomicU32::new(meta.nlink),
            data_state: AtomicU64::new(data_state(1, 0, false)),
            exec_write_state: AtomicIsize::new(0),
            has_xattrs: AtomicBool::new(false),
            ops,
            superblock,
            lifecycle: AtomicU8::new(STATE_LIVE),
        })
    }

    /// 返回该 Inode 的编号，无需获取任何锁。
    pub fn ino(&self) -> u64 {
        self.id.ino
    }

    /// 返回该 Inode 的文件类型，无需获取任何锁。
    pub fn kind(&self) -> FileType {
        self.kind
    }

    /// 是否可能携带扩展属性（快速路径提示；`false` 保证无 xattr）。
    pub fn has_xattrs(&self) -> bool {
        self.has_xattrs.load(Ordering::Acquire)
    }

    /// 标记该 inode 可能携带扩展属性（驱动在装载/写入时调用）。
    pub fn mark_has_xattrs(&self) {
        self.has_xattrs.store(true, Ordering::Release);
    }

    /// 清除 xattr 存在标记（驱动在删除最后一个属性后调用）。
    pub(crate) fn clear_has_xattrs(&self) {
        self.has_xattrs.store(false, Ordering::Release);
    }

    /// 返回可跨打开文件描述复用、且不会因对象地址复用而冲突的缓存身份。
    pub(crate) fn private_page_cache_key(&self) -> Option<usize> {
        (self.private_page_cache_id != 0).then_some(self.private_page_cache_id)
    }

    /// 返回当前文件大小的无锁快照。
    pub fn size(&self) -> u64 {
        self.cached_size.load(Ordering::Acquire)
    }

    /// 该文件是否属于内存文件系统（tmpfs / anonfs，Linux 统称 shmem）。
    ///
    /// `MADV_REMOVE` 只对这类文件生效；普通文件映射返回 `EINVAL`。anonfs 上的
    /// memfd 与 tmpfs 同为 Linux shmem 语义，因此一并计入。
    pub fn is_shmem_fs(&self) -> bool {
        matches!(
            self.superblock.upgrade().map(|sb| sb.fs_type),
            Some("tmpfs" | "anonfs")
        )
    }

    /// 返回当前硬链接计数的无锁快照。
    pub fn nlink(&self) -> u32 {
        self.cached_nlink.load(Ordering::Acquire)
    }

    /// 返回当前文件内容代际的无锁快照。
    pub fn data_generation(&self) -> u64 {
        data_state_generation(self.data_state.load(Ordering::Acquire))
    }

    /// 返回可用于私有干净页缓存的稳定代际。
    ///
    /// 代际、active 和禁用标志来自同一个原子状态字；VM 在读取文件页之后还会
    /// 再次验证同一状态，形成完整的乐观快照协议。
    pub(crate) fn private_page_cache_generation(&self) -> Option<u64> {
        if self.private_page_cache_id == 0 {
            return None;
        }
        let state = self.data_state.load(Ordering::Acquire);
        (!data_state_disabled(state) && data_state_active(state) == 0)
            .then(|| data_state_generation(state))
    }

    fn begin_data_mutation_raw(&self) -> bool {
        let previous = self
            .data_state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                let active = data_state_active(state);
                if active == DATA_MUTATION_MASK as usize {
                    return Some(state | DATA_CACHE_DISABLED);
                }
                Some(data_state(
                    data_state_generation(state),
                    active + 1,
                    data_state_disabled(state),
                ))
            })
            .expect("data state update closure must always produce a value");
        data_state_active(previous) != DATA_MUTATION_MASK as usize
    }

    fn end_data_mutation_raw(&self) {
        // 必须先推进代际再撤销最后一个 active，防止读者观察到“稳定的旧代际”。
        let result = self.data_state.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            finish_data_mutation_state,
        );
        assert!(result.is_ok(), "[vfs] data mutation publication underflow");
    }

    fn private_page_cache_supported(&self) -> bool {
        self.private_page_cache_id != 0
    }

    /// 在可能修改文件内容的 VFS 调用前建立发布 guard。失败路径也会推进代际，
    /// 因为文件系统错误可能发生在已经写入部分块之后。
    pub(crate) fn begin_data_mutation(&self) -> InodeDataMutation<'_> {
        let active = self.private_page_cache_supported() && self.begin_data_mutation_raw();
        InodeDataMutation {
            inode: self,
            active,
        }
    }

    /// 在可写共享映射生效前永久关闭私有干净页缓存。禁用标志与新代际在同一次
    /// 原子更新中发布，与 VM 发布候选页前的二次 generation 检查共同封闭并发窗口。
    pub(crate) fn disable_private_page_cache(&self) {
        if !self.private_page_cache_supported() {
            return;
        }
        let _ = self
            .data_state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                if data_state_disabled(state) {
                    return None;
                }
                let generation = data_state_generation(state)
                    .saturating_add(1)
                    .min(DATA_GENERATION_MAX);
                Some(data_state(generation, data_state_active(state), true))
            });
    }

    /// 获取普通文件写访问租约。
    ///
    /// 当该 inode 正被任一执行映像占用时返回 `ETXTBSY`。同类写访问可以并存，
    /// 租约析构时自动递减计数，因此 `dup`/`fork` 共享同一个 `File` 时不会重复计数。
    pub(crate) fn acquire_write_access(self: &Arc<Self>) -> VfsResult<InodeWriteAccess> {
        let mut current = self.exec_write_state.load(Ordering::Acquire);
        loop {
            if current < 0 {
                return Err(VfsError::TextFileBusy);
            }
            let next = current
                .checked_add(1)
                .ok_or(VfsError::TooManyOpenFilesSystem)?;
            match self.exec_write_state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(InodeWriteAccess {
                        inode: Arc::clone(self),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// 获取执行映像租约。
    ///
    /// 当该 inode 已有写打开时返回 `ETXTBSY`。多个由 `fork` 或重复 `exec` 产生的
    /// 执行映像可以同时持有租约；最后一个租约释放前，新的写打开和路径截断均被拒绝。
    pub fn acquire_exec_access(self: &Arc<Self>) -> VfsResult<InodeExecAccess> {
        let mut current = self.exec_write_state.load(Ordering::Acquire);
        loop {
            if current > 0 {
                return Err(VfsError::TextFileBusy);
            }
            let next = current
                .checked_sub(1)
                .ok_or(VfsError::TooManyOpenFilesSystem)?;
            match self.exec_write_state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(InodeExecAccess {
                        inode: Arc::clone(self),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// 返回当前元数据的一致性快照。
    pub(crate) fn meta_snapshot(&self) -> InodeMeta {
        *self.meta.lock()
    }

    /// 在持有元数据锁的情况下修改元数据，并在退出前同步热点字段镜像。
    #[allow(dead_code)]
    pub(crate) fn with_meta_mut<R>(&self, f: impl FnOnce(&mut InodeMeta) -> R) -> R {
        let mut meta = self.meta.lock();
        let result = f(&mut meta);
        self.sync_meta_hot_fields(&meta);
        result
    }

    /// 文件系统驱动完成磁盘 inode 写回后，用精确的底层元数据刷新 VFS 镜像。
    ///
    /// 与 `set_times`/`set_mode` 这类 VFS 语义入口不同，本接口不推导 ctime，也不做
    /// 权限检查；调用方必须已经完成对应的文件系统写操作。
    pub fn refresh_meta_from_fs(&self, new_meta: InodeMeta) {
        let mut meta = self.meta.lock();
        *meta = new_meta;
        self.sync_meta_hot_fields(&meta);
    }

    fn sync_meta_hot_fields(&self, meta: &InodeMeta) {
        self.cached_size.store(meta.size, Ordering::Release);
        self.cached_nlink.store(meta.nlink, Ordering::Release);
    }

    /// 构造一个 FileStat 快照，对应 stat(2) 系统调用的返回值。
    ///
    /// `st_dev` 已在 Inode 创建时缓存到只读字段中，因此这里无需再访问 Superblock。
    /// 这样既减少了一次 `Weak::upgrade()` 的原子开销，也进一步缩短了 `stat` 热路径
    /// 上的依赖链。
    pub fn stat(&self) -> VfsResult<FileStat> {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::VfsStat);
        let m = self.meta_snapshot();
        let size = i64::try_from(m.size).map_err(|_| VfsError::FileTooLarge)?;
        Ok(FileStat {
            dev: self.stat_dev,
            ino: self.id.ino,
            mode: self.kind.to_mode_bits() | m.mode.raw() as u32,
            nlink: m.nlink,
            uid: m.uid.0,
            gid: m.gid.0,
            rdev: self.rdev,
            size,
            blksize: self.blksize,
            blocks: m.blocks,
            atime: m.atime,
            mtime: m.mtime,
            ctime: m.ctime,
        })
    }

    /// 返回该 Inode 所属的文件系统实例标识符，无需获取任何锁。主要用于 VFS 层
    /// 在执行 rename 和 link 操作前检查源和目标是否位于同一文件系统。
    pub fn fs_id(&self) -> FsId {
        self.id.fs_id
    }

    /// 尝试将 InodeOps trait object 向下转型为具体的文件系统驱动类型。
    ///
    /// 当文件系统驱动的内部辅助函数需要访问驱动私有数据时，可以通过此方法从
    /// 通用的 Inode 引用中恢复出具体类型。如果类型不匹配则返回 None，调用方
    /// 应当优雅地处理这种情况而不是 panic。
    pub fn downcast_ops<T: InodeOps + 'static>(&self) -> Option<&T> {
        self.ops.as_any().downcast_ref::<T>()
    }

    /// 获取所属超级块的强引用（如果仍然存活）。
    pub fn superblock(&self) -> Option<Arc<Superblock>> {
        self.superblock.upgrade()
    }

    /// 设置文件大小。
    pub fn set_size(&self, new_size: u64) {
        let mut meta = self.meta.lock();
        meta.size = new_size;
        self.cached_size.store(new_size, Ordering::Release);
    }

    /// 同时设置文件大小和块数，避免文件系统在常规写入/截断路径中拆开更新。
    pub fn set_size_and_blocks(&self, new_size: u64, blocks: u64) {
        let mut meta = self.meta.lock();
        meta.size = new_size;
        meta.blocks = blocks;
        self.cached_size.store(new_size, Ordering::Release);
    }

    /// 同时发布常规写入后的大小、块数与修改时间。
    ///
    /// 文件系统已经完成数据写入后使用此入口，可把原本分散的三次元数据加锁和
    /// 两次时钟读取合并为一次。`mtime` 与 `ctime` 取同一个时间点，`atime` 以及
    /// 所有权、权限和链接计数保持不变。
    pub fn set_size_blocks_and_modified(&self, new_size: u64, blocks: u64) {
        let mut meta = self.meta.lock();
        let now = Timespec::now();
        meta.size = new_size;
        meta.blocks = blocks;
        meta.mtime = now;
        meta.ctime = now;
        self.cached_size.store(new_size, Ordering::Release);
    }

    /// 设置硬链接计数。
    pub fn set_nlink(&self, new_nlink: u32) {
        let mut meta = self.meta.lock();
        meta.nlink = new_nlink;
        self.cached_nlink.store(new_nlink, Ordering::Release);
    }

    /// 设置权限位并更新 ctime。
    pub fn set_mode(&self, mode: FileMode) {
        let mut meta = self.meta.lock();
        meta.mode = mode;
        meta.ctime = Timespec::now();
    }

    /// 设置所有者/所属组并更新 ctime；`None` 表示保持原值。
    pub fn set_owner(&self, uid: Option<Uid>, gid: Option<Gid>) {
        let mut meta = self.meta.lock();
        if let Some(uid) = uid {
            meta.uid = uid;
        }
        if let Some(gid) = gid {
            meta.gid = gid;
        }
        meta.ctime = Timespec::now();
    }

    /// 设置访问/修改时间并更新 ctime；`None` 表示保持原值。
    pub fn set_times(&self, atime: Option<Timespec>, mtime: Option<Timespec>) {
        let mut meta = self.meta.lock();
        if let Some(atime) = atime {
            meta.atime = atime;
        }
        if let Some(mtime) = mtime {
            meta.mtime = mtime;
        }
        meta.ctime = Timespec::now();
    }

    /// 增加硬链接计数。
    pub fn inc_nlink(&self) {
        let mut meta = self.meta.lock();
        meta.nlink = meta.nlink.saturating_add(1);
        self.cached_nlink.store(meta.nlink, Ordering::Release);
    }

    /// 减少硬链接计数。
    pub fn dec_nlink(&self) {
        let mut meta = self.meta.lock();
        meta.nlink = meta.nlink.saturating_sub(1);
        self.cached_nlink.store(meta.nlink, Ordering::Release);
    }

    /// 更新访问时间为当前时间。
    pub fn touch_atime(&self) {
        self.meta.lock().atime = Timespec::now();
    }

    /// 更新修改时间为当前时间。
    pub fn touch_mtime(&self) {
        self.meta.lock().mtime = Timespec::now();
    }

    /// 更新状态变更时间为当前时间。
    pub fn touch_ctime(&self) {
        self.meta.lock().ctime = Timespec::now();
    }

    /// 在此目录 Inode 中创建一个子目录，委托给底层 `InodeOps::mkdir`。
    pub fn mkdir(
        &self,
        name: &str,
        mode: crate::stat::FileMode,
        cred: &crate::cred::Credentials,
    ) -> VfsResult<Arc<Inode>> {
        self.ops.mkdir(self, name, mode, cred)
    }

    /// 在此目录 Inode 中创建一个普通文件，委托给底层 `InodeOps::create`。
    pub fn create(
        &self,
        name: &str,
        mode: crate::stat::FileMode,
        cred: &crate::cred::Credentials,
    ) -> VfsResult<Arc<Inode>> {
        self.ops.create(self, name, mode, cred)
    }

    /// 从目录中删除 `child` 对应的 `name` 条目，委托给底层 `InodeOps::unlink`。
    pub fn unlink(&self, name: &str, child: &Inode) -> VfsResult<()> {
        self.ops.unlink(self, name, child)
    }

    /// 在此目录 Inode 中按名称查找子项，委托给底层 `InodeOps::lookup`。
    pub fn lookup(&self, name: &str) -> VfsResult<Arc<Inode>> {
        self.ops.lookup(self, name)
    }

    /// 打开此 Inode,返回底层文件句柄(`FileOps`)。委托给 `InodeOps::open`。
    ///
    /// 高层 VFS 文件表一般通过 `mount + dentry + File` 组合暴露;此方法用于
    /// 启动期 bench/tests 这样不需要完整 `File` 封装的场景。
    pub fn open_ops(
        &self,
        opts: &crate::file::OpenOptions,
        cred: &crate::cred::Credentials,
    ) -> VfsResult<alloc::boxed::Box<dyn crate::file::FileOps + Send + Sync>> {
        self.ops.open(self, opts, cred)
    }

    /// 若该 Inode 已从目录树中摘除且无剩余硬链接，则将其转入"待回收"状态。
    ///
    /// 成功转入 ORPHANED 后会立即从所属超级块的 inode cache 中移除，确保后续
    /// lookup 不会再重用该对象；底层资源回收延迟到 `Drop`。
    pub(crate) fn retire_if_unlinked(&self) -> bool {
        if self.nlink() != 0 {
            return false;
        }
        if self
            .lifecycle
            .compare_exchange(
                STATE_LIVE,
                STATE_ORPHANED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        if let Some(sb) = self.superblock.upgrade() {
            sb.remove_inode(self.id.ino);
        }
        true
    }
}

/// 普通文件写访问租约，只能由 VFS 打开和路径截断流程创建。
pub(crate) struct InodeWriteAccess {
    inode: Arc<Inode>,
}

impl Drop for InodeWriteAccess {
    fn drop(&mut self) {
        let previous = self.inode.exec_write_state.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "write access state must be positive");
    }
}

/// 执行映像租约。
///
/// 装载器必须在 ELF 校验和复制前获取该租约，并将其保存到任务执行状态；这样文件
/// 从 `execve` 成功直至最后一个相关任务退出期间都不能被写打开或截断。
pub struct InodeExecAccess {
    inode: Arc<Inode>,
}

impl Drop for InodeExecAccess {
    fn drop(&mut self) {
        let previous = self.inode.exec_write_state.fetch_add(1, Ordering::AcqRel);
        debug_assert!(previous < 0, "exec access state must be negative");
    }
}

impl Drop for Inode {
    fn drop(&mut self) {
        if self.lifecycle.load(Ordering::Acquire) == STATE_ORPHANED {
            debug_assert_eq!(
                self.cached_nlink.load(Ordering::Acquire),
                0,
                "orphaned inode must have nlink == 0 before evict"
            );
            self.lifecycle.store(STATE_EVICTED, Ordering::Release);
            self.ops.evict(self);
        }
    }
}

/// Inode 操作接口，对应 Linux 内核中的 struct inode_operations。
///
/// 每种文件系统驱动为其管理的 Inode 提供一份该 trait 的实现。VFS 层在路径解析、
/// 文件创建、删除、重命名等操作中，通过 Inode 持有的 trait object 调用这些方法，
/// 实现对底层文件系统的透明访问。驱动只需要关注自己的存储逻辑，不需要了解 VFS
/// 层的缓存管理、权限检查和挂载点穿越等细节。
///
/// 所有方法的第一个参数 inode 是当前操作所针对的 Inode 的引用（对于目录操作，
/// 这是父目录的 Inode）。参数类型为 &Inode 而非 &Arc<Inode>，这是一个有意的
/// 设计选择：传递裸引用可以防止驱动随意克隆 Arc 导致引用计数管理混乱，同时与
/// evict 方法的签名保持一致。如果驱动确实需要持有 Inode 的长期引用（极少见），
/// 应当通过其他机制（如 Superblock 的 inode_cache）获取。
///
/// 权限检查由 VFS 层在调用 InodeOps 方法之前统一完成，驱动无需重复检查调用方
/// 是否有权执行该操作。修改类操作（create、mkdir、unlink、rmdir 等）提供了返回
/// NotSupported 错误的默认实现，只读文件系统只需实现 lookup、readlink 和 open
/// 等读取类方法即可。
pub trait InodeOps {
    /// 是否保证普通文件内容的每次变化都经由 VFS 数据代际发布。
    ///
    /// 默认关闭，避免 procfs、sysfs 和设备文件等动态内容被跨地址空间复用。
    /// 只有内容变化完全受 VFS write/truncate/fallocate 路径约束的文件系统才能开启。
    fn supports_private_page_cache(&self) -> bool {
        false
    }

    /// 在当前目录中按名称查找子项。
    ///
    /// name 是单个路径分量（不含路径分隔符），例如 "etc" 或 "passwd"。如果目录中
    /// 存在该名称，返回对应的 Inode；如果不存在，返回 NotFound 错误。VFS 层会将
    /// 返回的 Inode 包装为 Dentry 并插入缓存，后续对同一名称的查找将直接命中缓存
    /// 而不再调用此方法。
    fn lookup(&self, inode: &Inode, name: &str) -> VfsResult<Arc<Inode>>;

    /// 在当前目录中创建一个新的普通文件。
    ///
    /// mode 是经过 umask 掩码处理后的最终权限位，cred 是发起操作的进程凭据（驱动
    /// 应当用 cred 中的 fsuid 和 fsgid 设置新文件的所有者和所属组）。如果目录中已
    /// 存在同名条目，驱动应当返回 AlreadyExists 错误。
    fn create(
        &self,
        _inode: &Inode,
        _name: &str,
        _mode: FileMode,
        _cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    /// 在当前目录中创建一个新的子目录。
    ///
    /// 语义与 create 类似，但创建的是目录类型的 Inode。驱动需要在新目录中初始化
    /// "." 和 ".." 两个特殊条目，并将父目录的 nlink 加一（因为子目录的 ".." 构成
    /// 了对父目录的一个硬链接）。
    fn mkdir(
        &self,
        _inode: &Inode,
        _name: &str,
        _mode: FileMode,
        _cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    /// 从当前目录中删除指定的非目录文件。
    ///
    /// child 是被删除文件的 Inode 引用，由 VFS 层通过 lookup 获得后传入，驱动无需
    /// 再次查找。驱动应当将 child 的 nlink 减一，并从目录的数据结构中移除该条目。
    /// 如果 nlink 降为零，VFS 层会在此方法返回后将该 inode 标记为待回收，并在最后
    /// 一个强引用释放时触发 `evict`。
    fn unlink(&self, _inode: &Inode, _name: &str, _child: &Inode) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// 从当前目录中删除指定的空子目录。
    ///
    /// 如果子目录非空（包含除 "." 和 ".." 之外的条目），驱动应当返回
    /// DirectoryNotEmpty 错误。删除成功后，驱动需要将父目录的 nlink 减一
    /// （因为子目录的 ".." 不再指向父目录）。
    fn rmdir(&self, _inode: &Inode, _name: &str, _child: &Inode) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// 在当前目录中创建一个符号链接。
    ///
    /// target 是符号链接指向的目标路径字符串，可以是绝对路径也可以是相对路径，
    /// 甚至可以指向一个不存在的路径（符号链接的目标在创建时不做任何验证，只在
    /// 解析时才检查）。
    fn symlink(
        &self,
        _inode: &Inode,
        _name: &str,
        _target: &str,
        _cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    /// 在当前目录中创建一个硬链接，使新的目录项 name 指向已存在的 target Inode。
    ///
    /// VFS 层在调用此方法前已经验证了以下前置条件：target 不是目录（对目录创建
    /// 硬链接会破坏目录树的有向无环图结构，导致路径解析无限递归）；target 与当前
    /// 目录位于同一文件系统（跨文件系统的硬链接在物理上不可能实现）；调用方满足
    /// protected_hardlinks 安全规则（非特权用户只能给自己拥有的、且未设置 setuid
    /// 或 setgid 位的文件创建硬链接）。驱动只需将 target 的 nlink 加一并在目录
    /// 数据结构中添加新条目。
    fn link(&self, _inode: &Inode, _target: &Inode, _name: &str) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// 在当前目录中创建一个特殊文件节点。
    ///
    /// kind 指定要创建的文件类型（字符设备、块设备、FIFO 管道或 Unix 域套接字），
    /// dev 是设备号（仅对字符设备和块设备有意义）。创建设备节点通常需要 CAP_MKNOD
    /// 能力，这个检查由 VFS 层在调用前完成。
    fn mknod(
        &self,
        _inode: &Inode,
        _name: &str,
        _kind: FileType,
        _mode: FileMode,
        _dev: DevId,
        _cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    /// 将当前目录下的 old_name 重命名为 new_name，目标可能位于另一个目录 new_dir 中。
    ///
    /// old_inode 是被移动条目的 Inode 引用，驱动可以用它来检查类型（例如目录的
    /// 重命名需要额外更新 ".." 条目的指向）和调整 nlink 计数。new_dir 可以与当前
    /// 目录相同（同目录内重命名）或不同（跨目录移动），但 VFS 层保证两者位于同一
    /// 文件系统。如果目标位置已存在同名条目，驱动应当原子地替换它。
    fn rename(
        &self,
        _inode: &Inode,
        _old_name: &str,
        _old_inode: &Inode,
        _new_dir: &Inode,
        _new_name: &str,
    ) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// 读取符号链接的目标路径字符串。
    ///
    /// 仅对文件类型为 Symlink 的 Inode 有意义。默认实现返回 InvalidArgument 错误，
    /// 适用于不支持符号链接的文件系统（如 FAT）。
    fn readlink(&self, _inode: &Inode) -> VfsResult<alloc::string::String> {
        Err(VfsError::InvalidArgument)
    }

    /// 打开文件，返回文件系统驱动提供的 FileOps 实现。
    ///
    /// VFS 层在完成权限检查后调用此方法。驱动在此分配文件打开所需的内部状态
    /// （如读写缓冲区、设备驱动句柄、目录遍历游标等），打包为 Box<dyn FileOps>
    /// 返回。VFS 层负责构造 File 对象并填充 inode、flags、cred、pos 等字段，
    /// 驱动不需要也不应该接触这些 VFS 层管理的状态。
    fn open(
        &self,
        inode: &Inode,
        opts: &OpenOptions,
        cred: &Credentials,
    ) -> VfsResult<Box<dyn crate::vfs::file::FileOps + Send + Sync>>;

    /// 修改文件的访问权限位。VFS 层在调用前已验证调用方是文件所有者或持有
    /// CAP_FOWNER 能力。
    fn chmod(&self, _inode: &Inode, _mode: FileMode) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// 修改文件的所有者和所属组。uid 或 gid 为 None 表示不修改对应字段。VFS 层
    /// 在调用前已验证调用方持有 CAP_CHOWN 能力或满足 POSIX 规定的其他条件。
    fn chown(&self, _inode: &Inode, _uid: Option<Uid>, _gid: Option<Gid>) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// 修改文件大小。如果新大小小于当前大小，文件被截断，多余的数据块被释放；
    /// 如果新大小大于当前大小，文件被扩展，扩展部分形成稀疏空洞（读取时返回
    /// 全零，不占用实际磁盘空间）。
    fn truncate(&self, _inode: &Inode, _size: u64) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// 设置文件的访问时间和修改时间。atime 或 mtime 为 None 表示不修改对应字段。
    /// 对应 utimes(2) 和 futimens(2) 系统调用。
    fn utimes(
        &self,
        _inode: &Inode,
        _atime: Option<Timespec>,
        _mtime: Option<Timespec>,
    ) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// 当 Inode 已被 VFS 标记为待回收且最后一个强引用释放时调用。
    ///
    /// 驱动应当在此方法中释放该 Inode 占用的所有磁盘资源，包括数据块、间接块、
    /// 扩展属性块以及 inode 位图中的对应位。对于纯内存文件系统（如 tmpfs），由于
    /// 没有磁盘资源需要释放，默认的空实现即可。
    fn evict(&self, _inode: &Inode) {}

    /// 将该 Inode 的脏元数据（权限位、时间戳、大小等）从内存刷写到底层持久存储。
    /// 对于纯内存文件系统，默认的空实现即可。
    fn sync_metadata(&self, _inode: &Inode) -> VfsResult<()> {
        Ok(())
    }

    /// 读取扩展属性；`None` 表示属性不存在（`ENODATA`）。
    /// 默认实现表示该文件系统不支持 xattr（`EOPNOTSUPP`）。
    fn getxattr(&self, _name: &[u8]) -> VfsResult<Option<Vec<u8>>> {
        Err(VfsError::NotSupported)
    }

    /// 设置扩展属性；`flags` 为 `XATTR_CREATE`/`XATTR_REPLACE`。
    fn setxattr(&self, _name: &[u8], _value: &[u8], _flags: u32) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// 列出全部扩展属性名。
    fn listxattr(&self) -> VfsResult<Vec<Vec<u8>>> {
        Err(VfsError::NotSupported)
    }

    /// 删除扩展属性；属性不存在返回 `ENODATA`。
    fn removexattr(&self, _name: &[u8]) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// 返回 &dyn Any 引用，用于支持从 trait object 向下转型到具体的驱动类型。
    /// 实现者只需写 fn as_any(&self) -> &dyn Any { self } 即可。
    fn as_any(&self) -> &dyn core::any::Any;
}

#[cfg(test)]
mod data_state_tests {
    use super::{
        DATA_GENERATION_MAX, data_state, data_state_active, data_state_disabled,
        data_state_generation, finish_data_mutation_state,
    };

    #[test]
    fn data_mutation_finish_advances_generation_before_last_active_clears() {
        let state = data_state(7, 1, false);
        let next = finish_data_mutation_state(state).expect("活跃 mutation 必须能结束");

        assert_eq!(data_state_generation(next), 8);
        assert_eq!(data_state_active(next), 0);
        assert!(!data_state_disabled(next));
    }

    #[test]
    fn data_generation_saturation_permanently_disables_private_cache() {
        let state = data_state(DATA_GENERATION_MAX, 1, false);
        let next = finish_data_mutation_state(state).expect("最后一个 mutation 必须能结束");

        assert_eq!(data_state_generation(next), DATA_GENERATION_MAX);
        assert_eq!(data_state_active(next), 0);
        assert!(data_state_disabled(next));
    }
}

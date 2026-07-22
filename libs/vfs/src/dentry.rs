//! 本模块定义了 VFS 层的 Dentry 与 DentryCache 抽象，它们共同承担“文件名解析”
//! 这一职责。与表示文件系统对象本体的 Inode 不同，Dentry 代表的是命名空间中的
//! 一个位置：它把“某个父目录下的某个名字”与最终的 Inode 关联起来，从而使路径
//! `/usr/bin/sh` 这类用户可见名称能够被逐分量解析为底层对象。
//!
//! Dentry 的核心意义在于把“名字”与“对象”分离。Inode 描述的是文件本体，多个
//! 硬链接可以共享同一个 Inode；而 Dentry 描述的是命名空间中的一条命名关系，
//! 它天然依赖父目录和名字，因此同一个 Inode 可以被多个不同的 Dentry 指向。
//! VFS 在做路径遍历时，真正高频访问的是“父目录 + 名称 -> 下一个节点”这一映射，
//! 因而 Dentry 缓存会直接决定 `open`、`stat`、`execve` 等系统调用的热路径性能。
//!
//! 在缓存策略方面，本模块同时支持正向 Dentry 和负向 Dentry。正向 Dentry 表示
//! 某个名字当前存在并指向一个 Inode；负向 Dentry 则记录“这个名字在该父目录下
//! 当前不存在”。负向缓存对于大量失败查找非常重要，例如 shell 反复探测命令路径、
//! 动态链接器尝试多个候选库路径，或者应用程序轮询一个尚未创建的临时文件。如果
//! 没有负向 Dentry，这些失败查找每次都必须再次进入底层文件系统驱动甚至触发磁盘
//! I/O，代价很高。与此同时，负向 Dentry 不能永久可信：一旦父目录发生创建类写入，
//! 旧的“不存在”结论就可能失效，因此调用方必须在适当时机将相关缓存逐出。
//!
//! 在所有权管理方面，Dentry 通过 `Arc<Dentry>` 形成一棵内存中的目录树。子节点
//! 持有父节点的强引用，使得只要某个路径下游节点仍在使用，其祖先目录就不会被过早
//! 释放；而根节点的 `parent` 明确为 `None`，避免通过自引用制造永久性的引用环。
//! 进程可见根的约束由 `VfsContext::root` 维护，因此“到达根目录时停止向上回溯”
//! 是路径解析逻辑的职责，而不是通过特殊的自环 Dentry 来隐式表达。
//!
//! 在并发设计方面，本模块明确区分“定位信息”和“存在性状态”两类数据。名称和父
//! 目录会在 `rename` 时发生变化，因此被封装到 `DentryMeta` 中并受 `Spinlock`
//! 保护；正向、负向、失效三种状态则通过 `AtomicU8` 表示，使最常见的状态判断可以
//! 在无锁条件下完成。这样的拆分遵循一个很重要的原则：路径解析热路径上最频繁的
//! 操作应尽量避免拿锁，而只有真正修改命名空间结构的慢路径才进入临界区。
//!
//! 在缓存组织方面，`DentryCache` 使用分片哈希表而不是单一全局映射。路径解析往往
//! 是多核系统中最常见的共享读操作之一，如果所有查找都争用同一把锁，那么扩展性会
//! 很快恶化。本实现按 `(parent_ptr, name)` 选择分片，把并发访问拆散到多个独立
//! 锁上，使得不同目录、不同名字的查找大概率可以并行执行。缓存中的键并不直接保存
//! 父节点的 `Arc`，而是保存父 Dentry 的地址和值语义名称；与此同时，子 Dentry 的
//! `parent` 字段继续强持有父节点，从而保证缓存不会因为地址复用而返回悬垂条目。
//!
//! 在生命周期语义方面，DentryCache 以强引用持有缓存项。这意味着”被缓存”本身
//! 就足以让一个 Dentry 继续存活，因此缓存淘汰必须是显式的：文件被删除、目录被
//! 重命名、挂载被卸载时，都需要主动逐出对应条目。当前实现不再包含全局 GC 机制，
//! 但提供了三类显式维护手段：
//! 1. 单键失效：删除单个名称映射；
//! 2. 子树失效：目录删除、目录覆盖和卸载时批量逐出整棵缓存子树；
//! 3. 有界缓存：对负向项和总条目数都施加分片上限，压力下优先逐出冷条目或直接跳过缓存。
//!
//! 这使得 dcache 仍然是“加速层”而不是“命名空间真相来源”：当缓存预算耗尽时，
//! 新条目可以选择不缓存，但不会影响命名空间语义的正确性。

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use crate::vfs::inode::Inode;
use crate::vfs::mount::Mount;
use crate::vfs::sync::Spinlock;

/// 父链遍历允许的最大深度，用于防御性环检测。
const MAX_PARENT_CHAIN_DEPTH: usize = 4096;

#[inline]
fn assert_valid_dentry_name(name: &str, is_root: bool) {
    if is_root {
        assert!(
            name.is_empty(),
            "root dentry must use empty name, got {:?}",
            name
        );
        return;
    }

    assert!(!name.is_empty(), "non-root dentry name must not be empty");
    assert!(
        !name.contains('/'),
        "dentry name must be a single path component, got {:?}",
        name
    );
    assert!(
        !name.contains('\0'),
        "dentry name must not contain NUL byte"
    );
    assert!(
        name != "." && name != "..",
        "reserved path component {:?} must not be materialized as a normal dentry",
        name
    );
}

// ── SmallStr：短路径分量的零堆分配表示 ────────────────────────────────────────
//
// 路径解析中最频繁处理的数据之一就是“单个路径分量”。现实系统里的大多数名称都很短，
// 例如 "etc"、"tmp"、"bin"、"passwd"、"uart0" 等，长度通常远小于 16 字节。
// 若对这些短字符串一律使用 heap `String`，则每次创建 dentry 时都要承担堆分配、
// 元数据头部和缓存局部性变差的成本。SmallStr 的目的就是把这一类高频短名称内联到
// 结构体内部，在不改变调用方语义的前提下减少分配次数和内存碎片。
//
// 这里没有使用带手写位布局的 union 技巧，而是使用更符合 Rust 风格的 enum 表示：
// 名称足够短时走 `Inline` 分支，把字节直接放进固定大小数组；名称较长时退化为普通
// `String`。这种实现虽然在理论上的极限紧凑度略逊于手工位打包，但可读性更高，
// 语义更清晰，也更容易在 `no_std` 环境中保持正确性。

/// 短字符串优化类型：≤23 字节的路径分量存储在栈上，超出则退化为 `String`。
///
/// 路径分量（如 `"etc"`、`"passwd"`）绝大多数 ≤23 字节，使用此类型可显著减少
/// 高频路径解析场景下的堆分配次数。
///
/// 内存布局优化：enum tag (1) + len (1) + buf (23) + padding (1) = 32 字节，
/// 与 Heap 分支（tag + String 的 3×usize）对齐，无额外空间浪费。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SmallStr {
    /// 内联存储：名称 ≤ 23 字节时完全不进行堆分配。
    Inline { len: u8, buf: [u8; 23] },
    /// 堆存储：名称 > 23 字节时退化为 `String`。
    Heap(String),
}

impl SmallStr {
    /// 从 `&str` 构造 `SmallStr`，自动选择内联或堆存储。
    pub fn new(s: &str) -> Self {
        let bytes = s.as_bytes();
        if bytes.len() <= 23 {
            let mut buf = [0u8; 23];
            buf[..bytes.len()].copy_from_slice(bytes);
            SmallStr::Inline {
                len: bytes.len() as u8,
                buf,
            }
        } else {
            SmallStr::Heap(String::from(s))
        }
    }

    /// 返回字符串切片视图。
    pub fn as_str(&self) -> &str {
        match self {
            SmallStr::Inline { len, buf } => {
                // Safety: buf 中的字节由 SmallStr::new 从有效 &str 写入，保证 UTF-8；
                // len 在 new() 中由 bytes.len() as u8 赋值，不超过 INLINE_CAP=23。
                // 防御性地限制 len 到 23，避免内存损坏导致越界。
                let safe_len = (*len as usize).min(23);
                unsafe { core::str::from_utf8_unchecked(&buf[..safe_len]) }
            }
            SmallStr::Heap(s) => s.as_str(),
        }
    }

    /// 返回字节长度。
    pub fn len(&self) -> usize {
        match self {
            SmallStr::Inline { len, .. } => (*len as usize).min(23),
            SmallStr::Heap(s) => s.len(),
        }
    }

    /// 判断是否为空串。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl core::fmt::Display for SmallStr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for SmallStr {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl AsRef<str> for SmallStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl core::ops::Deref for SmallStr {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

// ── DentryMeta：受内部锁保护的可变字段 ────────────────────────────────────────
//
// Dentry 中只有“名字”和“父目录”这两个字段会在生命周期内变化，而且它们总是作为
// 一个整体变化：rename 既可能改名字，也可能改父目录，甚至两者同时改变。如果把它们
// 拆成两个独立字段分别修改，并发的 `full_path()` 或调试输出就可能观察到“新名字 +
// 旧父目录”或“旧名字 + 新父目录”这样的撕裂状态。DentryMeta 把这两个字段聚合到
// 同一把锁下，保证“命名空间中的位置”始终作为一个原子整体被读取和更新。

/// Dentry 的可变定位字段。
///
/// Dentry 表示”父目录中的一个名字”这一位置关系，因此 `name` 与 `parent` 并不是
/// 两个彼此独立的属性，而是同一命名位置的两个维度。只要发生 rename，这两个值就
/// 必须在同一临界区内一起更新，否则任何基于 parent 链的路径构造都会得到不一致的
/// 结果。把它们打包在 DentryMeta 中，可以让调用方直接以”更新定位信息”这一语义
/// 操作整个结构，而不必记住多个字段之间的耦合关系。
pub struct DentryMeta {
    /// 本节点的文件名（单个路径分量，不含 `'/'`）。
    ///
    /// 使用 [`SmallStr`] 避免短名称的堆分配。
    pub(crate) name: SmallStr,

    /// 父目录的强引用。
    ///
    /// 根 Dentry 的 `parent` 为 `None`。路径解析中遇到根目录的 `..` 时，
    /// 由 `VfsContext::root` 决定是否停止向上回溯，而不依赖此字段的自引用。
    /// 这样设计避免了根 Dentry 通过 Arc 自引用形成循环、导致内存永不释放的问题。
    pub(crate) parent: Option<Arc<Dentry>>,
}

impl DentryMeta {
    /// 克隆名称。
    ///
    /// 调用方通常在持有 `meta` 锁期间调用此方法，将结果带到锁外继续使用。
    #[inline]
    pub fn name_cloned(&self) -> SmallStr {
        self.name.clone()
    }

    /// 克隆父目录引用。
    ///
    /// 调用方通常在持有 `meta` 锁期间调用此方法，将结果带到锁外继续使用。
    #[inline]
    pub fn parent_cloned(&self) -> Option<Arc<Dentry>> {
        self.parent.clone()
    }
}

/// Dentry 状态标志。
///
/// Dentry 的“位置”与“状态”是两个正交概念。名字和父目录决定它在命名空间中的
/// 坐标，而状态决定这条坐标当前是正向存在、负向不存在，还是已经失效。这里使用
/// `AtomicU8` 而不是额外的锁，原因在于状态转换非常简单，而且具有明显的单向性：
/// 正向条目可以在删除时进入失效状态，负向条目可以在缓存淘汰时直接消失，但不会原地
/// 变成另一个完全不同的实体。对这类简单状态机，原子变量足以提供所需的可见性。
const STATE_POSITIVE: u8 = 0;
const STATE_NEGATIVE: u8 = 1;
const STATE_INVALID: u8 = 2;

/// 目录项，VFS 路径解析树中的单个节点。
///
/// 每个 Dentry 都表示“某个父目录下的某个名字”这一命名关系。它既不是纯粹的路径
/// 字符串，也不是文件对象本身，而是连接两者的中间层：路径解析时，VFS 会拿着当前
/// 目录的 Dentry 和下一段名字去查缓存或调驱动，得到下一个 Dentry，然后继续向下。
/// 因此，从根目录到目标文件的整条路径，实际上就是一串 Dentry 逐级相连形成的链。
///
/// 在状态建模方面，Dentry 本身并不需要频繁修改除定位信息之外的内容。与之相反，
/// 路径遍历极其频繁地需要回答一个简单问题：“这个名字当前是否可用？”为此，本实现
/// 将正向、负向、失效三种状态放到一个原子字段中，并让 `is_positive()` 成为无锁的
/// 热路径。Acquire 读取与 `invalidate()` 的 Release 写配对，保证删除一旦对并发
/// 线程可见，该线程就不会继续把该条目当成有效正向节点使用。
///
/// 在对象绑定方面，Dentry 内部持有一个不可变的 `Option<Arc<Inode>>`。正向 Dentry
/// 在构造时就绑定到目标 Inode，之后不再原地替换；负向 Dentry 的 inode 永远为
/// `None`。这种设计刻意避免了“同一个 Dentry 实例在生命周期中先指向 A，再指向 B”
/// 的可变别名问题，使得缓存键迁移和状态失效都能通过更简单、更局部的规则表达。
/// 当命名关系发生本质变化时，系统更倾向于创建新 Dentry、失效旧 Dentry 或迁移缓存
/// 键，而不是让一个现有节点在语义上变成另一个对象。
///
/// 在锁设计方面，单个 Dentry 内部只有一把 `meta` 锁，负责保护名字和父目录这两个
/// 位置字段。状态位和 inode 绑定都不受这把锁保护：前者依赖原子操作，后者在构造后
/// 不再变化。这样的划分让并发读者可以在绝大多数情况下只读原子位或克隆 Arc，不必
/// 为每一次查找都进入临界区，从而保持路径解析的可扩展性。
pub struct Dentry {
    /// 可变的定位字段（名称 + 父节点），受 `meta` 锁保护，支持原子重命名。
    pub meta: Spinlock<DentryMeta>,

    /// 对应的 Inode（正向 Dentry 为 `Some`，负向为 `None`），创建后不可变。
    inode: Option<Arc<Inode>>,

    /// 状态标志：0=Positive, 1=Negative, 2=Invalid。
    ///
    /// 使用 `AtomicU8` 替代 `Spinlock<DentryState>`：状态转换是单向的，
    /// `is_positive()` 只需 Acquire 原子读，消除热路径锁开销并提供失效可见性。
    state_flag: AtomicU8,
}

impl Dentry {
    /// 构造一个正向 Dentry（名称存在，对应给定 Inode）。
    pub fn new_positive(name: &str, parent: Option<Arc<Dentry>>, inode: Arc<Inode>) -> Arc<Self> {
        assert_valid_dentry_name(name, parent.is_none());
        Arc::new(Self {
            meta: Spinlock::new(DentryMeta {
                name: SmallStr::new(name),
                parent,
            }),
            inode: Some(inode),
            state_flag: AtomicU8::new(STATE_POSITIVE),
        })
    }

    /// 构造一个负向 Dentry（名称在父目录中不存在）。
    pub fn new_negative(name: &str, parent: Option<Arc<Dentry>>) -> Arc<Self> {
        assert_valid_dentry_name(name, parent.is_none());
        Arc::new(Self {
            meta: Spinlock::new(DentryMeta {
                name: SmallStr::new(name),
                parent,
            }),
            inode: None,
            state_flag: AtomicU8::new(STATE_NEGATIVE),
        })
    }

    /// 判断当前 Dentry 是否为正向（名称存在）。无锁热路径。
    #[inline]
    pub fn is_positive(&self) -> bool {
        self.state_flag.load(Ordering::Acquire) == STATE_POSITIVE
    }

    /// 判断当前 Dentry 是否为负向（名称不存在）。无锁热路径。
    #[inline]
    pub fn is_negative(&self) -> bool {
        self.state_flag.load(Ordering::Acquire) == STATE_NEGATIVE
    }

    /// 判断当前 Dentry 是否已失效（已被删除或替换）。无锁热路径。
    #[inline]
    pub fn is_invalid(&self) -> bool {
        self.state_flag.load(Ordering::Acquire) == STATE_INVALID
    }

    /// 返回正向 Dentry 对应的 Inode，若为负向或已失效则返回 `None`。
    ///
    /// 线性化点是开头的状态读取：若并发 `invalidate()` 发生在该读取之前，
    /// 此方法保证返回 `None`；若发生在之后，则当前调用可视为先于失效完成。
    pub fn inode(&self) -> Option<Arc<Inode>> {
        if self.state_flag.load(Ordering::Acquire) != STATE_POSITIVE {
            return None;
        }
        self.inode.as_ref().map(Arc::clone)
    }

    /// 该 Dentry 是否可以从缓存中安全驱逐。
    ///
    /// 对没有持久化后端的文件系统（tmpfs / ramfs / devtmpfs），驱逐 positive
    /// Dentry 等同于删除文件——Superblock 的 inode_cache 持有的是 Weak 引用，
    /// Dentry 是 Inode 的唯一强引用持有者。一旦驱逐，inode 被 drop，文件数据
    /// 永久丢失。是否能安全重建由具体文件系统显式声明，不能用
    /// `dev_id` 推断；后者只描述 `stat(2)` 的设备号来源。
    ///
    /// 正向项只有在缓存是唯一强引用时才可驱逐，避免让挂载点、当前
    /// 工作目录或正在路径遍历中的 Dentry 与重建后的新对象失联。
    pub fn is_evictable(self: &Arc<Self>) -> bool {
        if self.state_flag.load(Ordering::Acquire) != STATE_POSITIVE {
            return true;
        }
        if Arc::strong_count(self) != 1 {
            return false;
        }
        let Some(inode) = self.inode.as_ref() else {
            return true;
        };
        let Some(sb) = inode.superblock() else {
            return true;
        };
        sb.ops.can_evict_positive_dentry()
    }

    /// 将 Dentry 标记为失效（文件被删除时调用）。
    ///
    /// 不清除 `inode` 字段（它是不可变的），只修改状态标志。
    /// 后续 `is_positive()` 返回 `false`，`inode()` 返回 `None`。
    pub fn invalidate(&self) {
        self.state_flag.store(STATE_INVALID, Ordering::Release);
    }

    /// 判断 `self` 是否位于 `ancestor` 子树中（包含自身）。
    ///
    /// 若 parent 链损坏成环，返回 `false`，而不是无限循环。
    pub fn is_descendant_of(self: &Arc<Self>, ancestor: &Arc<Dentry>) -> bool {
        let mut current = Some(Arc::clone(self));
        let mut seen: Vec<usize> = Vec::new();

        for _ in 0..MAX_PARENT_CHAIN_DEPTH {
            let Some(node) = current else {
                return false;
            };
            if Arc::ptr_eq(&node, ancestor) {
                return true;
            }

            let ptr = Arc::as_ptr(&node) as usize;
            if seen.contains(&ptr) {
                return false;
            }
            seen.push(ptr);

            current = {
                let meta = node.meta.lock();
                meta.parent_cloned()
            };
        }

        false
    }

    /// 构造相对于 `visible_root` 的完整路径字符串。
    ///
    /// 若 `visible_root` 不是当前 dentry 的祖先，或 parent 链损坏成环，则返回 `None`。
    /// 调用方不得再退回到全局根路径，以免越过命名空间边界泄露不可见路径。
    pub fn full_path(self: &Arc<Self>, visible_root: &Arc<Dentry>) -> Option<String> {
        let mut components: Vec<String> = Vec::new();
        let mut current = Some(Arc::clone(self));
        let mut seen: Vec<usize> = Vec::new();

        for _ in 0..MAX_PARENT_CHAIN_DEPTH {
            let node = current?;
            let ptr = Arc::as_ptr(&node) as usize;
            if seen.contains(&ptr) {
                return None;
            }
            seen.push(ptr);

            if Arc::ptr_eq(&node, visible_root) {
                components.reverse();
                let mut path = String::with_capacity(1 + components.len() * 9);
                path.push('/');
                let mut first = true;
                for comp in components.iter() {
                    if comp.is_empty() {
                        continue;
                    }
                    if !first {
                        path.push('/');
                    }
                    path.push_str(comp);
                    first = false;
                }
                return Some(path);
            }

            let (name, parent) = {
                let meta = node.meta.lock();
                (meta.name.as_str().to_string(), meta.parent_cloned())
            };
            components.push(name);
            current = parent;
        }

        None
    }
}

// ── 分片 Dentry 缓存（Sharded DentryCache） ────────────────────────────────────
//
// 问题：单一全局 Spinlock<BTreeMap> 在多核 SMP 环境下是严重的扩展性瓶颈。
// 路径解析（如打开 `/usr/lib/libc.so`）是极高频操作：每解析一个路径分量就需要
// 抢夺这把锁进行 get 查询。当多个核心并发执行文件操作时，全局锁引发的 Cache Line
// Bouncing 会成为系统最大的性能瓶颈。
//
// 解决方案：将缓存分成 N_SHARDS 个独立分片，每个分片有自己的锁。
// 查询时根据 (parent_ptr, name) 的哈希值选择分片，不同分片的操作完全并行，
// 锁竞争降低至约 1/N_SHARDS。
//
// 分片数选择：
// - 太少（< 8）：效果不明显；
// - 太多（> 128）：冷启动时内存占用大，且 static 初始化困难；
// - 16 是实践中常用的折中值（Linux 的 dname_hash 表也是类似思路）。
//
// 哈希函数：使用 FNV-1a（Fowler–Noll–Vo）算法，`no_std` 友好，无外部依赖。
// FNV-1a 对短字符串（路径分量通常 < 16 字节）表现良好，分布均匀。
//
// 后续演进方向（当前未实现）：
// - 读多写少路径可进一步引入 Seqlock，使读操作完全无锁；
// - 最终演进为类 Linux 的 RCU dcache，实现读路径零锁开销。

/// 分片数量。必须是 2 的幂次以使位掩码选片高效。
const N_SHARDS: usize = 16;
const SHARD_MASK: usize = N_SHARDS - 1;

/// 单个缓存分片：开放寻址哈希表。
///
/// 参照 [`crate::dev::char::DtbPathIndex`] 的设计，使用线性探测法。
/// 键为 (parent_ptr, name)，值为 `Arc<Dentry>`。
/// 负载因子超过 75% 时自动扩容（容量翻倍）。
struct DentryShard {
    /// 桶数组。`None` 表示空桶。
    buckets: Vec<Option<DentryBucket>>,
    /// 当前已占用的桶数量。
    count: usize,
    /// 当前分片中的负向/失效条目数。
    non_positive_count: usize,
    /// 机会式驱逐的起始位置，避免每次都从桶 0 开始扫描。
    eviction_cursor: usize,
}

/// 哈希表桶条目。
struct DentryBucket {
    hash: usize,
    parent_ptr: usize,
    name: SmallStr,
    dentry: Arc<Dentry>,
}

/// 计算 (parent_ptr, name) 的完整 FNV-1a 哈希值。
/// 返回值的高位用于选择分片，低位用于选择桶，避免重复计算。
#[inline]
fn dentry_hash(parent_ptr: usize, name: &str) -> usize {
    const FNV_OFFSET: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let mut hash = FNV_OFFSET;
    for byte in parent_ptr.to_ne_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    for byte in name.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash as usize
}

/// 初始桶容量（每个分片）。
const INITIAL_CAPACITY: usize = 32;

/// 单个分片允许保留的负向/失效条目上限。
///
/// 负向 dentry 的收益主要来自“短时间内重复失败查找”，因此把它们做成有界缓存更合理：
/// 常见热点仍可命中，而随机探测不会无限放大内存占用。
const NON_POSITIVE_LIMIT_PER_SHARD: usize = 256;

/// 单个分片允许保留的总缓存条目上限。
const TOTAL_LIMIT_PER_SHARD: usize = 1024;

impl DentryShard {
    const fn new_empty() -> Self {
        // Vec::new() 是 const fn（零分配），首次 insert 时 grow() 会分配。
        Self {
            buckets: Vec::new(),
            count: 0,
            non_positive_count: 0,
            eviction_cursor: 0,
        }
    }

    #[inline]
    fn capacity(&self) -> usize {
        self.buckets.len()
    }

    #[inline]
    fn mask(&self) -> usize {
        debug_assert!(self.buckets.len().is_power_of_two());
        self.buckets.len() - 1
    }

    /// 查找 (parent_ptr, name) 对应的桶索引。
    /// 返回 `Some(idx)` 表示找到匹配条目，`None` 表示未找到。
    fn find_bucket(&self, hash: usize, parent_ptr: usize, name: &str) -> Option<usize> {
        if self.buckets.is_empty() {
            return None;
        }
        let start = hash & self.mask();
        for probe in 0..self.buckets.len() {
            let idx = (start + probe) & self.mask();
            match &self.buckets[idx] {
                None => return None,
                Some(b) => {
                    // 优化比较顺序：先比 hash（最快），再比 parent_ptr，最后比字符串
                    if b.hash == hash && b.parent_ptr == parent_ptr && b.name.as_str() == name {
                        return Some(idx);
                    }
                }
            }
        }
        None
    }

    /// 获取条目。
    fn get(&self, hash: usize, parent_ptr: usize, name: &str) -> Option<&Arc<Dentry>> {
        self.find_bucket(hash, parent_ptr, name)
            .map(|idx| &self.buckets[idx].as_ref().unwrap().dentry)
    }

    #[inline]
    fn is_non_positive(dentry: &Arc<Dentry>) -> bool {
        !dentry.is_positive()
    }

    fn evict_from_cursor<F>(&mut self, mut predicate: F) -> Option<Arc<Dentry>>
    where
        F: FnMut(&Arc<Dentry>) -> bool,
    {
        if self.buckets.is_empty() {
            return None;
        }

        let start = self.eviction_cursor & self.mask();
        for offset in 0..self.capacity() {
            let idx = (start + offset) & self.mask();
            let should_remove = self.buckets[idx]
                .as_ref()
                .is_some_and(|bucket| predicate(&bucket.dentry));
            if should_remove {
                self.eviction_cursor = (idx + 1) & self.mask();
                return Some(self.remove_at(idx));
            }
        }

        None
    }

    /// 插入条目。若键已存在则覆盖。返回是否是新插入（true）还是覆盖（false）。
    fn insert(
        &mut self,
        hash: usize,
        parent_ptr: usize,
        name: SmallStr,
        dentry: Arc<Dentry>,
    ) -> bool {
        // 空表或负载因子 > 75% 时扩容
        if self.buckets.is_empty() || (self.count + 1) * 4 > self.buckets.len() * 3 {
            self.grow();
        }

        let start = hash & self.mask();
        for probe in 0..self.capacity() {
            let idx = (start + probe) & self.mask();
            match &self.buckets[idx] {
                None => {
                    let is_non_positive = Self::is_non_positive(&dentry);
                    self.buckets[idx] = Some(DentryBucket {
                        hash,
                        parent_ptr,
                        name,
                        dentry,
                    });
                    self.count += 1;
                    if is_non_positive {
                        self.non_positive_count += 1;
                    }
                    return true; // 新插入
                }
                Some(b) if b.hash == hash && b.parent_ptr == parent_ptr && b.name == name => {
                    let old_non_positive = Self::is_non_positive(&b.dentry);
                    let new_non_positive = Self::is_non_positive(&dentry);
                    // 覆盖已存在的键，count 不变
                    self.buckets[idx] = Some(DentryBucket {
                        hash,
                        parent_ptr,
                        name,
                        dentry,
                    });
                    match (old_non_positive, new_non_positive) {
                        (true, false) => self.non_positive_count -= 1,
                        (false, true) => self.non_positive_count += 1,
                        _ => {}
                    }
                    return false; // 覆盖
                }
                _ => continue,
            }
        }
        // 如果到这里说明表已满（所有桶都被占用），这不应该发生，因为扩容逻辑会在负载因子 > 75% 时触发
        panic!("Hash table full after grow() - this should never happen");
    }

    /// 从指定桶位移除条目。使用"后移填充"（backward shift deletion）维护线性探测的正确性。
    fn remove_at(&mut self, idx: usize) -> Arc<Dentry> {
        let removed = self.buckets[idx].take().unwrap().dentry;
        self.count -= 1;
        if Self::is_non_positive(&removed) {
            self.non_positive_count -= 1;
        }

        // 后移填充：检查被删除位置之后的连续非空桶，若其自然位置不在环形区间
        // (empty, i] 内，则将其移到空位，然后继续处理新空位。
        let mut empty = idx;
        let mut i = (idx + 1) & self.mask();
        while self.buckets[i].is_some() {
            let natural = {
                let b = self.buckets[i].as_ref().unwrap();
                // 使用存储的 hash 字段，避免重新计算
                b.hash & self.mask()
            };
            // 判断 natural 是否在环形区间 (empty, i] 内
            let in_range = if empty < i {
                natural > empty && natural <= i
            } else {
                // 环形回绕：(empty, cap-1] ∪ [0, i]
                natural > empty || natural <= i
            };
            // 若 natural 不在 (empty, i] 内，说明它被 empty 处的空洞阻挡了，需要前移
            if !in_range {
                self.buckets[empty] = self.buckets[i].take();
                empty = i;
            }
            i = (i + 1) & self.mask();
        }

        removed
    }

    /// 移除条目。使用"后移填充"（backward shift deletion）维护线性探测的正确性。
    fn remove(&mut self, hash: usize, parent_ptr: usize, name: &str) -> Option<Arc<Dentry>> {
        let idx = self.find_bucket(hash, parent_ptr, name)?;
        Some(self.remove_at(idx))
    }

    /// 仅当键当前映射到 `expected` 时才移除。
    fn remove_if_matches(
        &mut self,
        hash: usize,
        parent_ptr: usize,
        name: &str,
        expected: &Arc<Dentry>,
    ) -> Option<Arc<Dentry>> {
        let idx = self.find_bucket(hash, parent_ptr, name)?;
        if Arc::ptr_eq(&self.buckets[idx].as_ref()?.dentry, expected) {
            self.remove(hash, parent_ptr, name)
        } else {
            None
        }
    }

    /// 机会式驱逐一个无效或负向条目。
    fn evict_one_non_positive(&mut self) -> Option<Arc<Dentry>> {
        if self.non_positive_count == 0 {
            return None;
        }
        self.evict_from_cursor(|dentry| !dentry.is_positive())
    }

    fn evict_one_any(&mut self) -> Option<Arc<Dentry>> {
        self.evict_one_non_positive()
            .or_else(|| self.evict_from_cursor(|d| d.is_evictable()))
    }

    /// 扩容：容量翻倍（空表时初始化为 INITIAL_CAPACITY），重新哈希所有条目。
    /// 对于大表（> 4K），改为 1.5 倍增长以减少内存浪费。
    fn grow(&mut self) {
        let new_cap = if self.buckets.is_empty() {
            INITIAL_CAPACITY
        } else {
            let current = self.buckets.len();
            if current > 4096 {
                // 大表：1.5 倍增长，然后向上取整到 2 的幂次
                // 例如：5000 → 7500 → 8192
                current + current / 2
            } else {
                // 小表：2 倍增长
                current * 2
            }
        };
        // 确保容量是 2 的幂次（用于位掩码优化）
        let new_cap = new_cap.next_power_of_two();
        assert!(
            new_cap.is_power_of_two(),
            "grow() must produce power-of-2 capacity"
        );

        let old_buckets = core::mem::replace(&mut self.buckets, {
            let mut v = Vec::with_capacity(new_cap);
            v.resize_with(new_cap, || None);
            v
        });
        self.count = 0;
        self.non_positive_count = 0;
        self.eviction_cursor = 0;
        for bucket in old_buckets.into_iter().flatten() {
            // 使用存储的 hash 字段，避免重新计算
            let _ = self.insert(bucket.hash, bucket.parent_ptr, bucket.name, bucket.dentry);
        }
    }
}

// ── 辅助宏：构造 N_SHARDS 个 Spinlock 数组 ────────────────────────────────────
//
// Rust 目前不允许在 const context 中构造含有 AtomicBool 的数组（除非手动
// 展开），故此处使用宏展开 16 个元素的数组初始化。

macro_rules! spinlock_array_16 {
    ($init:expr) => {
        [
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
            Spinlock::new($init),
        ]
    };
}

/// 分片 Dentry 缓存。
///
/// DentryCache 的职责不是“保存一组 Dentry”这么简单，而是为路径解析提供一个高并发、
/// 低开销的名称查找索引。它按 `(parent_ptr, name)` 查找子节点，使给定目录下的名字
/// 可以在不进入底层文件系统驱动的情况下直接命中缓存。由于这类查找在现代系统中极其
/// 高频，本实现避免使用单一全局锁，而是把缓存分成多个独立分片；这样不同目录、不同
/// 名称的大量并发访问通常会落到不同锁上，显著减轻争用。
///
/// 分片内部选择开放寻址哈希表而不是树结构，是因为这里的典型负载是“短键、高频读、
/// 少量写”，而开放寻址对这类场景更友好。键由父 Dentry 地址和名称共同组成，这与
/// Dentry 的语义完全一致：同名条目在不同目录下显然不是同一个命名关系；反过来，即使
/// 不同名字最终指向同一个 Inode，它们也应当在缓存中有各自独立的条目。
///
/// 这里把父节点放进键时只保存其地址，而不额外保存 `Arc<Dentry>`。这样做的目的并非
/// 炫技，而是避免缓存层再额外制造一层父子强引用，否则一个父目录只要曾经拥有过子项，
/// 就可能因为缓存索引而长期无法被逐出。地址键之所以仍然安全，是因为子 Dentry 的
/// `meta.parent` 本身就强持有父节点：只要缓存里还有任何孩子条目，父节点就仍然活着，
/// 不会先被释放再被新对象复用到同一地址。
///
/// 需要特别强调的是，DentryCache 的生命周期管理是显式的。缓存以强引用持有条目，
/// 因此”还在缓存里”本身就意味着”还活着”。这能减少重复分配，但也要求系统在
/// `unlink`、`rmdir`、`rename`、`umount` 时主动清理。当前实现不包含全局 GC 机制，
/// 但提供了 `invalidate_dentry()`、`invalidate_subtree()` 和 `rename_dentry()` 三类
/// 显式维护接口来保持缓存与命名空间的一致性。
pub struct DentryCache {
    shards: [Spinlock<DentryShard>; N_SHARDS],
    /// 全局条目计数器，用于快速实现 len() 和 is_empty()。
    total_count: AtomicUsize,
}

impl DentryCache {
    /// 构造一个空的分片 Dentry 缓存。
    pub const fn new() -> Self {
        Self {
            shards: spinlock_array_16!(DentryShard::new_empty()),
            total_count: AtomicUsize::new(0),
        }
    }

    /// 调整 rename 操作后的全局计数器。
    ///
    /// rename 正常情况下条目数不变（旧键删除、新键插入），但在以下异常情况下会变化：
    /// - `(true, false)`：新键插入成功，但旧键不在缓存中（可能已被并发删除）→ +1
    /// - `(false, true)`：新键覆盖了已有条目，旧键删除成功 → -1
    /// - `(true, true)` 或 `(false, false)`：正常情况或双重失败 → 不变
    #[inline]
    fn adjust_count_for_rename(&self, new_inserted: bool, old_removed: bool) {
        match (new_inserted, old_removed) {
            (true, false) => {
                self.total_count.fetch_add(1, Ordering::Relaxed);
            }
            (false, true) => {
                self.total_count.fetch_sub(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    /// Debug 模式下的慢速计数验证（遍历所有分片统计实际条目数）。
    #[cfg(debug_assertions)]
    fn recount_slow(&self) -> usize {
        self.shards.iter().map(|shard| shard.lock().count).sum()
    }

    /// Debug 模式下验证 total_count 与实际条目数一致。
    #[cfg(debug_assertions)]
    #[inline]
    fn debug_verify_count(&self) {
        let actual = self.recount_slow();
        let cached = self.total_count.load(Ordering::Relaxed);
        debug_assert_eq!(
            cached, actual,
            "DentryCache total_count mismatch: cached={}, actual={}",
            cached, actual
        );
    }

    #[cfg(not(debug_assertions))]
    #[inline]
    fn debug_verify_count(&self) {}

    /// 按（父地址, 名称）查找缓存。
    ///
    /// 返回 `Some(Arc<Dentry>)` 表示缓存命中；返回 `None` 表示缓存未命中，
    /// 调用方应当调用 `InodeOps::lookup` 并将结果插入缓存。
    ///
    /// 只锁定一个分片，与其他分片的并发操作完全独立。
    pub fn get(&self, parent: &Arc<Dentry>, name: &str) -> Option<Arc<Dentry>> {
        let parent_ptr = Arc::as_ptr(parent) as usize;
        let hash = dentry_hash(parent_ptr, name);
        let idx = hash & SHARD_MASK;
        let shard = self.shards[idx].lock();
        shard.get(hash, parent_ptr, name).cloned()
    }

    /// 将 Dentry 插入缓存（由 VFS 层在 `lookup`/`create` 之后调用）。
    ///
    /// 若同键条目已存在（并发插入的竞争结果），保留先插入的条目并返回它，
    /// 确保同一名称在缓存中只有一份正规的 Dentry（唯一性不变式）。
    ///
    /// 父目录信息完全从 `dentry.meta.parent` 推导，而不是由调用方重复传入，避免缓存键与
    /// dentry 自身定位信息发生撕裂。调用方必须使用返回值作为后续操作的 Dentry，而非
    /// 自己传入的参数。
    ///
    /// 在缓存预算耗尽时，本方法允许直接跳过缓存并返回传入的 dentry；这只会降低命中率，
    /// 不会影响语义正确性。
    pub fn insert(&self, dentry: Arc<Dentry>) -> Arc<Dentry> {
        let incoming_positive = dentry.is_positive();
        let (parent_ptr, name) = {
            let meta = dentry.meta.lock();
            let parent_ptr = meta
                .parent_cloned()
                .as_ref()
                .map(|parent| Arc::as_ptr(parent) as usize)
                .unwrap_or(0);
            (parent_ptr, meta.name_cloned())
        };
        let hash = dentry_hash(parent_ptr, name.as_str());
        let idx = hash & SHARD_MASK;

        let mut replaced_non_positive = None;
        let mut total_delta: isize = 0;
        let result = {
            let mut shard = self.shards[idx].lock();
            if let Some(existing) = shard.get(hash, parent_ptr, name.as_str()) {
                if existing.is_positive() || !incoming_positive {
                    return Arc::clone(existing);
                }
                replaced_non_positive = Some(Arc::clone(existing));
            } else {
                if !incoming_positive && shard.non_positive_count >= NON_POSITIVE_LIMIT_PER_SHARD {
                    if shard.evict_one_non_positive().is_some() {
                        total_delta -= 1;
                    } else {
                        return Arc::clone(&dentry);
                    }
                }
                if shard.count >= TOTAL_LIMIT_PER_SHARD {
                    if shard.evict_one_any().is_some() {
                        total_delta -= 1;
                    } else {
                        return Arc::clone(&dentry);
                    }
                }
            }

            let is_new = shard.insert(hash, parent_ptr, name, Arc::clone(&dentry));
            if is_new {
                total_delta += 1;
            }
            Arc::clone(&dentry)
        };

        match total_delta.cmp(&0) {
            core::cmp::Ordering::Greater => {
                self.total_count
                    .fetch_add(total_delta as usize, Ordering::Relaxed);
            }
            core::cmp::Ordering::Less => {
                self.total_count
                    .fetch_sub((-total_delta) as usize, Ordering::Relaxed);
            }
            core::cmp::Ordering::Equal => {}
        }
        if let Some(existing) = replaced_non_positive {
            existing.invalidate();
        }
        self.debug_verify_count();
        result
    }

    /// 驱逐（invalidate）指定 Dentry 在缓存中的条目。
    ///
    /// 在以下情况下调用：
    /// - 文件被 `unlink`：移除该名称的条目；
    /// - 目录或挂载树的批量清理由 `invalidate_subtree()` 处理；
    /// - 文件系统被卸载（`umount`）时也应改用 `invalidate_subtree()`。
    ///
    /// 注意：此方法不会修改 `dentry` 本身的状态（正/负向），调用方应在适当时机
    /// 单独调用 `Dentry::invalidate()`。
    pub fn invalidate_dentry(&self, dentry: &Arc<Dentry>) {
        let (parent_ptr, name) = {
            let meta = dentry.meta.lock();
            let parent_ptr = meta
                .parent_cloned()
                .as_ref()
                .map(|p| Arc::as_ptr(p) as usize)
                .unwrap_or(0);
            (parent_ptr, meta.name_cloned())
        };
        let hash = dentry_hash(parent_ptr, name.as_str());
        let idx = hash & SHARD_MASK;

        {
            let mut shard = self.shards[idx].lock();
            // 仅当缓存中的条目指向同一 Arc 时才移除（防止移除新插入的同名条目）。
            if shard
                .remove_if_matches(hash, parent_ptr, name.as_str(), dentry)
                .is_some()
            {
                self.total_count.fetch_sub(1, Ordering::Relaxed);
            }
        }
        self.debug_verify_count();
    }

    /// 驱逐并失效整棵 dentry 子树。
    ///
    /// 与 `invalidate_dentry()` 的区别在于：此方法会同时删除 `root` 自身及其所有后代的
    /// 缓存键，并把这些旧节点全部标记为 `INVALID`，防止外部仍持有旧 `Arc` 时继续把
    /// 已删除/已卸载的命名空间对象当成有效路径继续遍历。
    pub fn invalidate_subtree(&self, root: &Arc<Dentry>) {
        let mut removed: Vec<Arc<Dentry>> = Vec::new();
        let mut removed_count = 0usize;

        for shard_lock in self.shards.iter() {
            let mut shard = shard_lock.lock();
            let mut idx = 0usize;
            while idx < shard.capacity() {
                let should_remove = shard.buckets[idx]
                    .as_ref()
                    .is_some_and(|bucket| bucket.dentry.is_descendant_of(root));
                if should_remove {
                    removed.push(shard.remove_at(idx));
                    removed_count += 1;
                } else {
                    idx += 1;
                }
            }
        }

        if removed_count != 0 {
            self.total_count.fetch_sub(removed_count, Ordering::Relaxed);
        }
        for dentry in removed {
            dentry.invalidate();
        }
        self.debug_verify_count();
    }

    /// 在 rename 后原子更新缓存键和 dentry 的定位信息。
    ///
    /// rename 操作改变了 Dentry 的 (parent, name)，缓存键也必须对应更新。
    /// 该方法在持有 dentry 的 `meta` 锁和相关分片锁期间完成：
    /// 1. 先插入新缓存键（避免并发 get 找不到）；
    /// 2. 更新 dentry 的 `(name, parent)`；
    /// 3. 移除旧缓存键。
    ///
    /// 因而并发路径解析要么看到旧映射，要么看到已经更新完成的新映射；
    /// 不会再观察到”新键指向旧 parent/name”的撕裂状态或短暂的”不存在”窗口。
    ///
    /// 锁顺序统一为 `dentry.meta -> shard(s)`，与 `insert`/`invalidate_dentry`
    /// 保持一致，避免锁序反转导致的死锁。
    pub fn rename_dentry(&self, dentry: &Arc<Dentry>, new_parent: &Arc<Dentry>, new_name: &str) {
        assert_valid_dentry_name(new_name, false);
        let new_ptr = Arc::as_ptr(new_parent) as usize;
        let new_name_small = SmallStr::new(new_name);
        let new_hash = dentry_hash(new_ptr, new_name);

        {
            let mut meta = dentry.meta.lock();
            let old_name = meta.name_cloned();
            let old_ptr = meta
                .parent_cloned()
                .as_ref()
                .map(|parent| Arc::as_ptr(parent) as usize)
                .unwrap_or(0);

            if old_ptr == new_ptr && old_name.as_str() == new_name {
                return;
            }

            let old_hash = dentry_hash(old_ptr, old_name.as_str());
            let old_idx = old_hash & SHARD_MASK;
            let new_idx = new_hash & SHARD_MASK;

            if old_idx == new_idx {
                let mut shard = self.shards[old_idx].lock();
                let new_inserted = shard.insert(
                    new_hash,
                    new_ptr,
                    new_name_small.clone(),
                    Arc::clone(dentry),
                );
                meta.name = new_name_small;
                meta.parent = Some(Arc::clone(new_parent));
                let old_removed = shard
                    .remove_if_matches(old_hash, old_ptr, old_name.as_str(), dentry)
                    .is_some();
                self.adjust_count_for_rename(new_inserted, old_removed);
            } else {
                let (first_idx, second_idx) = if old_idx < new_idx {
                    (old_idx, new_idx)
                } else {
                    (new_idx, old_idx)
                };
                let mut first_guard = self.shards[first_idx].lock();
                let mut second_guard = self.shards[second_idx].lock();

                let (old_guard, new_guard) = if old_idx == first_idx {
                    (&mut *first_guard, &mut *second_guard)
                } else {
                    (&mut *second_guard, &mut *first_guard)
                };
                let new_inserted = new_guard.insert(
                    new_hash,
                    new_ptr,
                    new_name_small.clone(),
                    Arc::clone(dentry),
                );
                meta.name = new_name_small;
                meta.parent = Some(Arc::clone(new_parent));
                let old_removed = old_guard
                    .remove_if_matches(old_hash, old_ptr, old_name.as_str(), dentry)
                    .is_some();
                self.adjust_count_for_rename(new_inserted, old_removed);
            }
        }
        self.debug_verify_count();
    }

    /// 返回当前缓存中的总条目数（近似值，O(1) 操作）。
    ///
    /// 注意：由于使用 Relaxed 内存顺序，返回值可能短暂地与实际条目数不一致。
    /// 这对于统计和调试用途是可接受的。
    ///
    /// **重要**：此值仅用于统计、trace、debug 输出，不得用于任何内核正确性逻辑。
    /// 不能用它来决定是否卸载、是否触发回收、是否判定缓存一致性等。
    pub fn len(&self) -> usize {
        self.total_count.load(Ordering::Relaxed)
    }

    /// 判断缓存是否为空（近似值，O(1) 操作）。
    ///
    /// 注意：由于使用 Relaxed 内存顺序，返回值可能短暂地不准确。
    ///
    /// **重要**：此值仅用于统计、trace、debug 输出，不得用于任何内核正确性逻辑。
    pub fn is_empty(&self) -> bool {
        self.total_count.load(Ordering::Relaxed) == 0
    }
}

impl Default for DentryCache {
    fn default() -> Self {
        Self::new()
    }
}

/// 进程可见的 VFS 根。
///
/// 这里的”根”不是全局唯一的超级块根目录，而是某个进程在当前命名空间和安全上下文
/// 下被允许看到的路径解析起点。普通系统启动后它通常就是 `/`；执行 `chroot`、
/// `pivot_root` 或进入新的挂载命名空间之后，它可以变成全局目录树中的任意子树。
/// 路径解析在处理绝对路径和 `..` 向上回溯时都会依赖这个对象，因此它实际上定义了
/// 进程所能观察到的命名空间边界。
struct VfsRootState {
    root_dentry: Arc<Dentry>,
    root_mount: Arc<Mount>,
}

pub struct VfsRoot {
    state: Spinlock<VfsRootState>,
}

impl VfsRoot {
    /// 构造一个新的 VFS 根。
    pub fn new(root_dentry: Arc<Dentry>, root_mount: Arc<Mount>) -> Self {
        root_mount.inc_open();
        Self {
            state: Spinlock::new(VfsRootState {
                root_dentry,
                root_mount,
            }),
        }
    }

    /// 切换当前进程可见根目录。
    ///
    /// `root_mount` 必须是包含 `root_dentry` 的挂载点。调用方通常应使用路径解析
    /// 返回的 [`LookupResult`](crate::vfs::path::LookupResult) 同时取得两者。
    pub fn set(&self, root_dentry: Arc<Dentry>, root_mount: Arc<Mount>) {
        root_mount.inc_open();
        let mut state = self.state.lock();
        let old = core::mem::replace(
            &mut *state,
            VfsRootState {
                root_dentry,
                root_mount,
            },
        );
        old.root_mount.dec_open();
    }

    /// 判断给定的 dentry 是否为当前进程的根目录。
    ///
    /// 用于路径解析中判断是否应该停止向上回溯（处理 `..`）。
    #[inline]
    pub fn is_at_root(&self, dentry: &Arc<Dentry>) -> bool {
        Arc::ptr_eq(&self.state.lock().root_dentry, dentry)
    }

    /// 判断给定的 dentry 与 mount 是否同时匹配当前进程根目录。
    ///
    /// 同一个 dentry 可能在不同 mount 上下文中出现；`..` 跨挂载边界时必须同时
    /// 比较两者，避免把父文件系统中的 dentry 误判为子挂载的根。
    #[inline]
    pub fn is_at_root_in_mount(&self, dentry: &Arc<Dentry>, mount: &Arc<Mount>) -> bool {
        let state = self.state.lock();
        Arc::ptr_eq(&state.root_dentry, dentry) && Arc::ptr_eq(&state.root_mount, mount)
    }

    /// 返回根目录的克隆引用。
    #[inline]
    pub fn root(&self) -> Arc<Dentry> {
        Arc::clone(&self.state.lock().root_dentry)
    }

    /// 返回根目录所在挂载点的克隆引用。
    #[inline]
    pub fn mount(&self) -> Arc<Mount> {
        Arc::clone(&self.state.lock().root_mount)
    }
}

impl Drop for VfsRoot {
    fn drop(&mut self) {
        self.state.lock().root_mount.dec_open();
    }
}

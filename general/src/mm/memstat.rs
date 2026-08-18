//! 全局内存记账与 `/proc/sys/vm` 参数。
//!
//! 本模块把所有跨地址空间的内存观测收束到一组无锁原子上：
//!
//! - **overcommit 记账**：每个 `VmSpace` 维护自己的承诺页数（`committed_pages`），
//!   并在此聚合为全局 `Committed_AS`。`overcommit_memory` 为 2（严格模式）时，
//!   映射/brk 前按 `CommitLimit` 检查，超限返回 `ENOMEM`；为 0（启发式）时拒绝
//!   超过 `总内存 + 交换` 的单次大映射；为 1 时总是允许。
//! - **页类别计数器**：`ResidentPage` 构造/析构时按类别增减，供 `/proc/meminfo`
//!   与 `sysinfo(2)` 输出标准行。
//! - **vm 参数**：`/proc/sys/vm/*` 的存储与解析。除 `drop_caches`（有真实清缓存
//!   动作）与 overcommit 检查外，多数参数当前仅"存储并呈现"，语义与 Linux 默认
//!   一致（如 `swappiness` 无回收器时无效果）。
//!
//! 本模块不依赖 arch，也不依赖 vfs——两类消费方（syscall 层、procfs 渲染层）
//! 各自调用这里的纯函数接口。

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

// ── overcommit / 承诺记账 ────────────────────────────────────────────────────

/// 全局已承诺页数（所有地址空间的 `committed_pages` 之和）。
static COMMITTED_PAGES: AtomicU64 = AtomicU64::new(0);

/// 页类别全局计数器（用户地址空间驻留页，含私有文件缓存里的页）。
pub static ANON_PAGES: AtomicU64 = AtomicU64::new(0);
pub static SHARED_ANON_PAGES: AtomicU64 = AtomicU64::new(0);
pub static PRIVATE_FILE_PAGES: AtomicU64 = AtomicU64::new(0);
pub static SHARED_FILE_PAGES: AtomicU64 = AtomicU64::new(0);

/// 全局锁页数（所有地址空间 `locked_pages` 之和；`/proc/meminfo` Mlocked 用）。
static LOCKED_PAGES: AtomicU64 = AtomicU64::new(0);

/// 记账一次锁页数变化（页为单位，可为负）。
pub fn locked_pages_delta(delta: i64) {
    if delta > 0 {
        LOCKED_PAGES.fetch_add(delta as u64, Ordering::Relaxed);
    } else {
        LOCKED_PAGES.fetch_sub(delta.unsigned_abs(), Ordering::Relaxed);
    }
}

/// 当前全局锁页数。
pub fn locked_pages() -> u64 {
    LOCKED_PAGES.load(Ordering::Acquire)
}

/// 文件页累计写回数（成功写回底层存储的页数）。
///
/// `cachestat(2)` 的 `nr_writeback` 用。本内核写回为同步执行、无异步 in-flight
/// 窗口，因此以累计写回数近似（Linux 报"当前正在写回"的瞬时页数）。
static FILE_WRITEBACK_PAGES: AtomicU64 = AtomicU64::new(0);

/// 文件页累计淘汰数（私有干净文件页缓存被回收的页数）。
///
/// `cachestat(2)` 的 `nr_evicted` 用。无 LRU 时钟，`nr_recently_evicted` 同样
/// 退化为此累计值（见 [`file_evicted_pages`]）。
static FILE_EVICTED_PAGES: AtomicU64 = AtomicU64::new(0);

/// 记账一次成功写回。
pub fn record_file_writeback() {
    FILE_WRITEBACK_PAGES.fetch_add(1, Ordering::Relaxed);
}

/// 记账一次文件缓存淘汰。
pub fn record_file_evict() {
    FILE_EVICTED_PAGES.fetch_add(1, Ordering::Relaxed);
}

/// 文件页累计写回数。
pub fn file_writeback_pages() -> u64 {
    FILE_WRITEBACK_PAGES.load(Ordering::Acquire)
}

/// 文件页累计淘汰数（`nr_recently_evicted` 亦退化为该值）。
pub fn file_evicted_pages() -> u64 {
    FILE_EVICTED_PAGES.load(Ordering::Acquire)
}

/// 记账一次承诺页数变化（页为单位，可为负）。
pub fn commit_pages(delta: i64) {
    if delta > 0 {
        COMMITTED_PAGES.fetch_add(delta as u64, Ordering::Relaxed);
    } else {
        COMMITTED_PAGES.fetch_sub(delta.unsigned_abs(), Ordering::Relaxed);
    }
}

/// 当前全局承诺页数。
pub fn committed_pages() -> u64 {
    COMMITTED_PAGES.load(Ordering::Acquire)
}

// ── /proc/sys/vm 参数 ────────────────────────────────────────────────────────

/// overcommit 模式：0 启发式，1 总是允许，2 严格拒绝。
static OVERCOMMIT_MEMORY: AtomicU32 = AtomicU32::new(0);
/// 严格模式下允许的承诺上限 = (物理内存 + 交换) * ratio / 100。
static OVERCOMMIT_RATIO: AtomicU32 = AtomicU32::new(50);
/// 严格模式下以 KB 直接指定承诺上限；非 0 时优先于 ratio。
static OVERCOMMIT_KBYTES: AtomicU64 = AtomicU64::new(0);
/// 单进程 VMA 数量上限（Linux 默认 65530）。超限的 mmap/brk/mremap 返回 ENOMEM。
static MAX_MAP_COUNT: AtomicU32 = AtomicU32::new(65_530);
/// 保留内存下限（KB）。本内核不参与分配策略，仅存储呈现。
static MIN_FREE_KBYTES: AtomicU32 = AtomicU32::new(0);
/// 换出倾向（0..=200）。无回收器时仅存储呈现。
static SWAPPINESS: AtomicU32 = AtomicU32::new(60);
/// panic_on_oom：0 不 panic（本内核无 OOM killer，仅记账）。
static PANIC_ON_OOM: AtomicU32 = AtomicU32::new(0);
/// oom_dump_tasks：是否在 OOM 时输出任务列表（本内核不触发）。
static OOM_DUMP_TASKS: AtomicBool = AtomicBool::new(true);
/// oom_kill_allocating_task：是否优先杀掉触发分配的任务（本内核不触发）。
static OOM_KILL_ALLOCATING_TASK: AtomicBool = AtomicBool::new(false);
/// 一次 swap 读取的页簇大小。
static PAGE_CLUSTER: AtomicU32 = AtomicU32::new(3);
/// 脏页占比上限（%）。
static DIRTY_RATIO: AtomicU32 = AtomicU32::new(20);
/// 后台回写脏页占比阈值（%）。
static DIRTY_BACKGROUND_RATIO: AtomicU32 = AtomicU32::new(10);
/// 周期性回写间隔（百分之一秒）。
static DIRTY_WRITEBACK_CENTISECS: AtomicU32 = AtomicU32::new(500);
/// 脏页过期时间（百分之一秒）。
static DIRTY_EXPIRE_CENTISECS: AtomicU32 = AtomicU32::new(3000);
/// vfs cache pressure（0..=1000）。本内核无 dentry 回收压力模型，仅存储。
static VFS_CACHE_PRESSURE: AtomicU32 = AtomicU32::new(100);
/// 非特权用户是否允许 userfaultfd（Linux 6.x 默认 0）。
static UNPRIVILEGED_USERFAULTFD: AtomicBool = AtomicBool::new(false);

/// `drop_caches` 写入值。1 = 清页缓存，2 = 清 dentry 缓存，3 = 两者。
pub fn drop_caches_request() -> u32 {
    DROP_CACHES.load(Ordering::Acquire)
}
fn clear_drop_caches_request() {
    DROP_CACHES.store(0, Ordering::Release);
}
static DROP_CACHES: AtomicU32 = AtomicU32::new(0);

/// 读取一个 u32 vm 参数。
pub fn get_vm_u32(which: VmParam) -> u32 {
    match which {
        VmParam::OvercommitMemory => OVERCOMMIT_MEMORY.load(Ordering::Acquire),
        VmParam::OvercommitRatio => OVERCOMMIT_RATIO.load(Ordering::Acquire),
        VmParam::OvercommitKbytes => OVERCOMMIT_KBYTES
            .load(Ordering::Acquire)
            .min(u32::MAX as u64) as u32,
        VmParam::MaxMapCount => MAX_MAP_COUNT.load(Ordering::Acquire),
        VmParam::MinFreeKbytes => MIN_FREE_KBYTES.load(Ordering::Acquire),
        VmParam::Swappiness => SWAPPINESS.load(Ordering::Acquire),
        VmParam::PanicOnOom => PANIC_ON_OOM.load(Ordering::Acquire),
        VmParam::PageCluster => PAGE_CLUSTER.load(Ordering::Acquire),
        VmParam::DirtyRatio => DIRTY_RATIO.load(Ordering::Acquire),
        VmParam::DirtyBackgroundRatio => DIRTY_BACKGROUND_RATIO.load(Ordering::Acquire),
        VmParam::DirtyWritebackCentisecs => DIRTY_WRITEBACK_CENTISECS.load(Ordering::Acquire),
        VmParam::DirtyExpireCentisecs => DIRTY_EXPIRE_CENTISECS.load(Ordering::Acquire),
        VmParam::VfsCachePressure => VFS_CACHE_PRESSURE.load(Ordering::Acquire),
        VmParam::OomDumpTasks => u32::from(OOM_DUMP_TASKS.load(Ordering::Acquire)),
        VmParam::OomKillAllocatingTask => {
            u32::from(OOM_KILL_ALLOCATING_TASK.load(Ordering::Acquire))
        }
        VmParam::UnprivilegedUserfaultfd => {
            u32::from(UNPRIVILEGED_USERFAULTFD.load(Ordering::Acquire))
        }
        VmParam::DropCaches => DROP_CACHES.load(Ordering::Acquire),
    }
}

/// 写入一个 u32 vm 参数。写入值不做范围规整，由调用方（procfs 解析层）校验。
pub fn set_vm_u32(which: VmParam, value: u32) {
    let store = |slot: &AtomicU32| slot.store(value, Ordering::Release);
    match which {
        VmParam::OvercommitMemory => store(&OVERCOMMIT_MEMORY),
        VmParam::OvercommitRatio => store(&OVERCOMMIT_RATIO),
        VmParam::OvercommitKbytes => OVERCOMMIT_KBYTES.store(value as u64, Ordering::Release),
        VmParam::MaxMapCount => store(&MAX_MAP_COUNT),
        VmParam::MinFreeKbytes => store(&MIN_FREE_KBYTES),
        VmParam::Swappiness => store(&SWAPPINESS),
        VmParam::PanicOnOom => store(&PANIC_ON_OOM),
        VmParam::PageCluster => store(&PAGE_CLUSTER),
        VmParam::DirtyRatio => store(&DIRTY_RATIO),
        VmParam::DirtyBackgroundRatio => store(&DIRTY_BACKGROUND_RATIO),
        VmParam::DirtyWritebackCentisecs => store(&DIRTY_WRITEBACK_CENTISECS),
        VmParam::DirtyExpireCentisecs => store(&DIRTY_EXPIRE_CENTISECS),
        VmParam::VfsCachePressure => store(&VFS_CACHE_PRESSURE),
        VmParam::OomDumpTasks => OOM_DUMP_TASKS.store(value != 0, Ordering::Release),
        VmParam::OomKillAllocatingTask => {
            OOM_KILL_ALLOCATING_TASK.store(value != 0, Ordering::Release)
        }
        VmParam::UnprivilegedUserfaultfd => {
            UNPRIVILEGED_USERFAULTFD.store(value != 0, Ordering::Release)
        }
        VmParam::DropCaches => DROP_CACHES.store(value, Ordering::Release),
    }
}

pub fn get_vm_u64(which: VmParam) -> u64 {
    match which {
        VmParam::OvercommitKbytes => OVERCOMMIT_KBYTES.load(Ordering::Acquire),
        _ => get_vm_u32(which) as u64,
    }
}

pub fn set_vm_u64(which: VmParam, value: u64) {
    if matches!(which, VmParam::OvercommitKbytes) {
        OVERCOMMIT_KBYTES.store(value, Ordering::Release);
        return;
    }
    set_vm_u32(which, value.min(u32::MAX as u64) as u32);
}

pub fn get_vm_bool(which: VmParam) -> bool {
    match which {
        VmParam::OomDumpTasks => OOM_DUMP_TASKS.load(Ordering::Acquire),
        VmParam::OomKillAllocatingTask => OOM_KILL_ALLOCATING_TASK.load(Ordering::Acquire),
        VmParam::UnprivilegedUserfaultfd => UNPRIVILEGED_USERFAULTFD.load(Ordering::Acquire),
        _ => get_vm_u32(which) != 0,
    }
}

pub fn set_vm_bool(which: VmParam, value: bool) {
    match which {
        VmParam::OomDumpTasks => OOM_DUMP_TASKS.store(value, Ordering::Release),
        VmParam::OomKillAllocatingTask => OOM_KILL_ALLOCATING_TASK.store(value, Ordering::Release),
        VmParam::UnprivilegedUserfaultfd => {
            UNPRIVILEGED_USERFAULTFD.store(value, Ordering::Release)
        }
        _ => set_vm_u32(which, u32::from(value)),
    }
}

/// 写入 `drop_caches`。合法值为 1/2/3；返回是否接受。真正清缓存的动作由
/// procfs 写路径调用 [`perform_drop_caches`] 完成。
pub fn accept_drop_caches(value: u32) -> bool {
    if value == 0 || value > 3 {
        return false;
    }
    DROP_CACHES.store(value, Ordering::Release);
    true
}

/// 执行一次 drop_caches 请求并复位。返回 (清页缓存, 清 dentry 缓存) 是否执行。
pub fn perform_drop_caches() -> (bool, bool) {
    let request = DROP_CACHES.swap(0, Ordering::AcqRel);
    (request & 1 != 0, request & 2 != 0)
}

/// vm 参数枚举（与 `/proc/sys/vm/*` 文件名一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmParam {
    OvercommitMemory,
    OvercommitRatio,
    OvercommitKbytes,
    MaxMapCount,
    MinFreeKbytes,
    Swappiness,
    PanicOnOom,
    OomDumpTasks,
    OomKillAllocatingTask,
    PageCluster,
    DirtyRatio,
    DirtyBackgroundRatio,
    DirtyWritebackCentisecs,
    DirtyExpireCentisecs,
    VfsCachePressure,
    UnprivilegedUserfaultfd,
    DropCaches,
}

impl VmParam {
    /// Linux `/proc/sys/vm/` 下的文件名。
    pub const fn name(self) -> &'static str {
        match self {
            Self::OvercommitMemory => "overcommit_memory",
            Self::OvercommitRatio => "overcommit_ratio",
            Self::OvercommitKbytes => "overcommit_kbytes",
            Self::MaxMapCount => "max_map_count",
            Self::MinFreeKbytes => "min_free_kbytes",
            Self::Swappiness => "swappiness",
            Self::PanicOnOom => "panic_on_oom",
            Self::OomDumpTasks => "oom_dump_tasks",
            Self::OomKillAllocatingTask => "oom_kill_allocating_task",
            Self::PageCluster => "page-cluster",
            Self::DirtyRatio => "dirty_ratio",
            Self::DirtyBackgroundRatio => "dirty_background_ratio",
            Self::DirtyWritebackCentisecs => "dirty_writeback_centisecs",
            Self::DirtyExpireCentisecs => "dirty_expire_centisecs",
            Self::VfsCachePressure => "vfs_cache_pressure",
            Self::UnprivilegedUserfaultfd => "unprivileged_userfaultfd",
            Self::DropCaches => "drop_caches",
        }
    }

    /// 按文件名解析；未知返回 None。
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "overcommit_memory" => Self::OvercommitMemory,
            "overcommit_ratio" => Self::OvercommitRatio,
            "overcommit_kbytes" => Self::OvercommitKbytes,
            "max_map_count" => Self::MaxMapCount,
            "min_free_kbytes" => Self::MinFreeKbytes,
            "swappiness" => Self::Swappiness,
            "panic_on_oom" => Self::PanicOnOom,
            "oom_dump_tasks" => Self::OomDumpTasks,
            "oom_kill_allocating_task" => Self::OomKillAllocatingTask,
            "page-cluster" => Self::PageCluster,
            "dirty_ratio" => Self::DirtyRatio,
            "dirty_background_ratio" => Self::DirtyBackgroundRatio,
            "dirty_writeback_centisecs" => Self::DirtyWritebackCentisecs,
            "dirty_expire_centisecs" => Self::DirtyExpireCentisecs,
            "vfs_cache_pressure" => Self::VfsCachePressure,
            "unprivileged_userfaultfd" => Self::UnprivilegedUserfaultfd,
            "drop_caches" => Self::DropCaches,
            _ => return None,
        })
    }
}

// ── overcommit 判定 ──────────────────────────────────────────────────────────

/// 检查新增 `pages` 页承诺是否被 overcommit 策略拒绝。
///
/// `noreserve` 为 true 时跳过记账（对应 `MAP_NORESERVE`，与 Linux 行为一致：
/// 严格模式下 NORESERVE 映射仍受启发式上限约束）。
///
/// 返回 Err 表示应拒绝（ENOMEM）。本内核没有 OOM killer：拒绝发生在 syscall
/// 层，进程继续存活。
pub fn check_overcommit(
    pages: u64,
    noreserve: bool,
    total_ram_pages: u64,
    total_swap_pages: u64,
) -> Result<(), ()> {
    match OVERCOMMIT_MEMORY.load(Ordering::Acquire) {
        // 总是允许。
        1 => return Ok(()),
        // 严格模式：承诺上限 = overcommit_kbytes（优先）或 (ram+swap)*ratio/100。
        2 => {
            let limit_kb = if OVERCOMMIT_KBYTES.load(Ordering::Acquire) != 0 {
                OVERCOMMIT_KBYTES.load(Ordering::Acquire)
            } else {
                (total_ram_pages.saturating_add(total_swap_pages)
                    * OVERCOMMIT_RATIO.load(Ordering::Acquire) as u64)
                    / 100
                    * (allocator::PAGE_SIZE as u64 / 1024)
            };
            let committed_kb =
                COMMITTED_PAGES.load(Ordering::Acquire) * (allocator::PAGE_SIZE as u64 / 1024);
            let new_kb = pages.saturating_mul(allocator::PAGE_SIZE as u64 / 1024);
            if committed_kb.saturating_add(new_kb) > limit_kb {
                return Err(());
            }
            return Ok(());
        }
        // 启发式：拒绝超过总内存+交换的单次大映射。
        _ => {
            if noreserve {
                return Ok(());
            }
            if pages > total_ram_pages.saturating_add(total_swap_pages) {
                return Err(());
            }
            return Ok(());
        }
    }
}

/// 当前 overcommit 上限（KB）：`overcommit_kbytes` 优先，否则
/// `(物理内存 + 交换) * overcommit_ratio / 100`。`/proc/meminfo` CommitLimit 用。
pub fn commit_limit_kb(total_ram_pages: u64, total_swap_pages: u64) -> u64 {
    let kbytes = OVERCOMMIT_KBYTES.load(Ordering::Acquire);
    if kbytes != 0 {
        return kbytes;
    }
    (total_ram_pages.saturating_add(total_swap_pages)
        * OVERCOMMIT_RATIO.load(Ordering::Acquire) as u64
        / 100)
        * (allocator::PAGE_SIZE as u64 / 1024)
}

/// 当前 VMA 数量是否超过 `max_map_count`。
pub fn map_count_allowed(count: usize) -> bool {
    count <= MAX_MAP_COUNT.load(Ordering::Acquire) as usize
}

/// userfaultfd 是否对当前权限可用（`vm.unprivileged_userfaultfd`）。
pub fn userfaultfd_allowed(privileged: bool) -> bool {
    privileged || UNPRIVILEGED_USERFAULTFD.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_param_name_roundtrip() {
        for param in [
            VmParam::OvercommitMemory,
            VmParam::OvercommitRatio,
            VmParam::OvercommitKbytes,
            VmParam::MaxMapCount,
            VmParam::MinFreeKbytes,
            VmParam::Swappiness,
            VmParam::PanicOnOom,
            VmParam::OomDumpTasks,
            VmParam::OomKillAllocatingTask,
            VmParam::PageCluster,
            VmParam::DirtyRatio,
            VmParam::DirtyBackgroundRatio,
            VmParam::DirtyWritebackCentisecs,
            VmParam::DirtyExpireCentisecs,
            VmParam::VfsCachePressure,
            VmParam::UnprivilegedUserfaultfd,
        ] {
            assert_eq!(VmParam::from_name(param.name()), Some(param));
        }
        assert_eq!(VmParam::from_name("no_such_param"), None);
    }

    #[test]
    fn overcommit_always_ok_in_mode_one() {
        OVERCOMMIT_MEMORY.store(1, Ordering::Relaxed);
        assert!(check_overcommit(u64::MAX / 2, false, 100, 100).is_ok());
        OVERCOMMIT_MEMORY.store(0, Ordering::Relaxed);
    }

    #[test]
    fn heuristic_rejects_huge_single_mapping() {
        OVERCOMMIT_MEMORY.store(0, Ordering::Relaxed);
        assert!(check_overcommit(50, false, 100, 100).is_ok());
        assert!(check_overcommit(201, false, 100, 100).is_err());
        // NORESERVE 豁免启发式检查。
        assert!(check_overcommit(201, true, 100, 100).is_ok());
    }

    #[test]
    fn strict_mode_enforces_ratio_limit() {
        OVERCOMMIT_MEMORY.store(2, Ordering::Relaxed);
        OVERCOMMIT_RATIO.store(50, Ordering::Relaxed);
        OVERCOMMIT_KBYTES.store(0, Ordering::Relaxed);
        let committed = COMMITTED_PAGES.load(Ordering::Relaxed);
        COMMITTED_PAGES.store(0, Ordering::Relaxed);
        // ram=200 页, swap=0, ratio=50% → 上限 100 页。
        assert!(check_overcommit(100, false, 200, 0).is_ok());
        assert!(check_overcommit(101, false, 200, 0).is_err());
        COMMITTED_PAGES.store(committed, Ordering::Relaxed);
        OVERCOMMIT_MEMORY.store(0, Ordering::Relaxed);
    }

    #[test]
    fn drop_caches_accepts_only_one_to_three() {
        assert!(accept_drop_caches(1));
        assert!(accept_drop_caches(3));
        assert!(!accept_drop_caches(0));
        assert!(!accept_drop_caches(4));
        DROP_CACHES.store(0, Ordering::Relaxed);
    }

    #[test]
    fn map_count_allowed_respects_limit() {
        MAX_MAP_COUNT.store(4, Ordering::Relaxed);
        assert!(map_count_allowed(4));
        assert!(!map_count_allowed(5));
        MAX_MAP_COUNT.store(65_530, Ordering::Relaxed);
    }
}

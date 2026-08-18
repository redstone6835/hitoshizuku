//! VmSpace —— 进程地址空间的顶层对象。
//!
//! `VmSpace` 负责把纯 VMA 代数、用户页表 ops、用户数据页生命周期三件事收束在
//! general 层。arch 只提供页表机械动作，COW / `MAP_SHARED` / 脏页写回这些策略
//! 都在这里处理，避免未来把 MM 逻辑散到具体架构里。

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use errno::Errno;
use hashbrown::HashTable;
use mm::area::AnonMergeDomain;
use mm::{FileLike, SharedAnonObject, VmArea, VmBacking, VmFlags, VmaSet};
use sched::sync::Spinlock;
use sched::{WaitQueue, WaitReason};
use smallvec::SmallVec;

use crate::mm::fault::{FaultKind, FaultOutcome, KernelFaultReason};
use crate::mm::memstat;
use crate::mm::ops::{PgdHandle, UserPteUpdate, UserVmLayoutOps, user_pgd_ops, user_vm_layout};
use crate::mm::resident_map::RadixPageMap;
use crate::mm::swap::SwapSlot;
use crate::mm::uffd::{
    UFFD_PAGEFAULT_FLAG_MINOR, UFFD_PAGEFAULT_FLAG_WP, UFFD_PAGEFAULT_FLAG_WRITE,
    UFFDIO_REGISTER_MODE_MINOR, UFFDIO_REGISTER_MODE_MISSING, UFFDIO_REGISTER_MODE_WP, UffdRegion,
    UffdState,
};

/// 顺序只读文件缺页一次最多预装的页数（包含硬件实际命中的页）。
///
/// BuildStorm 会反复执行体积较大的 rustc/链接器映像；适度预装可减少 TCG 下的
/// 硬件缺页陷入，同时避免冷缓存首次缺页同步读取过多无关页面。
const FILE_FAULT_AROUND_PAGES: usize = 16;
/// 私有匿名写缺页一次最多向高地址预映射的页数。
///
/// 生产路径先取 4 页，在顺序写陷阱收益和稀疏映射的投机内存之间折中。
const ANON_STORE_FAULT_AROUND_PAGES: usize = 4;
/// 匿名 Store fault-around 影子模型的最大前向页数。
///
/// 模型只观察真实 nonresident fault，绝不分配或安装额外页面。
#[cfg(any(test, feature = "performance-profile"))]
const ANON_STORE_SHADOW_PAGES: usize = ANON_STORE_FAULT_AROUND_PAGES;
/// 内容持续变化时最多尝试发布缓存快照的次数，随后退回不缓存读取保证前进性。
const PRIVATE_FILE_CACHE_RETRIES: usize = 3;
/// 连续缓存缺失达到该阈值后，才值得进入批量候选页填充路径。
const PRIVATE_FILE_BATCH_MIN_PAGES: usize = 4;
/// 单次批量填页不超过 fault-around 窗口，避免扩大投机读取范围。
const PRIVATE_FILE_BATCH_MAX_PAGES: usize = 16;
/// 限制缺页栈外临时内存；LoongArch 当前 4 KiB 页下对应 16 页。
const PRIVATE_FILE_BATCH_MAX_BYTES: usize = 64 * 1024;
/// 私有干净文件页的强缓存上限；在 4 KiB 页配置下约为 1 GiB。
///
/// BuildStorm 的完整样本会填满 512 MiB 缓存并在构建结束前触发数万次淘汰；
/// 保留 1 GiB 的有界热集可覆盖当前工具链工作集，避免仍有数 GiB 空闲内存时
/// 从 ext4 重读刚淘汰的页。物理页分配失败仍会按批次回收，因此该预算不会阻塞
/// 匿名页和 COW 分配的前进性。
const PRIVATE_FILE_CACHE_MAX_PAGES: usize = 262_144;
/// 独立的私有文件页缓存分片数；32 个分片可覆盖 BuildStorm 的并行 rustc 缺页。
const PRIVATE_FILE_CACHE_SHARD_COUNT: usize = 32;
/// Ready 范围索引的分片粒度。
///
/// fault-around 窗口最大为 64 KiB；使用 256 KiB 文件块可以让绝大多数窗口只获取
/// 一把分片锁，同时仍把大型工具链映像分散到全部缓存分片，避免单文件热点退化为
/// 全局锁。
const PRIVATE_FILE_CACHE_SHARD_CHUNK_BYTES: u64 = 256 * 1024;
/// 共享加载等待表大小。与 Linux folio wait table 一样用固定哈希桶承载等待者，
/// 避免为每个正在装载的文件页单独分配等待对象。
const PRIVATE_FILE_LOAD_WAIT_BUCKETS: usize = 256;
/// clock 淘汰在全局锁内最多检查的条目数，防止满缓存缺页形成 O(N) 长停顿。
const PRIVATE_FILE_CACHE_EVICTION_SCAN_LIMIT: usize = 64;
/// 物理页分配失败时每轮释放的缓存引用数。
const PRIVATE_FILE_CACHE_RECLAIM_BATCH: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileFaultAroundWindow {
    start: usize,
    end: usize,
    file_offset: u64,
}

/// NUMA 内存策略（单节点语义）。
///
/// 目标架构当前只有 node 0，因此策略只记录"模式 + 节点掩码"，不参与页分配。
/// 模式取值与 Linux `MPOL_*` 一致：DEFAULT=0、PREFERRED=1、BIND=2、
/// INTERLEAVE=3、LOCAL=4。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mempolicy {
    pub mode: u32,
    /// 节点位图（本内核只有 node 0，合法掩码只能是 0 或 1）。
    pub node_mask: u64,
    /// `MPOL_BIND`/`MPOL_PREFERRED` 的 home node（`set_mempolicy_home_node`）。
    /// 单节点系统恒为 0；未显式设置时与 `node_mask` 首个节点一致。
    pub home_node: u32,
}

/// 地址空间级内存策略状态：进程默认策略 + `mbind` 区域覆盖。
///
/// 区域表以 `(start, end)` 为键（`Range` 未实现 `Ord`，用元组代替）。
#[derive(Default, Clone)]
struct MempolicyState {
    default_policy: Option<Mempolicy>,
    ranges: BTreeMap<(usize, usize), Mempolicy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrivateFileBatchPlan {
    pages: usize,
    buffer_len: usize,
    read_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrivateFileCacheSnapshot {
    file_key: usize,
    generation: u64,
    file_size: u64,
}

impl FileFaultAroundWindow {
    #[cfg(test)]
    fn page_count(self, page_size: usize) -> usize {
        (self.end - self.start) / page_size
    }
}

/// 计算从硬件故障页向高地址预装的只读文件窗口。
///
/// 窗口同时受最大页数、VMA 末端和文件 EOF 限制；文件最后一个非整页仍计入。
fn file_fault_around_window(
    fault_page: usize,
    area_start: usize,
    area_end: usize,
    area_file_offset: u64,
    file_size: u64,
    page_size: usize,
) -> Option<FileFaultAroundWindow> {
    if page_size == 0
        || !page_size.is_power_of_two()
        || fault_page % page_size != 0
        || area_start % page_size != 0
        || area_end % page_size != 0
        || fault_page < area_start
        || fault_page >= area_end
    {
        return None;
    }
    let delta = fault_page.checked_sub(area_start)?;
    let file_offset = area_file_offset.checked_add(u64::try_from(delta).ok()?)?;
    if file_offset >= file_size {
        return None;
    }
    let vma_pages = area_end.checked_sub(fault_page)? / page_size;
    let page_size_u64 = u64::try_from(page_size).ok()?;
    let file_bytes = file_size.checked_sub(file_offset)?;
    let file_pages = file_bytes / page_size_u64 + u64::from(file_bytes % page_size_u64 != 0);
    let pages = vma_pages
        .min(usize::try_from(file_pages).ok()?)
        .min(FILE_FAULT_AROUND_PAGES);
    if pages == 0 {
        return None;
    }
    let len = pages.checked_mul(page_size)?;
    Some(FileFaultAroundWindow {
        start: fault_page,
        end: fault_page.checked_add(len)?,
        file_offset,
    })
}

fn private_file_cache_snapshot(file: &dyn FileLike) -> (u64, Option<PrivateFileCacheSnapshot>) {
    let Some(file_key) = file.private_page_cache_key() else {
        return (file.size(), None);
    };
    let Some(generation) = file.private_page_cache_generation() else {
        return (file.size(), None);
    };
    let file_size = file.size();
    let snapshot = (file.private_page_cache_generation() == Some(generation)).then_some(
        PrivateFileCacheSnapshot {
            file_key,
            generation,
            file_size,
        },
    );
    (file_size, snapshot)
}

fn anon_store_fault_around_end(
    fault_page: usize,
    area_range: &Range<usize>,
    page_size: usize,
) -> Option<usize> {
    if page_size == 0
        || !page_size.is_power_of_two()
        || fault_page % page_size != 0
        || area_range.start % page_size != 0
        || area_range.end % page_size != 0
        || !area_range.contains(&fault_page)
    {
        return None;
    }
    let max_len = ANON_STORE_FAULT_AROUND_PAGES.checked_mul(page_size)?;
    let end = fault_page.saturating_add(max_len).min(area_range.end);
    (end > fault_page).then_some(end)
}

#[cfg(any(test, feature = "performance-profile"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnonStoreShadowKey {
    task_id: u64,
    task_epoch: u64,
    vm_id: u64,
    vma_end: usize,
}

#[cfg(any(test, feature = "performance-profile"))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AnonStoreShadowState {
    key: Option<AnonStoreShadowKey>,
    window_start: usize,
    window_end: usize,
}

#[cfg(any(test, feature = "performance-profile"))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AnonStoreShadowObservation {
    state: AnonStoreShadowState,
    simulated_batch: bool,
    would_save: bool,
    reset: bool,
}

/// 在不改变映射的前提下推进一次匿名写 fault-around 影子窗口。
///
/// key 包含稳定 VmSpace id、task 发布代际和 VMA 末端。代际变化会主动丢弃
/// 旧窗口，防止任务迁移后返回旧 CPU 或其它任务插入时复用陈旧状态。
/// 这种重置会漏掉真实预装页跨调度仍然有效的收益，通常给出保守下界；但模型
/// 没有 VMA 修改代际，也不模拟分配失败、并发 PTE 冲突和 `madvise` 回收，因此
/// 同末端 unmap/remap 或重复 fault 仍可能造成少量向上偏差，不能视为严格下界。
#[cfg(any(test, feature = "performance-profile"))]
fn observe_anon_store_shadow(
    state: AnonStoreShadowState,
    key: AnonStoreShadowKey,
    fault_page: usize,
    page_size: usize,
) -> Option<AnonStoreShadowObservation> {
    if page_size == 0
        || !page_size.is_power_of_two()
        || fault_page % page_size != 0
        || key.vma_end % page_size != 0
        || fault_page >= key.vma_end
    {
        return None;
    }

    let reset = state.key.is_some_and(|old| old != key);
    if !reset
        && state.key == Some(key)
        && fault_page >= state.window_start
        && fault_page < state.window_end
    {
        return Some(AnonStoreShadowObservation {
            state,
            would_save: true,
            ..AnonStoreShadowObservation::default()
        });
    }

    let window_bytes = ANON_STORE_SHADOW_PAGES.checked_mul(page_size)?;
    let window_end = fault_page.saturating_add(window_bytes).min(key.vma_end);
    Some(AnonStoreShadowObservation {
        state: AnonStoreShadowState {
            key: Some(key),
            window_start: fault_page,
            window_end,
        },
        simulated_batch: true,
        would_save: false,
        reset,
    })
}

/// 计算连续私有文件 cache miss 的批量读取尺寸。
///
/// 每个候选物理页都直接作为文件读取目标；`buffer_len` 表示本轮最终页覆盖的总
/// 字节数，`read_len` 只包含 EOF 前有效数据，最后一个非整页的尾部单独清零。
fn private_file_batch_plan(
    file_offset: u64,
    file_size: u64,
    window_pages: usize,
    consecutive_misses: usize,
    page_size: usize,
) -> Option<PrivateFileBatchPlan> {
    if page_size == 0 || !page_size.is_power_of_two() {
        return None;
    }
    let remaining = file_size.checked_sub(file_offset)?;
    if remaining == 0 {
        return None;
    }
    let max_pages_by_bytes = PRIVATE_FILE_BATCH_MAX_BYTES / page_size;
    let pages_cap = window_pages
        .min(consecutive_misses)
        .min(PRIVATE_FILE_BATCH_MAX_PAGES)
        .min(max_pages_by_bytes);
    if pages_cap < PRIVATE_FILE_BATCH_MIN_PAGES {
        return None;
    }

    let page_size_u64 = u64::try_from(page_size).ok()?;
    let pages_before_eof = remaining / page_size_u64 + u64::from(remaining % page_size_u64 != 0);
    let pages = pages_cap.min(usize::try_from(pages_before_eof).unwrap_or(usize::MAX));
    if pages < PRIVATE_FILE_BATCH_MIN_PAGES {
        return None;
    }
    let buffer_len = pages.checked_mul(page_size)?;
    let read_len = usize::try_from(remaining.min(u64::try_from(buffer_len).ok()?)).ok()?;
    Some(PrivateFileBatchPlan {
        pages,
        buffer_len,
        read_len,
    })
}

fn private_file_batch_page_offset(base: u64, index: usize, page_size: usize) -> Option<u64> {
    let delta = index.checked_mul(page_size)?;
    base.checked_add(u64::try_from(delta).ok()?)
}

/// 只有批次首项对应真实 fault 页；后续投机邻页的失败只能截断 fault-around。
const fn private_file_batch_error_is_fatal(page_index: usize) -> bool {
    page_index == 0
}

fn permits_file_fault_around(flags: VmFlags, kind: FaultKind) -> bool {
    let permits_access = match kind {
        FaultKind::Load => flags.has(VmFlags::READ),
        FaultKind::Exec => flags.has(VmFlags::EXEC),
        _ => false,
    };
    permits_access
        && !flags.has(VmFlags::WRITE)
        && !flags.has(VmFlags::SHARED)
        && !flags.has(VmFlags::GROWS_DOWN)
}

/// 返回并发映射出现前仍可安全安装的连续候选前缀长度。
fn unmapped_prefix_len(
    addresses: impl IntoIterator<Item = usize>,
    mut is_mapped: impl FnMut(usize) -> bool,
) -> usize {
    let mut count = 0usize;
    for address in addresses {
        if is_mapped(address) {
            break;
        }
        count += 1;
    }
    count
}

fn same_backing_snapshot(current: &VmBacking, snapshot: &VmBacking) -> bool {
    match (current, snapshot) {
        (
            VmBacking::Anon {
                merge_domain: current,
            },
            VmBacking::Anon {
                merge_domain: snapshot,
            },
        ) => current.same_snapshot_identity(*snapshot),
        (
            VmBacking::SharedAnon {
                object: current_object,
                offset: current_offset,
            },
            VmBacking::SharedAnon {
                object: snapshot_object,
                offset: snapshot_offset,
            },
        ) => Arc::ptr_eq(current_object, snapshot_object) && current_offset == snapshot_offset,
        (
            VmBacking::File {
                file: current_file,
                offset: current_offset,
            },
            VmBacking::File {
                file: snapshot_file,
                offset: snapshot_offset,
            },
        ) => Arc::ptr_eq(current_file, snapshot_file) && current_offset == snapshot_offset,
        (VmBacking::Direct(current), VmBacking::Direct(snapshot)) => current == snapshot,
        _ => false,
    }
}

#[inline]
fn vm_layout() -> &'static UserVmLayoutOps {
    user_vm_layout().expect("[mm] user_vm_layout_ops not registered")
}

/// 当前架构注入的用户页粒度。
#[kernel_symbols::export(name = "general.mm.page_size", contract = "kernel.mm.query@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
pub fn page_size() -> usize {
    vm_layout().page_size
}

/// 栈向低地址生长的最大字节数，取 `RLIMIT_STACK` 软上限与架构布局上限的较小值。
///
/// Linux 栈扩展按 `RLIMIT_STACK` 软上限限制；`RLIM_INFINITY` 或调度器尚未就绪
/// （启动早期自检）时退回架构布局硬上限 `max_grows_down_bytes`。
fn stack_growth_limit() -> usize {
    let layout_max = vm_layout().max_grows_down_bytes;
    if !sched::is_ready() {
        return layout_max;
    }
    match sched::operation::get_rlimit(sched::rlimit::Resource::Stack) {
        Ok(pair) if !pair.soft.is_infinity() => {
            let soft = usize::try_from(pair.soft.0).unwrap_or(usize::MAX);
            soft.min(layout_max)
        }
        _ => layout_max,
    }
}

#[inline]
fn page_base(addr: usize) -> usize {
    let page_size = page_size();
    addr & !(page_size - 1)
}

fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

fn covered_len(areas: &[VmArea], range: &Range<usize>) -> usize {
    let mut cursor = range.start;
    let mut total = 0usize;
    for area in areas {
        if area.range.start > cursor {
            break;
        }
        let end = area.range.end.min(range.end);
        if end > cursor {
            total += end - cursor;
            cursor = end;
        }
        if cursor >= range.end {
            break;
        }
    }
    total
}

type WeakFilePageCache = Spinlock<BTreeMap<FilePageKey, Weak<ResidentPage>>>;
type PrivateFilePageCache = ShardedPrivateFilePageCache<PRIVATE_FILE_CACHE_SHARD_COUNT>;

static PRIVATE_FILE_PAGES: PrivateFilePageCache =
    ShardedPrivateFilePageCache::new(PRIVATE_FILE_CACHE_MAX_PAGES);
static SHARED_FILE_PAGES: WeakFilePageCache = Spinlock::new(BTreeMap::new());
static SHARED_ANON_PAGES: Spinlock<BTreeMap<SharedAnonPageKey, SharedAnonPageEntry>> =
    Spinlock::new(BTreeMap::new());
static VM_SPACE_LIVE: AtomicUsize = AtomicUsize::new(0);
static VM_SPACE_CREATED: AtomicUsize = AtomicUsize::new(0);
static VM_SPACE_DROPPED: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "performance-profile")]
static VM_SPACE_PROFILE_ID_NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AnonStoreShadowDiag {
    pub faults: u64,
    pub simulated_batches: u64,
    pub would_save: u64,
    pub migration_interleave_resets: u64,
}

#[cfg(feature = "performance-profile")]
#[repr(align(64))]
struct AnonStoreShadowCpu {
    task_id: AtomicU64,
    task_epoch: AtomicU64,
    vm_id: AtomicU64,
    vma_end: AtomicUsize,
    window_start: AtomicUsize,
    window_end: AtomicUsize,
    faults: AtomicU64,
    simulated_batches: AtomicU64,
    would_save: AtomicU64,
    migration_interleave_resets: AtomicU64,
}

#[cfg(feature = "performance-profile")]
impl AnonStoreShadowCpu {
    const fn new() -> Self {
        Self {
            task_id: AtomicU64::new(0),
            task_epoch: AtomicU64::new(0),
            vm_id: AtomicU64::new(0),
            vma_end: AtomicUsize::new(0),
            window_start: AtomicUsize::new(0),
            window_end: AtomicUsize::new(0),
            faults: AtomicU64::new(0),
            simulated_batches: AtomicU64::new(0),
            would_save: AtomicU64::new(0),
            migration_interleave_resets: AtomicU64::new(0),
        }
    }

    fn state(&self) -> AnonStoreShadowState {
        let vm_id = self.vm_id.load(Ordering::Relaxed);
        AnonStoreShadowState {
            key: (vm_id != 0).then(|| AnonStoreShadowKey {
                task_id: self.task_id.load(Ordering::Relaxed),
                task_epoch: self.task_epoch.load(Ordering::Relaxed),
                vm_id,
                vma_end: self.vma_end.load(Ordering::Relaxed),
            }),
            window_start: self.window_start.load(Ordering::Relaxed),
            window_end: self.window_end.load(Ordering::Relaxed),
        }
    }

    fn store_state(&self, state: AnonStoreShadowState) {
        let Some(key) = state.key else {
            self.vm_id.store(0, Ordering::Relaxed);
            return;
        };
        self.task_id.store(key.task_id, Ordering::Relaxed);
        self.task_epoch.store(key.task_epoch, Ordering::Relaxed);
        self.vma_end.store(key.vma_end, Ordering::Relaxed);
        self.window_start
            .store(state.window_start, Ordering::Relaxed);
        self.window_end.store(state.window_end, Ordering::Relaxed);
        self.vm_id.store(key.vm_id, Ordering::Relaxed);
    }
}

#[cfg(feature = "performance-profile")]
static ANON_STORE_SHADOW_CPUS: [AnonStoreShadowCpu; sched::NR_CPUS] =
    [const { AnonStoreShadowCpu::new() }; sched::NR_CPUS];

#[cfg(feature = "performance-profile")]
fn anon_store_shadow_cpu() -> &'static AnonStoreShadowCpu {
    &ANON_STORE_SHADOW_CPUS[sched::current_cpu_id().min(sched::NR_CPUS - 1)]
}

#[cfg(feature = "performance-profile")]
fn record_anon_store_shadow_fault(vm_id: u64, fault_page: usize, vma_end: usize) {
    let cpu = anon_store_shadow_cpu();
    add_local_fault_around_counter(&cpu.faults, 1);
    let key = AnonStoreShadowKey {
        task_id: sched::current_task_id(),
        task_epoch: sched::current_task_epoch(),
        vm_id,
        vma_end,
    };
    let Some(observation) = observe_anon_store_shadow(cpu.state(), key, fault_page, page_size())
    else {
        return;
    };
    if observation.simulated_batch {
        add_local_fault_around_counter(&cpu.simulated_batches, 1);
    }
    if observation.would_save {
        add_local_fault_around_counter(&cpu.would_save, 1);
    }
    if observation.reset {
        add_local_fault_around_counter(&cpu.migration_interleave_resets, 1);
    }
    cpu.store_state(observation.state);
}

pub(crate) fn anon_store_shadow_diag() -> AnonStoreShadowDiag {
    #[cfg(feature = "performance-profile")]
    let mut diag = AnonStoreShadowDiag::default();
    #[cfg(not(feature = "performance-profile"))]
    let diag = AnonStoreShadowDiag::default();
    #[cfg(feature = "performance-profile")]
    for cpu in &ANON_STORE_SHADOW_CPUS {
        diag.faults = diag
            .faults
            .saturating_add(cpu.faults.load(Ordering::Relaxed));
        diag.simulated_batches = diag
            .simulated_batches
            .saturating_add(cpu.simulated_batches.load(Ordering::Relaxed));
        diag.would_save = diag
            .would_save
            .saturating_add(cpu.would_save.load(Ordering::Relaxed));
        diag.migration_interleave_resets = diag
            .migration_interleave_resets
            .saturating_add(cpu.migration_interleave_resets.load(Ordering::Relaxed));
    }
    diag
}

#[cfg(feature = "performance-profile")]
const HARDWARE_FAULT_BACKING_COUNT: usize = 5;
#[cfg(feature = "performance-profile")]
const HARDWARE_FAULT_ACCESS_COUNT: usize = 4;
#[cfg(feature = "performance-profile")]
const HARDWARE_FAULT_RESIDENCY_COUNT: usize = 2;

/// 硬件用户缺页对应的 VMA backing 分类。
#[cfg(feature = "performance-profile")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum HardwareFaultBacking {
    Anon = 0,
    SharedAnon,
    PrivateFile,
    SharedFile,
    Direct,
}

#[cfg(feature = "performance-profile")]
impl HardwareFaultBacking {
    pub(crate) const ALL: [Self; HARDWARE_FAULT_BACKING_COUNT] = [
        Self::Anon,
        Self::SharedAnon,
        Self::PrivateFile,
        Self::SharedFile,
        Self::Direct,
    ];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Anon => "Anon",
            Self::SharedAnon => "SharedAnon",
            Self::PrivateFile => "PrivateFile",
            Self::SharedFile => "SharedFile",
            Self::Direct => "Direct",
        }
    }

    fn from_vma(backing: &VmBacking, flags: VmFlags) -> Self {
        match backing {
            VmBacking::Anon { .. } => Self::Anon,
            VmBacking::SharedAnon { .. } => Self::SharedAnon,
            VmBacking::File { .. } if flags.has(VmFlags::SHARED) => Self::SharedFile,
            VmBacking::File { .. } => Self::PrivateFile,
            VmBacking::Direct(_) => Self::Direct,
        }
    }
}

/// 硬件缺页访问类型。LoongArch PPI 不携带读写取指信息，必须单列。
#[cfg(feature = "performance-profile")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum HardwareFaultAccess {
    Load = 0,
    Store,
    Exec,
    Privilege,
}

#[cfg(feature = "performance-profile")]
impl HardwareFaultAccess {
    pub(crate) const ALL: [Self; HARDWARE_FAULT_ACCESS_COUNT] =
        [Self::Load, Self::Store, Self::Exec, Self::Privilege];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Load => "Load",
            Self::Store => "Store",
            Self::Exec => "Exec",
            Self::Privilege => "Privilege",
        }
    }

    const fn from_kind(kind: FaultKind) -> Self {
        match kind {
            FaultKind::Load | FaultKind::PermRead => Self::Load,
            FaultKind::Store | FaultKind::PermWrite => Self::Store,
            FaultKind::Exec | FaultKind::PermExec => Self::Exec,
            FaultKind::Privilege => Self::Privilege,
        }
    }
}

/// `/proc/meminfo` 导出的硬件用户缺页累计快照。
#[cfg(feature = "performance-profile")]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HardwareFaultDiag {
    counts: [[[u64; HARDWARE_FAULT_RESIDENCY_COUNT]; HARDWARE_FAULT_ACCESS_COUNT];
        HARDWARE_FAULT_BACKING_COUNT],
}

#[cfg(feature = "performance-profile")]
impl HardwareFaultDiag {
    pub(crate) fn count(
        &self,
        backing: HardwareFaultBacking,
        access: HardwareFaultAccess,
        resident: bool,
    ) -> u64 {
        self.counts[backing as usize][access as usize][usize::from(resident)]
    }
}

#[cfg(feature = "performance-profile")]
#[repr(align(64))]
struct HardwareFaultCpuCounters {
    counts: [[[AtomicU64; HARDWARE_FAULT_RESIDENCY_COUNT]; HARDWARE_FAULT_ACCESS_COUNT];
        HARDWARE_FAULT_BACKING_COUNT],
}

#[cfg(feature = "performance-profile")]
impl HardwareFaultCpuCounters {
    const fn new() -> Self {
        Self {
            counts: [const {
                [const { [const { AtomicU64::new(0) }; HARDWARE_FAULT_RESIDENCY_COUNT] };
                    HARDWARE_FAULT_ACCESS_COUNT]
            }; HARDWARE_FAULT_BACKING_COUNT],
        }
    }
}

#[cfg(feature = "performance-profile")]
static HARDWARE_FAULT_COUNTERS: [HardwareFaultCpuCounters; sched::NR_CPUS] =
    [const { HardwareFaultCpuCounters::new() }; sched::NR_CPUS];

#[cfg(feature = "performance-profile")]
#[inline]
fn record_hardware_user_fault(
    backing: HardwareFaultBacking,
    access: HardwareFaultAccess,
    resident: bool,
) {
    let cpu = sched::current_cpu_id().min(sched::NR_CPUS - 1);
    let counter =
        &HARDWARE_FAULT_COUNTERS[cpu].counts[backing as usize][access as usize][resident as usize];
    // 缺页路径不会在本 CPU 上抢占或复入；用单写 relaxed load/store 避免 QEMU
    // 为无竞争 fetch_add 模拟昂贵的原子 RMW，同时允许其它 CPU 读取累计快照。
    let value = counter.load(Ordering::Relaxed);
    counter.store(value.wrapping_add(1), Ordering::Relaxed);
}

#[cfg(feature = "performance-profile")]
pub(crate) fn hardware_fault_diag() -> HardwareFaultDiag {
    let mut diag = HardwareFaultDiag::default();
    for counters in &HARDWARE_FAULT_COUNTERS {
        for backing in HardwareFaultBacking::ALL {
            for access in HardwareFaultAccess::ALL {
                for resident in [false, true] {
                    diag.counts[backing as usize][access as usize][resident as usize] = diag.counts
                        [backing as usize][access as usize][resident as usize]
                        .saturating_add(
                            counters.counts[backing as usize][access as usize][resident as usize]
                                .load(Ordering::Relaxed),
                        );
                }
            }
        }
    }
    diag
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FaultAroundDiag {
    pub windows: u64,
    pub requested_pages: u64,
    pub prepared_pages: u64,
    pub commits: u64,
    pub installed_pages: u64,
    pub raced_commits: u64,
    pub collision_windows: u64,
    pub duplicate_pages: u64,
    pub discarded_unmapped_pages: u64,
    pub vma_retry_pages: u64,
    pub raced_pages: u64,
    pub map_failed_pages: u64,
}

#[cfg(feature = "performance-profile")]
#[repr(align(64))]
struct FaultAroundCpuCounters {
    windows: AtomicU64,
    requested_pages: AtomicU64,
    prepared_pages: AtomicU64,
    commits: AtomicU64,
    installed_pages: AtomicU64,
    raced_commits: AtomicU64,
    collision_windows: AtomicU64,
    duplicate_pages: AtomicU64,
    discarded_unmapped_pages: AtomicU64,
    vma_retry_pages: AtomicU64,
    raced_pages: AtomicU64,
    map_failed_pages: AtomicU64,
}

#[cfg(feature = "performance-profile")]
impl FaultAroundCpuCounters {
    const fn new() -> Self {
        Self {
            windows: AtomicU64::new(0),
            requested_pages: AtomicU64::new(0),
            prepared_pages: AtomicU64::new(0),
            commits: AtomicU64::new(0),
            installed_pages: AtomicU64::new(0),
            raced_commits: AtomicU64::new(0),
            collision_windows: AtomicU64::new(0),
            duplicate_pages: AtomicU64::new(0),
            discarded_unmapped_pages: AtomicU64::new(0),
            vma_retry_pages: AtomicU64::new(0),
            raced_pages: AtomicU64::new(0),
            map_failed_pages: AtomicU64::new(0),
        }
    }
}

#[cfg(feature = "performance-profile")]
static FAULT_AROUND_COUNTERS: [FaultAroundCpuCounters; sched::NR_CPUS] =
    [const { FaultAroundCpuCounters::new() }; sched::NR_CPUS];

#[cfg(feature = "performance-profile")]
fn fault_around_cpu_counters() -> &'static FaultAroundCpuCounters {
    &FAULT_AROUND_COUNTERS[sched::current_cpu_id().min(sched::NR_CPUS - 1)]
}

#[cfg(feature = "performance-profile")]
#[inline]
fn add_local_fault_around_counter(counter: &AtomicU64, delta: u64) {
    // 当前内核不会在内核态抢占，fault-around 也不会从中断路径复入；因此每个
    // CPU 槽只有本 CPU 单写。保留原子 load/store 允许其它 CPU 并发读取快照，
    // 同时避免 LoongArch/QEMU 为无需竞争的 fetch_add 执行昂贵的原子 RMW。
    let value = counter.load(Ordering::Relaxed);
    counter.store(value.wrapping_add(delta), Ordering::Relaxed);
}

#[cfg(feature = "performance-profile")]
fn record_fault_around_prepare(requested: usize, prepared: usize) {
    let counters = fault_around_cpu_counters();
    add_local_fault_around_counter(&counters.windows, 1);
    add_local_fault_around_counter(&counters.requested_pages, requested as u64);
    add_local_fault_around_counter(&counters.prepared_pages, prepared as u64);
}

#[cfg(feature = "performance-profile")]
fn record_fault_around_commit(installed: usize, raced: bool) {
    let counters = fault_around_cpu_counters();
    add_local_fault_around_counter(&counters.commits, 1);
    add_local_fault_around_counter(&counters.installed_pages, installed as u64);
    if raced {
        add_local_fault_around_counter(&counters.raced_commits, 1);
    }
}

#[cfg(feature = "performance-profile")]
fn record_fault_around_collision(duplicate: usize, discarded_unmapped: usize) {
    let counters = fault_around_cpu_counters();
    add_local_fault_around_counter(&counters.collision_windows, 1);
    add_local_fault_around_counter(&counters.duplicate_pages, duplicate as u64);
    add_local_fault_around_counter(
        &counters.discarded_unmapped_pages,
        discarded_unmapped as u64,
    );
}

#[cfg(feature = "performance-profile")]
fn record_fault_around_vma_retry(prepared: usize) {
    add_local_fault_around_counter(
        &fault_around_cpu_counters().vma_retry_pages,
        prepared as u64,
    );
}

#[cfg(feature = "performance-profile")]
fn record_fault_around_raced_pages(prepared: usize) {
    add_local_fault_around_counter(&fault_around_cpu_counters().raced_pages, prepared as u64);
}

#[cfg(feature = "performance-profile")]
fn record_fault_around_map_failed_pages(pages: usize) {
    add_local_fault_around_counter(&fault_around_cpu_counters().map_failed_pages, pages as u64);
}

pub(crate) fn fault_around_diag() -> FaultAroundDiag {
    #[cfg(feature = "performance-profile")]
    let mut diag = FaultAroundDiag::default();
    #[cfg(not(feature = "performance-profile"))]
    let diag = FaultAroundDiag::default();
    #[cfg(feature = "performance-profile")]
    for counters in &FAULT_AROUND_COUNTERS {
        diag.windows = diag
            .windows
            .saturating_add(counters.windows.load(Ordering::Relaxed));
        diag.requested_pages = diag
            .requested_pages
            .saturating_add(counters.requested_pages.load(Ordering::Relaxed));
        diag.prepared_pages = diag
            .prepared_pages
            .saturating_add(counters.prepared_pages.load(Ordering::Relaxed));
        diag.commits = diag
            .commits
            .saturating_add(counters.commits.load(Ordering::Relaxed));
        diag.installed_pages = diag
            .installed_pages
            .saturating_add(counters.installed_pages.load(Ordering::Relaxed));
        diag.raced_commits = diag
            .raced_commits
            .saturating_add(counters.raced_commits.load(Ordering::Relaxed));
        diag.collision_windows = diag
            .collision_windows
            .saturating_add(counters.collision_windows.load(Ordering::Relaxed));
        diag.duplicate_pages = diag
            .duplicate_pages
            .saturating_add(counters.duplicate_pages.load(Ordering::Relaxed));
        diag.discarded_unmapped_pages = diag
            .discarded_unmapped_pages
            .saturating_add(counters.discarded_unmapped_pages.load(Ordering::Relaxed));
        diag.vma_retry_pages = diag
            .vma_retry_pages
            .saturating_add(counters.vma_retry_pages.load(Ordering::Relaxed));
        diag.raced_pages = diag
            .raced_pages
            .saturating_add(counters.raced_pages.load(Ordering::Relaxed));
        diag.map_failed_pages = diag
            .map_failed_pages
            .saturating_add(counters.map_failed_pages.load(Ordering::Relaxed));
    }
    diag
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AnonFaultAroundDiag {
    pub windows: u64,
    pub requested_pages: u64,
    pub prepared_pages: u64,
    pub allocation_shortfall_pages: u64,
    pub reserve_fallbacks: u64,
    pub vma_retry_pages: u64,
    pub raced_pages: u64,
    pub invariant_failure_pages: u64,
    pub collision_discarded_pages: u64,
    pub map_discarded_pages: u64,
    pub installed_pages: u64,
    pub commits: u64,
    pub partial_commits: u64,
    pub map_failures: u64,
}

#[cfg(feature = "performance-profile")]
#[repr(align(64))]
struct AnonFaultAroundCpuCounters {
    windows: AtomicU64,
    requested_pages: AtomicU64,
    prepared_pages: AtomicU64,
    allocation_shortfall_pages: AtomicU64,
    reserve_fallbacks: AtomicU64,
    vma_retry_pages: AtomicU64,
    raced_pages: AtomicU64,
    invariant_failure_pages: AtomicU64,
    collision_discarded_pages: AtomicU64,
    map_discarded_pages: AtomicU64,
    installed_pages: AtomicU64,
    commits: AtomicU64,
    partial_commits: AtomicU64,
    map_failures: AtomicU64,
}

#[cfg(feature = "performance-profile")]
impl AnonFaultAroundCpuCounters {
    const fn new() -> Self {
        Self {
            windows: AtomicU64::new(0),
            requested_pages: AtomicU64::new(0),
            prepared_pages: AtomicU64::new(0),
            allocation_shortfall_pages: AtomicU64::new(0),
            reserve_fallbacks: AtomicU64::new(0),
            vma_retry_pages: AtomicU64::new(0),
            raced_pages: AtomicU64::new(0),
            invariant_failure_pages: AtomicU64::new(0),
            collision_discarded_pages: AtomicU64::new(0),
            map_discarded_pages: AtomicU64::new(0),
            installed_pages: AtomicU64::new(0),
            commits: AtomicU64::new(0),
            partial_commits: AtomicU64::new(0),
            map_failures: AtomicU64::new(0),
        }
    }
}

#[cfg(feature = "performance-profile")]
static ANON_FAULT_AROUND_COUNTERS: [AnonFaultAroundCpuCounters; sched::NR_CPUS] =
    [const { AnonFaultAroundCpuCounters::new() }; sched::NR_CPUS];

#[cfg(feature = "performance-profile")]
fn anon_fault_around_cpu_counters() -> &'static AnonFaultAroundCpuCounters {
    &ANON_FAULT_AROUND_COUNTERS[sched::current_cpu_id().min(sched::NR_CPUS - 1)]
}

#[cfg(feature = "performance-profile")]
fn record_anon_fault_around_prepare(requested: usize, prepared: usize, reserve_fallback: bool) {
    let counters = anon_fault_around_cpu_counters();
    add_local_fault_around_counter(&counters.windows, 1);
    add_local_fault_around_counter(&counters.requested_pages, requested as u64);
    add_local_fault_around_counter(&counters.prepared_pages, prepared as u64);
    add_local_fault_around_counter(
        &counters.allocation_shortfall_pages,
        requested.saturating_sub(prepared) as u64,
    );
    if reserve_fallback {
        add_local_fault_around_counter(&counters.reserve_fallbacks, 1);
    }
}

#[cfg(feature = "performance-profile")]
fn record_anon_fault_around_discard(counter: &AtomicU64, pages: usize) {
    add_local_fault_around_counter(counter, pages as u64);
}

#[cfg(feature = "performance-profile")]
fn record_anon_fault_around_commit(
    installed: usize,
    collision_discarded: usize,
    map_discarded: usize,
    map_failed: bool,
) {
    let counters = anon_fault_around_cpu_counters();
    add_local_fault_around_counter(&counters.commits, 1);
    add_local_fault_around_counter(&counters.installed_pages, installed as u64);
    add_local_fault_around_counter(
        &counters.collision_discarded_pages,
        collision_discarded as u64,
    );
    add_local_fault_around_counter(&counters.map_discarded_pages, map_discarded as u64);
    if collision_discarded != 0 || map_discarded != 0 {
        add_local_fault_around_counter(&counters.partial_commits, 1);
    }
    if map_failed {
        add_local_fault_around_counter(&counters.map_failures, 1);
    }
}

pub(crate) fn anon_fault_around_diag() -> AnonFaultAroundDiag {
    #[cfg(feature = "performance-profile")]
    let mut diag = AnonFaultAroundDiag::default();
    #[cfg(not(feature = "performance-profile"))]
    let diag = AnonFaultAroundDiag::default();
    #[cfg(feature = "performance-profile")]
    for counters in &ANON_FAULT_AROUND_COUNTERS {
        diag.windows = diag
            .windows
            .saturating_add(counters.windows.load(Ordering::Relaxed));
        diag.requested_pages = diag
            .requested_pages
            .saturating_add(counters.requested_pages.load(Ordering::Relaxed));
        diag.prepared_pages = diag
            .prepared_pages
            .saturating_add(counters.prepared_pages.load(Ordering::Relaxed));
        diag.allocation_shortfall_pages = diag
            .allocation_shortfall_pages
            .saturating_add(counters.allocation_shortfall_pages.load(Ordering::Relaxed));
        diag.reserve_fallbacks = diag
            .reserve_fallbacks
            .saturating_add(counters.reserve_fallbacks.load(Ordering::Relaxed));
        diag.vma_retry_pages = diag
            .vma_retry_pages
            .saturating_add(counters.vma_retry_pages.load(Ordering::Relaxed));
        diag.raced_pages = diag
            .raced_pages
            .saturating_add(counters.raced_pages.load(Ordering::Relaxed));
        diag.invariant_failure_pages = diag
            .invariant_failure_pages
            .saturating_add(counters.invariant_failure_pages.load(Ordering::Relaxed));
        diag.collision_discarded_pages = diag
            .collision_discarded_pages
            .saturating_add(counters.collision_discarded_pages.load(Ordering::Relaxed));
        diag.map_discarded_pages = diag
            .map_discarded_pages
            .saturating_add(counters.map_discarded_pages.load(Ordering::Relaxed));
        diag.installed_pages = diag
            .installed_pages
            .saturating_add(counters.installed_pages.load(Ordering::Relaxed));
        diag.commits = diag
            .commits
            .saturating_add(counters.commits.load(Ordering::Relaxed));
        diag.partial_commits = diag
            .partial_commits
            .saturating_add(counters.partial_commits.load(Ordering::Relaxed));
        diag.map_failures = diag
            .map_failures
            .saturating_add(counters.map_failures.load(Ordering::Relaxed));
    }
    diag
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSegmentPlan {
    mapping: Range<usize>,
    lazy_file: Range<usize>,
    lazy_file_offset: u64,
    fragment_pages: [usize; 2],
    fragment_count: usize,
}

impl FileSegmentPlan {
    fn fragments(&self) -> &[usize] {
        &self.fragment_pages[..self.fragment_count]
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VmSpaceDiag {
    pub live: usize,
    pub created: usize,
    pub dropped: usize,
    pub private_file_pressure_reclaims: u64,
}

#[kernel_symbols::export(name = "general.mm.vm_space_diag", contract = "kernel.mm.diagnostic@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC)]
pub fn vm_space_diag() -> VmSpaceDiag {
    let private_file_cache = private_file_page_cache_diag();
    VmSpaceDiag {
        live: VM_SPACE_LIVE.load(Ordering::Acquire),
        created: VM_SPACE_CREATED.load(Ordering::Acquire),
        dropped: VM_SPACE_DROPPED.load(Ordering::Acquire),
        private_file_pressure_reclaims: private_file_cache.pressure_reclaims,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FilePageKey {
    file_key: usize,
    offset: u64,
    generation: u64,
}

#[derive(Clone, Copy)]
struct PrivateFilePageHashes {
    table: u64,
}

struct PrivateFilePageCacheReady {
    page: Arc<ResidentPage>,
    referenced: bool,
}

enum PrivateFilePageCacheEntry {
    Loading {
        id: u64,
        waiters: usize,
    },
    Failed {
        id: u64,
        error: Errno,
        remaining: usize,
    },
    Ready(PrivateFilePageCacheReady),
}

type PrivateFilePageTableEntry = (FilePageKey, PrivateFilePageCacheEntry);

struct PrivateFilePageTable {
    entries: HashTable<PrivateFilePageTableEntry>,
}

enum PrivateFilePageCacheClaim<'a, const SHARD_COUNT: usize> {
    Ready(Arc<ResidentPage>),
    Loading(PrivateFilePageLoadWait<'a, SHARD_COUNT>),
    Failed(Errno),
    Owner(u64),
    Bypass,
}

enum PrivateFilePageCacheStateClaim {
    Ready(Arc<ResidentPage>),
    Loading(u64),
    Failed(Errno),
    Owner(u64),
    Bypass,
}

/// 单个私有文件页缓存分片。
///
/// `pages` 提供按文件代际和偏移查找，`clock` 实现 second-chance 淘汰。缓存只
/// 持有固定数量的强引用，使短生命周期编译进程退出后仍可复用工具链和 crate 页，
/// 同时避免长期构建把所有历史文件内容永久钉在内存中。
struct PrivateFilePageCacheState {
    pages: PrivateFilePageTable,
    clock: VecDeque<FilePageKey>,
    ready_pages: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
    pressure_reclaims: u64,
    load_leaders: u64,
    load_waiters: u64,
    load_errors: u64,
}

/// 有界的私有干净文件页强缓存。
///
/// 完整的文件身份、偏移和代际经过稳定混合后选择分片，使不同 rustc 进程的并行
/// 缺页通常只竞争各自分片。容量按分片精确拆分，压力回收则轮换起始分片。
struct ShardedPrivateFilePageCache<const SHARD_COUNT: usize> {
    shards: [Spinlock<PrivateFilePageCacheState>; SHARD_COUNT],
    load_waits: [WaitQueue; PRIVATE_FILE_LOAD_WAIT_BUCKETS],
    next_load_id: AtomicU64,
    capacity: usize,
    reclaim_shard: AtomicUsize,
}

/// 已在 `Loading.waiters` 中登记的栈上等待句柄。
///
/// 未调用 [`Self::wait`] 就离开作用域时会自动撤销登记；若 owner 已发布错误，
/// 则同时消费对应 `Failed.remaining`，避免批量探测分支遗留失败条目。
struct PrivateFilePageLoadWait<'a, const SHARD_COUNT: usize> {
    cache: &'a ShardedPrivateFilePageCache<SHARD_COUNT>,
    key: FilePageKey,
    id: u64,
    active: bool,
}

impl<const SHARD_COUNT: usize> PrivateFilePageLoadWait<'_, SHARD_COUNT> {
    fn wait(mut self) -> Result<Option<Arc<ResidentPage>>, Errno> {
        let result = self.cache.wait_for_load(self.key, self.id);
        self.active = false;
        result
    }

    #[cfg(test)]
    fn id(&self) -> u64 {
        self.id
    }
}

impl<const SHARD_COUNT: usize> Drop for PrivateFilePageLoadWait<'_, SHARD_COUNT> {
    fn drop(&mut self) {
        if self.active {
            self.cache.cancel_load_waiter(self.key, self.id);
        }
    }
}

/// 分配全局唯一且永不复用的加载编号。
///
/// 计数耗尽后返回 `None` 并永久退回不缓存读取；不允许回绕，因此等待者对
/// `(FilePageKey, load_id)` 的比较不存在 ABA。
fn next_private_file_load_id(next: &AtomicU64) -> Option<u64> {
    next.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        current.checked_add(1)
    })
    .ok()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PrivateFilePageCacheDiag {
    pub pages: usize,
    pub capacity: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub pressure_reclaims: u64,
    pub load_leaders: u64,
    pub load_waiters: u64,
    pub load_errors: u64,
}

impl FilePageKey {
    fn new(file: &Arc<dyn FileLike>, offset: u64, generation: u64) -> Self {
        Self {
            file_key: file.cache_key(),
            offset,
            generation,
        }
    }

    fn new_private(file_key: usize, offset: u64, generation: u64) -> Self {
        Self {
            file_key,
            offset,
            generation,
        }
    }

    /// 对完整缓存身份做轻量、与平台无关的稳定混合，供强缓存选择分片。
    ///
    /// 这里故意只使用移位、旋转和异或：缺页路径会为每个候选页执行一次选片，
    /// 在 LoongArch TCG 上避免几次 64 位乘法比更重的通用哈希更重要。
    #[inline]
    fn private_cache_hash(self) -> u64 {
        let mut hash = self.file_key as u64;
        // 缓存键的文件偏移按 4 KiB 页对齐；直接把页号放入低位，使同一大型
        // 工具链映像的连续页也能分散到不同分片，而不是只按 inode 聚集。
        let page_index = self.offset >> 12;
        hash ^= self.offset ^ page_index ^ page_index.rotate_left(17);
        hash ^= self.generation.rotate_left(37) ^ (self.generation >> 11);
        hash ^= hash >> 29;
        hash ^ (hash >> 17)
    }

    #[inline]
    fn private_cache_hashes(self) -> PrivateFilePageHashes {
        let cache = self.private_cache_hash();
        PrivateFilePageHashes {
            table: private_table_hash_from_cache_hash(cache),
        }
    }

    /// 按文件代际和粗粒度文件块选择 Ready 范围所在的缓存分片。
    ///
    /// 页内偏移仍参与分片内 HashTable 的完整哈希；这里只让相邻页共享分片锁，
    /// 使 fault-around 能一次取得连续 Ready 页而不改变条目状态机语义。
    #[inline]
    fn private_cache_shard_hash(self) -> u64 {
        let chunk = self.offset / PRIVATE_FILE_CACHE_SHARD_CHUNK_BYTES;
        let mut hash = self.file_key as u64;
        hash ^= self.generation.rotate_left(37) ^ (self.generation >> 11);
        hash ^= chunk.rotate_left(17) ^ (chunk >> 7);
        hash ^= hash >> 29;
        hash ^ (hash >> 17)
    }

    /// 为分片内 SwissTable 生成独立哈希。
    ///
    /// 分片已经消费了 `private_cache_hash` 的低位；再次直接使用同一哈希会让同一
    /// 分片的所有初始 bucket 共享这些低位。这里在选片之后使用一次奇数乘法和旋转
    /// avalanche，同时打散 bucket 与 7-bit control tag；选片热路径本身仍不做乘法。
    #[inline]
    fn private_table_hash(self) -> u64 {
        private_table_hash_from_cache_hash(self.private_cache_hash())
    }
}

#[inline]
fn private_table_hash_from_cache_hash(cache_hash: u64) -> u64 {
    let hash = cache_hash.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    hash ^ hash.rotate_right(29)
}

#[inline]
fn private_file_page_table_entry_hash(entry: &PrivateFilePageTableEntry) -> u64 {
    entry.0.private_table_hash()
}

impl PrivateFilePageTable {
    const fn new() -> Self {
        Self {
            entries: HashTable::new(),
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[inline]
    fn get(&self, key: &FilePageKey) -> Option<&PrivateFilePageCacheEntry> {
        self.entries
            .find(key.private_table_hash(), |entry| entry.0 == *key)
            .map(|entry| &entry.1)
    }

    #[inline]
    fn get_mut(&mut self, key: &FilePageKey) -> Option<&mut PrivateFilePageCacheEntry> {
        self.get_mut_hashed(key, key.private_table_hash())
    }

    #[inline]
    fn get_mut_hashed(
        &mut self,
        key: &FilePageKey,
        table_hash: u64,
    ) -> Option<&mut PrivateFilePageCacheEntry> {
        self.entries
            .find_mut(table_hash, |entry| entry.0 == *key)
            .map(|entry| &mut entry.1)
    }

    #[cfg(test)]
    fn contains_key(&self, key: &FilePageKey) -> bool {
        self.get(key).is_some()
    }

    fn remove(&mut self, key: &FilePageKey) -> Option<PrivateFilePageCacheEntry> {
        let entry = self
            .entries
            .find_entry(key.private_table_hash(), |entry| entry.0 == *key)
            .ok()?;
        let ((_, value), _) = entry.remove();
        Some(value)
    }

    fn iter(&self) -> impl Iterator<Item = (&FilePageKey, &PrivateFilePageCacheEntry)> {
        self.entries.iter().map(|entry| (&entry.0, &entry.1))
    }

    #[inline]
    fn insertion_needs_reserve(&self) -> bool {
        self.entries.len() == self.entries.capacity()
    }

    fn try_reserve_one(&mut self) -> bool {
        self.entries
            .try_reserve(1, private_file_page_table_entry_hash)
            .is_ok()
    }

    fn insert_unique_hashed(
        &mut self,
        key: FilePageKey,
        table_hash: u64,
        entry: PrivateFilePageCacheEntry,
    ) {
        self.entries
            .insert_unique(table_hash, (key, entry), private_file_page_table_entry_hash);
    }
}

impl PrivateFilePageCacheState {
    const fn new() -> Self {
        Self {
            pages: PrivateFilePageTable::new(),
            clock: VecDeque::new(),
            ready_pages: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
            pressure_reclaims: 0,
            load_leaders: 0,
            load_waiters: 0,
            load_errors: 0,
        }
    }

    #[cfg(test)]
    fn find(&mut self, key: FilePageKey) -> Option<Arc<ResidentPage>> {
        let page = self.find_existing(key);
        if page.is_some() {
            self.hits = self.hits.saturating_add(1);
        } else {
            self.misses = self.misses.saturating_add(1);
        }
        page
    }

    #[cfg(test)]
    fn find_existing(&mut self, key: FilePageKey) -> Option<Arc<ResidentPage>> {
        let PrivateFilePageCacheEntry::Ready(entry) = self.pages.get_mut(&key)? else {
            return None;
        };
        entry.referenced = true;
        Some(Arc::clone(&entry.page))
    }

    fn claim_existing(
        &mut self,
        key: FilePageKey,
        table_hash: u64,
    ) -> Option<PrivateFilePageCacheStateClaim> {
        let entry = self.pages.get_mut_hashed(&key, table_hash)?;
        Some(match entry {
            PrivateFilePageCacheEntry::Ready(entry) => {
                entry.referenced = true;
                self.hits = self.hits.saturating_add(1);
                PrivateFilePageCacheStateClaim::Ready(Arc::clone(&entry.page))
            }
            PrivateFilePageCacheEntry::Loading { id, waiters } => {
                self.misses = self.misses.saturating_add(1);
                let Some(next_waiters) = waiters.checked_add(1) else {
                    return Some(PrivateFilePageCacheStateClaim::Failed(Errno::ENOMEM));
                };
                *waiters = next_waiters;
                PrivateFilePageCacheStateClaim::Loading(*id)
            }
            PrivateFilePageCacheEntry::Failed { error, .. } => {
                self.misses = self.misses.saturating_add(1);
                PrivateFilePageCacheStateClaim::Failed(*error)
            }
        })
    }

    fn claim(
        &mut self,
        key: FilePageKey,
        table_hash: u64,
        next_load_id: &AtomicU64,
    ) -> PrivateFilePageCacheStateClaim {
        // BuildStorm 的缓存命中占绝大多数。先走只查询路径，避免命中时构造带
        // reserve 语义的 HashTable entry；分片锁保证 miss 后不会出现并发插入。
        if let Some(existing) = self.claim_existing(key, table_hash) {
            return existing;
        }
        self.misses = self.misses.saturating_add(1);
        if self.pages.insertion_needs_reserve() && !self.pages.try_reserve_one() {
            return PrivateFilePageCacheStateClaim::Bypass;
        }
        let Some(id) = next_private_file_load_id(next_load_id) else {
            return PrivateFilePageCacheStateClaim::Bypass;
        };
        self.pages.insert_unique_hashed(
            key,
            table_hash,
            PrivateFilePageCacheEntry::Loading { id, waiters: 0 },
        );
        self.load_leaders = self.load_leaders.saturating_add(1);
        PrivateFilePageCacheStateClaim::Owner(id)
    }

    fn finish_load(
        &mut self,
        key: FilePageKey,
        load_id: u64,
        page: &Arc<ResidentPage>,
        capacity: usize,
    ) -> (bool, Option<Arc<ResidentPage>>) {
        let Some(entry) = self.pages.get_mut(&key) else {
            return (false, None);
        };
        if !matches!(entry, PrivateFilePageCacheEntry::Loading { id, .. } if *id == load_id) {
            return (false, None);
        }
        *entry = PrivateFilePageCacheEntry::Ready(PrivateFilePageCacheReady {
            page: Arc::clone(page),
            referenced: false,
        });
        self.ready_pages += 1;
        self.clock.push_back(key);
        let retired = (self.ready_pages > capacity)
            .then(|| self.evict_one())
            .flatten();
        (true, retired)
    }

    fn abort_load(&mut self, key: FilePageKey, load_id: u64, error: Option<Errno>) -> bool {
        let Some(entry) = self.pages.get_mut(&key) else {
            return false;
        };
        let PrivateFilePageCacheEntry::Loading { id, waiters } = entry else {
            return false;
        };
        if *id != load_id {
            return false;
        }
        let waiters = *waiters;
        if let Some(error) = error
            && waiters != 0
        {
            *entry = PrivateFilePageCacheEntry::Failed {
                id: load_id,
                error,
                remaining: waiters,
            };
        } else {
            self.pages.remove(&key);
        }
        if error.is_some() {
            self.load_errors = self.load_errors.saturating_add(1);
        }
        true
    }

    fn cancel_waiter(&mut self, key: FilePageKey, load_id: u64) {
        let remove_failed = match self.pages.get_mut(&key) {
            Some(PrivateFilePageCacheEntry::Loading { id, waiters }) if *id == load_id => {
                debug_assert!(*waiters != 0);
                *waiters = waiters.saturating_sub(1);
                false
            }
            Some(PrivateFilePageCacheEntry::Failed { id, remaining, .. }) if *id == load_id => {
                debug_assert!(*remaining != 0);
                *remaining = remaining.saturating_sub(1);
                *remaining == 0
            }
            _ => false,
        };
        if remove_failed {
            self.pages.remove(&key);
        }
    }

    fn load_pending(&self, key: FilePageKey, load_id: u64) -> bool {
        matches!(
            self.pages.get(&key),
            Some(PrivateFilePageCacheEntry::Loading { id, .. }) if *id == load_id
        )
    }

    fn consume_load_result(
        &mut self,
        key: FilePageKey,
        load_id: u64,
    ) -> Result<Option<Arc<ResidentPage>>, Errno> {
        let mut remove_failed = false;
        let result = match self.pages.get_mut(&key) {
            Some(PrivateFilePageCacheEntry::Ready(entry)) => Ok(Some(Arc::clone(&entry.page))),
            Some(PrivateFilePageCacheEntry::Failed {
                id,
                error,
                remaining,
            }) if *id == load_id => {
                debug_assert!(*remaining != 0);
                let error = *error;
                *remaining = remaining.saturating_sub(1);
                remove_failed = *remaining == 0;
                Err(error)
            }
            _ => Ok(None),
        };
        if remove_failed {
            self.pages.remove(&key);
        }
        result
    }

    /// 清理一个未被近期访问的页。调用者必须在释放返回的 Arc 前放开缓存锁。
    fn evict_one(&mut self) -> Option<Arc<ResidentPage>> {
        // second chance 只近似表达近期复用。分片缺页锁内必须保持固定上界，
        // 即使整个缓存都很热也不能扫描数万个缓存条目。
        let scans = self.clock.len().min(PRIVATE_FILE_CACHE_EVICTION_SCAN_LIMIT);
        for _ in 0..scans {
            let Some(key) = self.clock.pop_front() else {
                break;
            };
            let Some(entry) = self.pages.get_mut(&key) else {
                // 仅用于容忍测试/恢复路径留下的旧 clock 节点。
                continue;
            };
            let PrivateFilePageCacheEntry::Ready(entry) = entry else {
                continue;
            };
            if entry.referenced {
                entry.referenced = false;
                self.clock.push_back(key);
                continue;
            }
            // `remove` 把 Arc 移到锁外；不要让 map entry 在锁守卫仍存活时析构。
            return self.remove_ready(key);
        }

        // 所有受检条目都获得了 second chance 时，固定淘汰下一个最老条目；
        // 容量不变量和缺页前进性比精确 LRU 更重要。
        self.evict_oldest()
    }

    /// 无视 reference 位移除最老条目，供容量兜底和内存压力回收使用。
    fn evict_oldest(&mut self) -> Option<Arc<ResidentPage>> {
        while let Some(key) = self.clock.pop_front() {
            if let Some(page) = self.remove_ready(key) {
                return Some(page);
            }
        }
        // clock 元数据若意外缺项，仍保证 map 不会永久失去可回收性。
        let key = self.pages.iter().find_map(|(key, entry)| {
            matches!(entry, PrivateFilePageCacheEntry::Ready(_)).then_some(*key)
        })?;
        self.remove_ready(key)
    }

    fn reclaim_oldest(&mut self) -> Option<Arc<ResidentPage>> {
        let page = self.evict_oldest()?;
        self.pressure_reclaims = self.pressure_reclaims.saturating_add(1);
        Some(page)
    }

    fn remove_ready(&mut self, key: FilePageKey) -> Option<Arc<ResidentPage>> {
        if !matches!(
            self.pages.get(&key),
            Some(PrivateFilePageCacheEntry::Ready(_))
        ) {
            return None;
        }
        let PrivateFilePageCacheEntry::Ready(entry) = self.pages.remove(&key)? else {
            unreachable!("entry kind was checked while holding the cache shard lock");
        };
        self.ready_pages = self.ready_pages.saturating_sub(1);
        self.evictions = self.evictions.saturating_add(1);
        memstat::record_file_evict();
        Some(entry.page)
    }

    fn remove_if_same(
        &mut self,
        key: FilePageKey,
        page: &ResidentPage,
    ) -> Option<Arc<ResidentPage>> {
        let same = self
            .pages
            .get(&key)
            .is_some_and(|entry| {
                matches!(entry, PrivateFilePageCacheEntry::Ready(entry) if core::ptr::eq(entry.page.as_ref(), page))
            });
        if !same {
            return None;
        }

        // 代际校验失败会走这里主动撤销刚发布的候选。同步摘除 clock 节点，避免
        // 文件反复变化时累积陈旧 key，最终让压力回收在分片锁内无界扫描。
        self.clock.retain(|queued| *queued != key);
        self.remove_ready(key)
    }
}

impl<const SHARD_COUNT: usize> ShardedPrivateFilePageCache<SHARD_COUNT> {
    const fn new(capacity: usize) -> Self {
        assert!(SHARD_COUNT > 0);
        assert!(SHARD_COUNT.is_power_of_two());
        assert!(PRIVATE_FILE_LOAD_WAIT_BUCKETS.is_power_of_two());
        Self {
            shards: [const { Spinlock::new(PrivateFilePageCacheState::new()) }; SHARD_COUNT],
            load_waits: [const { WaitQueue::new_with_reason(WaitReason::BlockIo) };
                PRIVATE_FILE_LOAD_WAIT_BUCKETS],
            next_load_id: AtomicU64::new(1),
            capacity,
            reclaim_shard: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn shard_index(&self, key: FilePageKey) -> usize {
        (key.private_cache_shard_hash() as usize) & (SHARD_COUNT - 1)
    }

    fn shard_capacity(&self, index: usize) -> usize {
        self.capacity / SHARD_COUNT + usize::from(index < self.capacity % SHARD_COUNT)
    }

    #[cfg(test)]
    fn find(&self, key: FilePageKey) -> Option<Arc<ResidentPage>> {
        self.shards[self.shard_index(key)].lock().find(key)
    }

    fn load_wait_index(&self, key: FilePageKey, load_id: u64) -> usize {
        let hash =
            key.private_cache_hash() ^ load_id.rotate_left(23) ^ (load_id >> 17) ^ (load_id << 7);
        (hash as usize) & (PRIVATE_FILE_LOAD_WAIT_BUCKETS - 1)
    }

    fn claim(&self, key: FilePageKey) -> PrivateFilePageCacheClaim<'_, SHARD_COUNT> {
        let hashes = key.private_cache_hashes();
        let shard_index = self.shard_index(key);
        let claim = self.shards[shard_index]
            .lock()
            .claim(key, hashes.table, &self.next_load_id);
        self.wrap_claim(key, claim)
    }

    /// 返回从 `first_key` 开始的连续 Ready 页前缀。
    ///
    /// 每个粗粒度文件块只获取一次分片锁；遇到首个非 Ready 条目立即停止，剩余
    /// 页面继续由原有 claim/loading/waiter 路径处理。该方法只折叠 Ready 命中，
    /// 不创建 owner，也不改变 miss 和失败传播语义。
    fn ready_contiguous(
        &self,
        first_key: FilePageKey,
        page_size: usize,
        pages: usize,
    ) -> Option<PrivateFilePageBatch> {
        if page_size == 0
            || !page_size.is_power_of_two()
            || pages == 0
            || pages > PRIVATE_FILE_BATCH_MAX_PAGES
        {
            return None;
        }

        let mut ready = PrivateFilePageBatch::new();
        let mut index = 0usize;
        while index < pages {
            let offset = private_file_batch_page_offset(first_key.offset, index, page_size)?;
            let first = FilePageKey {
                offset,
                ..first_key
            };
            let shard_index = self.shard_index(first);
            let mut end = index + 1;
            while end < pages {
                let Some(offset) = private_file_batch_page_offset(first_key.offset, end, page_size)
                else {
                    break;
                };
                if self.shard_index(FilePageKey {
                    offset,
                    ..first_key
                }) != shard_index
                {
                    break;
                }
                end += 1;
            }

            let mut shard = self.shards[shard_index].lock();
            while index < end {
                let offset = private_file_batch_page_offset(first_key.offset, index, page_size)?;
                let key = FilePageKey {
                    offset,
                    ..first_key
                };
                let page = match shard.pages.get_mut_hashed(&key, key.private_table_hash()) {
                    Some(PrivateFilePageCacheEntry::Ready(entry)) => {
                        entry.referenced = true;
                        Arc::clone(&entry.page)
                    }
                    _ => return (!ready.is_empty()).then_some(ready),
                };
                shard.hits = shard.hits.saturating_add(1);
                ready.push(page);
                index += 1;
            }
        }
        debug_assert!(!ready.spilled());
        Some(ready)
    }

    #[cfg(test)]
    fn claim_batch(&self, keys: &[FilePageKey]) -> PrivateFilePageClaims<'_, SHARD_COUNT> {
        let mut hashes =
            SmallVec::<[PrivateFilePageHashes; PRIVATE_FILE_BATCH_MAX_PAGES]>::with_capacity(
                keys.len(),
            );
        let mut claims = SmallVec::<
            [Option<PrivateFilePageCacheStateClaim>; PRIVATE_FILE_BATCH_MAX_PAGES],
        >::with_capacity(keys.len());
        let mut shard_mask = 0usize;
        for key in keys {
            let key_hashes = key.private_cache_hashes();
            let shard = self.shard_index(*key);
            shard_mask |= 1usize << shard;
            hashes.push(key_hashes);
            claims.push(None);
        }

        while shard_mask != 0 {
            let shard_index = shard_mask.trailing_zeros() as usize;
            shard_mask &= shard_mask - 1;
            let mut shard = self.shards[shard_index].lock();
            for (index, (key, key_hashes)) in keys.iter().zip(&hashes).enumerate() {
                if self.shard_index(*key) != shard_index {
                    continue;
                }
                claims[index] = Some(shard.claim(*key, key_hashes.table, &self.next_load_id));
            }
        }

        keys.iter()
            .copied()
            .zip(claims)
            .map(|(key, claim)| {
                self.wrap_claim(key, claim.expect("batch claim must cover every input key"))
            })
            .collect()
    }

    #[cfg(test)]
    fn claim_batch_prefix(&self, keys: &[FilePageKey]) -> PrivateFilePageClaims<'_, SHARD_COUNT> {
        let mut claims = PrivateFilePageClaims::new();
        for key in keys {
            let claim = self.claim(*key);
            let owner = matches!(&claim, PrivateFilePageCacheClaim::Owner(_));
            claims.push(claim);
            if !owner {
                break;
            }
        }
        claims
    }

    fn claim_contiguous_prefix(
        &self,
        first_key: FilePageKey,
        page_size: usize,
        pages: usize,
    ) -> Option<PrivateFilePageClaimPrefix<'_, SHARD_COUNT>> {
        let mut owners = PrivateFilePageLoadOwners::new();
        for index in 0..pages {
            let Some(offset) = private_file_batch_page_offset(first_key.offset, index, page_size)
            else {
                for (key, load_id) in &owners {
                    self.abort_load(*key, *load_id, None);
                }
                return None;
            };
            let key = FilePageKey {
                offset,
                ..first_key
            };
            match self.claim(key) {
                PrivateFilePageCacheClaim::Owner(load_id) => owners.push((key, load_id)),
                terminal => {
                    return Some(PrivateFilePageClaimPrefix {
                        owners,
                        terminal: Some((index, terminal)),
                    });
                }
            }
        }
        Some(PrivateFilePageClaimPrefix {
            owners,
            terminal: None,
        })
    }

    fn wrap_claim(
        &self,
        key: FilePageKey,
        claim: PrivateFilePageCacheStateClaim,
    ) -> PrivateFilePageCacheClaim<'_, SHARD_COUNT> {
        match claim {
            PrivateFilePageCacheStateClaim::Ready(page) => PrivateFilePageCacheClaim::Ready(page),
            PrivateFilePageCacheStateClaim::Loading(id) => {
                PrivateFilePageCacheClaim::Loading(PrivateFilePageLoadWait {
                    cache: self,
                    key,
                    id,
                    active: true,
                })
            }
            PrivateFilePageCacheStateClaim::Failed(error) => {
                PrivateFilePageCacheClaim::Failed(error)
            }
            PrivateFilePageCacheStateClaim::Owner(id) => PrivateFilePageCacheClaim::Owner(id),
            PrivateFilePageCacheStateClaim::Bypass => PrivateFilePageCacheClaim::Bypass,
        }
    }

    fn finish_load(
        &self,
        key: FilePageKey,
        load_id: u64,
        page: Arc<ResidentPage>,
    ) -> Option<Arc<ResidentPage>> {
        let index = self.shard_index(key);
        let (owned, retired) =
            self.shards[index]
                .lock()
                .finish_load(key, load_id, &page, self.shard_capacity(index));
        drop(retired);
        if owned {
            self.wake_load(key, load_id);
            Some(page)
        } else {
            None
        }
    }

    fn abort_load(&self, key: FilePageKey, load_id: u64, error: Option<Errno>) {
        let index = self.shard_index(key);
        let removed = self.shards[index].lock().abort_load(key, load_id, error);
        if removed {
            self.wake_load(key, load_id);
        }
    }

    fn load_pending(&self, key: FilePageKey, load_id: u64) -> bool {
        self.shards[self.shard_index(key)]
            .lock()
            .load_pending(key, load_id)
    }

    fn cancel_load_waiter(&self, key: FilePageKey, load_id: u64) {
        self.shards[self.shard_index(key)]
            .lock()
            .cancel_waiter(key, load_id);
    }

    fn wait_for_load(
        &self,
        key: FilePageKey,
        load_id: u64,
    ) -> Result<Option<Arc<ResidentPage>>, Errno> {
        {
            let mut shard = self.shards[self.shard_index(key)].lock();
            shard.load_waiters = shard.load_waiters.saturating_add(1);
        }
        let wait_queue = &self.load_waits[self.load_wait_index(key, load_id)];
        if sched::is_ready() {
            let task = sched::current_task();
            wait_queue.wait_event(&task, || !self.load_pending(key, load_id));
        } else {
            while self.load_pending(key, load_id) {
                core::hint::spin_loop();
            }
        }
        self.shards[self.shard_index(key)]
            .lock()
            .consume_load_result(key, load_id)
    }

    fn wake_load(&self, key: FilePageKey, load_id: u64) {
        self.load_waits[self.load_wait_index(key, load_id)].wake_all();
    }

    /// 仅移除仍指向指定候选的旧代际条目，避免并发发布覆盖后误删新页面。
    fn remove_if_same(&self, key: FilePageKey, page: &ResidentPage) {
        let index = self.shard_index(key);
        let retired = self.shards[index].lock().remove_if_same(key, page);
        drop(retired);
    }

    /// 从轮换的起始分片批量摘取缓存引用，并在所有分片锁之外统一析构。
    fn reclaim(&self, limit: usize) -> usize {
        if limit == 0 {
            return 0;
        }
        let start = self.reclaim_shard.fetch_add(1, Ordering::Relaxed) % SHARD_COUNT;
        let mut retired = Vec::new();
        if retired.try_reserve_exact(limit).is_err() {
            return self.reclaim_unbatched(start, limit);
        }

        for offset in 0..SHARD_COUNT {
            let index = (start + offset) % SHARD_COUNT;
            let mut shard = self.shards[index].lock();
            while retired.len() < limit {
                let Some(page) = shard.reclaim_oldest() else {
                    break;
                };
                retired.push(page);
            }
            if retired.len() == limit {
                break;
            }
        }
        let reclaimed = retired.len();
        drop(retired);
        reclaimed
    }

    /// 仅在回收批次的临时 Vec 无法分配时使用；每次仍先释放锁再析构页面。
    fn reclaim_unbatched(&self, start: usize, limit: usize) -> usize {
        let mut reclaimed = 0usize;
        for offset in 0..SHARD_COUNT {
            let index = (start + offset) % SHARD_COUNT;
            while reclaimed < limit {
                let retired = self.shards[index].lock().reclaim_oldest();
                let Some(retired) = retired else {
                    break;
                };
                drop(retired);
                reclaimed += 1;
            }
            if reclaimed == limit {
                break;
            }
        }
        reclaimed
    }

    fn diag(&self) -> PrivateFilePageCacheDiag {
        let mut diag = PrivateFilePageCacheDiag {
            capacity: self.capacity,
            ..PrivateFilePageCacheDiag::default()
        };
        for shard in &self.shards {
            let shard = shard.lock();
            diag.pages = diag.pages.saturating_add(shard.ready_pages);
            diag.hits = diag.hits.saturating_add(shard.hits);
            diag.misses = diag.misses.saturating_add(shard.misses);
            diag.evictions = diag.evictions.saturating_add(shard.evictions);
            diag.pressure_reclaims = diag
                .pressure_reclaims
                .saturating_add(shard.pressure_reclaims);
            diag.load_leaders = diag.load_leaders.saturating_add(shard.load_leaders);
            diag.load_waiters = diag.load_waiters.saturating_add(shard.load_waiters);
            diag.load_errors = diag.load_errors.saturating_add(shard.load_errors);
        }
        diag
    }
}

pub(crate) fn private_file_page_cache_diag() -> PrivateFilePageCacheDiag {
    PRIVATE_FILE_PAGES.diag()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SharedAnonPageKey {
    id: usize,
    offset: u64,
}

struct SharedAnonPageEntry {
    owner: Weak<SharedAnonObject>,
    page: Arc<ResidentPage>,
}

fn shared_anon_object_id(object: &Arc<SharedAnonObject>) -> usize {
    Arc::as_ptr(object) as usize
}

/// futex 等用户态同步原语使用的稳定地址 key。
///
/// 私有 futex 绑定到当前地址空间；共享 futex 绑定到底层 shared backing，
/// 这样同一文件页或同一 shared-anon 页在不同进程中的不同 VA 也能互相唤醒。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VmFutexKey {
    Private {
        vm: usize,
        page: usize,
        offset: u16,
    },
    SharedFile {
        file_key: usize,
        offset: u64,
        word_offset: u16,
    },
    SharedAnon {
        id: usize,
        offset: u64,
        word_offset: u16,
    },
    Direct {
        paddr: usize,
        word_offset: u16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageAccess {
    ReadOnly,
    Writable,
    Cow,
    SharedTracked,
}

impl PageAccess {
    fn pte_writable(self) -> bool {
        matches!(self, Self::Writable)
    }
}

#[derive(Clone)]
struct PageMapping {
    page: Arc<ResidentPage>,
    access: PageAccess,
}

#[derive(Clone, Copy)]
struct ForkChildMap {
    vaddr: usize,
    paddr: usize,
    flags: VmFlags,
}

fn map_fork_child_batches<F>(
    maps: &[ForkChildMap],
    page_size: usize,
    mut map_batch: F,
) -> Result<(), crate::MapBatchResult>
where
    F: FnMut(usize, &[usize], VmFlags) -> crate::MapBatchResult,
{
    let Some(first) = maps.first().copied() else {
        return Ok(());
    };
    let mut batch_vaddr = first.vaddr;
    let mut batch_flags = first.flags;
    let mut paddrs = Vec::new();

    for mapping in maps {
        let next_vaddr = paddrs
            .len()
            .checked_mul(page_size)
            .and_then(|len| batch_vaddr.checked_add(len));
        if !paddrs.is_empty() && (next_vaddr != Some(mapping.vaddr) || mapping.flags != batch_flags)
        {
            let result = map_batch(batch_vaddr, &paddrs, batch_flags);
            if result.error.is_some() || result.mapped != paddrs.len() {
                return Err(result);
            }
            paddrs.clear();
            batch_vaddr = mapping.vaddr;
            batch_flags = mapping.flags;
        }
        paddrs.push(mapping.paddr);
    }

    let result = map_batch(batch_vaddr, &paddrs, batch_flags);
    if result.error.is_some() || result.mapped != paddrs.len() {
        Err(result)
    } else {
        Ok(())
    }
}

struct PrivateFileFaultAround {
    fault_page: usize,
    end: usize,
    area_range: Range<usize>,
    area_file_offset: u64,
    fault_file_offset: u64,
    cache_snapshot: Option<PrivateFileCacheSnapshot>,
    flags: VmFlags,
    file: Arc<dyn FileLike>,
}

struct PreparedFilePage {
    vaddr: usize,
    page: Arc<ResidentPage>,
}

type PreparedFilePages = SmallVec<[PreparedFilePage; FILE_FAULT_AROUND_PAGES]>;
type PrivateFilePageBatch = SmallVec<[Arc<ResidentPage>; PRIVATE_FILE_BATCH_MAX_PAGES]>;
type PrivateFilePageLoadOwner = (FilePageKey, u64);
type PrivateFilePageLoadOwners = SmallVec<[PrivateFilePageLoadOwner; PRIVATE_FILE_BATCH_MAX_PAGES]>;
#[cfg(test)]
type PrivateFilePageClaims<'a, const SHARD_COUNT: usize> =
    SmallVec<[PrivateFilePageCacheClaim<'a, SHARD_COUNT>; PRIVATE_FILE_BATCH_MAX_PAGES]>;
type PrivateFilePageTargets<'a> = SmallVec<[&'a mut [u8]; PRIVATE_FILE_BATCH_MAX_PAGES]>;

struct PrivateFilePageClaimPrefix<'a, const SHARD_COUNT: usize> {
    owners: PrivateFilePageLoadOwners,
    terminal: Option<(usize, PrivateFilePageCacheClaim<'a, SHARD_COUNT>)>,
}

struct AnonStoreFaultAround {
    fault_page: usize,
    end: usize,
    area_range: Range<usize>,
    flags: VmFlags,
    merge_domain: AnonMergeDomain,
}

struct PreparedAnonPage {
    vaddr: usize,
    page: Arc<ResidentPage>,
}

type PreparedAnonPages = SmallVec<[PreparedAnonPage; ANON_STORE_FAULT_AROUND_PAGES]>;

enum PreparedPrivateFileCacheRun {
    Cached(Arc<ResidentPage>),
    Batched(PrivateFilePageBatch),
    Error(Errno),
    Fallback,
}

enum PrivateFilePageBatchLoad {
    Cached(Arc<ResidentPage>),
    Batched(PrivateFilePageBatch),
    Fallback,
}

enum FaultAroundCommit {
    Done(FaultOutcome),
    Retry,
}

enum ResidentPageKind {
    Anon,
    SharedAnon,
    PrivateFile,
    SharedFile {
        file: Arc<dyn FileLike>,
        offset: u64,
        generation: u64,
    },
    Direct,
}

struct ResidentPage {
    paddr: usize,
    kind: ResidentPageKind,
    dirty: AtomicBool,
}

impl ResidentPage {
    fn new_anon(paddr: usize) -> Arc<Self> {
        Arc::new(Self {
            paddr,
            kind: ResidentPageKind::Anon,
            dirty: AtomicBool::new(false),
        })
    }

    fn new_shared_anon(paddr: usize) -> Arc<Self> {
        Arc::new(Self {
            paddr,
            kind: ResidentPageKind::SharedAnon,
            dirty: AtomicBool::new(false),
        })
    }

    fn new_private_file(paddr: usize) -> Arc<Self> {
        Arc::new(Self {
            paddr,
            kind: ResidentPageKind::PrivateFile,
            dirty: AtomicBool::new(false),
        })
    }

    fn new_shared_file(
        paddr: usize,
        file: Arc<dyn FileLike>,
        offset: u64,
        generation: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            paddr,
            kind: ResidentPageKind::SharedFile {
                file,
                offset,
                generation,
            },
            dirty: AtomicBool::new(false),
        })
    }

    fn new_direct(paddr: usize) -> Arc<Self> {
        Arc::new(Self {
            paddr,
            kind: ResidentPageKind::Direct,
            dirty: AtomicBool::new(false),
        })
    }

    fn paddr(&self) -> usize {
        self.paddr
    }

    fn is_direct(&self) -> bool {
        matches!(self.kind, ResidentPageKind::Direct)
    }

    fn is_shared_anon(&self) -> bool {
        matches!(self.kind, ResidentPageKind::SharedAnon)
    }

    fn is_private_file(&self) -> bool {
        matches!(self.kind, ResidentPageKind::PrivateFile)
    }

    fn is_shared_file(&self) -> bool {
        matches!(self.kind, ResidentPageKind::SharedFile { .. })
    }

    fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    fn is_sysv_shm(&self) -> bool {
        matches!(&self.kind, ResidentPageKind::SharedFile { file, .. } if file.is_sysv_shm())
    }

    fn is_direct_shared_writable(&self) -> bool {
        self.is_direct() || self.is_shared_anon() || self.is_sysv_shm()
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    fn flush_to_backing(&self) -> Result<(), Errno> {
        if !self.dirty.load(Ordering::Acquire) {
            return Ok(());
        }
        let ResidentPageKind::SharedFile { file, offset, .. } = &self.kind else {
            return Ok(());
        };
        let file_size = file.size();
        if *offset >= file_size {
            return Ok(());
        }
        let page_size = page_size();
        let len = (file_size - *offset).min(page_size as u64) as usize;
        let virt = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;
        let buf = unsafe { core::slice::from_raw_parts(virt(self.paddr) as *const u8, len) };
        let mut written = 0usize;
        while written < len {
            let n = file.write_at(*offset + written as u64, &buf[written..])?;
            if n == 0 {
                return Err(Errno::EIO);
            }
            written += n;
        }
        file.sync()?;
        // cachestat 的 nr_writeback：累计成功写回底层存储的页数(本内核写回为
        // 同步执行,无"in-flight"窗口,故以累计值近似)。
        memstat::record_file_writeback();
        Ok(())
    }
}

impl Drop for ResidentPage {
    fn drop(&mut self) {
        match &self.kind {
            ResidentPageKind::SharedFile {
                file,
                offset,
                generation,
            } => {
                remove_cached_file_page(
                    &SHARED_FILE_PAGES,
                    FilePageKey::new(file, *offset, *generation),
                    self,
                );
            }
            _ => {}
        }
        if let Err(err) = self.flush_to_backing() {
            log::error!(
                "[mm] failed to flush shared mmap page paddr={:#x}: {:?}",
                self.paddr,
                err
            );
        }
        if !matches!(self.kind, ResidentPageKind::Direct) {
            free_user_page(self.paddr);
        }
    }
}

/// 一组已经完成权限检查和缺页处理的只读用户页窗口。
///
/// 每个窗口持有 resident page 的强引用，因此调用方可以先在普通上下文中完成
/// fault-in，再在不能触发缺页的子系统临界区内复制数据。窗口不会保存用户虚拟
/// 地址，也不会在复制时重新获取地址空间锁。
pub struct UserReadWindows<const N: usize> {
    windows: [Option<UserReadWindow>; N],
    count: usize,
    len: usize,
}

/// 一组已经完成权限检查、COW 和缺页处理的可写用户页窗口。
///
/// 窗口持有 resident page 的强引用，允许调用方先在可缺页上下文中固定目标，
/// 再在子系统短临界区内直接写入。成功写入的页面会立即标脏。
pub struct UserWriteWindows<const N: usize> {
    windows: [Option<UserWriteWindow>; N],
    count: usize,
    len: usize,
}

struct UserWriteWindow {
    page: Arc<ResidentPage>,
    address: usize,
    len: usize,
}

struct UserReadWindow {
    _page: Arc<ResidentPage>,
    address: usize,
    len: usize,
}

struct ResidentUserWindow {
    page: Arc<ResidentPage>,
    address: usize,
    len: usize,
}

struct ResidentUserWindows<const N: usize> {
    windows: [Option<ResidentUserWindow>; N],
    count: usize,
    len: usize,
}

impl<const N: usize> ResidentUserWindows<N> {
    fn empty() -> Self {
        Self {
            windows: core::array::from_fn(|_| None),
            count: 0,
            len: 0,
        }
    }

    fn clear(&mut self) {
        for window in &mut self.windows[..self.count] {
            *window = None;
        }
        self.count = 0;
        self.len = 0;
    }
}

impl<const N: usize> UserReadWindows<N> {
    pub fn empty() -> Self {
        Self {
            windows: core::array::from_fn(|_| None),
            count: 0,
            len: 0,
        }
    }

    fn clear(&mut self) {
        for window in &mut self.windows[..self.count] {
            *window = None;
        }
        self.count = 0;
        self.len = 0;
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn window_count(&self) -> usize {
        self.count
    }

    /// 从已经固定的窗口复制数据；该操作不会触发缺页或访问用户页表。
    pub fn copy_into(&self, offset: usize, output: &mut [u8]) -> Result<(), Errno> {
        let end = offset.checked_add(output.len()).ok_or(Errno::EFAULT)?;
        if end > self.len {
            return Err(Errno::EFAULT);
        }
        let mut logical = 0usize;
        let mut copied = 0usize;
        for window in self.windows[..self.count].iter().flatten() {
            let window_end = logical + window.len;
            if offset >= window_end {
                logical = window_end;
                continue;
            }
            let start = offset.saturating_sub(logical);
            let take = (window.len - start).min(output.len() - copied);
            // Safety: address 来自仍由 `_page` 保活的 direct-map 页，范围在固定窗口内。
            let input =
                unsafe { core::slice::from_raw_parts((window.address + start) as *const u8, take) };
            output[copied..copied + take].copy_from_slice(input);
            copied += take;
            logical = window_end;
            if copied == output.len() {
                return Ok(());
            }
        }
        Err(Errno::EFAULT)
    }
}

impl<const N: usize> UserWriteWindows<N> {
    pub fn empty() -> Self {
        Self {
            windows: core::array::from_fn(|_| None),
            count: 0,
            len: 0,
        }
    }

    fn clear(&mut self) {
        for window in &mut self.windows[..self.count] {
            *window = None;
        }
        self.count = 0;
        self.len = 0;
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn window_count(&self) -> usize {
        self.count
    }

    /// 向已经固定的窗口写入数据；该操作不会触发缺页或访问用户页表。
    pub fn copy_from(&self, offset: usize, input: &[u8]) -> Result<(), Errno> {
        let end = offset.checked_add(input.len()).ok_or(Errno::EFAULT)?;
        if end > self.len {
            return Err(Errno::EFAULT);
        }
        let mut logical = 0usize;
        let mut copied = 0usize;
        for window in self.windows[..self.count].iter().flatten() {
            let window_end = logical + window.len;
            if offset >= window_end {
                logical = window_end;
                continue;
            }
            let start = offset.saturating_sub(logical);
            let take = (window.len - start).min(input.len() - copied);
            // Safety: address 来自仍由 `page` 保活的 direct-map 页，范围在固定窗口内。
            let output = unsafe {
                core::slice::from_raw_parts_mut((window.address + start) as *mut u8, take)
            };
            output.copy_from_slice(&input[copied..copied + take]);
            window.page.mark_dirty();
            copied += take;
            logical = window_end;
            if copied == input.len() {
                return Ok(());
            }
        }
        Err(Errno::EFAULT)
    }
}

impl PrivateFileFaultAround {
    /// 从锁外的 VMA 快照生成预装计划；`FileLike::size` 不在 VMA 锁内调用。
    fn new(
        fault_page: usize,
        area_range: Range<usize>,
        flags: VmFlags,
        backing: &VmBacking,
        kind: FaultKind,
    ) -> Option<Self> {
        if !permits_file_fault_around(flags, kind) {
            return None;
        }
        let VmBacking::File { file, offset } = backing else {
            return None;
        };
        let (file_size, cache_snapshot) = private_file_cache_snapshot(file.as_ref());
        let window = file_fault_around_window(
            fault_page,
            area_range.start,
            area_range.end,
            *offset,
            file_size,
            page_size(),
        )?;
        Some(Self {
            fault_page: window.start,
            end: window.end,
            area_range,
            area_file_offset: *offset,
            fault_file_offset: window.file_offset,
            cache_snapshot,
            flags,
            file: Arc::clone(file),
        })
    }

    /// 在不持有 VMA/pages 锁时分配并读取连续候选页。
    ///
    /// 故障页失败沿用普通 fault 的错误；邻页属于投机行为，首次失败即缩短窗口。
    fn prepare_into(
        &self,
        prepared: &mut PreparedFilePages,
        profile_phases: bool,
    ) -> Result<(), Errno> {
        #[cfg(feature = "performance-profile")]
        let _profile = profile_phases.then(|| profiling::scope(profiling::Event::PageFaultPrepare));
        #[cfg(not(feature = "performance-profile"))]
        let _ = profile_phases;
        let page_size = page_size();
        let pages = (self.end - self.fault_page) / page_size;
        if pages > FILE_FAULT_AROUND_PAGES {
            return Err(Errno::EINVAL);
        }
        prepared.clear();
        let mut index = 0usize;
        while index < pages {
            let delta = index.checked_mul(page_size).ok_or(Errno::EINVAL)?;
            let vaddr = self.fault_page.checked_add(delta).ok_or(Errno::EINVAL)?;
            let file_offset = self
                .fault_file_offset
                .checked_add(u64::try_from(delta).map_err(|_| Errno::EINVAL)?)
                .ok_or(Errno::EINVAL)?;

            match prepare_private_file_cache_run(
                &self.file,
                self.cache_snapshot,
                file_offset,
                pages - index,
                page_size,
                profile_phases,
            ) {
                PreparedPrivateFileCacheRun::Cached(page) => {
                    prepared.push(PreparedFilePage { vaddr, page });
                    index += 1;
                    continue;
                }
                PreparedPrivateFileCacheRun::Batched(batch) => {
                    let batch_len = batch.len();
                    for (batch_index, page) in batch.into_iter().enumerate() {
                        let batch_delta =
                            batch_index.checked_mul(page_size).ok_or(Errno::EINVAL)?;
                        let batch_vaddr = vaddr.checked_add(batch_delta).ok_or(Errno::EINVAL)?;
                        prepared.push(PreparedFilePage {
                            vaddr: batch_vaddr,
                            page,
                        });
                    }
                    index = index.checked_add(batch_len).ok_or(Errno::EINVAL)?;
                    continue;
                }
                PreparedPrivateFileCacheRun::Error(err) if index == 0 => return Err(err),
                PreparedPrivateFileCacheRun::Error(_) => break,
                PreparedPrivateFileCacheRun::Fallback => {}
            }
            match private_file_page(&self.file, file_offset, profile_phases) {
                Ok(page) => prepared.push(PreparedFilePage { vaddr, page }),
                Err(err) if index == 0 => return Err(err),
                Err(_) => break,
            }
            index += 1;
        }
        #[cfg(feature = "performance-profile")]
        record_fault_around_prepare(pages, prepared.len());
        debug_assert!(!prepared.spilled());
        Ok(())
    }

    fn matches_area(&self, area: &VmArea) -> bool {
        if area.range != self.area_range || area.flags != self.flags {
            return false;
        }
        matches!(
            &area.backing,
            VmBacking::File { file, offset }
                if *offset == self.area_file_offset && Arc::ptr_eq(file, &self.file)
        )
    }
}

impl AnonStoreFaultAround {
    fn new(
        fault_page: usize,
        area_range: Range<usize>,
        flags: VmFlags,
        backing: &VmBacking,
        kind: FaultKind,
    ) -> Option<Self> {
        let VmBacking::Anon { merge_domain } = backing else {
            return None;
        };
        if !matches!(kind, FaultKind::Store)
            || !flags.contains_all(VmFlags::USER | VmFlags::WRITE | VmFlags::ANON)
            || flags.has(VmFlags::SHARED)
            || flags.has(VmFlags::GROWS_DOWN)
        {
            return None;
        }
        let end = anon_store_fault_around_end(fault_page, &area_range, page_size())?;
        Some(Self {
            fault_page,
            end,
            area_range,
            flags,
            merge_domain: *merge_domain,
        })
    }

    /// 在 VMA/pages 锁外分配并清零候选页。
    ///
    /// 真实故障页分配失败保留 ENOMEM；投机邻页首次失败只缩短窗口。
    fn prepare_into(&self, prepared: &mut PreparedAnonPages) -> Result<(), Errno> {
        let page_size = page_size();
        let requested = (self.end - self.fault_page) / page_size;
        prepared.clear();
        // 元数据分配失败不应把一次可退化的优化升级为内核 fault；空前缀会让
        // 提交路径转回既有单页处理。
        if prepared.try_reserve_exact(requested).is_err() {
            #[cfg(feature = "performance-profile")]
            record_anon_fault_around_prepare(requested, 0, true);
            return Ok(());
        }
        let mut batch = [None; ANON_STORE_FAULT_AROUND_PAGES];
        let _batch_count = alloc_uninitialized_user_page_batch(&mut batch[..requested]);
        let virt = allocator::KERNEL_ALLOCATOR.load_phys_to_virt();
        for index in 0..requested {
            let delta = index.checked_mul(page_size).ok_or(Errno::EINVAL)?;
            let vaddr = self.fault_page.checked_add(delta).ok_or(Errno::EINVAL)?;
            let paddr = if let Some(allocation) = batch[index].take() {
                let Some(virt) = virt else {
                    let _ = allocator::KERNEL_ALLOCATOR.try_free_untracked_physical(allocation);
                    for allocation in batch.iter_mut().filter_map(Option::take) {
                        let _ = allocator::KERNEL_ALLOCATOR.try_free_untracked_physical(allocation);
                    }
                    break;
                };
                #[cfg(feature = "performance-profile")]
                let _profile = profiling::scope(profiling::Event::MemZeroAnonPage).bytes(page_size);
                // Safety: 批量分配返回独占且尚未发布的完整基础页。
                unsafe { zero_unpublished_user_pages(virt(allocation.paddr), page_size) };
                Some(allocation.paddr)
            } else if index == 0 {
                alloc_zeroed_user_page()
            } else {
                let order = user_page_order().ok_or(Errno::EINVAL)?;
                try_alloc_zeroed_user_page(order, page_size)
            };
            let Some(paddr) = paddr else {
                if index == 0 {
                    #[cfg(feature = "performance-profile")]
                    record_anon_fault_around_prepare(requested, 0, false);
                    return Err(Errno::ENOMEM);
                }
                break;
            };
            prepared.push(PreparedAnonPage {
                vaddr,
                page: ResidentPage::new_anon(paddr),
            });
        }
        #[cfg(feature = "performance-profile")]
        record_anon_fault_around_prepare(requested, prepared.len(), false);
        Ok(())
    }

    fn matches_area(&self, area: &VmArea) -> bool {
        area.range == self.area_range
            && area.flags == self.flags
            && matches!(
                &area.backing,
                VmBacking::Anon { merge_domain }
                    if merge_domain.same_snapshot_identity(self.merge_domain)
            )
    }
}

/// 进程地址空间。
pub struct VmSpace {
    vmas: Spinlock<VmaSet>,
    pages: Spinlock<RadixPageMap<PageMapping>>,
    pgd: PgdHandle,
    brk_start: AtomicUsize,
    brk_current: AtomicUsize,
    mmap_next: AtomicUsize,
    mlock_future: AtomicBool,
    /// 本地址空间承诺页数（overcommit 记账，`MAP_NORESERVE` 区域不计）。
    ///
    /// 与 Linux `mm->total_vm` 的记账口径一致：VMA 几何大小，而非驻留页数。
    /// `fork` 时子进程复制父进程的承诺并在全局聚合中加一份。
    committed_pages: AtomicUsize,
    /// 已锁页数（`RLIMIT_MEMLOCK` 记账与 `/proc/self/status` 的 `VmLck`）。
    ///
    /// 口径与 Linux `mm->locked_vm` 一致：带 `LOCKED` 标记的 VMA 几何页数。
    locked_pages: AtomicUsize,
    /// NUMA 内存策略（单节点语义；`set_mempolicy`/`mbind` 状态）。
    mempolicy: Spinlock<MempolicyState>,
    /// userfaultfd 登记区域。fork 不继承（Linux 语义：子进程不带 uffd 状态）。
    uffd_regions: Spinlock<Vec<UffdRegion>>,
    /// 已换出匿名页的槽位表（`虚拟页 -> SwapSlot`）。见 [`crate::mm::swap`] 顶部
    /// 关于"槽位表替代 PTE swap 编码"的说明。
    swapped: Spinlock<RadixPageMap<SwapSlot>>,
    /// `MADV_FREE` 标记的可释放页；无 LRU 回收器时仅在显式回收点被丢弃。
    freeable: Spinlock<RadixPageMap<()>>,
    /// `MADV_COLD` 标记的冷页；无 LRU 回收器时仅作为 `MADV_PAGEOUT` 的优先级
    /// 提示并记录可观测状态。
    cold: Spinlock<RadixPageMap<()>>,
    /// `membarrier(2)` expedited 命令的地址空间级注册位。
    membarrier_registration: AtomicUsize,
    /// 诊断辅助：记录当前已建立页表映射的用户页数。
    mapped_pages: AtomicUsize,
    /// 性能影子模型使用的稳定地址空间身份；不参与任何映射决策。
    #[cfg(feature = "performance-profile")]
    profile_identity: u64,
}

// Safety: PgdHandle 是 arch opaque 句柄；VMA 与 resident page map 均由锁保护。
unsafe impl Send for VmSpace {}
unsafe impl Sync for VmSpace {}

#[kernel_symbols::export]
impl VmSpace {
    /// 新建一个空地址空间。必须在 `register_user_pgd` 完成之后调用。
    #[kernel_symbols::export(name = "general.mm.VmSpace.new", contract = "kernel.mm.address-space@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn new() -> Self {
        let ops = user_pgd_ops().expect("[mm] user_pgd_ops not registered");
        let layout = vm_layout();
        let pgd = (ops.new_pgd_for_user)();
        VM_SPACE_CREATED.fetch_add(1, Ordering::Relaxed);
        VM_SPACE_LIVE.fetch_add(1, Ordering::Relaxed);
        Self {
            vmas: Spinlock::new(VmaSet::new()),
            pages: Spinlock::new(RadixPageMap::new(layout.page_size)),
            pgd,
            brk_start: AtomicUsize::new(layout.user_heap_base),
            brk_current: AtomicUsize::new(layout.user_heap_base),
            mmap_next: AtomicUsize::new(layout.user_mmap_base),
            mlock_future: AtomicBool::new(false),
            committed_pages: AtomicUsize::new(0),
            locked_pages: AtomicUsize::new(0),
            mempolicy: Spinlock::new(MempolicyState::default()),
            uffd_regions: Spinlock::new(Vec::new()),
            swapped: Spinlock::new(RadixPageMap::new(layout.page_size)),
            freeable: Spinlock::new(RadixPageMap::new(layout.page_size)),
            cold: Spinlock::new(RadixPageMap::new(layout.page_size)),
            membarrier_registration: AtomicUsize::new(0),
            mapped_pages: AtomicUsize::new(0),
            #[cfg(feature = "performance-profile")]
            profile_identity: VM_SPACE_PROFILE_ID_NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }

    #[kernel_symbols::export(name = "general.mm.VmSpace.pgd", contract = "kernel.mm.address-space@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn pgd(&self) -> PgdHandle {
        self.pgd
    }

    #[kernel_symbols::export(name = "general.mm.VmSpace.mapped_pages", contract = "kernel.mm.address-space@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC)]
    pub fn mapped_pages(&self) -> usize {
        self.mapped_pages.load(Ordering::Acquire)
    }

    /// 为当前地址空间登记可用的 expedited membarrier 命令。
    pub fn register_membarrier(&self, commands: usize) {
        self.membarrier_registration
            .fetch_or(commands, Ordering::AcqRel);
    }

    pub fn membarrier_registration(&self) -> usize {
        self.membarrier_registration.load(Ordering::Acquire)
    }

    fn with_future_mlock(&self, flags: VmFlags) -> VmFlags {
        if self.mlock_future.load(Ordering::Acquire) {
            flags.with(VmFlags::LOCKED)
        } else {
            flags
        }
    }

    /// 区域页数（VMA 几何口径，与 Linux `vma_pages` 一致）。
    fn area_page_count(area: &VmArea) -> usize {
        (area.range.end - area.range.start) / page_size()
    }

    /// 区域插入前的映射策略检查：`max_map_count` 与 overcommit 记账。
    ///
    /// `count_after` 为插入后集合的预期 VMA 数量；`pages` 为新增区域页数；
    /// `noreserve` 对应 `MAP_NORESERVE`（严格模式下仍受启发式上限约束）。
    fn check_map_policy(
        &self,
        count_after: usize,
        pages: usize,
        noreserve: bool,
    ) -> Result<(), Errno> {
        if !memstat::map_count_allowed(count_after) {
            return Err(Errno::ENOMEM);
        }
        let overview = allocator::KERNEL_ALLOCATOR.detailed_stats();
        let total_pages = (overview.total_physical / allocator::PAGE_SIZE) as u64;
        let swap_pages = crate::mm::swap::swap_totals().0;
        memstat::check_overcommit(pages as u64, noreserve, total_pages, swap_pages)
            .map_err(|()| Errno::ENOMEM)
    }

    /// 集合中是否有 `SEALED`(mseal)区域与 `range` 相交。
    ///
    /// mseal 语义：密封区域禁止后续 mprotect/munmap/mremap/MAP_FIXED 覆盖，
    /// 任何相交即拒绝（EPERM），与 Linux `can_modify_mm` 一致。
    fn contains_sealed(set: &VmaSet, range: &Range<usize>) -> bool {
        set.iter_overlap(range)
            .any(|area| area.flags.has(VmFlags::SEALED))
    }

    /// 记账一次成功的区域插入：承诺页与锁页都按 VMA 几何口径累加。
    ///
    /// 调用方必须保证区域已经进入 VMA 集合。合并不破坏口径：可合并的邻居
    /// flags 必须完全相同，因此分片记账与合并后几何一致。
    fn account_area_insert(&self, area: &VmArea) {
        let pages = Self::area_page_count(area);
        if !area.flags.has(VmFlags::NORESERVE) {
            self.committed_pages.fetch_add(pages, Ordering::Relaxed);
            memstat::commit_pages(pages as i64);
        }
        if area.flags.has(VmFlags::LOCKED) {
            self.locked_pages.fetch_add(pages, Ordering::Relaxed);
        }
    }

    /// 记账一次区域移除（unmap 摘除的片段）。
    fn account_area_remove(&self, area: &VmArea) {
        let pages = Self::area_page_count(area);
        if !area.flags.has(VmFlags::NORESERVE) {
            self.committed_pages.fetch_sub(pages, Ordering::Relaxed);
            memstat::commit_pages(-(pages as i64));
        }
        if area.flags.has(VmFlags::LOCKED) {
            self.locked_pages.fetch_sub(pages, Ordering::Relaxed);
        }
    }

    /// 当前已锁页数（`RLIMIT_MEMLOCK` 检查与 `/proc/self/status` VmLck 用）。
    pub fn locked_pages(&self) -> usize {
        self.locked_pages.load(Ordering::Acquire)
    }

    /// 若锁定 `range` 会新增的锁页数（仅统计当前未带 `LOCKED` 标记的区域）。
    pub fn would_lock_pages(&self, range: &Range<usize>) -> usize {
        let set = self.vmas.lock();
        let mut pages = 0usize;
        for area in set.iter_overlap(range) {
            if area.flags.has(VmFlags::LOCKED) {
                continue;
            }
            let start = area.range.start.max(range.start);
            let end = area.range.end.min(range.end);
            pages = pages.saturating_add((end - start) / page_size());
        }
        pages
    }

    /// 若 `mlockall(MCL_CURRENT)` 会新增的锁页数。
    pub fn would_lock_all_pages(&self) -> usize {
        let set = self.vmas.lock();
        let mut pages = 0usize;
        for area in set.iter() {
            if !area.flags.has(VmFlags::LOCKED) {
                pages = pages.saturating_add(Self::area_page_count(area));
            }
        }
        pages
    }

    /// 当前承诺页数（overcommit 记账）。
    pub fn committed_pages(&self) -> usize {
        self.committed_pages.load(Ordering::Acquire)
    }

    /// 全部 VMA 的几何页数（含 `MAP_NORESERVE`），供 `RLIMIT_AS` 检查。
    ///
    /// 口径与 Linux `mm->total_vm` 一致：不区分是否预留承诺，只统计地址空间
    /// 当前占用的虚拟页几何大小。
    pub fn total_vm_pages(&self) -> usize {
        self.vmas.lock().iter().map(Self::area_page_count).sum()
    }

    /// 私有可写非栈 VMA 的几何页数，供 `RLIMIT_DATA` 检查。
    ///
    /// 判定条件对齐 Linux `is_data_mapping`：`VM_WRITE` 置位且 `VM_SHARED`/
    /// `VM_STACK` 均未置位。本内核没有独立的 `VM_STACK` 位，`MAP_GROWSDOWN`/
    /// 主栈用 `GROWS_DOWN` 近似 `VM_STACK`。
    pub fn data_vm_pages(&self) -> usize {
        let set = self.vmas.lock();
        set.iter()
            .filter(|area| {
                area.flags.has(VmFlags::WRITE)
                    && !area.flags.has(VmFlags::SHARED)
                    && !area.flags.has(VmFlags::GROWS_DOWN)
            })
            .map(Self::area_page_count)
            .sum()
    }

    /// `range` 是否与任何 `SEALED`（mseal）区域相交。
    ///
    /// `mprotect`/`munmap`/`mremap`/`MAP_FIXED` 之外的修改性 `madvise` 也按
    /// Linux `can_modify_mm` 语义拦截。
    pub fn has_sealed_in(&self, range: &Range<usize>) -> bool {
        Self::contains_sealed(&self.vmas.lock(), range)
    }

    /// `addr` 所在 VMA 是否计入 `RLIMIT_DATA`（Linux `is_data_mapping`）。
    pub fn vma_is_data(&self, addr: usize) -> bool {
        self.vmas.lock().find(addr).is_some_and(|area| {
            area.flags.has(VmFlags::WRITE)
                && !area.flags.has(VmFlags::SHARED)
                && !area.flags.has(VmFlags::GROWS_DOWN)
        })
    }

    /// `addr` 所在页当前是否驻留（resident ledger 中是否存在条目）。
    ///
    /// 不触发缺页、不修改页表；`move_pages` 用它与 `mincore` 同口径判断页是否
    /// 存在（Linux `move_pages` 的 status 以页是否驻留为准，而非 VMA 是否可读）。
    pub fn is_page_resident(&self, addr: usize) -> bool {
        self.pages.lock().contains_key(page_base(addr))
    }

    #[kernel_symbols::export(name = "general.mm.VmSpace.current_brk", contract = "kernel.mm.address-space@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn current_brk(&self) -> usize {
        self.brk_current.load(Ordering::Acquire)
    }

    /// ELF loader 装载完成后调用：将 brk 起点对齐到主程序数据段末尾。
    pub fn init_brk_after_load(&self, max_segment_end: usize) {
        let page_size = page_size();
        let new_brk = align_up(max_segment_end, page_size).unwrap_or(max_segment_end);
        let brk = new_brk.max(self.brk_start.load(Ordering::Relaxed));
        self.brk_start.store(brk, Ordering::Release);
        self.brk_current.store(brk, Ordering::Release);
    }

    /// 对 PIE 主程序使用的 brk 初始化。
    ///
    /// `user_heap_base` 是架构选择的独立 brk 区域。低地址 PIE 可以自然落在 heap
    /// base 之前；高地址 PIE 则必须把 brk 起点整体放到主程序段之后，不能只更新
    /// current，否则后续 brk shrink 会跨过一大段非 heap 区间。
    pub fn init_brk_after_pie_load(&self, max_segment_end: usize) {
        let page_size = page_size();
        let new_brk = align_up(max_segment_end, page_size).unwrap_or(max_segment_end);
        let brk = new_brk.max(self.brk_start.load(Ordering::Relaxed));
        self.brk_start.store(brk, Ordering::Release);
        self.brk_current.store(brk, Ordering::Release);
    }

    #[kernel_symbols::export(name = "general.mm.VmSpace.set_brk", contract = "kernel.mm.address-space@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn set_brk(&self, requested: usize) -> usize {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::MmBrk).trace_args(requested as u64, 0);
        if requested == 0 {
            return self.current_brk();
        }
        let brk_start = self.brk_start.load(Ordering::Relaxed);
        if requested < brk_start {
            return self.current_brk();
        }

        let old = self.current_brk();
        let page_size = page_size();
        let old_end = align_up(old, page_size).unwrap_or(old);
        let new_end = match align_up(requested, page_size) {
            Some(v) => v,
            None => return old,
        };

        let result = if new_end > old_end {
            self.map_anon(
                old_end..new_end,
                VmFlags::EMPTY
                    .with(VmFlags::READ)
                    .with(VmFlags::WRITE)
                    .with(VmFlags::USER),
            )
        } else if new_end < old_end {
            self.unmap(new_end..old_end)
        } else {
            Ok(())
        };

        if result.is_ok() {
            self.brk_current.store(requested, Ordering::Release);
            requested
        } else {
            old
        }
    }

    #[kernel_symbols::export(name = "general.mm.VmSpace.alloc_mmap_range", contract = "kernel.mm.address-space@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn alloc_mmap_range(&self, len: usize) -> Result<Range<usize>, Errno> {
        let layout = vm_layout();
        let page_size = layout.page_size;
        let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
        if len == 0 {
            return Err(Errno::EINVAL);
        }

        let cursor = align_up(self.mmap_next.load(Ordering::Acquire), page_size)
            .unwrap_or(layout.user_mmap_base)
            .clamp(layout.user_mmap_base, layout.user_mmap_limit);
        let set = self.vmas.lock();
        let candidates = [
            (layout.user_mmap_base, cursor),
            (cursor, layout.user_mmap_limit),
        ];
        for (start, end) in candidates {
            if start >= end {
                continue;
            }
            if let Some(range) = set.find_gap(start..end, len) {
                self.mmap_next.store(range.end, Ordering::Release);
                return Ok(range);
            }
        }
        Err(Errno::ENOMEM)
    }

    /// 查询一个非 `MAP_FIXED` mmap 地址提示是否可以原样使用。
    ///
    /// Linux 把非零 `mmap` 地址当作 hint：区间空闲时优先使用该地址，冲突时
    /// 才回退到普通的自动分配。调用方随后仍需在同一地址空间锁下登记 VMA，
    /// 因此这里是一个可失败的乐观查询；若并发映射抢先占用，登记操作会返回
    /// `EEXIST`，由 syscall 层回退到自动分配路径。
    pub fn mmap_hint_range(&self, addr: usize, len: usize) -> Option<Range<usize>> {
        let page_size = page_size();
        if addr % page_size != 0 {
            return None;
        }
        let len = align_up(len, page_size)?;
        if len == 0 {
            return None;
        }
        let end = addr.checked_add(len)?;
        let range = addr..end;
        self.validate_range(&range).ok()?;
        self.vmas.lock().is_range_free(&range).then_some(range)
    }

    /// 在地址空间内原子选择并登记一段满足对齐要求的匿名映射。
    pub fn map_anon_any_aligned(
        &self,
        len: usize,
        alignment: usize,
        flags: VmFlags,
    ) -> Result<Range<usize>, Errno> {
        let layout = vm_layout();
        let page_size = layout.page_size;
        let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
        if len == 0
            || alignment < page_size
            || !alignment.is_power_of_two()
            || alignment % page_size != 0
        {
            return Err(Errno::EINVAL);
        }

        let cursor = align_up(self.mmap_next.load(Ordering::Acquire), page_size)
            .unwrap_or(layout.user_mmap_base)
            .clamp(layout.user_mmap_base, layout.user_mmap_limit);
        let flags = self.with_future_mlock(flags).with(VmFlags::ANON);
        let backing = if flags.has(VmFlags::SHARED) {
            VmBacking::SharedAnon {
                object: Arc::new(SharedAnonObject::new()),
                offset: 0,
            }
        } else {
            VmBacking::anonymous()
        };
        let mut set = self.vmas.lock();
        let candidates = [
            (layout.user_mmap_base, cursor),
            (cursor, layout.user_mmap_limit),
        ];
        for (start, end) in candidates {
            if start >= end {
                continue;
            }
            let Some(range) = set.find_aligned_gap(start..end, len, alignment) else {
                continue;
            };
            let area = VmArea {
                range: range.clone(),
                flags,
                backing,
            };
            self.check_map_policy(
                set.len() + 1,
                Self::area_page_count(&area),
                flags.has(VmFlags::NORESERVE),
            )?;
            set.insert(area.clone())?;
            self.account_area_insert(&area);
            self.mmap_next.store(range.end, Ordering::Release);
            return Ok(range);
        }
        Err(Errno::ENOMEM)
    }

    /// 在地址空间内原子选择并登记一段共享匿名对象映射。
    ///
    /// `object_offset` 表示返回区间起点对应的对象内偏移；同一对象的不同映射
    /// 因而可以在不同地址空间共享物理页，而不依赖文件描述符或全局名称。
    pub fn map_shared_anon_any_aligned(
        &self,
        len: usize,
        alignment: usize,
        object: Arc<SharedAnonObject>,
        object_offset: u64,
        flags: VmFlags,
    ) -> Result<Range<usize>, Errno> {
        let layout = vm_layout();
        let page_size = layout.page_size;
        let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
        if len == 0
            || alignment < page_size
            || !alignment.is_power_of_two()
            || alignment % page_size != 0
            || object_offset % page_size as u64 != 0
            || object_offset.checked_add(len as u64).is_none()
        {
            return Err(Errno::EINVAL);
        }

        let cursor = align_up(self.mmap_next.load(Ordering::Acquire), page_size)
            .unwrap_or(layout.user_mmap_base)
            .clamp(layout.user_mmap_base, layout.user_mmap_limit);
        let flags = self
            .with_future_mlock(flags)
            .with(VmFlags::ANON)
            .with(VmFlags::SHARED);
        let mut set = self.vmas.lock();
        for (start, end) in [
            (layout.user_mmap_base, cursor),
            (cursor, layout.user_mmap_limit),
        ] {
            if start >= end {
                continue;
            }
            let Some(range) = set.find_aligned_gap(start..end, len, alignment) else {
                continue;
            };
            let area = VmArea {
                range: range.clone(),
                flags,
                backing: VmBacking::SharedAnon {
                    object: Arc::clone(&object),
                    offset: object_offset,
                },
            };
            self.check_map_policy(
                set.len() + 1,
                Self::area_page_count(&area),
                flags.has(VmFlags::NORESERVE),
            )?;
            set.insert(area.clone())?;
            self.account_area_insert(&area);
            self.mmap_next.store(range.end, Ordering::Release);
            return Ok(range);
        }
        Err(Errno::ENOMEM)
    }

    /// 在地址空间内原子选择并登记一段 file-backed 映射。
    pub fn map_file_any_aligned(
        &self,
        len: usize,
        alignment: usize,
        file: Arc<dyn FileLike>,
        offset: u64,
        flags: VmFlags,
    ) -> Result<Range<usize>, Errno> {
        let layout = vm_layout();
        let page_size = layout.page_size;
        let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
        if len == 0
            || alignment < page_size
            || !alignment.is_power_of_two()
            || alignment % page_size != 0
            || offset % page_size as u64 != 0
            || offset.checked_add(len as u64).is_none()
        {
            return Err(Errno::EINVAL);
        }

        let cursor = align_up(self.mmap_next.load(Ordering::Acquire), page_size)
            .unwrap_or(layout.user_mmap_base)
            .clamp(layout.user_mmap_base, layout.user_mmap_limit);
        let flags = self.with_future_mlock(flags);
        let mapped_file = Arc::clone(&file);
        let mut set = self.vmas.lock();
        for (start, end) in [
            (layout.user_mmap_base, cursor),
            (cursor, layout.user_mmap_limit),
        ] {
            if start >= end {
                continue;
            }
            let Some(range) = set.find_aligned_gap(start..end, len, alignment) else {
                continue;
            };
            let area = VmArea {
                range: range.clone(),
                flags,
                backing: VmBacking::File {
                    file: Arc::clone(&file),
                    offset,
                },
            };
            self.check_map_policy(
                set.len() + 1,
                Self::area_page_count(&area),
                flags.has(VmFlags::NORESERVE),
            )?;
            set.insert(area.clone())?;
            self.account_area_insert(&area);
            self.mmap_next.store(range.end, Ordering::Release);
            drop(set);
            mapped_file.on_mapped();
            return Ok(range);
        }
        Err(Errno::ENOMEM)
    }

    /// 在地址空间内原子选择地址，并立即映射一段连续物理内存。
    ///
    /// 物理内存的所有权仍由调用方持有；VMA 只保存 direct backing。调用方必须
    /// 保证所有用户映射撤销前底层分配不会释放。
    pub fn map_direct_any_aligned(
        &self,
        len: usize,
        alignment: usize,
        paddr: usize,
        flags: VmFlags,
    ) -> Result<Range<usize>, Errno> {
        let layout = vm_layout();
        let page_size = layout.page_size;
        let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
        if len == 0
            || alignment < page_size
            || !alignment.is_power_of_two()
            || alignment % page_size != 0
            || paddr % page_size != 0
        {
            return Err(Errno::EINVAL);
        }
        let cursor = align_up(self.mmap_next.load(Ordering::Acquire), page_size)
            .unwrap_or(layout.user_mmap_base)
            .clamp(layout.user_mmap_base, layout.user_mmap_limit);
        let area_flags = self.with_future_mlock(flags).with(VmFlags::USER);
        let mut set = self.vmas.lock();
        for (start, end) in [
            (layout.user_mmap_base, cursor),
            (cursor, layout.user_mmap_limit),
        ] {
            if start >= end {
                continue;
            }
            let Some(range) = set.find_aligned_gap(start..end, len, alignment) else {
                continue;
            };
            let area = VmArea {
                range: range.clone(),
                flags: area_flags,
                backing: VmBacking::Direct(paddr),
            };
            self.check_map_policy(
                set.len() + 1,
                Self::area_page_count(&area),
                area_flags.has(VmFlags::NORESERVE),
            )?;
            set.insert(area.clone())?;
            self.account_area_insert(&area);
            self.mmap_next.store(range.end, Ordering::Release);
            drop(set);
            if let Err(error) = self.populate_direct_mapping(range.clone(), paddr, area_flags) {
                // 回滚路径复用 unmap：其中的记账移除与上面的插入记账对冲。
                let _ = self.unmap_existing(range);
                return Err(error);
            }
            return Ok(range);
        }
        Err(Errno::ENOMEM)
    }

    /// 在调用者指定的空闲地址登记共享匿名对象映射；已有映射不会被替换。
    pub fn map_shared_anon(
        &self,
        range: Range<usize>,
        object: Arc<SharedAnonObject>,
        object_offset: u64,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let page_size = page_size();
        if object_offset % page_size as u64 != 0
            || object_offset.checked_add(range.len() as u64).is_none()
        {
            return Err(Errno::EINVAL);
        }
        let area = VmArea {
            range,
            flags: self
                .with_future_mlock(flags)
                .with(VmFlags::ANON)
                .with(VmFlags::SHARED),
            backing: VmBacking::SharedAnon {
                object,
                offset: object_offset,
            },
        };
        {
            let mut set = self.vmas.lock();
            self.check_map_policy(
                set.len() + 1,
                Self::area_page_count(&area),
                area.flags.has(VmFlags::NORESERVE),
            )?;
            set.insert(area.clone())?;
        }
        self.account_area_insert(&area);
        Ok(())
    }

    #[kernel_symbols::export(name = "general.mm.VmSpace.is_range_free", contract = "kernel.mm.address-space@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn is_range_free(&self, range: Range<usize>) -> bool {
        self.validate_range(&range).is_ok() && self.vmas.lock().is_range_free(&range)
    }

    /// 检查一段用户地址是否被可读用户 VMA 连续覆盖。
    ///
    /// 这个接口不触发缺页，也不承诺页表页已经常驻；它只用于 syscall 在访问用户
    /// 指针前做快速结构性校验，避免退出清理这类不可失败路径卡在明显损坏的链表上。
    #[kernel_symbols::export(name = "general.mm.VmSpace.is_user_range_readable", contract = "kernel.mm.address-space@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn is_user_range_readable(&self, addr: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let Some(end) = addr.checked_add(len) else {
            return false;
        };
        let range = addr..end;
        let set = self.vmas.lock();
        let mut cursor = range.start;
        for area in set.iter_overlap(&range) {
            if area.range.start > cursor {
                return false;
            }
            if !area.flags.contains_all(VmFlags::USER | VmFlags::READ) {
                return false;
            }
            cursor = cursor.max(area.range.end.min(range.end));
            if cursor >= range.end {
                return true;
            }
        }
        false
    }

    /// 按 `shmdt` 的入口地址查找一整段 SysV shm 映射。
    ///
    /// SysV shm 通过普通 file-backed VMA 接入 VM，因此这里不引入新的 backing
    /// 枚举；只要求底层 [`FileLike`] 暴露 shm id。`mprotect` 可能把同一段映射
    /// 分裂成多个相邻 VMA，所以检查时按文件 offset 把整段重新拼起来，避免把
    /// 其他文件或后来复用的地址误当成可 detach 的 shm。
    pub fn sysv_shm_mapping_at(&self, addr: usize) -> Option<(Range<usize>, i32)> {
        let set = self.vmas.lock();
        let first = set.find(addr)?;
        if first.range.start != addr {
            return None;
        }
        let VmBacking::File { file, offset } = &first.backing else {
            return None;
        };
        if *offset != 0 {
            return None;
        }
        let shmid = file.sysv_shm_id()?;
        let file_size = file.size();
        if file_size == 0 || file_size > usize::MAX as u64 {
            return None;
        }
        let len = align_up(file_size as usize, page_size())?;
        let end = addr.checked_add(len)?;
        let range = addr..end;
        if !set.contains_range(&range) {
            return None;
        }

        let mut cursor = range.start;
        for area in set.iter_overlap(&range) {
            if area.range.start > cursor {
                return None;
            }
            let VmBacking::File {
                file: area_file,
                offset: area_offset,
            } = &area.backing
            else {
                return None;
            };
            if area_file.sysv_shm_id() != Some(shmid) {
                return None;
            }
            let expected_offset = (area.range.start - range.start) as u64;
            if *area_offset != expected_offset {
                return None;
            }
            cursor = cursor.max(area.range.end.min(range.end));
            if cursor >= range.end {
                return Some((range.clone(), shmid));
            }
        }
        None
    }

    /// 根据用户地址生成 futex key。
    ///
    /// `private` 对应 `FUTEX_PRIVATE_FLAG`。未带 private flag 时，也只有真正
    /// `MAP_SHARED`/direct shared backing 才生成跨地址空间 key；普通 private
    /// VMA 仍按本地址空间隔离，避免不同进程相同 VA 错误互唤醒。
    pub fn futex_key_for(&self, uaddr: usize, private: bool) -> Result<VmFutexKey, Errno> {
        if uaddr % 4 != 0 {
            return Err(Errno::EINVAL);
        }
        let page = page_base(uaddr);
        let word_offset = u16::try_from(uaddr - page).map_err(|_| Errno::EINVAL)?;
        if private {
            return Ok(VmFutexKey::Private {
                vm: self as *const Self as usize,
                page,
                offset: word_offset,
            });
        }

        let set = self.vmas.lock();
        let area = set.find(uaddr).ok_or(Errno::EFAULT)?;
        if !area.flags.has(VmFlags::SHARED) && !matches!(area.backing, VmBacking::Direct(_)) {
            return Ok(VmFutexKey::Private {
                vm: self as *const Self as usize,
                page,
                offset: word_offset,
            });
        }
        let page_delta = page.checked_sub(area.range.start).ok_or(Errno::EFAULT)?;
        match &area.backing {
            VmBacking::File { file, offset } => Ok(VmFutexKey::SharedFile {
                file_key: file.cache_key(),
                offset: offset
                    .checked_add(u64::try_from(page_delta).map_err(|_| Errno::EINVAL)?)
                    .ok_or(Errno::EINVAL)?,
                word_offset,
            }),
            VmBacking::SharedAnon { object, offset } => Ok(VmFutexKey::SharedAnon {
                id: shared_anon_object_id(object),
                offset: offset
                    .checked_add(u64::try_from(page_delta).map_err(|_| Errno::EINVAL)?)
                    .ok_or(Errno::EINVAL)?,
                word_offset,
            }),
            VmBacking::Direct(base) => Ok(VmFutexKey::Direct {
                paddr: base.checked_add(page_delta).ok_or(Errno::EINVAL)?,
                word_offset,
            }),
            VmBacking::Anon { .. } => Ok(VmFutexKey::Private {
                vm: self as *const Self as usize,
                page,
                offset: word_offset,
            }),
        }
    }

    /// 注册一段匿名 VMA。不立即分配物理页。
    #[kernel_symbols::export(name = "general.mm.VmSpace.map_anon", contract = "kernel.mm.mapping@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn map_anon(&self, range: Range<usize>, flags: VmFlags) -> Result<(), Errno> {
        #[cfg(feature = "performance-profile")]
        let _profile =
            profiling::scope(profiling::Event::MmMap).bytes(range.end.saturating_sub(range.start));
        self.validate_range(&range)?;
        let flags = self.with_future_mlock(flags);
        let pages = (range.end - range.start) / page_size();
        let backing = if flags.has(VmFlags::SHARED) {
            VmBacking::SharedAnon {
                object: Arc::new(SharedAnonObject::new()),
                offset: 0,
            }
        } else {
            VmBacking::anonymous()
        };
        let area = VmArea {
            range,
            flags: flags.with(VmFlags::ANON),
            backing,
        };
        {
            let mut set = self.vmas.lock();
            self.check_map_policy(set.len() + 1, pages, flags.has(VmFlags::NORESERVE))?;
            set.insert(area.clone())?;
        }
        self.account_area_insert(&area);
        Ok(())
    }

    /// 注册一段 file-backed VMA。缺页时按 offset + (addr - range.start) 读文件。
    #[kernel_symbols::export(name = "general.mm.VmSpace.map_file", contract = "kernel.mm.mapping@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE, retained_args = 1 << 2)]
    pub fn map_file(
        &self,
        range: Range<usize>,
        file: Arc<dyn FileLike>,
        offset: u64,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        #[cfg(feature = "performance-profile")]
        let _profile =
            profiling::scope(profiling::Event::MmMap).bytes(range.end.saturating_sub(range.start));
        self.validate_range(&range)?;
        let flags = self.with_future_mlock(flags);
        let shared_writable = flags.contains_all(VmFlags::SHARED | VmFlags::WRITE);
        let mapped_file = Arc::clone(&file);
        let area = VmArea {
            range,
            flags,
            backing: VmBacking::File { file, offset },
        };
        {
            let mut vmas = self.vmas.lock();
            self.check_map_policy(
                vmas.len() + 1,
                Self::area_page_count(&area),
                flags.has(VmFlags::NORESERVE),
            )?;
            vmas.insert(area.clone())?;
            if shared_writable {
                mapped_file.disable_private_page_cache();
            }
        }
        self.account_area_insert(&area);
        mapped_file.on_mapped();
        Ok(())
    }

    /// MAP_FIXED 原子操作：在同一把 VMA 锁内先 unmap 再 insert，消除竞态窗口。
    pub fn map_fixed_anon(&self, range: Range<usize>, flags: VmFlags) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let flags = self.with_future_mlock(flags);
        let backing = if flags.has(VmFlags::SHARED) {
            VmBacking::SharedAnon {
                object: Arc::new(SharedAnonObject::new()),
                offset: 0,
            }
        } else {
            VmBacking::anonymous()
        };
        let area = VmArea {
            range: range.clone(),
            flags: flags.with(VmFlags::ANON),
            backing,
        };
        let (removed_areas, removed) = {
            let mut vmas = self.vmas.lock();
            if Self::contains_sealed(&vmas, &range) {
                return Err(Errno::EPERM);
            }
            self.check_map_policy(
                vmas.len() + 1,
                Self::area_page_count(&area),
                flags.has(VmFlags::NORESERVE),
            )?;
            let removed_areas = vmas.unmap_range(&range);
            if let Err(err) = vmas.insert(area.clone()) {
                drop(vmas);
                Self::notify_file_unmapped(&removed_areas);
                return Err(err);
            }
            // VMA 替换和旧 resident/PTE 清理必须对 fault 提交呈现为同一事务。
            // fault-around 也遵循 vmas -> pages，因此不会在新 VMA 可见后又被
            // 本次旧映射清理误删。
            let removed = self.unmap_page_mappings(range.clone())?;
            (removed_areas, removed)
        };
        for removed_area in &removed_areas {
            self.account_area_remove(removed_area);
        }
        self.account_area_insert(&area);
        Self::notify_file_unmapped(&removed_areas);
        if !removed.is_empty() {
            self.invalidate_user_range(range.start, range.end - range.start);
        }
        drop(removed);
        drop(removed_areas);
        prune_shared_anon_pages();
        Ok(())
    }

    pub fn map_fixed_file(
        &self,
        range: Range<usize>,
        file: Arc<dyn FileLike>,
        offset: u64,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let flags = self.with_future_mlock(flags);
        let shared_writable = flags.contains_all(VmFlags::SHARED | VmFlags::WRITE);
        let mapped_file = Arc::clone(&file);
        let area = VmArea {
            range: range.clone(),
            flags,
            backing: VmBacking::File { file, offset },
        };
        let (removed_areas, removed) = {
            let mut vmas = self.vmas.lock();
            if Self::contains_sealed(&vmas, &range) {
                return Err(Errno::EPERM);
            }
            self.check_map_policy(
                vmas.len() + 1,
                Self::area_page_count(&area),
                flags.has(VmFlags::NORESERVE),
            )?;
            let removed_areas = vmas.unmap_range(&range);
            if let Err(err) = vmas.insert(area.clone()) {
                drop(vmas);
                Self::notify_file_unmapped(&removed_areas);
                return Err(err);
            }
            if shared_writable {
                mapped_file.disable_private_page_cache();
            }
            let removed = self.unmap_page_mappings(range.clone())?;
            (removed_areas, removed)
        };
        for removed_area in &removed_areas {
            self.account_area_remove(removed_area);
        }
        self.account_area_insert(&area);
        Self::notify_file_unmapped(&removed_areas);
        mapped_file.on_mapped();
        if !removed.is_empty() {
            self.invalidate_user_range(range.start, range.end - range.start);
        }
        drop(removed);
        drop(removed_areas);
        prune_shared_anon_pages();
        Ok(())
    }

    /// 注册并立即建立一段 direct physical mapping。
    #[kernel_symbols::export(name = "general.mm.VmSpace.map_direct", contract = "kernel.mm.mapping@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn map_direct(
        &self,
        range: Range<usize>,
        paddr: usize,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let page_size = page_size();
        if paddr % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        let area_flags = self.with_future_mlock(flags).with(VmFlags::USER);
        let area = VmArea {
            range: range.clone(),
            flags: area_flags,
            backing: VmBacking::Direct(paddr),
        };
        {
            let mut set = self.vmas.lock();
            if Self::contains_sealed(&set, &range) {
                return Err(Errno::EPERM);
            }
            self.check_map_policy(
                set.len() + 1,
                Self::area_page_count(&area),
                area_flags.has(VmFlags::NORESERVE),
            )?;
            set.insert(area.clone())?;
        }
        self.account_area_insert(&area);
        if let Err(error) = self.populate_direct_mapping(range.clone(), paddr, area_flags) {
            // 回滚路径复用 unmap：其中的记账移除会与上面的插入记账对冲。
            let _ = self.unmap_existing(range);
            return Err(error);
        }
        Ok(())
    }

    fn populate_direct_mapping(
        &self,
        range: Range<usize>,
        paddr: usize,
        area_flags: VmFlags,
    ) -> Result<(), Errno> {
        let page_size = page_size();
        let mut pages = self.pages.lock();
        let mut va = range.start;
        while va < range.end {
            let off = va - range.start;
            let page = ResidentPage::new_direct(paddr + off);
            let access = access_for_new_page(area_flags, &page);
            self.map_page_no_flush(va, page.paddr(), pte_flags_for(area_flags, access))?;
            pages.insert(va, PageMapping { page, access });
            va += page_size;
        }
        let mapped = pages.len();
        self.mapped_pages.store(mapped, Ordering::Release);
        drop(pages);
        self.publish_new_user_range(range.start, range.end - range.start);
        Ok(())
    }

    /// 取消映射。同时把已 commit 的页表项摘掉；物理页由 resident page refcount 回收。
    #[kernel_symbols::export(name = "general.mm.VmSpace.unmap", contract = "kernel.mm.mapping@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn unmap(&self, range: Range<usize>) -> Result<(), Errno> {
        self.unmap_inner(range, false)
    }

    /// 仅在目标范围被 VMA 完整覆盖时取消映射。
    ///
    /// 覆盖检查和 VMA 摘除共用同一临界区，供要求 no-hole 语义的 Native ABI 使用。
    pub fn unmap_existing(&self, range: Range<usize>) -> Result<(), Errno> {
        self.unmap_inner(range, true)
    }

    fn unmap_inner(&self, range: Range<usize>, require_existing: bool) -> Result<(), Errno> {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::MmUnmap)
            .bytes(range.end.saturating_sub(range.start));
        self.validate_range(&range)?;
        let (removed_areas, removed) = {
            let mut vmas = self.vmas.lock();
            if require_existing && !vmas.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
            if Self::contains_sealed(&vmas, &range) {
                // mseal 语义：munmap 命中密封区域返回 EPERM，且不做部分拆除。
                return Err(Errno::EPERM);
            }
            let removed_areas = vmas.unmap_range(&range);
            let removed = self.unmap_page_mappings(range.clone())?;
            (removed_areas, removed)
        };
        for removed_area in &removed_areas {
            self.account_area_remove(removed_area);
        }
        Self::notify_file_unmapped(&removed_areas);
        if !removed.is_empty() {
            self.invalidate_user_range(range.start, range.end - range.start);
        }
        drop(removed);
        drop(removed_areas);
        prune_shared_anon_pages();
        Ok(())
    }

    /// 调整一段既有映射的大小或位置。
    ///
    /// 这是 `mremap(2)` 的核心实现：VMA 元数据迁移与页表迁移在这里保持一致。
    /// 不支持 `DONTUNMAP` 的双映射语义，因为那需要额外的 resident page 所有权
    /// 标记；普通 shrink / in-place grow / move / fixed move 都在此闭环。
    pub fn mremap(
        &self,
        old_range: Range<usize>,
        new_len: usize,
        may_move: bool,
        fixed_addr: Option<usize>,
    ) -> Result<usize, Errno> {
        self.validate_range(&old_range)?;
        let page_size = page_size();
        if new_len == 0 || new_len % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        let old_len = old_range.end - old_range.start;
        if new_len <= old_len {
            if new_len < old_len {
                self.unmap(old_range.start + new_len..old_range.end)?;
            }
            return Ok(old_range.start);
        }

        let in_place_end = old_range.start.checked_add(new_len).ok_or(Errno::EINVAL)?;
        let in_place_tail = old_range.end..in_place_end;
        {
            let set = self.vmas.lock();
            if Self::contains_sealed(&set, &old_range) {
                return Err(Errno::EPERM);
            }
        }
        if fixed_addr == Some(old_range.start) {
            return if self.extend_mapping_in_place(&old_range, &in_place_tail)? {
                Ok(old_range.start)
            } else {
                Err(Errno::ENOMEM)
            };
        }
        if fixed_addr.is_none() && self.extend_mapping_in_place(&old_range, &in_place_tail)? {
            return Ok(old_range.start);
        }
        if !may_move && fixed_addr.is_none() {
            return Err(Errno::ENOMEM);
        }

        let new_start = if let Some(addr) = fixed_addr {
            addr
        } else {
            self.alloc_mmap_range(new_len)?.start
        };
        let new_end = new_start.checked_add(new_len).ok_or(Errno::EINVAL)?;
        let new_range = new_start..new_end;
        self.validate_range(&new_range)?;
        if ranges_overlap(&old_range, &new_range) && new_range.start != old_range.start {
            return Err(Errno::EINVAL);
        }

        let (removed_target, mapped_tail, removed_pages, moved_pages) = {
            let mut vmas = self.vmas.lock();
            if !vmas.contains_range(&old_range) {
                return Err(Errno::ENOMEM);
            }
            if Self::contains_sealed(&vmas, &new_range) {
                return Err(Errno::EPERM);
            }
            let removed_target = if fixed_addr.is_some() {
                vmas.unmap_range(&new_range)
            } else {
                if !vmas.is_range_free(&new_range) {
                    return Err(Errno::EEXIST);
                }
                Vec::new()
            };
            let old_pieces = vmas.unmap_range(&old_range);
            let old_covered = covered_len(&old_pieces, &old_range);
            if old_covered != old_len {
                return Err(Errno::ENOMEM);
            }

            let mut cursor = new_range.start;
            let mut last_inserted = None;
            for mut area in old_pieces {
                let len = area.range.end - area.range.start;
                area.range = cursor..cursor + len;
                cursor += len;
                last_inserted = Some(area.clone());
                vmas.insert(area)?;
            }

            let mapped_tail = if cursor < new_range.end {
                let last = last_inserted.ok_or(Errno::ENOMEM)?;
                let last_len = last.range.end - last.range.start;
                let backing = last.backing.checked_shift(last_len).ok_or(Errno::EINVAL)?;
                let tail = VmArea {
                    range: cursor..new_range.end,
                    flags: last.flags,
                    backing,
                };
                let files = Self::collect_file_backings(core::iter::once(&tail));
                vmas.insert(tail.clone())?;
                Some(tail)
            } else {
                None
            };
            let removed_pages = self.unmap_page_mappings(new_range.clone())?;
            let moved_pages =
                self.move_page_mappings_locked(&vmas, old_range.start, new_range.start, old_len)?;
            (removed_target, mapped_tail, removed_pages, moved_pages)
        };
        for removed_area in &removed_target {
            self.account_area_remove(removed_area);
        }
        if let Some(tail) = &mapped_tail {
            self.account_area_insert(tail);
        }
        Self::notify_file_unmapped(&removed_target);
        if let Some(tail) = &mapped_tail {
            Self::notify_files_mapped(Self::collect_file_backings(core::iter::once(tail)));
        }
        if !removed_pages.is_empty() {
            self.invalidate_user_range(new_range.start, new_range.end - new_range.start);
        }
        if moved_pages {
            self.invalidate_user_range(old_range.start, old_len);
            self.invalidate_user_range(new_range.start, old_len);
        }
        drop(removed_pages);
        drop(removed_target);
        prune_shared_anon_pages();
        self.mmap_next.store(new_range.end, Ordering::Release);
        Ok(new_range.start)
    }

    /// `mremap(MREMAP_DONTUNMAP)`：把私有匿名映射搬到新地址，同时把旧地址替换为
    /// 一段不驻留的空匿名映射（Linux 5.7+ 的原地扩展双映射语义）。
    ///
    /// 限制与取舍：仅支持私有匿名映射（Linux 同样要求）；不支持 `new_len <
    /// old_len` 的收缩（返回 `EINVAL`）。迁移后旧地址首次访问得到零页，新地址
    /// 持有原页。
    pub fn mremap_dontunmap(
        &self,
        old_range: Range<usize>,
        new_len: usize,
        fixed_addr: Option<usize>,
    ) -> Result<usize, Errno> {
        self.validate_range(&old_range)?;
        let page_size = page_size();
        if new_len == 0 || new_len % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        let old_len = old_range.end - old_range.start;
        if new_len < old_len {
            return Err(Errno::EINVAL);
        }
        // 旧范围必须被私有匿名 VMA 连续覆盖。
        {
            let set = self.vmas.lock();
            if !set.contains_range(&old_range) {
                return Err(Errno::ENOMEM);
            }
            for area in set.iter_overlap(&old_range) {
                if !matches!(area.backing, VmBacking::Anon { .. })
                    || area.flags.has(VmFlags::SHARED)
                {
                    return Err(Errno::EINVAL);
                }
            }
            if Self::contains_sealed(&set, &old_range) {
                return Err(Errno::EPERM);
            }
        }
        let new_start = if let Some(addr) = fixed_addr {
            addr
        } else {
            self.alloc_mmap_range(new_len)?.start
        };
        let new_end = new_start.checked_add(new_len).ok_or(Errno::EINVAL)?;
        let new_range = new_start..new_end;
        self.validate_range(&new_range)?;
        if ranges_overlap(&old_range, &new_range) {
            return Err(Errno::EINVAL);
        }

        let (removed_target, empty_anon, tail, moved_pages) = {
            let mut vmas = self.vmas.lock();
            if !vmas.contains_range(&old_range) {
                return Err(Errno::ENOMEM);
            }
            if Self::contains_sealed(&vmas, &new_range) {
                return Err(Errno::EPERM);
            }
            let removed_target = if fixed_addr.is_some() {
                vmas.unmap_range(&new_range)
            } else {
                if !vmas.is_range_free(&new_range) {
                    return Err(Errno::EEXIST);
                }
                Vec::new()
            };
            let old_pieces = vmas.unmap_range(&old_range);
            let old_covered = covered_len(&old_pieces, &old_range);
            if old_covered != old_len {
                return Err(Errno::ENOMEM);
            }

            // 1) 旧地址:插入空匿名 VMA(保留原权限/标志,换用全新合并域)。
            let mut empty_anon = Vec::with_capacity(old_pieces.len());
            let mut cursor = old_range.start;
            for mut area in old_pieces.clone() {
                let len = area.range.end - area.range.start;
                area.backing = VmBacking::anonymous();
                area.flags = area.flags.without(VmFlags::SHARED).with(VmFlags::ANON);
                area.range = cursor..cursor + len;
                cursor += len;
                vmas.insert(area.clone())?;
                empty_anon.push(area);
            }
            // 2) 新地址:插入搬移后的原 VMA。
            let mut cursor = new_range.start;
            let mut last_inserted = None;
            for mut area in old_pieces {
                let len = area.range.end - area.range.start;
                area.range = cursor..cursor + len;
                cursor += len;
                last_inserted = Some(area.clone());
                vmas.insert(area)?;
            }
            let tail = if cursor < new_range.end {
                let last = last_inserted.ok_or(Errno::ENOMEM)?;
                let last_len = last.range.end - last.range.start;
                let backing = last.backing.checked_shift(last_len).ok_or(Errno::EINVAL)?;
                let tail = VmArea {
                    range: cursor..new_range.end,
                    flags: last.flags,
                    backing,
                };
                vmas.insert(tail.clone())?;
                Some(tail)
            } else {
                None
            };
            let removed_pages = self.unmap_page_mappings(new_range.clone())?;
            let moved_pages =
                self.move_page_mappings_locked(&vmas, old_range.start, new_range.start, old_len)?;
            (removed_target, empty_anon, tail, moved_pages)
        };
        for area in &removed_target {
            self.account_area_remove(area);
        }
        for area in &empty_anon {
            self.account_area_insert(area);
        }
        if let Some(tail) = &tail {
            self.account_area_insert(tail);
        }
        Self::notify_file_unmapped(&removed_target);
        if moved_pages {
            self.invalidate_user_range(old_range.start, old_len);
            self.invalidate_user_range(new_range.start, old_len);
        }
        drop(removed_target);
        prune_shared_anon_pages();
        self.mmap_next.store(new_range.end, Ordering::Release);
        Ok(new_range.start)
    }

    /// 修改权限。要求整个 range 已被 VMA 连续覆盖。
    #[kernel_symbols::export(name = "general.mm.VmSpace.mprotect", contract = "kernel.mm.mapping@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn mprotect(&self, range: Range<usize>, new_flags: VmFlags) -> Result<(), Errno> {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::MmProtect)
            .bytes(range.end.saturating_sub(range.start));
        self.validate_range(&range)?;
        let mut touched = false;
        {
            let mut set = self.vmas.lock();
            let single_area = set
                .find(range.start)
                .is_some_and(|area| range.end <= area.range.end);
            if !single_area && !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
            // mseal 语义：密封区域禁止 mprotect。
            if Self::contains_sealed(&set, &range) {
                return Err(Errno::EPERM);
            }
            // Linux mprotect 语义：`MAP_SHARED` 映射请求 `PROT_WRITE` 时，底层
            // 文件句柄必须可写（`file->f_mode & FMODE_WRITE`），否则返回 EACCES。
            // 只读 fd 的 MAP_SHARED 映射不能通过 mprotect 提权。
            if new_flags.has(VmFlags::WRITE) {
                for area in set.iter_overlap(&range) {
                    if !area.flags.has(VmFlags::SHARED) {
                        continue;
                    }
                    if let VmBacking::File { file, .. } = &area.backing
                        && file.writable_hint() == Some(false)
                    {
                        return Err(Errno::EACCES);
                    }
                }
            }
            if new_flags.has(VmFlags::WRITE) && single_area {
                let area = set
                    .find(range.start)
                    .expect("单 VMA 判定后区域必须仍然存在");
                if area.flags.has(VmFlags::SHARED)
                    && let VmBacking::File { file, .. } = &area.backing
                {
                    file.disable_private_page_cache();
                }
            } else if new_flags.has(VmFlags::WRITE) {
                for area in set.iter_overlap(&range) {
                    if !area.flags.has(VmFlags::SHARED) {
                        continue;
                    }
                    if let VmBacking::File { file, .. } = &area.backing {
                        file.disable_private_page_cache();
                    }
                }
            }
            let protected_flags = new_flags.with(VmFlags::USER);
            match set.protect_single_area(&range, protected_flags) {
                Some(false) => {
                    #[cfg(feature = "performance-profile")]
                    profiling::record(
                        profiling::Event::MmProtectNoop,
                        0,
                        range.end.saturating_sub(range.start) as u64,
                        1,
                    );
                }
                Some(true) => {}
                None => {
                    set.protect_range(&range, protected_flags);
                }
            }

            let mut pages = self.pages.lock();
            // 只遍历已经驻留的页，避免在动态链接器的大量稀疏 mprotect 范围中
            // 对每个空洞页分别执行 VMA 与常驻页映射表查找。
            let page_size = page_size();
            let mut batch: Option<(usize, usize, PageAccess, VmFlags)> = None;
            let mut protect_error = None;
            for area in set.iter_overlap(&range) {
                let resident_range =
                    area.range.start.max(range.start)..area.range.end.min(range.end);
                let area_flags = area.flags;
                pages.for_each_range_mut(resident_range, |va, mapping| {
                    if protect_error.is_some() {
                        return;
                    }
                    let access = access_for_existing_page(area_flags, &mapping.page);
                    let flags = pte_flags_for(area_flags, access);
                    mapping.access = access;
                    if let Some((batch_start, batch_end, batch_access, batch_flags)) = batch {
                        if va == batch_end && access == batch_access && flags == batch_flags {
                            batch = Some((
                                batch_start,
                                batch_end + page_size,
                                batch_access,
                                batch_flags,
                            ));
                            return;
                        }
                        if let Err(err) = self.protect_pages_no_flush(
                            batch_start,
                            batch_end - batch_start,
                            batch_flags,
                        ) {
                            protect_error = Some(err);
                            return;
                        }
                        #[cfg(feature = "performance-profile")]
                        profiling::record(
                            profiling::Event::MmProtectBatch,
                            0,
                            (batch_end - batch_start) as u64,
                            ((batch_end - batch_start) / page_size) as u64,
                        );
                        touched = true;
                    }
                    batch = Some((va, va + page_size, access, flags));
                });
            }
            if let Some(err) = protect_error {
                return Err(err);
            }
            if let Some((batch_start, batch_end, _, batch_flags)) = batch {
                self.protect_pages_no_flush(batch_start, batch_end - batch_start, batch_flags)?;
                #[cfg(feature = "performance-profile")]
                profiling::record(
                    profiling::Event::MmProtectBatch,
                    0,
                    (batch_end - batch_start) as u64,
                    ((batch_end - batch_start) / page_size) as u64,
                );
                touched = true;
            }
        }
        if touched {
            self.invalidate_user_range(range.start, range.end - range.start);
        }
        Ok(())
    }

    #[kernel_symbols::export(name = "general.mm.VmSpace.resident_bitmap", contract = "kernel.mm.query@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED | kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC)]
    pub fn resident_bitmap(&self, range: Range<usize>) -> Result<Vec<u8>, Errno> {
        self.validate_range(&range)?;
        let page_size = page_size();
        let page_count = (range.end - range.start) / page_size;
        // Linux `mincore(2)` 逐页遍历页表：VMA 空洞（未映射页）在向量中置 0，
        // 不要求范围被 VMA 连续覆盖，也不返回 ENOMEM/EFAULT。
        let pages = self.pages.lock();
        let mut out = Vec::new();
        out.try_reserve_exact(page_count)
            .map_err(|_| Errno::ENOMEM)?;
        let mut va = range.start;
        while va < range.end {
            out.push(if pages.contains_key(va) { 1 } else { 0 });
            va += page_size;
        }
        Ok(out)
    }

    /// 校验一段用户 VMA 是否连续存在，不触发缺页也不改变页表状态。
    pub fn contains_user_range(&self, range: Range<usize>) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let set = self.vmas.lock();
        if !set.contains_range(&range) {
            return Err(Errno::ENOMEM);
        }
        Ok(())
    }

    /// 校验一段用户 VMA 连续存在且每一段都包含指定权限。
    pub fn contains_user_range_with_flags(
        &self,
        range: Range<usize>,
        required: u32,
    ) -> Result<(), Errno> {
        self.validate_range(&range)?;
        if required == 0 {
            return Err(Errno::EINVAL);
        }
        let set = self.vmas.lock();
        let mut cursor = range.start;
        for area in set.iter_overlap(&range) {
            if area.range.start > cursor || !area.flags.contains_all(required) {
                return Err(Errno::EACCES);
            }
            cursor = cursor.max(area.range.end);
            if cursor >= range.end {
                return Ok(());
            }
        }
        Err(Errno::ENOMEM)
    }

    /// 丢弃指定范围内已经常驻的页，保留 VMA 语义供后续缺页按 backing 重建。
    ///
    /// 带 `LOCKED` 标记的区域返回 `EINVAL`——这是 Linux `MADV_DONTNEED` 对
    /// mlock 区域的行为；`MADV_DONTNEED_LOCKED` 走 [`Self::discard_resident_range_locked`]。
    pub fn discard_resident_range(&self, range: Range<usize>) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let removed = {
            let set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
            if set
                .iter_overlap(&range)
                .any(|area| area.flags.has(VmFlags::LOCKED))
            {
                return Err(Errno::EINVAL);
            }
            self.unmap_page_mappings(range.clone())?
        };
        if !removed.is_empty() {
            self.invalidate_user_range(range.start, range.end - range.start);
        }
        drop(removed);
        Ok(())
    }

    /// `MADV_DONTNEED_LOCKED`：同 [`Self::discard_resident_range`]，但允许命中
    /// 已锁区域（Linux 5.18+ 语义）。
    pub fn discard_resident_range_locked(&self, range: Range<usize>) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let removed = {
            let set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
            self.unmap_page_mappings(range.clone())?
        };
        if !removed.is_empty() {
            self.invalidate_user_range(range.start, range.end - range.start);
        }
        drop(removed);
        Ok(())
    }

    /// flag 类 `madvise` advice（DONTFORK/DOFORK/MERGEABLE/UNMERGEABLE/
    /// HUGEPAGE/NOHUGEPAGE/DONTDUMP/DODUMP/WIPEONFORK/KEEPONFORK）：
    /// 对范围内全部 VMA 应用 `update`。范围必须被 VMA 连续覆盖。
    ///
    /// 不触发任何页级动作；`update_flags_range` 的拆分/合并保持 VMA 几何不变，
    /// 因此不影响承诺/锁页记账。
    pub fn update_area_flags(
        &self,
        range: Range<usize>,
        update: impl Fn(VmFlags) -> VmFlags,
    ) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let mut set = self.vmas.lock();
        if !set.contains_range(&range) {
            return Err(Errno::ENOMEM);
        }
        set.update_flags_range(&range, update);
        Ok(())
    }

    /// `MADV_PAGEOUT`：尝试回收范围内的页。
    ///
    /// - 共享文件页：先写回脏数据再丢弃（内容保留在文件中，重读恢复）；
    /// - 私有文件页：直接丢弃（干净页重读恢复；脏私有页内容丢失，与 Linux 一致）；
    /// - 匿名页：有 swap 空间时换出；`MADV_FREE`/`MAP_DROPPABLE` 页直接丢弃
    ///   （内容读回零页）；无 swap 空间时保持不动（等价无 swap 的 Linux 行为）；
    /// - 已锁页：跳过（unevictable）。
    pub fn madvise_pagout(&self, range: Range<usize>) -> Result<(), Errno> {
        self.validate_range(&range)?;
        {
            let set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
        }
        let shared_dirty: Vec<Arc<ResidentPage>> = {
            let pages = self.pages.lock();
            let mut out = Vec::new();
            pages.for_each_range(range.clone(), |_va, mapping| {
                if mapping.page.is_shared_file() && mapping.page.is_dirty() {
                    out.push(Arc::clone(&mapping.page));
                }
            });
            out
        };
        for page in shared_dirty {
            page.flush_to_backing()?;
        }
        let file_subranges: Vec<Range<usize>> = {
            let set = self.vmas.lock();
            let mut out = Vec::new();
            for area in set.iter_overlap(&range) {
                if area.flags.has(VmFlags::LOCKED) {
                    continue;
                }
                if !matches!(area.backing, VmBacking::File { .. }) {
                    continue;
                }
                out.push(area.range.start.max(range.start)..area.range.end.min(range.end));
            }
            out
        };
        for sub in file_subranges {
            let removed = self.unmap_page_mappings(sub.clone())?;
            if !removed.is_empty() {
                self.invalidate_user_range(sub.start, sub.end - sub.start);
            }
            drop(removed);
        }
        self.pagout_anon_subranges(&range)?;
        Ok(())
    }

    /// `MADV_PAGEOUT` 的匿名页部分：换出到 swap，或对 FREE/DROPPABLE 页直接丢弃。
    fn pagout_anon_subranges(&self, range: &Range<usize>) -> Result<(), Errno> {
        let page_size = page_size();
        let anon_subranges: Vec<(Range<usize>, bool)> = {
            let set = self.vmas.lock();
            let mut out = Vec::new();
            for area in set.iter_overlap(range) {
                if area.flags.has(VmFlags::LOCKED) {
                    continue;
                }
                if !matches!(area.backing, VmBacking::Anon { .. }) {
                    continue;
                }
                let sub = area.range.start.max(range.start)..area.range.end.min(range.end);
                out.push((sub, area.flags.has(VmFlags::DROPPABLE)));
            }
            out
        };
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;
        for (sub, area_droppable) in anon_subranges {
            // 收集驻留的私有匿名页(不持锁做 I/O)。
            let resident: Vec<(usize, Arc<ResidentPage>)> = {
                let pages = self.pages.lock();
                let mut out = Vec::new();
                pages.for_each_range(sub.clone(), |va, mapping| {
                    out.push((va, Arc::clone(&mapping.page)));
                });
                out
            };
            // 先确定每页是否被 MADV_FREE 标记(短锁),换出 I/O 不持任何自旋锁。
            let freeable_marked: Vec<bool> = {
                let freeable = self.freeable.lock();
                resident
                    .iter()
                    .map(|(va, _)| freeable.contains_key(*va))
                    .collect()
            };
            let mut to_drop = Vec::new();
            for ((va, page), freeable) in resident.iter().zip(freeable_marked) {
                if area_droppable || freeable {
                    // FREE/DROPPABLE：不写回,直接丢弃,后续读回零页。
                    to_drop.push(*va);
                    continue;
                }
                // 换出到 swap;无空闲槽位时保持驻留(等价无 swap 的 Linux 行为)。
                let buf = unsafe {
                    core::slice::from_raw_parts(virt_fn(page.paddr()) as *const u8, page_size)
                };
                if let Ok(slot) = crate::mm::swap::swap_out_page(buf) {
                    self.swapped.lock().insert(*va, slot);
                    to_drop.push(*va);
                }
            }
            for va in to_drop {
                let removed = self.unmap_page_mappings_preserve_swap(va..va + page_size)?;
                drop(removed);
                self.invalidate_user_range(va, page_size);
            }
        }
        Ok(())
    }

    /// `MADV_FREE`：标记范围内已驻留的私有匿名页为可释放，内容保留到实际回收。
    ///
    /// 本内核无 LRU/内存压力回收器，页在后续 `MADV_DONTNEED` / `MADV_PAGEOUT` /
    /// 显式回收点时才会被丢弃；无压力场景下内容始终保留，与 Linux 可观测行为一致。
    pub fn madvise_free(&self, range: Range<usize>) -> Result<(), Errno> {
        self.validate_range(&range)?;
        {
            let set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
        }
        let keys = { self.pages.lock().keys_in_range(range) };
        let mut freeable = self.freeable.lock();
        for va in keys {
            freeable.insert(va, ());
        }
        Ok(())
    }

    /// `MADV_COLD`：标记范围内已驻留的私有匿名页为冷页。
    ///
    /// 无 LRU/回收器时"冷"只影响 `MADV_PAGEOUT` 的回收优先级（本内核 PAGEOUT 会
    /// 换出范围内全部匿名页,因此冷标记不改变换出行为,仅作为可观测状态记录）。
    pub fn madvise_cold(&self, range: Range<usize>) -> Result<(), Errno> {
        self.validate_range(&range)?;
        {
            let set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
        }
        let keys = { self.pages.lock().keys_in_range(range) };
        let mut cold = self.cold.lock();
        for va in keys {
            cold.insert(va, ());
        }
        Ok(())
    }

    /// `MADV_REMOVE`：仅对 tmpfs/shmem 文件映射有效（Linux 语义）。
    ///
    /// 等价于对文件执行 `fallocate(PUNCH_HOLE | KEEP_SIZE)`（数据释放、读回零），
    /// 并丢弃范围内驻留页。范围内出现任何非 shmem VMA 即返回 `EINVAL`。
    pub fn madvise_remove(&self, range: Range<usize>) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let subranges: Vec<(Range<usize>, Arc<dyn FileLike>, u64)> = {
            let set = self.vmas.lock();
            let mut out = Vec::new();
            for area in set.iter_overlap(&range) {
                let VmBacking::File { file, offset } = &area.backing else {
                    return Err(Errno::EINVAL);
                };
                if !file.is_shmem() {
                    return Err(Errno::EINVAL);
                }
                let sub = area.range.start.max(range.start)..area.range.end.min(range.end);
                let file_off = offset.saturating_add((sub.start - area.range.start) as u64);
                out.push((sub, Arc::clone(file), file_off));
            }
            out
        };
        for (sub, file, file_off) in subranges {
            file.punch_hole(file_off, (sub.end - sub.start) as u64)?;
            let removed = self.unmap_page_mappings(sub.clone())?;
            if !removed.is_empty() {
                self.invalidate_user_range(sub.start, sub.end - sub.start);
            }
            drop(removed);
        }
        Ok(())
    }

    /// `MADV_WILLNEED` / `MADV_POPULATE_READ` / `MADV_POPULATE_WRITE`：
    /// 预解析范围内的全部页（匿名分配、文件读入或 COW）。
    ///
    /// `strict` 为 true（POPULATE_*）时，未映射或无法填充返回 `EINVAL`；
    /// WILLNEED 忽略填充错误（Linux 语义：尽力而为）。
    pub fn madvise_populate(
        &self,
        range: Range<usize>,
        write: bool,
        strict: bool,
    ) -> Result<(), Errno> {
        self.validate_range(&range)?;
        {
            let set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(if strict { Errno::EINVAL } else { Errno::ENOMEM });
            }
        }
        if strict {
            self.prefault_user_range(range, write)
                .map_err(|_| Errno::EINVAL)
        } else {
            let _ = self.prefault_user_range(range, write);
            Ok(())
        }
    }

    /// `remap_file_pages(2)` 实现：把 `[addr, addr+size)` 内**已驻留**的页
    /// 重新映射为文件内自 `pgoff*page_size` 起的内容。
    ///
    /// 与 Linux 语义一致：只有调用时已驻留的页被重映射；未驻留页不受影响，
    /// 后续缺页仍按 VMA 线性偏移读取（Linux 文档化的历史行为）。
    ///
    /// 限制与取舍：`prot`/`flags` 必须为 0（调用方校验）；范围必须整体落在单个
    /// file-backed VMA 内。重映射后的页不发布到共享页缓存——同一文件的其它
    /// 映射仍看到旧内容（与 Linux 共享页语义有细微差异，见注释）。
    pub fn remap_file_pages(&self, addr: usize, size: usize, pgoff: usize) -> Result<(), Errno> {
        if size == 0 {
            return Ok(());
        }
        let page_size = page_size();
        if addr % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        let len = align_up(size, page_size).ok_or(Errno::EINVAL)?;
        let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
        let (file, flags) = {
            let set = self.vmas.lock();
            let area = set.find(addr).ok_or(Errno::EINVAL)?;
            if end > area.range.end {
                return Err(Errno::EINVAL);
            }
            let VmBacking::File { file, .. } = &area.backing else {
                return Err(Errno::EINVAL);
            };
            (Arc::clone(file), area.flags)
        };
        let new_start_off = (pgoff as u64)
            .checked_mul(page_size as u64)
            .ok_or(Errno::EINVAL)?;
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;
        let file_size = file.size();
        let mut va = addr;
        while va < end {
            let present = { self.pages.lock().get(va).is_some() };
            if !present {
                va += page_size;
                continue;
            }
            let file_off = new_start_off
                .checked_add((va - addr) as u64)
                .ok_or(Errno::EINVAL)?;
            // 锁外分配并读入新内容（I/O 不持 VM 锁）。
            let new_paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
            let buf = unsafe {
                core::slice::from_raw_parts_mut(virt_fn(new_paddr) as *mut u8, page_size)
            };
            if file_off < file_size {
                let read_len = usize::try_from(file_size - file_off)
                    .unwrap_or(page_size)
                    .min(page_size);
                let mut done = 0usize;
                while done < read_len {
                    let n = file.read_at(file_off + done as u64, &mut buf[done..read_len])?;
                    if n == 0 {
                        break;
                    }
                    done += n;
                }
            }
            // 锁内换页：页仍驻留才替换，否则丢弃新页。
            let new_page = if flags.has(VmFlags::SHARED) {
                ResidentPage::new_shared_file(
                    new_paddr,
                    Arc::clone(&file),
                    file_off,
                    shared_file_page_generation(&file),
                )
            } else {
                ResidentPage::new_private_file(new_paddr)
            };
            let mut pages = self.pages.lock();
            let Some(mapping) = pages.get_mut(va) else {
                drop(pages);
                free_user_page(new_paddr);
                va += page_size;
                continue;
            };
            let access = mapping.access;
            let paddr = new_page.paddr();
            self.replace_page_no_flush(va, paddr, pte_flags_for(flags, access))?;
            mapping.page = new_page;
            drop(pages);
            self.invalidate_user_range(va, page_size);
            va += page_size;
        }
        Ok(())
    }

    /// 设置进程默认 NUMA 内存策略（`set_mempolicy(2)` 单节点语义）。
    pub fn set_task_mempolicy(&self, policy: Option<Mempolicy>) {
        self.mempolicy.lock().default_policy = policy;
    }

    /// 读取进程默认 NUMA 内存策略。
    pub fn task_mempolicy(&self) -> Option<Mempolicy> {
        self.mempolicy.lock().default_policy
    }

    /// `mbind(2)` 单节点语义：把区域策略写入策略表，替换被覆盖的旧区域。
    /// 范围必须已被 VMA 连续覆盖（否则 `ENOMEM`）。
    pub fn mbind_range(&self, range: Range<usize>, policy: Mempolicy) -> Result<(), Errno> {
        self.validate_range(&range)?;
        {
            let set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
        }
        let mut state = self.mempolicy.lock();
        state
            .ranges
            .retain(|(start, end), _| !(*start < range.end && range.start < *end));
        state.ranges.insert((range.start, range.end), policy);
        Ok(())
    }

    /// 查询 `addr` 生效的 NUMA 内存策略（`get_mempolicy(MPOL_F_ADDR)`）。
    /// 返回 `(策略, 是否命中区域覆盖)`；未命中区域覆盖时返回默认策略。
    pub fn mempolicy_at(&self, addr: usize) -> (Option<Mempolicy>, bool) {
        let state = self.mempolicy.lock();
        for ((start, end), policy) in &state.ranges {
            if *start <= addr && addr < *end {
                return (Some(*policy), true);
            }
        }
        (state.default_policy, false)
    }

    /// `set_mempolicy_home_node(2)`：为范围上已存在的 `MPOL_BIND`（或
    /// `MPOL_PREFERRED`）区域策略设置 home node。
    ///
    /// Linux 语义：范围必须被带 `MPOL_BIND`/`MPOL_PREFERRED` 策略的 VMA 连续
    /// 覆盖，否则返回 `EINVAL`；home node 必须属于该策略的节点集，否则 `EINVAL`。
    /// 单节点系统 home node 只能为 0，因此这里主要校验"范围确实存在可设置
    /// home node 的策略"，并记录该设置（供 `get_mempolicy` 观测）。
    pub fn set_mempolicy_home_node(
        &self,
        range: Range<usize>,
        home_node: u32,
    ) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let mut state = self.mempolicy.lock();
        let mut cursor = range.start;
        for ((start, end), policy) in state.ranges.iter_mut() {
            if *end <= range.start || *start >= range.end {
                continue;
            }
            if *start > cursor {
                return Err(Errno::EINVAL);
            }
            if !matches!(policy.mode, 2 /* MPOL_BIND */ | 1 /* MPOL_PREFERRED */) {
                return Err(Errno::EINVAL);
            }
            policy.home_node = home_node;
            cursor = cursor.max(*end);
            if cursor >= range.end {
                return Ok(());
            }
        }
        Err(Errno::EINVAL)
    }

    /// 确保远程地址空间（其它进程的 `VmSpace`）的 `addr` 页可访问。
    ///
    /// `process_vm_readv/writev` 使用：缺页、COW 通过目标地址空间自己的
    /// `handle_fault` 完成（所有页表操作都以 `self.pgd` 为句柄，不依赖当前
    /// 任务的地址空间）。无法修复返回 `EFAULT`。
    pub fn ensure_remote_page(&self, addr: usize, kind: FaultKind) -> Result<(), Errno> {
        match self.handle_fault(addr, kind) {
            FaultOutcome::Fixed => Ok(()),
            FaultOutcome::Segv | FaultOutcome::OutOfMemory | FaultOutcome::Kernel(_) => {
                Err(Errno::EFAULT)
            }
        }
    }

    /// 把远程地址空间 `[range.start, range.start+out.len())` 的驻留页内容拷入
    /// `out`。要求范围内每页都已驻留（调用方先用 [`Self::ensure_remote_page`]
    /// 逐页解析）；缺失页返回 `EFAULT`。起点允许落在页内。
    pub fn copy_resident_bytes_out(
        &self,
        range: Range<usize>,
        out: &mut [u8],
    ) -> Result<(), Errno> {
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EFAULT)?;
        let pages = self.pages.lock();
        let mut written = 0usize;
        let mut va = page_base(range.start);
        let mut skip = range.start - va;
        while written < out.len() {
            let mapping = pages.get(va).ok_or(Errno::EFAULT)?;
            let copy_len = out.len().saturating_sub(written).min(page_size() - skip);
            let src = virt_fn(mapping.page.paddr()) + skip;
            // Safety: 页由 pages 锁内的 Arc 保活；copy_len 不越过页边界。
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src as *const u8,
                    out[written..].as_mut_ptr(),
                    copy_len,
                );
            }
            written += copy_len;
            va += page_size();
            skip = 0;
        }
        Ok(())
    }

    /// 把 `input` 拷入远程地址空间的 `[range.start, range.start+input.len())`。
    /// 要求范围内每页已驻留且 PTE 可写（调用方先用
    /// [`Self::ensure_remote_page`](addr, Store) 完成 COW）；成功后标脏。
    /// 起点允许落在页内。
    pub fn copy_resident_bytes_in(&self, range: Range<usize>, input: &[u8]) -> Result<(), Errno> {
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EFAULT)?;
        let pages = self.pages.lock();
        let mut written = 0usize;
        let mut va = page_base(range.start);
        let mut skip = range.start - va;
        while written < input.len() {
            let mapping = pages.get(va).ok_or(Errno::EFAULT)?;
            if !mapping.access.pte_writable() {
                return Err(Errno::EFAULT);
            }
            let copy_len = input.len().saturating_sub(written).min(page_size() - skip);
            let dst = virt_fn(mapping.page.paddr()) + skip;
            // Safety: 页由 pages 锁内的 Arc 保活；copy_len 不越过页边界。
            unsafe {
                core::ptr::copy_nonoverlapping(input[written..].as_ptr(), dst as *mut u8, copy_len);
            }
            mapping.page.mark_dirty();
            written += copy_len;
            va += page_size();
            skip = 0;
        }
        Ok(())
    }

    // ── userfaultfd ──────────────────────────────────────────────────────────

    /// `UFFDIO_REGISTER`：登记一段私有匿名区域交给用户态处理缺页。
    ///
    /// 校验：页对齐、长度非零、范围被私有匿名 VMA 连续覆盖（否则 `EINVAL`）、
    /// 与既有登记不重叠（否则 `EEXIST`）。shmem/hugetlb/file 区域不支持。
    pub(crate) fn uffd_register(
        &self,
        start: usize,
        len: usize,
        mode: u64,
        state: &Arc<UffdState>,
    ) -> Result<Range<usize>, Errno> {
        let page_size = page_size();
        if start % page_size != 0 || len == 0 {
            return Err(Errno::EINVAL);
        }
        let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
        let end = start.checked_add(len).ok_or(Errno::EINVAL)?;
        let range = start..end;
        {
            let set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::EINVAL);
            }
            for area in set.iter_overlap(&range) {
                if !matches!(area.backing, VmBacking::Anon { .. })
                    || area.flags.has(VmFlags::SHARED)
                {
                    return Err(Errno::EINVAL);
                }
            }
        }
        let mut regions = self.uffd_regions.lock();
        if regions
            .iter()
            .any(|region| region.range.start < end && start < region.range.end)
        {
            return Err(Errno::EEXIST);
        }
        regions.push(UffdRegion {
            range: range.clone(),
            mode,
            state: Arc::clone(state),
        });
        Ok(range)
    }

    /// `UFFDIO_UNREGISTER`：摘除与范围相交、且属于指定状态对象的登记。
    pub(crate) fn uffd_unregister(
        &self,
        start: usize,
        len: usize,
        state: &Arc<UffdState>,
    ) -> Result<(), Errno> {
        let page_size = page_size();
        if start % page_size != 0 || len == 0 {
            return Err(Errno::EINVAL);
        }
        let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
        let end = start.checked_add(len).ok_or(Errno::EINVAL)?;
        let mut regions = self.uffd_regions.lock();
        regions.retain(|region| {
            !(Arc::ptr_eq(&region.state, state)
                && region.range.start < end
                && start < region.range.end)
        });
        Ok(())
    }

    /// fd 关闭时由 `UffdState::release` 调用：摘除指定状态对象的登记。
    pub(crate) fn uffd_remove_state(&self, state: &Arc<UffdState>, range: &Range<usize>) {
        let mut regions = self.uffd_regions.lock();
        regions.retain(|region| {
            !(Arc::ptr_eq(&region.state, state) && ranges_overlap(&region.range, range))
        });
    }

    /// `UFFDIO_COPY`：把调用者用户内存拷入目标地址空间并安装为匿名页。
    ///
    /// 要求目标范围逐页都未驻留（存在任何已驻留页返回 `EEXIST`，Linux 语义）；
    /// `wp` 对应 `UFFDIO_COPY_MODE_WP`（安装为只读页）。返回安装字节数。
    pub(crate) fn uffd_copy(
        &self,
        dst: usize,
        src: usize,
        len: usize,
        wp: bool,
    ) -> Result<u64, Errno> {
        let page_size = page_size();
        if dst % page_size != 0 || src % page_size != 0 || len == 0 || len % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        let end = dst.checked_add(len).ok_or(Errno::EINVAL)?;
        {
            let pages = self.pages.lock();
            let mut va = dst;
            while va < end {
                if pages.contains_key(va) {
                    return Err(Errno::EEXIST);
                }
                va += page_size;
            }
        }
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;
        let mut va = dst;
        while va < end {
            let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
            let buf =
                unsafe { core::slice::from_raw_parts_mut(virt_fn(paddr) as *mut u8, page_size) };
            if crate::mm::copy_from_user(src + (va - dst), buf).is_err() {
                free_user_page(paddr);
                return Err(Errno::EFAULT);
            }
            let page = ResidentPage::new_anon(paddr);
            self.uffd_install_page(va, page, wp)?;
            va += page_size;
        }
        Ok(len as u64)
    }

    /// `UFFDIO_ZEROPAGE`：在目标地址空间安装零页。返回安装字节数。
    pub(crate) fn uffd_zeropage(&self, start: usize, len: usize) -> Result<u64, Errno> {
        let page_size = page_size();
        if start % page_size != 0 || len == 0 || len % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        let end = start.checked_add(len).ok_or(Errno::EINVAL)?;
        {
            let pages = self.pages.lock();
            let mut va = start;
            while va < end {
                if pages.contains_key(va) {
                    return Err(Errno::EEXIST);
                }
                va += page_size;
            }
        }
        let mut va = start;
        while va < end {
            let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
            let page = ResidentPage::new_anon(paddr);
            self.uffd_install_page(va, page, false)?;
            va += page_size;
        }
        Ok(len as u64)
    }

    /// `UFFDIO_CONTINUE`：为 MINOR 模式缺页安装"页缓存中已存在"的页。
    ///
    /// 与 Linux 的差异：Linux 的 MINOR 缺页针对 shmem/file 后端（页已在页缓存、
    /// 只差映射）；本内核仅支持私有匿名后端，没有共享页缓存页可 continue，因此
    /// 退化为对非驻留页安装零页（等价 ZEROPAGE），并据此注明 shmem/file 后端缺失。
    pub(crate) fn uffd_continue(&self, start: usize, len: usize) -> Result<u64, Errno> {
        let page_size = page_size();
        if start % page_size != 0 || len == 0 || len % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        let end = start.checked_add(len).ok_or(Errno::EINVAL)?;
        {
            let pages = self.pages.lock();
            let mut va = start;
            while va < end {
                if pages.contains_key(va) {
                    return Err(Errno::EEXIST);
                }
                va += page_size;
            }
        }
        let mut va = start;
        while va < end {
            let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
            let page = ResidentPage::new_anon(paddr);
            self.uffd_install_page(va, page, false)?;
            va += page_size;
        }
        Ok(len as u64)
    }

    /// `UFFDIO_WRITEPROTECT`：设置/清除范围内驻留页的写保护。
    ///
    /// 要求范围命中登记了 WP 模式且属于 `state` 的区域（否则 `EINVAL`）。
    /// 清除写保护时直接把页置为可写并唤醒等待者——这是 userfaultfd WP 协议
    /// 的语义（页面是否做 COW 由用户态自行管理）。
    pub(crate) fn uffd_writeprotect(
        &self,
        start: usize,
        len: usize,
        wp: bool,
        state: &Arc<UffdState>,
    ) -> Result<(), Errno> {
        let page_size = page_size();
        if start % page_size != 0 || len == 0 || len % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        let end = start.checked_add(len).ok_or(Errno::EINVAL)?;
        {
            // Linux 语义：范围必须**整体**落在本状态对象的 WP 登记区域内。
            let regions = self.uffd_regions.lock();
            let mut cursor = start;
            for region in regions.iter() {
                if !Arc::ptr_eq(&region.state, state)
                    || region.mode & UFFDIO_REGISTER_MODE_WP == 0
                    || region.range.start > cursor
                {
                    continue;
                }
                cursor = cursor.max(region.range.end);
                if cursor >= end {
                    break;
                }
            }
            if cursor < end {
                return Err(Errno::EINVAL);
            }
        }
        let mut set = self.vmas.lock();
        let mut pages = self.pages.lock();
        let mut batch: Option<(usize, usize, VmFlags)> = None;
        let mut protect_error = None;
        pages.for_each_range_mut(start..end, |va, mapping| {
            if protect_error.is_some() {
                return;
            }
            let Some(area) = set.find(va) else {
                return;
            };
            let new_access = if wp {
                PageAccess::ReadOnly
            } else {
                PageAccess::Writable
            };
            if new_access == mapping.access {
                return;
            }
            mapping.access = new_access;
            let pte_flags = pte_flags_for(area.flags, new_access);
            if let Some((batch_start, batch_end, batch_flags)) = batch {
                if va == batch_end && batch_flags == pte_flags {
                    batch = Some((batch_start, batch_end + page_size, batch_flags));
                    return;
                }
                if let Err(error) =
                    self.protect_pages_no_flush(batch_start, batch_end - batch_start, batch_flags)
                {
                    protect_error = Some(error);
                    return;
                }
            }
            batch = Some((va, va + page_size, pte_flags));
        });
        if let Some(error) = protect_error {
            return Err(error);
        }
        if let Some((batch_start, batch_end, batch_flags)) = batch {
            self.protect_pages_no_flush(batch_start, batch_end - batch_start, batch_flags)?;
        }
        drop(pages);
        drop(set);
        self.invalidate_user_range(start, end - start);
        Ok(())
    }

    /// 把用户态准备好的匿名页安装到 `va`（UFFDIO_COPY/ZEROPAGE 的公共步骤）。
    ///
    /// 要求 `va` 属于私有匿名 VMA 且页未驻留；成功后发布新映射（当前 CPU
    /// 收敛即可，其它 CPU 会在缺页重试路径自行收敛）。
    fn uffd_install_page(&self, va: usize, page: Arc<ResidentPage>, wp: bool) -> Result<(), Errno> {
        let (access, flags) = {
            let set = self.vmas.lock();
            let area = set.find(va).ok_or(Errno::EINVAL)?;
            if !matches!(area.backing, VmBacking::Anon { .. }) || area.flags.has(VmFlags::SHARED) {
                return Err(Errno::EINVAL);
            }
            let access = if wp {
                PageAccess::ReadOnly
            } else {
                access_for_new_page(area.flags, &page)
            };
            (access, area.flags)
        };
        let mut pages = self.pages.lock();
        if pages.contains_key(va) {
            return Err(Errno::EEXIST);
        }
        self.map_page_no_flush(va, page.paddr(), pte_flags_for(flags, access))?;
        pages.insert(va, PageMapping { page, access });
        let mapped = pages.len();
        self.mapped_pages.store(mapped, Ordering::Release);
        drop(pages);
        self.publish_new_user_range(va, page_size());
        Ok(())
    }

    /// MISSING 拦截：`page` 命中 MISSING 登记区域且未驻留时，入队事件并挂起
    /// 当前任务，直到页面被用户态安装或登记失效。返回 true 表示缺页已解决。
    fn uffd_missing_intercept(&self, page: usize, kind: FaultKind) -> bool {
        let region = {
            let regions = self.uffd_regions.lock();
            if regions.is_empty() {
                return false;
            }
            let Some(region) = regions.iter().find(|region| {
                region.range.contains(&page) && region.mode & UFFDIO_REGISTER_MODE_MISSING != 0
            }) else {
                return false;
            };
            region.clone()
        };
        if self.pages.lock().contains_key(page) {
            return false;
        }
        // 注册后 VMA 可能被替换；只拦截仍属于私有匿名的区域。
        {
            let set = self.vmas.lock();
            let Some(area) = set.find(page) else {
                return false;
            };
            if !matches!(area.backing, VmBacking::Anon { .. }) || area.flags.has(VmFlags::SHARED) {
                return false;
            }
        }
        let state = Arc::clone(&region.state);
        let flags = if matches!(kind, FaultKind::Store | FaultKind::PermWrite) {
            UFFD_PAGEFAULT_FLAG_WRITE
        } else {
            0
        };
        state.enqueue_fault(flags, page);
        let page_present = || self.pages.lock().contains_key(page);
        let region_alive = || {
            if !state.alive() {
                return false;
            }
            self.uffd_regions
                .lock()
                .iter()
                .any(|region| region.range.contains(&page) && Arc::ptr_eq(&region.state, &state))
        };
        state.wait_fault(|| page_present() || !region_alive());
        page_present()
    }

    /// MINOR 拦截：`page` 命中 MINOR 登记区域且未驻留时，入队 MINOR 事件并挂起，
    /// 由用户态通过 `UFFDIO_CONTINUE`/`UFFDIO_COPY` 解决。
    ///
    /// 与 Linux 的差异：Linux 的 MINOR 缺页表示"页已在页缓存、只差映射"；本内核
    /// 仅支持私有匿名后端，无共享页缓存，因此 MINOR 拦截退化为对非驻留页触发，
    /// 等价 MISSING 但事件带 `UFFD_PAGEFAULT_FLAG_MINOR`。
    fn uffd_minor_intercept(&self, page: usize, kind: FaultKind) -> bool {
        let region = {
            let regions = self.uffd_regions.lock();
            if regions.is_empty() {
                return false;
            }
            let Some(region) = regions.iter().find(|region| {
                region.range.contains(&page) && region.mode & UFFDIO_REGISTER_MODE_MINOR != 0
            }) else {
                return false;
            };
            region.clone()
        };
        if self.pages.lock().contains_key(page) {
            return false;
        }
        {
            let set = self.vmas.lock();
            let Some(area) = set.find(page) else {
                return false;
            };
            if !matches!(area.backing, VmBacking::Anon { .. }) || area.flags.has(VmFlags::SHARED) {
                return false;
            }
        }
        let state = Arc::clone(&region.state);
        let mut flags = UFFD_PAGEFAULT_FLAG_MINOR;
        if matches!(kind, FaultKind::Store | FaultKind::PermWrite) {
            flags |= UFFD_PAGEFAULT_FLAG_WRITE;
        }
        state.enqueue_fault(flags, page);
        let page_present = || self.pages.lock().contains_key(page);
        let region_alive = || {
            if !state.alive() {
                return false;
            }
            self.uffd_regions
                .lock()
                .iter()
                .any(|region| region.range.contains(&page) && Arc::ptr_eq(&region.state, &state))
        };
        state.wait_fault(|| page_present() || !region_alive());
        page_present()
    }

    /// WP 拦截：`page` 命中 WP 登记区域、已驻留但不可写，且本次是写访问时，
    /// 入队 WP 事件并挂起，直到页面被 `UFFDIO_WRITEPROTECT` 解除保护。
    fn uffd_wp_intercept(&self, page: usize, kind: FaultKind) -> bool {
        if !matches!(kind, FaultKind::Store | FaultKind::PermWrite) {
            return false;
        }
        let region = {
            let regions = self.uffd_regions.lock();
            if regions.is_empty() {
                return false;
            }
            let Some(region) = regions.iter().find(|region| {
                region.range.contains(&page) && region.mode & UFFDIO_REGISTER_MODE_WP != 0
            }) else {
                return false;
            };
            region.clone()
        };
        let writable_now = {
            let pages = self.pages.lock();
            let Some(mapping) = pages.get(page) else {
                return false;
            };
            mapping.access.pte_writable()
        };
        if writable_now {
            return false;
        }
        let state = Arc::clone(&region.state);
        state.enqueue_fault(UFFD_PAGEFAULT_FLAG_WRITE | UFFD_PAGEFAULT_FLAG_WP, page);
        let page_writable = || {
            self.pages
                .lock()
                .get(page)
                .is_some_and(|mapping| mapping.access.pte_writable())
        };
        let region_alive = || {
            if !state.alive() {
                return false;
            }
            self.uffd_regions
                .lock()
                .iter()
                .any(|region| region.range.contains(&page) && Arc::ptr_eq(&region.state, &state))
        };
        state.wait_fault(|| page_writable() || !region_alive());
        page_writable()
    }

    pub fn sync_range(&self, range: Range<usize>) -> Result<(), Errno> {
        self.validate_range(&range)?;
        {
            let set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
        }
        let pages: Vec<Arc<ResidentPage>> = {
            let pages = self.pages.lock();
            let mut resident = Vec::new();
            pages.for_each_range(range, |_va, mapping| {
                resident.push(Arc::clone(&mapping.page));
            });
            resident
        };
        for page in pages {
            page.flush_to_backing()?;
        }
        Ok(())
    }

    /// `mlock(2)` 语义：把 `range` 内全部 VMA 标记 `LOCKED` 并记账。
    ///
    /// `populate` 为 true 时先 fault-in 页面（Linux 默认行为；`mlock2` 的
    /// `MLOCK_ONFAULT` 不 populate）。populate 失败返回 `EAGAIN`（Linux 语义），
    /// 且不改变任何锁状态。`RLIMIT_MEMLOCK` 检查由 syscall 层在调用前用
    /// [`Self::would_lock_pages`] 完成，这里只负责状态与记账。
    pub fn mlock_range(&self, range: Range<usize>, populate: bool) -> Result<(), Errno> {
        self.validate_range(&range)?;
        if populate {
            self.prefault_user_range(range.clone(), true)
                .map_err(|_| Errno::EAGAIN)?;
        }
        self.update_locked_range(range, true)
    }

    pub fn munlock_range(&self, range: Range<usize>) -> Result<(), Errno> {
        self.update_locked_range(range, false)
    }

    /// `mlockall(MCL_CURRENT)`：锁定当前全部 VMA 并记账。
    pub fn mlock_all_current(&self, populate: bool) -> Result<(), Errno> {
        let ranges: Vec<Range<usize>> = {
            let set = self.vmas.lock();
            set.iter().map(|area| area.range.clone()).collect()
        };
        if populate {
            for range in &ranges {
                self.prefault_user_range(range.clone(), true)
                    .map_err(|_| Errno::EAGAIN)?;
            }
        }
        let mut set = self.vmas.lock();
        for range in &ranges {
            set.update_flags_range(range, |flags| flags.with(VmFlags::LOCKED));
        }
        // 全部区域已带 LOCKED：锁页数 = 全部 VMA 几何页数。
        let total: usize = set.iter().map(Self::area_page_count).sum();
        self.locked_pages.store(total, Ordering::Release);
        Ok(())
    }

    pub fn set_mlock_future(&self, enabled: bool) {
        self.mlock_future.store(enabled, Ordering::Release);
    }

    pub fn munlock_all(&self) {
        self.mlock_future.store(false, Ordering::Release);
        let mut set = self.vmas.lock();
        let ranges: Vec<Range<usize>> = set.iter().map(|area| area.range.clone()).collect();
        for range in ranges {
            set.update_flags_range(&range, |flags| flags.without(VmFlags::LOCKED));
        }
        self.locked_pages.store(0, Ordering::Release);
    }

    /// 翻转 `range` 内全部 VMA 的 `LOCKED` 位并同步锁页记账。
    ///
    /// 记账口径与 Linux `mm->locked_vm` 一致：带 `LOCKED` 的 VMA 几何页数。
    /// 由于 `update_flags_range` 的拆分可能让部分片段原本就带 `LOCKED`，
    /// 需要先统计旧状态，避免重复计数。
    fn update_locked_range(&self, range: Range<usize>, locked: bool) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let (already_locked, pieces) = {
            let mut set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
            let already_locked: usize = set
                .iter_overlap(&range)
                .filter(|area| area.flags.has(VmFlags::LOCKED))
                .map(|area| {
                    let start = area.range.start.max(range.start);
                    let end = area.range.end.min(range.end);
                    (end - start) / page_size()
                })
                .sum();
            let pieces = set.update_flags_range(&range, |flags| {
                if locked {
                    flags.with(VmFlags::LOCKED)
                } else {
                    flags.without(VmFlags::LOCKED)
                }
            });
            (already_locked, pieces)
        };
        let after_locked: usize = pieces
            .iter()
            .filter(|(_, flags)| flags.has(VmFlags::LOCKED))
            .map(|(piece_range, _)| (piece_range.end - piece_range.start) / page_size())
            .sum();
        if locked {
            let delta = after_locked.saturating_sub(already_locked);
            self.locked_pages.fetch_add(delta, Ordering::Relaxed);
            memstat::locked_pages_delta(delta as i64);
        } else {
            self.locked_pages
                .fetch_sub(already_locked, Ordering::Relaxed);
            memstat::locked_pages_delta(-(already_locked as i64));
        }
        Ok(())
    }

    /// fork：克隆 VMA 元数据，已驻留的页按 private-COW / shared 语义重建页表。
    ///
    /// `DONTFORK` VMA 不进子进程（`fork_clone_metadata` 已过滤）；`WIPEONFORK`
    /// VMA 保留元数据但子进程不复制页，首次缺页得到零页。
    #[kernel_symbols::export(name = "general.mm.VmSpace.fork", contract = "kernel.mm.address-space@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn fork(&self) -> Self {
        let ops = user_pgd_ops().expect("[mm] user_pgd_ops not registered");
        // 先换入全部换出页,保证子进程看到父进程完整内容(见 swap_in_all 注释)。
        self.swap_in_all();
        let new_pgd = (ops.new_pgd_for_user)();
        let mut parent_set = self.vmas.lock();
        let cloned_set = parent_set.fork_clone_metadata();
        let cloned_file_backings = Self::collect_file_backings(cloned_set.iter());
        let mut child_pages = RadixPageMap::new(page_size());
        let mut child_maps = Vec::new();

        {
            let mut parent_pages = self.pages.lock();
            parent_pages.for_each_mut(|va, mapping| {
                let Some(area) = cloned_set.find(va) else {
                    return;
                };
                if area.flags.has(VmFlags::WIPEONFORK) {
                    // WIPEONFORK：子进程不继承页，缺页时分配零页。
                    return;
                }
                let old_access = mapping.access;
                mapping.access = access_after_fork(area.flags, &mapping.page);
                if old_access != mapping.access {
                    self.protect_page_no_flush(va, pte_flags_for(area.flags, mapping.access))
                        .expect("[mm] fork parent protect failed");
                }
                let child_mapping = mapping.clone();
                child_maps.push(ForkChildMap {
                    vaddr: va,
                    paddr: child_mapping.page.paddr(),
                    flags: pte_flags_for(area.flags, child_mapping.access).with(VmFlags::USER),
                });
                child_pages.insert(va, child_mapping);
            });
        }
        drop(parent_set);
        if !child_maps.is_empty() {
            self.flush_full_user_tlb();
        }

        map_fork_child_batches(&child_maps, page_size(), |vaddr, paddrs, flags| unsafe {
            (ops.map_pages)(new_pgd, vaddr, paddrs, flags)
        })
        .expect("[mm] fork child map failed");

        Self::notify_files_mapped(cloned_file_backings);

        let mapped_pages = child_pages.len();
        // overcommit 记账：子进程复制父进程承诺，全局聚合再计一份。
        let inherited_committed = self.committed_pages.load(Ordering::Acquire);
        memstat::commit_pages(inherited_committed as i64);
        let inherited_locked = self.locked_pages.load(Ordering::Acquire);
        VM_SPACE_CREATED.fetch_add(1, Ordering::Relaxed);
        VM_SPACE_LIVE.fetch_add(1, Ordering::Relaxed);
        Self {
            vmas: Spinlock::new(cloned_set),
            pages: Spinlock::new(child_pages),
            pgd: new_pgd,
            brk_start: AtomicUsize::new(self.brk_start.load(Ordering::Relaxed)),
            brk_current: AtomicUsize::new(self.current_brk()),
            mmap_next: AtomicUsize::new(self.mmap_next.load(Ordering::Acquire)),
            mlock_future: AtomicBool::new(self.mlock_future.load(Ordering::Acquire)),
            committed_pages: AtomicUsize::new(inherited_committed),
            locked_pages: AtomicUsize::new(inherited_locked),
            // fork 继承内存策略（Linux 语义：mempolicy 随 mm 复制）。
            mempolicy: Spinlock::new(self.mempolicy.lock().clone()),
            // fork 不继承 userfaultfd 登记（Linux 语义）。
            uffd_regions: Spinlock::new(Vec::new()),
            // 父进程已在 swap_in_all 中完全换入,子进程从零开始建立换出/FREE/COLD 状态。
            swapped: Spinlock::new(RadixPageMap::new(page_size())),
            freeable: Spinlock::new(RadixPageMap::new(page_size())),
            cold: Spinlock::new(RadixPageMap::new(page_size())),
            // fork 创建独立 mm，按 Linux 语义不继承 expedited 注册状态。
            membarrier_registration: AtomicUsize::new(0),
            mapped_pages: AtomicUsize::new(mapped_pages),
            #[cfg(feature = "performance-profile")]
            profile_identity: VM_SPACE_PROFILE_ID_NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// 切到本地址空间（`schedule_once` 调；写 PGDL 并 flush TLB）。
    #[kernel_symbols::export(name = "general.mm.VmSpace.activate", contract = "kernel.mm.address-space@1", version = 1, capabilities = kernel_symbols::capability::MM_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn activate(&self) {
        if let Some(ops) = user_pgd_ops() {
            unsafe { (ops.activate)(self.pgd) };
        }
    }

    /// page-fault 分派进来的入口。按 VMA backing / 权限决定该做什么。
    pub fn handle_fault(&self, addr: usize, kind: FaultKind) -> FaultOutcome {
        self.handle_fault_inner(
            addr,
            kind,
            true,
            true,
            true,
            #[cfg(feature = "performance-profile")]
            false,
        )
    }

    /// 仅供真实用户态硬件 page-fault 分派使用；软件 prefault 不进入该统计。
    #[cfg(feature = "performance-profile")]
    pub fn handle_user_hardware_fault(&self, addr: usize, kind: FaultKind) -> FaultOutcome {
        self.handle_fault_inner(addr, kind, true, true, true, true)
    }

    /// 预解析用户页访问，但不把已驻留页当作硬件缓存了无效 translation。
    fn ensure_page_access(&self, addr: usize, kind: FaultKind) -> FaultOutcome {
        self.handle_fault_inner(
            addr,
            kind,
            false,
            false,
            false,
            #[cfg(feature = "performance-profile")]
            false,
        )
    }

    fn handle_fault_inner(
        &self,
        addr: usize,
        kind: FaultKind,
        publish_unchanged_mapping: bool,
        allow_fault_around: bool,
        profile_phases: bool,
        #[cfg(feature = "performance-profile")] profile_hardware_fault: bool,
    ) -> FaultOutcome {
        if user_pgd_ops().is_none() {
            return FaultOutcome::Kernel(KernelFaultReason::NotInitialized);
        }
        let page = page_base(addr);
        // userfaultfd 拦截：MISSING 区域的缺页与 WP 区域的写保护缺页先入队
        // 事件并挂起本任务，由用户态解决后返回 Fixed 让硬件重试。
        if self.uffd_missing_intercept(page, kind) {
            return FaultOutcome::Fixed;
        }
        if self.uffd_minor_intercept(page, kind) {
            return FaultOutcome::Fixed;
        }
        if self.uffd_wp_intercept(page, kind) {
            return FaultOutcome::Fixed;
        }
        #[cfg(feature = "performance-profile")]
        let vma_profile =
            profile_phases.then(|| profiling::scope(profiling::Event::PageFaultVmaLookup));
        let set = self.vmas.lock();
        let area = set.find(page);
        #[cfg(feature = "performance-profile")]
        drop(vma_profile);
        let Some(area) = area else {
            drop(set);
            let mut set = self.vmas.lock();
            let Some((_added, flags)) = set.grow_down_to(page, stack_growth_limit()) else {
                return FaultOutcome::Segv;
            };
            let grown_area = set
                .find(page)
                .expect("[mm] grow_down_to 成功后必须覆盖目标页");
            let backing = grown_area.backing.clone();
            let area_range = grown_area.range.clone();
            drop(set);
            #[cfg(feature = "performance-profile")]
            if profile_hardware_fault {
                record_hardware_user_fault(
                    HardwareFaultBacking::from_vma(&backing, flags),
                    HardwareFaultAccess::from_kind(kind),
                    false,
                );
            }
            return match self.commit_fault_page(
                page,
                area_range,
                backing,
                flags,
                kind,
                profile_phases,
            ) {
                FaultAroundCommit::Done(outcome) => outcome,
                FaultAroundCommit::Retry => self.handle_fault_inner(
                    addr,
                    kind,
                    publish_unchanged_mapping,
                    false,
                    profile_phases,
                    #[cfg(feature = "performance-profile")]
                    false,
                ),
            };
        };
        #[cfg(feature = "performance-profile")]
        let hardware_fault_backing = profile_hardware_fault
            .then(|| HardwareFaultBacking::from_vma(&area.backing, area.flags));
        #[cfg(feature = "performance-profile")]
        let hardware_fault_access = HardwareFaultAccess::from_kind(kind);
        if !permits(area.flags, kind) {
            #[cfg(feature = "performance-profile")]
            if let Some(backing) = hardware_fault_backing {
                let resident = self.pages.lock().contains_key(page);
                record_hardware_user_fault(backing, hardware_fault_access, resident);
            }
            return FaultOutcome::Segv;
        }
        let flags = area.flags;
        #[cfg(feature = "performance-profile")]
        let resident_profile =
            profile_phases.then(|| profiling::scope(profiling::Event::PageFaultResident));
        #[cfg(feature = "performance-profile")]
        let page_lookup_profile =
            profile_phases.then(|| profiling::scope(profiling::Event::PageFaultPageLookup));
        let mut pages = self.pages.lock();
        // resident ledger 与叶 PTE 在同一组 VMA/pages 锁内提交和撤销，正常路径不需要
        // 再为每次硬件缺页遍历一次页表。调试构建保留一致性检查以捕获实现错误。
        #[cfg(debug_assertions)]
        {
            let pte_present = user_pgd_ops().is_some_and(|ops| {
                (unsafe { (ops.count_mapped)(self.pgd, page, page_size()) }) != 0
            });
            if pte_present != pages.contains_key(page) {
                return FaultOutcome::Kernel(KernelFaultReason::UncaughtKernelAccess);
            }
        }
        let mapping = pages.get_mut(page);
        #[cfg(feature = "performance-profile")]
        drop(page_lookup_profile);
        if let Some(mapping) = mapping {
            #[cfg(feature = "performance-profile")]
            if let Some(backing) = hardware_fault_backing {
                record_hardware_user_fault(backing, hardware_fault_access, true);
            }
            let update = self.handle_resident_fault_locked(page, flags, kind, mapping);
            drop(pages);
            drop(set);
            return self.finish_resident_fault(page, update, publish_unchanged_mapping);
        }
        drop(pages);
        #[cfg(feature = "performance-profile")]
        drop(resident_profile);
        let backing = area.backing.clone();
        let area_range = area.range.clone();
        drop(set);
        // swap 换入：私有匿名页若先前被 MADV_PAGEOUT 换出，先按槽位读回内容，
        // 而不是重新分配零页。
        if matches!(&backing, VmBacking::Anon { .. }) {
            let slot = self.swapped.lock().remove(page);
            if let Some(slot) = slot {
                return match self.commit_swap_in(page, slot) {
                    FaultAroundCommit::Done(outcome) => outcome,
                    FaultAroundCommit::Retry => self.handle_fault_inner(
                        addr,
                        kind,
                        publish_unchanged_mapping,
                        false,
                        profile_phases,
                        #[cfg(feature = "performance-profile")]
                        false,
                    ),
                };
            }
        }
        #[cfg(feature = "performance-profile")]
        if let Some(backing) = hardware_fault_backing {
            record_hardware_user_fault(backing, hardware_fault_access, false);
        }
        #[cfg(feature = "performance-profile")]
        let _nonresident_profile =
            profile_phases.then(|| profiling::scope(profiling::Event::PageFaultNonresident));

        #[cfg(feature = "performance-profile")]
        if allow_fault_around
            && matches!(kind, FaultKind::Store)
            && matches!(&backing, VmBacking::Anon { .. })
            && flags.contains_all(VmFlags::USER | VmFlags::WRITE | VmFlags::ANON)
            && !flags.has(VmFlags::SHARED)
            && !flags.has(VmFlags::GROWS_DOWN)
        {
            record_anon_store_shadow_fault(self.profile_identity, page, area_range.end);
        }

        if allow_fault_around {
            if let Some(plan) =
                AnonStoreFaultAround::new(page, area_range.clone(), flags, &backing, kind)
            {
                let mut prepared = PreparedAnonPages::new();
                if let Err(err) = plan.prepare_into(&mut prepared) {
                    return fault_from_errno(err);
                }
                match self.commit_anon_store_fault_around(&plan, &mut prepared) {
                    FaultAroundCommit::Done(outcome) => return outcome,
                    FaultAroundCommit::Retry => {
                        return self.handle_fault_inner(
                            addr,
                            kind,
                            publish_unchanged_mapping,
                            false,
                            profile_phases,
                            #[cfg(feature = "performance-profile")]
                            false,
                        );
                    }
                }
            }
            if let Some(plan) =
                PrivateFileFaultAround::new(page, area_range.clone(), flags, &backing, kind)
            {
                let mut prepared = PreparedFilePages::new();
                if let Err(err) = plan.prepare_into(&mut prepared, profile_phases) {
                    return fault_from_errno(err);
                }
                match self.commit_private_file_fault_around(&plan, &mut prepared, profile_phases) {
                    FaultAroundCommit::Done(outcome) => return outcome,
                    FaultAroundCommit::Retry => {
                        // VMA 在锁外 I/O 期间发生变化；只重试普通单页路径，避免在
                        // 高频 mmap/mprotect 竞争下反复执行投机读取。
                        return self.handle_fault_inner(
                            addr,
                            kind,
                            publish_unchanged_mapping,
                            false,
                            profile_phases,
                            #[cfg(feature = "performance-profile")]
                            false,
                        );
                    }
                }
            }
        }

        match self.commit_fault_page(page, area_range, backing, flags, kind, profile_phases) {
            FaultAroundCommit::Done(outcome) => outcome,
            FaultAroundCommit::Retry => self.handle_fault_inner(
                addr,
                kind,
                publish_unchanged_mapping,
                false,
                profile_phases,
                #[cfg(feature = "performance-profile")]
                false,
            ),
        }
    }

    /// 取得从用户地址读取的一页内连续窗口。
    ///
    /// 这个接口面向大块 I/O / bulk copy 热路径：先通过 VmSpace 完成权限检查、
    /// lazy fault-in 和 COW，再把 resident page 的物理页转成内核直映 slice。
    /// 因此闭包内访问的是内核地址，不需要走 arch uaccess 的逐元素 fixup。
    ///
    /// # Safety
    ///
    /// 调用方必须保证闭包不会保存传入 slice；用户映射可能被其它线程并发改变，
    /// 本函数只通过 resident page 的 Arc 保证底层物理页在闭包期间不被释放。
    pub unsafe fn with_user_read_slice<R>(
        &self,
        user: usize,
        max_len: usize,
        f: impl FnOnce(&[u8]) -> R,
    ) -> Result<R, Errno> {
        let (_page, kva, len) = self.user_page_window(user, max_len, FaultKind::Load)?;
        let slice = unsafe { core::slice::from_raw_parts(kva as *const u8, len) };
        Ok(f(slice))
    }

    /// 把可能跨越多个页面的用户区完整复制到内核缓冲区。
    ///
    /// 与 [`Self::with_user_read_slice`] 不同，本接口保证成功时填满整个 `output`；
    /// 调用方不需要理解单页窗口边界，也不会因跨页结构产生长度不等的 slice。
    pub fn copy_user_bytes_in(&self, user: usize, output: &mut [u8]) -> Result<(), Errno> {
        user.checked_add(output.len()).ok_or(Errno::EFAULT)?;
        let mut copied = 0usize;
        while copied < output.len() {
            let address = user.checked_add(copied).ok_or(Errno::EFAULT)?;
            let count = unsafe {
                self.with_user_read_slice(address, output.len() - copied, |window| {
                    output[copied..copied + window.len()].copy_from_slice(window);
                    window.len()
                })
            }?;
            if count == 0 {
                return Err(Errno::EFAULT);
            }
            copied += count;
        }
        Ok(())
    }

    /// 固定覆盖给定范围的只读用户页，并在返回前完成全部权限检查和 fault-in。
    ///
    /// 固定窗口只保留到返回值析构为止，适合把用户复制与其它子系统的自旋锁分开。
    pub fn pin_user_read_windows<const N: usize>(
        &self,
        user: usize,
        len: usize,
    ) -> Result<UserReadWindows<N>, Errno> {
        let mut windows = UserReadWindows::empty();
        self.pin_user_read_windows_into(user, len, &mut windows)?;
        Ok(windows)
    }

    /// 将只读用户页窗口直接写入调用方提供的缓冲区。
    pub fn pin_user_read_windows_into<const N: usize>(
        &self,
        user: usize,
        len: usize,
        output: &mut UserReadWindows<N>,
    ) -> Result<(), Errno> {
        output.clear();
        if len == 0 {
            return Ok(());
        }
        let mut resident = ResidentUserWindows::<N>::empty();
        if self.try_pin_resident_windows_into(user, len, FaultKind::Load, &mut resident)? {
            for index in 0..resident.count {
                let window = resident.windows[index]
                    .take()
                    .expect("驻留用户读窗口计数必须对应有效槽位");
                output.windows[index] = Some(UserReadWindow {
                    _page: window.page,
                    address: window.address,
                    len: window.len,
                });
            }
            output.count = resident.count;
            output.len = resident.len;
            return Ok(());
        }
        user.checked_add(len).ok_or(Errno::EFAULT)?;
        let mut copied = 0usize;
        let mut count = 0usize;
        while copied < len {
            if count == N {
                return Err(Errno::EFAULT);
            }
            let address = user.checked_add(copied).ok_or(Errno::EFAULT)?;
            let (page, kernel_address, window_len) =
                self.user_page_window(address, len - copied, FaultKind::Load)?;
            if window_len == 0 {
                return Err(Errno::EFAULT);
            }
            output.windows[count] = Some(UserReadWindow {
                _page: page,
                address: kernel_address,
                len: window_len,
            });
            copied += window_len;
            count += 1;
            output.count = count;
        }
        output.count = count;
        output.len = len;
        Ok(())
    }

    /// 固定覆盖给定范围的可写用户页，并在返回前完成权限检查、COW 和 fault-in。
    ///
    /// 调用方应只在确认数据已经可读后固定窗口，避免阻塞等待期间长期保留用户页。
    pub fn pin_user_write_windows<const N: usize>(
        &self,
        user: usize,
        len: usize,
    ) -> Result<UserWriteWindows<N>, Errno> {
        let mut windows = UserWriteWindows::empty();
        self.pin_user_write_windows_into(user, len, &mut windows)?;
        Ok(windows)
    }

    /// 将可写用户页窗口直接写入调用方提供的缓冲区。
    pub fn pin_user_write_windows_into<const N: usize>(
        &self,
        user: usize,
        len: usize,
        output: &mut UserWriteWindows<N>,
    ) -> Result<(), Errno> {
        output.clear();
        if len == 0 {
            return Ok(());
        }
        let mut resident = ResidentUserWindows::<N>::empty();
        if self.try_pin_resident_windows_into(user, len, FaultKind::Store, &mut resident)? {
            for index in 0..resident.count {
                let window = resident.windows[index]
                    .take()
                    .expect("驻留用户写窗口计数必须对应有效槽位");
                output.windows[index] = Some(UserWriteWindow {
                    page: window.page,
                    address: window.address,
                    len: window.len,
                });
            }
            output.count = resident.count;
            output.len = resident.len;
            return Ok(());
        }
        user.checked_add(len).ok_or(Errno::EFAULT)?;
        let mut copied = 0usize;
        let mut count = 0usize;
        while copied < len {
            if count == N {
                return Err(Errno::EFAULT);
            }
            let address = user.checked_add(copied).ok_or(Errno::EFAULT)?;
            let (page, kernel_address, window_len) =
                self.user_page_window(address, len - copied, FaultKind::Store)?;
            if window_len == 0 {
                return Err(Errno::EFAULT);
            }
            output.windows[count] = Some(UserWriteWindow {
                page,
                address: kernel_address,
                len: window_len,
            });
            copied += window_len;
            count += 1;
            output.count = count;
        }
        output.count = count;
        output.len = len;
        Ok(())
    }

    /// 为用户态原子 u32 访问预先解析 lazy fault 和可选的 COW。
    ///
    /// 该操作可能分配和修改页表，只能在获取 futex 等子系统自旋锁之前调用。
    pub fn prefault_user_u32(&self, user: usize, write: bool) -> Result<(), Errno> {
        self.user_u32_location(user)?;
        let kind = if write {
            FaultKind::Store
        } else {
            FaultKind::Load
        };
        match self.ensure_page_access(user, kind) {
            FaultOutcome::Fixed => self.with_user_atomic_u32(user, write, |_| ((), false)),
            FaultOutcome::Segv | FaultOutcome::OutOfMemory | FaultOutcome::Kernel(_) => {
                Err(Errno::EFAULT)
            }
        }
    }

    /// 预先解析一段已注册用户 VMA 覆盖的全部页。
    ///
    /// 本函数不创建或扩展 VMA，只为区间相交页完成匿名分配、文件读入或 COW；
    /// 适合在首次进入用户态前保证内核即将直接写入的少量页面已经驻留。
    pub fn prefault_user_range(&self, range: Range<usize>, write: bool) -> Result<(), Errno> {
        if range.start >= range.end {
            return Ok(());
        }
        let page_size = page_size();
        let end = align_up(range.end, page_size).ok_or(Errno::EFAULT)?;
        let kind = if write {
            FaultKind::Store
        } else {
            FaultKind::Load
        };
        let mut page = page_base(range.start);
        while page < end {
            match self.ensure_page_access(page, kind) {
                FaultOutcome::Fixed => {}
                FaultOutcome::Segv | FaultOutcome::OutOfMemory | FaultOutcome::Kernel(_) => {
                    return Err(Errno::EFAULT);
                }
            }
            page = page.checked_add(page_size).ok_or(Errno::EFAULT)?;
        }
        Ok(())
    }

    /// 从已经常驻的用户页原子读取一个 u32，不触发缺页或分配。
    ///
    /// 调用方应先在普通上下文中完成 fault-in。该接口只用于已经持有其它子系统
    /// 自旋锁、不能再进入缺页路径的窄临界区；映射或权限已经变化时返回 EFAULT。
    pub fn read_user_u32_nofault(&self, user: usize) -> Result<u32, Errno> {
        self.with_user_atomic_u32(user, false, |word| (word.load(Ordering::Acquire), false))
    }

    /// 对已经常驻且可写的用户 u32 执行原子 compare-exchange。
    ///
    /// 返回本次观察到的旧值；仅当它等于 `current` 时写入 `new`。该接口不会
    /// 触发缺页或 COW，调用方必须先执行 [`Self::prefault_user_u32`]。
    pub fn compare_exchange_user_u32_nofault(
        &self,
        user: usize,
        current: u32,
        new: u32,
    ) -> Result<u32, Errno> {
        self.with_user_atomic_u32(user, true, |word| {
            match word.compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(previous) => (previous, true),
                Err(observed) => (observed, false),
            }
        })
    }

    /// 向已经常驻且可写的用户 u32 执行 release store。
    ///
    /// 该接口供共享队列等内核生产者发布 head/tail；调用方必须先完成 fault-in，
    /// 映射或权限已经变化时返回 EFAULT。
    pub fn store_user_u32_nofault(&self, user: usize, value: u32) -> Result<(), Errno> {
        self.with_user_atomic_u32(user, true, |word| {
            word.store(value, Ordering::Release);
            ((), true)
        })
    }

    fn user_u32_location(&self, user: usize) -> Result<(usize, usize), Errno> {
        if user % core::mem::align_of::<u32>() != 0 {
            return Err(Errno::EINVAL);
        }
        let end = user
            .checked_add(core::mem::size_of::<u32>())
            .ok_or(Errno::EFAULT)?;
        let page_va = page_base(user);
        if end > page_va.checked_add(page_size()).ok_or(Errno::EFAULT)? {
            return Err(Errno::EFAULT);
        }
        Ok((page_va, end))
    }

    fn with_user_atomic_u32<R>(
        &self,
        user: usize,
        write: bool,
        operation: impl FnOnce(&AtomicU32) -> (R, bool),
    ) -> Result<R, Errno> {
        let (page_va, end) = self.user_u32_location(user)?;
        // 同时持有 VMA 和 resident page 锁，确保权限检查与物理页选择来自同一份
        // 映射快照。Arc 由 pages 表持有，因此原子操作期间物理页不会被回收。
        let set = self.vmas.lock();
        let area = set.find(user).ok_or(Errno::EFAULT)?;
        let required = if write {
            VmFlags::USER | VmFlags::READ | VmFlags::WRITE
        } else {
            VmFlags::USER | VmFlags::READ
        };
        if end > area.range.end || !area.flags.contains_all(required) {
            return Err(Errno::EFAULT);
        }
        let pages = self.pages.lock();
        let mapping = pages.get(page_va).ok_or(Errno::EFAULT)?;
        if write && !mapping.access.pte_writable() {
            return Err(Errno::EFAULT);
        }
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EFAULT)?;
        let pointer = (virt_fn(mapping.page.paddr()) + (user - page_va)) as *const AtomicU32;
        // Safety: futex 地址已经按 u32 对齐，四字节不会跨页；VMA/pages 锁保证当前
        // resident mapping 不被替换，底层页由 mapping 的 Arc 保活。
        let (result, changed) = operation(unsafe { &*pointer });
        if changed {
            mapping.page.mark_dirty();
        }
        Ok(result)
    }

    /// 取得写入用户地址的一页内连续窗口。
    ///
    /// Store fault 会在返回前解析 COW / shared dirty 状态。闭包返回后再次标脏，
    /// 覆盖 VFS 在闭包内写入用户页但没有显式 fault 的场景。
    ///
    /// # Safety
    ///
    /// 同 [`Self::with_user_read_slice`]。调用方还必须保证闭包不会制造跨线程可见
    /// 的长期 `&mut [u8]` 别名。
    pub unsafe fn with_user_write_slice<R>(
        &self,
        user: usize,
        max_len: usize,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, Errno> {
        let (page, kva, len) = self.user_page_window(user, max_len, FaultKind::Store)?;
        let slice = unsafe { core::slice::from_raw_parts_mut(kva as *mut u8, len) };
        let result = f(slice);
        page.mark_dirty();
        Ok(result)
    }

    /// 把内核缓冲区完整复制到可能跨越多个页面的用户区。
    ///
    /// 成功返回前会访问并标脏覆盖范围内的每一页；任一页不可写时返回 `EFAULT`。
    pub fn copy_user_bytes_out(&self, user: usize, input: &[u8]) -> Result<(), Errno> {
        user.checked_add(input.len()).ok_or(Errno::EFAULT)?;
        let mut copied = 0usize;
        while copied < input.len() {
            let address = user.checked_add(copied).ok_or(Errno::EFAULT)?;
            let count = unsafe {
                self.with_user_write_slice(address, input.len() - copied, |window| {
                    window.copy_from_slice(&input[copied..copied + window.len()]);
                    window.len()
                })
            }?;
            if count == 0 {
                return Err(Errno::EFAULT);
            }
            copied += count;
        }
        Ok(())
    }

    /// 立即为一个 ELF 段分配并填充物理页。
    pub fn commit_segment(
        &self,
        vaddr: usize,
        memsz: usize,
        file_size: usize,
        data: &[u8],
        flags: VmFlags,
    ) -> Result<(), Errno> {
        if memsz == 0 {
            return Ok(());
        }
        if file_size > memsz || data.len() != file_size {
            return Err(Errno::EINVAL);
        }
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;

        let page_size = page_size();
        let start = page_base(vaddr);
        let end_unaligned = vaddr.checked_add(memsz).ok_or(Errno::EINVAL)?;
        let end = align_up(end_unaligned, page_size).ok_or(Errno::EINVAL)?;
        let area_flags = flags.with(VmFlags::USER).with(VmFlags::ANON);

        self.map_anon(start..end, area_flags)?;

        let file_end_vaddr = vaddr + file_size;
        let mut pages = self.pages.lock();
        let mut page_va = start;
        while page_va < end {
            let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
            let copy_start_va = page_va.max(vaddr);
            let copy_end_va = (page_va + page_size).min(file_end_vaddr);
            if copy_end_va > copy_start_va {
                let seg_off = copy_start_va - vaddr;
                let len = copy_end_va - copy_start_va;
                let dst_off_in_page = copy_start_va - page_va;
                let kva = virt_fn(paddr) + dst_off_in_page;
                unsafe {
                    core::ptr::copy_nonoverlapping(data.as_ptr().add(seg_off), kva as *mut u8, len);
                }
            }
            let page = ResidentPage::new_anon(paddr);
            let access = access_for_new_page(area_flags, &page);
            self.map_page_no_flush(page_va, page.paddr(), pte_flags_for(area_flags, access))?;
            pages.insert(page_va, PageMapping { page, access });
            page_va += page_size;
        }
        let mapped = pages.len();
        self.mapped_pages.store(mapped, Ordering::Release);
        drop(pages);
        self.publish_new_user_range(start, end - start);
        Ok(())
    }

    /// 注册 ELF 文件段，并只立即填充不能直接映射文件的首尾碎片页。
    ///
    /// 完整落在 `filesz` 内的页保留为 file-backed VMA，由硬件缺页按需读取；
    /// BSS 完整页保留为匿名 VMA。这样短命编译器进程不会在 `execve` 时读取从未
    /// 执行或访问的全部代码/数据，同时碎片页仍严格保证段前空洞与 BSS 尾部清零。
    pub fn commit_file_segment(
        &self,
        vaddr: usize,
        memsz: usize,
        file_offset: u64,
        file_size: usize,
        file: Arc<dyn FileLike>,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        if memsz == 0 {
            return Ok(());
        }
        let page_size = page_size();
        let plan = plan_file_segment(vaddr, memsz, file_offset, file_size, page_size)?;
        let area_flags = flags.with(VmFlags::USER);

        if plan.lazy_file.start >= plan.lazy_file.end {
            self.map_anon(plan.mapping.clone(), area_flags)?;
        } else {
            if plan.mapping.start < plan.lazy_file.start {
                self.map_anon(plan.mapping.start..plan.lazy_file.start, area_flags)?;
            }
            self.map_file(
                plan.lazy_file.clone(),
                Arc::clone(&file),
                plan.lazy_file_offset,
                area_flags,
            )?;
            if plan.lazy_file.end < plan.mapping.end {
                self.map_anon(plan.lazy_file.end..plan.mapping.end, area_flags)?;
            }
        }

        for &page_va in plan.fragments() {
            self.commit_file_fragment_page(
                page_va,
                vaddr,
                file_offset,
                file_size,
                file.as_ref(),
                area_flags.with(VmFlags::ANON),
            )?;
        }
        Ok(())
    }

    /// 用文件段替换已经预留的同址 VMA，并保留首尾碎片页与 BSS 的精确清零语义。
    ///
    /// 动态组件先以匿名 VMA 预留完整映像地址，再用本入口把无需重定位的段改为
    /// file-backed。每个子区间都通过 fixed 映射在 VMA 锁内替换，地址不会在事务
    /// 准备期间被其它线程抢占。
    pub fn commit_file_segment_fixed(
        &self,
        vaddr: usize,
        memsz: usize,
        file_offset: u64,
        file_size: usize,
        file: Arc<dyn FileLike>,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        if memsz == 0 {
            return Ok(());
        }
        let page_size = page_size();
        let plan = plan_file_segment(vaddr, memsz, file_offset, file_size, page_size)?;
        let area_flags = flags.with(VmFlags::USER);

        if plan.lazy_file.start >= plan.lazy_file.end {
            self.map_fixed_anon(plan.mapping.clone(), area_flags)?;
        } else {
            if plan.mapping.start < plan.lazy_file.start {
                self.map_fixed_anon(plan.mapping.start..plan.lazy_file.start, area_flags)?;
            }
            self.map_fixed_file(
                plan.lazy_file.clone(),
                Arc::clone(&file),
                plan.lazy_file_offset,
                area_flags,
            )?;
            if plan.lazy_file.end < plan.mapping.end {
                self.map_fixed_anon(plan.lazy_file.end..plan.mapping.end, area_flags)?;
            }
        }

        for &page_va in plan.fragments() {
            self.commit_file_fragment_page(
                page_va,
                vaddr,
                file_offset,
                file_size,
                file.as_ref(),
                area_flags.with(VmFlags::ANON),
            )?;
        }
        Ok(())
    }

    /// 填充 ELF 文件段中不能作为完整文件页延迟映射的首尾页。
    fn commit_file_fragment_page(
        &self,
        page_va: usize,
        vaddr: usize,
        file_offset: u64,
        file_size: usize,
        file: &dyn FileLike,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        let page_size = page_size();
        let file_end_vaddr = vaddr.checked_add(file_size).ok_or(Errno::EINVAL)?;
        let page_end = page_va.checked_add(page_size).ok_or(Errno::EINVAL)?;
        let copy_start_va = page_va.max(vaddr);
        let copy_end_va = page_end.min(file_end_vaddr);
        if copy_end_va <= copy_start_va {
            return Err(Errno::EINVAL);
        }

        let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
        let result = (|| {
            let virt_fn = allocator::KERNEL_ALLOCATOR
                .load_phys_to_virt()
                .ok_or(Errno::EINVAL)?;
            let seg_off = copy_start_va - vaddr;
            let len = copy_end_va - copy_start_va;
            let dst_off_in_page = copy_start_va - page_va;
            let kva = virt_fn(paddr) + dst_off_in_page;
            // Safety: paddr 来自一整页用户物理页分配；dst_off/len 均由该页与
            // 文件字节区间的交集计算，构造的切片不会越过这页。
            let dst = unsafe { core::slice::from_raw_parts_mut(kva as *mut u8, len) };
            let mut done = 0usize;
            while done < len {
                let read_off = file_offset
                    .checked_add(u64::try_from(seg_off + done).map_err(|_| Errno::EINVAL)?)
                    .ok_or(Errno::EINVAL)?;
                let n = file.read_at(read_off, &mut dst[done..])?;
                if n == 0 {
                    return Err(Errno::ENOEXEC);
                }
                done += n;
            }
            Ok(())
        })();
        if let Err(err) = result {
            free_user_page(paddr);
            return Err(err);
        }

        let page = ResidentPage::new_anon(paddr);
        let access = access_for_new_page(flags, &page);
        let mut pages = self.pages.lock();
        if pages.contains_key(page_va) {
            return Err(Errno::EEXIST);
        }
        self.map_page_no_flush(page_va, page.paddr(), pte_flags_for(flags, access))?;
        pages.insert(page_va, PageMapping { page, access });
        let mapped = pages.len();
        self.mapped_pages.store(mapped, Ordering::Release);
        drop(pages);
        self.publish_new_user_range(page_va, page_size);
        Ok(())
    }

    fn validate_range(&self, range: &Range<usize>) -> Result<(), Errno> {
        let page_size = page_size();
        if range.start % page_size != 0 || range.end % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        if range.start >= range.end {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }

    fn user_page_window(
        &self,
        user: usize,
        max_len: usize,
        kind: FaultKind,
    ) -> Result<(Arc<ResidentPage>, usize, usize), Errno> {
        if max_len == 0 || user.checked_add(max_len - 1).is_none() {
            return Err(Errno::EFAULT);
        }
        match self.ensure_page_access(user, kind) {
            FaultOutcome::Fixed => {}
            FaultOutcome::Segv | FaultOutcome::OutOfMemory | FaultOutcome::Kernel(_) => {
                return Err(Errno::EFAULT);
            }
        }

        let page_va = page_base(user);
        let offset = user - page_va;
        let len = max_len.min(page_size() - offset);
        let page = {
            let pages = self.pages.lock();
            pages
                .get(page_va)
                .map(|mapping| Arc::clone(&mapping.page))
                .ok_or(Errno::EFAULT)?
        };
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EFAULT)?;
        let kva = virt_fn(page.paddr()) + offset;
        Ok((page, kva, len))
    }

    /// 在一次 VMA/pages 快照中固定已经常驻且权限就绪的用户页。
    ///
    /// 缺页、栈增长和写时复制都返回 `None`，由调用方进入完整 fault 路径。该
    /// 快路径只合并重复的锁与映射查询，不放宽权限，也不缓存可能失效的页表状态。
    fn try_pin_resident_windows_into<const N: usize>(
        &self,
        user: usize,
        len: usize,
        kind: FaultKind,
        output: &mut ResidentUserWindows<N>,
    ) -> Result<bool, Errno> {
        output.clear();
        let end = user.checked_add(len).ok_or(Errno::EFAULT)?;
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EFAULT)?;
        let set = self.vmas.lock();
        let pages = self.pages.lock();
        let mut copied = 0usize;
        let mut count = 0usize;
        while copied < len {
            if count == N {
                return Err(Errno::EFAULT);
            }
            let address = user.checked_add(copied).ok_or(Errno::EFAULT)?;
            let Some(area) = set.find(address) else {
                output.clear();
                return Ok(false);
            };
            if !permits(area.flags, kind) || address >= end {
                output.clear();
                return Ok(false);
            }
            let page_va = page_base(address);
            let Some(mapping) = pages.get(page_va) else {
                output.clear();
                return Ok(false);
            };
            if is_write_fault(kind) && !mapping.access.pte_writable() {
                output.clear();
                return Ok(false);
            }
            let offset = address - page_va;
            let window_len = (len - copied)
                .min(page_size() - offset)
                .min(area.range.end - address);
            if window_len == 0 {
                output.clear();
                return Ok(false);
            }
            output.windows[count] = Some(ResidentUserWindow {
                page: Arc::clone(&mapping.page),
                address: virt_fn(mapping.page.paddr()) + offset,
                len: window_len,
            });
            copied += window_len;
            count += 1;
            output.count = count;
        }
        output.count = count;
        output.len = len;
        Ok(true)
    }

    /// 提交已在锁外读好的只读私有文件页。
    ///
    /// 重新取得 VMA/pages 锁后先验证快照。并发 fault 若已经安装候选页，只提交
    /// 该页之前的连续新页前缀，剩余候选在解锁后由 Arc 析构回收；因此不会覆盖
    /// 现有 PTE，也能用一次 `publish_new_mapping` 发布完整前缀。
    fn commit_private_file_fault_around(
        &self,
        plan: &PrivateFileFaultAround,
        prepared: &mut PreparedFilePages,
        profile_phases: bool,
    ) -> FaultAroundCommit {
        #[cfg(feature = "performance-profile")]
        let _profile = profile_phases.then(|| profiling::scope(profiling::Event::PageFaultCommit));
        #[cfg(not(feature = "performance-profile"))]
        let _ = profile_phases;
        if prepared.is_empty() {
            #[cfg(feature = "performance-profile")]
            record_fault_around_commit(0, false);
            return FaultAroundCommit::Done(FaultOutcome::Segv);
        }

        let set = self.vmas.lock();
        let Some(area) = set.find(plan.fault_page) else {
            #[cfg(feature = "performance-profile")]
            record_fault_around_vma_retry(prepared.len());
            drop(set);
            prepared.clear();
            return FaultAroundCommit::Retry;
        };
        if !plan.matches_area(area) {
            #[cfg(feature = "performance-profile")]
            record_fault_around_vma_retry(prepared.len());
            drop(set);
            prepared.clear();
            return FaultAroundCommit::Retry;
        }

        let mut pages = self.pages.lock();
        if pages.contains_key(plan.fault_page) {
            #[cfg(feature = "performance-profile")]
            record_fault_around_raced_pages(prepared.len());
            drop(pages);
            drop(set);
            prepared.clear();
            // 另一 CPU 在本次 I/O 期间先发布了 PTE；当前 CPU 仍需收敛导致
            // 本次硬件 fault 的旧无效 translation。
            #[cfg(feature = "performance-profile")]
            record_fault_around_commit(0, true);
            self.publish_new_user_range(plan.fault_page, page_size());
            return FaultAroundCommit::Done(FaultOutcome::Fixed);
        }

        #[cfg(feature = "performance-profile")]
        let prepared_len = prepared.len();
        let prefix_len =
            unmapped_prefix_len(prepared.iter().map(|candidate| candidate.vaddr), |vaddr| {
                pages.contains_key(vaddr)
            });
        #[cfg(feature = "performance-profile")]
        if prefix_len != prepared_len {
            let suffix_start = prepared[prefix_len].vaddr;
            let suffix_end = prepared
                .last()
                .and_then(|candidate| candidate.vaddr.checked_add(page_size()))
                .unwrap_or(suffix_start);
            let duplicate = pages.count_range(suffix_start..suffix_end);
            record_fault_around_collision(duplicate, prepared_len - prefix_len - duplicate);
        }
        debug_assert!(prefix_len <= FILE_FAULT_AROUND_PAGES);
        let mut paddrs = [0usize; FILE_FAULT_AROUND_PAGES];
        for (slot, candidate) in paddrs.iter_mut().zip(prepared.iter()).take(prefix_len) {
            *slot = candidate.page.paddr();
        }
        let (installed, map_error) = self.map_pages_no_flush(
            plan.fault_page,
            &paddrs[..prefix_len],
            pte_flags_for(plan.flags, PageAccess::ReadOnly),
        );
        let mut candidates = prepared.drain(..);
        let replaced = pages.insert_contiguous(
            plan.fault_page,
            candidates
                .by_ref()
                .take(installed)
                .enumerate()
                .map(|(index, candidate)| {
                    debug_assert_eq!(candidate.vaddr, plan.fault_page + index * page_size());
                    PageMapping {
                        page: candidate.page,
                        access: PageAccess::ReadOnly,
                    }
                }),
        );
        debug_assert_eq!(replaced, 0);
        let mapped = pages.len();
        if installed != 0 {
            self.mapped_pages.store(mapped, Ordering::Release);
        }
        drop(pages);
        drop(set);
        // 未采用的投机页可能触发物理页回收，必须在 VMA/pages 锁外析构。
        drop(candidates);
        #[cfg(feature = "performance-profile")]
        if installed != prefix_len {
            record_fault_around_map_failed_pages(prefix_len - installed);
        }
        #[cfg(feature = "performance-profile")]
        record_fault_around_commit(installed, false);

        if installed != 0 {
            // 页表屏障会发布本轮写入的全部投机 PTE；当前 CPU 只有真正触发
            // 硬件异常的 fault_page 必然缓存过旧的无效 translation。邻页若在
            // 其它 CPU 上并发 fault，会在 resident/race 路径各自做本地定向
            // 收敛。这里只失效 fault_page，避免 LoongArch 把多页范围退化为
            // 清空当前 CPU 的全部 TLB（包括内核/global translation）。
            self.publish_new_user_range(plan.fault_page, page_size());
            return FaultAroundCommit::Done(FaultOutcome::Fixed);
        }
        FaultAroundCommit::Done(fault_from_errno(map_error.unwrap_or(Errno::EINVAL)))
    }

    /// 提交在锁外准备的私有匿名零页。
    ///
    /// 锁序保持 `vmas -> pages`。持锁后重验 VMA 快照，并以 resident
    /// map 和真实叶 PTE 的首个冲突同时截断连续前缀，绝不覆盖已有映射。
    fn commit_anon_store_fault_around(
        &self,
        plan: &AnonStoreFaultAround,
        prepared: &mut PreparedAnonPages,
    ) -> FaultAroundCommit {
        if prepared.is_empty() {
            return FaultAroundCommit::Retry;
        }

        let set = self.vmas.lock();
        let Some(area) = set.find(plan.fault_page) else {
            #[cfg(feature = "performance-profile")]
            let discarded = prepared.len();
            drop(set);
            prepared.clear();
            #[cfg(feature = "performance-profile")]
            record_anon_fault_around_discard(
                &anon_fault_around_cpu_counters().vma_retry_pages,
                discarded,
            );
            return FaultAroundCommit::Retry;
        };
        if !plan.matches_area(area) {
            #[cfg(feature = "performance-profile")]
            let discarded = prepared.len();
            drop(set);
            prepared.clear();
            #[cfg(feature = "performance-profile")]
            record_anon_fault_around_discard(
                &anon_fault_around_cpu_counters().vma_retry_pages,
                discarded,
            );
            return FaultAroundCommit::Retry;
        }

        let Some(ops) = user_pgd_ops() else {
            #[cfg(feature = "performance-profile")]
            let discarded = prepared.len();
            drop(set);
            prepared.clear();
            #[cfg(feature = "performance-profile")]
            record_anon_fault_around_discard(
                &anon_fault_around_cpu_counters().invariant_failure_pages,
                discarded,
            );
            return FaultAroundCommit::Done(FaultOutcome::Kernel(
                KernelFaultReason::NotInitialized,
            ));
        };
        let page_size = page_size();
        let mut pages = self.pages.lock();
        let fault_resident = pages.contains_key(plan.fault_page);
        let fault_present = if cfg!(debug_assertions) {
            (unsafe { (ops.count_mapped)(self.pgd, plan.fault_page, page_size) }) != 0
        } else {
            fault_resident
        };
        if fault_resident != fault_present {
            #[cfg(feature = "performance-profile")]
            let discarded = prepared.len();
            drop(pages);
            drop(set);
            prepared.clear();
            #[cfg(feature = "performance-profile")]
            record_anon_fault_around_discard(
                &anon_fault_around_cpu_counters().invariant_failure_pages,
                discarded,
            );
            return FaultAroundCommit::Done(FaultOutcome::Kernel(
                KernelFaultReason::UncaughtKernelAccess,
            ));
        }
        if fault_resident {
            #[cfg(feature = "performance-profile")]
            let discarded = prepared.len();
            drop(pages);
            drop(set);
            prepared.clear();
            #[cfg(feature = "performance-profile")]
            record_anon_fault_around_discard(
                &anon_fault_around_cpu_counters().raced_pages,
                discarded,
            );
            self.publish_new_user_range(plan.fault_page, page_size);
            return FaultAroundCommit::Done(FaultOutcome::Fixed);
        }

        // resident ledger 在 VMA/pages 锁内与叶 PTE 同步提交，发布构建据此截断
        // 投机前缀，避免每页重复遍历页表。调试构建仍核对真实 PTE 以捕获不变量破坏。
        let mut prefix_len = 0usize;
        let mut invariant_failure = false;
        for candidate in prepared.iter() {
            let resident = pages.contains_key(candidate.vaddr);
            let present = if cfg!(debug_assertions) {
                (unsafe { (ops.count_mapped)(self.pgd, candidate.vaddr, page_size) }) != 0
            } else {
                resident
            };
            if resident != present {
                invariant_failure = true;
                break;
            }
            if resident {
                break;
            }
            prefix_len += 1;
        }
        if invariant_failure || prefix_len == 0 {
            #[cfg(feature = "performance-profile")]
            let discarded = prepared.len();
            drop(pages);
            drop(set);
            prepared.clear();
            #[cfg(feature = "performance-profile")]
            record_anon_fault_around_discard(
                &anon_fault_around_cpu_counters().invariant_failure_pages,
                discarded,
            );
            return FaultAroundCommit::Done(FaultOutcome::Kernel(
                KernelFaultReason::UncaughtKernelAccess,
            ));
        }

        #[cfg(feature = "performance-profile")]
        let prepared_len = prepared.len();
        debug_assert!(prefix_len <= ANON_STORE_FAULT_AROUND_PAGES);
        let mut paddrs = [0usize; ANON_STORE_FAULT_AROUND_PAGES];
        for (slot, candidate) in paddrs.iter_mut().zip(prepared.iter()).take(prefix_len) {
            *slot = candidate.page.paddr();
        }
        let (installed, map_error) = self.map_pages_no_flush(
            plan.fault_page,
            &paddrs[..prefix_len],
            pte_flags_for(plan.flags, PageAccess::Writable),
        );
        let mut candidates = prepared.drain(..);
        let replaced = pages.insert_contiguous(
            plan.fault_page,
            candidates
                .by_ref()
                .take(installed)
                .enumerate()
                .map(|(index, candidate)| {
                    debug_assert_eq!(candidate.vaddr, plan.fault_page + index * page_size);
                    PageMapping {
                        page: candidate.page,
                        access: PageAccess::Writable,
                    }
                }),
        );
        debug_assert_eq!(replaced, 0);
        let mapped = pages.len();
        if installed != 0 {
            self.mapped_pages.store(mapped, Ordering::Release);
        }
        drop(pages);
        drop(set);
        // 未提交页的 Arc 析构会归还物理页，必须发生在 VMA/pages 锁外。
        drop(candidates);
        #[cfg(feature = "performance-profile")]
        record_anon_fault_around_commit(
            installed,
            prepared_len - prefix_len,
            prefix_len - installed,
            map_error.is_some(),
        );

        if installed == 0 {
            if matches!(map_error, Some(Errno::ENOMEM)) {
                // 释放全部投机数据页后退回单页路径，让页表页分配在更低内存
                // 压力下重试；不能把批量优化自身造成的临界 OOM 升级为 panic。
                return FaultAroundCommit::Retry;
            }
            return FaultAroundCommit::Done(fault_from_errno(map_error.unwrap_or(Errno::EINVAL)));
        }
        // 一次屏障发布全部 PTE；仅 fault_page 必然在本 CPU 缓存了无效翻译。
        self.publish_new_user_range(plan.fault_page, page_size);
        FaultAroundCommit::Done(FaultOutcome::Fixed)
    }

    fn commit_fault_page(
        &self,
        page_va: usize,
        area_range: Range<usize>,
        backing: VmBacking,
        flags: VmFlags,
        kind: FaultKind,
        profile_phases: bool,
    ) -> FaultAroundCommit {
        #[cfg(feature = "performance-profile")]
        let _profile = profile_phases.then(|| profiling::scope(profiling::Event::PageFaultSingle));
        #[cfg(not(feature = "performance-profile"))]
        let _ = profile_phases;
        let page = match &backing {
            VmBacking::Anon { .. } => alloc_zeroed_user_page()
                .map(ResidentPage::new_anon)
                .ok_or(Errno::ENOMEM),
            VmBacking::SharedAnon { object, offset } => {
                let object_off = offset + (page_va - area_range.start) as u64;
                shared_anon_page(object, object_off)
            }
            VmBacking::File { file, offset } => {
                let file_off = offset + (page_va - area_range.start) as u64;
                if flags.has(VmFlags::SHARED) {
                    shared_file_page(Arc::clone(file), file_off)
                } else {
                    private_file_page(file, file_off, profile_phases)
                }
            }
            VmBacking::Direct(base) => {
                let paddr = base + (page_va - area_range.start);
                Ok(ResidentPage::new_direct(paddr))
            }
        };
        let page = match page {
            Ok(page) => page,
            Err(err) => return FaultAroundCommit::Done(fault_from_errno(err)),
        };
        let mut page = page;
        let mut access = access_for_new_page(flags, &page);
        if is_write_fault(kind) && matches!(access, PageAccess::Cow) {
            page = match clone_page_to_anon(&page) {
                Ok(page) => page,
                Err(err) => return FaultAroundCommit::Done(fault_from_errno(err)),
            };
            access = PageAccess::Writable;
        }
        if page.is_sysv_shm() && flags.has(VmFlags::WRITE) {
            // SysV shm is a shared memory object, not a regular file mapping.
            // Keep it writable across fork, but conservatively flush it back if
            // the last resident page disappears before another attach faults it.
            page.mark_dirty();
        }
        if is_write_fault(kind) && matches!(access, PageAccess::SharedTracked) {
            page.mark_dirty();
            access = PageAccess::Writable;
        }

        let set = self.vmas.lock();
        let Some(area) = set.find(page_va) else {
            drop(set);
            drop(page);
            return FaultAroundCommit::Retry;
        };
        if area.range != area_range
            || area.flags != flags
            || !same_backing_snapshot(&area.backing, &backing)
        {
            drop(set);
            drop(page);
            return FaultAroundCommit::Retry;
        }

        let mut pages = self.pages.lock();
        #[cfg(debug_assertions)]
        let pte_present = user_pgd_ops().is_some_and(|ops| {
            (unsafe { (ops.count_mapped)(self.pgd, page_va, page_size()) }) != 0
        });
        if let Some(mapping) = pages.get_mut(page_va) {
            #[cfg(debug_assertions)]
            if !pte_present {
                drop(pages);
                drop(set);
                drop(page);
                return FaultAroundCommit::Done(FaultOutcome::Kernel(
                    KernelFaultReason::UncaughtKernelAccess,
                ));
            }
            let update = self.handle_resident_fault_locked(page_va, flags, kind, mapping);
            drop(pages);
            drop(set);
            drop(page);
            return FaultAroundCommit::Done(self.finish_resident_fault(page_va, update, true));
        }
        #[cfg(debug_assertions)]
        if pte_present {
            drop(pages);
            drop(set);
            drop(page);
            return FaultAroundCommit::Done(FaultOutcome::Kernel(
                KernelFaultReason::UncaughtKernelAccess,
            ));
        }
        if let Err(err) =
            self.map_page_no_flush(page_va, page.paddr(), pte_flags_for(flags, access))
        {
            drop(pages);
            drop(set);
            drop(page);
            return FaultAroundCommit::Done(fault_from_errno(err));
        }
        let previous = pages.insert(page_va, PageMapping { page, access });
        debug_assert!(previous.is_none());
        let mapped = pages.len();
        self.mapped_pages.store(mapped, Ordering::Release);
        drop(pages);
        drop(set);
        self.publish_new_user_range(page_va, page_size());
        FaultAroundCommit::Done(FaultOutcome::Fixed)
    }

    /// swap 换入的缺页提交：槽位已在调用方从换出表中摘除，这里读回内容并安装。
    fn commit_swap_in(&self, page_va: usize, slot: SwapSlot) -> FaultAroundCommit {
        match self.swap_in_one(page_va, slot) {
            Ok(()) => FaultAroundCommit::Done(FaultOutcome::Fixed),
            Err(Errno::ENOMEM) => FaultAroundCommit::Done(FaultOutcome::OutOfMemory),
            // 换入失败(交换区损坏/并发变化):退回普通匿名零页缺页路径保证前进性。
            Err(_) => FaultAroundCommit::Retry,
        }
    }

    /// 把 `slot` 的页读回并安装到 `page_va`；槽位无论成败都会被归还。
    ///
    /// 调用方负责在此之前把该页从 `self.swapped` 表中摘除，避免与 unmap 的
    /// 槽位归还重复。失败返回 `Err` 时内容已无法恢复，调用方回退零页或放弃。
    fn swap_in_one(&self, page_va: usize, slot: SwapSlot) -> Result<(), Errno> {
        let page_size = page_size();
        let paddr = match unsafe { alloc_uninitialized_user_page() } {
            Some(paddr) => paddr,
            None => {
                crate::mm::swap::swap_free(slot);
                return Err(Errno::ENOMEM);
            }
        };
        let read_result = (|| {
            let virt = allocator::KERNEL_ALLOCATOR
                .load_phys_to_virt()
                .ok_or(Errno::EINVAL)?;
            // Safety: 尚未发布的独占整页,直映地址覆盖 page_size 字节。
            let buf = unsafe { core::slice::from_raw_parts_mut(virt(paddr) as *mut u8, page_size) };
            crate::mm::swap::swap_in_page(slot, buf)
        })();
        crate::mm::swap::swap_free(slot);
        if let Err(err) = read_result {
            free_user_page(paddr);
            return Err(err);
        }
        let page = ResidentPage::new_anon(paddr);
        let (access, flags) = {
            let set = self.vmas.lock();
            let area = set.find(page_va).ok_or(Errno::EINVAL)?;
            if !matches!(area.backing, VmBacking::Anon { .. }) || area.flags.has(VmFlags::SHARED) {
                return Err(Errno::EINVAL);
            }
            (access_for_new_page(area.flags, &page), area.flags)
        };
        let mut pages = self.pages.lock();
        if pages.contains_key(page_va) {
            // 并发安装：刚读回的页作废(Arc 析构归还物理页)。
            return Err(Errno::EEXIST);
        }
        if let Err(err) =
            self.map_page_no_flush(page_va, page.paddr(), pte_flags_for(flags, access))
        {
            drop(pages);
            return Err(err);
        }
        pages.insert(page_va, PageMapping { page, access });
        let mapped = pages.len();
        self.mapped_pages.store(mapped, Ordering::Release);
        drop(pages);
        self.publish_new_user_range(page_va, page_size);
        Ok(())
    }

    /// fork 前把父进程全部换出页读回，保证子进程按 COW 语义看到完整内容。
    ///
    /// 这是"槽位表不随 fork 共享"这一简化下的正确性兜底：父进程先完全驻留，
    /// 再走普通 COW fork 快照（Linux 会共享 swap 槽位并延迟换入，本内核不做）。
    fn swap_in_all(&self) {
        let mut all = self.swapped.lock().take_all();
        let mut entries = Vec::new();
        all.for_each_mut(|va, slot| entries.push((va, *slot)));
        for (va, slot) in entries {
            let _ = self.swap_in_one(va, slot);
        }
    }

    fn finish_resident_fault(
        &self,
        page_va: usize,
        update: (FaultOutcome, bool, Option<Arc<ResidentPage>>),
        publish_unchanged_mapping: bool,
    ) -> FaultOutcome {
        let (outcome, invalidate, retired) = update;
        if invalidate {
            self.invalidate_user_range(page_va, page_size());
        } else if publish_unchanged_mapping && matches!(outcome, FaultOutcome::Fixed) {
            // 本核因旧无效 translation 进入缺页，但另一 CPU 可能已经安装了叶 PTE。
            // 这里只需收敛当前 CPU；该分支没有替换任何旧有效映射。
            self.publish_new_user_range(page_va, page_size());
        }
        // COW 的旧页必须活到所有 CPU 都完成 TLB 失效之后，避免远端仍通过旧
        // TLB 访问已回收的物理页。
        drop(retired);
        outcome
    }

    fn handle_resident_fault_locked(
        &self,
        page_va: usize,
        flags: VmFlags,
        kind: FaultKind,
        mapping: &mut PageMapping,
    ) -> (FaultOutcome, bool, Option<Arc<ResidentPage>>) {
        if matches!(kind, FaultKind::Privilege) {
            return match self.protect_page_no_flush(page_va, pte_flags_for(flags, mapping.access)) {
                Ok(()) => (FaultOutcome::Fixed, true, None),
                Err(err) => (fault_from_errno(err), false, None),
            };
        }
        if !is_write_fault(kind) {
            return (FaultOutcome::Fixed, false, None);
        }
        match mapping.access {
            PageAccess::Writable => (FaultOutcome::Fixed, false, None),
            PageAccess::SharedTracked => {
                let access = PageAccess::Writable;
                if let Err(err) = self.protect_page_no_flush(page_va, pte_flags_for(flags, access))
                {
                    return (fault_from_errno(err), false, None);
                }
                mapping.page.mark_dirty();
                mapping.access = access;
                (FaultOutcome::Fixed, true, None)
            }
            PageAccess::Cow => {
                #[cfg(feature = "performance-profile")]
                profiling::record(profiling::Event::PageFaultCow, 0, page_size() as u64, 1);
                let new_page = match clone_page_to_anon(&mapping.page) {
                    Ok(page) => page,
                    Err(err) => return (fault_from_errno(err), false, None),
                };
                if let Err(err) = self.replace_page_no_flush(
                    page_va,
                    new_page.paddr(),
                    pte_flags_for(flags, PageAccess::Writable),
                ) {
                    return (fault_from_errno(err), false, None);
                }
                let old_page = core::mem::replace(&mut mapping.page, new_page);
                mapping.access = PageAccess::Writable;
                (FaultOutcome::Fixed, true, Some(old_page))
            }
            PageAccess::ReadOnly => (FaultOutcome::Segv, false, None),
        }
    }

    fn map_page_no_flush(&self, vaddr: usize, paddr: usize, flags: VmFlags) -> Result<(), Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        unsafe { (ops.map)(self.pgd, vaddr, paddr, flags.with(VmFlags::USER)) }
            .map_err(errno_from_map_error)
    }

    /// 连续安装基础页叶 PTE，并返回已生效前缀及首个错误。
    ///
    /// 批量 arch 回调可能在页表页分配失败前已经发布一部分叶 PTE；调用方必须
    /// 用返回的页数同步提交 resident 页所有权，不能回滚这些已可见映射。
    fn map_pages_no_flush(
        &self,
        vaddr: usize,
        paddrs: &[usize],
        flags: VmFlags,
    ) -> (usize, Option<Errno>) {
        let Some(ops) = user_pgd_ops() else {
            return (0, Some(Errno::EINVAL));
        };
        let result = unsafe { (ops.map_pages)(self.pgd, vaddr, paddrs, flags.with(VmFlags::USER)) };
        (result.mapped, result.error.map(errno_from_map_error))
    }

    fn protect_page_no_flush(&self, vaddr: usize, flags: VmFlags) -> Result<(), Errno> {
        self.protect_pages_no_flush(vaddr, page_size(), flags)
    }

    fn protect_pages_no_flush(
        &self,
        vaddr: usize,
        len: usize,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        unsafe {
            (ops.protect)(self.pgd, vaddr, len, flags.with(VmFlags::USER));
        }
        Ok(())
    }

    fn invalidate_user_range(&self, vaddr: usize, len: usize) {
        if let Some(ops) = user_pgd_ops() {
            // Safety: 调用方已经完成页表更新；ExistingMapping 要求同步所有历史
            // 激活 CPU，防止旧有效 translation 越过权限或资源回收边界。
            unsafe {
                UserPteUpdate::ExistingMapping.publish(ops, self.pgd, vaddr, len);
            }
        }
    }

    fn publish_new_user_range(&self, vaddr: usize, len: usize) {
        if let Some(ops) = user_pgd_ops() {
            // Safety: 仅由确认原先没有叶 PTE 的建图路径或硬件缺页的无效缓存
            // 收敛路径调用；不会替换仍可能被其它 CPU 使用的有效 translation。
            unsafe {
                UserPteUpdate::NewMapping.publish(ops, self.pgd, vaddr, len);
            }
        }
    }

    fn flush_full_user_tlb(&self) {
        // vaddr=1, len=usize::MAX 会溢出，触发 arch 层全局 flush（with_asid(asid, None)）。
        if let Some(ops) = user_pgd_ops() {
            unsafe { (ops.invalidate_range)(self.pgd, 1, usize::MAX) };
        }
    }

    fn replace_page_no_flush(
        &self,
        vaddr: usize,
        paddr: usize,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        let page_size = page_size();
        unsafe {
            (ops.unmap)(self.pgd, vaddr, page_size);
            (ops.map)(self.pgd, vaddr, paddr, flags.with(VmFlags::USER))
                .expect("[mm] replacement map failed after retaining page-table path");
        }
        Ok(())
    }

    fn unmap_page_mappings(&self, range: Range<usize>) -> Result<Vec<(usize, PageMapping)>, Errno> {
        self.unmap_page_mappings_impl(range, false)
    }

    /// 同 [`Self::unmap_page_mappings`]，但**保留**范围内已登记的换出槽位。
    ///
    /// 供 `MADV_PAGEOUT` 使用：槽位刚在换出路径中登记，摘除驻留页时不能把槽位
    /// 一起归还；FREE/COLD 标记仍随页失效。
    fn unmap_page_mappings_preserve_swap(
        &self,
        range: Range<usize>,
    ) -> Result<Vec<(usize, PageMapping)>, Errno> {
        self.unmap_page_mappings_impl(range, true)
    }

    fn unmap_page_mappings_impl(
        &self,
        range: Range<usize>,
        preserve_swap: bool,
    ) -> Result<Vec<(usize, PageMapping)>, Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        let mut pages = self.pages.lock();
        let removed = pages.take_range(range.clone());
        let page_size = page_size();
        let mut run_start = None;
        let mut run_end = 0usize;
        for (vaddr, _) in &removed {
            if let Some(start) = run_start
                && *vaddr != run_end
            {
                unsafe { (ops.unmap)(self.pgd, start, run_end - start) };
                run_start = None;
            }
            run_start.get_or_insert(*vaddr);
            run_end = vaddr.saturating_add(page_size);
        }
        if let Some(start) = run_start {
            unsafe { (ops.unmap)(self.pgd, start, run_end - start) };
        }
        let mapped = pages.len();
        self.mapped_pages.store(mapped, Ordering::Release);
        drop(pages);
        if !preserve_swap {
            // 换出槽位随页一起失效：换出页被丢弃时归还槽位。
            let swapped = self.swapped.lock().take_range(range.clone());
            for (_, slot) in swapped {
                crate::mm::swap::swap_free(slot);
            }
        }
        self.freeable.lock().take_range(range.clone());
        self.cold.lock().take_range(range);
        Ok(removed)
    }

    /// 在调用方持有 VMA 锁时迁移 resident/PTE，保持元数据和页表事务一致。
    fn move_page_mappings_locked(
        &self,
        set: &VmaSet,
        old_start: usize,
        new_start: usize,
        len: usize,
    ) -> Result<bool, Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        let old_range = old_start..old_start + len;
        let mut pages = self.pages.lock();
        let keys = pages.keys_in_range(old_range.clone());
        let mut moves = Vec::with_capacity(keys.len());
        for old_va in &keys {
            let new_va = new_start + (old_va - old_start);
            let area = set.find(new_va).ok_or(Errno::ENOMEM)?;
            let mapping = pages.get(*old_va).ok_or(Errno::ENOMEM)?;
            moves.push((
                *old_va,
                new_va,
                mapping.page.paddr(),
                pte_flags_for(area.flags, mapping.access),
            ));
        }
        for (old_va, new_va, paddr, flags) in moves {
            let mapping = pages.remove(old_va).ok_or(Errno::ENOMEM)?;
            unsafe {
                (ops.unmap)(self.pgd, old_va, page_size());
                (ops.map)(self.pgd, new_va, paddr, flags.with(VmFlags::USER))
                    .expect("[mm] mremap destination map failed");
            }
            pages.insert(new_va, mapping);
        }
        let mapped = pages.len();
        self.mapped_pages.store(mapped, Ordering::Release);
        drop(pages);
        // 换出槽位与 FREE/COLD 标记跟随页一起迁移到新地址。
        self.relocate_aux_maps(old_start, new_start, len);
        Ok(!keys.is_empty())
    }

    /// mremap 迁移时把 `old_start..old_start+len` 内的换出槽位与 FREE/COLD
    /// 标记整体搬移到以 `new_start` 开头的对应区间。
    fn relocate_aux_maps(&self, old_start: usize, new_start: usize, len: usize) {
        let old_range = old_start..old_start + len;
        relocate_aux_map(&self.swapped, old_range.clone(), new_start);
        relocate_aux_map(&self.freeable, old_range.clone(), new_start);
        relocate_aux_map(&self.cold, old_range, new_start);
    }

    fn extend_mapping_in_place(
        &self,
        old_range: &Range<usize>,
        tail_range: &Range<usize>,
    ) -> Result<bool, Errno> {
        if tail_range.start >= tail_range.end {
            return Ok(true);
        }
        let mapped_tail = {
            let mut vmas = self.vmas.lock();
            if !vmas.contains_range(old_range) {
                return Err(Errno::ENOMEM);
            }
            if !vmas.is_range_free(tail_range) {
                return Ok(false);
            }
            let last = vmas
                .find(old_range.end - page_size())
                .cloned()
                .ok_or(Errno::ENOMEM)?;
            let shift = last.range.end - last.range.start;
            let backing = last.backing.checked_shift(shift).ok_or(Errno::EINVAL)?;
            let tail = VmArea {
                range: tail_range.clone(),
                flags: last.flags,
                backing,
            };
            let files = Self::collect_file_backings(core::iter::once(&tail));
            vmas.insert(tail.clone())?;
            (tail, files)
        };
        let (tail, files) = mapped_tail;
        self.account_area_insert(&tail);
        Self::notify_files_mapped(files);
        Ok(true)
    }

    /// 收集 VMA 上的 file backing，生命周期 hook 统一在锁外调用。
    ///
    /// 这样 VMA 树只负责描述已经生效的映射变化，SysV shm 等特殊 FileLike 在
    /// hook 内维护 attach 计数时，不会反向持有或阻塞 VM 内部锁。
    fn collect_file_backings<'a>(
        areas: impl IntoIterator<Item = &'a VmArea>,
    ) -> Vec<Arc<dyn FileLike>> {
        let mut files = Vec::new();
        for area in areas {
            if let VmBacking::File { file, .. } = &area.backing {
                files.push(Arc::clone(file));
            }
        }
        files
    }

    fn notify_files_mapped(files: Vec<Arc<dyn FileLike>>) {
        for file in files {
            file.on_mapped();
        }
    }

    fn notify_file_unmapped(areas: &[VmArea]) {
        let files = Self::collect_file_backings(areas.iter());
        for file in files {
            file.on_unmapped();
        }
    }
}

impl Drop for VmSpace {
    fn drop(&mut self) {
        VM_SPACE_DROPPED.fetch_add(1, Ordering::Relaxed);
        VM_SPACE_LIVE.fetch_sub(1, Ordering::Relaxed);
        // 归还 overcommit 承诺记账。
        let committed = self.committed_pages.load(Ordering::Acquire);
        if committed != 0 {
            memstat::commit_pages(-(committed as i64));
        }
        let (files, areas) = {
            let mut vmas = self.vmas.lock();
            let files = Self::collect_file_backings(vmas.iter());
            let areas = vmas.take_all();
            (files, areas)
        };
        for file in files {
            file.on_unmapped();
        }
        let resident_pages = self.pages.lock().take_all();
        drop(resident_pages);
        drop(areas);
        prune_shared_anon_pages();
        if let Some(ops) = user_pgd_ops() {
            unsafe { (ops.drop_pgd)(self.pgd) };
        }
    }
}

/// 把 `old_range` 内的全部条目搬移到以 `new_start` 开头的对应区间。
///
/// 供 mremap 迁移换出槽位 / FREE / COLD 标记使用。
fn relocate_aux_map<T>(map: &Spinlock<RadixPageMap<T>>, old_range: Range<usize>, new_start: usize) {
    let moves = map.lock().take_range(old_range.clone());
    let mut map = map.lock();
    for (old_va, value) in moves {
        let new_va = new_start + (old_va - old_range.start);
        map.insert(new_va, value);
    }
}

fn access_for_new_page(flags: VmFlags, page: &ResidentPage) -> PageAccess {
    if page.is_private_file() {
        return access_for_private_file(flags);
    }
    if page.is_direct_shared_writable() {
        return if flags.has(VmFlags::WRITE) {
            PageAccess::Writable
        } else {
            PageAccess::ReadOnly
        };
    }
    if !flags.has(VmFlags::WRITE) {
        PageAccess::ReadOnly
    } else if flags.has(VmFlags::SHARED) {
        PageAccess::SharedTracked
    } else {
        PageAccess::Writable
    }
}

fn access_for_existing_page(flags: VmFlags, page: &Arc<ResidentPage>) -> PageAccess {
    if page.is_private_file() {
        return access_for_private_file(flags);
    }
    if page.is_direct_shared_writable() {
        return if flags.has(VmFlags::WRITE) {
            PageAccess::Writable
        } else {
            PageAccess::ReadOnly
        };
    }
    if !flags.has(VmFlags::WRITE) {
        PageAccess::ReadOnly
    } else if flags.has(VmFlags::SHARED) {
        PageAccess::SharedTracked
    } else if Arc::strong_count(page) > 1 {
        PageAccess::Cow
    } else {
        PageAccess::Writable
    }
}

fn access_after_fork(flags: VmFlags, page: &Arc<ResidentPage>) -> PageAccess {
    if page.is_direct_shared_writable() {
        return if flags.has(VmFlags::WRITE) {
            PageAccess::Writable
        } else {
            PageAccess::ReadOnly
        };
    }
    if !flags.has(VmFlags::WRITE) {
        PageAccess::ReadOnly
    } else if flags.has(VmFlags::SHARED) {
        PageAccess::SharedTracked
    } else {
        PageAccess::Cow
    }
}

fn access_for_private_file(flags: VmFlags) -> PageAccess {
    if flags.has(VmFlags::WRITE) {
        PageAccess::Cow
    } else {
        PageAccess::ReadOnly
    }
}

fn pte_flags_for(flags: VmFlags, access: PageAccess) -> VmFlags {
    let flags = flags.with(VmFlags::USER);
    if access.pte_writable() {
        flags
    } else {
        flags.without(VmFlags::WRITE)
    }
}

/// 读取文件当前的共享页缓存代际；实现未提供信号时退化为 `0`，与既有行为一致。
///
/// 该函数是共享页缓存代际感知的唯一切入点：加载、查找、发布、删除共享页时都使用
/// 同一代际，保证旧代际的驻留页在新代际下不再命中，同时 `Drop` 仍能按其加载时
/// 的代际正确从缓存移除。
fn shared_file_page_generation(file: &Arc<dyn FileLike>) -> u64 {
    file.shared_page_cache_generation().unwrap_or(0)
}

fn shared_file_page(file: Arc<dyn FileLike>, file_off: u64) -> Result<Arc<ResidentPage>, Errno> {
    let generation = shared_file_page_generation(&file);
    let key = FilePageKey::new(&file, file_off, generation);
    if let Some(page) = find_cached_file_page(&SHARED_FILE_PAGES, key) {
        return Ok(page);
    }
    let paddr = load_file_page(&*file, file_off)?;
    let page = ResidentPage::new_shared_file(paddr, Arc::clone(&file), file_off, generation);
    Ok(publish_cached_file_page(&SHARED_FILE_PAGES, key, page))
}

/// 在 fault-around 准备阶段合并连续私有文件 cache miss。
///
/// 命中页直接返回，短 miss 前缀或任何乐观快照失败均交回原有逐页路径；候选窗口
/// 只执行一次 claim 流，不再先逐页探测、随后为同一批 key 重走缓存查找。
fn prepare_private_file_cache_run(
    file: &Arc<dyn FileLike>,
    cache_snapshot: Option<PrivateFileCacheSnapshot>,
    file_off: u64,
    window_pages: usize,
    page_size: usize,
    profile_phases: bool,
) -> PreparedPrivateFileCacheRun {
    let Some(cache_snapshot) = cache_snapshot else {
        return PreparedPrivateFileCacheRun::Fallback;
    };

    let Some(plan) = private_file_batch_plan(
        file_off,
        cache_snapshot.file_size,
        window_pages,
        window_pages,
        page_size,
    ) else {
        return PreparedPrivateFileCacheRun::Fallback;
    };
    match load_private_file_page_batch(
        file,
        cache_snapshot.file_key,
        cache_snapshot.generation,
        file_off,
        page_size,
        plan,
        profile_phases,
    ) {
        Ok(PrivateFilePageBatchLoad::Cached(page)) => PreparedPrivateFileCacheRun::Cached(page),
        Ok(PrivateFilePageBatchLoad::Batched(batch)) => PreparedPrivateFileCacheRun::Batched(batch),
        Ok(PrivateFilePageBatchLoad::Fallback) => PreparedPrivateFileCacheRun::Fallback,
        Err(error) => PreparedPrivateFileCacheRun::Error(error),
    }
}

fn load_private_file_page_batch(
    file: &Arc<dyn FileLike>,
    file_key: usize,
    generation: u64,
    file_off: u64,
    page_size: usize,
    plan: PrivateFileBatchPlan,
    profile_phases: bool,
) -> Result<PrivateFilePageBatchLoad, Errno> {
    #[cfg(not(feature = "performance-profile"))]
    let _ = profile_phases;
    #[cfg(feature = "performance-profile")]
    let _profile = profile_phases.then(|| profiling::scope(profiling::Event::PageFaultCacheFill));
    let Some(virt) = allocator::KERNEL_ALLOCATOR.load_phys_to_virt() else {
        return Ok(PrivateFilePageBatchLoad::Fallback);
    };
    if plan.pages > PRIVATE_FILE_BATCH_MAX_PAGES {
        return Ok(PrivateFilePageBatchLoad::Fallback);
    }
    let first_key = FilePageKey::new_private(file_key, file_off, generation);
    if let Some(ready) = PRIVATE_FILE_PAGES.ready_contiguous(first_key, page_size, plan.pages) {
        if file.private_page_cache_generation() != Some(generation) {
            return Ok(PrivateFilePageBatchLoad::Fallback);
        }
        if ready.len() == 1 {
            return Ok(PrivateFilePageBatchLoad::Cached(
                ready.into_iter().next().expect("单页 Ready 前缀不能为空"),
            ));
        }
        return Ok(PrivateFilePageBatchLoad::Batched(ready));
    }
    let Some(prefix) = PRIVATE_FILE_PAGES.claim_contiguous_prefix(first_key, page_size, plan.pages)
    else {
        return Ok(PrivateFilePageBatchLoad::Fallback);
    };
    let owners = prefix.owners;
    let terminal = prefix.terminal;
    match terminal {
        Some((0, PrivateFilePageCacheClaim::Ready(page))) => {
            abort_private_file_page_loads(&owners, None);
            return if file.private_page_cache_generation() == Some(generation) {
                Ok(PrivateFilePageBatchLoad::Cached(page))
            } else {
                Ok(PrivateFilePageBatchLoad::Fallback)
            };
        }
        Some((index, PrivateFilePageCacheClaim::Failed(error)))
            if private_file_batch_error_is_fatal(index) =>
        {
            abort_private_file_page_loads(&owners, None);
            return if file.private_page_cache_generation() == Some(generation) {
                Err(error)
            } else {
                Ok(PrivateFilePageBatchLoad::Fallback)
            };
        }
        Some((_, PrivateFilePageCacheClaim::Bypass)) => {
            abort_private_file_page_loads(&owners, None);
            return Ok(PrivateFilePageBatchLoad::Fallback);
        }
        _ => {}
    }
    if owners.len() < PRIVATE_FILE_BATCH_MIN_PAGES {
        abort_private_file_page_loads(&owners, None);
        return Ok(PrivateFilePageBatchLoad::Fallback);
    }
    debug_assert!(!owners.spilled());
    if file.private_page_cache_generation() != Some(generation) {
        abort_private_file_page_loads(&owners, None);
        return Ok(PrivateFilePageBatchLoad::Fallback);
    }

    let mut candidates = PrivateFilePageBatch::new();
    let mut pages = PrivateFilePageBatch::new();

    let mut allocations = [None; PRIVATE_FILE_BATCH_MAX_PAGES];
    let allocated = alloc_uninitialized_user_page_batch(&mut allocations[..owners.len()]);
    if allocated != owners.len() {
        for allocation in allocations[..allocated].iter_mut().filter_map(Option::take) {
            let _ = allocator::KERNEL_ALLOCATOR.try_free_untracked_physical(allocation);
        }
        abort_private_file_page_loads(&owners, None);
        return Ok(PrivateFilePageBatchLoad::Fallback);
    }
    for allocation in allocations[..allocated].iter_mut().filter_map(Option::take) {
        candidates.push(ResidentPage::new_private_file(allocation.paddr));
    }
    debug_assert!(!candidates.spilled());
    let Some(owner_capacity) = owners.len().checked_mul(page_size) else {
        abort_private_file_page_loads(&owners, None);
        return Ok(PrivateFilePageBatchLoad::Fallback);
    };
    let valid_len = plan.read_len.min(owner_capacity);
    if valid_len == 0 {
        abort_private_file_page_loads(&owners, None);
        return Ok(PrivateFilePageBatchLoad::Fallback);
    }
    let mut targets = PrivateFilePageTargets::new();
    for candidate in &candidates {
        // Safety: 每个 candidate 持有不同的独占物理页；所有页在本次批量读取完成
        // 前都只存在于本地内联批次，尚未进入 cache、resident map 或用户页表。
        targets.push(unsafe {
            core::slice::from_raw_parts_mut(virt(candidate.paddr()) as *mut u8, page_size)
        });
    }
    debug_assert!(!targets.spilled());
    let read_result = file.read_pages_at(file_off, targets.as_mut_slice(), valid_len);
    drop(targets);
    if let Err(error) = read_result {
        if file.private_page_cache_generation() == Some(generation) {
            abort_private_file_page_loads(&owners, Some(error));
            return Err(error);
        }
        abort_private_file_page_loads(&owners, None);
        return Ok(PrivateFilePageBatchLoad::Fallback);
    }
    if file.private_page_cache_generation() != Some(generation) {
        abort_private_file_page_loads(&owners, None);
        return Ok(PrivateFilePageBatchLoad::Fallback);
    }

    for (index, ((key, load_id), candidate)) in owners.iter().copied().zip(candidates).enumerate() {
        let Some(page) = PRIVATE_FILE_PAGES.finish_load(key, load_id, candidate) else {
            for (remaining_key, remaining_load_id) in &owners[index + 1..] {
                PRIVATE_FILE_PAGES.abort_load(*remaining_key, *remaining_load_id, None);
            }
            rollback_private_file_page_batch(&PRIVATE_FILE_PAGES, &owners, &pages);
            return Ok(PrivateFilePageBatchLoad::Fallback);
        };
        pages.push(page);
    }
    debug_assert!(!pages.spilled());
    if file.private_page_cache_generation() != Some(generation) {
        rollback_private_file_page_batch(&PRIVATE_FILE_PAGES, &owners, &pages);
        return Ok(PrivateFilePageBatchLoad::Fallback);
    }
    Ok(PrivateFilePageBatchLoad::Batched(pages))
}

fn rollback_private_file_page_batch<const SHARD_COUNT: usize>(
    cache: &ShardedPrivateFilePageCache<SHARD_COUNT>,
    owners: &[(FilePageKey, u64)],
    published: &[Arc<ResidentPage>],
) {
    for ((key, _), page) in owners.iter().zip(published) {
        cache.remove_if_same(*key, page);
    }
}

fn private_file_page(
    file: &Arc<dyn FileLike>,
    file_off: u64,
    profile_phases: bool,
) -> Result<Arc<ResidentPage>, Errno> {
    #[cfg(not(feature = "performance-profile"))]
    let _ = profile_phases;
    for _ in 0..PRIVATE_FILE_CACHE_RETRIES {
        let (Some(file_key), Some(generation)) = (
            file.private_page_cache_key(),
            file.private_page_cache_generation(),
        ) else {
            #[cfg(feature = "performance-profile")]
            let _profile =
                profile_phases.then(|| profiling::scope(profiling::Event::PageFaultUncachedFill));
            let paddr = load_file_page(file.as_ref(), file_off)?;
            return Ok(ResidentPage::new_private_file(paddr));
        };
        let key = FilePageKey::new_private(file_key, file_off, generation);
        let load = match PRIVATE_FILE_PAGES.claim(key) {
            PrivateFilePageCacheClaim::Ready(page) => {
                if file.private_page_cache_generation() == Some(generation) {
                    return Ok(page);
                }
                continue;
            }
            PrivateFilePageCacheClaim::Loading(load) => {
                if file.private_page_cache_generation() != Some(generation) {
                    continue;
                }
                match load.wait()? {
                    Some(page) if file.private_page_cache_generation() == Some(generation) => {
                        return Ok(page);
                    }
                    Some(_) | None => continue,
                }
            }
            PrivateFilePageCacheClaim::Failed(error) => {
                if file.private_page_cache_generation() == Some(generation) {
                    return Err(error);
                }
                continue;
            }
            PrivateFilePageCacheClaim::Owner(load_id) => load_id,
            PrivateFilePageCacheClaim::Bypass => break,
        };
        #[cfg(feature = "performance-profile")]
        let _profile =
            profile_phases.then(|| profiling::scope(profiling::Event::PageFaultCacheFill));
        let paddr = match load_file_page(file.as_ref(), file_off) {
            Ok(paddr) => paddr,
            Err(err) => {
                // truncate/write 可在读页期间改变 EOF。若代际已经变化，这次短读
                // 只是乐观快照失效，应重试新代际；稳定代际的真实 I/O 错误才传播。
                if file.private_page_cache_generation() != Some(generation) {
                    PRIVATE_FILE_PAGES.abort_load(key, load, None);
                    continue;
                }
                PRIVATE_FILE_PAGES.abort_load(key, load, Some(err));
                return Err(err);
            }
        };
        let page = ResidentPage::new_private_file(paddr);
        if file.private_page_cache_generation() != Some(generation) {
            PRIVATE_FILE_PAGES.abort_load(key, load, None);
            continue;
        }
        let Some(page) = PRIVATE_FILE_PAGES.finish_load(key, load, page) else {
            continue;
        };
        if file.private_page_cache_generation() == Some(generation) {
            return Ok(page);
        }
        // 文件在发布窗口内发生写入时，已观察到的旧代际不应继续占据热缓存。
        // 只有仍指向本次候选的条目才会被移除，避免误删并发线程发布的新页。
        PRIVATE_FILE_PAGES.remove_if_same(key, &page);
    }
    #[cfg(feature = "performance-profile")]
    let _profile =
        profile_phases.then(|| profiling::scope(profiling::Event::PageFaultUncachedFill));
    let paddr = load_file_page(file.as_ref(), file_off)?;
    Ok(ResidentPage::new_private_file(paddr))
}

#[cfg(test)]
fn find_cached_private_file_page<const SHARD_COUNT: usize>(
    cache: &ShardedPrivateFilePageCache<SHARD_COUNT>,
    key: FilePageKey,
) -> Option<Arc<ResidentPage>> {
    cache.find(key)
}

fn abort_private_file_page_loads(loads: &[(FilePageKey, u64)], error: Option<Errno>) {
    for (key, load_id) in loads {
        PRIVATE_FILE_PAGES.abort_load(*key, *load_id, error);
    }
}

#[cfg(test)]
fn publish_cached_private_file_page<const SHARD_COUNT: usize>(
    cache: &ShardedPrivateFilePageCache<SHARD_COUNT>,
    key: FilePageKey,
    candidate: Arc<ResidentPage>,
) -> Arc<ResidentPage> {
    loop {
        match cache.claim(key) {
            PrivateFilePageCacheClaim::Ready(page) => return page,
            PrivateFilePageCacheClaim::Loading(load) => match load.wait() {
                Ok(Some(page)) => return page,
                Ok(None) | Err(_) => continue,
            },
            PrivateFilePageCacheClaim::Failed(_) => continue,
            PrivateFilePageCacheClaim::Owner(load_id) => {
                if let Some(page) = cache.finish_load(key, load_id, Arc::clone(&candidate)) {
                    return page;
                }
            }
            PrivateFilePageCacheClaim::Bypass => return candidate,
        }
    }
}

/// 在物理页压力下强制释放一批私有文件缓存引用。
///
/// 每个 Arc 都在缓存锁外析构；仍映射到进程的页会由对应 VMA 继续保活，已经只由
/// 缓存持有的页则立即归还 buddy。返回移除的缓存条目数，而不是实际释放的物理页数。
fn reclaim_private_file_cache_pages(limit: usize) -> usize {
    PRIVATE_FILE_PAGES.reclaim(limit)
}

/// `drop_caches=1`：清空私有干净文件页缓存（逐批回收直到空）。
pub fn drop_private_file_cache() {
    loop {
        let reclaimed = reclaim_private_file_cache_pages(1024);
        if reclaimed == 0 {
            break;
        }
    }
}

fn find_cached_file_page(cache: &WeakFilePageCache, key: FilePageKey) -> Option<Arc<ResidentPage>> {
    cache.lock().get(&key).and_then(Weak::upgrade)
}

/// 并发缺页可能在锁外同时读出同一文件页；这里只允许一个候选进入缓存，
/// 其它候选在释放缓存锁后析构，避免重复驻留和锁递归。
fn publish_cached_file_page(
    cache: &WeakFilePageCache,
    key: FilePageKey,
    candidate: Arc<ResidentPage>,
) -> Arc<ResidentPage> {
    let mut pages = cache.lock();
    if let Some(existing) = pages.get(&key).and_then(Weak::upgrade) {
        drop(pages);
        return existing;
    }
    pages.insert(key, Arc::downgrade(&candidate));
    drop(pages);
    candidate
}

fn remove_cached_file_page(cache: &WeakFilePageCache, key: FilePageKey, page: &ResidentPage) {
    let mut pages = cache.lock();
    if pages
        .get(&key)
        .is_some_and(|weak| core::ptr::eq(weak.as_ptr(), page as *const ResidentPage))
    {
        pages.remove(&key);
    }
}

/// `cachestat(2)` 计数：统计文件 `[off, off+len)` 内处于"页缓存"状态的页。
///
/// 本内核的页缓存由两部分构成：私有干净文件页强缓存（`PRIVATE_FILE_PAGES`，
/// 含未映射的缓存页）与共享文件页缓存（`SHARED_FILE_PAGES`，驻留页的弱引用表）。
///
/// 返回 `(cached, dirty, writeback, evicted, recently_evicted)`。前两个按
/// `[off, off+len)` 精确统计；后三个为**系统级累计计数**（写回/淘汰计数状态，
/// 无 per-file LRU 台账），`recently_evicted` 因无 LRU 时钟退化为累计淘汰数。
pub fn file_cache_stat(
    shared_key: usize,
    private_key: Option<usize>,
    off: u64,
    len: u64,
) -> (u64, u64, u64, u64, u64) {
    let end = off.saturating_add(len);
    let mut cached = 0u64;
    let mut dirty = 0u64;
    {
        let pages = SHARED_FILE_PAGES.lock();
        for (key, weak) in pages.iter() {
            if key.file_key != shared_key || key.offset < off || key.offset >= end {
                continue;
            }
            if let Some(page) = weak.upgrade() {
                cached += 1;
                if page.is_dirty() {
                    dirty += 1;
                }
            }
        }
    }
    if let Some(private_key) = private_key {
        for shard in &PRIVATE_FILE_PAGES.shards {
            let shard = shard.lock();
            for (key, entry) in shard.pages.entries.iter() {
                if key.file_key != private_key || key.offset < off || key.offset >= end {
                    continue;
                }
                if let PrivateFilePageCacheEntry::Ready(ready) = entry {
                    cached += 1;
                    if ready.page.is_dirty() {
                        dirty += 1;
                    }
                }
            }
        }
    }
    let writeback = memstat::file_writeback_pages();
    let evicted = memstat::file_evicted_pages();
    (cached, dirty, writeback, evicted, evicted)
}

fn shared_anon_page(
    object: &Arc<SharedAnonObject>,
    offset: u64,
) -> Result<Arc<ResidentPage>, Errno> {
    prune_shared_anon_pages();
    let key = SharedAnonPageKey {
        id: shared_anon_object_id(object),
        offset,
    };
    {
        let cache = SHARED_ANON_PAGES.lock();
        if let Some(entry) = cache.get(&key) {
            return Ok(Arc::clone(&entry.page));
        }
    }
    let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
    let page = ResidentPage::new_shared_anon(paddr);
    let mut cache = SHARED_ANON_PAGES.lock();
    if let Some(entry) = cache.get(&key) {
        return Ok(Arc::clone(&entry.page));
    }
    cache.insert(
        key,
        SharedAnonPageEntry {
            owner: Arc::downgrade(object),
            page: Arc::clone(&page),
        },
    );
    Ok(page)
}

/// 从共享匿名 backing 读取一段连续数据，不要求对象已经映射到某个用户地址空间。
pub fn read_shared_anon(
    object: &Arc<SharedAnonObject>,
    offset: u64,
    output: &mut [u8],
) -> Result<(), Errno> {
    shared_anon_transfer(object, offset, output.as_mut_ptr(), output.len(), false)
}

/// 向共享匿名 backing 写入一段连续数据，不要求对象已经映射到某个用户地址空间。
pub fn write_shared_anon(
    object: &Arc<SharedAnonObject>,
    offset: u64,
    input: &[u8],
) -> Result<(), Errno> {
    shared_anon_transfer(object, offset, input.as_ptr() as *mut u8, input.len(), true)
}

fn shared_anon_transfer(
    object: &Arc<SharedAnonObject>,
    offset: u64,
    buffer: *mut u8,
    length: usize,
    write: bool,
) -> Result<(), Errno> {
    if length == 0 {
        return Ok(());
    }
    let page_size = page_size();
    let page_size_u64 = u64::try_from(page_size).map_err(|_| Errno::EOVERFLOW)?;
    let end = offset
        .checked_add(u64::try_from(length).map_err(|_| Errno::EOVERFLOW)?)
        .ok_or(Errno::EOVERFLOW)?;
    let virt = allocator::KERNEL_ALLOCATOR
        .load_phys_to_virt()
        .ok_or(Errno::EINVAL)?;
    let mut cursor = offset;
    let mut done = 0usize;
    while cursor < end {
        let page_offset = cursor / page_size_u64 * page_size_u64;
        let within = usize::try_from(cursor - page_offset).map_err(|_| Errno::EOVERFLOW)?;
        let count = (page_size - within).min(length - done);
        let page = shared_anon_page(object, page_offset)?;
        let address = virt(page.paddr())
            .checked_add(within)
            .ok_or(Errno::EOVERFLOW)?;
        if write {
            unsafe {
                core::ptr::copy_nonoverlapping(buffer.add(done), address as *mut u8, count);
            }
            page.mark_dirty();
        } else {
            unsafe {
                core::ptr::copy_nonoverlapping(address as *const u8, buffer.add(done), count);
            }
        }
        cursor = cursor
            .checked_add(u64::try_from(count).map_err(|_| Errno::EOVERFLOW)?)
            .ok_or(Errno::EOVERFLOW)?;
        done += count;
    }
    Ok(())
}

fn prune_shared_anon_pages() {
    SHARED_ANON_PAGES
        .lock()
        .retain(|_, entry| entry.owner.strong_count() != 0);
}

fn load_file_page(file: &dyn FileLike, file_off: u64) -> Result<usize, Errno> {
    let file_size = file.size();
    if file_off >= file_size {
        return Err(Errno::EINVAL);
    }
    // Safety: 本函数只在文件有效前缀和 EOF 尾部全部初始化成功后返回物理页。
    let paddr = unsafe { alloc_uninitialized_user_page().ok_or(Errno::ENOMEM)? };
    let result = (|| {
        let virt = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;
        let page_size = page_size();
        let len = (file_size - file_off).min(page_size as u64) as usize;
        // Safety: paddr 是尚未发布的独占整页，直映地址覆盖 page_size 字节。
        let kbuf = unsafe { core::slice::from_raw_parts_mut(virt(paddr) as *mut u8, page_size) };
        read_file_page_exact(file, file_off, kbuf, len)
    })();
    if result.is_err() {
        free_user_page(paddr);
    }
    result.map(|()| paddr)
}

/// FileLike 允许合法短读；页填充必须持续读取到请求末端。文件长度快照声称仍有
/// 数据却提前返回 EOF 时，确定性报告 I/O 错误，不能把零页尾误当成文件内容。
fn read_file_bytes_exact(file: &dyn FileLike, offset: u64, buf: &mut [u8]) -> Result<(), Errno> {
    let mut done = 0usize;
    while done < buf.len() {
        let read_offset = offset
            .checked_add(u64::try_from(done).map_err(|_| Errno::EINVAL)?)
            .ok_or(Errno::EINVAL)?;
        let remaining = &mut buf[done..];
        let count = file.read_at(read_offset, remaining)?;
        if count == 0 || count > remaining.len() {
            return Err(Errno::EIO);
        }
        done += count;
    }
    Ok(())
}

/// 把一个尚未发布的文件页完整初始化：读取 EOF 前有效字节，只清零不可由文件
/// 提供的尾部。完整页不做预清零，避免随后覆盖同一 4 KiB 的重复写流量。
fn read_file_page_exact(
    file: &dyn FileLike,
    offset: u64,
    page: &mut [u8],
    valid_len: usize,
) -> Result<(), Errno> {
    if valid_len == 0 || valid_len > page.len() {
        return Err(Errno::EINVAL);
    }
    read_file_bytes_exact(file, offset, &mut page[..valid_len])?;
    page[valid_len..].fill(0);
    Ok(())
}

fn clone_page_to_anon(source: &ResidentPage) -> Result<Arc<ResidentPage>, Errno> {
    // Safety: COW 在新页进入 resident ledger/PTE 前无条件覆盖完整用户页。
    let paddr = unsafe { alloc_uninitialized_user_page().ok_or(Errno::ENOMEM)? };
    let result = (|| {
        let virt = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::MemCopyCow).bytes(page_size());
        // Safety: 源页由 ResidentPage 的 Arc 保活；目标页是未发布的独占分配，
        // 两个物理页不重叠，且复制长度恰好等于页大小。
        unsafe {
            core::ptr::copy_nonoverlapping(
                virt(source.paddr()) as *const u8,
                virt(paddr) as *mut u8,
                page_size(),
            );
        }
        Ok(())
    })();
    if result.is_err() {
        free_user_page(paddr);
    }
    result.map(|()| ResidentPage::new_anon(paddr))
}

fn fault_from_errno(err: Errno) -> FaultOutcome {
    match err {
        Errno::ENOMEM => FaultOutcome::OutOfMemory,
        _ => FaultOutcome::Segv,
    }
}

fn errno_from_map_error(error: crate::MapError) -> Errno {
    match error {
        crate::MapError::OutOfMemory => Errno::ENOMEM,
        crate::MapError::AlreadyMapped => Errno::EEXIST,
        crate::MapError::NotMapped => Errno::EFAULT,
        crate::MapError::Misaligned
        | crate::MapError::UnsupportedLevel
        | crate::MapError::UnsupportedHugePage
        | crate::MapError::InvalidPermission => Errno::EINVAL,
    }
}

fn is_write_fault(kind: FaultKind) -> bool {
    matches!(kind, FaultKind::Store | FaultKind::PermWrite)
}

/// flags 是否允许该类访问。
fn permits(flags: VmFlags, kind: FaultKind) -> bool {
    match kind {
        FaultKind::Load | FaultKind::PermRead => flags.has(VmFlags::READ),
        FaultKind::Store | FaultKind::PermWrite => flags.has(VmFlags::WRITE),
        FaultKind::Exec | FaultKind::PermExec => flags.has(VmFlags::EXEC),
        FaultKind::Privilege => flags.permissions().bits() != 0,
    }
}

/// 把 ELF 文件段拆成完整文件页、匿名页和最多两个碎片页。
fn plan_file_segment(
    vaddr: usize,
    memsz: usize,
    file_offset: u64,
    file_size: usize,
    page_size: usize,
) -> Result<FileSegmentPlan, Errno> {
    if page_size == 0 || !page_size.is_power_of_two() || memsz == 0 || file_size > memsz {
        return Err(Errno::EINVAL);
    }
    let mem_end = vaddr.checked_add(memsz).ok_or(Errno::EINVAL)?;
    let file_end = vaddr.checked_add(file_size).ok_or(Errno::EINVAL)?;
    file_offset
        .checked_add(u64::try_from(file_size).map_err(|_| Errno::EINVAL)?)
        .ok_or(Errno::EINVAL)?;

    let mapping_start = vaddr & !(page_size - 1);
    let mapping_end = align_up(mem_end, page_size).ok_or(Errno::EINVAL)?;
    let full_file_start = align_up(vaddr, page_size).ok_or(Errno::EINVAL)?;
    let full_file_end = file_end & !(page_size - 1);
    let (lazy_file, lazy_file_offset) = if full_file_start < full_file_end {
        let delta = u64::try_from(full_file_start - vaddr).map_err(|_| Errno::EINVAL)?;
        let offset = file_offset.checked_add(delta).ok_or(Errno::EINVAL)?;
        (full_file_start..full_file_end, offset)
    } else {
        (mapping_start..mapping_start, file_offset)
    };

    let mut fragment_pages = [0usize; 2];
    let mut fragment_count = 0usize;
    if file_size != 0 {
        let first = mapping_start;
        let last = (file_end - 1) & !(page_size - 1);
        if !lazy_file.contains(&first) {
            fragment_pages[fragment_count] = first;
            fragment_count += 1;
        }
        if last != first && !lazy_file.contains(&last) {
            fragment_pages[fragment_count] = last;
            fragment_count += 1;
        }
    }

    Ok(FileSegmentPlan {
        mapping: mapping_start..mapping_end,
        lazy_file,
        lazy_file_offset,
        fragment_pages,
        fragment_count,
    })
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    Some(value.checked_add(align - 1)? & !(align - 1))
}

fn alloc_zeroed_user_page() -> Option<usize> {
    // Safety: 该页在返回前立即覆盖为全零，不会把旧物理页内容暴露给调用方。
    let paddr = unsafe { alloc_uninitialized_user_page()? };
    let Some(virt) = allocator::KERNEL_ALLOCATOR.load_phys_to_virt() else {
        free_user_page(paddr);
        return None;
    };
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::MemZeroAnonPage).bytes(page_size());
    // Safety: 分配器保证该页至少覆盖 `page_size()` 字节，且当前没有映射发布。
    unsafe { zero_unpublished_user_pages(virt(paddr), page_size()) };
    Some(paddr)
}

/// 分配一个尚未清零、尚未发布的用户物理页。
///
/// # Safety
///
/// 调用方必须在把该页放入 resident ledger、文件 cache 或用户页表之前初始化完整
/// 页面；失败路径必须归还物理页，不能让旧用户数据通过部分写入页泄露。
unsafe fn alloc_uninitialized_user_page() -> Option<usize> {
    let order = user_page_order()?;
    let size = page_size();
    if let Some(paddr) = try_alloc_user_page(order, size) {
        return Some(paddr);
    }

    // 编译负载会把 8 GiB guest 推到很低的空闲页水位。强文件缓存必须是可回收
    // 的性能层，而不能让匿名页/COW 因固定缓存预算提前 ENOMEM。分批释放后重试，
    // 既避免一次丢掉整个热集，也保证持续压力最终可以清空缓存。
    loop {
        if reclaim_private_file_cache_pages(PRIVATE_FILE_CACHE_RECLAIM_BATCH) == 0 {
            return None;
        }
        if let Some(paddr) = try_alloc_user_page(order, size) {
            return Some(paddr);
        }
    }
}

fn alloc_uninitialized_user_page_batch(
    output: &mut [Option<allocator::PhysicalAllocation>],
) -> usize {
    if user_page_order() != Some(0) || page_size() != allocator::PAGE_SIZE {
        output.fill(None);
        return 0;
    }
    allocator::KERNEL_ALLOCATOR.allocate_untracked_order0_batch(output)
}

fn try_alloc_user_page(order: usize, size: usize) -> Option<usize> {
    let allocation = allocator::KERNEL_ALLOCATOR
        .allocate_untracked_physical(allocator::PhysicalAllocRequest::new(
            size,
            allocator::PAGE_SIZE,
        ))
        .ok()?;
    if allocation.order != order || allocation.size != size {
        let _ = allocator::KERNEL_ALLOCATOR.try_free_untracked_physical(allocation);
        return None;
    }
    Some(allocation.paddr)
}

/// 不触发文件缓存回收的零页分配，供可退化的匿名投机邻页使用。
fn try_alloc_zeroed_user_page(order: usize, size: usize) -> Option<usize> {
    let paddr = try_alloc_user_page(order, size)?;
    let Some(virt) = allocator::KERNEL_ALLOCATOR.load_phys_to_virt() else {
        free_user_page(paddr);
        return None;
    };
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::MemZeroAnonPage).bytes(size);
    // Safety: `try_alloc_user_page` 返回独占且尚未发布的完整物理页。
    unsafe { zero_unpublished_user_pages(virt(paddr), size) };
    Some(paddr)
}

/// 清零尚未发布到 resident ledger 或用户页表的完整页面。
///
/// 正常内核路径总是在创建 [`VmSpace`] 前注册 arch MM ops；保留通用回退是为了
/// 让 host 单元测试和早期 smoketest 不依赖具体 ISA。
unsafe fn zero_unpublished_user_pages(vaddr: usize, len: usize) {
    if let Some(ops) = user_pgd_ops() {
        // Safety: 调用方保证该 direct-map 范围独占、可写且覆盖完整用户页。
        unsafe { (ops.zero_user_pages)(vaddr, len) };
    } else {
        // Safety: 与上面的 arch 回调共享同一范围契约。
        unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, len) };
    }
}

fn free_user_page(paddr: usize) {
    let Some(allocation) = user_page_allocation_handle(paddr, page_size()) else {
        log::error!(
            "[mm] invalid user page geometry paddr={:#x} page_size={:#x}",
            paddr,
            page_size()
        );
        return;
    };
    if let Err(err) = allocator::KERNEL_ALLOCATOR.try_free_untracked_physical(allocation) {
        log::error!(
            "[mm] failed to free user page paddr={:#x}: {:?}",
            paddr,
            err
        );
    }
}

#[inline]
fn user_page_allocation_handle(
    paddr: usize,
    page_size: usize,
) -> Option<allocator::PhysicalAllocation> {
    if page_size < allocator::PAGE_SIZE
        || !page_size.is_power_of_two()
        || paddr & (page_size - 1) != 0
    {
        return None;
    }
    let order = page_size.trailing_zeros() - allocator::PAGE_SIZE.trailing_zeros();
    Some(allocator::PhysicalAllocation {
        paddr,
        size: page_size,
        order: order as usize,
        page_size,
    })
}

fn user_page_order() -> Option<usize> {
    let page_size = page_size();
    if page_size < allocator::PAGE_SIZE || !page_size.is_power_of_two() {
        return None;
    }
    Some((page_size.trailing_zeros() - allocator::PAGE_SIZE.trailing_zeros()) as usize)
}

/// 获取 Vec<Range<usize>> 视图，方便调试打印 / smoketest。
#[kernel_symbols::export(name = "general.mm.dump_vmas", contract = "kernel.mm.diagnostic@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED | kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC)]
pub fn dump_vmas(vm: &VmSpace) -> Vec<(Range<usize>, VmFlags)> {
    vm.vmas
        .lock()
        .iter()
        .map(|a| (a.range.clone(), a.flags))
        .collect()
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use super::{
        ANON_STORE_FAULT_AROUND_PAGES, ANON_STORE_SHADOW_PAGES, AnonStoreFaultAround,
        AnonStoreShadowKey, AnonStoreShadowState, FILE_FAULT_AROUND_PAGES, FaultAroundCommit,
        FaultKind, FaultOutcome, FilePageKey, ForkChildMap, PRIVATE_FILE_BATCH_MAX_BYTES,
        PageAccess, PreparedAnonPages, PreparedFilePages, PrivateFileFaultAround,
        PrivateFilePageBatch, PrivateFilePageCacheClaim, PrivateFilePageCacheEntry,
        PrivateFilePageLoadOwners, ResidentPage, ShardedPrivateFilePageCache, VmFlags, VmSpace,
        WeakFilePageCache, access_for_private_file, anon_store_fault_around_end, fault_from_errno,
        file_fault_around_window, find_cached_file_page, find_cached_private_file_page,
        map_fork_child_batches, observe_anon_store_shadow, permits_file_fault_around,
        plan_file_segment, private_file_batch_error_is_fatal, private_file_batch_page_offset,
        private_file_batch_plan, private_file_cache_snapshot, publish_cached_file_page,
        publish_cached_private_file_page, read_file_bytes_exact, read_file_page_exact,
        remove_cached_file_page, rollback_private_file_page_batch, same_backing_snapshot,
        shared_file_page_generation, unmapped_prefix_len, user_page_allocation_handle,
    };
    use errno::Errno;
    use mm::{FileLike, VmBacking};
    use sched::sync::Spinlock;

    const PAGE_SIZE: usize = 4096;

    #[test]
    fn fork_child_mapping_batches_contiguous_pages_with_same_flags() {
        let flags = VmFlags::EMPTY.with(VmFlags::READ);
        let maps = [
            ForkChildMap {
                vaddr: 0x1000,
                paddr: 0x9000,
                flags,
            },
            ForkChildMap {
                vaddr: 0x2000,
                paddr: 0xb000,
                flags,
            },
            ForkChildMap {
                vaddr: 0x3000,
                paddr: 0xa000,
                flags,
            },
        ];
        let mut batches = Vec::new();

        map_fork_child_batches(&maps, PAGE_SIZE, |vaddr, paddrs, batch_flags| {
            batches.push((vaddr, paddrs.to_vec(), batch_flags));
            crate::MapBatchResult {
                mapped: paddrs.len(),
                error: None,
            }
        })
        .expect("contiguous child pages must map as one batch");

        assert_eq!(batches, vec![(0x1000, vec![0x9000, 0xb000, 0xa000], flags)]);
    }

    #[test]
    fn fork_child_mapping_splits_gaps_and_permission_changes() {
        let read = VmFlags::EMPTY.with(VmFlags::READ);
        let write = read.with(VmFlags::WRITE);
        let maps = [
            ForkChildMap {
                vaddr: 0x1000,
                paddr: 0x9000,
                flags: read,
            },
            ForkChildMap {
                vaddr: 0x3000,
                paddr: 0xa000,
                flags: read,
            },
            ForkChildMap {
                vaddr: 0x4000,
                paddr: 0xb000,
                flags: write,
            },
        ];
        let mut starts = Vec::new();

        map_fork_child_batches(&maps, PAGE_SIZE, |vaddr, paddrs, flags| {
            starts.push((vaddr, paddrs.len(), flags));
            crate::MapBatchResult {
                mapped: paddrs.len(),
                error: None,
            }
        })
        .expect("valid child batches must map");

        assert_eq!(
            starts,
            vec![(0x1000, 1, read), (0x3000, 1, read), (0x4000, 1, write)]
        );
    }

    #[test]
    fn fork_child_mapping_stops_after_partial_batch_failure() {
        let flags = VmFlags::EMPTY.with(VmFlags::READ);
        let maps = [
            ForkChildMap {
                vaddr: 0x1000,
                paddr: 0x9000,
                flags,
            },
            ForkChildMap {
                vaddr: 0x2000,
                paddr: 0xa000,
                flags,
            },
        ];

        let result = map_fork_child_batches(&maps, PAGE_SIZE, |_, _, _| crate::MapBatchResult {
            mapped: 1,
            error: Some(crate::MapError::OutOfMemory),
        });

        assert_eq!(
            result,
            Err(crate::MapBatchResult {
                mapped: 1,
                error: Some(crate::MapError::OutOfMemory),
            })
        );
    }

    #[test]
    fn user_page_allocation_failure_is_not_a_kernel_access_fault() {
        assert_eq!(fault_from_errno(Errno::ENOMEM), FaultOutcome::OutOfMemory);
        assert_eq!(fault_from_errno(Errno::EIO), FaultOutcome::Segv);
    }

    #[test]
    fn user_page_allocation_handle_preserves_exact_buddy_geometry() {
        let allocation = user_page_allocation_handle(0x20_000, PAGE_SIZE)
            .expect("valid user page allocation handle");

        assert_eq!(allocation.paddr, 0x20_000);
        assert_eq!(allocation.size, PAGE_SIZE);
        assert_eq!(allocation.order, 0);
        assert_eq!(allocation.page_size, allocator::PAGE_SIZE);
    }

    struct ChunkedFile {
        bytes: &'static [u8],
        max_chunk: usize,
        eof_at: usize,
    }

    struct CacheMetadataFile {
        size_reads: AtomicUsize,
        generation_reads: AtomicUsize,
    }

    impl FileLike for CacheMetadataFile {
        fn cache_key(&self) -> usize {
            0x1234
        }

        fn private_page_cache_key(&self) -> Option<usize> {
            Some(0x5678)
        }

        fn private_page_cache_generation(&self) -> Option<u64> {
            self.generation_reads.fetch_add(1, Ordering::Relaxed);
            Some(9)
        }

        fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
            Err(Errno::EIO)
        }

        fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, Errno> {
            Err(Errno::EIO)
        }

        fn sync(&self) -> Result<(), Errno> {
            Ok(())
        }

        fn size(&self) -> u64 {
            self.size_reads.fetch_add(1, Ordering::Relaxed);
            (32 * PAGE_SIZE) as u64
        }
    }

    impl FileLike for ChunkedFile {
        fn cache_key(&self) -> usize {
            self as *const Self as usize
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
            let start = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
            let end_limit = self.eof_at.min(self.bytes.len());
            if start >= end_limit {
                return Ok(0);
            }
            let count = buf
                .len()
                .min(self.max_chunk)
                .min(end_limit.saturating_sub(start));
            buf[..count].copy_from_slice(&self.bytes[start..start + count]);
            Ok(count)
        }

        fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, Errno> {
            Err(Errno::EIO)
        }

        fn sync(&self) -> Result<(), Errno> {
            Ok(())
        }

        fn size(&self) -> u64 {
            self.bytes.len() as u64
        }
    }

    struct SharedGenFile {
        generation: AtomicU64,
        provide: bool,
    }

    impl FileLike for SharedGenFile {
        fn cache_key(&self) -> usize {
            self as *const Self as usize
        }

        fn shared_page_cache_generation(&self) -> Option<u64> {
            self.provide
                .then(|| self.generation.load(Ordering::Acquire))
        }

        fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
            Err(Errno::EIO)
        }

        fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, Errno> {
            Err(Errno::EIO)
        }

        fn sync(&self) -> Result<(), Errno> {
            Ok(())
        }

        fn size(&self) -> u64 {
            PAGE_SIZE as u64
        }
    }

    #[test]
    fn shared_file_page_generation_defaults_to_zero_when_signal_absent() {
        let file: Arc<dyn FileLike> = Arc::new(SharedGenFile {
            generation: AtomicU64::new(0),
            provide: false,
        });
        assert_eq!(shared_file_page_generation(&file), 0);
    }

    #[test]
    fn shared_file_page_generation_uses_provided_value() {
        let file: Arc<dyn FileLike> = Arc::new(SharedGenFile {
            generation: AtomicU64::new(7),
            provide: true,
        });
        assert_eq!(shared_file_page_generation(&file), 7);
    }

    #[test]
    fn shared_file_page_cache_misses_after_generation_change() {
        let cache: WeakFilePageCache = Spinlock::new(BTreeMap::new());
        let implementation = Arc::new(SharedGenFile {
            generation: AtomicU64::new(0),
            provide: true,
        });
        let file: Arc<dyn FileLike> = implementation.clone();
        let file_off = 0x1000u64;

        // 代际 0：发布并命中。
        let old_gen = shared_file_page_generation(&file);
        let old_key = FilePageKey::new(&file, file_off, old_gen);
        let page = ResidentPage::new_direct(0x5000);
        publish_cached_file_page(&cache, old_key, Arc::clone(&page));
        assert!(find_cached_file_page(&cache, old_key).is_some());

        // 内容变更前推进代际：旧代际条目仍驻留（等待 Drop 回收），但新代际键不再命中。
        implementation.generation.store(1, Ordering::Release);
        let new_gen = shared_file_page_generation(&file);
        assert_ne!(new_gen, old_gen);
        let new_key = FilePageKey::new(&file, file_off, new_gen);
        assert!(find_cached_file_page(&cache, new_key).is_none());

        // 旧条目仍能按其加载时代际精确删除。
        remove_cached_file_page(&cache, old_key, &page);
        assert!(find_cached_file_page(&cache, old_key).is_none());
        assert_eq!(cache.lock().len(), 0);
    }

    #[test]
    fn private_file_cache_snapshot_reads_stable_metadata_once() {
        let implementation = Arc::new(CacheMetadataFile {
            size_reads: AtomicUsize::new(0),
            generation_reads: AtomicUsize::new(0),
        });
        let file: Arc<dyn FileLike> = implementation.clone();

        let (_, snapshot) = private_file_cache_snapshot(file.as_ref());
        let snapshot = snapshot.expect("stable cache metadata must produce a snapshot");

        assert_eq!(snapshot.file_key, 0x5678);
        assert_eq!(snapshot.generation, 9);
        assert_eq!(snapshot.file_size, (32 * PAGE_SIZE) as u64);
        assert_eq!(implementation.size_reads.load(Ordering::Relaxed), 1);
        assert_eq!(implementation.generation_reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn fault_around_uses_caller_owned_prepare_buffers() {
        let _anon_prepare: for<'a, 'b> fn(
            &'a AnonStoreFaultAround,
            &'b mut PreparedAnonPages,
        ) -> Result<(), Errno> = AnonStoreFaultAround::prepare_into;
        let _file_prepare: for<'a, 'b> fn(
            &'a PrivateFileFaultAround,
            &'b mut PreparedFilePages,
            bool,
        ) -> Result<(), Errno> = PrivateFileFaultAround::prepare_into;
        let _anon_commit: for<'a, 'b, 'c> fn(
            &'a VmSpace,
            &'b AnonStoreFaultAround,
            &'c mut PreparedAnonPages,
        ) -> FaultAroundCommit = VmSpace::commit_anon_store_fault_around;
        let _file_commit: for<'a, 'b, 'c> fn(
            &'a VmSpace,
            &'b PrivateFileFaultAround,
            &'c mut PreparedFilePages,
            bool,
        ) -> FaultAroundCommit = VmSpace::commit_private_file_fault_around;
    }

    #[test]
    fn aligned_file_pages_remain_fully_lazy() {
        let plan = plan_file_segment(0x4000, 0x3000, 0x8000, 0x3000, PAGE_SIZE).unwrap();

        assert_eq!(plan.mapping, 0x4000..0x7000);
        assert_eq!(plan.lazy_file, 0x4000..0x7000);
        assert_eq!(plan.lazy_file_offset, 0x8000);
        assert!(plan.fragments().is_empty());
    }

    #[test]
    fn unaligned_file_and_bss_keep_only_edge_pages_eager() {
        let plan = plan_file_segment(0x4103, 0x4000, 0x103, 0x2400, PAGE_SIZE).unwrap();

        assert_eq!(plan.mapping, 0x4000..0x9000);
        assert_eq!(plan.lazy_file, 0x5000..0x6000);
        assert_eq!(plan.lazy_file_offset, 0x1000);
        assert_eq!(plan.fragments(), &[0x4000, 0x6000]);
    }

    #[test]
    fn short_unaligned_file_can_span_two_fragment_pages() {
        let plan = plan_file_segment(0x4f00, 0x800, 0x2f00, 0x300, PAGE_SIZE).unwrap();

        assert_eq!(plan.mapping, 0x4000..0x6000);
        assert!(plan.lazy_file.is_empty());
        assert_eq!(plan.fragments(), &[0x4000, 0x5000]);
    }

    #[test]
    fn pure_bss_stays_lazy_anonymous() {
        let plan = plan_file_segment(0x4103, 0x2800, 0, 0, PAGE_SIZE).unwrap();

        assert_eq!(plan.mapping, 0x4000..0x7000);
        assert!(plan.lazy_file.is_empty());
        assert!(plan.fragments().is_empty());
    }

    #[test]
    fn invalid_or_overflowing_segments_are_rejected() {
        assert!(plan_file_segment(0x4000, 0x1000, 0, 0x1001, PAGE_SIZE).is_err());
        assert!(plan_file_segment(usize::MAX - 1, 4, 0, 0, PAGE_SIZE).is_err());
        assert!(plan_file_segment(0x4000, 0x1000, u64::MAX, 1, PAGE_SIZE).is_err());
        assert!(plan_file_segment(0x4000, 0x1000, 0, 0, 3).is_err());
    }

    #[test]
    fn file_fault_around_caps_forward_window() {
        let fault = 0x4000;
        let window =
            file_fault_around_window(fault, 0x1000, 0x40_0000, 0x2000, 0x80_0000, PAGE_SIZE)
                .expect("valid file window");

        assert_eq!(window.start, fault);
        assert_eq!(window.file_offset, 0x5000);
        assert_eq!(window.page_count(PAGE_SIZE), FILE_FAULT_AROUND_PAGES);
    }

    fn anon_shadow_key(task_id: u64, task_epoch: u64, vm_id: u64) -> AnonStoreShadowKey {
        AnonStoreShadowKey {
            task_id,
            task_epoch,
            vm_id,
            vma_end: 0x40_0000,
        }
    }

    #[test]
    fn anon_store_shadow_opens_production_window_and_counts_later_faults() {
        let key = anon_shadow_key(7, 11, 13);
        let first =
            observe_anon_store_shadow(AnonStoreShadowState::default(), key, 0x4000, PAGE_SIZE)
                .expect("valid first fault");
        assert!(first.simulated_batch);
        assert!(!first.would_save);
        assert!(!first.reset);
        assert_eq!(first.state.window_start, 0x4000);
        assert_eq!(
            first.state.window_end,
            0x4000 + ANON_STORE_SHADOW_PAGES * PAGE_SIZE
        );

        let hit = observe_anon_store_shadow(first.state, key, 0x6000, PAGE_SIZE)
            .expect("fault inside shadow window");
        assert!(!hit.simulated_batch);
        assert!(hit.would_save);
        assert!(!hit.reset);
        assert_eq!(hit.state, first.state);

        let boundary = observe_anon_store_shadow(hit.state, key, first.state.window_end, PAGE_SIZE)
            .expect("fault at exclusive boundary");
        assert!(boundary.simulated_batch);
        assert!(!boundary.would_save);
    }

    #[test]
    fn anon_store_shadow_caps_window_at_vma_end() {
        let fault = 0x8000;
        let vma_end = fault + 3 * PAGE_SIZE;
        let mut key = anon_shadow_key(7, 11, 13);
        key.vma_end = vma_end;
        let observed =
            observe_anon_store_shadow(AnonStoreShadowState::default(), key, fault, PAGE_SIZE)
                .expect("three-page VMA suffix");

        assert_eq!(observed.state.window_end, vma_end);
    }

    #[test]
    fn anon_store_shadow_resets_on_task_epoch_vm_or_vma_change() {
        let key = anon_shadow_key(7, 11, 13);
        let initial =
            observe_anon_store_shadow(AnonStoreShadowState::default(), key, 0x4000, PAGE_SIZE)
                .unwrap();

        let changed = [
            anon_shadow_key(8, 11, 13),
            anon_shadow_key(7, 12, 13),
            anon_shadow_key(7, 11, 14),
            AnonStoreShadowKey {
                vma_end: key.vma_end - PAGE_SIZE,
                ..key
            },
        ];
        for changed_key in changed {
            let observed =
                observe_anon_store_shadow(initial.state, changed_key, 0x5000, PAGE_SIZE).unwrap();
            assert!(observed.reset);
            assert!(observed.simulated_batch);
            assert!(!observed.would_save);
            assert_eq!(observed.state.key, Some(changed_key));
        }
    }

    #[test]
    fn anon_store_shadow_rejects_invalid_geometry() {
        let key = anon_shadow_key(7, 11, 13);
        let state = AnonStoreShadowState::default();
        assert!(observe_anon_store_shadow(state, key, 0x4001, PAGE_SIZE).is_none());
        assert!(observe_anon_store_shadow(state, key, key.vma_end, PAGE_SIZE).is_none());
        assert!(observe_anon_store_shadow(state, key, 0x4000, 0).is_none());
        assert!(observe_anon_store_shadow(state, key, 0x4000, 3).is_none());
        assert!(
            observe_anon_store_shadow(
                state,
                AnonStoreShadowKey {
                    vma_end: key.vma_end + 1,
                    ..key
                },
                0x4000,
                PAGE_SIZE,
            )
            .is_none()
        );
    }

    #[test]
    fn file_fault_around_stops_at_vma_end() {
        let fault = 0x8000;
        let window = file_fault_around_window(
            fault,
            0x4000,
            fault + 3 * PAGE_SIZE,
            0,
            0x20_0000,
            PAGE_SIZE,
        )
        .expect("VMA contains three pages");

        assert_eq!(window.end, fault + 3 * PAGE_SIZE);
        assert_eq!(window.page_count(PAGE_SIZE), 3);
    }

    #[test]
    fn anon_store_fault_around_caps_forward_window_and_vma_tail() {
        let fault = 0x8000;
        let large = anon_store_fault_around_end(fault, &(0x1000..0x40_0000), PAGE_SIZE)
            .expect("valid anonymous fault window");
        assert_eq!(large, fault + ANON_STORE_FAULT_AROUND_PAGES * PAGE_SIZE);

        let tail_end = fault + 2 * PAGE_SIZE;
        assert_eq!(
            anon_store_fault_around_end(fault, &(0x1000..tail_end), PAGE_SIZE),
            Some(tail_end)
        );
    }

    #[test]
    fn anon_store_fault_around_rejects_invalid_geometry() {
        assert_eq!(
            anon_store_fault_around_end(0x8001, &(0x1000..0x20_000), PAGE_SIZE),
            None
        );
        assert_eq!(
            anon_store_fault_around_end(0x8000, &(0x1001..0x20_000), PAGE_SIZE),
            None
        );
        assert_eq!(
            anon_store_fault_around_end(0x20_000, &(0x1000..0x20_000), PAGE_SIZE),
            None
        );
        assert_eq!(
            anon_store_fault_around_end(0x8000, &(0x1000..0x20_000), 0),
            None
        );
    }

    #[test]
    fn anonymous_fault_snapshot_rejects_fresh_backing_aba() {
        let original = VmBacking::anonymous();
        let split_snapshot = original.clone();
        let fresh = VmBacking::anonymous();

        assert!(same_backing_snapshot(&original, &split_snapshot));
        assert!(!same_backing_snapshot(&original, &fresh));
    }

    #[test]
    fn file_fault_around_keeps_partial_eof_page() {
        let fault = 0x2000;
        let fault_file_offset = PAGE_SIZE as u64;
        let window = file_fault_around_window(
            fault,
            0x1000,
            0x20_0000,
            0,
            fault_file_offset + (2 * PAGE_SIZE) as u64 + 1,
            PAGE_SIZE,
        )
        .expect("partial final page remains faultable");

        assert_eq!(window.file_offset, fault_file_offset);
        assert_eq!(window.page_count(PAGE_SIZE), 3);
    }

    #[test]
    fn file_fault_around_rejects_eof_and_offset_overflow() {
        assert!(
            file_fault_around_window(0x3000, 0x1000, 0x8000, 0, (2 * PAGE_SIZE) as u64, PAGE_SIZE,)
                .is_none()
        );
        assert!(
            file_fault_around_window(0x2000, 0x1000, 0x8000, u64::MAX, u64::MAX, PAGE_SIZE,)
                .is_none()
        );
    }

    #[test]
    fn private_file_batch_caps_at_sixteen_pages() {
        let plan = private_file_batch_plan(0, u64::MAX, 32, 32, PAGE_SIZE)
            .expect("large miss prefix should batch");

        assert_eq!(plan.pages, 16);
        assert_eq!(plan.buffer_len, PRIVATE_FILE_BATCH_MAX_BYTES);
        assert_eq!(plan.read_len, PRIVATE_FILE_BATCH_MAX_BYTES);
    }

    #[test]
    fn private_file_batch_requires_four_consecutive_misses() {
        assert!(private_file_batch_plan(0, u64::MAX, 16, 3, PAGE_SIZE).is_none());
        assert_eq!(
            private_file_batch_plan(0, u64::MAX, 16, 4, PAGE_SIZE)
                .expect("four misses should batch")
                .pages,
            4
        );
    }

    #[test]
    fn private_file_batch_stops_at_partial_eof() {
        let file_offset = 0x2000;
        let remaining = 3 * PAGE_SIZE + 1;
        let plan = private_file_batch_plan(
            file_offset,
            file_offset + remaining as u64,
            16,
            16,
            PAGE_SIZE,
        )
        .expect("partial fourth page should remain batchable");

        assert_eq!(plan.pages, 4);
        assert_eq!(plan.buffer_len, 4 * PAGE_SIZE);
        assert_eq!(plan.read_len, remaining);
    }

    #[test]
    fn private_file_batch_rejects_short_eof_prefix() {
        assert!(private_file_batch_plan(0, (3 * PAGE_SIZE) as u64, 16, 16, PAGE_SIZE).is_none());
        assert!(private_file_batch_plan(0x4000, 0x4000, 16, 16, PAGE_SIZE).is_none());
    }

    #[test]
    fn private_file_batch_obeys_sixty_four_kibibyte_limit() {
        let large_page = 8192;
        let plan = private_file_batch_plan(0, u64::MAX, 16, 16, large_page)
            .expect("eight large pages fit the batch byte limit");

        assert_eq!(plan.pages, 8);
        assert_eq!(plan.buffer_len, PRIVATE_FILE_BATCH_MAX_BYTES);
        assert!(private_file_batch_plan(0, u64::MAX, 16, 16, 1 << 17).is_none());
    }

    #[test]
    fn private_file_batch_page_offset_rejects_overflow() {
        assert_eq!(
            private_file_batch_page_offset(0x2000, 3, PAGE_SIZE),
            Some(0x5000)
        );
        assert!(private_file_batch_page_offset(u64::MAX - 1, 1, PAGE_SIZE).is_none());
        assert!(private_file_batch_page_offset(0, usize::MAX, PAGE_SIZE).is_none());
    }

    #[test]
    fn private_file_batch_only_propagates_fault_page_errors() {
        assert!(private_file_batch_error_is_fatal(0));
        for speculative_index in 1..16 {
            assert!(!private_file_batch_error_is_fatal(speculative_index));
        }
    }

    #[test]
    fn file_fault_around_only_accepts_private_read_faults() {
        let read_only = VmFlags::EMPTY.with(VmFlags::READ);
        let executable = read_only.with(VmFlags::EXEC);
        assert!(permits_file_fault_around(read_only, FaultKind::Load));
        assert!(permits_file_fault_around(executable, FaultKind::Exec));
        assert!(!permits_file_fault_around(read_only, FaultKind::Exec));
        assert!(!permits_file_fault_around(
            read_only.with(VmFlags::WRITE),
            FaultKind::Load
        ));
        assert!(!permits_file_fault_around(
            read_only.with(VmFlags::SHARED),
            FaultKind::Load
        ));
        assert!(!permits_file_fault_around(
            read_only.with(VmFlags::GROWS_DOWN),
            FaultKind::Load
        ));
        assert!(!permits_file_fault_around(read_only, FaultKind::Store));
        assert!(!permits_file_fault_around(read_only, FaultKind::PermRead));
    }

    #[test]
    fn file_fault_around_stops_before_concurrent_mapping() {
        let candidates = [0x1000, 0x2000, 0x3000, 0x4000];

        assert_eq!(
            unmapped_prefix_len(candidates, |address| address == 0x3000),
            2
        );
        assert_eq!(
            unmapped_prefix_len(candidates, |address| address == 0x1000),
            0
        );
        assert_eq!(unmapped_prefix_len(candidates, |_| false), candidates.len());
    }

    #[test]
    fn writable_private_file_page_starts_as_cow() {
        let read_only = VmFlags::EMPTY.with(VmFlags::READ);
        let writable = read_only.with(VmFlags::WRITE);

        assert_eq!(access_for_private_file(read_only), PageAccess::ReadOnly);
        assert_eq!(access_for_private_file(writable), PageAccess::Cow);
    }

    #[test]
    fn file_page_reader_completes_legal_short_reads() {
        let file = ChunkedFile {
            bytes: b"0123456789abcdef",
            max_chunk: 3,
            eof_at: 16,
        };
        let mut output = [0u8; 9];

        read_file_bytes_exact(&file, 2, &mut output).expect("short reads must be retried");

        assert_eq!(&output, b"23456789a");
    }

    #[test]
    fn file_page_reader_rejects_premature_eof() {
        let file = ChunkedFile {
            bytes: b"0123456789abcdef",
            max_chunk: 3,
            eof_at: 6,
        };
        let mut output = [0u8; 8];

        assert_eq!(
            read_file_bytes_exact(&file, 2, &mut output),
            Err(Errno::EIO)
        );
    }

    #[test]
    fn file_page_reader_zeroes_only_partial_eof_tail() {
        let file = ChunkedFile {
            bytes: b"0123456789abcdef",
            max_chunk: 3,
            eof_at: 16,
        };
        let mut output = [0xa5; 8];

        read_file_page_exact(&file, 2, &mut output, 4).expect("partial page must initialize");

        assert_eq!(&output, b"2345\0\0\0\0");
    }

    #[test]
    fn file_page_reader_fills_full_page_without_tail() {
        let file = ChunkedFile {
            bytes: b"0123456789abcdef",
            max_chunk: 3,
            eof_at: 16,
        };
        let mut output = [0xa5; 8];

        read_file_page_exact(&file, 4, &mut output, 8).expect("full page must initialize");

        assert_eq!(&output, b"456789ab");
    }

    #[test]
    fn file_page_reader_rejects_invalid_valid_length() {
        let file = ChunkedFile {
            bytes: b"0123456789abcdef",
            max_chunk: 16,
            eof_at: 16,
        };
        let mut output = [0xa5; 8];

        assert_eq!(
            read_file_page_exact(&file, 0, &mut output, 0),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            read_file_page_exact(&file, 0, &mut output, 9),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn concurrent_file_page_publish_keeps_first_candidate() {
        let cache: WeakFilePageCache = Spinlock::new(BTreeMap::new());
        let key = FilePageKey {
            file_key: 7,
            offset: 0x2000,
            generation: 11,
        };
        let first = ResidentPage::new_direct(0x1000);
        let second = ResidentPage::new_direct(0x2000);

        let published = publish_cached_file_page(&cache, key, Arc::clone(&first));
        let raced = publish_cached_file_page(&cache, key, second);

        assert!(Arc::ptr_eq(&published, &first));
        assert!(Arc::ptr_eq(&raced, &first));
        assert_eq!(cache.lock().len(), 1);
    }

    #[test]
    fn private_file_cache_claim_allocates_ids_only_for_new_loads() {
        let cache = ShardedPrivateFilePageCache::<1>::new(2);
        let key = cache_key(51);
        let load_id = match cache.claim(key) {
            PrivateFilePageCacheClaim::Owner(load_id) => load_id,
            _ => panic!("vacant key must create a load owner"),
        };
        let owner_diag = cache.diag();
        assert_eq!(owner_diag.misses, 1);
        assert_eq!(owner_diag.load_leaders, 1);
        assert_eq!(owner_diag.hits, 0);
        let next_after_owner = cache.next_load_id.load(Ordering::Relaxed);

        let waiter = match cache.claim(key) {
            PrivateFilePageCacheClaim::Loading(waiter) => waiter,
            _ => panic!("second claim must observe the active load"),
        };
        let waiter_diag = cache.diag();
        assert_eq!(waiter_diag.misses, 2);
        assert_eq!(waiter_diag.load_leaders, 1);
        assert_eq!(waiter_diag.hits, 0);
        assert_eq!(waiter.id(), load_id);
        assert_eq!(cache.next_load_id.load(Ordering::Relaxed), next_after_owner);
        drop(waiter);

        let page = ResidentPage::new_direct(0x51000);
        cache
            .finish_load(key, load_id, Arc::clone(&page))
            .expect("owner publishes ready page");
        assert!(matches!(
            cache.claim(key),
            PrivateFilePageCacheClaim::Ready(_)
        ));
        let ready_diag = cache.diag();
        assert_eq!(ready_diag.misses, 2);
        assert_eq!(ready_diag.load_leaders, 1);
        assert_eq!(ready_diag.hits, 1);
        assert_eq!(cache.next_load_id.load(Ordering::Relaxed), next_after_owner);
    }

    #[test]
    fn private_file_cache_batch_claim_preserves_key_order() {
        let cache = ShardedPrivateFilePageCache::<4>::new(16);
        let keys = [
            cache_key_for_shard(&cache, 3, 0),
            cache_key_for_shard(&cache, 0, 0),
            cache_key_for_shard(&cache, 3, 1),
            cache_key_for_shard(&cache, 1, 0),
        ];

        let claims = cache.claim_batch(&keys);
        assert_eq!(claims.len(), keys.len());
        for (key, claim) in keys.into_iter().zip(claims) {
            let PrivateFilePageCacheClaim::Owner(load_id) = claim else {
                panic!("vacant batch key must create a load owner");
            };
            assert!(cache.load_pending(key, load_id));
            cache.abort_load(key, load_id, None);
        }
    }

    #[test]
    fn private_file_cache_batch_claim_stops_after_first_non_owner() {
        let cache = ShardedPrivateFilePageCache::<1>::new(8);
        let keys = [cache_key(61), cache_key(62), cache_key(63)];
        let second_load = match cache.claim(keys[1]) {
            PrivateFilePageCacheClaim::Owner(load_id) => load_id,
            _ => panic!("second key must start as an owner"),
        };

        let claims = cache.claim_batch_prefix(&keys);
        assert_eq!(claims.len(), 2);
        assert!(matches!(&claims[0], PrivateFilePageCacheClaim::Owner(_)));
        assert!(matches!(&claims[1], PrivateFilePageCacheClaim::Loading(_)));
        assert!(cache.shards[0].lock().pages.get(&keys[2]).is_none());

        let first_load = match &claims[0] {
            PrivateFilePageCacheClaim::Owner(load_id) => *load_id,
            _ => unreachable!(),
        };
        drop(claims);
        cache.abort_load(keys[0], first_load, None);
        cache.abort_load(keys[1], second_load, None);
    }

    #[test]
    fn private_file_cache_contiguous_claim_returns_owner_prefix() {
        let cache = ShardedPrivateFilePageCache::<1>::new(8);
        let first = cache_key(71);
        let keys = [
            first,
            FilePageKey {
                offset: first.offset + PAGE_SIZE as u64,
                ..first
            },
            FilePageKey {
                offset: first.offset + (2 * PAGE_SIZE) as u64,
                ..first
            },
        ];
        let competing_load = match cache.claim(keys[1]) {
            PrivateFilePageCacheClaim::Owner(load_id) => load_id,
            _ => panic!("第二页必须先进入加载状态"),
        };

        let prefix = cache
            .claim_contiguous_prefix(first, PAGE_SIZE, keys.len())
            .expect("连续页偏移必须有效");
        assert_eq!(prefix.owners.len(), 1);
        assert_eq!(prefix.owners[0].0, keys[0]);
        assert!(matches!(
            &prefix.terminal,
            Some((1, PrivateFilePageCacheClaim::Loading(_)))
        ));
        assert!(cache.shards[0].lock().pages.get(&keys[2]).is_none());

        let first_load = prefix.owners[0].1;
        drop(prefix);
        cache.abort_load(keys[0], first_load, None);
        cache.abort_load(keys[1], competing_load, None);
    }

    #[test]
    fn private_file_cache_ready_range_returns_all_pages_without_claiming_loads() {
        let cache = ShardedPrivateFilePageCache::<1>::new(8);
        let first = cache_key(81);
        let keys = [
            first,
            FilePageKey {
                offset: first.offset + PAGE_SIZE as u64,
                ..first
            },
            FilePageKey {
                offset: first.offset + (2 * PAGE_SIZE) as u64,
                ..first
            },
        ];
        let pages = [
            ResidentPage::new_direct(0x81000),
            ResidentPage::new_direct(0x82000),
            ResidentPage::new_direct(0x83000),
        ];
        for (key, page) in keys.into_iter().zip(pages) {
            let load_id = match cache.claim(key) {
                PrivateFilePageCacheClaim::Owner(load_id) => load_id,
                _ => panic!("首次发布必须取得 owner"),
            };
            cache
                .finish_load(key, load_id, page)
                .expect("owner 必须发布 ready 页");
        }

        let ready = cache
            .ready_contiguous(first, PAGE_SIZE, keys.len())
            .expect("连续 ready 页必须走范围命中");
        assert_eq!(ready.len(), keys.len());
        assert_eq!(ready[0].paddr(), 0x81000);
        assert_eq!(ready[1].paddr(), 0x82000);
        assert_eq!(ready[2].paddr(), 0x83000);
        assert_eq!(cache.diag().hits, keys.len() as u64);
        assert_eq!(cache.diag().load_leaders, keys.len() as u64);
    }

    #[test]
    fn private_file_cache_ready_range_crosses_chunk_shards() {
        let cache = ShardedPrivateFilePageCache::<32>::new(128);
        let first = (0x1000..0x2000)
            .map(|file_key| FilePageKey {
                file_key,
                offset: super::PRIVATE_FILE_CACHE_SHARD_CHUNK_BYTES - PAGE_SIZE as u64,
                generation: 1,
            })
            .find(|key| {
                cache.shard_index(*key)
                    != cache.shard_index(FilePageKey {
                        offset: key.offset + PAGE_SIZE as u64,
                        ..*key
                    })
            })
            .expect("测试键必须跨越两个缓存分片");
        let keys = [
            first,
            FilePageKey {
                offset: first.offset + PAGE_SIZE as u64,
                ..first
            },
        ];

        for (key, paddr) in keys.into_iter().zip([0x91000, 0x92000]) {
            let load_id = match cache.claim(key) {
                PrivateFilePageCacheClaim::Owner(load_id) => load_id,
                _ => panic!("首次发布必须取得 owner"),
            };
            cache
                .finish_load(key, load_id, ResidentPage::new_direct(paddr))
                .expect("owner 必须发布 ready 页");
        }

        let ready = cache
            .ready_contiguous(first, PAGE_SIZE, keys.len())
            .expect("跨分片的连续 ready 页必须完整命中");
        assert_eq!(ready.len(), keys.len());
        assert_eq!(ready[0].paddr(), 0x91000);
        assert_eq!(ready[1].paddr(), 0x92000);
    }

    #[test]
    fn private_file_cache_ready_range_stops_at_miss_without_claiming_it() {
        let cache = ShardedPrivateFilePageCache::<1>::new(8);
        let first = cache_key(82);
        let keys = [
            first,
            FilePageKey {
                offset: first.offset + PAGE_SIZE as u64,
                ..first
            },
            FilePageKey {
                offset: first.offset + (2 * PAGE_SIZE) as u64,
                ..first
            },
        ];
        for (key, paddr) in [(keys[0], 0xa1000), (keys[2], 0xa3000)] {
            let load_id = match cache.claim(key) {
                PrivateFilePageCacheClaim::Owner(load_id) => load_id,
                _ => panic!("首次发布必须取得 owner"),
            };
            cache
                .finish_load(key, load_id, ResidentPage::new_direct(paddr))
                .expect("owner 必须发布 ready 页");
        }

        let ready = cache
            .ready_contiguous(first, PAGE_SIZE, keys.len())
            .expect("首个 ready 页必须作为连续前缀返回");
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].paddr(), 0xa1000);
        assert_eq!(cache.diag().load_leaders, 2);

        let load_id = match cache.claim(keys[1]) {
            PrivateFilePageCacheClaim::Owner(load_id) => load_id,
            _ => panic!("范围查询不能提前占有 miss 页"),
        };
        cache.abort_load(keys[1], load_id, None);
    }

    #[test]
    fn private_file_cache_batch_rollback_uses_owner_order() {
        let cache = ShardedPrivateFilePageCache::<1>::new(8);
        let keys = [cache_key(73), cache_key(74)];
        let mut owners = PrivateFilePageLoadOwners::new();
        let mut pages = PrivateFilePageBatch::new();

        for (index, key) in keys.into_iter().enumerate() {
            let load_id = match cache.claim(key) {
                PrivateFilePageCacheClaim::Owner(load_id) => load_id,
                _ => panic!("空键必须由当前批次持有"),
            };
            let candidate = ResidentPage::new_direct((index + 1) * PAGE_SIZE);
            let page = cache
                .finish_load(key, load_id, candidate)
                .expect("当前 owner 必须能发布页面");
            owners.push((key, load_id));
            pages.push(page);
        }

        rollback_private_file_page_batch(&cache, &owners, &pages);
        for key in keys {
            assert!(find_cached_private_file_page(&cache, key).is_none());
        }
    }

    #[test]
    fn private_file_cache_waiters_share_one_stable_load_error() {
        let cache = ShardedPrivateFilePageCache::<1>::new(2);
        let key = cache_key(53);
        let load_id = match cache.claim(key) {
            PrivateFilePageCacheClaim::Owner(load_id) => load_id,
            _ => panic!("vacant key must create a load owner"),
        };
        let first = match cache.claim(key) {
            PrivateFilePageCacheClaim::Loading(waiter) => waiter,
            _ => panic!("first waiter must register on active load"),
        };
        let second = match cache.claim(key) {
            PrivateFilePageCacheClaim::Loading(waiter) => waiter,
            _ => panic!("second waiter must register on active load"),
        };

        cache.abort_load(key, load_id, Some(Errno::EIO));
        assert!(matches!(first.wait(), Err(Errno::EIO)));
        assert!(matches!(
            cache.claim(key),
            PrivateFilePageCacheClaim::Failed(Errno::EIO)
        ));
        assert!(matches!(second.wait(), Err(Errno::EIO)));
        assert!(!cache.shards[0].lock().pages.contains_key(&key));
        assert_eq!(cache.diag().load_errors, 1);
    }

    #[test]
    fn private_file_cache_dropped_waiter_cancels_failed_result_slot() {
        let cache = ShardedPrivateFilePageCache::<1>::new(2);
        let key = cache_key(55);
        let load_id = match cache.claim(key) {
            PrivateFilePageCacheClaim::Owner(load_id) => load_id,
            _ => panic!("vacant key must create a load owner"),
        };
        let waiter = match cache.claim(key) {
            PrivateFilePageCacheClaim::Loading(waiter) => waiter,
            _ => panic!("waiter must register on active load"),
        };
        cache.abort_load(key, load_id, Some(Errno::EIO));
        assert!(matches!(
            cache.shards[0].lock().pages.get(&key),
            Some(PrivateFilePageCacheEntry::Failed { .. })
        ));
        drop(waiter);

        assert!(!cache.shards[0].lock().pages.contains_key(&key));
    }

    #[test]
    fn private_file_cache_load_ids_never_wrap_into_aba() {
        let cache = ShardedPrivateFilePageCache::<1>::new(2);
        cache.next_load_id.store(u64::MAX, Ordering::Relaxed);
        let key = cache_key(57);

        assert!(matches!(
            cache.claim(key),
            PrivateFilePageCacheClaim::Bypass
        ));
        assert!(!matches!(
            cache.shards[0].lock().pages.get(&key),
            Some(PrivateFilePageCacheEntry::Loading { .. })
        ));
    }

    #[test]
    fn private_file_cache_retains_pages_until_bounded_eviction() {
        let cache = ShardedPrivateFilePageCache::<1>::new(2);
        let keys = [cache_key(1), cache_key(2), cache_key(3)];
        let first = ResidentPage::new_direct(0x1000);
        let first_weak = Arc::downgrade(&first);

        drop(publish_cached_private_file_page(&cache, keys[0], first));
        assert!(first_weak.upgrade().is_some());
        drop(publish_cached_private_file_page(
            &cache,
            keys[1],
            ResidentPage::new_direct(0x2000),
        ));
        drop(publish_cached_private_file_page(
            &cache,
            keys[2],
            ResidentPage::new_direct(0x3000),
        ));

        assert_eq!(cache.diag().pages, 2);
        assert!(first_weak.upgrade().is_none());
        assert!(find_cached_private_file_page(&cache, keys[1]).is_some());
        assert!(find_cached_private_file_page(&cache, keys[2]).is_some());
    }

    #[test]
    fn private_file_cache_clock_preserves_a_recent_hit() {
        let cache = ShardedPrivateFilePageCache::<1>::new(2);
        let keys = [cache_key(1), cache_key(2), cache_key(3), cache_key(4)];

        for (index, key) in keys[..3].iter().enumerate() {
            drop(publish_cached_private_file_page(
                &cache,
                *key,
                ResidentPage::new_direct((index + 1) * PAGE_SIZE),
            ));
        }
        // 第一次超限淘汰 key 1，并清除了其余条目的 reference 位。
        drop(find_cached_private_file_page(&cache, keys[1]));
        drop(publish_cached_private_file_page(
            &cache,
            keys[3],
            ResidentPage::new_direct(4 * PAGE_SIZE),
        ));

        assert!(find_cached_private_file_page(&cache, keys[1]).is_some());
        assert!(find_cached_private_file_page(&cache, keys[2]).is_none());
        assert!(find_cached_private_file_page(&cache, keys[3]).is_some());
        assert_eq!(cache.diag().pages, 2);
    }

    #[test]
    fn private_file_cache_enforces_total_capacity_across_shards() {
        let cache = ShardedPrivateFilePageCache::<4>::new(7);
        for shard in 0..4 {
            for ordinal in 0..4 {
                let key = cache_key_for_shard(&cache, shard, ordinal);
                drop(publish_cached_private_file_page(
                    &cache,
                    key,
                    ResidentPage::new_direct((shard * 4 + ordinal + 1) * PAGE_SIZE),
                ));
            }
        }

        let diag = cache.diag();
        assert_eq!(diag.capacity, 7);
        assert_eq!(diag.pages, 7);
        assert_eq!(diag.evictions, 9);
        assert_eq!(cache.shards[0].lock().pages.len(), 2);
        assert_eq!(cache.shards[1].lock().pages.len(), 2);
        assert_eq!(cache.shards[2].lock().pages.len(), 2);
        assert_eq!(cache.shards[3].lock().pages.len(), 1);
    }

    #[test]
    fn private_file_cache_hash_uses_complete_stable_key() {
        let cache = ShardedPrivateFilePageCache::<8>::new(16);
        let key = FilePageKey {
            file_key: 0x1234,
            offset: 0x5678,
            generation: 0x9abc,
        };
        let hashes = key.private_cache_hashes();

        assert_eq!(cache.shard_index(key), cache.shard_index(key));
        assert_eq!(hashes.table, key.private_table_hash());
        assert_ne!(
            key.private_cache_hash(),
            FilePageKey {
                file_key: key.file_key + 1,
                ..key
            }
            .private_cache_hash()
        );
        assert_ne!(
            key.private_cache_hash(),
            FilePageKey {
                offset: key.offset + 1,
                ..key
            }
            .private_cache_hash()
        );
        assert_ne!(
            key.private_cache_hash(),
            FilePageKey {
                generation: key.generation + 1,
                ..key
            }
            .private_cache_hash()
        );
    }

    #[test]
    fn private_file_cache_table_hash_decorrelates_shard_bits() {
        let cache = ShardedPrivateFilePageCache::<8>::new(16);
        let mut hashes = [0u64; 8];
        let mut found = 0usize;
        for page_index in 0..1024u64 {
            let key = FilePageKey {
                file_key: 0x1234,
                offset: page_index * PAGE_SIZE as u64,
                generation: 7,
            };
            if cache.shard_index(key) != 3 {
                continue;
            }
            hashes[found] = key.private_table_hash();
            found += 1;
            if found == hashes.len() {
                break;
            }
        }
        assert_eq!(found, hashes.len());

        let distinct_bucket_prefixes = hashes
            .iter()
            .enumerate()
            .filter(|(index, hash)| {
                !hashes[..*index]
                    .iter()
                    .any(|seen| seen & 0xff == **hash & 0xff)
            })
            .count();
        let distinct_control_tags = hashes
            .iter()
            .enumerate()
            .filter(|(index, hash)| {
                !hashes[..*index]
                    .iter()
                    .any(|seen| seen >> 57 == **hash >> 57)
            })
            .count();
        assert!(distinct_bucket_prefixes >= 6);
        assert!(distinct_control_tags >= 4);
    }

    #[test]
    fn private_file_cache_stale_publish_removes_only_matching_page() {
        let cache = ShardedPrivateFilePageCache::<2>::new(4);
        let key = cache_key(41);
        let first = ResidentPage::new_direct(0x1000);
        let second = ResidentPage::new_direct(0x2000);
        drop(publish_cached_private_file_page(
            &cache,
            key,
            Arc::clone(&first),
        ));

        cache.remove_if_same(key, &second);
        assert!(find_cached_private_file_page(&cache, key).is_some());
        cache.remove_if_same(key, &first);
        assert!(find_cached_private_file_page(&cache, key).is_none());
        assert!(cache.shards[cache.shard_index(key)].lock().clock.is_empty());
    }

    #[test]
    fn private_file_cache_repeated_stale_publish_keeps_clock_bounded() {
        let cache = ShardedPrivateFilePageCache::<1>::new(2);
        let key = cache_key(43);

        for address in 1..=64 {
            let page = ResidentPage::new_direct(address * PAGE_SIZE);
            drop(publish_cached_private_file_page(
                &cache,
                key,
                Arc::clone(&page),
            ));
            cache.remove_if_same(key, &page);
        }

        let shard = cache.shards[0].lock();
        assert!(shard.pages.is_empty());
        assert!(shard.clock.is_empty());
    }

    #[test]
    fn private_file_cache_pressure_reclaim_rotates_shards() {
        let cache = ShardedPrivateFilePageCache::<2>::new(4);
        let keys = [
            cache_key_for_shard(&cache, 0, 0),
            cache_key_for_shard(&cache, 0, 1),
            cache_key_for_shard(&cache, 1, 0),
            cache_key_for_shard(&cache, 1, 1),
        ];
        for (index, key) in keys.iter().enumerate() {
            drop(publish_cached_private_file_page(
                &cache,
                *key,
                ResidentPage::new_direct((index + 1) * PAGE_SIZE),
            ));
        }

        assert_eq!(cache.reclaim(1), 1);
        assert_eq!(cache.shards[0].lock().pages.len(), 1);
        assert_eq!(cache.shards[1].lock().pages.len(), 2);
        assert_eq!(cache.reclaim(1), 1);
        assert_eq!(cache.shards[0].lock().pages.len(), 1);
        assert_eq!(cache.shards[1].lock().pages.len(), 1);

        let diag = cache.diag();
        assert_eq!(diag.pages, 2);
        assert_eq!(diag.evictions, 2);
        assert_eq!(diag.pressure_reclaims, 2);
        assert!(find_cached_private_file_page(&cache, keys[0]).is_none());
        assert!(find_cached_private_file_page(&cache, keys[2]).is_none());
    }

    fn cache_key(file_key: usize) -> FilePageKey {
        FilePageKey {
            file_key,
            offset: 0,
            generation: 1,
        }
    }

    fn cache_key_for_shard<const SHARD_COUNT: usize>(
        cache: &ShardedPrivateFilePageCache<SHARD_COUNT>,
        shard: usize,
        ordinal: usize,
    ) -> FilePageKey {
        let mut offset = ((shard * 64 + ordinal) as u64) << 32;
        loop {
            let key = FilePageKey {
                file_key: 0x1000 + shard,
                offset,
                generation: ordinal as u64 + 1,
            };
            if cache.shard_index(key) == shard {
                return key;
            }
            offset += PAGE_SIZE as u64;
        }
    }
}

//! VmSpace —— 进程地址空间的顶层对象。
//!
//! `VmSpace` 负责把纯 VMA 代数、用户页表 ops、用户数据页生命周期三件事收束在
//! general 层。arch 只提供页表机械动作，COW / `MAP_SHARED` / 脏页写回这些策略
//! 都在这里处理，避免未来把 MM 逻辑散到具体架构里。

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::Range;
#[cfg(feature = "performance-profile")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use errno::Errno;
use mm::{FileLike, SharedAnonObject, VmArea, VmBacking, VmFlags, VmaSet};
use sched::sync::Spinlock;

use crate::mm::fault::{FaultKind, FaultOutcome, KernelFaultReason};
use crate::mm::ops::{PgdHandle, UserPteUpdate, UserVmLayoutOps, user_pgd_ops, user_vm_layout};

/// 顺序只读文件缺页一次最多预装的页数（包含硬件实际命中的页）。
///
/// BuildStorm 会反复执行体积较大的 rustc/链接器映像；适度预装可减少 TCG 下的
/// 硬件缺页陷入，同时避免冷缓存首次缺页同步读取过多无关页面。
const FILE_FAULT_AROUND_PAGES: usize = 16;
/// 内容持续变化时最多尝试发布缓存快照的次数，随后退回不缓存读取保证前进性。
const PRIVATE_FILE_CACHE_RETRIES: usize = 3;
/// 私有干净文件页的强缓存上限；在 4 KiB 页配置下约为 512 MiB。
///
/// BuildStorm 的工具链和 crate 工作集明显超过 128 MiB；保留更大的有界热集可
/// 避免在仍有数 GiB 空闲内存时反复从 ext4 重读同一页。物理页分配失败仍会按
/// 批次回收，因此该预算不会阻塞匿名页和 COW 分配的前进性。
const PRIVATE_FILE_CACHE_MAX_PAGES: usize = 131_072;
/// 独立的私有文件页缓存分片数；32 个分片可覆盖 BuildStorm 的并行 rustc 缺页。
const PRIVATE_FILE_CACHE_SHARD_COUNT: usize = 32;
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

#[inline]
fn vm_layout() -> &'static UserVmLayoutOps {
    user_vm_layout().expect("[mm] user_vm_layout_ops not registered")
}

/// 当前架构注入的用户页粒度。
#[kernel_symbols::export(name = "general.mm.page_size", contract = "kernel.mm.query@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
pub fn page_size() -> usize {
    vm_layout().page_size
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

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FaultAroundDiag {
    pub windows: u64,
    pub requested_pages: u64,
    pub prepared_pages: u64,
    pub commits: u64,
    pub installed_pages: u64,
    pub raced_commits: u64,
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

pub(crate) fn fault_around_diag() -> FaultAroundDiag {
    let mut diag = FaultAroundDiag::default();
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
}

#[kernel_symbols::export(name = "general.mm.vm_space_diag", contract = "kernel.mm.diagnostic@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC)]
pub fn vm_space_diag() -> VmSpaceDiag {
    VmSpaceDiag {
        live: VM_SPACE_LIVE.load(Ordering::Acquire),
        created: VM_SPACE_CREATED.load(Ordering::Acquire),
        dropped: VM_SPACE_DROPPED.load(Ordering::Acquire),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FilePageKey {
    file_key: usize,
    offset: u64,
    generation: u64,
}

struct PrivateFilePageCacheEntry {
    page: Arc<ResidentPage>,
    referenced: bool,
}

/// 单个私有文件页缓存分片。
///
/// `pages` 提供按文件代际和偏移查找，`clock` 实现 second-chance 淘汰。缓存只
/// 持有固定数量的强引用，使短生命周期编译进程退出后仍可复用工具链和 crate 页，
/// 同时避免长期构建把所有历史文件内容永久钉在内存中。
struct PrivateFilePageCacheState {
    pages: BTreeMap<FilePageKey, PrivateFilePageCacheEntry>,
    clock: VecDeque<FilePageKey>,
    hits: u64,
    misses: u64,
    evictions: u64,
    pressure_reclaims: u64,
}

/// 有界的私有干净文件页强缓存。
///
/// 完整的文件身份、偏移和代际经过稳定混合后选择分片，使不同 rustc 进程的并行
/// 缺页通常只竞争各自分片。容量按分片精确拆分，压力回收则轮换起始分片。
struct ShardedPrivateFilePageCache<const SHARD_COUNT: usize> {
    shards: [Spinlock<PrivateFilePageCacheState>; SHARD_COUNT],
    capacity: usize,
    reclaim_shard: AtomicUsize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PrivateFilePageCacheDiag {
    pub pages: usize,
    pub capacity: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub pressure_reclaims: u64,
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
}

impl PrivateFilePageCacheState {
    const fn new() -> Self {
        Self {
            pages: BTreeMap::new(),
            clock: VecDeque::new(),
            hits: 0,
            misses: 0,
            evictions: 0,
            pressure_reclaims: 0,
        }
    }

    fn find(&mut self, key: FilePageKey) -> Option<Arc<ResidentPage>> {
        let page = self.find_existing(key);
        if page.is_some() {
            self.hits = self.hits.saturating_add(1);
        } else {
            self.misses = self.misses.saturating_add(1);
        }
        page
    }

    fn find_existing(&mut self, key: FilePageKey) -> Option<Arc<ResidentPage>> {
        let entry = self.pages.get_mut(&key)?;
        entry.referenced = true;
        Some(Arc::clone(&entry.page))
    }

    /// 插入一个此前不存在的条目，并返回需要在锁外释放的淘汰页。
    fn insert(
        &mut self,
        key: FilePageKey,
        page: &Arc<ResidentPage>,
        capacity: usize,
    ) -> Option<Arc<ResidentPage>> {
        use alloc::collections::btree_map::Entry;

        let Entry::Vacant(slot) = self.pages.entry(key) else {
            return None;
        };
        slot.insert(PrivateFilePageCacheEntry {
            page: Arc::clone(page),
            // 插入本身不算跨地址空间复用；只有后续缓存命中才获得 second chance。
            referenced: false,
        });
        self.clock.push_back(key);
        (self.pages.len() > capacity)
            .then(|| self.evict_one())
            .flatten()
    }

    /// 清理一个未被近期访问的页。调用者必须在释放返回的 Arc 前放开缓存锁。
    fn evict_one(&mut self) -> Option<Arc<ResidentPage>> {
        // second chance 只近似表达近期复用。分片缺页锁内必须保持固定上界，
        // 即使整个缓存都很热也不能扫描数万棵 BTree 节点。
        let scans = self.clock.len().min(PRIVATE_FILE_CACHE_EVICTION_SCAN_LIMIT);
        for _ in 0..scans {
            let Some(key) = self.clock.pop_front() else {
                break;
            };
            let Some(entry) = self.pages.get_mut(&key) else {
                // 仅用于容忍测试/恢复路径留下的旧 clock 节点。
                continue;
            };
            if entry.referenced {
                entry.referenced = false;
                self.clock.push_back(key);
                continue;
            }
            // `remove` 把 Arc 移到锁外；不要让 map entry 在锁守卫仍存活时析构。
            return self.remove(key);
        }

        // 所有受检条目都获得了 second chance 时，固定淘汰下一个最老条目；
        // 容量不变量和缺页前进性比精确 LRU 更重要。
        self.evict_oldest()
    }

    /// 无视 reference 位移除最老条目，供容量兜底和内存压力回收使用。
    fn evict_oldest(&mut self) -> Option<Arc<ResidentPage>> {
        while let Some(key) = self.clock.pop_front() {
            if let Some(page) = self.remove(key) {
                return Some(page);
            }
        }
        // clock 元数据若意外缺项，仍保证 map 不会永久失去可回收性。
        let key = *self.pages.keys().next()?;
        self.remove(key)
    }

    fn reclaim_oldest(&mut self) -> Option<Arc<ResidentPage>> {
        let page = self.evict_oldest()?;
        self.pressure_reclaims = self.pressure_reclaims.saturating_add(1);
        Some(page)
    }

    fn remove(&mut self, key: FilePageKey) -> Option<Arc<ResidentPage>> {
        let page = self.pages.remove(&key).map(|entry| entry.page)?;
        self.evictions = self.evictions.saturating_add(1);
        Some(page)
    }

    fn remove_if_same(
        &mut self,
        key: FilePageKey,
        page: &ResidentPage,
    ) -> Option<Arc<ResidentPage>> {
        let same = self
            .pages
            .get(&key)
            .is_some_and(|entry| core::ptr::eq(entry.page.as_ref(), page));
        if !same {
            return None;
        }

        // 代际校验失败会走这里主动撤销刚发布的候选。同步摘除 clock 节点，避免
        // 文件反复变化时累积陈旧 key，最终让压力回收在分片锁内无界扫描。
        self.clock.retain(|queued| *queued != key);
        self.remove(key)
    }
}

impl<const SHARD_COUNT: usize> ShardedPrivateFilePageCache<SHARD_COUNT> {
    const fn new(capacity: usize) -> Self {
        assert!(SHARD_COUNT > 0);
        assert!(SHARD_COUNT.is_power_of_two());
        Self {
            shards: [const { Spinlock::new(PrivateFilePageCacheState::new()) }; SHARD_COUNT],
            capacity,
            reclaim_shard: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn shard_index(&self, key: FilePageKey) -> usize {
        (key.private_cache_hash() as usize) & (SHARD_COUNT - 1)
    }

    fn shard_capacity(&self, index: usize) -> usize {
        self.capacity / SHARD_COUNT + usize::from(index < self.capacity % SHARD_COUNT)
    }

    fn find(&self, key: FilePageKey) -> Option<Arc<ResidentPage>> {
        self.shards[self.shard_index(key)].lock().find(key)
    }

    /// 锁内只发布候选并摘取一个淘汰页，候选或淘汰页均在锁外析构。
    fn publish(&self, key: FilePageKey, candidate: Arc<ResidentPage>) -> Arc<ResidentPage> {
        let index = self.shard_index(key);
        let (existing, retired) = {
            let mut shard = self.shards[index].lock();
            if let Some(existing) = shard.find_existing(key) {
                (Some(existing), None)
            } else {
                let retired = shard.insert(key, &candidate, self.shard_capacity(index));
                (None, retired)
            }
        };
        drop(retired);
        if let Some(existing) = existing {
            drop(candidate);
            existing
        } else {
            candidate
        }
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
            diag.pages = diag.pages.saturating_add(shard.pages.len());
            diag.hits = diag.hits.saturating_add(shard.hits);
            diag.misses = diag.misses.saturating_add(shard.misses);
            diag.evictions = diag.evictions.saturating_add(shard.evictions);
            diag.pressure_reclaims = diag
                .pressure_reclaims
                .saturating_add(shard.pressure_reclaims);
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

struct PrivateFileFaultAround {
    fault_page: usize,
    end: usize,
    area_range: Range<usize>,
    area_file_offset: u64,
    fault_file_offset: u64,
    flags: VmFlags,
    file: Arc<dyn FileLike>,
}

struct PreparedFilePage {
    vaddr: usize,
    page: Arc<ResidentPage>,
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

    fn new_shared_file(paddr: usize, file: Arc<dyn FileLike>, offset: u64) -> Arc<Self> {
        Arc::new(Self {
            paddr,
            kind: ResidentPageKind::SharedFile { file, offset },
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
        let ResidentPageKind::SharedFile { file, offset } = &self.kind else {
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
        file.sync()
    }
}

impl Drop for ResidentPage {
    fn drop(&mut self) {
        match &self.kind {
            ResidentPageKind::SharedFile { file, offset } => {
                remove_cached_file_page(
                    &SHARED_FILE_PAGES,
                    FilePageKey::new(file, *offset, 0),
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
        let window = file_fault_around_window(
            fault_page,
            area_range.start,
            area_range.end,
            *offset,
            file.size(),
            page_size(),
        )?;
        Some(Self {
            fault_page: window.start,
            end: window.end,
            area_range,
            area_file_offset: *offset,
            fault_file_offset: window.file_offset,
            flags,
            file: Arc::clone(file),
        })
    }

    /// 在不持有 VMA/pages 锁时分配并读取连续候选页。
    ///
    /// 故障页失败沿用普通 fault 的错误；邻页属于投机行为，首次失败即缩短窗口。
    fn prepare(&self) -> Result<Vec<PreparedFilePage>, Errno> {
        let page_size = page_size();
        let pages = (self.end - self.fault_page) / page_size;
        let mut prepared = Vec::with_capacity(pages);
        for index in 0..pages {
            let delta = index.checked_mul(page_size).ok_or(Errno::EINVAL)?;
            let vaddr = self.fault_page.checked_add(delta).ok_or(Errno::EINVAL)?;
            let file_offset = self
                .fault_file_offset
                .checked_add(u64::try_from(delta).map_err(|_| Errno::EINVAL)?)
                .ok_or(Errno::EINVAL)?;
            match private_file_page(&self.file, file_offset) {
                Ok(page) => prepared.push(PreparedFilePage { vaddr, page }),
                Err(err) if index == 0 => return Err(err),
                Err(_) => break,
            }
        }
        #[cfg(feature = "performance-profile")]
        record_fault_around_prepare(pages, prepared.len());
        Ok(prepared)
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

/// 进程地址空间。
pub struct VmSpace {
    vmas: Spinlock<VmaSet>,
    pages: Spinlock<BTreeMap<usize, PageMapping>>,
    pgd: PgdHandle,
    brk_start: AtomicUsize,
    brk_current: AtomicUsize,
    mmap_next: AtomicUsize,
    mlock_future: AtomicBool,
    /// `membarrier(2)` expedited 命令的地址空间级注册位。
    membarrier_registration: AtomicUsize,
    /// 诊断辅助：记录当前已建立页表映射的用户页数。
    mapped_pages: AtomicUsize,
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
            pages: Spinlock::new(BTreeMap::new()),
            pgd,
            brk_start: AtomicUsize::new(layout.user_heap_base),
            brk_current: AtomicUsize::new(layout.user_heap_base),
            mmap_next: AtomicUsize::new(layout.user_mmap_base),
            mlock_future: AtomicBool::new(false),
            membarrier_registration: AtomicUsize::new(0),
            mapped_pages: AtomicUsize::new(0),
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
            range,
            flags: flags.with(VmFlags::ANON),
            backing,
        };
        self.vmas.lock().insert(area)
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
            vmas.insert(area)?;
            if shared_writable {
                mapped_file.disable_private_page_cache();
            }
        }
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
        let removed_areas = {
            let mut vmas = self.vmas.lock();
            let removed_areas = vmas.unmap_range(&range);
            if let Err(err) = vmas.insert(area) {
                drop(vmas);
                Self::notify_file_unmapped(&removed_areas);
                return Err(err);
            }
            removed_areas
        };
        Self::notify_file_unmapped(&removed_areas);
        let removed = self.unmap_page_mappings(range.clone())?;
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
        let removed_areas = {
            let mut vmas = self.vmas.lock();
            let removed_areas = vmas.unmap_range(&range);
            if let Err(err) = vmas.insert(area) {
                drop(vmas);
                Self::notify_file_unmapped(&removed_areas);
                return Err(err);
            }
            if shared_writable {
                mapped_file.disable_private_page_cache();
            }
            removed_areas
        };
        Self::notify_file_unmapped(&removed_areas);
        mapped_file.on_mapped();
        let removed = self.unmap_page_mappings(range.clone())?;
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
        self.vmas.lock().insert(area)?;

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
        drop(pages);
        self.mapped_pages.store(mapped, Ordering::Release);
        self.publish_new_user_range(range.start, range.end - range.start);
        Ok(())
    }

    /// 取消映射。同时把已 commit 的页表项摘掉；物理页由 resident page refcount 回收。
    #[kernel_symbols::export(name = "general.mm.VmSpace.unmap", contract = "kernel.mm.mapping@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn unmap(&self, range: Range<usize>) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let removed_areas = self.vmas.lock().unmap_range(&range);
        Self::notify_file_unmapped(&removed_areas);
        let removed = self.unmap_page_mappings(range.clone())?;
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

        let (removed_target, mapped_tail) = {
            let mut vmas = self.vmas.lock();
            if !vmas.contains_range(&old_range) {
                return Err(Errno::ENOMEM);
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
                vmas.insert(tail)?;
                files
            } else {
                Vec::new()
            };
            (removed_target, mapped_tail)
        };
        Self::notify_file_unmapped(&removed_target);
        Self::notify_files_mapped(mapped_tail);
        drop(removed_target);
        prune_shared_anon_pages();

        let removed_pages = self.unmap_page_mappings(new_range.clone())?;
        if !removed_pages.is_empty() {
            self.invalidate_user_range(new_range.start, new_range.end - new_range.start);
        }
        drop(removed_pages);
        self.move_page_mappings(old_range.start, new_range.start, old_len)?;
        self.mmap_next.store(new_range.end, Ordering::Release);
        Ok(new_range.start)
    }

    /// 修改权限。要求整个 range 已被 VMA 连续覆盖。
    #[kernel_symbols::export(name = "general.mm.VmSpace.mprotect", contract = "kernel.mm.mapping@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn mprotect(&self, range: Range<usize>, new_flags: VmFlags) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let mut touched = false;
        {
            let mut set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
            if new_flags.has(VmFlags::WRITE) {
                for area in set.iter_overlap(&range) {
                    if !area.flags.has(VmFlags::SHARED) {
                        continue;
                    }
                    if let VmBacking::File { file, .. } = &area.backing {
                        file.disable_private_page_cache();
                    }
                }
            }
            set.protect_range(&range, new_flags.with(VmFlags::USER));

            let mut pages = self.pages.lock();
            // mprotect 会被动态链接器和 lmbench mmap/munmap 小测频繁调用。
            // range 已按页对齐，直接逐页探测现有映射，避免先收集 key 到 Vec。
            let page_size = page_size();
            let mut va = range.start;
            while va < range.end {
                let Some(area) = set.find(va) else {
                    va += page_size;
                    continue;
                };
                let Some(mapping) = pages.get_mut(&va) else {
                    va += page_size;
                    continue;
                };
                let access = access_for_existing_page(area.flags, &mapping.page);
                self.protect_page_no_flush(va, pte_flags_for(area.flags, access))?;
                mapping.access = access;
                touched = true;
                va += page_size;
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
        {
            let set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
        }
        let pages = self.pages.lock();
        let mut out = Vec::with_capacity(page_count);
        let mut va = range.start;
        while va < range.end {
            out.push(if pages.contains_key(&va) { 1 } else { 0 });
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

    /// 丢弃指定范围内已经常驻的页，保留 VMA 语义供后续缺页按 backing 重建。
    pub fn discard_resident_range(&self, range: Range<usize>) -> Result<(), Errno> {
        self.contains_user_range(range.clone())?;
        let removed = self.unmap_page_mappings(range.clone())?;
        if !removed.is_empty() {
            self.invalidate_user_range(range.start, range.end - range.start);
        }
        drop(removed);
        Ok(())
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
            pages
                .range(range)
                .map(|(_va, mapping)| Arc::clone(&mapping.page))
                .collect()
        };
        for page in pages {
            page.flush_to_backing()?;
        }
        Ok(())
    }

    pub fn mlock_range(&self, range: Range<usize>) -> Result<(), Errno> {
        self.update_locked_range(range, true)
    }

    pub fn munlock_range(&self, range: Range<usize>) -> Result<(), Errno> {
        self.update_locked_range(range, false)
    }

    pub fn mlock_all_current(&self) {
        let mut set = self.vmas.lock();
        let ranges: Vec<Range<usize>> = set.iter().map(|area| area.range.clone()).collect();
        for range in ranges {
            set.update_flags_range(&range, |flags| flags.with(VmFlags::LOCKED));
        }
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
    }

    fn update_locked_range(&self, range: Range<usize>, locked: bool) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let mut set = self.vmas.lock();
        if !set.contains_range(&range) {
            return Err(Errno::ENOMEM);
        }
        if locked {
            set.update_flags_range(&range, |flags| flags.with(VmFlags::LOCKED));
        } else {
            set.update_flags_range(&range, |flags| flags.without(VmFlags::LOCKED));
        }
        Ok(())
    }

    /// fork：克隆 VMA 元数据，已驻留的页按 private-COW / shared 语义重建页表。
    #[kernel_symbols::export(name = "general.mm.VmSpace.fork", contract = "kernel.mm.address-space@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn fork(&self) -> Self {
        let ops = user_pgd_ops().expect("[mm] user_pgd_ops not registered");
        let new_pgd = (ops.new_pgd_for_user)();
        let cloned_set = self.vmas.lock().fork_clone_metadata();
        let cloned_file_backings = Self::collect_file_backings(cloned_set.iter());
        let mut child_pages = BTreeMap::new();
        let mut child_maps = Vec::new();

        {
            let mut parent_pages = self.pages.lock();
            for (va, mapping) in parent_pages.iter_mut() {
                let Some(area) = cloned_set.find(*va) else {
                    continue;
                };
                let old_access = mapping.access;
                mapping.access = access_after_fork(area.flags, &mapping.page);
                if old_access != mapping.access {
                    self.protect_page_no_flush(*va, pte_flags_for(area.flags, mapping.access))
                        .expect("[mm] fork parent protect failed");
                }
                let child_mapping = mapping.clone();
                child_maps.push((
                    *va,
                    child_mapping.page.clone(),
                    area.flags,
                    child_mapping.access,
                ));
                child_pages.insert(*va, child_mapping);
            }
        }
        if !child_maps.is_empty() {
            self.flush_full_user_tlb();
        }

        for (va, page, flags, access) in &child_maps {
            unsafe {
                (ops.map)(
                    new_pgd,
                    *va,
                    page.paddr(),
                    pte_flags_for(*flags, *access).with(VmFlags::USER),
                );
            }
        }

        Self::notify_files_mapped(cloned_file_backings);

        let mapped_pages = child_pages.len();
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
            // fork 创建独立 mm，按 Linux 语义不继承 expedited 注册状态。
            membarrier_registration: AtomicUsize::new(0),
            mapped_pages: AtomicUsize::new(mapped_pages),
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
        self.handle_fault_inner(addr, kind, true, true)
    }

    /// 预解析用户页访问，但不把已驻留页当作硬件缓存了无效 translation。
    fn ensure_page_access(&self, addr: usize, kind: FaultKind) -> FaultOutcome {
        self.handle_fault_inner(addr, kind, false, false)
    }

    fn handle_fault_inner(
        &self,
        addr: usize,
        kind: FaultKind,
        publish_unchanged_mapping: bool,
        allow_fault_around: bool,
    ) -> FaultOutcome {
        if user_pgd_ops().is_none() {
            return FaultOutcome::Kernel(KernelFaultReason::NotInitialized);
        }
        let page = page_base(addr);
        let set = self.vmas.lock();
        let Some(area) = set.find(page) else {
            drop(set);
            let mut set = self.vmas.lock();
            let Some((_added, flags)) = set.grow_down_to(page, vm_layout().max_grows_down_bytes)
            else {
                return FaultOutcome::Segv;
            };
            let backing = set
                .find(page)
                .expect("[mm] grow_down_to 成功后必须覆盖目标页")
                .backing
                .clone();
            drop(set);
            return self.commit_fault_page(page, backing, flags, page, kind);
        };
        if !permits(area.flags, kind) {
            return FaultOutcome::Segv;
        }
        let backing = area.backing.clone();
        let flags = area.flags;
        let area_start = area.range.start;
        let area_range = area.range.clone();
        drop(set);

        if let Some(outcome) =
            self.handle_resident_fault(page, flags, kind, publish_unchanged_mapping)
        {
            return outcome;
        }

        if allow_fault_around {
            if let Some(plan) = PrivateFileFaultAround::new(page, area_range, flags, &backing, kind)
            {
                let prepared = match plan.prepare() {
                    Ok(prepared) => prepared,
                    Err(err) => return fault_from_errno(err),
                };
                match self.commit_private_file_fault_around(&plan, prepared) {
                    FaultAroundCommit::Done(outcome) => return outcome,
                    FaultAroundCommit::Retry => {
                        // VMA 在锁外 I/O 期间发生变化；只重试普通单页路径，避免在
                        // 高频 mmap/mprotect 竞争下反复执行投机读取。
                        return self.handle_fault_inner(
                            addr,
                            kind,
                            publish_unchanged_mapping,
                            false,
                        );
                    }
                }
            }
        }

        self.commit_fault_page(page, backing, flags, area_start, kind)
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
            FaultOutcome::Segv | FaultOutcome::Kernel(_) => Err(Errno::EFAULT),
        }
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
        let mapping = pages.get(&page_va).ok_or(Errno::EFAULT)?;
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
        drop(pages);
        self.mapped_pages.store(mapped, Ordering::Release);
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
        if pages.contains_key(&page_va) {
            return Err(Errno::EEXIST);
        }
        self.map_page_no_flush(page_va, page.paddr(), pte_flags_for(flags, access))?;
        pages.insert(page_va, PageMapping { page, access });
        let mapped = pages.len();
        drop(pages);
        self.mapped_pages.store(mapped, Ordering::Release);
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
            FaultOutcome::Segv | FaultOutcome::Kernel(_) => return Err(Errno::EFAULT),
        }

        let page_va = page_base(user);
        let offset = user - page_va;
        let len = max_len.min(page_size() - offset);
        let page = {
            let pages = self.pages.lock();
            pages
                .get(&page_va)
                .map(|mapping| Arc::clone(&mapping.page))
                .ok_or(Errno::EFAULT)?
        };
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EFAULT)?;
        Ok((Arc::clone(&page), virt_fn(page.paddr()) + offset, len))
    }

    /// 提交已在锁外读好的只读私有文件页。
    ///
    /// 重新取得 VMA/pages 锁后先验证快照。并发 fault 若已经安装候选页，只提交
    /// 该页之前的连续新页前缀，剩余候选在解锁后由 Arc 析构回收；因此不会覆盖
    /// 现有 PTE，也能用一次 `publish_new_mapping` 发布完整前缀。
    fn commit_private_file_fault_around(
        &self,
        plan: &PrivateFileFaultAround,
        prepared: Vec<PreparedFilePage>,
    ) -> FaultAroundCommit {
        if prepared.is_empty() {
            #[cfg(feature = "performance-profile")]
            record_fault_around_commit(0, false);
            return FaultAroundCommit::Done(FaultOutcome::Segv);
        }

        let set = self.vmas.lock();
        let Some(area) = set.find(plan.fault_page) else {
            drop(set);
            drop(prepared);
            return FaultAroundCommit::Retry;
        };
        if !plan.matches_area(area) {
            drop(set);
            drop(prepared);
            return FaultAroundCommit::Retry;
        }

        let mut pages = self.pages.lock();
        if pages.contains_key(&plan.fault_page) {
            drop(pages);
            drop(set);
            drop(prepared);
            // 另一 CPU 在本次 I/O 期间先发布了 PTE；当前 CPU 仍需收敛导致
            // 本次硬件 fault 的旧无效 translation。
            #[cfg(feature = "performance-profile")]
            record_fault_around_commit(0, true);
            self.publish_new_user_range(plan.fault_page, page_size());
            return FaultAroundCommit::Done(FaultOutcome::Fixed);
        }

        let prefix_len =
            unmapped_prefix_len(prepared.iter().map(|candidate| candidate.vaddr), |vaddr| {
                pages.contains_key(&vaddr)
            });
        let mut installed = 0usize;
        let mut map_error = None;
        let mut candidates = prepared.into_iter();
        let mut failed_candidate = None;
        for candidate in candidates.by_ref().take(prefix_len) {
            let access = PageAccess::ReadOnly;
            if let Err(err) = self.map_page_no_flush(
                candidate.vaddr,
                candidate.page.paddr(),
                pte_flags_for(plan.flags, access),
            ) {
                map_error = Some(err);
                failed_candidate = Some(candidate);
                break;
            }
            pages.insert(
                candidate.vaddr,
                PageMapping {
                    page: candidate.page,
                    access,
                },
            );
            installed += 1;
        }
        let mapped = pages.len();
        drop(pages);
        drop(set);
        // 未采用的投机页可能触发物理页回收，必须在 VMA/pages 锁外析构。
        drop(failed_candidate);
        drop(candidates);
        #[cfg(feature = "performance-profile")]
        record_fault_around_commit(installed, false);

        if installed != 0 {
            self.mapped_pages.store(mapped, Ordering::Release);
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

    fn commit_fault_page(
        &self,
        page_va: usize,
        backing: VmBacking,
        flags: VmFlags,
        area_start: usize,
        kind: FaultKind,
    ) -> FaultOutcome {
        let page = match backing {
            VmBacking::Anon { .. } => alloc_zeroed_user_page()
                .map(ResidentPage::new_anon)
                .ok_or(Errno::ENOMEM),
            VmBacking::SharedAnon { object, offset } => {
                let object_off = offset + (page_va - area_start) as u64;
                shared_anon_page(&object, object_off)
            }
            VmBacking::File { file, offset } => {
                let file_off = offset + (page_va - area_start) as u64;
                if flags.has(VmFlags::SHARED) {
                    shared_file_page(file, file_off)
                } else {
                    private_file_page(&file, file_off)
                }
            }
            VmBacking::Direct(base) => {
                let paddr = base + (page_va - area_start);
                Ok(ResidentPage::new_direct(paddr))
            }
        };
        let page = match page {
            Ok(page) => page,
            Err(err) => return fault_from_errno(err),
        };
        let mut page = page;
        let mut access = access_for_new_page(flags, &page);
        if is_write_fault(kind) && matches!(access, PageAccess::Cow) {
            page = match clone_page_to_anon(&page) {
                Ok(page) => page,
                Err(err) => return fault_from_errno(err),
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

        let mut pages = self.pages.lock();
        if let Some(mapping) = pages.get_mut(&page_va) {
            let update = self.handle_resident_fault_locked(page_va, flags, kind, mapping);
            drop(pages);
            return self.finish_resident_fault(page_va, update, true);
        }
        if let Err(err) =
            self.map_page_no_flush(page_va, page.paddr(), pte_flags_for(flags, access))
        {
            drop(pages);
            return fault_from_errno(err);
        }
        pages.insert(page_va, PageMapping { page, access });
        let mapped = pages.len();
        drop(pages);
        self.mapped_pages.store(mapped, Ordering::Release);
        self.publish_new_user_range(page_va, page_size());
        FaultOutcome::Fixed
    }

    fn handle_resident_fault(
        &self,
        page_va: usize,
        flags: VmFlags,
        kind: FaultKind,
        publish_unchanged_mapping: bool,
    ) -> Option<FaultOutcome> {
        let mut pages = self.pages.lock();
        let mapping = pages.get_mut(&page_va)?;
        let update = self.handle_resident_fault_locked(page_va, flags, kind, mapping);
        drop(pages);
        Some(self.finish_resident_fault(page_va, update, publish_unchanged_mapping))
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
        unsafe {
            (ops.map)(self.pgd, vaddr, paddr, flags.with(VmFlags::USER));
        }
        Ok(())
    }

    fn protect_page_no_flush(&self, vaddr: usize, flags: VmFlags) -> Result<(), Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        let page_size = page_size();
        unsafe {
            (ops.protect)(self.pgd, vaddr, page_size, flags.with(VmFlags::USER));
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
            (ops.map)(self.pgd, vaddr, paddr, flags.with(VmFlags::USER));
        }
        Ok(())
    }

    fn unmap_page_mappings(&self, range: Range<usize>) -> Result<Vec<(usize, PageMapping)>, Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        let mut pages = self.pages.lock();
        let keys: Vec<usize> = pages.range(range).map(|(k, _)| *k).collect();
        let mut removed = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(mapping) = pages.remove(&key) {
                unsafe { (ops.unmap)(self.pgd, key, page_size()) };
                removed.push((key, mapping));
            }
        }
        let mapped = pages.len();
        drop(pages);
        self.mapped_pages.store(mapped, Ordering::Release);
        Ok(removed)
    }

    fn move_page_mappings(
        &self,
        old_start: usize,
        new_start: usize,
        len: usize,
    ) -> Result<(), Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        let old_range = old_start..old_start + len;
        let set = self.vmas.lock();
        let mut pages = self.pages.lock();
        let keys: Vec<usize> = pages.range(old_range.clone()).map(|(va, _)| *va).collect();
        let mut moves = Vec::with_capacity(keys.len());
        for old_va in &keys {
            let new_va = new_start + (old_va - old_start);
            let area = set.find(new_va).ok_or(Errno::ENOMEM)?;
            let mapping = pages.get(old_va).ok_or(Errno::ENOMEM)?;
            moves.push((
                *old_va,
                new_va,
                mapping.page.paddr(),
                pte_flags_for(area.flags, mapping.access),
            ));
        }
        for (old_va, new_va, paddr, flags) in moves {
            let mapping = pages.remove(&old_va).ok_or(Errno::ENOMEM)?;
            unsafe {
                (ops.unmap)(self.pgd, old_va, page_size());
                (ops.map)(self.pgd, new_va, paddr, flags.with(VmFlags::USER));
            }
            pages.insert(new_va, mapping);
        }
        let mapped = pages.len();
        drop(pages);
        drop(set);
        self.mapped_pages.store(mapped, Ordering::Release);
        if !keys.is_empty() {
            self.invalidate_user_range(old_start, len);
            self.invalidate_user_range(new_start, len);
        }
        Ok(())
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
            vmas.insert(tail)?;
            files
        };
        Self::notify_files_mapped(mapped_tail);
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
        let (files, areas) = {
            let mut vmas = self.vmas.lock();
            let files = Self::collect_file_backings(vmas.iter());
            let areas = vmas.take_all();
            (files, areas)
        };
        for file in files {
            file.on_unmapped();
        }
        self.pages.lock().clear();
        drop(areas);
        prune_shared_anon_pages();
        if let Some(ops) = user_pgd_ops() {
            unsafe { (ops.drop_pgd)(self.pgd) };
        }
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

fn shared_file_page(file: Arc<dyn FileLike>, file_off: u64) -> Result<Arc<ResidentPage>, Errno> {
    let key = FilePageKey::new(&file, file_off, 0);
    if let Some(page) = find_cached_file_page(&SHARED_FILE_PAGES, key) {
        return Ok(page);
    }
    let paddr = load_file_page(&*file, file_off)?;
    let page = ResidentPage::new_shared_file(paddr, Arc::clone(&file), file_off);
    Ok(publish_cached_file_page(&SHARED_FILE_PAGES, key, page))
}

fn private_file_page(file: &Arc<dyn FileLike>, file_off: u64) -> Result<Arc<ResidentPage>, Errno> {
    for _ in 0..PRIVATE_FILE_CACHE_RETRIES {
        let (Some(file_key), Some(generation)) = (
            file.private_page_cache_key(),
            file.private_page_cache_generation(),
        ) else {
            let paddr = load_file_page(file.as_ref(), file_off)?;
            return Ok(ResidentPage::new_private_file(paddr));
        };
        let key = FilePageKey::new_private(file_key, file_off, generation);
        if let Some(page) = find_cached_private_file_page(&PRIVATE_FILE_PAGES, key) {
            if file.private_page_cache_generation() == Some(generation) {
                return Ok(page);
            }
            continue;
        }
        let paddr = match load_file_page(file.as_ref(), file_off) {
            Ok(paddr) => paddr,
            Err(err) => {
                // truncate/write 可在读页期间改变 EOF。若代际已经变化，这次短读
                // 只是乐观快照失效，应重试新代际；稳定代际的真实 I/O 错误才传播。
                if file.private_page_cache_generation() != Some(generation) {
                    continue;
                }
                return Err(err);
            }
        };
        let page = ResidentPage::new_private_file(paddr);
        if file.private_page_cache_generation() != Some(generation) {
            continue;
        }
        let page = publish_cached_private_file_page(&PRIVATE_FILE_PAGES, key, page);
        if file.private_page_cache_generation() == Some(generation) {
            return Ok(page);
        }
        // 文件在发布窗口内发生写入时，已观察到的旧代际不应继续占据热缓存。
        // 只有仍指向本次候选的条目才会被移除，避免误删并发线程发布的新页。
        PRIVATE_FILE_PAGES.remove_if_same(key, &page);
    }
    let paddr = load_file_page(file.as_ref(), file_off)?;
    Ok(ResidentPage::new_private_file(paddr))
}

fn find_cached_private_file_page<const SHARD_COUNT: usize>(
    cache: &ShardedPrivateFilePageCache<SHARD_COUNT>,
    key: FilePageKey,
) -> Option<Arc<ResidentPage>> {
    cache.find(key)
}

/// 并发缺页可能同时读出同一私有文件页。缓存锁内只发布强引用和选择淘汰者；
/// 竞争失败的候选及淘汰页都在锁外析构，避免页析构/物理页释放扩大临界区。
fn publish_cached_private_file_page<const SHARD_COUNT: usize>(
    cache: &ShardedPrivateFilePageCache<SHARD_COUNT>,
    key: FilePageKey,
    candidate: Arc<ResidentPage>,
) -> Arc<ResidentPage> {
    cache.publish(key, candidate)
}

/// 在物理页压力下强制释放一批私有文件缓存引用。
///
/// 每个 Arc 都在缓存锁外析构；仍映射到进程的页会由对应 VMA 继续保活，已经只由
/// 缓存持有的页则立即归还 buddy。返回移除的缓存条目数，而不是实际释放的物理页数。
fn reclaim_private_file_cache_pages(limit: usize) -> usize {
    PRIVATE_FILE_PAGES.reclaim(limit)
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
    let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
    let result = (|| {
        let virt = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;
        let page_size = page_size();
        let len = (file_size - file_off).min(page_size as u64) as usize;
        let kbuf = unsafe { core::slice::from_raw_parts_mut(virt(paddr) as *mut u8, page_size) };
        read_file_bytes_exact(file, file_off, &mut kbuf[..len])
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

fn clone_page_to_anon(source: &ResidentPage) -> Result<Arc<ResidentPage>, Errno> {
    let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
    let result = (|| {
        let virt = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;
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
        Errno::ENOMEM => FaultOutcome::Kernel(KernelFaultReason::UncaughtKernelAccess),
        _ => FaultOutcome::Segv,
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
    let order = user_page_order()?;
    let size = page_size();
    if let Some(paddr) = try_alloc_zeroed_user_page(order, size) {
        return Some(paddr);
    }

    // 编译负载会把 8 GiB guest 推到很低的空闲页水位。强文件缓存必须是可回收
    // 的性能层，而不能让匿名页/COW 因固定缓存预算提前 ENOMEM。分批释放后重试，
    // 既避免一次丢掉整个热集，也保证持续压力最终可以清空缓存。
    loop {
        if reclaim_private_file_cache_pages(PRIVATE_FILE_CACHE_RECLAIM_BATCH) == 0 {
            return None;
        }
        if let Some(paddr) = try_alloc_zeroed_user_page(order, size) {
            return Some(paddr);
        }
    }
}

fn try_alloc_zeroed_user_page(order: usize, size: usize) -> Option<usize> {
    // 用户物理页必须进入 allocator registry；否则 fork/munmap/drop 路径无法被
    // allocator 审计发现泄漏或重复释放。
    let allocation = allocator::KERNEL_ALLOCATOR
        .allocate_physical(allocator::PhysicalAllocRequest::new(
            size,
            allocator::PAGE_SIZE,
        ))
        .ok()?;
    let Some(virt) = allocator::KERNEL_ALLOCATOR.load_phys_to_virt() else {
        let _ = allocator::KERNEL_ALLOCATOR.try_free_physical(allocation);
        return None;
    };
    if allocation.order != order || allocation.size != size {
        let _ = allocator::KERNEL_ALLOCATOR.try_free_physical(allocation);
        return None;
    }
    unsafe { core::ptr::write_bytes(virt(allocation.paddr) as *mut u8, 0, size) };
    Some(allocation.paddr)
}

fn free_user_page(paddr: usize) {
    if let Err(err) = allocator::KERNEL_ALLOCATOR.try_free_physical_addr(paddr) {
        log::error!(
            "[mm] failed to free tracked user page paddr={:#x}: {:?}",
            paddr,
            err
        );
    }
}

fn user_page_order() -> Option<usize> {
    let page_size = page_size();
    if page_size < allocator::PAGE_SIZE || page_size % allocator::PAGE_SIZE != 0 {
        return None;
    }
    let allocator_pages = page_size / allocator::PAGE_SIZE;
    if !allocator_pages.is_power_of_two() {
        return None;
    }
    Some(allocator_pages.trailing_zeros() as usize)
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

    use super::{
        FILE_FAULT_AROUND_PAGES, FaultKind, FilePageKey, PageAccess, ResidentPage,
        ShardedPrivateFilePageCache, VmFlags, WeakFilePageCache, access_for_private_file,
        file_fault_around_window, find_cached_private_file_page, permits_file_fault_around,
        plan_file_segment, publish_cached_file_page, publish_cached_private_file_page,
        read_file_bytes_exact, unmapped_prefix_len,
    };
    use errno::Errno;
    use mm::FileLike;
    use sched::sync::Spinlock;

    const PAGE_SIZE: usize = 4096;

    struct ChunkedFile {
        bytes: &'static [u8],
        max_chunk: usize,
        eof_at: usize,
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

        assert_eq!(cache.shard_index(key), cache.shard_index(key));
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

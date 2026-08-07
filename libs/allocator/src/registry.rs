//! 分配记录注册表。
//!
//! 这个模块维护“用户指针 -> 分配记录”的映射，用来支持以下关键能力：
//!
//! - `deallocate(ptr)` 时根据裸指针找回分配来源与布局信息；
//! - `realloc` 时判断对象是 boot/small/large/physical 哪一路分配；
//! - 统计和调试时追踪当前活跃分配。
//!
//! 从设计上看，它是整个 allocator 的“账本”。真正的内存页由 buddy、slab、
//! kernel heap 持有，而注册表负责记账、查账和销账。
//!
//! 实现采用固定桶数组 + 单向链表节点的哈希表形式，强调三点：
//!
//! 1. 结构简单，可在 `no_std` 环境下工作；
//! 2. 通过自管节点池避免依赖外部容器；
//! 3. 查询和删除路径明确，便于在释放时恢复原始分配语义。
//!
//! 热路径按指针哈希切成固定 shard。每个 shard 拥有自己的桶数组、节点 freelist 和锁，
//! 普通 alloc/free 只会碰其中一个 shard，避免所有 CPU 都竞争同一把 registry 全局锁。
use core::alloc::Layout;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::Mutex;

use crate::boot::BootAllocator;
use crate::error::RegistryError;
use crate::request::{AllocationKind, AllocationRecord, MemoryDomain};

// 注册表覆盖所有内核堆对象，桶数需要按长期活跃对象规模配置。两份 usize 数组
// （桶头与链长）合计占用 1 MiB，避免文件系统等高分配负载退化成长链扫描。
const DEFAULT_BUCKETS: usize = 65_536;
const REGISTRY_NODE_REFILL: usize = 64;
/// registry 固定分片数。保持 2 的幂，才能用哈希低位直接选择 shard。
const REGISTRY_SHARDS: usize = 64;

#[derive(Clone, Copy, Debug, Default)]
pub struct AllocationRegistryStats {
    pub shard_count: usize,
    pub bucket_count: usize,
    pub live_records: usize,
    pub max_shard_live_records: usize,
    pub free_nodes: usize,
    pub node_refills: u64,
    pub nodes_allocated: usize,
    pub collisions: u64,
    pub lookup_misses: u64,
    pub insert_failures: u64,
    pub remove_failures: u64,
    pub duplicate_inserts: u64,
    pub double_free_attempts: u64,
    pub accounting_underflows: u64,
    pub chain_corruptions: u64,
    pub max_chain_len: usize,
    pub live_boot: usize,
    pub live_small: usize,
    pub live_large: usize,
    pub live_physical: usize,
}

/// profiling 构建中 registry 各类路径的累计调用计数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegistryPathCounters {
    pub register_kernel: u64,
    pub register_owned: u64,
    pub remove_kernel: u64,
    pub remove_owned: u64,
    pub containing_queries: u64,
    pub containing_scanned_shards: u64,
    pub containing_scanned_buckets: u64,
    pub containing_scanned_nodes: u64,
}

impl RegistryPathCounters {
    pub const fn saturating_sub(self, earlier: Self) -> Self {
        Self {
            register_kernel: self.register_kernel.saturating_sub(earlier.register_kernel),
            register_owned: self.register_owned.saturating_sub(earlier.register_owned),
            remove_kernel: self.remove_kernel.saturating_sub(earlier.remove_kernel),
            remove_owned: self.remove_owned.saturating_sub(earlier.remove_owned),
            containing_queries: self
                .containing_queries
                .saturating_sub(earlier.containing_queries),
            containing_scanned_shards: self
                .containing_scanned_shards
                .saturating_sub(earlier.containing_scanned_shards),
            containing_scanned_buckets: self
                .containing_scanned_buckets
                .saturating_sub(earlier.containing_scanned_buckets),
            containing_scanned_nodes: self
                .containing_scanned_nodes
                .saturating_sub(earlier.containing_scanned_nodes),
        }
    }
}

/// 注册表结构审计结果。
///
/// `AllocationRegistryStats` 依赖热路径维护的 O(1) 计数器；这个结构则在冷路径里重新扫描
/// bucket 链和 freelist，用来验证计数器本身没有和链表结构脱节。它不分配内存，也不修复
/// 状态，只作为测试、诊断和 benchmark 的一致性证据。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocationRegistryAudit {
    pub flags: AllocationRegistryAuditFlags,
    pub initialized_shards: usize,
    pub corrupt_shards: usize,
    pub scanned_live_records: usize,
    pub scanned_free_nodes: usize,
    pub scanned_live_boot: usize,
    pub scanned_live_small: usize,
    pub scanned_live_large: usize,
    pub scanned_live_physical: usize,
    pub scanned_max_chain_len: usize,
}

/// 注册表的一致性快照。
///
/// `stats()` 是轻量计数器读取，`audit()` 是结构扫描。诊断和 benchmark 通常两者都需要；
/// 这个快照把二者合并到每个 shard 的同一次加锁里，避免冷路径重复锁 shard、重复读取
/// bucket 元数据，也让统计值和结构扫描结果来自更接近的时间点。
#[derive(Clone, Copy, Debug, Default)]
pub struct AllocationRegistrySnapshot {
    pub stats: AllocationRegistryStats,
    pub audit: AllocationRegistryAudit,
}

/// 指定外部所有者仍存活的分配记录摘要。
///
/// 该快照直接扫描 allocator registry，不分配内存，也不暴露对象地址。它用于在 ELM
/// 退役被资源账本阻塞时区分普通堆对象、大对象和显式物理页泄漏。
#[derive(Clone, Copy, Debug, Default)]
pub struct AllocationOwnerStats {
    pub records: usize,
    pub requested_bytes: usize,
    pub usable_bytes: usize,
    pub boot_records: usize,
    pub small_records: usize,
    pub large_records: usize,
    pub physical_records: usize,
    pub largest_requested_bytes: usize,
    pub largest_usable_bytes: usize,
    pub scan_errors: usize,
}

impl AllocationRegistryAudit {
    pub const fn is_consistent(self) -> bool {
        self.flags.is_empty()
    }
}

/// 注册表结构审计发现的问题集合。
///
/// 位标志用于保留多个同时存在的问题；调用方可以直接按位判断，而不需要解析日志字符串。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocationRegistryAuditFlags(u32);

impl AllocationRegistryAuditFlags {
    pub const UNINITIALIZED_SHARD: Self = Self(1 << 0);
    pub const NULL_BUCKETS: Self = Self(1 << 1);
    pub const BUCKET_CHAIN_LOOP: Self = Self(1 << 2);
    pub const FREE_LIST_LOOP: Self = Self(1 << 3);
    pub const LIVE_COUNT_MISMATCH: Self = Self(1 << 4);
    pub const FREE_COUNT_MISMATCH: Self = Self(1 << 5);
    pub const KIND_COUNT_MISMATCH: Self = Self(1 << 6);
    pub const NODE_ACCOUNTING_MISMATCH: Self = Self(1 << 7);
    /// 兼容旧命名。节点池少记或多记都会破坏 registry 完整性，因此新代码应使用
    /// [`AllocationRegistryAuditFlags::NODE_ACCOUNTING_MISMATCH`]。
    pub const NODE_ACCOUNTING_OVERFLOW: Self = Self::NODE_ACCOUNTING_MISMATCH;
    pub const MAX_CHAIN_MISMATCH: Self = Self(1 << 8);
    pub const INVALID_RECORD: Self = Self(1 << 9);
    pub const WRONG_BUCKET: Self = Self(1 << 10);
    pub const ACCOUNTING_UNDERFLOW: Self = Self(1 << 11);
    pub const CHAIN_CORRUPTION_OBSERVED: Self = Self(1 << 12);
    pub const BUCKET_LENGTH_MISMATCH: Self = Self(1 << 13);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, flag: Self) -> bool {
        (self.0 & flag.0) != 0
    }

    fn insert(&mut self, flag: Self) {
        self.0 |= flag.0;
    }
}

#[derive(Clone, Copy)]
struct RegistryNode {
    record: StoredAllocationRecord,
    next: usize,
}

/// registry 内部的紧凑分配记录。
///
/// `AllocationRecord` 为公开接口保留了易用的 `Option<usize>` 和完整 `usize` 字段，
/// 直接嵌入链表节点会让每个节点达到 104 字节。allocator 产生的对齐和页尺寸都必然
/// 是 2 的幂，order 也受 buddy 上限约束，因此内部只保存它们的指数和窄 order；
/// 读取记录时再无损还原公开结构。这样不改变 registry 的查询、审计和错误语义，同时
/// 显著减少注册、删除时触碰的 metadata cache line 数量。
#[derive(Clone, Copy)]
struct StoredAllocationRecord {
    ptr: usize,
    paddr: usize,
    size: usize,
    usable_size: usize,
    accounting_owner: u64,
    backend_cookie: usize,
    kind: AllocationKind,
    domain: MemoryDomain,
    arena: Option<crate::request::AllocationArena>,
    has_paddr: bool,
    align_log2: u8,
    order: u8,
    page_size_log2: u8,
}

impl StoredAllocationRecord {
    fn try_from_record(record: AllocationRecord) -> Result<Self, RegistryError> {
        if record.ptr == 0 || !record.align.is_power_of_two() || !record.page_size.is_power_of_two()
        {
            return Err(RegistryError::InvalidRecord);
        }
        let order = u8::try_from(record.order).map_err(|_| RegistryError::InvalidRecord)?;
        Ok(Self {
            ptr: record.ptr,
            paddr: record.paddr.unwrap_or(0),
            size: record.size,
            usable_size: record.usable_size,
            accounting_owner: record.accounting_owner(),
            backend_cookie: record.backend_cookie,
            kind: record.kind,
            domain: record.domain,
            arena: record.arena,
            has_paddr: record.paddr.is_some(),
            align_log2: record.align.trailing_zeros() as u8,
            order,
            page_size_log2: record.page_size.trailing_zeros() as u8,
        })
    }

    fn into_record(self) -> AllocationRecord {
        let align = 1usize << self.align_log2;
        let page_size = 1usize << self.page_size_log2;
        let mut record = AllocationRecord::new(self.kind, self.domain, self.ptr)
            .with_sizes(self.size, self.usable_size, align)
            .with_accounting_owner(self.accounting_owner)
            .with_backend_cookie(self.backend_cookie);
        record.arena = self.arena;
        if self.has_paddr {
            record = record.with_physical(self.paddr, self.order as usize, page_size);
        } else {
            record.order = self.order as usize;
            record.page_size = page_size;
        }
        record
    }

    const fn empty() -> Self {
        Self {
            ptr: 0,
            paddr: 0,
            size: 0,
            usable_size: 0,
            accounting_owner: 0,
            backend_cookie: 0,
            kind: AllocationKind::Boot,
            domain: MemoryDomain::Kernel,
            arena: None,
            has_paddr: false,
            align_log2: 0,
            order: 0,
            page_size_log2: crate::buddy::PAGE_SIZE.trailing_zeros() as u8,
        }
    }
}

struct AllocationRegistryInner {
    buckets: *mut usize,
    /// 每个桶当前的链长。它与桶头数组在同一块 metadata 内存中连续存放。
    bucket_lengths: *mut usize,
    bucket_count: usize,
    /// 空闲 RegistryNode 单链表的头地址。
    ///
    /// 这里保留裸地址而不是引用，是因为节点存储来自 allocator 自己的 metadata
    /// arena；配套的 `free_node_count` 负责 O(1) 统计，避免诊断路径持锁扫描长链表。
    free_nodes: usize,
    free_node_count: usize,
    live_records: usize,
    node_refills: u64,
    nodes_allocated: usize,
    collisions: u64,
    lookup_misses: u64,
    insert_failures: u64,
    remove_failures: u64,
    duplicate_inserts: u64,
    double_free_attempts: u64,
    accounting_underflows: u64,
    chain_corruptions: u64,
    max_chain_len: usize,
    /// 删除当前最长链中的节点后，缓存的最大链长可能只是上界。
    ///
    /// alloc/free 热路径只标记失效；stats/audit 等冷路径读取前再扫描桶长数组，
    /// 避免稀疏哈希表在 `max_chain_len == 1` 时每次释放都持锁遍历整个 shard。
    max_chain_len_dirty: bool,
    live_by_kind: [usize; 4],
    initializing: bool,
    initialized: bool,
}

impl AllocationRegistryInner {
    const fn new() -> Self {
        Self {
            buckets: null_mut(),
            bucket_lengths: null_mut(),
            bucket_count: 0,
            free_nodes: 0,
            free_node_count: 0,
            live_records: 0,
            node_refills: 0,
            nodes_allocated: 0,
            collisions: 0,
            lookup_misses: 0,
            insert_failures: 0,
            remove_failures: 0,
            duplicate_inserts: 0,
            double_free_attempts: 0,
            accounting_underflows: 0,
            chain_corruptions: 0,
            max_chain_len: 0,
            max_chain_len_dirty: false,
            live_by_kind: [0; 4],
            initializing: false,
            initialized: false,
        }
    }
}

struct RegistryShard {
    /// 每个 shard 独立加锁。跨指针迁移通过 remove + register 两阶段完成，不同时持有
    /// 两个 shard 锁，避免未来 realloc 路径出现锁顺序问题。
    inner: Mutex<AllocationRegistryInner>,
}

impl RegistryShard {
    const fn new() -> Self {
        Self {
            inner: Mutex::new(AllocationRegistryInner::new()),
        }
    }
}

pub struct AllocationRegistry {
    shards: [RegistryShard; REGISTRY_SHARDS],
    register_kernel: AtomicU64,
    register_owned: AtomicU64,
    remove_kernel: AtomicU64,
    remove_owned: AtomicU64,
    containing_queries: AtomicU64,
    containing_scanned_shards: AtomicU64,
    containing_scanned_buckets: AtomicU64,
    containing_scanned_nodes: AtomicU64,
}

impl AllocationRegistry {
    pub const fn new() -> Self {
        Self {
            shards: [const { RegistryShard::new() }; REGISTRY_SHARDS],
            register_kernel: AtomicU64::new(0),
            register_owned: AtomicU64::new(0),
            remove_kernel: AtomicU64::new(0),
            remove_owned: AtomicU64::new(0),
            containing_queries: AtomicU64::new(0),
            containing_scanned_shards: AtomicU64::new(0),
            containing_scanned_buckets: AtomicU64::new(0),
            containing_scanned_nodes: AtomicU64::new(0),
        }
    }

    pub fn init(&self, boot: &BootAllocator) -> bool {
        self.init_with_buckets(boot, DEFAULT_BUCKETS)
    }

    pub fn init_with_buckets(&self, _boot: &BootAllocator, bucket_count: usize) -> bool {
        let bucket_count = normalize_bucket_count(bucket_count.max(REGISTRY_SHARDS));
        let buckets_per_shard = normalize_bucket_count(bucket_count.div_ceil(REGISTRY_SHARDS));
        for shard in &self.shards {
            loop {
                let mut inner = shard.inner.lock();
                if inner.initialized {
                    break;
                }
                if !inner.initializing {
                    inner.initializing = true;
                    break;
                }
                drop(inner);
                core::hint::spin_loop();
            }

            if shard.inner.lock().initialized {
                continue;
            }

            let metadata_slots = match buckets_per_shard.checked_mul(2) {
                Some(count) => count,
                None => {
                    shard.inner.lock().initializing = false;
                    return false;
                }
            };
            let layout = match Layout::array::<usize>(metadata_slots) {
                Ok(layout) => layout,
                Err(_) => {
                    shard.inner.lock().initializing = false;
                    return false;
                }
            };
            let buckets = crate::alloc_internal_metadata(layout) as *mut usize;
            if buckets.is_null() {
                shard.inner.lock().initializing = false;
                return false;
            }
            for idx in 0..metadata_slots {
                unsafe {
                    buckets.add(idx).write(0);
                }
            }

            let mut inner = shard.inner.lock();
            if inner.initialized {
                inner.initializing = false;
                continue;
            }
            inner.buckets = buckets;
            inner.bucket_lengths = unsafe { buckets.add(buckets_per_shard) };
            inner.bucket_count = buckets_per_shard;
            inner.free_nodes = 0;
            inner.free_node_count = 0;
            inner.live_records = 0;
            inner.node_refills = 0;
            inner.nodes_allocated = 0;
            inner.collisions = 0;
            inner.lookup_misses = 0;
            inner.insert_failures = 0;
            inner.remove_failures = 0;
            inner.duplicate_inserts = 0;
            inner.double_free_attempts = 0;
            inner.accounting_underflows = 0;
            inner.chain_corruptions = 0;
            inner.max_chain_len = 0;
            inner.max_chain_len_dirty = false;
            inner.live_by_kind = [0; 4];
            inner.initializing = false;
            inner.initialized = true;
        }
        true
    }

    pub fn register_result(
        &self,
        _boot: &BootAllocator,
        record: AllocationRecord,
    ) -> Result<(), RegistryError> {
        let owner = record.accounting_owner();
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(if owner == 0 {
            profiling::Event::AllocRegistryRegisterKernel
        } else {
            profiling::Event::AllocRegistryRegisterOwned
        });
        if record.ptr == 0 {
            let mut inner = self.shards[0].inner.lock();
            inner.insert_failures += 1;
            return Err(RegistryError::InvalidRecord);
        }

        let hash = hash_ptr(record.ptr);
        let shard = self.shard_for_hash(hash);
        let stored = match StoredAllocationRecord::try_from_record(record) {
            Ok(stored) => stored,
            Err(err) => {
                shard.inner.lock().insert_failures += 1;
                return Err(err);
            }
        };
        let mut pending_nodes = 0usize;
        let mut pending_node_count = 0usize;
        let mut pending_refill_count = 0usize;
        loop {
            let mut inner = shard.inner.lock();
            if !inner.initialized {
                inner.insert_failures += 1;
                if pending_nodes != 0 {
                    recycle_node_list_locked(&mut inner, pending_nodes, pending_node_count);
                }
                return Err(RegistryError::NotInitialized);
            }
            if pending_refill_count != 0 {
                inner.node_refills += 1;
                inner.nodes_allocated += pending_refill_count;
            }

            let bucket = bucket_index(hash, inner.bucket_count);
            let head = bucket_head(&inner, bucket);
            let (duplicate, chain_len_before) =
                match find_node_and_chain_len(head, record.ptr, inner.nodes_allocated) {
                    Ok(result) => result,
                    Err(()) => {
                        note_chain_corruption_locked(&mut inner);
                        inner.insert_failures += 1;
                        if pending_nodes != 0 {
                            recycle_node_list_locked(&mut inner, pending_nodes, pending_node_count);
                        }
                        return Err(RegistryError::InvalidRecord);
                    }
                };
            if duplicate.is_some() {
                inner.insert_failures += 1;
                inner.duplicate_inserts += 1;
                if pending_nodes != 0 {
                    recycle_node_list_locked(&mut inner, pending_nodes, pending_node_count);
                }
                return Err(RegistryError::DuplicatePointer);
            }

            let node_addr = if pending_nodes != 0 {
                pop_node_from_list(&mut pending_nodes, &mut pending_node_count)
            } else if inner.free_nodes != 0 {
                pop_free_node_locked(&mut inner)
            } else {
                drop(inner);
                match alloc_node_batch() {
                    Some((head, count)) => {
                        pending_nodes = head;
                        pending_node_count = count;
                        pending_refill_count = count;
                    }
                    None => {
                        let mut inner = shard.inner.lock();
                        inner.insert_failures += 1;
                        return Err(RegistryError::MetadataOutOfMemory);
                    }
                }
                continue;
            };
            if pending_nodes != 0 {
                recycle_node_list_locked(&mut inner, pending_nodes, pending_node_count);
            }

            write_node(
                node_addr,
                RegistryNode {
                    record: stored,
                    next: head,
                },
            );
            set_bucket_head(&inner, bucket, node_addr);
            if head != 0 {
                inner.collisions += 1;
            }
            inner.live_records += 1;
            inner.live_by_kind[kind_index(record.kind)] += 1;
            let chain_len = 1 + chain_len_before;
            set_bucket_chain_len(&inner, bucket, chain_len);
            note_chain_insert_locked(&mut inner, chain_len);
            self.note_register(owner);
            return Ok(());
        }
    }

    pub fn get(&self, ptr: usize) -> Option<AllocationRecord> {
        self.get_result(ptr).ok()
    }

    /// 查询完整覆盖给定地址范围的活跃分配记录。
    ///
    /// 注册表按分配起点散列，内部指针无法直接命中桶，因此该接口会遍历各 shard。
    /// 它只用于跨 ABI 裸指针校验，不应放进常规分配热路径。
    pub fn find_containing(&self, ptr: usize, len: usize) -> Option<AllocationRecord> {
        if ptr == 0 || len == 0 {
            return None;
        }
        let end = ptr.checked_add(len)?;
        let mut scanned_shards = 0u64;
        let mut scanned_buckets = 0u64;
        let mut scanned_nodes = 0u64;
        for shard in &self.shards {
            scanned_shards = scanned_shards.saturating_add(1);
            let mut inner = shard.inner.lock();
            if !inner.initialized {
                continue;
            }
            for bucket in 0..inner.bucket_count {
                scanned_buckets = scanned_buckets.saturating_add(1);
                let mut current = bucket_head(&inner, bucket);
                let mut visited = 0usize;
                while current != 0 {
                    if visited >= inner.nodes_allocated {
                        note_chain_corruption_locked(&mut inner);
                        self.note_containing_scan(scanned_shards, scanned_buckets, scanned_nodes);
                        return None;
                    }
                    scanned_nodes = scanned_nodes.saturating_add(1);
                    let node = read_node(current);
                    let record = node.record.into_record();
                    let usable = record.usable_size.max(record.size);
                    if usable != 0
                        && record
                            .ptr
                            .checked_add(usable)
                            .is_some_and(|record_end| record.ptr <= ptr && end <= record_end)
                    {
                        self.note_containing_scan(scanned_shards, scanned_buckets, scanned_nodes);
                        return Some(record);
                    }
                    current = node.next;
                    visited += 1;
                }
            }
        }
        self.note_containing_scan(scanned_shards, scanned_buckets, scanned_nodes);
        None
    }

    pub fn get_result(&self, ptr: usize) -> Result<AllocationRecord, RegistryError> {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::AllocRegistryLookup);
        let hash = hash_ptr(ptr);
        let mut inner = self.shard_for_hash(hash).inner.lock();
        if !inner.initialized {
            inner.lookup_misses += 1;
            return Err(RegistryError::NotInitialized);
        }
        let bucket = bucket_index(hash, inner.bucket_count);
        let node_addr = match find_node(bucket_head(&inner, bucket), ptr, inner.nodes_allocated) {
            Ok(Some(node_addr)) => node_addr,
            Ok(None) => {
                inner.lookup_misses += 1;
                return Err(RegistryError::UnknownPointer);
            }
            Err(()) => {
                note_chain_corruption_locked(&mut inner);
                inner.lookup_misses += 1;
                return Err(RegistryError::InvalidRecord);
            }
        };
        Ok(read_node_record(node_addr))
    }

    pub fn remove(&self, ptr: usize) -> Option<AllocationRecord> {
        self.remove_result(ptr).ok()
    }

    pub fn remove_result(&self, ptr: usize) -> Result<AllocationRecord, RegistryError> {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::AllocRegistryRemove);
        let hash = hash_ptr(ptr);
        let mut inner = self.shard_for_hash(hash).inner.lock();
        if !inner.initialized {
            inner.remove_failures += 1;
            return Err(RegistryError::NotInitialized);
        }
        let bucket = bucket_index(hash, inner.bucket_count);
        let mut prev = 0usize;
        let mut current = bucket_head(&inner, bucket);
        let mut visited = 0usize;
        while current != 0 {
            if visited >= inner.nodes_allocated {
                note_chain_corruption_locked(&mut inner);
                inner.remove_failures += 1;
                return Err(RegistryError::InvalidRecord);
            }
            visited += 1;

            let next = read_node_next(current);
            if read_node_ptr(current) == ptr {
                let record = read_node_record(current);
                let old_chain_len = bucket_chain_len(&inner, bucket);
                if old_chain_len == 0 {
                    note_chain_corruption_locked(&mut inner);
                    inner.remove_failures += 1;
                    return Err(RegistryError::InvalidRecord);
                }
                if prev == 0 {
                    set_bucket_head(&inner, bucket, next);
                } else {
                    write_node_next(prev, next);
                }
                set_bucket_chain_len(&inner, bucket, old_chain_len - 1);
                note_chain_remove_locked(&mut inner, old_chain_len);

                write_node_next(current, inner.free_nodes);
                inner.free_nodes = current;
                inner.free_node_count += 1;
                decrement_live_records_locked(&mut inner);
                let idx = kind_index(record.kind);
                decrement_live_kind_locked(&mut inner, idx);
                self.note_remove(record.accounting_owner());
                return Ok(record);
            }
            prev = current;
            current = next;
        }
        inner.remove_failures += 1;
        inner.double_free_attempts += 1;
        Err(RegistryError::UnknownPointer)
    }

    #[allow(dead_code)]
    pub fn update_result(
        &self,
        boot: &BootAllocator,
        old_ptr: usize,
        new_record: AllocationRecord,
    ) -> Result<(), RegistryError> {
        let old_record = self.remove_result(old_ptr)?;
        match self.register_result(boot, new_record) {
            Ok(()) => Ok(()),
            Err(err) => {
                // 在指针迁移更新失败时保留注册表所有权。从 `remove_result` 回收的节点
                // 仍然可用，因此回滚在常规路径下不需要再分配。
                let _ = self.register_result(boot, old_record);
                Err(err)
            }
        }
    }

    pub fn update_existing_result(
        &self,
        ptr: usize,
        new_record: AllocationRecord,
    ) -> Result<(), RegistryError> {
        self.update_existing_maybe_result(ptr, |_| Some(new_record))
            .map(|_| ())
    }

    /// 在持有目标 shard 锁的一次查找中完成可选记录改写。
    ///
    /// 这个接口只允许调用方基于旧记录做纯计算，不应在闭包里进入 allocator 其它层。
    /// 这样 `realloc` 的原地调整路径可以避免“先查账本、再锁同一 shard 更新”的两次
    /// 链表扫描；搬迁路径也能直接复用返回的旧记录，不需要失败后再查一次账本。
    pub fn update_existing_maybe_result<F>(
        &self,
        ptr: usize,
        f: F,
    ) -> Result<(AllocationRecord, bool), RegistryError>
    where
        F: FnOnce(AllocationRecord) -> Option<AllocationRecord>,
    {
        let hash = hash_ptr(ptr);
        let mut inner = self.shard_for_hash(hash).inner.lock();
        if !inner.initialized {
            inner.lookup_misses += 1;
            return Err(RegistryError::NotInitialized);
        }
        let bucket = bucket_index(hash, inner.bucket_count);
        let mut current = bucket_head(&inner, bucket);
        let mut visited = 0usize;
        while current != 0 {
            if visited >= inner.nodes_allocated {
                note_chain_corruption_locked(&mut inner);
                inner.lookup_misses += 1;
                return Err(RegistryError::InvalidRecord);
            }
            visited += 1;

            let next = read_node_next(current);
            if read_node_ptr(current) == ptr {
                let old_record = read_node_record(current);
                let Some(new_record) = f(old_record) else {
                    return Ok((old_record, false));
                };
                if new_record.ptr != ptr {
                    inner.insert_failures += 1;
                    return Err(RegistryError::InvalidRecord);
                }
                if old_record.kind != new_record.kind {
                    decrement_live_kind_locked(&mut inner, kind_index(old_record.kind));
                    inner.live_by_kind[kind_index(new_record.kind)] += 1;
                }
                let stored = match StoredAllocationRecord::try_from_record(new_record) {
                    Ok(stored) => stored,
                    Err(err) => {
                        inner.insert_failures += 1;
                        return Err(err);
                    }
                };
                write_node_record(current, stored);
                return Ok((new_record, true));
            }
            current = next;
        }
        inner.lookup_misses += 1;
        Err(RegistryError::UnknownPointer)
    }

    pub fn stats(&self) -> AllocationRegistryStats {
        let mut out = AllocationRegistryStats {
            shard_count: REGISTRY_SHARDS,
            ..AllocationRegistryStats::default()
        };
        for shard in &self.shards {
            let mut inner = shard.inner.lock();
            refresh_max_chain_len_if_dirty_locked(&mut inner);
            accumulate_stats_locked(&mut out, &inner);
        }
        out
    }

    pub fn path_counters(&self) -> RegistryPathCounters {
        RegistryPathCounters {
            register_kernel: self.register_kernel.load(Ordering::Relaxed),
            register_owned: self.register_owned.load(Ordering::Relaxed),
            remove_kernel: self.remove_kernel.load(Ordering::Relaxed),
            remove_owned: self.remove_owned.load(Ordering::Relaxed),
            containing_queries: self.containing_queries.load(Ordering::Relaxed),
            containing_scanned_shards: self.containing_scanned_shards.load(Ordering::Relaxed),
            containing_scanned_buckets: self.containing_scanned_buckets.load(Ordering::Relaxed),
            containing_scanned_nodes: self.containing_scanned_nodes.load(Ordering::Relaxed),
        }
    }

    pub fn audit(&self) -> AllocationRegistryAudit {
        self.snapshot().audit
    }

    pub fn snapshot(&self) -> AllocationRegistrySnapshot {
        let mut stats = AllocationRegistryStats {
            shard_count: REGISTRY_SHARDS,
            ..AllocationRegistryStats::default()
        };
        let mut out = AllocationRegistryAudit::default();
        for (shard_idx, shard) in self.shards.iter().enumerate() {
            let mut inner = shard.inner.lock();
            refresh_max_chain_len_if_dirty_locked(&mut inner);
            let mut shard_flags = AllocationRegistryAuditFlags::empty();
            accumulate_stats_locked(&mut stats, &inner);

            if !inner.initialized {
                shard_flags.insert(AllocationRegistryAuditFlags::UNINITIALIZED_SHARD);
                out.corrupt_shards += 1;
                out.flags.insert(shard_flags);
                continue;
            }
            out.initialized_shards += 1;

            if inner.buckets.is_null() || inner.bucket_lengths.is_null() || inner.bucket_count == 0
            {
                shard_flags.insert(AllocationRegistryAuditFlags::NULL_BUCKETS);
                out.corrupt_shards += 1;
                out.flags.insert(shard_flags);
                continue;
            }

            let scanned = audit_shard_locked(shard_idx, &inner, &mut shard_flags);
            out.scanned_live_records += scanned.live_records;
            out.scanned_free_nodes += scanned.free_nodes;
            out.scanned_live_boot += scanned.live_by_kind[kind_index(AllocationKind::Boot)];
            out.scanned_live_small += scanned.live_by_kind[kind_index(AllocationKind::Small)];
            out.scanned_live_large += scanned.live_by_kind[kind_index(AllocationKind::Large)];
            out.scanned_live_physical += scanned.live_by_kind[kind_index(AllocationKind::Physical)];
            out.scanned_max_chain_len = out.scanned_max_chain_len.max(scanned.max_chain_len);

            if scanned.live_records != inner.live_records {
                shard_flags.insert(AllocationRegistryAuditFlags::LIVE_COUNT_MISMATCH);
            }
            if scanned.free_nodes != inner.free_node_count {
                shard_flags.insert(AllocationRegistryAuditFlags::FREE_COUNT_MISMATCH);
            }
            if scanned.live_by_kind != inner.live_by_kind {
                shard_flags.insert(AllocationRegistryAuditFlags::KIND_COUNT_MISMATCH);
            }
            if scanned.live_records.saturating_add(scanned.free_nodes) != inner.nodes_allocated {
                shard_flags.insert(AllocationRegistryAuditFlags::NODE_ACCOUNTING_MISMATCH);
            }
            if scanned.max_chain_len != inner.max_chain_len {
                shard_flags.insert(AllocationRegistryAuditFlags::MAX_CHAIN_MISMATCH);
            }
            if inner.accounting_underflows != 0 {
                shard_flags.insert(AllocationRegistryAuditFlags::ACCOUNTING_UNDERFLOW);
            }
            if inner.chain_corruptions != 0 {
                shard_flags.insert(AllocationRegistryAuditFlags::CHAIN_CORRUPTION_OBSERVED);
            }

            if !shard_flags.is_empty() {
                out.corrupt_shards += 1;
                out.flags.insert(shard_flags);
            }
        }
        AllocationRegistrySnapshot { stats, audit: out }
    }

    /// 扫描指定外部所有者仍存活的分配记录。
    pub fn owner_stats(&self, owner: u64) -> AllocationOwnerStats {
        let mut out = AllocationOwnerStats::default();
        for shard in &self.shards {
            let inner = shard.inner.lock();
            if !inner.initialized
                || inner.buckets.is_null()
                || inner.bucket_lengths.is_null()
                || inner.bucket_count == 0
            {
                out.scan_errors = out.scan_errors.saturating_add(1);
                continue;
            }
            for bucket in 0..inner.bucket_count {
                let mut current = bucket_head(&inner, bucket);
                let mut visited = 0usize;
                while current != 0 {
                    if visited >= inner.nodes_allocated {
                        out.scan_errors = out.scan_errors.saturating_add(1);
                        break;
                    }
                    let node = read_node(current);
                    let record = node.record.into_record();
                    if record.accounting_owner() == owner {
                        out.records = out.records.saturating_add(1);
                        out.requested_bytes = out.requested_bytes.saturating_add(record.size);
                        out.usable_bytes = out
                            .usable_bytes
                            .saturating_add(record.usable_size.max(record.size));
                        out.largest_requested_bytes = out.largest_requested_bytes.max(record.size);
                        out.largest_usable_bytes = out
                            .largest_usable_bytes
                            .max(record.usable_size.max(record.size));
                        match record.kind {
                            AllocationKind::Boot => {
                                out.boot_records = out.boot_records.saturating_add(1)
                            }
                            AllocationKind::Small => {
                                out.small_records = out.small_records.saturating_add(1)
                            }
                            AllocationKind::Large => {
                                out.large_records = out.large_records.saturating_add(1)
                            }
                            AllocationKind::Physical => {
                                out.physical_records = out.physical_records.saturating_add(1)
                            }
                        }
                    }
                    current = node.next;
                    visited = visited.saturating_add(1);
                }
            }
        }
        out
    }

    fn shard_for_hash(&self, hash: usize) -> &RegistryShard {
        &self.shards[hash & (REGISTRY_SHARDS - 1)]
    }

    #[inline]
    fn note_register(&self, owner: u64) {
        #[cfg(feature = "performance-profile")]
        if owner == 0 {
            self.register_kernel.fetch_add(1, Ordering::Relaxed);
        } else {
            self.register_owned.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(not(feature = "performance-profile"))]
        let _ = owner;
    }

    #[inline]
    fn note_remove(&self, owner: u64) {
        #[cfg(feature = "performance-profile")]
        if owner == 0 {
            self.remove_kernel.fetch_add(1, Ordering::Relaxed);
        } else {
            self.remove_owned.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(not(feature = "performance-profile"))]
        let _ = owner;
    }

    #[inline]
    fn note_containing_scan(&self, shards: u64, buckets: u64, nodes: u64) {
        #[cfg(feature = "performance-profile")]
        {
            self.containing_queries.fetch_add(1, Ordering::Relaxed);
            self.containing_scanned_shards
                .fetch_add(shards, Ordering::Relaxed);
            self.containing_scanned_buckets
                .fetch_add(buckets, Ordering::Relaxed);
            self.containing_scanned_nodes
                .fetch_add(nodes, Ordering::Relaxed);
        }
        #[cfg(not(feature = "performance-profile"))]
        let _ = (shards, buckets, nodes);
    }
}

fn accumulate_stats_locked(out: &mut AllocationRegistryStats, inner: &AllocationRegistryInner) {
    out.bucket_count += inner.bucket_count;
    out.live_records += inner.live_records;
    out.max_shard_live_records = out.max_shard_live_records.max(inner.live_records);
    out.free_nodes += inner.free_node_count;
    out.node_refills += inner.node_refills;
    out.nodes_allocated += inner.nodes_allocated;
    out.collisions += inner.collisions;
    out.lookup_misses += inner.lookup_misses;
    out.insert_failures += inner.insert_failures;
    out.remove_failures += inner.remove_failures;
    out.duplicate_inserts += inner.duplicate_inserts;
    out.double_free_attempts += inner.double_free_attempts;
    out.accounting_underflows += inner.accounting_underflows;
    out.chain_corruptions += inner.chain_corruptions;
    out.max_chain_len = out.max_chain_len.max(inner.max_chain_len);
    out.live_boot += inner.live_by_kind[kind_index(AllocationKind::Boot)];
    out.live_small += inner.live_by_kind[kind_index(AllocationKind::Small)];
    out.live_large += inner.live_by_kind[kind_index(AllocationKind::Large)];
    out.live_physical += inner.live_by_kind[kind_index(AllocationKind::Physical)];
}

#[derive(Clone, Copy, Default)]
struct RegistryShardScan {
    live_records: usize,
    free_nodes: usize,
    live_by_kind: [usize; 4],
    max_chain_len: usize,
}

fn audit_shard_locked(
    shard_idx: usize,
    inner: &AllocationRegistryInner,
    flags: &mut AllocationRegistryAuditFlags,
) -> RegistryShardScan {
    let mut scan = RegistryShardScan::default();
    for bucket in 0..inner.bucket_count {
        let mut current = bucket_head(inner, bucket);
        let mut chain_len = 0usize;
        while current != 0 {
            if inner.nodes_allocated == 0 || chain_len >= inner.nodes_allocated {
                flags.insert(AllocationRegistryAuditFlags::BUCKET_CHAIN_LOOP);
                break;
            }

            let node = read_node(current);
            chain_len += 1;
            scan.live_records += 1;
            scan.live_by_kind[kind_index(node.record.kind)] += 1;

            if node.record.ptr == 0 {
                flags.insert(AllocationRegistryAuditFlags::INVALID_RECORD);
            } else {
                let hash = hash_ptr(node.record.ptr);
                if hash & (REGISTRY_SHARDS - 1) != shard_idx
                    || bucket_index(hash, inner.bucket_count) != bucket
                {
                    flags.insert(AllocationRegistryAuditFlags::WRONG_BUCKET);
                }
            }

            current = node.next;
        }
        if bucket_chain_len(inner, bucket) != chain_len {
            flags.insert(AllocationRegistryAuditFlags::BUCKET_LENGTH_MISMATCH);
        }
        scan.max_chain_len = scan.max_chain_len.max(chain_len);
    }

    let mut current = inner.free_nodes;
    while current != 0 {
        if inner.nodes_allocated == 0 || scan.free_nodes >= inner.nodes_allocated {
            flags.insert(AllocationRegistryAuditFlags::FREE_LIST_LOOP);
            break;
        }
        scan.free_nodes += 1;
        current = read_node_next(current);
    }

    scan
}

fn pop_free_node_locked(inner: &mut AllocationRegistryInner) -> usize {
    let node_addr = inner.free_nodes;
    if node_addr != 0 {
        inner.free_nodes = read_node_next(node_addr);
        decrement_free_node_count_locked(inner);
    }
    node_addr
}

fn pop_node_from_list(head: &mut usize, count: &mut usize) -> usize {
    let node_addr = *head;
    if node_addr != 0 {
        *head = read_node_next(node_addr);
        *count = count.saturating_sub(1);
    }
    node_addr
}

fn decrement_live_records_locked(inner: &mut AllocationRegistryInner) {
    if decrement_registry_counter(&mut inner.live_records) {
        inner.accounting_underflows = inner.accounting_underflows.saturating_add(1);
    }
}

fn decrement_live_kind_locked(inner: &mut AllocationRegistryInner, idx: usize) {
    if decrement_registry_counter(&mut inner.live_by_kind[idx]) {
        inner.accounting_underflows = inner.accounting_underflows.saturating_add(1);
    }
}

fn decrement_free_node_count_locked(inner: &mut AllocationRegistryInner) {
    if decrement_registry_counter(&mut inner.free_node_count) {
        inner.accounting_underflows = inner.accounting_underflows.saturating_add(1);
    }
}

fn decrement_registry_counter(counter: &mut usize) -> bool {
    // registry 热路径不应把计数器损坏静默压成 0。正常路径只走一次 checked_sub；
    // 若内部状态已经不一致，保留可继续运行的 0 值，同时把事件交给 stats/audit 暴露。
    match counter.checked_sub(1) {
        Some(next) => {
            *counter = next;
            false
        }
        None => true,
    }
}

fn note_chain_corruption_locked(inner: &mut AllocationRegistryInner) {
    // 热路径发现 bucket 链超过节点池规模时立即失败，避免在持锁状态下死循环。
    // 完整审计会通过 CHAIN_CORRUPTION_OBSERVED 暴露该事件。
    inner.chain_corruptions = inner.chain_corruptions.saturating_add(1);
}

fn note_chain_insert_locked(inner: &mut AllocationRegistryInner, chain_len: usize) {
    if chain_len >= inner.max_chain_len {
        // 若缓存此前因删除而失效，新链已经达到旧上界，就能重新证明最大值精确；
        // 超过旧上界时同理，新链必然是唯一可能的新最大值。
        inner.max_chain_len = chain_len;
        inner.max_chain_len_dirty = false;
    }
}

fn note_chain_remove_locked(inner: &mut AllocationRegistryInner, old_bucket_len: usize) {
    if old_bucket_len == inner.max_chain_len {
        inner.max_chain_len_dirty = true;
    }
}

fn refresh_max_chain_len_if_dirty_locked(inner: &mut AllocationRegistryInner) {
    if !inner.max_chain_len_dirty {
        return;
    }
    inner.max_chain_len = recompute_max_chain_len_locked(inner);
    inner.max_chain_len_dirty = false;
}

fn recompute_max_chain_len_locked(inner: &AllocationRegistryInner) -> usize {
    let mut max_chain_len = 0usize;
    for bucket in 0..inner.bucket_count {
        max_chain_len = max_chain_len.max(bucket_chain_len(inner, bucket));
    }
    max_chain_len
}

fn recycle_node_list_locked(inner: &mut AllocationRegistryInner, head: usize, count: usize) {
    if head == 0 {
        return;
    }
    let mut tail = head;
    let mut visited = 0usize;
    while visited < count {
        visited += 1;
        let next = read_node_next(tail);
        if next == 0 {
            break;
        }
        tail = next;
    }
    if visited != count || read_node_next(tail) != 0 {
        note_chain_corruption_locked(inner);
    }
    write_node_next(tail, inner.free_nodes);
    inner.free_nodes = head;
    inner.free_node_count = inner.free_node_count.saturating_add(visited);
}

fn alloc_node_batch() -> Option<(usize, usize)> {
    let layout = Layout::array::<RegistryNode>(REGISTRY_NODE_REFILL).ok()?;
    let base = crate::alloc_internal_metadata(layout) as usize;
    if base == 0 {
        return None;
    }

    let mut head = 0usize;
    for idx in (0..REGISTRY_NODE_REFILL).rev() {
        let addr = base + idx * core::mem::size_of::<RegistryNode>();
        write_node(
            addr,
            RegistryNode {
                record: StoredAllocationRecord::empty(),
                next: head,
            },
        );
        head = addr;
    }
    Some((head, REGISTRY_NODE_REFILL))
}

fn hash_ptr(ptr: usize) -> usize {
    // SplitMix64 的终结混合能打散 slab 地址中的页号、size class 和对齐低位相关性。
    let mut value = ptr as u64;
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^= value >> 31;
    value as usize
}

fn bucket_index(hash: usize, bucket_count: usize) -> usize {
    debug_assert!(bucket_count.is_power_of_two());
    // 低位已经用于 shard 选择，桶索引从更高位继续取，减少同一 shard 内的系统性碰撞。
    (hash >> REGISTRY_SHARDS.trailing_zeros()) & (bucket_count - 1)
}

fn normalize_bucket_count(bucket_count: usize) -> usize {
    let requested = bucket_count.max(1);
    if requested.is_power_of_two() {
        requested
    } else {
        // 溢出时返回 usize 能表达的最大 2 的幂；随后的 Layout::array 会因为
        // 总大小不可表示而失败，保证不会用非 2 的幂桶数进入位掩码索引路径。
        requested
            .checked_next_power_of_two()
            .unwrap_or((usize::MAX >> 1) + 1)
    }
}

fn bucket_head(inner: &AllocationRegistryInner, bucket: usize) -> usize {
    unsafe { *inner.buckets.add(bucket) }
}

fn set_bucket_head(inner: &AllocationRegistryInner, bucket: usize, head: usize) {
    unsafe {
        inner.buckets.add(bucket).write(head);
    }
}

fn bucket_chain_len(inner: &AllocationRegistryInner, bucket: usize) -> usize {
    unsafe { *inner.bucket_lengths.add(bucket) }
}

fn set_bucket_chain_len(inner: &AllocationRegistryInner, bucket: usize, length: usize) {
    unsafe {
        inner.bucket_lengths.add(bucket).write(length);
    }
}

fn find_node(mut head: usize, ptr: usize, limit: usize) -> Result<Option<usize>, ()> {
    let mut visited = 0usize;
    while head != 0 {
        if visited >= limit {
            return Err(());
        }
        visited += 1;

        if read_node_ptr(head) == ptr {
            return Ok(Some(head));
        }
        head = read_node_next(head);
    }
    Ok(None)
}

fn find_node_and_chain_len(
    mut head: usize,
    ptr: usize,
    limit: usize,
) -> Result<(Option<usize>, usize), ()> {
    let mut len = 0;
    while head != 0 {
        if len >= limit {
            return Err(());
        }
        len += 1;
        if read_node_ptr(head) == ptr {
            return Ok((Some(head), len));
        }
        head = read_node_next(head);
    }
    Ok((None, len))
}

fn kind_index(kind: AllocationKind) -> usize {
    match kind {
        AllocationKind::Boot => 0,
        AllocationKind::Small => 1,
        AllocationKind::Large => 2,
        AllocationKind::Physical => 3,
    }
}

fn read_node(addr: usize) -> RegistryNode {
    unsafe { *(addr as *const RegistryNode) }
}

#[inline]
fn read_node_ptr(addr: usize) -> usize {
    unsafe { core::ptr::addr_of!((*(addr as *const RegistryNode)).record.ptr).read() }
}

#[inline]
fn read_node_next(addr: usize) -> usize {
    unsafe { core::ptr::addr_of!((*(addr as *const RegistryNode)).next).read() }
}

#[inline]
fn read_node_record(addr: usize) -> AllocationRecord {
    unsafe {
        core::ptr::addr_of!((*(addr as *const RegistryNode)).record)
            .read()
            .into_record()
    }
}

#[inline]
fn write_node_next(addr: usize, next: usize) {
    unsafe { core::ptr::addr_of_mut!((*(addr as *mut RegistryNode)).next).write(next) }
}

#[inline]
fn write_node_record(addr: usize, stored: StoredAllocationRecord) {
    unsafe { core::ptr::addr_of_mut!((*(addr as *mut RegistryNode)).record).write(stored) }
}

fn write_node(addr: usize, node: RegistryNode) {
    unsafe {
        (addr as *mut RegistryNode).write(node);
    }
}

#[cfg(feature = "ktest-kernel")]
mod compact_record_tests {
    use super::*;
    use crate::request::AllocationArena;
    use ktest::ktest;

    #[ktest]
    fn compact_record_round_trips_all_fields() {
        let original = AllocationRecord::new(
            AllocationKind::Large,
            MemoryDomain::Kernel,
            0xffff_8000_1234_0000,
        )
        .with_arena(AllocationArena::Kernel)
        .with_physical(0x2345_0000, 9, 2 * 1024 * 1024)
        .with_sizes(123_457, 2 * 1024 * 1024, 64 * 1024)
        .with_accounting_owner(0x1234_5678_9abc_def0)
        .with_backend_cookie(0x55aa_aa55);

        let stored = StoredAllocationRecord::try_from_record(original).expect("compact record");
        assert_eq!(stored.into_record(), original);
    }

    #[ktest]
    fn compact_record_rejects_noncanonical_layout() {
        let bad_align = AllocationRecord::new(AllocationKind::Small, MemoryDomain::Kernel, 1)
            .with_sizes(8, 8, 3);
        assert!(matches!(
            StoredAllocationRecord::try_from_record(bad_align),
            Err(RegistryError::InvalidRecord)
        ));

        let mut bad_page =
            AllocationRecord::new(AllocationKind::Physical, MemoryDomain::Physical, 2);
        bad_page.page_size = 0;
        assert!(matches!(
            StoredAllocationRecord::try_from_record(bad_page),
            Err(RegistryError::InvalidRecord)
        ));
    }

    #[ktest]
    fn compact_node_fits_one_cache_line_on_supported_targets() {
        assert!(core::mem::size_of::<RegistryNode>() <= 64);
        assert!(
            core::mem::size_of::<RegistryNode>()
                < core::mem::size_of::<AllocationRecord>() + core::mem::size_of::<usize>()
        );
    }
}

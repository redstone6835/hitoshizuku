//! 分配记录注册表。
//!
//! 这个模块维护“用户指针 -> 分配记录”的映射，用来支持以下关键能力：
//!
//! - `deallocate(ptr)` 时根据裸指针找回分配来源与布局信息；
//! - `realloc` 时判断对象是 boot/small/large/managed/physical 哪一路分配；
//! - 统计和调试时追踪当前活跃分配。
//!
//! 从设计上看，它是整个 allocator 的“账本”。真正的内存页由 buddy、slab、
//! kernel heap 或 managed allocator 持有，而注册表负责记账、查账和销账。
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

use spin::mutex::Mutex;

use crate::boot::BootAllocator;
use crate::error::RegistryError;
use crate::request::{AllocationKind, AllocationRecord, MemoryDomain};

const DEFAULT_BUCKETS: usize = 32768;
const REGISTRY_NODE_REFILL: usize = 64;
/// registry 固定分片数。保持 2 的幂，才能用哈希低位直接选择 shard。
const REGISTRY_SHARDS: usize = 64;
const INVALID_BUCKET: usize = usize::MAX;

#[derive(Clone, Copy, Debug, Default)]
pub struct AllocationRegistryStats {
    pub shard_count: usize,
    pub bucket_count: usize,
    pub nonempty_buckets: usize,
    pub max_shard_nonempty_buckets: usize,
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
    pub max_chain_len: usize,
    pub live_boot: usize,
    pub live_small: usize,
    pub live_large: usize,
    pub live_managed: usize,
    pub live_physical: usize,
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
    pub scanned_nonempty_buckets: usize,
    pub scanned_live_records: usize,
    pub scanned_free_nodes: usize,
    pub scanned_live_boot: usize,
    pub scanned_live_small: usize,
    pub scanned_live_large: usize,
    pub scanned_live_managed: usize,
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
    pub const NONEMPTY_BUCKET_LOOP: Self = Self(1 << 12);
    pub const NONEMPTY_BUCKET_MISMATCH: Self = Self(1 << 13);
    pub const EMPTY_BUCKET_LINKED: Self = Self(1 << 14);
    pub const NONEMPTY_LINK_MISMATCH: Self = Self(1 << 15);

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
    record: AllocationRecord,
    next: usize,
}

#[derive(Clone, Copy)]
struct RegistryBucket {
    head: usize,
    nonempty_next: usize,
    nonempty_prev: usize,
}

impl RegistryBucket {
    const fn empty() -> Self {
        Self {
            head: 0,
            nonempty_next: INVALID_BUCKET,
            nonempty_prev: INVALID_BUCKET,
        }
    }
}

struct AllocationRegistryInner {
    buckets: *mut RegistryBucket,
    bucket_count: usize,
    nonempty_bucket_head: usize,
    nonempty_bucket_count: usize,
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
    max_chain_len: usize,
    live_by_kind: [usize; 5],
    initializing: bool,
    initialized: bool,
}

impl AllocationRegistryInner {
    const fn new() -> Self {
        Self {
            buckets: null_mut(),
            bucket_count: 0,
            nonempty_bucket_head: INVALID_BUCKET,
            nonempty_bucket_count: 0,
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
            max_chain_len: 0,
            live_by_kind: [0; 5],
            initializing: false,
            initialized: false,
        }
    }
}

struct RegistryShard {
    /// 每个 shard 独立加锁。跨指针迁移通过 remove + register 两阶段完成，不同时持有
    /// 两个 shard 锁，避免未来 managed compact / realloc 路径出现锁顺序问题。
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
}

impl AllocationRegistry {
    pub const fn new() -> Self {
        Self {
            shards: [const { RegistryShard::new() }; REGISTRY_SHARDS],
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

            let layout = match Layout::array::<RegistryBucket>(buckets_per_shard) {
                Ok(layout) => layout,
                Err(_) => {
                    shard.inner.lock().initializing = false;
                    return false;
                }
            };
            let buckets = crate::alloc_internal_metadata(layout) as *mut RegistryBucket;
            if buckets.is_null() {
                shard.inner.lock().initializing = false;
                return false;
            }
            for idx in 0..buckets_per_shard {
                unsafe {
                    buckets.add(idx).write(RegistryBucket::empty());
                }
            }

            let mut inner = shard.inner.lock();
            if inner.initialized {
                inner.initializing = false;
                continue;
            }
            inner.buckets = buckets;
            inner.bucket_count = buckets_per_shard;
            inner.nonempty_bucket_head = INVALID_BUCKET;
            inner.nonempty_bucket_count = 0;
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
            inner.max_chain_len = 0;
            inner.live_by_kind = [0; 5];
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
        if record.ptr == 0 {
            let mut inner = self.shards[0].inner.lock();
            inner.insert_failures += 1;
            return Err(RegistryError::InvalidRecord);
        }

        let hash = hash_ptr(record.ptr);
        let shard = self.shard_for_hash(hash);
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
            let (duplicate, chain_len_before) = find_node_and_chain_len(head, record.ptr);
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

            write_node(node_addr, RegistryNode { record, next: head });
            set_bucket_head_locked(&mut inner, bucket, node_addr);
            if head != 0 {
                inner.collisions += 1;
            }
            inner.live_records += 1;
            inner.live_by_kind[kind_index(record.kind)] += 1;
            let chain_len = 1 + chain_len_before;
            if chain_len > inner.max_chain_len {
                inner.max_chain_len = chain_len;
            }
            return Ok(());
        }
    }

    pub fn get(&self, ptr: usize) -> Option<AllocationRecord> {
        self.get_result(ptr).ok()
    }

    pub fn get_result(&self, ptr: usize) -> Result<AllocationRecord, RegistryError> {
        let hash = hash_ptr(ptr);
        let mut inner = self.shard_for_hash(hash).inner.lock();
        if !inner.initialized {
            inner.lookup_misses += 1;
            return Err(RegistryError::NotInitialized);
        }
        let bucket = bucket_index(hash, inner.bucket_count);
        let Some(node_addr) = find_node(bucket_head(&inner, bucket), ptr) else {
            inner.lookup_misses += 1;
            return Err(RegistryError::UnknownPointer);
        };
        Ok(read_node(node_addr).record)
    }

    pub fn remove(&self, ptr: usize) -> Option<AllocationRecord> {
        self.remove_result(ptr).ok()
    }

    pub fn remove_result(&self, ptr: usize) -> Result<AllocationRecord, RegistryError> {
        let hash = hash_ptr(ptr);
        let mut inner = self.shard_for_hash(hash).inner.lock();
        if !inner.initialized {
            inner.remove_failures += 1;
            return Err(RegistryError::NotInitialized);
        }
        let bucket = bucket_index(hash, inner.bucket_count);
        let mut prev = 0usize;
        let mut current = bucket_head(&inner, bucket);
        while current != 0 {
            let node = read_node(current);
            if node.record.ptr == ptr {
                if prev == 0 {
                    set_bucket_head_locked(&mut inner, bucket, node.next);
                } else {
                    let mut prev_node = read_node(prev);
                    prev_node.next = node.next;
                    write_node(prev, prev_node);
                }

                let mut recycled = node;
                recycled.next = inner.free_nodes;
                write_node(current, recycled);
                inner.free_nodes = current;
                inner.free_node_count += 1;
                decrement_live_records_locked(&mut inner);
                let idx = kind_index(node.record.kind);
                decrement_live_kind_locked(&mut inner, idx);
                return Ok(node.record);
            }
            prev = current;
            current = node.next;
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
        while current != 0 {
            let mut node = read_node(current);
            if node.record.ptr == ptr {
                let old_record = node.record;
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
                node.record = new_record;
                write_node(current, node);
                return Ok((new_record, true));
            }
            current = node.next;
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
            let inner = shard.inner.lock();
            accumulate_stats_locked(&mut out, &inner);
        }
        out
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
            let inner = shard.inner.lock();
            let mut shard_flags = AllocationRegistryAuditFlags::empty();
            accumulate_stats_locked(&mut stats, &inner);

            if !inner.initialized {
                shard_flags.insert(AllocationRegistryAuditFlags::UNINITIALIZED_SHARD);
                out.corrupt_shards += 1;
                out.flags.insert(shard_flags);
                continue;
            }
            out.initialized_shards += 1;

            if inner.buckets.is_null() || inner.bucket_count == 0 {
                shard_flags.insert(AllocationRegistryAuditFlags::NULL_BUCKETS);
                out.corrupt_shards += 1;
                out.flags.insert(shard_flags);
                continue;
            }

            let scanned = audit_shard_locked(shard_idx, &inner, &mut shard_flags);
            out.scanned_nonempty_buckets += scanned.nonempty_buckets;
            out.scanned_live_records += scanned.live_records;
            out.scanned_free_nodes += scanned.free_nodes;
            out.scanned_live_boot += scanned.live_by_kind[kind_index(AllocationKind::Boot)];
            out.scanned_live_small += scanned.live_by_kind[kind_index(AllocationKind::Small)];
            out.scanned_live_large += scanned.live_by_kind[kind_index(AllocationKind::Large)];
            out.scanned_live_managed += scanned.live_by_kind[kind_index(AllocationKind::Managed)];
            out.scanned_live_physical += scanned.live_by_kind[kind_index(AllocationKind::Physical)];
            out.scanned_max_chain_len = out.scanned_max_chain_len.max(scanned.max_chain_len);

            if scanned.live_records != inner.live_records {
                shard_flags.insert(AllocationRegistryAuditFlags::LIVE_COUNT_MISMATCH);
            }
            if scanned.free_nodes != inner.free_node_count {
                shard_flags.insert(AllocationRegistryAuditFlags::FREE_COUNT_MISMATCH);
            }
            if scanned.nonempty_buckets != inner.nonempty_bucket_count {
                shard_flags.insert(AllocationRegistryAuditFlags::NONEMPTY_BUCKET_MISMATCH);
            }
            if scanned.live_by_kind != inner.live_by_kind {
                shard_flags.insert(AllocationRegistryAuditFlags::KIND_COUNT_MISMATCH);
            }
            if scanned.live_records.saturating_add(scanned.free_nodes) != inner.nodes_allocated {
                shard_flags.insert(AllocationRegistryAuditFlags::NODE_ACCOUNTING_MISMATCH);
            }
            if scanned.max_chain_len > inner.max_chain_len {
                shard_flags.insert(AllocationRegistryAuditFlags::MAX_CHAIN_MISMATCH);
            }
            if inner.accounting_underflows != 0 {
                shard_flags.insert(AllocationRegistryAuditFlags::ACCOUNTING_UNDERFLOW);
            }

            if !shard_flags.is_empty() {
                out.corrupt_shards += 1;
                out.flags.insert(shard_flags);
            }
        }
        AllocationRegistrySnapshot { stats, audit: out }
    }

    fn shard_for_hash(&self, hash: usize) -> &RegistryShard {
        &self.shards[hash & (REGISTRY_SHARDS - 1)]
    }
}

fn accumulate_stats_locked(out: &mut AllocationRegistryStats, inner: &AllocationRegistryInner) {
    out.bucket_count += inner.bucket_count;
    out.nonempty_buckets += inner.nonempty_bucket_count;
    out.max_shard_nonempty_buckets = out
        .max_shard_nonempty_buckets
        .max(inner.nonempty_bucket_count);
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
    out.max_chain_len = out.max_chain_len.max(inner.max_chain_len);
    out.live_boot += inner.live_by_kind[kind_index(AllocationKind::Boot)];
    out.live_small += inner.live_by_kind[kind_index(AllocationKind::Small)];
    out.live_large += inner.live_by_kind[kind_index(AllocationKind::Large)];
    out.live_managed += inner.live_by_kind[kind_index(AllocationKind::Managed)];
    out.live_physical += inner.live_by_kind[kind_index(AllocationKind::Physical)];
}

#[derive(Clone, Copy, Default)]
struct RegistryShardScan {
    nonempty_buckets: usize,
    live_records: usize,
    free_nodes: usize,
    live_by_kind: [usize; 5],
    max_chain_len: usize,
}

fn audit_shard_locked(
    shard_idx: usize,
    inner: &AllocationRegistryInner,
    flags: &mut AllocationRegistryAuditFlags,
) -> RegistryShardScan {
    let mut scan = RegistryShardScan::default();
    let mut bucket = inner.nonempty_bucket_head;
    let mut prev_bucket = INVALID_BUCKET;
    while bucket != INVALID_BUCKET {
        if bucket >= inner.bucket_count || scan.nonempty_buckets >= inner.bucket_count {
            flags.insert(AllocationRegistryAuditFlags::NONEMPTY_BUCKET_LOOP);
            break;
        }

        let meta = bucket_meta(inner, bucket);
        if meta.nonempty_prev != prev_bucket {
            flags.insert(AllocationRegistryAuditFlags::NONEMPTY_LINK_MISMATCH);
        }
        if meta.head == 0 {
            flags.insert(AllocationRegistryAuditFlags::EMPTY_BUCKET_LINKED);
        }

        scan.nonempty_buckets += 1;
        let mut current = meta.head;
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
        scan.max_chain_len = scan.max_chain_len.max(chain_len);
        prev_bucket = bucket;
        bucket = meta.nonempty_next;
    }

    let mut current = inner.free_nodes;
    while current != 0 {
        if inner.nodes_allocated == 0 || scan.free_nodes >= inner.nodes_allocated {
            flags.insert(AllocationRegistryAuditFlags::FREE_LIST_LOOP);
            break;
        }
        let node = read_node(current);
        scan.free_nodes += 1;
        current = node.next;
    }

    scan
}

fn pop_free_node_locked(inner: &mut AllocationRegistryInner) -> usize {
    let node_addr = inner.free_nodes;
    if node_addr != 0 {
        inner.free_nodes = read_node(node_addr).next;
        decrement_free_node_count_locked(inner);
    }
    node_addr
}

fn pop_node_from_list(head: &mut usize, count: &mut usize) -> usize {
    let node_addr = *head;
    if node_addr != 0 {
        *head = read_node(node_addr).next;
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

fn recycle_node_list_locked(inner: &mut AllocationRegistryInner, head: usize, count: usize) {
    if head == 0 {
        return;
    }
    let mut tail = head;
    loop {
        let node = read_node(tail);
        if node.next == 0 {
            break;
        }
        tail = node.next;
    }
    let mut tail_node = read_node(tail);
    tail_node.next = inner.free_nodes;
    write_node(tail, tail_node);
    inner.free_nodes = head;
    inner.free_node_count = inner.free_node_count.saturating_add(count);
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
                record: AllocationRecord::new(AllocationKind::Boot, MemoryDomain::Kernel, 0),
                next: head,
            },
        );
        head = addr;
    }
    Some((head, REGISTRY_NODE_REFILL))
}

fn hash_ptr(ptr: usize) -> usize {
    (ptr >> 3) ^ (ptr >> 11) ^ (ptr >> 19) ^ (ptr >> 27)
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
    bucket_meta(inner, bucket).head
}

fn set_bucket_head_locked(inner: &mut AllocationRegistryInner, bucket: usize, head: usize) {
    let old_head = bucket_head(inner, bucket);
    unsafe {
        (*inner.buckets.add(bucket)).head = head;
    }
    match (old_head == 0, head == 0) {
        (true, false) => link_nonempty_bucket_locked(inner, bucket),
        (false, true) => unlink_nonempty_bucket_locked(inner, bucket),
        _ => {}
    }
}

fn bucket_meta(inner: &AllocationRegistryInner, bucket: usize) -> RegistryBucket {
    unsafe { *inner.buckets.add(bucket) }
}

fn write_bucket_meta(inner: &AllocationRegistryInner, bucket: usize, meta: RegistryBucket) {
    unsafe {
        inner.buckets.add(bucket).write(meta);
    }
}

fn link_nonempty_bucket_locked(inner: &mut AllocationRegistryInner, bucket: usize) {
    let mut meta = bucket_meta(inner, bucket);
    if meta.nonempty_prev != INVALID_BUCKET || meta.nonempty_next != INVALID_BUCKET {
        return;
    }
    meta.nonempty_prev = INVALID_BUCKET;
    meta.nonempty_next = inner.nonempty_bucket_head;
    write_bucket_meta(inner, bucket, meta);

    if inner.nonempty_bucket_head != INVALID_BUCKET {
        let mut old_head = bucket_meta(inner, inner.nonempty_bucket_head);
        old_head.nonempty_prev = bucket;
        write_bucket_meta(inner, inner.nonempty_bucket_head, old_head);
    }
    inner.nonempty_bucket_head = bucket;
    inner.nonempty_bucket_count = inner.nonempty_bucket_count.saturating_add(1);
}

fn unlink_nonempty_bucket_locked(inner: &mut AllocationRegistryInner, bucket: usize) {
    let mut meta = bucket_meta(inner, bucket);
    let prev = meta.nonempty_prev;
    let next = meta.nonempty_next;

    if prev != INVALID_BUCKET {
        let mut prev_meta = bucket_meta(inner, prev);
        prev_meta.nonempty_next = next;
        write_bucket_meta(inner, prev, prev_meta);
    } else if inner.nonempty_bucket_head == bucket {
        inner.nonempty_bucket_head = next;
    }

    if next != INVALID_BUCKET {
        let mut next_meta = bucket_meta(inner, next);
        next_meta.nonempty_prev = prev;
        write_bucket_meta(inner, next, next_meta);
    }

    meta.nonempty_prev = INVALID_BUCKET;
    meta.nonempty_next = INVALID_BUCKET;
    write_bucket_meta(inner, bucket, meta);
    if decrement_registry_counter(&mut inner.nonempty_bucket_count) {
        inner.accounting_underflows = inner.accounting_underflows.saturating_add(1);
    }
}

fn find_node(mut head: usize, ptr: usize) -> Option<usize> {
    while head != 0 {
        let node = read_node(head);
        if node.record.ptr == ptr {
            return Some(head);
        }
        head = node.next;
    }
    None
}

fn find_node_and_chain_len(mut head: usize, ptr: usize) -> (Option<usize>, usize) {
    let mut len = 0;
    while head != 0 {
        len += 1;
        let node = read_node(head);
        if node.record.ptr == ptr {
            return (Some(head), len);
        }
        head = node.next;
    }
    (None, len)
}

fn kind_index(kind: AllocationKind) -> usize {
    match kind {
        AllocationKind::Boot => 0,
        AllocationKind::Small => 1,
        AllocationKind::Large => 2,
        AllocationKind::Managed => 3,
        AllocationKind::Physical => 4,
    }
}

fn read_node(addr: usize) -> RegistryNode {
    unsafe { *(addr as *const RegistryNode) }
}

fn write_node(addr: usize, node: RegistryNode) {
    unsafe {
        (addr as *mut RegistryNode).write(node);
    }
}

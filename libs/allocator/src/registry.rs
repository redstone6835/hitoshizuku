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
use core::alloc::Layout;
use core::ptr::null_mut;

use spin::mutex::Mutex;

use crate::boot::BootAllocator;
use crate::error::RegistryError;
use crate::request::{AllocationKind, AllocationRecord};

const DEFAULT_BUCKETS: usize = 4096;

#[derive(Clone, Copy, Debug, Default)]
pub struct AllocationRegistryStats {
    pub bucket_count: usize,
    pub live_records: usize,
    pub collisions: u64,
    pub lookup_misses: u64,
    pub insert_failures: u64,
    pub remove_failures: u64,
    pub duplicate_inserts: u64,
    pub double_free_attempts: u64,
    pub max_chain_len: usize,
    pub live_boot: usize,
    pub live_small: usize,
    pub live_large: usize,
    pub live_managed: usize,
    pub live_physical: usize,
}

#[derive(Clone, Copy)]
struct RegistryNode {
    record: AllocationRecord,
    next: usize,
}

struct AllocationRegistryInner {
    buckets: *mut usize,
    bucket_count: usize,
    free_nodes: usize,
    live_records: usize,
    collisions: u64,
    lookup_misses: u64,
    insert_failures: u64,
    remove_failures: u64,
    duplicate_inserts: u64,
    double_free_attempts: u64,
    max_chain_len: usize,
    live_by_kind: [usize; 5],
    initialized: bool,
}

pub struct AllocationRegistry {
    inner: Mutex<AllocationRegistryInner>,
}

impl AllocationRegistry {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(AllocationRegistryInner {
                buckets: null_mut(),
                bucket_count: 0,
                free_nodes: 0,
                live_records: 0,
                collisions: 0,
                lookup_misses: 0,
                insert_failures: 0,
                remove_failures: 0,
                duplicate_inserts: 0,
                double_free_attempts: 0,
                max_chain_len: 0,
                live_by_kind: [0; 5],
                initialized: false,
            }),
        }
    }

    pub fn init(&self, boot: &BootAllocator) -> bool {
        self.init_with_buckets(boot, DEFAULT_BUCKETS)
    }

    pub fn init_with_buckets(&self, boot: &BootAllocator, bucket_count: usize) -> bool {
        let mut inner = self.inner.lock();
        if inner.initialized {
            return true;
        }

        let layout = match Layout::array::<usize>(bucket_count.max(1)) {
            Ok(layout) => layout,
            Err(_) => return false,
        };
        let buckets = {
            let ptr = crate::alloc_internal_metadata(layout) as *mut usize;
            if ptr.is_null() {
                boot.alloc(layout) as *mut usize
            } else {
                ptr
            }
        };
        if buckets.is_null() {
            return false;
        }
        for idx in 0..bucket_count.max(1) {
            unsafe {
                buckets.add(idx).write(0);
            }
        }

        inner.buckets = buckets;
        inner.bucket_count = bucket_count.max(1);
        inner.free_nodes = 0;
        inner.live_records = 0;
        inner.collisions = 0;
        inner.lookup_misses = 0;
        inner.insert_failures = 0;
        inner.remove_failures = 0;
        inner.duplicate_inserts = 0;
        inner.double_free_attempts = 0;
        inner.max_chain_len = 0;
        inner.live_by_kind = [0; 5];
        inner.initialized = true;
        true
    }

    pub fn register_result(
        &self,
        boot: &BootAllocator,
        record: AllocationRecord,
    ) -> Result<(), RegistryError> {
        let mut inner = self.inner.lock();
        if !inner.initialized {
            inner.insert_failures += 1;
            return Err(RegistryError::NotInitialized);
        }
        if record.ptr == 0 {
            inner.insert_failures += 1;
            return Err(RegistryError::InvalidRecord);
        }

        let bucket = bucket_index(record.ptr, inner.bucket_count);
        let head = bucket_head(&inner, bucket);
        if head != 0 {
            inner.collisions += 1;
        }
        if find_node(head, record.ptr).is_some() {
            inner.insert_failures += 1;
            inner.duplicate_inserts += 1;
            return Err(RegistryError::DuplicatePointer);
        }

        let Some(node_addr) = alloc_node(&mut inner, boot) else {
            inner.insert_failures += 1;
            return Err(RegistryError::MetadataOutOfMemory);
        };

        write_node(node_addr, RegistryNode { record, next: head });
        set_bucket_head(&inner, bucket, node_addr);
        inner.live_records += 1;
        inner.live_by_kind[kind_index(record.kind)] += 1;
        let chain_len = 1 + chain_len(head);
        if chain_len > inner.max_chain_len {
            inner.max_chain_len = chain_len;
        }
        Ok(())
    }

    pub fn get(&self, ptr: usize) -> Option<AllocationRecord> {
        self.get_result(ptr).ok()
    }

    pub fn get_result(&self, ptr: usize) -> Result<AllocationRecord, RegistryError> {
        let mut inner = self.inner.lock();
        if !inner.initialized {
            inner.lookup_misses += 1;
            return Err(RegistryError::NotInitialized);
        }
        let bucket = bucket_index(ptr, inner.bucket_count);
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
        let mut inner = self.inner.lock();
        if !inner.initialized {
            inner.remove_failures += 1;
            return Err(RegistryError::NotInitialized);
        }
        let bucket = bucket_index(ptr, inner.bucket_count);
        let mut prev = 0usize;
        let mut current = bucket_head(&inner, bucket);
        while current != 0 {
            let node = read_node(current);
            if node.record.ptr == ptr {
                if prev == 0 {
                    set_bucket_head(&inner, bucket, node.next);
                } else {
                    let mut prev_node = read_node(prev);
                    prev_node.next = node.next;
                    write_node(prev, prev_node);
                }

                let mut recycled = node;
                recycled.next = inner.free_nodes;
                write_node(current, recycled);
                inner.free_nodes = current;
                inner.live_records = inner.live_records.saturating_sub(1);
                let idx = kind_index(node.record.kind);
                inner.live_by_kind[idx] = inner.live_by_kind[idx].saturating_sub(1);
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

    pub fn update_existing(&self, ptr: usize, new_record: AllocationRecord) -> bool {
        self.update_existing_result(ptr, new_record).is_ok()
    }

    pub fn update_existing_result(
        &self,
        ptr: usize,
        new_record: AllocationRecord,
    ) -> Result<(), RegistryError> {
        let mut inner = self.inner.lock();
        if !inner.initialized {
            inner.lookup_misses += 1;
            return Err(RegistryError::NotInitialized);
        }
        if new_record.ptr != ptr {
            inner.insert_failures += 1;
            return Err(RegistryError::InvalidRecord);
        }
        let bucket = bucket_index(ptr, inner.bucket_count);
        let mut current = bucket_head(&inner, bucket);
        while current != 0 {
            let mut node = read_node(current);
            if node.record.ptr == ptr {
                if node.record.kind != new_record.kind {
                    inner.live_by_kind[kind_index(node.record.kind)] =
                        inner.live_by_kind[kind_index(node.record.kind)].saturating_sub(1);
                    inner.live_by_kind[kind_index(new_record.kind)] += 1;
                }
                node.record = new_record;
                write_node(current, node);
                return Ok(());
            }
            current = node.next;
        }
        inner.lookup_misses += 1;
        Err(RegistryError::UnknownPointer)
    }

    pub fn stats(&self) -> AllocationRegistryStats {
        let inner = self.inner.lock();
        AllocationRegistryStats {
            bucket_count: inner.bucket_count,
            live_records: inner.live_records,
            collisions: inner.collisions,
            lookup_misses: inner.lookup_misses,
            insert_failures: inner.insert_failures,
            remove_failures: inner.remove_failures,
            duplicate_inserts: inner.duplicate_inserts,
            double_free_attempts: inner.double_free_attempts,
            max_chain_len: inner.max_chain_len,
            live_boot: inner.live_by_kind[kind_index(AllocationKind::Boot)],
            live_small: inner.live_by_kind[kind_index(AllocationKind::Small)],
            live_large: inner.live_by_kind[kind_index(AllocationKind::Large)],
            live_managed: inner.live_by_kind[kind_index(AllocationKind::Managed)],
            live_physical: inner.live_by_kind[kind_index(AllocationKind::Physical)],
        }
    }
}

fn alloc_node(inner: &mut AllocationRegistryInner, boot: &BootAllocator) -> Option<usize> {
    if inner.free_nodes != 0 {
        let node_addr = inner.free_nodes;
        let node = read_node(node_addr);
        inner.free_nodes = node.next;
        return Some(node_addr);
    }

    let ptr = {
        let ptr = crate::alloc_internal_metadata(Layout::new::<RegistryNode>()) as usize;
        if ptr == 0 {
            boot.alloc(Layout::new::<RegistryNode>()) as usize
        } else {
            ptr
        }
    };
    (ptr != 0).then_some(ptr)
}

fn bucket_index(ptr: usize, bucket_count: usize) -> usize {
    ((ptr >> 3) ^ (ptr >> 11) ^ (ptr >> 19)) % bucket_count.max(1)
}

fn bucket_head(inner: &AllocationRegistryInner, bucket: usize) -> usize {
    unsafe { *inner.buckets.add(bucket) }
}

fn set_bucket_head(inner: &AllocationRegistryInner, bucket: usize, head: usize) {
    unsafe {
        inner.buckets.add(bucket).write(head);
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

fn chain_len(mut head: usize) -> usize {
    let mut len = 0;
    while head != 0 {
        len += 1;
        head = read_node(head).next;
    }
    len
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

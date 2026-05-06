//! 内核大对象分配层。
//!
//! slab 擅长处理大量小对象，但对于页级对齐需求更高、尺寸更大的对象，继续使用 slab
//! 会带来明显的内部碎片和管理复杂度。这个模块专门负责那部分"大对象"分配请求。
//!
//! 它本身并不直接操作页表，也不自己实现物理页算法，而是组合两部分现有能力：
//!
//! - 通过 `KernelAddressSpace` 预留一段虚拟地址；
//! - 通过 `BuddyAllocator` 获得对应的物理页块；
//! - 再由架构层回调把二者映射在一起。
//!
//! 因而 `KernelHeap` 更像是一个策略层：它决定何时走大对象路径、如何统计使用情况、
//! 如何在 free/realloc 时把请求反向拆回 address space 与 buddy 两层。
use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::buddy::{BuddyAllocator, PAGE_SIZE};
use crate::error::{AllocationError, DeallocationError};
use crate::request::AllocationRecord;
use crate::request::PagePolicy;
use crate::space::{ArenaKind, BackedRange, KernelAddressSpace};

#[derive(Clone, Copy, Debug, Default)]
pub struct KernelHeapStats {
    pub alloc_requests: u64,
    pub free_requests: u64,
    pub realloc_requests: u64,
    pub active_allocs: u64,
    pub active_bytes: usize,
    pub active_pages: usize,
    pub alloc_failures: u64,
    pub address_reservation_failures: u64,
    pub invalid_frees: u64,
}

pub struct KernelHeap {
    initialized: AtomicBool,
    alloc_requests: AtomicU64,
    free_requests: AtomicU64,
    realloc_requests: AtomicU64,
    active_allocs: AtomicU64,
    active_bytes: AtomicUsize,
    active_pages: AtomicUsize,
    alloc_failures: AtomicU64,
    address_reservation_failures: AtomicU64,
    invalid_frees: AtomicU64,
}

impl KernelHeap {
    pub const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            alloc_requests: AtomicU64::new(0),
            free_requests: AtomicU64::new(0),
            realloc_requests: AtomicU64::new(0),
            active_allocs: AtomicU64::new(0),
            active_bytes: AtomicUsize::new(0),
            active_pages: AtomicUsize::new(0),
            alloc_failures: AtomicU64::new(0),
            address_reservation_failures: AtomicU64::new(0),
            invalid_frees: AtomicU64::new(0),
        }
    }

    pub fn init(&self) {
        self.initialized.store(true, Ordering::Release);
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> KernelHeapStats {
        KernelHeapStats {
            alloc_requests: self.alloc_requests.load(Ordering::Acquire),
            free_requests: self.free_requests.load(Ordering::Acquire),
            realloc_requests: self.realloc_requests.load(Ordering::Acquire),
            active_allocs: self.active_allocs.load(Ordering::Acquire),
            active_bytes: self.active_bytes.load(Ordering::Acquire),
            active_pages: self.active_pages.load(Ordering::Acquire),
            alloc_failures: self.alloc_failures.load(Ordering::Acquire),
            address_reservation_failures: self.address_reservation_failures.load(Ordering::Acquire),
            invalid_frees: self.invalid_frees.load(Ordering::Acquire),
        }
    }

    pub fn record_realloc(&self) {
        self.realloc_requests.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn is_debug_enabled() -> bool {
        log::get_log_level() >= log::LogLevel::Debug
    }

    pub fn alloc_range(
        &self,
        layout: Layout,
        page_policy: PagePolicy,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> Result<BackedRange, AllocationError> {
        let aligned = layout.pad_to_align();
        let (order, page_policy) = effective_layout_policy(layout, page_policy);
        let block_pages = 1usize << order;

        if Self::is_debug_enabled() {
            log::debug!(
                "[alloc][kheap] request size={} align={} order={} pages={} page_policy={:?}",
                aligned.size(),
                aligned.align(),
                order,
                block_pages,
                page_policy,
            );
        }

        if !self.is_initialized() {
            self.alloc_failures.fetch_add(1, Ordering::Relaxed);
            return Err(AllocationError::NotInitialized);
        }
        self.alloc_requests.fetch_add(1, Ordering::Relaxed);

        let range = match vmem.alloc_kernel_backed_range(order, phys, page_policy) {
            Ok(range) => range,
            Err(err) => {
                self.alloc_failures.fetch_add(1, Ordering::Relaxed);
                self.address_reservation_failures
                    .fetch_add(1, Ordering::Relaxed);
                if Self::is_debug_enabled() {
                    log::debug!(
                        "[alloc][kheap] failed size={} align={} order={} err={:?}",
                        aligned.size(),
                        aligned.align(),
                        order,
                        err,
                    );
                }
                return Err(AllocationError::AddressSpace(err));
            }
        };

        self.active_allocs.fetch_add(1, Ordering::Relaxed);
        self.active_pages.fetch_add(block_pages, Ordering::Relaxed);
        self.active_bytes
            .fetch_add(block_pages * PAGE_SIZE, Ordering::Relaxed);

        if Self::is_debug_enabled() {
            log::debug!(
                "[alloc][kheap] success vaddr={:#x} paddr={:#x} size={} order={} pages={}",
                range.vaddr,
                range.paddr,
                range.size,
                range.order,
                block_pages,
            );
        }
        Ok(range)
    }

    pub fn free_record(
        &self,
        record: AllocationRecord,
        phys: &crate::Mutex<BuddyAllocator>,
        vmem: &KernelAddressSpace,
    ) -> Result<(), DeallocationError> {
        self.free_requests.fetch_add(1, Ordering::Relaxed);
        if !self.is_initialized() {
            self.invalid_frees.fetch_add(1, Ordering::Relaxed);
            return Err(DeallocationError::UnknownPointer);
        }

        let paddr = match record.paddr {
            Some(paddr) => paddr,
            None => {
                self.invalid_frees.fetch_add(1, Ordering::Relaxed);
                return Err(DeallocationError::InvalidPointer);
            }
        };
        let order = record.order;

        if Self::is_debug_enabled() {
            log::debug!(
                "[alloc][kheap] free ptr={:#x} paddr={:#x} order={} size={}",
                record.ptr,
                paddr,
                order,
                (1usize << order) * PAGE_SIZE,
            );
        }

        vmem.free_kernel_backed_range(
            BackedRange {
                arena: ArenaKind::Kernel,
                vaddr: record.ptr,
                paddr,
                size: (1usize << order) * PAGE_SIZE,
                order,
            },
            phys,
        )
        .map_err(|err| {
            self.invalid_frees.fetch_add(1, Ordering::Relaxed);
            DeallocationError::AddressSpace(err)
        })?;

        self.active_allocs.fetch_sub(1, Ordering::Relaxed);
        self.active_pages
            .fetch_sub(1usize << order, Ordering::Relaxed);
        self.active_bytes
            .fetch_sub((1usize << order) * PAGE_SIZE, Ordering::Relaxed);
        Ok(())
    }

    pub fn required_order_for(layout: Layout) -> usize {
        required_order(layout)
    }
}

impl Default for KernelHeap {
    fn default() -> Self {
        Self::new()
    }
}

fn required_order(layout: Layout) -> usize {
    let aligned = layout.pad_to_align();
    let size_pages = pages_for(aligned.size());
    let align_pages = pages_for(aligned.align().max(PAGE_SIZE));
    pages_to_order(size_pages.max(align_pages))
}

fn effective_layout_policy(layout: Layout, requested: PagePolicy) -> (usize, PagePolicy) {
    const MIN_LARGE_PAGE_ORDER: usize = 9; // 2 MiB

    let mut order = required_order(layout);
    let page_policy = match requested {
        PagePolicy::RequireLarge => {
            order = order.max(MIN_LARGE_PAGE_ORDER);
            PagePolicy::RequireLarge
        }
        PagePolicy::PreferLarge => PagePolicy::PreferLarge,
        PagePolicy::BaseOnly if order >= MIN_LARGE_PAGE_ORDER => PagePolicy::PreferLarge,
        PagePolicy::BaseOnly => PagePolicy::BaseOnly,
    };
    (order, page_policy)
}

#[inline]
fn pages_for(bytes: usize) -> usize {
    bytes.max(1).div_ceil(PAGE_SIZE).max(1)
}

#[inline]
fn pages_to_order(pages: usize) -> usize {
    let mut order = 0;
    let mut block = 1;
    while block < pages {
        block <<= 1;
        order += 1;
    }
    order
}

//! 分配器内部元数据分配器。
//!
//! 这个模块专门服务于“分配器自己要用的内存”，例如链表节点、注册表桶数组、内部
//! 管理结构等。它和面向普通调用者的对象分配路径不同，目标不是提供通用 malloc，
//! 而是保证 allocator 在初始化早期和初始化完成后都能稳定获得少量内部工作内存。
//!
//! 它的生命周期分成两个阶段：
//!
//! 1. **boot 阶段。**
//!    使用 `BootAllocator` 直接从一段线性区域 bump 分配，逻辑简单、依赖最少。
//! 2. **dynamic 阶段。**
//!    当物理页分配器已经可用时，从 buddy 中申请整页作为 metadata backing store，
//!    继续以线性游标方式切分给内部结构使用。
//!
//! 这样做的目的，是把“分配器初始化自身所需的少量内存”从通用分配路径中抽出来，
//! 避免形成自举死锁：如果 metadata 还要依赖完整 allocator 才能分配，allocator
//! 自己就无法完成初始化。
use core::alloc::Layout;
use core::ptr::null_mut;

use crate::Mutex;

use crate::boot::BootAllocator;
use crate::buddy::{BuddyAllocator, MAX_TRACKED_ORDER, PAGE_SIZE};

/// allocator 自身的元数据使用统计。
#[derive(Clone, Copy, Debug, Default)]
pub struct MetadataStats {
    pub backing_pages: usize,
    pub allocated_bytes: usize,
    pub boot_allocations: u64,
    pub dynamic_allocations: u64,
}

struct MetadataInner {
    boot_raw: usize,
    dynamic_enabled: bool,
    cursor: usize,
    end: usize,
    stats: MetadataStats,
}

/// allocator 内部元数据分配器。
///
/// 这个对象只给 allocator 自己服务，不直接暴露给普通调用者。它的目标是稳定、可预测，
/// 而不是通用或高吞吐。
pub struct MetadataAllocator {
    inner: Mutex<MetadataInner>,
}

impl MetadataAllocator {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(MetadataInner {
                boot_raw: 0,
                dynamic_enabled: false,
                cursor: 0,
                end: 0,
                stats: MetadataStats {
                    backing_pages: 0,
                    allocated_bytes: 0,
                    boot_allocations: 0,
                    dynamic_allocations: 0,
                },
            }),
        }
    }

    pub fn bind_boot_source(&self, boot: &BootAllocator) {
        let mut inner = self.inner.lock();
        inner.boot_raw = boot as *const BootAllocator as usize;
    }

    pub fn enable_dynamic(&self) {
        let mut inner = self.inner.lock();
        inner.dynamic_enabled = true;
    }

    pub fn stats(&self) -> MetadataStats {
        self.inner.lock().stats
    }

    pub fn alloc(
        &self,
        layout: Layout,
        phys: &Mutex<BuddyAllocator>,
        phys_to_virt: Option<fn(usize) -> usize>,
    ) -> *mut u8 {
        let layout = layout.pad_to_align();
        let size = layout.size().max(1);
        let align = layout.align().max(1);

        let mut inner = self.inner.lock();
        if let Some(ptr) = try_alloc_from_window(&mut inner, size, align) {
            inner.stats.allocated_bytes += size;
            inner.stats.dynamic_allocations += 1;
            return ptr as *mut u8;
        }

        if inner.dynamic_enabled {
            let Some(phys_to_virt) = phys_to_virt else {
                return null_mut();
            };
            let Some(order) = order_for_bytes(size.max(align)) else {
                return null_mut();
            };
            drop(inner);

            let paddr = {
                let mut phys = phys.lock();
                match phys.alloc_pages(order) {
                    Some(paddr) => paddr,
                    None => return null_mut(),
                }
            };
            let base = phys_to_virt(paddr);
            let span_size = (1usize << order) * PAGE_SIZE;
            let Some(span_end) = base.checked_add(span_size) else {
                let mut phys = phys.lock();
                let _ = phys.free_pages(paddr, order);
                return null_mut();
            };
            unsafe {
                core::ptr::write_bytes(base as *mut u8, 0, span_size);
            }

            let mut inner = self.inner.lock();
            if let Some(ptr) = try_alloc_from_window(&mut inner, size, align) {
                inner.stats.allocated_bytes += size;
                inner.stats.dynamic_allocations += 1;
                drop(inner);
                let mut phys = phys.lock();
                let _ = phys.free_pages(paddr, order);
                return ptr as *mut u8;
            }

            inner.cursor = base;
            inner.end = span_end;
            inner.stats.backing_pages += 1usize << order;
            if let Some(ptr) = try_alloc_from_window(&mut inner, size, align) {
                inner.stats.allocated_bytes += size;
                inner.stats.dynamic_allocations += 1;
                return ptr as *mut u8;
            }
            drop(inner);
            let mut phys = phys.lock();
            let _ = phys.free_pages(paddr, order);
            return null_mut();
        }

        let boot_raw = inner.boot_raw;
        drop(inner);

        match boot_source(boot_raw) {
            Some(boot) => {
                let ptr = boot.alloc(layout);
                if !ptr.is_null() {
                    let mut inner = self.inner.lock();
                    inner.stats.allocated_bytes += size;
                    inner.stats.boot_allocations += 1;
                }
                ptr
            }
            None => null_mut(),
        }
    }
}

fn boot_source(raw: usize) -> Option<&'static BootAllocator> {
    if raw == 0 {
        None
    } else {
        Some(unsafe { &*(raw as *const BootAllocator) })
    }
}

fn try_alloc_from_window(inner: &mut MetadataInner, size: usize, align: usize) -> Option<usize> {
    if inner.cursor == 0 || inner.cursor >= inner.end {
        return None;
    }
    let aligned = align_up(inner.cursor, align)?;
    let next = aligned.checked_add(size)?;
    if next > inner.end {
        return None;
    }
    inner.cursor = next;
    Some(aligned)
}

fn order_for_bytes(bytes: usize) -> Option<usize> {
    let mut order = 0usize;
    let mut span = PAGE_SIZE;
    while span < bytes {
        if order >= MAX_TRACKED_ORDER {
            return None;
        }
        span = span.checked_shl(1)?;
        order += 1;
    }
    Some(order)
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    if align == 0 || (align & (align - 1)) != 0 {
        return None;
    }
    Some(value.checked_add(align - 1)? & !(align - 1))
}

//! 启动阶段线性分配器。
//!
//! 内核在物理页分配器、虚拟地址空间和正式对象分配路径全部就绪之前，仍然需要一小段
//! "先活下来再说"的临时内存。它用来存放早期日志缓冲区、内部管理结构的元数据，以及各种
//! 初始化阶段必须提前分配的数据结构。
//!
//! `BootAllocator` 就是这条最早期路径的实现。它不支持释放、不做复杂的回收，也不追求
//! 通用性，只在一段预先划定好的连续区间里，按照请求的对齐要求单调向前推进游标。
//!
//! 它存在的核心价值不是功能丰富，而是依赖极少。正因为它足够简单，allocator 的其余
//! 组件才能顺利完成自举，并在后续切换到更加正式的分配体系。
use core::alloc::Layout;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// 启动期分配器的使用统计快照。
///
/// 因为 `BootAllocator` 从不开释已分配的内存，所以只需要记录总容量、已用量和剩余量。
#[derive(Clone, Copy, Debug, Default)]
pub struct BootStats {
    /// 启动期内存区域的总字节数。
    pub total_bytes: usize,
    /// 已经分配出去的字节数。
    pub used_bytes: usize,
    /// 剩余可分配的字节数。
    pub free_bytes: usize,
}

/// 启动期 bump 分配器。
///
/// 它的模型非常朴素：给定一段连续区间，内部维护一个单调递增的游标。每次分配时，
/// 按请求的对齐和大小向前推进游标，并返回对齐后的地址。因为它从不回收，所以只有
/// 在系统自举阶段使用才是安全的。
pub struct BootAllocator {
    /// 启动区间的起始地址。
    start: AtomicUsize,
    /// 启动区间的结束地址（不包含）。
    end: AtomicUsize,
    /// 当前分配游标，指向下一个可分配地址。
    pos: AtomicUsize,
    /// 是否已经完成初始化。
    initialized: AtomicBool,
    /// 是否已经结束启动期分配。
    ///
    /// 正式 allocator 接管后，boot allocator 的未用尾部会被归还给 buddy。此后继续
    /// 从 boot 区域分配会破坏 buddy 所有权，因此 sealed 状态下 `alloc()` 必须失败。
    sealed: AtomicBool,
}

impl BootAllocator {
    /// 创建一个未初始化的启动期分配器。
    ///
    /// 在调用 [`init`](Self::init) 之前，该分配器无法分配任何内存。
    pub const fn new() -> Self {
        Self {
            start: AtomicUsize::new(0),
            end: AtomicUsize::new(0),
            pos: AtomicUsize::new(0),
            initialized: AtomicBool::new(false),
            sealed: AtomicBool::new(false),
        }
    }

    /// 初始化启动期分配器。
    ///
    /// 传入一段物理地址或预留虚拟地址区间后，分配器就可以开始工作。
    /// `start` 是区间起始地址，`size` 是区间大小（字节）。
    pub fn init(&self, start: usize, size: usize) {
        let end = start.saturating_add(size);
        self.start.store(start, Ordering::Release);
        self.pos.store(start, Ordering::Release);
        self.end.store(end, Ordering::Release);
        self.sealed.store(false, Ordering::Release);
        self.initialized.store(true, Ordering::Release);
    }

    /// 检查启动期分配器是否已经完成初始化。
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// 从启动期堆中分配一块内存。
    ///
    /// 使用无锁 CAS 循环推进游标，保证在单核或尚未建立正式锁机制的多核早期阶段也能
    /// 安全使用。如果剩余空间不足，返回空指针。
    pub fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.is_initialized() || self.sealed.load(Ordering::Acquire) {
            return null_mut();
        }

        let size = layout.pad_to_align().size().max(1);
        let align = layout.align().max(1);

        loop {
            let pos = self.pos.load(Ordering::Relaxed);
            let end = self.end.load(Ordering::Acquire);
            let aligned = align_up(pos, align);
            let next = aligned.saturating_add(size);
            if next > end {
                return null_mut();
            }

            match self
                .pos
                .compare_exchange_weak(pos, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return aligned as *mut u8,
                Err(_) => continue,
            }
        }
    }

    /// 结束启动期分配，并取出可归还给正式物理页分配器的整页尾部。
    ///
    /// 返回的区间仍是 boot allocator 原本记录的地址空间（当前平台为直接映射虚拟地址），
    /// 调用方负责在交给 buddy 前转换成物理地址。只归还 `pos` 之后的完整页；包含已用
    /// 字节的部分页会保留为 boot used，避免释放仍可能存放早期元数据的页。
    pub fn seal_and_take_free_tail(&self, page_size: usize) -> Option<BootReclaimRange> {
        if !self.is_initialized() || page_size == 0 || !page_size.is_power_of_two() {
            self.sealed.store(true, Ordering::Release);
            return None;
        }

        self.sealed.store(true, Ordering::Release);

        let start = self.start.load(Ordering::Acquire);
        let end = self.end.load(Ordering::Acquire);
        let pos = self.pos.load(Ordering::Acquire).clamp(start, end);
        let reclaim_start = align_up(pos, page_size).min(end);
        let reclaim_end = align_down(end, page_size);

        // 收缩统计窗口，使 /proc/meminfo 不再把已移交的尾部显示为 BootFree。
        self.pos.store(reclaim_start, Ordering::Release);
        self.end.store(reclaim_start, Ordering::Release);

        if reclaim_start >= reclaim_end {
            return None;
        }
        Some(BootReclaimRange {
            start: reclaim_start,
            size: reclaim_end - reclaim_start,
        })
    }

    /// 检查给定地址是否落在启动期内存区间内。
    pub fn contains(&self, addr: usize) -> bool {
        if !self.is_initialized() {
            return false;
        }
        let start = self.start.load(Ordering::Acquire);
        let end = self.end.load(Ordering::Acquire);
        addr >= start && addr < end
    }

    /// 获取启动期分配器的使用统计快照。
    ///
    /// 返回当前的总量、已用量和剩余可用量。
    pub fn snapshot(&self) -> BootStats {
        if !self.is_initialized() {
            return BootStats::default();
        }

        let start = self.start.load(Ordering::Acquire);
        let end = self.end.load(Ordering::Acquire);
        let pos = self.pos.load(Ordering::Acquire);
        let total = end.saturating_sub(start);
        let used = pos.saturating_sub(start).min(total);
        BootStats {
            total_bytes: total,
            used_bytes: used,
            free_bytes: total.saturating_sub(used),
        }
    }
}

/// Boot allocator 已移交给正式物理页分配器的地址区间。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BootReclaimRange {
    pub start: usize,
    pub size: usize,
}

impl BootReclaimRange {
    pub const fn end(self) -> usize {
        self.start.saturating_add(self.size)
    }
}

/// 将给定值向上对齐到指定对齐边界。
///
/// `align` 必须是 2 的幂，调用者负责保证这一点。
#[inline]
fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

#[inline]
fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

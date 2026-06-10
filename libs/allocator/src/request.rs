//! 分配请求与分配记录的公共数据模型。
//!
//! allocator 内部有多条分配路径：boot、slab、kernel heap、managed、physical。
//! 如果每条路径都使用自己的一套参数与记录结构，上层接口会很快失去一致性，调试时也
//! 很难判断一次分配到底走了哪条线路。
//!
//! 这个模块的作用，就是把“请求什么”和“最后发生了什么”标准化：
//!
//! - `MemoryRequest` / `PhysicalAllocRequest` 描述调用者的意图；
//! - `AllocationRecord` 描述 allocator 最终采用的路径、布局、页级信息和附加属性。
//!
//! 这样一来，路由逻辑可以根据统一请求模型做决策，而释放、重分配和统计代码也能
//! 依赖统一记录格式，不需要再回头猜测原始上下文。
use core::alloc::Layout;
use core::fmt;

use crate::buddy::{MAX_TRACKED_ORDER, PAGE_SIZE};
use crate::gc::TraceDescriptor;

/// 内存请求所属的逻辑域。
///
/// 这个枚举决定 allocator 应该把一次请求路由到哪类后端：
///
/// - `Kernel`：普通内核对象与缓冲区，优先走 slab / kheap；
/// - `Managed`：受 GC 管理的对象，走 managed allocator；
/// - `Physical`：调用者直接请求物理页，不附带内核堆映射语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryDomain {
    Kernel,
    Managed,
    Physical,
}

/// allocator 最终采用的分配路径。
///
/// 它记录“这块内存是怎么来的”，而不是“调用者最初想要什么”。
/// 这对释放、重分配和统计非常关键，因为同一个 API 请求最终可能根据大小、对齐、
/// 初始化阶段或 page policy 被路由到不同实现。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocationKind {
    Boot,
    Small,
    Large,
    Managed,
    Physical,
}

/// 记录对象所在的虚拟地址 arena。
///
/// 这个概念与 `MemoryDomain` 有联系，但不完全相同：
/// `MemoryDomain` 面向外部语义，而 `AllocationArena` 面向内部地址空间布局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocationArena {
    DirectMap,
    Kernel,
    Managed,
}

/// 页级映射策略。
///
/// 它只影响那些最终需要建立页表映射的路径，例如 kernel heap 大对象或显式物理页映射。
/// 小对象 slab 通常不会直接关心这个字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PagePolicy {
    BaseOnly,
    PreferLarge,
    RequireLarge,
}

/// 物理页放置策略。
///
/// 默认情况下，buddy 可以在满足大小与对齐的前提下自由选择物理位置；如果上层需要
/// 指定确切物理地址，或者要求落在某个低端物理区间，则通过这里表达。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryPlacement {
    Any,
    LowMem,
    ExactPhys(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReclaimPolicy {
    NoReclaim,
    TryManagedGc,
    TryAllocatorReclaim,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zeroing {
    Uninitialized,
    Zeroed,
}

/// 分配请求在进入后端前的规范化错误。
///
/// 这里刻意放在 request 层，而不是让 slab/kheap/buddy 各自解释非法参数：外部扩展
/// 只要拿到一个请求对象，就可以先调用 `validate()` / `layout()` / `required_order()`
/// 做同一套边界检查，避免不同后端对 size=0、非 2 次幂对齐或超大 order 给出互相
/// 矛盾的行为。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocationRequestError {
    InvalidSize,
    InvalidAlignment,
    SizeOverflow,
    UnsupportedOrder,
    InvalidPlacement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManagedAllocFlags {
    pub pinned: bool,
    pub finalizer_id: Option<u16>,
    pub trace_descriptor: Option<&'static TraceDescriptor>,
}

impl ManagedAllocFlags {
    pub const fn new() -> Self {
        Self {
            pinned: false,
            finalizer_id: None,
            trace_descriptor: None,
        }
    }

    pub const fn pinned(mut self, pinned: bool) -> Self {
        self.pinned = pinned;
        self
    }

    pub const fn with_finalizer(mut self, finalizer_id: u16) -> Self {
        self.finalizer_id = Some(finalizer_id);
        self
    }

    pub const fn with_trace_descriptor(
        mut self,
        trace_descriptor: &'static TraceDescriptor,
    ) -> Self {
        self.trace_descriptor = Some(trace_descriptor);
        self
    }
}

impl Default for ManagedAllocFlags {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRequest {
    pub domain: MemoryDomain,
    pub size: usize,
    pub align: usize,
    pub page_policy: PagePolicy,
    pub placement: MemoryPlacement,
    pub reclaim: ReclaimPolicy,
    pub zeroing: Zeroing,
    pub managed: ManagedAllocFlags,
}

impl MemoryRequest {
    pub const fn new(domain: MemoryDomain, size: usize, align: usize) -> Self {
        Self {
            domain,
            size,
            align,
            page_policy: PagePolicy::BaseOnly,
            placement: MemoryPlacement::Any,
            reclaim: ReclaimPolicy::TryManagedGc,
            zeroing: Zeroing::Uninitialized,
            managed: ManagedAllocFlags::new(),
        }
    }

    pub fn for_kernel_layout(layout: Layout) -> Self {
        let aligned = layout.pad_to_align();
        Self::new(
            MemoryDomain::Kernel,
            aligned.size().max(1),
            aligned.align().max(1),
        )
    }

    pub fn for_managed_layout(layout: Layout) -> Self {
        let aligned = layout.pad_to_align();
        Self::new(
            MemoryDomain::Managed,
            aligned.size().max(1),
            aligned.align().max(1),
        )
    }

    pub const fn with_zeroing(mut self, zeroing: Zeroing) -> Self {
        self.zeroing = zeroing;
        self
    }

    pub const fn with_page_policy(mut self, page_policy: PagePolicy) -> Self {
        self.page_policy = page_policy;
        self
    }

    pub const fn with_placement(mut self, placement: MemoryPlacement) -> Self {
        self.placement = placement;
        self
    }

    pub const fn with_reclaim(mut self, reclaim: ReclaimPolicy) -> Self {
        self.reclaim = reclaim;
        self
    }

    pub const fn with_managed_flags(mut self, managed: ManagedAllocFlags) -> Self {
        self.managed = managed;
        self
    }

    /// 校验通用内存请求的基础 layout 约束。
    ///
    /// typed allocator API 不再把 `size=0` 或 `align=0` 静默改写成 1。这样做可以把调用方
    /// bug 挡在入口处，也避免 registry 中出现“实际分配 1 字节、记录大小 0 字节”的对象。
    pub fn validate(self) -> Result<Self, AllocationRequestError> {
        self.layout()?;
        Ok(self)
    }

    /// 将请求转换成 Rust `Layout`，同时保留 allocator 自己的错误语义。
    pub fn layout(self) -> Result<Layout, AllocationRequestError> {
        if self.size == 0 {
            return Err(AllocationRequestError::InvalidSize);
        }
        if self.align == 0 || !self.align.is_power_of_two() {
            return Err(AllocationRequestError::InvalidAlignment);
        }
        Layout::from_size_align(self.size, self.align)
            .map_err(|_| AllocationRequestError::SizeOverflow)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalAllocRequest {
    pub size: usize,
    pub align: usize,
    pub page_policy: PagePolicy,
    pub placement: MemoryPlacement,
}

impl PhysicalAllocRequest {
    pub const fn new(size: usize, align: usize) -> Self {
        Self {
            size,
            align,
            page_policy: PagePolicy::BaseOnly,
            placement: MemoryPlacement::Any,
        }
    }

    pub const fn with_page_policy(mut self, page_policy: PagePolicy) -> Self {
        self.page_policy = page_policy;
        self
    }

    pub const fn with_placement(mut self, placement: MemoryPlacement) -> Self {
        self.placement = placement;
        self
    }

    /// 校验物理页请求，并返回原请求方便链式使用。
    pub fn validate(self) -> Result<Self, AllocationRequestError> {
        self.required_order()?;
        Ok(self)
    }

    /// 计算满足 size、align、page policy 和 exact placement 的 buddy order。
    ///
    /// 这是物理页 API 的标准入口。它带上了上界检查，避免极大 size 让 order 推导时左移
    /// 溢出或陷入循环；buddy 后端和外部扩展都应复用这里的结果语义。
    pub fn required_order(self) -> Result<usize, AllocationRequestError> {
        const MIN_LARGE_PAGE_ORDER: usize = 9;

        if self.size == 0 {
            return Err(AllocationRequestError::InvalidSize);
        }
        if self.align == 0 || !self.align.is_power_of_two() {
            return Err(AllocationRequestError::InvalidAlignment);
        }

        let size_pages = pages_for_checked(self.size)?;
        let align_pages = pages_for_checked(self.align.max(PAGE_SIZE))?;
        let min_pages = size_pages.max(align_pages);
        let mut order = pages_to_order_bounded(min_pages)?;
        if matches!(self.page_policy, PagePolicy::RequireLarge) {
            order = order.max(MIN_LARGE_PAGE_ORDER);
        }
        if order > MAX_TRACKED_ORDER {
            return Err(AllocationRequestError::UnsupportedOrder);
        }

        if let MemoryPlacement::ExactPhys(addr) = self.placement {
            let block_size = block_size_for_order(order)?;
            if addr & (block_size - 1) != 0 {
                return Err(AllocationRequestError::InvalidPlacement);
            }
        }
        Ok(order)
    }

    /// 返回该物理页请求最终会保留的字节数。
    pub fn reserved_size(self) -> Result<usize, AllocationRequestError> {
        block_size_for_order(self.required_order()?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalAllocation {
    pub paddr: usize,
    pub size: usize,
    pub order: usize,
    pub page_size: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AllocationRecord {
    pub kind: AllocationKind,
    pub domain: MemoryDomain,
    pub arena: Option<AllocationArena>,
    pub ptr: usize,
    pub paddr: Option<usize>,
    pub size: usize,
    pub usable_size: usize,
    pub align: usize,
    pub order: usize,
    pub page_size: usize,
    /// 后端私有定位 cookie。
    ///
    /// 这个字段不属于外部所有权语义，只给 allocator 内部热路径使用。例如 slab 会在
    /// registry record 里保存所属 `SlabNode` 地址，释放时即可直接回到对应 slab，而不必
    /// 再按 size class 扫描整条 slab 链。外部扩展应通过公开字段理解对象属性，不能依赖
    /// 该值的格式或稳定性。
    pub(crate) backend_cookie: usize,
}

impl AllocationRecord {
    pub const fn new(kind: AllocationKind, domain: MemoryDomain, ptr: usize) -> Self {
        Self {
            kind,
            domain,
            arena: None,
            ptr,
            paddr: None,
            size: 0,
            usable_size: 0,
            align: 1,
            order: 0,
            page_size: PAGE_SIZE,
            backend_cookie: 0,
        }
    }

    pub const fn with_arena(mut self, arena: AllocationArena) -> Self {
        self.arena = Some(arena);
        self
    }

    pub const fn with_physical(mut self, paddr: usize, order: usize, page_size: usize) -> Self {
        self.paddr = Some(paddr);
        self.order = order;
        self.page_size = page_size;
        self
    }

    pub const fn with_sizes(mut self, size: usize, usable_size: usize, align: usize) -> Self {
        self.size = size;
        self.usable_size = usable_size;
        self.align = align;
        self
    }

    pub(crate) const fn with_backend_cookie(mut self, backend_cookie: usize) -> Self {
        self.backend_cookie = backend_cookie;
        self
    }
}

impl fmt::Debug for AllocationRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // backend_cookie 可能保存 slab 元数据地址。它只参与 allocator 内部快速定位和一致性
        // 比较，不应在普通日志或外部 Debug 输出中泄露。
        f.debug_struct("AllocationRecord")
            .field("kind", &self.kind)
            .field("domain", &self.domain)
            .field("arena", &self.arena)
            .field("ptr", &self.ptr)
            .field("paddr", &self.paddr)
            .field("size", &self.size)
            .field("usable_size", &self.usable_size)
            .field("align", &self.align)
            .field("order", &self.order)
            .field("page_size", &self.page_size)
            .finish()
    }
}

#[inline]
fn pages_for_checked(bytes: usize) -> Result<usize, AllocationRequestError> {
    if bytes == 0 {
        return Err(AllocationRequestError::InvalidSize);
    }
    Ok(bytes.div_ceil(PAGE_SIZE).max(1))
}

#[inline]
fn pages_to_order_bounded(pages: usize) -> Result<usize, AllocationRequestError> {
    let max_pages = 1usize
        .checked_shl(MAX_TRACKED_ORDER as u32)
        .ok_or(AllocationRequestError::UnsupportedOrder)?;
    if pages > max_pages {
        return Err(AllocationRequestError::UnsupportedOrder);
    }

    let mut order = 0usize;
    let mut block = 1usize;
    while block < pages {
        if order >= MAX_TRACKED_ORDER {
            return Err(AllocationRequestError::UnsupportedOrder);
        }
        block <<= 1;
        order += 1;
    }
    Ok(order)
}

#[inline]
fn block_size_for_order(order: usize) -> Result<usize, AllocationRequestError> {
    if order > MAX_TRACKED_ORDER {
        return Err(AllocationRequestError::UnsupportedOrder);
    }
    (1usize
        .checked_shl(order as u32)
        .and_then(|pages| pages.checked_mul(PAGE_SIZE)))
    .ok_or(AllocationRequestError::SizeOverflow)
}

//! ELM 普通内核内存服务的稳定函数表。
//!
//! `kernel.memory@1` 只开放由 allocator 逐对象跟踪的普通 Kernel 域分配。物理页、
//! boot allocator、受管堆和 allocator 内部缓存均不属于该契约。每个返回地址都绑定到
//! 发起调用的 ELM cell；其它 cell 即使猜中地址，也不能查询、调整或释放该对象。

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

use crate::{ApiGrantTokenV1, ApiImport, ApiTableHeaderV1, KernelApiTable};

/// 内存服务的规范命名空间 identifier。
pub const KERNEL_MEMORY_API_IDENTIFIER: &str = "kernel.memory";
/// 内存服务当前唯一的 ABI 版本。
pub const KERNEL_MEMORY_API_VERSION: u16 = 1;

/// 允许创建和释放普通 Kernel 域分配。
pub const KERNEL_MEMORY_CAP_ALLOCATE: u64 = 1 << 0;
/// 允许调整当前 cell 所有分配的大小。
pub const KERNEL_MEMORY_CAP_RESIZE: u64 = 1 << 1;
/// 允许查询当前 cell 所有分配的布局信息。
pub const KERNEL_MEMORY_CAP_QUERY: u64 = 1 << 2;
/// 允许读取当前 cell 的内存账本和受限的全局 allocator 计数器。
pub const KERNEL_MEMORY_CAP_STATS: u64 = 1 << 3;
/// `kernel.memory@1` 定义的全部能力位。
pub const KERNEL_MEMORY_CAPABILITIES: u64 = KERNEL_MEMORY_CAP_ALLOCATE
    | KERNEL_MEMORY_CAP_RESIZE
    | KERNEL_MEMORY_CAP_QUERY
    | KERNEL_MEMORY_CAP_STATS;
/// Rust 全局分配器适配器要求的能力集合。
pub const KERNEL_MEMORY_GLOBAL_ALLOCATOR_CAPABILITIES: u64 =
    KERNEL_MEMORY_CAP_ALLOCATE | KERNEL_MEMORY_CAP_RESIZE;

/// 请求返回的内存必须清零。
pub const KERNEL_MEMORY_REQUEST_ZEROED: u32 = 1 << 0;
/// v1 请求支持的全部标志位。
pub const KERNEL_MEMORY_REQUEST_FLAGS_MASK: u32 = KERNEL_MEMORY_REQUEST_ZEROED;

/// 调用成功。
pub const KERNEL_MEMORY_STATUS_OK: i32 = 0;
/// 请求字段、地址、输出范围或结构版本无效。
pub const KERNEL_MEMORY_STATUS_INVALID: i32 = -1;
/// grant、generation、capability 或对象所有权校验失败。
pub const KERNEL_MEMORY_STATUS_PERMISSION: i32 = -2;
/// allocator 无法满足请求或 ELM 已达到动态内存预算。
pub const KERNEL_MEMORY_STATUS_OUT_OF_MEMORY: i32 = -3;
/// 地址不是一个仍然活跃的逐对象分配起始地址。
pub const KERNEL_MEMORY_STATUS_NOT_FOUND: i32 = -4;
/// allocator 尚未进入可服务状态。
pub const KERNEL_MEMORY_STATUS_UNAVAILABLE: i32 = -5;

/// 分配由小对象 slab 路径提供。
pub const KERNEL_MEMORY_KIND_SMALL: u32 = 1;
/// 分配由大对象 kernel heap 路径提供。
pub const KERNEL_MEMORY_KIND_LARGE: u32 = 2;

/// `kernel.memory@1` 的规范布局描述。
///
/// 布局摘要是下列规范字符串的 SHA-256，不依赖 Rust 类型名混淆或编译器元数据：
///
/// ```text
/// kernel.memory@1|header:ApiTableHeaderV1|allocate:(ApiGrantTokenV1,KernelMemoryRequestV1,*mut KernelMemoryAllocationV1)->i32|deallocate:unsafe(ApiGrantTokenV1,u64)->i32|reallocate:unsafe(ApiGrantTokenV1,u64,KernelMemoryRequestV1,*mut KernelMemoryAllocationV1)->i32|query:(ApiGrantTokenV1,u64,*mut KernelMemoryAllocationV1)->i32|stats:(ApiGrantTokenV1,*mut KernelMemoryStatsV1)->i32
/// ```
pub const KERNEL_MEMORY_LAYOUT_HASH_V1: [u8; 32] = [
    0x32, 0xea, 0xb6, 0xe3, 0x52, 0xa7, 0x70, 0x93, 0x1e, 0xe0, 0x8f, 0x30, 0xeb, 0xdb, 0x6c, 0x7c,
    0x6a, 0x6b, 0xaf, 0xa3, 0xa5, 0xc0, 0x88, 0x95, 0x3c, 0x7d, 0x82, 0x71, 0x69, 0x5a, 0xb2, 0xa6,
];

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 普通内核内存分配或扩容请求。
pub struct KernelMemoryRequestV1 {
    /// 完整结构尺寸；v1 必须等于当前类型尺寸。
    pub struct_size: u32,
    /// [`KERNEL_MEMORY_REQUEST_ZEROED`] 等请求标志。
    pub flags: u32,
    /// 请求的逻辑字节数；必须非零且可表示为目标架构的 `usize`。
    pub size: u64,
    /// 请求对齐；必须是非零的 2 次幂。
    pub align: u64,
}

impl KernelMemoryRequestV1 {
    /// 构造一个不要求清零的普通分配请求。
    pub const fn new(size: u64, align: u64) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            size,
            align,
        }
    }

    /// 要求 allocator 在返回前把整个逻辑范围清零。
    pub const fn zeroed(mut self) -> Self {
        self.flags |= KERNEL_MEMORY_REQUEST_ZEROED;
        self
    }

    /// 检查不依赖目标 allocator 状态的固定布局约束。
    pub const fn is_well_formed(self) -> bool {
        self.struct_size == core::mem::size_of::<Self>() as u32
            && self.flags & !KERNEL_MEMORY_REQUEST_FLAGS_MASK == 0
            && self.size != 0
            && self.align != 0
            && self.align.is_power_of_two()
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
/// 一次活跃普通内核分配的稳定描述。
pub struct KernelMemoryAllocationV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// ELM 可直接访问的内核虚拟地址。
    pub address: u64,
    /// 调用方请求并由账本核算的逻辑字节数。
    pub size: u64,
    /// allocator 后端实际可用字节数；不能替代上层对象的有效长度。
    pub usable_size: u64,
    /// 当前记录的对齐要求。
    pub align: u64,
    /// [`KERNEL_MEMORY_KIND_SMALL`] 或 [`KERNEL_MEMORY_KIND_LARGE`]。
    pub kind: u32,
    /// v1 必须为零。
    pub reserved0: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
/// 当前 ELM cell 的内存账本和 allocator 健康计数器。
pub struct KernelMemoryStatsV1 {
    /// 完整结构尺寸。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// 当前 cell 仍持有的动态分配字节数。
    pub current_bytes: u64,
    /// 当前 cell 自装载以来的动态分配峰值。
    pub peak_bytes: u64,
    /// 当前 cell 的动态分配硬上限。
    pub limit_bytes: u64,
    /// 当前 cell 因资源预算被拒绝的累计次数。
    pub quota_denials: u64,
    /// 当前 cell 资源账本发现的不变量错误数。
    pub accounting_errors: u64,
    /// allocator 全局累计成功分配次数。
    pub total_allocations: u64,
    /// allocator 全局累计成功释放次数。
    pub total_deallocations: u64,
    /// allocator 全局累计调整大小次数。
    pub total_reallocations: u64,
    /// allocator 全局累计内存不足次数。
    pub out_of_memory_count: u64,
    /// allocator 当前压力等级。
    pub pressure_level: u32,
    /// v1 必须为零。
    pub reserved0: u32,
}

/// `kernel.memory@1` 固定布局函数表。
#[repr(C)]
pub struct KernelMemoryApiV1 {
    /// 所有 Kernel API 表共享的稳定头部。
    pub header: ApiTableHeaderV1,
    /// 创建一个绑定到当前 cell 的普通内核分配。
    pub allocate:
        extern "C" fn(ApiGrantTokenV1, KernelMemoryRequestV1, *mut KernelMemoryAllocationV1) -> i32,
    /// 释放当前 cell 所有的普通内核分配。
    pub deallocate: unsafe extern "C" fn(ApiGrantTokenV1, u64) -> i32,
    /// 调整当前 cell 所有分配的大小，必要时允许地址改变。
    pub reallocate: unsafe extern "C" fn(
        ApiGrantTokenV1,
        u64,
        KernelMemoryRequestV1,
        *mut KernelMemoryAllocationV1,
    ) -> i32,
    /// 查询当前 cell 所有分配的实际布局。
    pub query: extern "C" fn(ApiGrantTokenV1, u64, *mut KernelMemoryAllocationV1) -> i32,
    /// 读取当前 cell 的资源账本和 allocator 计数器。
    pub stats: extern "C" fn(ApiGrantTokenV1, *mut KernelMemoryStatsV1) -> i32,
}

/// 把 Rust `alloc` 请求转发到当前 ELM 的 `kernel.memory@1` 导入槽。
///
/// 此适配器让原生 ELM 使用 `Box`、`Vec`、`String` 和 `Arc` 等 Rust 容器，同时保留
/// 内核对 cell、generation、grant 和动态内存预算的逐次校验。它不直接链接内核的
/// allocator crate，也不暴露内核全局分配器符号。
///
/// 普通模块应使用 [`crate::elm_global_allocator!`] 一次性声明导入槽并安装全局分配器，
/// 不需要手工构造本类型。
///
/// # 失败语义
///
/// 资源预算耗尽和普通内存不足按 [`GlobalAlloc`] 约定返回空指针。权限失效、陈旧
/// generation、未知地址或损坏的函数表属于模块运行时不变量错误，适配器会记录诊断并经
/// ELM 受保护终止出口结束当前调用，不能静默忽略释放失败。
pub struct ElmKernelAllocator {
    memory: &'static ApiImport<KernelMemoryApiV1>,
}

impl ElmKernelAllocator {
    /// 创建绑定到静态 `kernel.memory@1` 导入槽的 Rust 全局分配器。
    ///
    /// `memory` 必须声明 [`KERNEL_MEMORY_GLOBAL_ALLOCATOR_CAPABILITIES`]。推荐通过
    /// [`crate::elm_global_allocator!`] 生成该槽，避免 capability 与 EBI requirement 不一致。
    pub const fn new(memory: &'static ApiImport<KernelMemoryApiV1>) -> Self {
        Self { memory }
    }

    fn acquire(&self) -> crate::ApiTableRef<'_, KernelMemoryApiV1> {
        match self.memory.acquire() {
            Ok(memory) => memory,
            Err(_) => allocator_invariant_failure("ELM 全局分配器无法取得 kernel.memory@1"),
        }
    }

    fn allocate_layout(&self, layout: Layout, zeroed: bool) -> *mut u8 {
        if layout.size() == 0 {
            return null_mut();
        }
        let memory = self.acquire();
        let mut request = KernelMemoryRequestV1::new(layout.size() as u64, layout.align() as u64);
        if zeroed {
            request = request.zeroed();
        }
        match memory.table().allocate_memory(memory.token(), request) {
            Ok(allocation) => allocation.address as *mut u8,
            Err(KERNEL_MEMORY_STATUS_OUT_OF_MEMORY) => null_mut(),
            Err(_) => allocator_invariant_failure("ELM 全局分配器分配请求违反运行时约束"),
        }
    }
}

// Safety: 所有分配、调整和释放都经稳定函数表转发到内核；内核按当前 ELM 上下文验证
// grant、generation、地址所有权和布局。适配器本身只持有线程安全的静态 ApiImport。
unsafe impl GlobalAlloc for ElmKernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.allocate_layout(layout, false)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.allocate_layout(layout, true)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() {
            return;
        }
        let memory = self.acquire();
        // Safety: GlobalAlloc 的调用契约保证 ptr 来自当前分配器且已经失去全部活跃引用；
        // 内核还会再次验证该地址属于当前 cell。
        if unsafe { memory.table().deallocate_memory(memory.token(), ptr as u64) }.is_err() {
            allocator_invariant_failure("ELM 全局分配器拒绝释放未知或失效的地址");
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ptr.is_null() {
            let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
                return null_mut();
            };
            return self.allocate_layout(new_layout, false);
        }
        if new_size == 0 {
            // Safety: 调用方已按 GlobalAlloc::realloc 契约交出 ptr 的独占所有权。
            unsafe { self.dealloc(ptr, layout) };
            return null_mut();
        }
        let Ok(request) = u64::try_from(new_size)
            .map(|size| KernelMemoryRequestV1::new(size, layout.align() as u64))
        else {
            return null_mut();
        };
        let memory = self.acquire();
        // Safety: GlobalAlloc 的调用契约保证 ptr 属于当前分配器，且成功后旧引用全部失效。
        match unsafe {
            memory
                .table()
                .reallocate_memory(memory.token(), ptr as u64, request)
        } {
            Ok(allocation) => allocation.address as *mut u8,
            Err(KERNEL_MEMORY_STATUS_OUT_OF_MEMORY) => null_mut(),
            Err(_) => allocator_invariant_failure("ELM 全局分配器调整大小违反运行时约束"),
        }
    }
}

fn allocator_invariant_failure(message: &'static str) -> ! {
    let _ = elm::runtime::log(3, message);
    elm::runtime::abort_panic()
}

impl KernelMemoryApiV1 {
    /// 通过函数表创建分配，并把非零状态原样返回给调用方。
    pub fn allocate_memory(
        &self,
        token: ApiGrantTokenV1,
        request: KernelMemoryRequestV1,
    ) -> Result<KernelMemoryAllocationV1, i32> {
        let mut output = KernelMemoryAllocationV1::default();
        let status = (self.allocate)(token, request, &mut output);
        if status == KERNEL_MEMORY_STATUS_OK {
            Ok(output)
        } else {
            Err(status)
        }
    }

    /// 查询一个仍然活跃且属于当前 cell 的分配。
    pub fn query_memory(
        &self,
        token: ApiGrantTokenV1,
        address: u64,
    ) -> Result<KernelMemoryAllocationV1, i32> {
        let mut output = KernelMemoryAllocationV1::default();
        let status = (self.query)(token, address, &mut output);
        if status == KERNEL_MEMORY_STATUS_OK {
            Ok(output)
        } else {
            Err(status)
        }
    }

    /// 释放一个由当前 cell 持有的分配。
    ///
    /// # Safety
    ///
    /// `address` 必须属于当前 cell 和当前有效 generation。普通动态对象不得跨热替换保留；
    /// 需要迁移的状态必须编码到固定迁移载荷，再由新 generation 重新分配。调用发生时不得
    /// 再存在任何访问该对象的引用、裸指针操作、DMA 或异步内核任务。成功返回后该地址立即
    /// 失效，不能再次查询或释放。
    pub unsafe fn deallocate_memory(
        &self,
        token: ApiGrantTokenV1,
        address: u64,
    ) -> Result<(), i32> {
        // Safety: 调用方承担本方法文档列出的独占所有权和无活跃引用约束。
        let status = unsafe { (self.deallocate)(token, address) };
        if status == KERNEL_MEMORY_STATUS_OK {
            Ok(())
        } else {
            Err(status)
        }
    }

    /// 调整一个由当前 cell 持有的分配，并返回可能变化的新地址。
    ///
    /// # Safety
    ///
    /// `address` 必须满足 [`KernelMemoryApiV1::deallocate_memory`] 的独占约束。无论调用
    /// 成功后地址是否改变，调用方都必须重新建立基于返回记录的引用；旧引用不能继续使用。
    pub unsafe fn reallocate_memory(
        &self,
        token: ApiGrantTokenV1,
        address: u64,
        request: KernelMemoryRequestV1,
    ) -> Result<KernelMemoryAllocationV1, i32> {
        let mut output = KernelMemoryAllocationV1::default();
        // Safety: 调用方承担本方法文档列出的独占所有权和旧引用失效约束。
        let status = unsafe { (self.reallocate)(token, address, request, &mut output) };
        if status == KERNEL_MEMORY_STATUS_OK {
            Ok(output)
        } else {
            Err(status)
        }
    }

    /// 读取当前 cell 的内存资源快照。
    pub fn memory_stats(&self, token: ApiGrantTokenV1) -> Result<KernelMemoryStatsV1, i32> {
        let mut output = KernelMemoryStatsV1::default();
        let status = (self.stats)(token, &mut output);
        if status == KERNEL_MEMORY_STATUS_OK {
            Ok(output)
        } else {
            Err(status)
        }
    }
}

impl crate::table::sealed::Sealed for KernelMemoryApiV1 {}

// Safety: 该类型使用 repr(C)，首字段为规范表头，所有入口均使用声明中的固定 C ABI。
unsafe impl KernelApiTable for KernelMemoryApiV1 {
    const IDENTIFIER: &'static str = KERNEL_MEMORY_API_IDENTIFIER;
    const VERSION: u16 = KERNEL_MEMORY_API_VERSION;
    const CAPABILITIES: u64 = KERNEL_MEMORY_CAPABILITIES;
    const LAYOUT_HASH: [u8; 32] = KERNEL_MEMORY_LAYOUT_HASH_V1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KernelApiLayoutV1;

    #[test]
    fn memory_contract_layout_is_stable() {
        assert_eq!(core::mem::size_of::<KernelMemoryRequestV1>(), 24);
        assert_eq!(core::mem::size_of::<KernelMemoryAllocationV1>(), 48);
        assert_eq!(core::mem::size_of::<KernelMemoryStatsV1>(), 88);
        assert_eq!(core::mem::size_of::<KernelMemoryApiV1>(), 56);
        assert_eq!(core::mem::offset_of!(KernelMemoryApiV1, allocate), 16);
        assert_eq!(core::mem::offset_of!(KernelMemoryApiV1, stats), 48);
        assert_eq!(
            KernelApiLayoutV1::of::<KernelMemoryApiV1>().layout_hash,
            KERNEL_MEMORY_LAYOUT_HASH_V1
        );
    }

    #[test]
    fn memory_request_rejects_unknown_flags_and_invalid_layouts() {
        assert!(KernelMemoryRequestV1::new(64, 16).is_well_formed());
        assert!(KernelMemoryRequestV1::new(64, 16).zeroed().is_well_formed());
        assert!(!KernelMemoryRequestV1::new(0, 16).is_well_formed());
        assert!(!KernelMemoryRequestV1::new(64, 3).is_well_formed());
        let mut unknown = KernelMemoryRequestV1::new(64, 16);
        unknown.flags = 1 << 31;
        assert!(!unknown.is_well_formed());
    }

    #[test]
    fn public_layout_directory_resolves_only_exact_version() {
        let layout = crate::layout(KERNEL_MEMORY_API_IDENTIFIER, KERNEL_MEMORY_API_VERSION)
            .expect("kernel.memory@1 必须发布");
        assert_eq!(layout.table_size, 56);
        assert_eq!(layout.capabilities, KERNEL_MEMORY_CAPABILITIES);
        assert!(crate::layout(KERNEL_MEMORY_API_IDENTIFIER, 2).is_none());
    }
}

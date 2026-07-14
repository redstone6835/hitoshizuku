//! 原生 ELM 开发侧基础工具。
//!
//! ELM 原生代码默认不承诺堆可用。依赖完整 `elm` crate 的 no_std 模块会把
//! `alloc` 链入最终镜像，因此即便模块本身不分配内存，也需要显式选择一个
//! 全局 allocator。本模块提供拒绝分配的无堆 allocator，让外部 ELM 可以清楚
//! 表达“当前镜像不使用堆”。需要堆的 ELM 当前通过 `kernel-api` 的受管内存协议安装
//! 全局 allocator；未来直接内核符号目录完成后，也可以选择经过审核的直接分配契约。
//!
//! [`crate::elm_no_heap_allocator!`] 生成拒绝所有分配请求的全局 allocator。它适用于只使用静态
//! 存储、栈和固定 frame 的 ELM；任何意外分配都会收到空指针，并由 Rust 的 allocation
//! failure 路径进入模块 panic handler。需要堆的模块应使用
//! `kernel_api::elm_global_allocator!()` 安装受管全局分配器；分配仍受当前 ELM 执行身份、
//! 所有权和资源预算约束。无堆宏本身不生成 panic handler，模块仍必须把 panic 收敛到
//! [`crate::runtime::abort_panic`]。

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

/// 始终拒绝分配的 `GlobalAlloc` 实现。
///
/// `alloc` 对任何 layout 返回空指针，`dealloc` 为无操作。此类型只用于明确声明“模块不得
/// 使用堆”，不能作为真实 allocator，也不统计资源预算。需要堆的模块应使用
/// `kernel_api::elm_global_allocator!()`。
pub struct ElmNoHeapAllocator;

unsafe impl GlobalAlloc for ElmNoHeapAllocator {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[macro_export]
/// 为无堆原生 ELM 安装拒绝分配的全局 allocator。
///
/// 宏不接受参数，展开为一个带 `#[global_allocator]` 的静态
/// [`ElmNoHeapAllocator`](crate::native::ElmNoHeapAllocator)。每个最终镜像只能调用一次，也
/// 不能同时声明其他 global allocator。
///
/// 该宏不会扫描代码证明“没有分配”，也不会生成 panic handler。若 `alloc`、`Vec`、`String`
/// 或其他路径实际请求堆，allocator 返回空指针，模块必须通过自己的 panic/allocation failure
/// 路径调用 [`runtime::abort_panic`](crate::runtime::abort_panic)。
///
/// # 示例
///
/// ```ignore
/// #![no_std]
/// #![no_main]
///
/// elm::elm_no_heap_allocator!();
///
/// #[panic_handler]
/// fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
///     elm::runtime::abort_panic()
/// }
/// ```
macro_rules! elm_no_heap_allocator {
    () => {
        #[global_allocator]
        static ELM_NO_HEAP_ALLOCATOR: $crate::native::ElmNoHeapAllocator =
            $crate::native::ElmNoHeapAllocator;
    };
}

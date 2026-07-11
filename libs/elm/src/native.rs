//! 原生 ELM 开发侧基础工具。
//!
//! ELM 原生代码默认不承诺堆可用。依赖完整 `elm` crate 的 no_std 模块会把
//! `alloc` 链入最终镜像，因此即便模块本身不分配内存，也需要显式选择一个
//! 全局 allocator。本模块提供拒绝分配的无堆 allocator，让外部 ELM 可以清楚
//! 表达“当前镜像不使用堆”。真正需要堆的 ELM 后续必须通过稳定 allocator
//! 能力或子系统 crate 接入，不能隐式复用内核堆。

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

pub struct ElmNoHeapAllocator;

unsafe impl GlobalAlloc for ElmNoHeapAllocator {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[macro_export]
macro_rules! elm_no_heap_allocator {
    () => {
        #[global_allocator]
        static ELM_NO_HEAP_ALLOCATOR: $crate::native::ElmNoHeapAllocator =
            $crate::native::ElmNoHeapAllocator;
    };
}

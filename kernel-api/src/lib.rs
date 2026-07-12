#![no_std]
#![warn(missing_docs)]

//! ELM 可访问的内核运行期 API 门面。
//!
//! 本 crate 只保存稳定契约、固定布局函数表和安全客户端，不包含内核子系统实现。
//! 外部 ELM 通过 [`ApiImport`] 在初始化阶段取得一个版本化命名空间，后续调用直接
//! 使用缓存函数表，不经过 elm-mgr 管理通道。

mod import;
pub mod memory;
mod table;

pub use import::{ApiImport, ApiImportError, ApiTableRef};
pub use table::{ApiGrantTokenV1, ApiTableHeaderV1, KernelApiLayoutV1, KernelApiTable};

/// Kernel API 命名空间 identifier 允许的最大字节数。
pub const KERNEL_API_IDENTIFIER_MAX_LEN: usize = elm::ELM_KERNEL_API_IDENTIFIER_MAX_LEN;

/// 返回内建 Kernel API 表的规范布局摘要。
///
/// 每个后续子系统批次会把已经完整实现的表加入该目录。未知命名空间不会得到零摘要，
/// 而是明确返回 `None`，避免打包器生成不可验证的依赖记录。
pub fn layout(identifier: &str, version: u16) -> Option<KernelApiLayoutV1> {
    table::layout(identifier, version)
}

#[macro_export]
/// 为原生 Rust ELM 安装由 `kernel.memory@1` 支持的全局分配器。
///
/// 宏会同时生成三项内容：
///
/// - 一个要求 `ALLOCATE | RESIZE` capability 的静态 Kernel API 导入槽；
/// - 对应的 EBI Kernel API requirement 元数据；
/// - 一个实现 `#[global_allocator]` 的 [`memory::ElmKernelAllocator`] 静态对象。
///
/// 每个最终 ELM 镜像只能调用一次本宏，且不能同时调用
/// `elm::elm_no_heap_allocator!()` 或声明其它全局分配器。宏不会改变跨 ELM 边界的 ABI；
/// `Vec`、`String` 等动态对象仍只能在模块内部使用，不能直接放入 provider、export 或
/// mixin 的固定载荷。
///
/// 模块卸载时必须在 `on_finalize` 中释放所有长期存活的堆对象；热替换则必须在发起替换
/// 事务前先清空长期堆状态，因为 replace 预检早于旧 generation 的 finalize。运行时会拒绝
/// 仍持有动态分配的 cell 退役或切换 generation。
///
/// # 示例
///
/// ```ignore
/// #![no_std]
/// #![no_main]
///
/// extern crate alloc;
///
/// use alloc::{string::String, vec::Vec};
/// use elm::{HookResult, LifecycleContext};
///
/// kernel_api::elm_global_allocator!();
///
/// #[elm::on_initialize]
/// fn initialize(_context: &LifecycleContext) -> HookResult {
///     let mut values = Vec::new();
///     values.extend_from_slice(&[1_u32, 2, 3]);
///     let message = String::from("ELM heap ready");
///     assert_eq!(values.iter().sum::<u32>(), 6);
///     elm::runtime::log(6, &message).map_err(|_| elm::HookError::new(-1))
/// }
///
/// #[elm::on_finalize]
/// fn finalize(_context: &LifecycleContext) -> HookResult {
///     Ok(())
/// }
///
/// #[panic_handler]
/// fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
///     elm::runtime::abort_panic()
/// }
/// ```
macro_rules! elm_global_allocator {
    () => {
        #[::elm::kernel_api(namespace = "kernel.memory", version = 1, capabilities = 3)]
        static ELM_KERNEL_MEMORY_IMPORT_V1: $crate::ApiImport<$crate::memory::KernelMemoryApiV1> =
            $crate::ApiImport::new("kernel.memory", 1, 3);

        #[global_allocator]
        static ELM_KERNEL_GLOBAL_ALLOCATOR_V1: $crate::memory::ElmKernelAllocator =
            $crate::memory::ElmKernelAllocator::new(&ELM_KERNEL_MEMORY_IMPORT_V1);
    };
}

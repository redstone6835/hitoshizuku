#![no_std]
#![warn(missing_docs)]

//! ELM 使用的受管内核运行期协议。
//!
//! 本 crate 只保存必须逐次授权的稳定契约、固定布局函数表和安全客户端，不包含内核
//! 子系统实现。外部 ELM 通过 [`ApiImport`] 在初始化阶段取得版本化命名空间，后续调用
//! 使用缓存函数表。EBI 已定义直接内核符号导入协议，但内核符号目录后端尚未安装；在
//! 后端完成前，普通 ELM 仍应通过本 crate 中已经注册的受管 API 访问内核能力。

pub mod device;
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
/// 为明确选择受管内存协议的原生 Rust ELM 安装全局分配器。
///
/// 宏会同时生成三项内容：
///
/// - 一个要求 `ALLOCATE | RESIZE` capability 的静态 Kernel API 导入槽；
/// - 对应的 EBI Kernel API requirement 元数据；
/// - 一个实现 `#[global_allocator]` 的 [`memory::ElmKernelAllocator`] 静态对象。
///
/// 本宏是当前外部 ELM 使用内核堆的受管入口。未来直接内核符号目录稳定后，模块可按
/// 明确发布的符号契约选择直接分配路径；两种路径不得混用。每个最终 ELM 镜像只能调用
/// 一次本宏，且不能同时调用
/// `elm::elm_no_heap_allocator!()` 或声明其它全局分配器。宏不会改变跨 ELM 边界的 ABI；
/// `Vec`、`String` 等动态对象仍只能在模块内部使用，不能直接放入 provider、export 或
/// mixin 的固定载荷。
///
/// 模块卸载时必须在 [`elm::ElmModule::finalize`] 中释放所有长期存活的堆对象；热替换则必须在发起替换
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
/// use elm::{ElmModule, HookError, HookResult, LifecycleContext};
///
/// kernel_api::elm_global_allocator!();
///
/// struct Demo;
///
/// #[elm::module]
/// impl ElmModule for Demo {
///     fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
///         Ok(Self)
///     }
///
///     fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
///         let mut values = Vec::new();
///         values.extend_from_slice(&[1_u32, 2, 3]);
///         let message = String::from("ELM heap ready");
///         assert_eq!(values.iter().sum::<u32>(), 6);
///         elm::runtime::log(6, &message).map_err(|_| HookError::new(-1))
///     }
///
///     fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
///         Ok(())
///     }
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

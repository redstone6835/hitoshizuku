//! 普通 ELM 访问当前运行时的稳定入口。
//!
//! 本模块是模块业务代码与 elm-mgr 运行时之间的最小通道。所有函数最终通过装载器注入的
//! [`ElmApiRootV1`](crate::ElmApiRootV1) 和 [`ElmRuntimeApiV1`](crate::ElmRuntimeApiV1)
//! 调用，不依赖内核内部符号，也不允许模块持有裸函数表指针。
//!
//! 普通 ELM 可以在生命周期钩子、provider、受管 export、entry 和 mixin 处理器中调用本
//! 模块。调用前应把 [`RuntimeApiError`] 转换为自己的业务错误或 [`HookError`](crate::HookError)；
//! 不要假定日志、上下文或可选能力在损坏的装载环境中必然可用。
//!
//! # 示例
//!
//! ```no_run
//! use elm::{HookError, HookResult, LifecycleContext};
//!
//! #[elm::on_initialize]
//! fn initialize(_context: &LifecycleContext) -> HookResult {
//!     let info = elm::runtime::info().map_err(|_| HookError::new(-1))?;
//!     if info.api_version != elm::ELM_API_VERSION_V1 {
//!         return Err(HookError::new(-95));
//!     }
//!     elm::runtime::log(6, "runtime API ready").map_err(|_| HookError::new(-1))
//! }
//! ```

use crate::developer::runtime_api;
use crate::{ELM_API_CURRENT_VERSION, ElmApiContextV1, ElmApiNamespaceV1, RuntimeApiError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 当前模块看到的 ELM 运行时协商结果。
///
/// 该值是对根 API 表的安全摘要，不包含可被模块直接调用的裸指针。能力位只能说明入口由
/// 当前版本提供，具体操作仍可能因单元状态、策略、资源预算或运行时故障而失败。
pub struct RuntimeInfo {
    /// 已选择的 ELM API 版本。
    pub api_version: u16,
    /// 根表声明的 `ELM_API_FEATURE_*` 能力位集合。
    pub capabilities: u64,
}

/// 返回当前 ELM API 版本和能力位。
///
/// 此函数会验证根表是否已由装载器注入、魔数和结构尺寸是否兼容，并读取当前根表的 feature
/// 集合。它不会执行管理鉴权，也不会保证每个可选功能对当前单元都可用。
///
/// # 错误
///
/// 根槽尚未重定位时返回 [`RuntimeApiError::RootUnavailable`]；根表版本或布局不兼容时返回
/// [`RuntimeApiError::IncompatibleRoot`]。
pub fn info() -> Result<RuntimeInfo, RuntimeApiError> {
    Ok(RuntimeInfo {
        api_version: ELM_API_CURRENT_VERSION,
        capabilities: runtime_api::features()?,
    })
}

/// 查询当前 ELM 调用上下文。
///
/// 返回值包含当前 cell、父 cell、generation、状态、生命周期阶段、kind、允许动作和本次
/// 调用标志。该上下文由内核在调用边界动态生成；不要把它当成长期授权凭据，尤其不要在
/// 热替换后复用其中的 generation。
///
/// # 错误
///
/// 运行时表不可用、当前线程不处于 ELM 调用上下文或返回结构校验失败时返回错误。
pub fn context() -> Result<ElmApiContextV1, RuntimeApiError> {
    runtime_api::current_context()
}

/// 按 identifier 和兼容版本列表取得一个额外的运行时命名空间。
///
/// 普通业务代码通常应使用 `kernel-api` 提供的类型化客户端，而不是直接处理函数表地址。
/// 本入口公开是为了让独立门面 crate 在不依赖内核实现的前提下完成统一协商。
pub fn query_namespace(
    identifier: &str,
    versions: &[u16],
) -> Result<ElmApiNamespaceV1, RuntimeApiError> {
    runtime_api::query_namespace(identifier, versions)
}

/// 向 elm-mgr 提交一条归属于当前 ELM 的运行时日志。
///
/// `level` 使用运行时约定的日志等级数值，`message` 以 UTF-8 字节传递且只在调用期间借用。
/// 运行时会附加当前 cell、generation 和审计上下文；模块不应自行伪造其他单元的身份。
/// 超长消息、未授权上下文或已静默单元会返回错误。
///
/// # 示例
///
/// ```no_run
/// elm::runtime::log(6, "provider started")?;
/// # Ok::<(), elm::RuntimeApiError>(())
/// ```
pub fn log(level: u32, message: &str) -> Result<(), RuntimeApiError> {
    runtime_api::log(level, message)
}

/// 以指定原因码主动终止当前原生 ELM 调用。
///
/// 此函数不返回。运行时应记录 fault、审计和调用链信息，并通过受保护恢复出口离开当前
/// native execution，而不是让模块继续执行。`reason` 应使用 `ELM_API_ABORT_REASON_*`
/// 常量；未知原因会保留为诊断值，但不得被当作成功。
///
/// 如果根表本身不可用，框架会进入不可返回的自旋兜底路径，因为此时已经没有安全 ABI
/// 可以把控制权交还内核。
pub fn abort(reason: u32) -> ! {
    runtime_api::abort_current(reason)
}

/// 以 [`ELM_API_ABORT_REASON_PANIC`](crate::ELM_API_ABORT_REASON_PANIC) 终止当前调用。
///
/// 外部 ELM 的 `#[panic_handler]` 应直接调用此函数。panic 不得跨 ELM ABI 展开；这样做会
/// 绕过运行时的 fault isolation，并可能破坏内核栈和热替换事务。
///
/// # 示例
///
/// ```ignore
/// #[panic_handler]
/// fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
///     elm::runtime::abort_panic()
/// }
/// ```
pub fn abort_panic() -> ! {
    runtime_api::abort_panic()
}

#![warn(missing_docs)]

//! ELM Rust 开发属性宏。
//!
//! 本 crate 实现 ELM 模块源码到 EBI Rust ABI v1 的编译期适配。模块作者只编写安全
//! Rust 函数、静态导入槽和固定载荷结构体；宏负责生成稳定导出符号、原始 ABI
//! trampoline，以及供 `elm-tools` 消费的 `.elm.meta` 元数据。业务代码不应手写
//! `extern "C"`、`export_name`、`link_section` 或 EBI 元数据记录。
//!
//! # 共同函数约束
//!
//! 除 [`macro@payload`] 和 [`macro@import`] 外，本 crate 的 attribute 均作用于函数。被标记函数必须：
//!
//! - 是普通安全 Rust 函数，不能是 `unsafe fn`；
//! - 不能声明 `extern` ABI、`async`、`const`、可变参数或泛型参数；
//! - 不能包含 `self` 接收者；
//! - 必须显式写出文档规定的返回类型；
//! - 参数数量和类型必须与对应 attribute 的规范签名一致。
//!
//! 宏先检查可由语法确定的约束，随后由 Rust 类型检查器验证参数和返回值的精确类型。
//! attribute 参数只接受字符串、整数、布尔字面量，以及 `stages(...)` 中的阶段标识符；
//! 未知参数、重复参数和超出范围的数值都会产生编译错误。
//!
//! # 生成内容
//!
//! 函数类 attribute 会保留原 Rust 函数，并额外生成位于 `.text.elm.abi` 的
//! `unsafe extern "C"` trampoline。trampoline 负责验证 ABI 版本、结构尺寸、保留字段、
//! 缓冲区边界和调用关联字段，然后把裸指针收敛为安全借用。每个声明还会在非装载段
//! `.elm.meta` 中生成一个或多个八字节对齐的 `ELMMETA1` 记录。记录字段按 tag 排序，
//! payload 使用 CRC32 校验并以零填充到八字节边界。
//!
//! `.elm.meta` 只供构建工具读取，不得进入 EKI 的可装载段。`elm-tools` 会再次独立校验
//! 元数据、符号、契约、ELF 段和重定位，因此通过宏展开不等于镜像已经获得装载资格。
//!
//! # 完整示例
//!
//! 以下代码展示生命周期、固定载荷和 provider 的组合。示例标记为 `ignore`，因为实际
//! ELM 工程还需要 `#![no_std]`、`#![no_main]`、专用链接脚本和 `elm-tools` 打包步骤。
//!
//! ```ignore
//! use elm::{
//!     HookResult, LifecycleContext, ProviderReply, ProviderRequest, ProviderResult,
//! };
//!
//! #[elm::payload("demo.request@1")]
//! struct Request {
//!     opcode: u32,
//! }
//!
//! #[elm::on_initialize]
//! fn initialize(_context: &LifecycleContext) -> HookResult {
//!     elm::runtime::log(6, "demo: initialized")
//!         .map_err(|_| elm::HookError::new(-1))
//! }
//!
//! #[elm::on_finalize]
//! fn finalize(_context: &LifecycleContext) -> HookResult {
//!     Ok(())
//! }
//!
//! #[elm::provider(contract = "demo.service@1")]
//! fn service(_request: &ProviderRequest) -> ProviderResult {
//!     Ok(ProviderReply::ok())
//! }
//! ```

use std::collections::BTreeMap;

use proc_macro::TokenStream;
use proc_macro2::{Ident, Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::punctuated::Punctuated;
use syn::{
    Expr, ExprLit, ExprUnary, Fields, FnArg, ItemFn, ItemStatic, ItemStruct, Lit, LitStr, Meta,
    Pat, ReturnType, Token, Type, UnOp, parse_macro_input,
};

const META_MAGIC: &[u8; 8] = b"ELMMETA1";
const META_VERSION: u16 = 1;
const META_HEADER_SIZE: usize = 32;

const KIND_LIFECYCLE: u16 = 1;
const KIND_ENTRY: u16 = 2;
const KIND_PROVIDER: u16 = 3;
const KIND_PROVIDER_SNAPSHOT: u16 = 4;
const KIND_EXPORT: u16 = 5;
const KIND_IMPORT: u16 = 6;
const KIND_EXTENSION_POINT: u16 = 7;
const KIND_EXTENSION: u16 = 8;
const KIND_PAYLOAD: u16 = 9;
const KIND_KERNEL_API: u16 = 10;

const VALUE_UTF8: u16 = 1;
const VALUE_U32: u16 = 2;
const VALUE_I32: u16 = 3;
const VALUE_U64: u16 = 4;

const FIELD_SYMBOL: u16 = 1;
const FIELD_HOOK_KIND: u16 = 2;
const FIELD_NAME: u16 = 3;
const FIELD_CONTRACT: u16 = 4;
const FIELD_MIN_VERSION: u16 = 5;
const FIELD_MAX_VERSION: u16 = 6;
const FIELD_VERSION: u16 = 7;
const FIELD_FLAGS: u16 = 8;
const FIELD_ACCESS: u16 = 9;
const FIELD_DIRECTION: u16 = 10;
const FIELD_MODE: u16 = 11;
const FIELD_TARGET: u16 = 12;
const FIELD_POINT: u16 = 13;
const FIELD_STAGE: u16 = 14;
const FIELD_PRIORITY: u16 = 15;
const FIELD_HANDLER_CONTRACT: u16 = 16;
const FIELD_PAYLOAD_CONTRACT: u16 = 17;
const FIELD_WIRE_SIZE: u16 = 18;
const FIELD_CAPABILITIES: u16 = 20;

const IMPORT_OPTIONAL: u32 = 1 << 0;
const IMPORT_MANAGED: u32 = 1 << 1;
const IMPORT_DIRECT_PINNED: u32 = 1 << 2;
const IMPORT_ALLOW_ANCESTOR: u32 = 1 << 3;
const IMPORT_ALLOW_BUILTIN: u32 = 1 << 4;
const EXPORT_MANAGED: u32 = 1 << 0;
const EXPORT_DIRECT_PINNED: u32 = 1 << 1;
const EXPORT_PRIVATE: u32 = 1 << 2;
const EXPORT_DEPENDENCY: u32 = 1 << 3;
const EXPORT_SUBTREE: u32 = 1 << 4;
const EBI_NAME_LEN: usize = 128;
const EBI_SYMBOL_NAME_LEN: usize = 128;
const NEXUS_CONTRACT_LEN: usize = 64;
const RELATION_POINT_LEN: usize = 32;

#[proc_macro_attribute]
/// 声明 ELM 的必需初始化前钩子。
///
/// # 调用时机
///
/// 装载器完成镜像验证、段映射、重定位、W^X 封口、指令缓存同步、根 API 表注入和导入槽
/// 暂存后调用此钩子。返回成功后，运行时才会公开该单元的依赖边、扩展点、provider 端口
/// 和菜单项，并把单元推进到 `Active`。初始化失败时，单元不会对外可见，运行时进入回滚
/// 和隔离路径。
///
/// 每个动态 ELM 必须且只能声明一个初始化钩子。
///
/// # 规范签名
///
/// ```text
/// fn(&elm::LifecycleContext) -> elm::HookResult
/// ```
///
/// attribute 不接受参数。业务函数保持 Rust ABI；宏生成导出名精确为 `on_initialize` 的
/// ABI trampoline，并写入生命周期种类为 initialize 的 `.elm.meta` 记录。
///
/// 返回 `Ok(())` 表示初始化事务可以继续提交。返回 `Err(HookError)` 时，错误状态码原样
/// 传播给运行时；状态码不能为零，`HookError::new(0)` 会归一化为无效调用错误。
///
/// # 限制
///
/// 初始化函数不能手工声明 `extern "C"`、`unsafe`、泛型、`async` 或 `const`。不要把尚未
/// 提升的导入句柄、当前上下文借用或内核临时地址保存到钩子返回之后。需要长期存在的对象
/// 必须通过运行时认可的资源所有权接口登记，以便卸载和热替换时排空。
///
/// # 示例
///
/// ```ignore
/// use elm::{HookError, HookResult, LifecycleContext};
///
/// #[elm::on_initialize]
/// fn initialize(context: &LifecycleContext) -> HookResult {
///     let message = if context.parent_id().is_some() {
///         "child ELM initialized"
///     } else {
///         "root ELM initialized"
///     };
///     elm::runtime::log(6, message).map_err(|_| HookError::new(-1))
/// }
/// ```
pub fn on_initialize(attr: TokenStream, item: TokenStream) -> TokenStream {
    lifecycle_attribute(attr, item, 1, 1, "on_initialize")
}

#[proc_macro_attribute]
/// 声明 ELM 的必需卸载前钩子。
///
/// # 调用时机
///
/// 运行时已经阻止新调用并完成必要排空后，在撤销单元拓扑、导入、provider、菜单和已登记
/// 资源之前调用此钩子。钩子负责撤销模块自定义状态、注销由模块主动创建的对象以及停止不再
/// 接受框架托管的工作。每个动态 ELM 必须且只能声明一个终结钩子。
///
/// 正常卸载、初始化失败回滚和热替换旧代退役均可能触发终结。代码必须允许“初始化只完成了
/// 一部分”的情况，并应设计为幂等或至少能根据模块自身状态安全收口。终结失败时，运行时会
/// 保留单元及其资源用于诊断，而不是伪装成卸载成功。
///
/// # 规范签名
///
/// ```text
/// fn(&elm::LifecycleContext) -> elm::HookResult
/// ```
///
/// attribute 不接受参数。宏生成导出名 `on_finalize`、对应 ABI trampoline 和生命周期
/// 元数据。`Ok(())` 允许卸载事务继续；`Err(HookError)` 中止提交并进入故障诊断路径。
///
/// # 示例
///
/// ```ignore
/// use elm::{HookResult, LifecycleContext};
///
/// #[elm::on_finalize]
/// fn finalize(_context: &LifecycleContext) -> HookResult {
///     elm::runtime::log(6, "demo: finalizing")
///         .map_err(|_| elm::HookError::new(-1))?;
///     Ok(())
/// }
/// ```
pub fn on_finalize(attr: TokenStream, item: TokenStream) -> TokenStream {
    lifecycle_attribute(attr, item, 2, 2, "on_finalize")
}

#[proc_macro_attribute]
/// 声明进入静默阶段前的可选钩子。
///
/// `on_quiesce` 在暂停或热替换需要旧代停止产生新工作时调用。钩子应关闭模块自有的工作
/// 入口、停止创建新异步请求，并促使已登记资源进入可排空状态；它不应直接释放仍可能被在途
/// 调用访问的对象。成功后运行时继续等待调用和租约排空，再进入暂停或迁移阶段。
///
/// # 规范签名
///
/// ```text
/// fn(&elm::LifecycleContext) -> elm::HookResult
/// ```
///
/// attribute 不接受参数，最多声明一次。宏导出 `on_quiesce` 并生成对应生命周期元数据。
/// 返回错误会阻止暂停或替换事务继续。
///
/// # 示例
///
/// ```ignore
/// use elm::{HookResult, LifecycleContext};
///
/// #[elm::on_quiesce]
/// fn quiesce(_context: &LifecycleContext) -> HookResult {
///     // 在此停止模块自有的生产入口；资源释放留给 finalize。
///     Ok(())
/// }
/// ```
pub fn on_quiesce(attr: TokenStream, item: TokenStream) -> TokenStream {
    lifecycle_attribute(attr, item, 6, 3, "on_quiesce")
}

#[proc_macro_attribute]
/// 声明单元完成排空后进入 `Paused` 前的可选钩子。
///
/// 运行时只在 `on_quiesce` 成功且受管调用、provider 调用及相关租约满足暂停条件后调用
/// `on_pause`。该钩子用于保存轻量暂停状态或切换模块内部状态机；它不是卸载钩子，不能假定
/// 导入、导出、镜像内存或已登记资源即将消失。
///
/// # 规范签名
///
/// ```text
/// fn(&elm::LifecycleContext) -> elm::HookResult
/// ```
///
/// attribute 不接受参数，最多声明一次。成功后单元进入 `Paused`；失败则暂停事务失败，
/// 运行时依据恢复结果保留活动状态或进入故障隔离。
///
/// # 示例
///
/// ```ignore
/// use elm::{HookResult, LifecycleContext};
///
/// #[elm::on_pause]
/// fn pause(_context: &LifecycleContext) -> HookResult {
///     Ok(())
/// }
/// ```
pub fn on_pause(attr: TokenStream, item: TokenStream) -> TokenStream {
    lifecycle_attribute(attr, item, 7, 4, "on_pause")
}

#[proc_macro_attribute]
/// 声明暂停单元恢复活动前的可选钩子。
///
/// `on_resume` 在单元仍处于受保护的恢复事务中调用。钩子应重新打开由 `on_quiesce` 关闭
/// 的模块自有入口，并恢复暂停期间冻结的状态。只有钩子成功后，运行时才把单元重新推进到
/// `Active` 并允许新调用进入。
///
/// # 规范签名
///
/// ```text
/// fn(&elm::LifecycleContext) -> elm::HookResult
/// ```
///
/// attribute 不接受参数，最多声明一次。失败时单元保持不可服务状态并留下诊断记录。
///
/// # 示例
///
/// ```ignore
/// use elm::{HookResult, LifecycleContext};
///
/// #[elm::on_resume]
/// fn resume(_context: &LifecycleContext) -> HookResult {
///     Ok(())
/// }
/// ```
pub fn on_resume(attr: TokenStream, item: TokenStream) -> TokenStream {
    lifecycle_attribute(attr, item, 8, 5, "on_resume")
}

#[proc_macro_attribute]
/// 声明热替换旧代的可选状态导出钩子。
///
/// # 规范签名
///
/// ```text
/// fn(&elm::MigrationContext, &mut [u8]) -> elm::MigrationExportResult
/// ```
///
/// attribute 不接受参数，最多声明一次。运行时在新镜像完成影子装载和初始化、旧代完成
/// 静默后调用此钩子。第二个参数是运行时拥有的迁移缓冲区，钩子应写入自描述、版本化的
/// 模块状态，并返回实际写入长度。返回长度大于缓冲容量会被 trampoline 视为 ABI 错误。
///
/// 宏生成导出名 `on_migrate_export`、迁移 ABI trampoline 和生命周期元数据。缓冲区只在
/// 本次调用期间有效；不得保存其地址。状态编码由模块定义，但新旧版本必须明确约定兼容性，
/// 并建议在首部放置模块私有版本和长度。
///
/// # 错误语义
///
/// 返回 `Err(HookError)` 会终止替换提交。运行时随后尝试恢复旧代，并调用新代的
/// `on_migrate_abort` 与 `on_finalize`。因此导出逻辑不应在成功提交前破坏旧代唯一状态。
///
/// # 示例
///
/// ```ignore
/// use elm::{MigrationContext, MigrationExportResult};
///
/// #[elm::on_migrate_export]
/// fn export_state(
///     _context: &MigrationContext,
///     output: &mut [u8],
/// ) -> MigrationExportResult {
///     let state = 7_u64.to_le_bytes();
///     if output.len() < state.len() {
///         return Err(elm::HookError::new(-12));
///     }
///     output[..state.len()].copy_from_slice(&state);
///     Ok(state.len())
/// }
/// ```
pub fn on_migrate_export(attr: TokenStream, item: TokenStream) -> TokenStream {
    migration_export_attribute(attr, item)
}

#[proc_macro_attribute]
/// 声明热替换新代的可选状态导入钩子。
///
/// # 规范签名
///
/// ```text
/// fn(&elm::MigrationContext, &[u8]) -> elm::HookResult
/// ```
///
/// attribute 不接受参数，最多声明一次。运行时把旧代 `on_migrate_export` 成功产生的完整
/// 字节串传给该钩子。新代应先验证模块私有状态版本和长度，再构造尚未对外公开的新状态。
/// 输入切片只在调用期间有效。
///
/// 返回成功仅表示新代接受了迁移状态；导入、provider backend、绑定和代际仍要在运行时
/// 提交点原子切换。返回错误会触发新代 abort/finalize 和旧代恢复，不会公开半完成的新代。
///
/// # 示例
///
/// ```ignore
/// use elm::{HookError, HookResult, MigrationContext};
///
/// #[elm::on_migrate_import]
/// fn import_state(_context: &MigrationContext, input: &[u8]) -> HookResult {
///     let bytes: [u8; 8] = input.try_into().map_err(|_| HookError::new(-22))?;
///     let _state = u64::from_le_bytes(bytes);
///     Ok(())
/// }
/// ```
pub fn on_migrate_import(attr: TokenStream, item: TokenStream) -> TokenStream {
    migration_input_attribute(attr, item, 4, 7, "on_migrate_import")
}

#[proc_macro_attribute]
/// 声明热替换新代在事务回滚时执行的可选清理钩子。
///
/// # 规范签名
///
/// ```text
/// fn(&elm::MigrationContext, &[u8]) -> elm::HookResult
/// ```
///
/// attribute 不接受参数，最多声明一次。新代初始化后，只要迁移或最终提交失败，运行时就可能
/// 调用此钩子。输入是本次替换使用的迁移状态；如果失败发生在状态导出之前，输入可以为空。
/// 钩子应撤销 `on_migrate_import` 已创建但尚未公开的模块私有状态，并能处理部分导入。
///
/// abort 之后运行时仍会调用新代 `on_finalize`。两者职责应区分：abort 撤销迁移事务特有的
/// 状态，finalize 完成单元通用收口。返回错误会被记录为回滚故障，可能使新旧代进入隔离。
///
/// # 示例
///
/// ```ignore
/// use elm::{HookResult, MigrationContext};
///
/// #[elm::on_migrate_abort]
/// fn abort_migration(_context: &MigrationContext, _input: &[u8]) -> HookResult {
///     Ok(())
/// }
/// ```
pub fn on_migrate_abort(attr: TokenStream, item: TokenStream) -> TokenStream {
    migration_input_attribute(attr, item, 5, 8, "on_migrate_abort")
}

#[proc_macro_attribute]
/// 声明 ELM 激活后的可选一次性入口。
///
/// # 调用时机
///
/// `entry` 在必需初始化钩子成功、声明式拓扑激活且单元进入 `Active` 后调用。它适合启动
/// 模块自己的受托工作或执行一次性自检，但不替代 `on_initialize`，也不能用于声明必须在
/// 单元公开前完成的不变量。一个 ELM 最多声明一个 entry。
///
/// # 规范签名
///
/// ```text
/// fn(&elm::EntryContext) -> elm::EntryResult
/// ```
///
/// attribute 不接受参数。若函数名为 `start`，宏生成导出符号 `__elm_entry_start`、ABI
/// trampoline 和 entry 元数据。业务函数名会成为符号的一部分，因此必须满足 EBI symbol
/// 字符约束。
///
/// 返回错误会写入 entry frame 的 `exit_code` 并报告给运行时；是否隔离单元由运行时策略
/// 决定。入口上下文和其中的代际信息只代表本次调用。
///
/// # 示例
///
/// ```ignore
/// use elm::{EntryContext, EntryResult};
///
/// #[elm::entry]
/// fn start(context: &EntryContext) -> EntryResult {
///     let _generation = context.generation();
///     Ok(())
/// }
/// ```
pub fn entry(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match entry_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
/// 声明一个原生 provider 调用门及其端口元数据。
///
/// # 参数语法
///
/// ```text
/// #[elm::provider(
///     contract = "identifier@version",
///     access = "public",
///     direction = "control",
///     mode = "shared"
/// )]
/// ```
///
/// `contract` 必填，必须是长度不超过 ABI 上限的 ELM 契约 identifier，并包含数字版本。
/// 其余参数可省略：
///
/// - `access`：`"internal"`、`"public"` 或 `"extension-only"`，默认 `"public"`；
/// - `direction`：`"source"`、`"sink"`、`"duplex"` 或 `"control"`，默认 `"control"`；
/// - `mode`：`"exclusive"`、`"shared"`、`"ordered"`、`"pipeline"` 或
///   `"broadcast"`，默认 `"shared"`。
///
/// # 规范签名
///
/// ```text
/// fn(&elm::ProviderRequest) -> elm::ProviderResult
/// ```
///
/// 宏为函数 `service` 生成 `__elm_provider_service` 导出符号、provider ABI trampoline 和
/// provider 元数据。trampoline 校验 ABI 版本、绑定号、载荷长度及保留字段，再构造安全的
/// `ProviderRequest`。成功回复由 `elm::ProviderReply` 构造；
/// `HookError` 状态码传播为调用失败。
///
/// provider 函数可能并发执行，具体并发语义由 `mode`、绑定策略和运行时决定。函数不得持有
/// 请求借用越过返回点，也不得绕过租约保存调用帧中的裸标识符所代表的内核对象。
///
/// # 示例
///
/// ```ignore
/// use elm::{ProviderReply, ProviderRequest, ProviderResult};
///
/// #[elm::provider(
///     contract = "demo.counter@1",
///     access = "public",
///     direction = "control",
///     mode = "ordered"
/// )]
/// fn counter(request: &ProviderRequest) -> ProviderResult {
///     Ok(ProviderReply::bytes(0, request.payload())
///         .map_err(|_| elm::HookError::new(-22))?)
/// }
/// ```
pub fn provider(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match provider_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
/// 为某个 provider 契约声明只读快照调用门。
///
/// # 参数与签名
///
/// attribute 只接受必需参数 `contract = "identifier@version"`，其值必须与对应 provider
/// 端口使用的契约一致。函数签名必须为：
///
/// ```text
/// fn(&elm::SnapshotRequest, &mut [u8]) -> elm::SnapshotResult
/// ```
///
/// 宏生成 `__elm_provider_snapshot_<函数名>` 导出符号、快照 ABI trampoline 和 snapshot
/// 元数据。输出切片容量由调用方提供；实现只能写入返回 `SnapshotReply::payload_len` 指定的
/// 前缀。普通快照应返回 `SnapshotReply::complete`，分页快照可返回
/// `SnapshotReply::more`，且下一游标必须非零并与当前游标不同。
///
/// 快照接口应只观察 provider 状态，不应改变服务语义。若无法在当前缓冲区中形成一条完整
/// 记录，应返回明确错误或使用分页，而不是截断记录。
///
/// # 示例
///
/// ```ignore
/// use elm::{SnapshotReply, SnapshotRequest, SnapshotResult};
///
/// #[elm::provider_snapshot(contract = "demo.counter@1")]
/// fn counter_snapshot(
///     request: &SnapshotRequest,
///     output: &mut [u8],
/// ) -> SnapshotResult {
///     let value = request.cursor.to_le_bytes();
///     if output.len() < value.len() {
///         return Ok(SnapshotReply::error(-12));
///     }
///     output[..value.len()].copy_from_slice(&value);
///     Ok(SnapshotReply::complete(value.len(), 1))
/// }
/// ```
pub fn provider_snapshot(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match provider_snapshot_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
/// 声明一个供其他 ELM 通过受管导入调用的 export。
///
/// # 参数语法
///
/// ```text
/// #[elm::export(
///     name = "demo.calculate",
///     contract = "demo.calculate@1",
///     version = 1,
///     mode = "managed",
///     visibility = "dependency"
/// )]
/// ```
///
/// - `contract` 和非零 `version` 必填；
/// - `name` 默认使用 Rust 函数名，并同时成为 EBI export 名和真实导出符号；
/// - `mode` 可为 `"managed"` 或 `"direct-pinned"`，默认 `"managed"`；
/// - `visibility` 可为 `"dependency"`、`"private"` 或 `"subtree"`，默认
///   `"dependency"`。
///
/// `name` 必须满足 EBI symbol 约束，`contract` 必须包含版本。当前 Rust 开发框架只为函数
/// 生成受管调用 trampoline；选择 `direct-pinned` 会把导出声明为直接固定能力，调用双方必须
/// 额外满足运行时的原生能力、代际固定和策略要求。
///
/// # 规范签名
///
/// ```text
/// fn(&elm::ManagedRequest) -> elm::ManagedResult
/// ```
///
/// trampoline 校验调用方、被调用方代际和载荷边界，再把 `ManagedResult` 写回固定回复帧。
/// 热替换时，受管调用按 generation 路由；实现不得把 `ManagedRequest` 中的借用保存到返回后。
///
/// # 示例
///
/// ```ignore
/// use elm::{ManagedRequest, ManagedResult, ProviderReply};
///
/// #[elm::export(
///     name = "demo.echo",
///     contract = "demo.echo@1",
///     version = 1,
///     visibility = "dependency"
/// )]
/// fn echo(request: &ManagedRequest) -> ManagedResult {
///     Ok(ProviderReply::bytes(0, request.payload())
///         .map_err(|_| elm::HookError::new(-22))?)
/// }
/// ```
pub fn export(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match export_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
/// 声明一个由装载器绑定的静态 import 槽。
///
/// # 作用对象
///
/// `import` 只能标记不可变 `static`。`mode = "managed"` 时槽类型必须精确为
/// `elm::ManagedImport`，初始化值通常为 `ManagedImport::new()`；
/// `mode = "direct-pinned"` 时必须为 `elm::UnsafeDirectImport`。禁止 `static mut`，也禁止
/// 手写 `used`、`no_mangle`、`export_name` 或 `link_section`，因为这些属性由宏独占管理。
///
/// # 参数语法
///
/// ```text
/// #[elm::import(
///     name = "demo.echo",
///     contract = "demo.echo@1",
///     min_version = 1,
///     max_version = 2,
///     mode = "managed",
///     scope = "any",
///     optional = false
/// )]
/// ```
///
/// - `name` 与 `contract` 必填；
/// - 可用 `version = N` 同时指定单一版本，或使用 `min_version`/`max_version`；默认最低版本
///   为 1，最高版本等于最低版本，且必须满足 `1 <= min_version <= max_version`；
/// - `mode` 为 `"managed"` 或 `"direct-pinned"`，默认 `"managed"`；
/// - `scope` 为 `"any"`、`"ancestor"` 或 `"builtin"`，默认 `"any"`；
/// - `optional` 为布尔值，默认 `false`。
///
/// 宏把槽导出为 `__elm_import_<小写静态名>`，放入 `.data.elm_imports`，并生成 import
/// 元数据。装载器在初始化事务中写入句柄或直接地址；必需 import 无匹配目标时镜像不能激活，
/// 可选 import 保持未绑定状态。
///
/// `ManagedImport` 是推荐路径，负责 call id、固定载荷编码和回复关联校验。直接固定导入返回
/// 裸地址，调用方必须在 `unsafe` 代码中自行证明函数签名、代际固定、生命周期和权限，且它会
/// 限制热替换能力。
///
/// # 示例
///
/// ```ignore
/// use elm::ManagedImport;
///
/// #[elm::import(
///     name = "demo.echo",
///     contract = "demo.echo@1",
///     min_version = 1,
///     max_version = 1,
///     optional = true
/// )]
/// static ECHO: ManagedImport = ManagedImport::new();
/// ```
pub fn import(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as ItemStatic);
    match import_impl(attr.into(), item) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
/// 声明当前 ELM 在初始化前必须取得的 Kernel API 命名空间。
///
/// attribute 只能标记不可变 `static`。静态对象使用 `kernel_api::ApiImport<Table>`，其
/// initializer 中的 identifier、版本和能力必须与 attribute 保持一致；打包器会把该声明
/// 转换为 EBI Kernel API requirement，内核在执行 `on_initialize` 前完成版本、布局、权限和
/// capability 校验。
///
/// ```ignore
/// #[elm::kernel_api(namespace = "kernel.time", version = 1, capabilities = 3)]
/// static TIME: kernel_api::ApiImport<kernel_api::time::TimeApiV1> =
///     kernel_api::ApiImport::new("kernel.time", 1, 3);
/// ```
pub fn kernel_api(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as ItemStatic);
    match kernel_api_impl(attr.into(), item) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
/// 为具名字段结构体派生 ELM 固定小端载荷协议。
///
/// # 参数与布局
///
/// attribute 参数是一个字符串字面量，表示载荷契约：
///
/// ```text
/// #[elm::payload("demo.request@1")]
/// ```
///
/// 契约必须是合法的 `identifier@version`。被标记项必须是无泛型的具名字段结构体。字段按
/// 源码声明顺序紧密编码，不采用 Rust 内存布局、不插入对齐填充，并统一使用小端字节序。
/// v1 允许的字段类型只有：
///
/// - `u8`、`u16`、`u32`、`u64`；
/// - `i8`、`i16`、`i32`、`i64`；
/// - `bool`，在线格式中仅接受 `0` 和 `1`；
/// - `[u8; N]`，其中 `N` 必须是整数字面量。
///
/// 禁止引用、裸指针、`usize`/`isize`、浮点、枚举、动态容器、嵌套自定义类型和泛型。总线
/// 格式尺寸不得超过 256 字节。
///
/// # 生成内容
///
/// 宏为结构体实现 `elm::ElmPayload`，生成 `CONTRACT`、`WIRE_SIZE`、
/// `encode` 和 `decode`，并写入包含契约与固定尺寸的 payload 元数据。`decode` 要求输入长度
/// 精确等于 `WIRE_SIZE`；尾随字节不会被忽略。
///
/// # 示例
///
/// ```ignore
/// use elm::ElmPayload;
///
/// #[elm::payload("demo.request@1")]
/// #[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// struct Request {
///     opcode: u32,
///     enabled: bool,
///     nonce: [u8; 8],
/// }
///
/// let value = Request { opcode: 3, enabled: true, nonce: [0; 8] };
/// let mut bytes = [0_u8; Request::WIRE_SIZE];
/// value.encode(&mut bytes)?;
/// assert_eq!(Request::decode(&bytes)?, value);
/// # Ok::<(), elm::PayloadError>(())
/// ```
pub fn payload(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as ItemStruct);
    match payload_impl(attr.into(), item) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
/// 把普通 Rust 函数声明为可被 mixin 扩展的分阶段补缀点。
///
/// # 参数语法
///
/// ```text
/// #[elm::mixin_point(
///     name = "scheduler.select",
///     contract = "scheduler.select.frame@1",
///     stages(ingress, substitute, egress, observe)
/// )]
/// ```
///
/// `contract` 必填，并且必须与帧类型实现的 `ElmPayload::CONTRACT` 一致。`name` 默认使用
/// 函数名。`stages(...)` 可从 `ingress`、`substitute`、`egress`、`observe` 中选择一个或
/// 多个阶段；省略时启用全部四个阶段，空列表和重复阶段会被拒绝。最终点名是
/// `<name>.<stage>`，每个最终名称都必须满足长度和 identifier 限制。
///
/// # 规范签名
///
/// ```text
/// fn(&mut T) -> elm::PointResult
/// where T: elm::ElmPayload
/// ```
///
/// 宏把原函数改名为私有原始实现，并以原名称生成包装函数。包装函数按以下固定顺序运行：
///
/// 1. `ingress`：在原实现前分发，可修改帧；
/// 2. `substitute`：可返回替换帧并跳过原实现；
/// 3. 未被替换时调用原实现；
/// 4. `egress`：在结果形成后分发，可继续修改帧；
/// 5. `observe`：最后观察，返回的替换标志不再改变控制流。
///
/// 任一阶段返回拒绝或运行时错误时，包装函数返回 `HookError`。宏会为每个启用阶段生成独立
/// extension-point 元数据，但不生成外部 ABI 导出；实际分发通过 ELM 运行时根 API 完成。
///
/// # 示例
///
/// ```ignore
/// use elm::PointResult;
///
/// #[elm::payload("demo.select.frame@1")]
/// struct SelectFrame {
///     candidate: u64,
/// }
///
/// #[elm::mixin_point(
///     name = "scheduler.select",
///     contract = "demo.select.frame@1",
///     stages(ingress, substitute, egress, observe)
/// )]
/// fn select(frame: &mut SelectFrame) -> PointResult {
///     frame.candidate = 1;
///     Ok(())
/// }
/// ```
pub fn mixin_point(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match mixin_point_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
/// 声明一个附着到其他 ELM 补缀点的 mixin 处理器。
///
/// # 参数语法
///
/// ```text
/// #[elm::mixin(
///     target = "scheduler.core",
///     point = "scheduler.select",
///     stage = "ingress",
///     contract = "demo.select.frame@1",
///     priority = -100,
///     handler_contract = "demo.mixin.handler@1"
/// )]
/// ```
///
/// - `target`：必需，目标 ELM 的规范名称 identifier；
/// - `point`：必需，不含阶段后缀的补缀点名称；
/// - `stage`：必需，为 `"ingress"`、`"substitute"`、`"egress"` 或 `"observe"`；
/// - `contract`：必需，补缀帧契约，必须与目标点完全匹配；
/// - `priority`：可选 `i32`，默认 0；运行时按稳定规则排序同一点处理器；
/// - `handler_contract`：可选，默认 `elm.mixin.<函数名>@1`，用于声明处理器 provider 契约。
///
/// # 规范签名
///
/// ```text
/// fn(&mut T) -> elm::MixinControl
/// where T: elm::ElmPayload
/// ```
///
/// 宏生成 `__elm_mixin_<函数名>` provider ABI trampoline，并同时生成 provider 与 extension
/// 两条元数据记录。运行时把线格式帧解码为 `T`，调用处理器，再根据返回值决定控制流：
///
/// - `Continue`：保留当前帧并继续后续处理器；
/// - `Stop`：停止当前阶段的后续处理器，不替换帧；
/// - `Replace`：用修改后的帧替换当前帧并继续；
/// - `ReplaceAndStop`：替换帧并停止当前阶段；
/// - `Deny`：拒绝整个补缀点调用。
///
/// 附着仍需通过运行时策略、目标点声明、契约、阶段、权限和优先级校验。声明 mixin 不会绕过
/// per-cell capability policy，也不会自动获得目标 ELM 的私有接口访问权。
///
/// # 示例
///
/// ```ignore
/// use elm::MixinControl;
///
/// #[elm::payload("demo.select.frame@1")]
/// struct SelectFrame {
///     candidate: u64,
/// }
///
/// #[elm::mixin(
///     target = "scheduler.core",
///     point = "scheduler.select",
///     stage = "substitute",
///     contract = "demo.select.frame@1",
///     priority = 100
/// )]
/// fn force_idle(frame: &mut SelectFrame) -> MixinControl {
///     frame.candidate = 0;
///     MixinControl::ReplaceAndStop
/// }
/// ```
pub fn mixin(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match mixin_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn lifecycle_attribute(
    attr: TokenStream,
    item: TokenStream,
    hook_kind: u32,
    phase: u16,
    symbol: &str,
) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match lifecycle_impl(attr.into(), function, hook_kind, phase, symbol) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn lifecycle_impl(
    attr: TokenStream2,
    function: ItemFn,
    hook_kind: u32,
    phase: u16,
    symbol: &str,
) -> syn::Result<TokenStream2> {
    require_empty_attr(attr)?;
    validate_function(&function, 1)?;
    let ident = &function.sig.ident;
    let abi_ident = format_ident!("__elm_abi_{}", symbol);
    let metadata = metadata_item(
        ident,
        symbol,
        metadata_record(
            KIND_LIFECYCLE,
            vec![
                MetaField::utf8(FIELD_SYMBOL, symbol),
                MetaField::u32(FIELD_HOOK_KIND, hook_kind),
            ],
        ),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            context: *mut ::elm::ElmNativeHookContextV1,
        ) -> i32 {
            unsafe {
                ::elm::__private::lifecycle_trampoline(context, #phase, #ident)
            }
        }

        #metadata
    })
}

fn migration_export_attribute(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match migration_export_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn migration_export_impl(attr: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    require_empty_attr(attr)?;
    validate_function(&function, 2)?;
    let ident = &function.sig.ident;
    let symbol = "on_migrate_export";
    let abi_ident = format_ident!("__elm_abi_on_migrate_export");
    let metadata = metadata_item(
        ident,
        symbol,
        metadata_record(
            KIND_LIFECYCLE,
            vec![
                MetaField::utf8(FIELD_SYMBOL, symbol),
                MetaField::u32(FIELD_HOOK_KIND, 3),
            ],
        ),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            context: *mut ::elm::ElmNativeMigrationContextV1,
        ) -> i32 {
            unsafe {
                ::elm::__private::migration_export_trampoline(context, #ident)
            }
        }

        #metadata
    })
}

fn migration_input_attribute(
    attr: TokenStream,
    item: TokenStream,
    hook_kind: u32,
    phase: u16,
    symbol: &str,
) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match migration_input_impl(attr.into(), function, hook_kind, phase, symbol) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn migration_input_impl(
    attr: TokenStream2,
    function: ItemFn,
    hook_kind: u32,
    phase: u16,
    symbol: &str,
) -> syn::Result<TokenStream2> {
    require_empty_attr(attr)?;
    validate_function(&function, 2)?;
    let ident = &function.sig.ident;
    let abi_ident = format_ident!("__elm_abi_{}", symbol);
    let metadata = metadata_item(
        ident,
        symbol,
        metadata_record(
            KIND_LIFECYCLE,
            vec![
                MetaField::utf8(FIELD_SYMBOL, symbol),
                MetaField::u32(FIELD_HOOK_KIND, hook_kind),
            ],
        ),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            context: *mut ::elm::ElmNativeMigrationContextV1,
        ) -> i32 {
            unsafe {
                ::elm::__private::migration_input_trampoline(
                    context,
                    #phase,
                    #ident,
                )
            }
        }

        #metadata
    })
}

fn entry_impl(attr: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    require_empty_attr(attr)?;
    validate_function(&function, 1)?;
    let ident = &function.sig.ident;
    let symbol = format!("__elm_entry_{}", ident);
    validate_symbol(&symbol, "entry symbol")?;
    let abi_ident = format_ident!("__elm_abi_entry_{}", ident);
    let metadata = metadata_item(
        ident,
        "entry",
        metadata_record(KIND_ENTRY, vec![MetaField::utf8(FIELD_SYMBOL, &symbol)]),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            frame: *mut ::elm::ElmNativeEntryFrameV1,
        ) -> i32 {
            unsafe { ::elm::__private::entry_trampoline(frame, #ident) }
        }

        #metadata
    })
}

fn provider_impl(attr: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    validate_function(&function, 1)?;
    let args = MetaArgs::parse(attr)?;
    let contract = args.required_string("contract")?;
    validate_contract(&contract)?;
    let access = parse_access(args.string_or("access", "public")?)?;
    let direction = parse_direction(args.string_or("direction", "control")?)?;
    let mode = parse_mode(args.string_or("mode", "shared")?)?;
    args.finish()?;
    let ident = &function.sig.ident;
    let symbol = format!("__elm_provider_{}", ident);
    validate_symbol(&symbol, "provider symbol")?;
    let abi_ident = format_ident!("__elm_abi_provider_{}", ident);
    let metadata = metadata_item(
        ident,
        "provider",
        metadata_record(
            KIND_PROVIDER,
            vec![
                MetaField::utf8(FIELD_SYMBOL, &symbol),
                MetaField::utf8(FIELD_CONTRACT, &contract),
                MetaField::u32(FIELD_FLAGS, 0),
                MetaField::u32(FIELD_ACCESS, access),
                MetaField::u32(FIELD_DIRECTION, direction),
                MetaField::u32(FIELD_MODE, mode),
            ],
        ),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            frame: *mut ::elm::ElmNativeProviderCallV1,
        ) -> i32 {
            unsafe { ::elm::__private::provider_trampoline(frame, #ident) }
        }

        #metadata
    })
}

fn provider_snapshot_impl(attr: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    validate_function(&function, 2)?;
    let args = MetaArgs::parse(attr)?;
    let contract = args.required_string("contract")?;
    validate_contract(&contract)?;
    args.finish()?;
    let ident = &function.sig.ident;
    let symbol = format!("__elm_provider_snapshot_{}", ident);
    validate_symbol(&symbol, "provider snapshot symbol")?;
    let abi_ident = format_ident!("__elm_abi_provider_snapshot_{}", ident);
    let metadata = metadata_item(
        ident,
        "provider_snapshot",
        metadata_record(
            KIND_PROVIDER_SNAPSHOT,
            vec![
                MetaField::utf8(FIELD_SYMBOL, &symbol),
                MetaField::utf8(FIELD_CONTRACT, &contract),
            ],
        ),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            frame: *mut ::elm::ElmNativeProviderSnapshotV1,
        ) -> i32 {
            unsafe { ::elm::__private::snapshot_trampoline(frame, #ident) }
        }

        #metadata
    })
}

fn export_impl(attr: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    validate_function(&function, 1)?;
    let args = MetaArgs::parse(attr)?;
    let name = args.string_or("name", &function.sig.ident.to_string())?;
    let contract = args.required_string("contract")?;
    let version = args.required_u32("version")?;
    validate_symbol(&name, "export name")?;
    validate_contract(&contract)?;
    if version == 0 {
        return Err(syn::Error::new(
            Span::call_site(),
            "export version 必须大于 0",
        ));
    }
    let mode = args.string_or("mode", "managed")?;
    let visibility = args.string_or("visibility", "dependency")?;
    let mut flags = match mode.as_str() {
        "managed" => EXPORT_MANAGED,
        "direct-pinned" => EXPORT_DIRECT_PINNED,
        _ => return Err(syn::Error::new(Span::call_site(), "未知 export mode")),
    };
    flags |= match visibility.as_str() {
        "dependency" => EXPORT_DEPENDENCY,
        "private" => EXPORT_PRIVATE,
        "subtree" => EXPORT_SUBTREE,
        _ => return Err(syn::Error::new(Span::call_site(), "未知 export visibility")),
    };
    args.finish()?;
    let ident = &function.sig.ident;
    let abi_ident = format_ident!("__elm_abi_export_{}", ident);
    let metadata = metadata_item(
        ident,
        "export",
        metadata_record(
            KIND_EXPORT,
            vec![
                MetaField::utf8(FIELD_SYMBOL, &name),
                MetaField::utf8(FIELD_NAME, &name),
                MetaField::utf8(FIELD_CONTRACT, &contract),
                MetaField::u32(FIELD_VERSION, version),
                MetaField::u32(FIELD_FLAGS, flags),
            ],
        ),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #name)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            frame: *mut ::elm::ElmNativeManagedCallV1,
        ) -> i32 {
            unsafe { ::elm::__private::managed_trampoline(frame, #ident) }
        }

        #metadata
    })
}

fn import_impl(attr: TokenStream2, mut item: ItemStatic) -> syn::Result<TokenStream2> {
    let args = MetaArgs::parse(attr)?;
    let name = args.required_string("name")?;
    let contract = args.required_string("contract")?;
    let min_version = args.u32_or("min_version", args.u32_or("version", 1)?)?;
    let max_version = args.u32_or("max_version", min_version)?;
    validate_symbol(&name, "import name")?;
    validate_contract(&contract)?;
    if min_version == 0 || max_version < min_version {
        return Err(syn::Error::new(
            Span::call_site(),
            "import 版本范围必须满足 1 <= min_version <= max_version",
        ));
    }
    let mode = args.string_or("mode", "managed")?;
    validate_import_slot(&item, &mode)?;
    let scope = args.string_or("scope", "any")?;
    let optional = args.bool_or("optional", false)?;
    let mut flags = match mode.as_str() {
        "managed" => IMPORT_MANAGED,
        "direct-pinned" => IMPORT_DIRECT_PINNED,
        _ => return Err(syn::Error::new(Span::call_site(), "未知 import mode")),
    };
    if optional {
        flags |= IMPORT_OPTIONAL;
    }
    flags |= match scope.as_str() {
        "any" => 0,
        "ancestor" => IMPORT_ALLOW_ANCESTOR,
        "builtin" => IMPORT_ALLOW_BUILTIN,
        _ => return Err(syn::Error::new(Span::call_site(), "未知 import scope")),
    };
    args.finish()?;
    let ident = item.ident.clone();
    let symbol = format!("__elm_import_{}", ident.to_string().to_ascii_lowercase());
    validate_symbol(&symbol, "import slot symbol")?;
    item.attrs.push(syn::parse_quote!(#[used]));
    item.attrs
        .push(syn::parse_quote!(#[unsafe(export_name = #symbol)]));
    item.attrs
        .push(syn::parse_quote!(#[unsafe(link_section = ".data.elm_imports")]));
    let metadata = metadata_item(
        &ident,
        "import",
        metadata_record(
            KIND_IMPORT,
            vec![
                MetaField::utf8(FIELD_SYMBOL, &symbol),
                MetaField::utf8(FIELD_NAME, &name),
                MetaField::utf8(FIELD_CONTRACT, &contract),
                MetaField::u32(FIELD_MIN_VERSION, min_version),
                MetaField::u32(FIELD_MAX_VERSION, max_version),
                MetaField::u32(FIELD_FLAGS, flags),
            ],
        ),
    );
    Ok(quote! {
        #item
        #metadata
    })
}

fn kernel_api_impl(attr: TokenStream2, item: ItemStatic) -> syn::Result<TokenStream2> {
    let args = MetaArgs::parse(attr)?;
    let namespace = args.required_string("namespace")?;
    let version = args.u32_or("version", 1)?;
    let capabilities = args.u64_or("capabilities", 0)?;
    if namespace.len() > 64
        || !namespace.starts_with("kernel.")
        || namespace.ends_with('.')
        || namespace.contains("..")
        || !namespace.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(syn::Error::new(
            Span::call_site(),
            "Kernel API namespace 必须是长度不超过 64 的 kernel.* identifier",
        ));
    }
    if version == 0 || version > u16::MAX as u32 {
        return Err(syn::Error::new(
            Span::call_site(),
            "Kernel API version 必须位于 1..=65535",
        ));
    }
    if matches!(item.mutability, syn::StaticMutability::Mut(_)) {
        return Err(syn::Error::new_spanned(
            &item.mutability,
            "Kernel API 导入槽必须是不可变 static",
        ));
    }
    validate_kernel_api_slot(&item, &namespace, version, capabilities)?;
    args.finish()?;
    let ident = item.ident.clone();
    let metadata = metadata_item(
        &ident,
        "kernel_api",
        metadata_record(
            KIND_KERNEL_API,
            vec![
                MetaField::utf8(FIELD_NAME, &namespace),
                MetaField::u32(FIELD_VERSION, version),
                MetaField::u64(FIELD_CAPABILITIES, capabilities),
            ],
        ),
    );
    Ok(quote! {
        #item
        #metadata
    })
}

fn validate_kernel_api_slot(
    item: &ItemStatic,
    namespace: &str,
    version: u32,
    capabilities: u64,
) -> syn::Result<()> {
    let Type::Path(path) = item.ty.as_ref() else {
        return Err(syn::Error::new_spanned(
            &item.ty,
            "Kernel API 导入槽类型必须是 kernel_api::ApiImport<Table>",
        ));
    };
    let Some(segment) = path.path.segments.last() else {
        return Err(syn::Error::new_spanned(
            &item.ty,
            "Kernel API 导入槽类型必须是 kernel_api::ApiImport<Table>",
        ));
    };
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return Err(syn::Error::new_spanned(
            &item.ty,
            "Kernel API 导入槽必须绑定一个由 kernel-api 发布的函数表类型",
        ));
    };
    if path.qself.is_some()
        || segment.ident != "ApiImport"
        || arguments.args.len() != 1
        || !matches!(arguments.args.first(), Some(syn::GenericArgument::Type(_)))
    {
        return Err(syn::Error::new_spanned(
            &item.ty,
            "Kernel API 导入槽类型必须是 kernel_api::ApiImport<Table>",
        ));
    }

    let Expr::Call(call) = item.expr.as_ref() else {
        return Err(syn::Error::new_spanned(
            &item.expr,
            "Kernel API 导入槽必须使用 ApiImport::new(namespace, version, capabilities) 初始化",
        ));
    };
    let Expr::Path(function) = call.func.as_ref() else {
        return Err(syn::Error::new_spanned(
            &call.func,
            "Kernel API 导入槽必须调用 ApiImport::new",
        ));
    };
    if function
        .path
        .segments
        .last()
        .is_none_or(|segment| segment.ident != "new")
        || call.args.len() != 3
    {
        return Err(syn::Error::new_spanned(
            call,
            "Kernel API 导入槽必须调用 ApiImport::new(namespace, version, capabilities)",
        ));
    }

    let mut args = call.args.iter();
    let initializer_namespace = expression_string(args.next().expect("参数数量已检查"))?;
    let initializer_version = expression_u64(args.next().expect("参数数量已检查"))?;
    let initializer_capabilities = expression_u64(args.next().expect("参数数量已检查"))?;
    if initializer_namespace != namespace
        || initializer_version != u64::from(version)
        || initializer_capabilities != capabilities
    {
        return Err(syn::Error::new_spanned(
            &item.expr,
            "ApiImport::new 的 namespace、version 和 capabilities 必须与 attribute 完全一致",
        ));
    }
    Ok(())
}

fn expression_string(expression: &Expr) -> syn::Result<String> {
    match expression {
        Expr::Lit(ExprLit {
            lit: Lit::Str(value),
            ..
        }) => Ok(value.value()),
        _ => Err(syn::Error::new_spanned(
            expression,
            "此参数必须是字符串字面量",
        )),
    }
}

fn expression_u64(expression: &Expr) -> syn::Result<u64> {
    match expression {
        Expr::Lit(ExprLit {
            lit: Lit::Int(value),
            ..
        }) => value.base10_parse(),
        _ => Err(syn::Error::new_spanned(
            expression,
            "此参数必须是非负整数字面量",
        )),
    }
}

fn payload_impl(attr: TokenStream2, item: ItemStruct) -> syn::Result<TokenStream2> {
    let contract = syn::parse2::<LitStr>(attr)?.value();
    validate_contract(&contract)?;
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &item.generics,
            "ELM 固定载荷不允许泛型参数",
        ));
    }
    let Fields::Named(fields) = &item.fields else {
        return Err(syn::Error::new_spanned(
            &item.fields,
            "ELM 固定载荷必须使用具名字段结构体",
        ));
    };
    let mut wire_size = 0usize;
    let mut encoders = Vec::new();
    let mut decoders = Vec::new();
    for field in &fields.named {
        let ident = field.ident.as_ref().expect("具名字段");
        let wire = WireType::parse(&field.ty)?;
        wire_size = wire_size
            .checked_add(wire.size())
            .ok_or_else(|| syn::Error::new_spanned(&field.ty, "载荷尺寸溢出"))?;
        encoders.push(wire.encoder(ident));
        decoders.push(wire.decoder(ident));
    }
    if wire_size > 256 {
        return Err(syn::Error::new_spanned(
            &item.ident,
            "ELM v1 固定载荷不得超过 256 字节",
        ));
    }
    let ident = &item.ident;
    let metadata = metadata_item(
        ident,
        "payload",
        metadata_record(
            KIND_PAYLOAD,
            vec![
                MetaField::utf8(FIELD_PAYLOAD_CONTRACT, &contract),
                MetaField::u32(FIELD_WIRE_SIZE, wire_size as u32),
            ],
        ),
    );
    Ok(quote! {
        #item

        impl ::elm::ElmPayload for #ident {
            const CONTRACT: &'static str = #contract;
            const WIRE_SIZE: usize = #wire_size;

            fn encode(
                &self,
                output: &mut [u8],
            ) -> ::core::result::Result<usize, ::elm::PayloadError> {
                if output.len() < Self::WIRE_SIZE {
                    return Err(::elm::PayloadError::BufferTooSmall);
                }
                let mut offset = 0usize;
                #(#encoders)*
                Ok(offset)
            }

            fn decode(
                input: &[u8],
            ) -> ::core::result::Result<Self, ::elm::PayloadError> {
                if input.len() != Self::WIRE_SIZE {
                    return Err(::elm::PayloadError::SizeMismatch);
                }
                let mut offset = 0usize;
                let value = Self {
                    #(#decoders),*
                };
                if offset != input.len() {
                    return Err(::elm::PayloadError::SizeMismatch);
                }
                Ok(value)
            }
        }

        #metadata
    })
}

fn mixin_point_impl(attr: TokenStream2, mut function: ItemFn) -> syn::Result<TokenStream2> {
    validate_function(&function, 1)?;
    let args = MetaArgs::parse(attr)?;
    let point = args.string_or("name", &function.sig.ident.to_string())?;
    let contract = args.required_string("contract")?;
    let stages = args.stages()?;
    args.finish()?;
    validate_contract(&contract)?;
    for (stage, bit) in [
        ("ingress", 1),
        ("substitute", 2),
        ("egress", 4),
        ("observe", 8),
    ] {
        if stages & bit != 0 {
            validate_point(&format!("{point}.{stage}"))?;
        }
    }
    let (argument, _) = mutable_reference_argument(&function)?;
    let original_ident = format_ident!("__elm_original_{}", function.sig.ident);
    let wrapper_ident = function.sig.ident.clone();
    let visibility = function.vis.clone();
    let signature = function.sig.clone();
    let wrapper_attrs = function.attrs.clone();
    function.sig.ident = original_ident.clone();
    function.vis = syn::Visibility::Inherited;
    function.attrs.clear();

    let mut records = Vec::new();
    let ingress = stage_point(
        &point,
        "ingress",
        stages & 1 != 0,
        1,
        1,
        &contract,
        &mut records,
    );
    let substitute = stage_point(
        &point,
        "substitute",
        stages & 2 != 0,
        2,
        3,
        &contract,
        &mut records,
    );
    let egress = stage_point(
        &point,
        "egress",
        stages & 4 != 0,
        3,
        1,
        &contract,
        &mut records,
    );
    let observe = stage_point(
        &point,
        "observe",
        stages & 8 != 0,
        4,
        2,
        &contract,
        &mut records,
    );
    let metadata = metadata_item(&wrapper_ident, "mixin_point", metadata_blob(records));
    Ok(quote! {
        #function

        #(#wrapper_attrs)*
        #visibility #signature {
            ::elm::run_mixin_point(
                ::elm::MixinPointDescriptor {
                    contract: #contract,
                    ingress: #ingress,
                    substitute: #substitute,
                    egress: #egress,
                    observe: #observe,
                },
                #argument,
                #original_ident,
            )
        }

        #metadata
    })
}

fn mixin_impl(attr: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    validate_function(&function, 1)?;
    let args = MetaArgs::parse(attr)?;
    let target = args.required_string("target")?;
    let point = args.required_string("point")?;
    let stage = args.required_string("stage")?;
    let contract = args.required_string("contract")?;
    let priority = args.i32_or("priority", 0)?;
    let default_handler_contract = format!("elm.mixin.{}@1", function.sig.ident);
    let handler_contract = args.string_or("handler_contract", &default_handler_contract)?;
    args.finish()?;
    validate_identifier(&target, EBI_NAME_LEN, "mixin target")?;
    validate_contract(&contract)?;
    validate_contract(&handler_contract)?;
    let (_, frame_ty) = mutable_reference_argument(&function)?;
    let stage_code = parse_stage(&stage)?;
    let full_point = format!("{point}.{stage}");
    validate_point(&full_point)?;
    let ident = &function.sig.ident;
    let symbol = format!("__elm_mixin_{}", ident);
    validate_symbol(&symbol, "mixin symbol")?;
    let abi_ident = format_ident!("__elm_abi_mixin_{}", ident);
    let records = vec![
        metadata_record(
            KIND_PROVIDER,
            vec![
                MetaField::utf8(FIELD_SYMBOL, &symbol),
                MetaField::utf8(FIELD_CONTRACT, &handler_contract),
                MetaField::u32(FIELD_FLAGS, 0),
                MetaField::u32(FIELD_ACCESS, 3),
                MetaField::u32(FIELD_DIRECTION, 4),
                MetaField::u32(FIELD_MODE, 2),
            ],
        ),
        metadata_record(
            KIND_EXTENSION,
            vec![
                MetaField::utf8(FIELD_CONTRACT, &contract),
                MetaField::utf8(FIELD_TARGET, &target),
                MetaField::utf8(FIELD_POINT, &full_point),
                MetaField::u32(FIELD_STAGE, stage_code),
                MetaField::i32(FIELD_PRIORITY, priority),
                MetaField::utf8(FIELD_HANDLER_CONTRACT, &handler_contract),
                MetaField::utf8(FIELD_PAYLOAD_CONTRACT, &contract),
            ],
        ),
    ];
    let metadata = metadata_item(ident, "mixin", metadata_blob(records));
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            frame: *mut ::elm::ElmNativeProviderCallV1,
        ) -> i32 {
            unsafe {
                ::elm::__private::mixin_trampoline::<#frame_ty>(frame, #ident)
            }
        }

        #metadata
    })
}

fn stage_point(
    point: &str,
    stage: &str,
    enabled: bool,
    stage_code: u32,
    mode: u32,
    contract: &str,
    records: &mut Vec<Vec<u8>>,
) -> TokenStream2 {
    if !enabled {
        return quote!(None);
    }
    let full = format!("{point}.{stage}");
    records.push(metadata_record(
        KIND_EXTENSION_POINT,
        vec![
            MetaField::utf8(FIELD_CONTRACT, contract),
            MetaField::u32(FIELD_MODE, mode),
            MetaField::utf8(FIELD_POINT, &full),
            MetaField::u32(FIELD_STAGE, stage_code),
            MetaField::utf8(FIELD_PAYLOAD_CONTRACT, contract),
        ],
    ));
    quote!(Some(#full))
}

fn parse_stage(stage: &str) -> syn::Result<u32> {
    match stage {
        "ingress" => Ok(1),
        "substitute" => Ok(2),
        "egress" => Ok(3),
        "observe" => Ok(4),
        _ => Err(syn::Error::new(
            Span::call_site(),
            "mixin stage 必须是 ingress、substitute、egress 或 observe",
        )),
    }
}

fn parse_access(value: String) -> syn::Result<u32> {
    match value.as_str() {
        "internal" => Ok(1),
        "public" => Ok(2),
        "extension-only" => Ok(3),
        _ => Err(syn::Error::new(Span::call_site(), "未知 provider access")),
    }
}

fn parse_direction(value: String) -> syn::Result<u32> {
    match value.as_str() {
        "source" => Ok(1),
        "sink" => Ok(2),
        "duplex" => Ok(3),
        "control" => Ok(4),
        _ => Err(syn::Error::new(
            Span::call_site(),
            "未知 provider direction",
        )),
    }
}

fn parse_mode(value: String) -> syn::Result<u32> {
    match value.as_str() {
        "exclusive" => Ok(1),
        "shared" => Ok(2),
        "ordered" => Ok(3),
        "pipeline" => Ok(4),
        "broadcast" => Ok(5),
        _ => Err(syn::Error::new(Span::call_site(), "未知 provider mode")),
    }
}

fn validate_identifier(value: &str, max_len: usize, label: &str) -> syn::Result<()> {
    if value.is_empty()
        || value.len() > max_len
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(syn::Error::new(
            Span::call_site(),
            format!("{label} 不是有效 identifier"),
        ));
    }
    Ok(())
}

fn validate_symbol(value: &str, label: &str) -> syn::Result<()> {
    if value.is_empty()
        || value.len() > EBI_SYMBOL_NAME_LEN
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'@' | b':')
        })
    {
        return Err(syn::Error::new(
            Span::call_site(),
            format!("{label} 不是有效 EBI symbol"),
        ));
    }
    Ok(())
}

fn validate_contract(value: &str) -> syn::Result<()> {
    let Some((name, version)) = value.rsplit_once('@') else {
        return Err(syn::Error::new(
            Span::call_site(),
            "contract 必须包含 @version",
        ));
    };
    if value.len() > NEXUS_CONTRACT_LEN
        || name.is_empty()
        || version.is_empty()
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
        || !version.split('.').all(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(syn::Error::new(
            Span::call_site(),
            "contract 不是有效的 ELM 契约 identifier",
        ));
    }
    Ok(())
}

fn validate_import_slot(item: &ItemStatic, mode: &str) -> syn::Result<()> {
    if matches!(item.mutability, syn::StaticMutability::Mut(_)) {
        return Err(syn::Error::new_spanned(
            &item.mutability,
            "ELM import 槽必须是不可变 static，内部写入由框架 UnsafeCell 承担",
        ));
    }
    if let Some(attribute) = item.attrs.iter().find(|attribute| {
        let path = attribute.path();
        path.is_ident("used")
            || path.is_ident("no_mangle")
            || path.is_ident("export_name")
            || path.is_ident("link_section")
    }) {
        return Err(syn::Error::new_spanned(
            attribute,
            "ELM import 槽的导出名和段属性由 #[elm::import] 独占管理",
        ));
    }
    let expected = match mode {
        "managed" => "ManagedImport",
        "direct-pinned" => "UnsafeDirectImport",
        _ => return Err(syn::Error::new(Span::call_site(), "未知 import mode")),
    };
    let Type::Path(path) = item.ty.as_ref() else {
        return Err(syn::Error::new_spanned(
            &item.ty,
            format!("{mode} import 槽类型必须是 {expected}"),
        ));
    };
    let Some(segment) = path.path.segments.last() else {
        return Err(syn::Error::new_spanned(&item.ty, "ELM import 槽类型无效"));
    };
    if path.qself.is_some()
        || segment.ident != expected
        || !matches!(segment.arguments, syn::PathArguments::None)
    {
        return Err(syn::Error::new_spanned(
            &item.ty,
            format!("{mode} import 槽类型必须是 {expected}"),
        ));
    }
    Ok(())
}

fn validate_point(value: &str) -> syn::Result<()> {
    validate_identifier(value, RELATION_POINT_LEN, "mixin point")
}

fn validate_function(function: &ItemFn, argument_count: usize) -> syn::Result<()> {
    if function.sig.constness.is_some()
        || function.sig.asyncness.is_some()
        || function.sig.unsafety.is_some()
        || function.sig.abi.is_some()
        || function.sig.variadic.is_some()
        || !function.sig.generics.params.is_empty()
    {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "ELM attribute 函数必须是非泛型安全 Rust 函数，不能手写 extern ABI",
        ));
    }
    if function.sig.inputs.len() != argument_count
        || function
            .sig
            .inputs
            .iter()
            .any(|argument| matches!(argument, FnArg::Receiver(_)))
    {
        return Err(syn::Error::new_spanned(
            &function.sig.inputs,
            format!("该 ELM attribute 要求恰好 {argument_count} 个普通参数"),
        ));
    }
    if matches!(function.sig.output, ReturnType::Default) {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "ELM attribute 函数必须显式返回对应的 Result 类型",
        ));
    }
    Ok(())
}

fn mutable_reference_argument(function: &ItemFn) -> syn::Result<(Ident, Type)> {
    let Some(FnArg::Typed(argument)) = function.sig.inputs.first() else {
        return Err(syn::Error::new_spanned(
            &function.sig.inputs,
            "mixin 函数缺少帧参数",
        ));
    };
    let Pat::Ident(pattern) = argument.pat.as_ref() else {
        return Err(syn::Error::new_spanned(
            &argument.pat,
            "mixin 帧参数必须使用简单标识符",
        ));
    };
    let Type::Reference(reference) = argument.ty.as_ref() else {
        return Err(syn::Error::new_spanned(
            &argument.ty,
            "mixin 帧参数必须是可变借用",
        ));
    };
    if reference.mutability.is_none() {
        return Err(syn::Error::new_spanned(
            &argument.ty,
            "mixin 帧参数必须是可变借用",
        ));
    }
    Ok((pattern.ident.clone(), (*reference.elem).clone()))
}

fn require_empty_attr(attr: TokenStream2) -> syn::Result<()> {
    if attr.is_empty() {
        Ok(())
    } else {
        Err(syn::Error::new_spanned(attr, "该 attribute 不接受参数"))
    }
}

#[derive(Clone)]
enum MetaValue {
    String(String),
    U32(u32),
    U64(u64),
    I32(i32),
    Bool(bool),
    Stages(u32),
}

struct MetaArgs {
    values: std::cell::RefCell<BTreeMap<String, MetaValue>>,
}

impl MetaArgs {
    fn parse(tokens: TokenStream2) -> syn::Result<Self> {
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(tokens)?;
        let mut values = BTreeMap::new();
        for meta in metas {
            match meta {
                Meta::NameValue(value) => {
                    let Some(name) = value.path.get_ident().map(ToString::to_string) else {
                        return Err(syn::Error::new_spanned(value.path, "参数名必须是标识符"));
                    };
                    let parsed = parse_meta_value(value.value)?;
                    if values.insert(name.clone(), parsed).is_some() {
                        return Err(syn::Error::new_spanned(value.path, "重复 attribute 参数"));
                    }
                }
                Meta::List(list) if list.path.is_ident("stages") => {
                    let paths =
                        list.parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated)?;
                    let mut mask = 0u32;
                    for path in paths {
                        let Some(stage) = path.get_ident().map(ToString::to_string) else {
                            return Err(syn::Error::new_spanned(path, "stage 必须是标识符"));
                        };
                        let bit = 1 << (parse_stage(&stage)? - 1);
                        if mask & bit != 0 {
                            return Err(syn::Error::new_spanned(path, "stage 不能重复"));
                        }
                        mask |= bit;
                    }
                    if mask == 0
                        || values
                            .insert("stages".into(), MetaValue::Stages(mask))
                            .is_some()
                    {
                        return Err(syn::Error::new_spanned(list, "stages 不能为空或重复"));
                    }
                }
                other => {
                    return Err(syn::Error::new_spanned(other, "未知 ELM attribute 参数"));
                }
            }
        }
        Ok(Self {
            values: std::cell::RefCell::new(values),
        })
    }

    fn required_string(&self, name: &str) -> syn::Result<String> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::String(value)) if !value.is_empty() => Ok(value),
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是非空字符串"),
            )),
            None => Err(syn::Error::new(
                Span::call_site(),
                format!("缺少必需参数 {name}"),
            )),
        }
    }

    fn string_or(&self, name: &str, default: &str) -> syn::Result<String> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::String(value)) if !value.is_empty() => Ok(value),
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是非空字符串"),
            )),
            None => Ok(default.to_string()),
        }
    }

    fn required_u32(&self, name: &str) -> syn::Result<u32> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::U32(value)) => Ok(value),
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是 u32 字面量"),
            )),
            None => Err(syn::Error::new(
                Span::call_site(),
                format!("缺少必需参数 {name}"),
            )),
        }
    }

    fn u64_or(&self, name: &str, default: u64) -> syn::Result<u64> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::U32(value)) => Ok(u64::from(value)),
            Some(MetaValue::U64(value)) => Ok(value),
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是 u64 字面量"),
            )),
            None => Ok(default),
        }
    }

    fn u32_or(&self, name: &str, default: u32) -> syn::Result<u32> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::U32(value)) => Ok(value),
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是 u32 字面量"),
            )),
            None => Ok(default),
        }
    }

    fn i32_or(&self, name: &str, default: i32) -> syn::Result<i32> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::I32(value)) => Ok(value),
            Some(MetaValue::U32(value)) => {
                i32::try_from(value).map_err(|_| syn::Error::new(Span::call_site(), "i32 参数越界"))
            }
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是 i32 字面量"),
            )),
            None => Ok(default),
        }
    }

    fn bool_or(&self, name: &str, default: bool) -> syn::Result<bool> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::Bool(value)) => Ok(value),
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是布尔字面量"),
            )),
            None => Ok(default),
        }
    }

    fn stages(&self) -> syn::Result<u32> {
        match self.values.borrow_mut().remove("stages") {
            Some(MetaValue::Stages(value)) => Ok(value),
            Some(_) => Err(syn::Error::new(Span::call_site(), "stages 格式无效")),
            None => Ok(0b1111),
        }
    }

    fn finish(&self) -> syn::Result<()> {
        if let Some(name) = self.values.borrow().keys().next() {
            Err(syn::Error::new(
                Span::call_site(),
                format!("未知或未使用的 attribute 参数 {name}"),
            ))
        } else {
            Ok(())
        }
    }
}

fn parse_meta_value(value: Expr) -> syn::Result<MetaValue> {
    match value {
        Expr::Lit(ExprLit {
            lit: Lit::Str(value),
            ..
        }) => Ok(MetaValue::String(value.value())),
        Expr::Lit(ExprLit {
            lit: Lit::Int(value),
            ..
        }) => parse_integer_literal(value),
        Expr::Lit(ExprLit {
            lit: Lit::Bool(value),
            ..
        }) => Ok(MetaValue::Bool(value.value)),
        Expr::Unary(ExprUnary {
            op: UnOp::Neg(_),
            expr,
            ..
        }) => {
            let Expr::Lit(ExprLit {
                lit: Lit::Int(value),
                ..
            }) = *expr
            else {
                return Err(syn::Error::new_spanned(expr, "负数参数必须是整数常量"));
            };
            parse_negative_integer_literal(value)
        }
        other => Err(syn::Error::new_spanned(
            other,
            "ELM attribute 只接受字符串、整数和布尔字面量",
        )),
    }
}

fn parse_integer_literal(value: syn::LitInt) -> syn::Result<MetaValue> {
    let digits = value.base10_digits();
    if digits.starts_with('-') {
        parse_negative_integer_literal(value)
    } else {
        let parsed = digits
            .parse::<u64>()
            .map_err(|_| syn::Error::new_spanned(&value, "u64 参数越界"))?;
        if let Ok(value) = u32::try_from(parsed) {
            Ok(MetaValue::U32(value))
        } else {
            Ok(MetaValue::U64(parsed))
        }
    }
}

fn parse_negative_integer_literal(value: syn::LitInt) -> syn::Result<MetaValue> {
    let digits = value
        .base10_digits()
        .strip_prefix('-')
        .unwrap_or(value.base10_digits());
    let magnitude = digits
        .parse::<u64>()
        .map_err(|_| syn::Error::new_spanned(&value, "负数参数必须是十进制整数"))?;
    if magnitude > i32::MAX as u64 + 1 {
        return Err(syn::Error::new_spanned(value, "i32 参数越界"));
    }
    Ok(MetaValue::I32(-(magnitude as i64) as i32))
}

enum WireType {
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    Bool,
    Bytes(usize),
}

impl WireType {
    fn parse(ty: &Type) -> syn::Result<Self> {
        match ty {
            Type::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 => {
                match path.path.segments[0].ident.to_string().as_str() {
                    "u8" => Ok(Self::U8),
                    "u16" => Ok(Self::U16),
                    "u32" => Ok(Self::U32),
                    "u64" => Ok(Self::U64),
                    "i8" => Ok(Self::I8),
                    "i16" => Ok(Self::I16),
                    "i32" => Ok(Self::I32),
                    "i64" => Ok(Self::I64),
                    "bool" => Ok(Self::Bool),
                    _ => Err(syn::Error::new_spanned(
                        ty,
                        "载荷字段只允许定宽整数、bool 和 [u8; N]",
                    )),
                }
            }
            Type::Array(array) => {
                let Type::Path(element) = array.elem.as_ref() else {
                    return Err(syn::Error::new_spanned(ty, "数组元素必须是 u8"));
                };
                if !element.path.is_ident("u8") {
                    return Err(syn::Error::new_spanned(ty, "数组元素必须是 u8"));
                }
                let Expr::Lit(ExprLit {
                    lit: Lit::Int(length),
                    ..
                }) = &array.len
                else {
                    return Err(syn::Error::new_spanned(
                        &array.len,
                        "数组长度必须是整数字面量",
                    ));
                };
                Ok(Self::Bytes(length.base10_parse()?))
            }
            _ => Err(syn::Error::new_spanned(
                ty,
                "载荷字段禁止引用、指针、usize、浮点、动态容器和泛型",
            )),
        }
    }

    const fn size(&self) -> usize {
        match self {
            Self::U8 | Self::I8 | Self::Bool => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 => 4,
            Self::U64 | Self::I64 => 8,
            Self::Bytes(length) => *length,
        }
    }

    fn encoder(&self, ident: &Ident) -> TokenStream2 {
        match self {
            Self::U8 => quote! {
                ::elm::__private::write_bytes(output, &mut offset, &[self.#ident])?;
            },
            Self::I8 => quote! {
                ::elm::__private::write_bytes(
                    output,
                    &mut offset,
                    &self.#ident.to_le_bytes(),
                )?;
            },
            Self::Bool => quote! {
                ::elm::__private::write_bytes(
                    output,
                    &mut offset,
                    &[u8::from(self.#ident)],
                )?;
            },
            Self::U16 | Self::U32 | Self::U64 | Self::I16 | Self::I32 | Self::I64 => quote! {
                ::elm::__private::write_bytes(
                    output,
                    &mut offset,
                    &self.#ident.to_le_bytes(),
                )?;
            },
            Self::Bytes(_) => quote! {
                ::elm::__private::write_bytes(output, &mut offset, &self.#ident)?;
            },
        }
    }

    fn decoder(&self, ident: &Ident) -> TokenStream2 {
        match self {
            Self::U8 => quote! {
                #ident: ::elm::__private::read_array::<1>(input, &mut offset)?[0]
            },
            Self::I8 => quote! {
                #ident: i8::from_le_bytes(
                    ::elm::__private::read_array::<1>(input, &mut offset)?,
                )
            },
            Self::U16 => decode_integer(ident, quote!(u16), 2),
            Self::U32 => decode_integer(ident, quote!(u32), 4),
            Self::U64 => decode_integer(ident, quote!(u64), 8),
            Self::I16 => decode_integer(ident, quote!(i16), 2),
            Self::I32 => decode_integer(ident, quote!(i32), 4),
            Self::I64 => decode_integer(ident, quote!(i64), 8),
            Self::Bool => quote! {
                #ident: ::elm::__private::read_bool(input, &mut offset)?
            },
            Self::Bytes(length) => quote! {
                #ident: ::elm::__private::read_array::<#length>(input, &mut offset)?
            },
        }
    }
}

fn decode_integer(ident: &Ident, ty: TokenStream2, size: usize) -> TokenStream2 {
    quote! {
        #ident: #ty::from_le_bytes(
            ::elm::__private::read_array::<#size>(input, &mut offset)?,
        )
    }
}

struct MetaField {
    tag: u16,
    kind: u16,
    bytes: Vec<u8>,
}

impl MetaField {
    fn utf8(tag: u16, value: &str) -> Self {
        Self {
            tag,
            kind: VALUE_UTF8,
            bytes: value.as_bytes().to_vec(),
        }
    }

    fn u32(tag: u16, value: u32) -> Self {
        Self {
            tag,
            kind: VALUE_U32,
            bytes: value.to_le_bytes().to_vec(),
        }
    }

    fn i32(tag: u16, value: i32) -> Self {
        Self {
            tag,
            kind: VALUE_I32,
            bytes: value.to_le_bytes().to_vec(),
        }
    }

    fn u64(tag: u16, value: u64) -> Self {
        Self {
            tag,
            kind: VALUE_U64,
            bytes: value.to_le_bytes().to_vec(),
        }
    }
}

fn metadata_record(kind: u16, mut fields: Vec<MetaField>) -> Vec<u8> {
    fields.sort_by_key(|field| field.tag);
    let mut payload = Vec::new();
    for field in &fields {
        payload.extend_from_slice(&field.tag.to_le_bytes());
        payload.extend_from_slice(&field.kind.to_le_bytes());
        payload.extend_from_slice(&(field.bytes.len() as u32).to_le_bytes());
        payload.extend_from_slice(&field.bytes);
        while payload.len() % 8 != 0 {
            payload.push(0);
        }
    }
    let record_size = META_HEADER_SIZE + payload.len();
    let mut output = Vec::with_capacity(record_size);
    output.extend_from_slice(META_MAGIC);
    output.extend_from_slice(&META_VERSION.to_le_bytes());
    output.extend_from_slice(&kind.to_le_bytes());
    output.extend_from_slice(&(META_HEADER_SIZE as u16).to_le_bytes());
    output.extend_from_slice(&(fields.len() as u16).to_le_bytes());
    output.extend_from_slice(&(record_size as u32).to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output.extend_from_slice(&crc32(&payload).to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output.extend_from_slice(&payload);
    output
}

fn metadata_blob(records: Vec<Vec<u8>>) -> Vec<u8> {
    let total = records.iter().map(Vec::len).sum();
    let mut output = Vec::with_capacity(total);
    for record in records {
        output.extend_from_slice(&record);
    }
    output
}

fn metadata_item(anchor: &Ident, suffix: &str, bytes: Vec<u8>) -> TokenStream2 {
    let suffix = sanitize_ident(suffix);
    let align_ident = format_ident!("__ElmMetaAlign_{}_{}", anchor, suffix);
    let static_ident = format_ident!("__ELM_META_{}_{}", anchor, suffix);
    let length = bytes.len();
    let values = bytes.iter();
    quote! {
        #[doc(hidden)]
        #[repr(C, align(8))]
        struct #align_ident([u8; #length]);

        #[doc(hidden)]
        #[used]
        #[allow(non_upper_case_globals)]
        #[unsafe(link_section = ".elm.meta")]
        static #static_ident: #align_ident = #align_ident([#(#values),*]);
    }
}

fn sanitize_ident(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

use syn::parse::Parser as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_negative_i32_attribute_values() {
        let args = MetaArgs::parse(quote!(priority = -2147483648)).unwrap();
        assert_eq!(args.i32_or("priority", 0).unwrap(), i32::MIN);
        assert!(args.finish().is_ok());
    }

    #[test]
    fn rejects_duplicate_mixin_stages() {
        assert!(MetaArgs::parse(quote!(stages(ingress, ingress))).is_err());
    }

    #[test]
    fn validates_final_mixin_point_length() {
        assert!(validate_point("short.ingress").is_ok());
        assert!(validate_point("this-point-name-is-too-long.ingress").is_err());
    }

    #[test]
    fn validates_import_slot_type_against_mode() {
        let managed: ItemStatic = syn::parse_quote! {
            static REMOTE: ::elm::ManagedImport = ::elm::ManagedImport::new();
        };
        let direct: ItemStatic = syn::parse_quote! {
            static REMOTE: ::elm::UnsafeDirectImport = ::elm::UnsafeDirectImport::new();
        };
        assert!(validate_import_slot(&managed, "managed").is_ok());
        assert!(validate_import_slot(&direct, "direct-pinned").is_ok());
        assert!(validate_import_slot(&managed, "direct-pinned").is_err());
    }

    #[test]
    fn validates_kernel_api_slot_initializer() {
        let valid: ItemStatic = syn::parse_quote! {
            static TIME: ::kernel_api::ApiImport<::kernel_api::time::TimeApiV1> =
                ::kernel_api::ApiImport::new("kernel.time", 1, 3);
        };
        assert!(validate_kernel_api_slot(&valid, "kernel.time", 1, 3).is_ok());

        let mismatched: ItemStatic = syn::parse_quote! {
            static TIME: ::kernel_api::ApiImport<::kernel_api::time::TimeApiV1> =
                ::kernel_api::ApiImport::new("kernel.random", 1, 3);
        };
        assert!(validate_kernel_api_slot(&mismatched, "kernel.time", 1, 3).is_err());

        let wrong_type: ItemStatic = syn::parse_quote! {
            static TIME: usize = 0;
        };
        assert!(validate_kernel_api_slot(&wrong_type, "kernel.time", 1, 3).is_err());
    }

    #[test]
    fn rejects_noncanonical_kernel_api_namespace() {
        let slot: ItemStatic = syn::parse_quote! {
            static TIME: ::kernel_api::ApiImport<::kernel_api::time::TimeApiV1> =
                ::kernel_api::ApiImport::new("kernel.time", 1, 3);
        };
        let trailing_dot = quote!(namespace = "kernel.", version = 1, capabilities = 3);
        assert!(kernel_api_impl(trailing_dot, slot.clone()).is_err());
        let empty_segment = quote!(namespace = "kernel..time", version = 1, capabilities = 3);
        assert!(kernel_api_impl(empty_segment, slot).is_err());
    }

    #[test]
    fn rejects_contract_with_empty_version_component() {
        assert!(validate_contract("test.contract@1.0").is_ok());
        assert!(validate_contract("test.contract@1..0").is_err());
        assert!(validate_contract("test.contract@.").is_err());
    }
}

#![warn(missing_docs)]

//! ELM Rust 开发属性宏。
//!
//! 本 crate 实现 ELM 模块源码到 EBI Rust ABI v1 的编译期适配。模块作者只编写安全
//! Rust 函数、静态导入槽和固定载荷结构体；宏负责生成稳定导出符号、原始 ABI
//! trampoline，以及供 `cargo elm` 消费的 `.elm.meta` 元数据。业务代码不应手写
//! `extern "C"`、`export_name`、`link_section` 或 EBI 元数据记录。
//!
//! # 共同函数约束
//!
//! [`macro@module`] 作用于 `ElmModule` Trait 实现，并可在该实现中收纳 provider、export、
//! device 与 mixin 方法。生命周期和激活入口只能使用 trait 方法；被注册的业务方法必须：
//!
//! - 是普通安全 Rust 函数，不能是 `unsafe fn`；
//! - 不能声明 `extern` ABI、`async`、`const`、可变参数或泛型参数；
//! - 独立函数不能包含 `self`；根模块内回调必须使用 `&self`，设备发现可使用 `&mut self`；
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
//! `.elm.meta` 只供构建工具读取，不得进入 EKI 的可装载段。`cargo elm` 会再次独立校验
//! 元数据、符号、契约、ELF 段和重定位，因此通过宏展开不等于镜像已经获得装载资格。
//!
//! # 完整示例
//!
//! 以下代码展示根模块、固定载荷和 provider 的组合。示例标记为 `ignore`，因为实际
//! ELM 工程还需要 `#![no_std]`、`#![no_main]`、专用链接脚本和 `cargo elm` 打包步骤。
//!
//! ```ignore
//! use elm::{ElmModule, HookError, HookResult, LifecycleContext, ProviderReply,
//!     ProviderRequest, ProviderResult};
//!
//! #[elm::payload("demo.request@1")]
//! struct Request {
//!     opcode: u32,
//! }
//!
//! struct Demo;
//!
//! #[elm::module]
//! impl ElmModule for Demo {
//!     fn create(_context: &LifecycleContext) -> Result<Self, HookError> { Ok(Self) }
//!     fn initialize(&mut self, _context: &LifecycleContext) -> HookResult { Ok(()) }
//!     fn finalize(&mut self, _context: &LifecycleContext) -> HookResult { Ok(()) }
//!
//!     #[elm::provider(contract = "demo.service@1")]
//!     fn service(&self, _request: &ProviderRequest) -> ProviderResult {
//!         Ok(ProviderReply::ok())
//!     }
//! }
//! ```

use std::collections::BTreeMap;

use proc_macro::TokenStream;
use proc_macro2::{Ident, Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::punctuated::Punctuated;
use syn::{
    Attribute, Expr, ExprLit, ExprUnary, Fields, FnArg, GenericArgument, ImplItem, ImplItemFn,
    Item, ItemFn, ItemImpl, ItemStatic, ItemStruct, Lit, LitStr, Meta, Pat, PathArguments,
    ReturnType, Signature, Token, Type, TypeBareFn, UnOp, parse_macro_input,
};

const META_MAGIC: &[u8; 8] = b"ELMMETA1";
const META_VERSION: u16 = 1;
const META_HEADER_SIZE: usize = 32;

const KIND_PROVIDER: u16 = 3;
const KIND_PROVIDER_SNAPSHOT: u16 = 4;
const KIND_EXPORT: u16 = 5;
const KIND_IMPORT: u16 = 6;
const KIND_EXTENSION_POINT: u16 = 7;
const KIND_EXTENSION: u16 = 8;
const KIND_PAYLOAD: u16 = 9;
const KIND_KERNEL_MIXIN: u16 = 10;
const KIND_MODULE: u16 = 18;

const VALUE_UTF8: u16 = 1;
const VALUE_U32: u16 = 2;
const VALUE_I32: u16 = 3;
// 元数据线格式已经分配 u64 类型编号，当前 attribute 尚未消费该宽度。
#[allow(dead_code)]
const VALUE_U64: u16 = 4;

const FIELD_SYMBOL: u16 = 1;
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
const FIELD_RUST_ABI: u16 = 27;

const IMPORT_OPTIONAL: u32 = 1 << 0;
const IMPORT_MANAGED: u32 = 1 << 1;
const IMPORT_DIRECT_PINNED: u32 = 1 << 2;
const IMPORT_ALLOW_ANCESTOR: u32 = 1 << 3;
const IMPORT_ALLOW_BUILTIN: u32 = 1 << 4;
const IMPORT_KERNEL_SYMBOL: u32 = 1 << 5;
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
/// 注册当前镜像唯一的 [`elm::ElmModule`] 实现。
///
/// attribute 必须标记 `impl elm::ElmModule for T`，不接受参数，也不允许泛型实现。宏会
/// 生成当前 generation 唯一的实例槽、统一模块描述符，以及动态和编译期内化构建使用的
/// 生命周期入口。模块作者不再为每个生命周期方法单独添加 attribute。
///
/// ```ignore
/// struct Demo;
///
/// #[elm::module]
/// impl elm::ElmModule for Demo {
///     fn create(_context: &elm::LifecycleContext) -> Result<Self, elm::HookError> {
///         Ok(Self)
///     }
///
///     fn initialize(&mut self, _context: &elm::LifecycleContext) -> elm::HookResult {
///         Ok(())
///     }
///
///     fn finalize(&mut self, _context: &elm::LifecycleContext) -> elm::HookResult {
///         Ok(())
///     }
/// }
/// ```
pub fn module(attr: TokenStream, item: TokenStream) -> TokenStream {
    let implementation = parse_macro_input!(item as ItemImpl);
    match module_impl(attr.into(), implementation) {
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
/// 声明一个供其他 ELM 调用的 export。
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
/// `name` 必须满足 EBI symbol 约束，`contract` 必须包含版本。`managed` 模式生成固定调用帧
/// trampoline；`direct-pinned` 模式导出真实 Rust 函数，构建链从函数指针类型生成规范签名并
/// 写入 SHA-256。运行时只有在名称、契约、版本和 ABI 摘要全部匹配时才写入调用方槽位，并在
/// 直接调用者存活期间固定 provider generation。
///
/// # 受管模式签名
///
/// ```text
/// fn(&elm::ManagedRequest) -> elm::ManagedResult
/// ```
///
/// trampoline 校验调用方、被调用方代际和载荷边界，再把 `ManagedResult` 写回固定回复帧。
/// 热替换时，受管调用按 generation 路由；实现不得把 `ManagedRequest` 中的借用保存到返回后。
/// `direct-pinned` 可以使用不含泛型、`impl Trait` 或显式 `extern` 的普通 Rust 函数签名；参数
/// 与返回值必须遵循同一 rustc、target spec、panic 策略和目标特性生成的 Rust ABI。跨边界
/// 借用的有效期不能超过一次调用，panic 必须通过 ELM 受保护终止出口收敛。
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
///
/// 直接固定导出保留普通 Rust 调用形式：
///
/// ```ignore
/// #[elm::export(
///     name = "demo.add",
///     contract = "demo.add@1",
///     version = 1,
///     mode = "direct-pinned",
///     visibility = "dependency"
/// )]
/// fn add(left: u64, right: u64) -> u64 {
///     left + right
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
/// `mode = "direct-pinned"` 时必须为 `elm::DirectImport<fn(...) -> ...>`。禁止 `static mut`，也禁止
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
/// `ManagedImport` 负责 call id、固定载荷编码和回复关联校验。`DirectImport<F>` 返回已经过
/// ABI 摘要校验的类型化 Rust 函数指针；调用方仍需在 `unsafe` 代码中证明业务参数、借用与
/// panic 约束。直接固定依赖会阻止 provider 卸载，并限制其热替换能力。
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
///
/// 直接固定导入必须显式携带函数指针类型：
///
/// ```ignore
/// #[elm::import(
///     name = "demo.add",
///     contract = "demo.add@1",
///     version = 1,
///     mode = "direct-pinned"
/// )]
/// static ADD: elm::DirectImport<fn(u64, u64) -> u64> = elm::DirectImport::new();
///
/// // Safety: 装载器已校验 Rust ABI 摘要；调用方仍负责业务参数与 panic 约束。
/// let add = unsafe { ADD.get() }.ok_or(elm::HookError::new(-2))?;
/// assert_eq!(add(20, 22), 42);
/// # Ok::<(), elm::HookError>(())
/// ```
pub fn import(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as ItemStatic);
    match import_impl(attr.into(), item) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
/// 声明一个由内核直接符号目录解析的 Rust 函数地址槽。
///
/// 该 attribute 只供 `cargo elm` 同步的子系统接口投影使用。它必须标记不可变
/// `static elm::DirectImport<fn(...) -> ...>`，并要求 `name`、`contract` 与非零 `version`。宏会
/// 生成内核符号 import 元数据和八字节地址槽；装载器在调用任何模块代码前按名称、契约和
/// 版本精确解析并写入地址。解析过程不经过 elm-mgr、provider、受管调用帧或 ELM export。
///
/// 投影 crate 应在普通安全函数中把槽地址转换为与目录声明完全一致的 Rust 函数指针，模块
/// 作者不应直接使用此 attribute。内核与 ELM 必须通过同一工具链、目标特性和 ABI 指纹校验；
/// 错误签名转换属于未定义行为，因此符号目录与投影必须由同一份接口定义生成。
///
/// 每个槽使用确定名称和独立 `.data.elm_imports.<slot>` 输入段，并通过 `#[used]` 保留到
/// 最终目标文件；这条实现不依赖 nightly-only 的 `linkage` 属性，普通默认 Rust 工具链
/// 也能编译接口投影。`.elm.meta` 可以保存完整接口目录，但 `cargo elm` 只把最终 ELF 中
/// 仍存在的槽投影为 EBI import，避免“依赖一个接口 crate 就获得整组符号”的权限扩张。
///
/// ```ignore
/// #[elm::kernel_symbol(
///     name = "sched.now_ns_public",
///     contract = "kernel.sched.now-ns@1",
///     version = 1,
///     abi = "fn()->u64"
/// )]
/// static NOW_NS: elm::DirectImport<fn() -> u64> = elm::DirectImport::new();
/// ```
pub fn kernel_symbol(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as ItemStatic);
    match kernel_symbol_impl(attr.into(), item) {
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
/// 声明一个 ELM 补缀处理器或内核符号级 Mixin 处理器集合。
///
/// 该 attribute 有两种互斥形态：
///
/// - 标记独立函数时，声明附着到其他 ELM 显式补缀点的线协议处理器；
/// - 标记固有 `impl` 时，声明附着到内核直接符号目录中真实 Rust 函数站点的处理器集合。
///
/// 两种形态共享名称只是为了表达“对既有行为进行组合”，其装载协议和热路径完全不同。
/// 独立函数形态经过 provider 与固定 payload；固有 `impl` 形态经过 EKI `KernelMixins` 表、
/// 精确源码站点身份和同步 Rust 调用帧，不经过 provider 或 `elm-mgr` 数据路径。
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
///
/// # 内核符号级形态
///
/// ```text
/// #[elm::mixin(target = "allocator")]
/// impl ModuleType {
///     #[elm::inject(method = "GlobalAlloc.alloc", at = "head", priority = 300)]
///     fn trace(&self, context: &mut elm::KernelMixinContext<'_>) -> elm::HookResult;
/// }
/// ```
///
/// `target` 是稳定 API 路径前缀。若 `method` 已经以该前缀开头则直接使用，否则宏以
/// `target + "." + method` 组成完整路径。固有 `impl` 必须属于模块类型、使用安全非泛型
/// 方法，并且每个处理器签名必须是：
///
/// ```text
/// fn(&self, &mut elm::KernelMixinContext<'_>) -> elm::HookResult
/// ```
///
/// 当前稳定方法 attribute：
///
/// - `#[elm::inject(method = "...", at = "head|return", priority = N)]`：观察或修改当前帧，
///   返回后自动继续；`at` 默认是 `head`；
/// - `#[elm::modify_arg(method = "...", priority = N)]`：在 `head` 站点修改允许写入的参数；
/// - `#[elm::modify_return(method = "...", priority = N)]`：在 `return` 站点读取或修改结果；
/// - `#[elm::overwrite(method = "...", priority = N)]`：在 `head` 站点取得 continuation，必须
///   调用 `context.proceed()` 或明确写入/取消结果。
///
/// `priority` 是可选 `i32`，默认 0，数值较大者先执行。宏会生成固定 C ABI trampoline，
/// 但该 ABI 是框架内部细节，开发者不得手写 `extern`、`export_name` 或同名符号。
///
/// `modify_local`、`redirect` 和 `wrap_operation` 已保留语法名称，但当前会以
/// `TODO(ELM-MIR)` 在编译期拒绝。内部调用、局部变量和字段访问必须由后续 MIR 级织入器
/// 在掌握类型、借用、临时值和控制流信息后生成，proc-macro 不提供不完整替代实现。
///
/// # 内核符号级示例
///
/// ```ignore
/// use core::alloc::Layout;
/// use elm::{HookError, HookResult, KernelMixinContext};
///
/// struct ModuleType;
///
/// #[elm::mixin(target = "allocator")]
/// impl ModuleType {
///     #[elm::inject(method = "GlobalAlloc.alloc", at = "head", priority = 300)]
///     fn trace(&self, context: &mut KernelMixinContext<'_>) -> HookResult {
///         let _layout = context.argument::<Layout>(1).ok_or(HookError::new(-1))?;
///         Ok(())
///     }
///
///     #[elm::overwrite(method = "GlobalAlloc.alloc", priority = 100)]
///     fn wrap(&self, context: &mut KernelMixinContext<'_>) -> HookResult {
///         context.proceed()
///     }
/// }
/// ```
pub fn mixin(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as Item);
    let result = match item {
        Item::Impl(implementation) => kernel_mixin_impl(attr.into(), implementation),
        Item::Fn(function) => mixin_impl(attr.into(), function),
        other => Err(syn::Error::new_spanned(
            other,
            "#[elm::mixin] 只能标记独立 Mixin impl",
        )),
    };
    match result {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[derive(Clone, Copy)]
enum ModuleMethodKind {
    Provider,
    ProviderSnapshot,
    Export,
    MixinPoint,
    Mixin,
}

struct ModuleMethodExpansion {
    inherent_methods: Vec<ImplItemFn>,
    generated: TokenStream2,
}

fn take_module_method_attribute(
    method: &mut ImplItemFn,
) -> syn::Result<Option<(ModuleMethodKind, TokenStream2)>> {
    let mut selected = None;
    let mut retained = Vec::with_capacity(method.attrs.len());
    for attribute in core::mem::take(&mut method.attrs) {
        let Some(name) = attribute
            .path()
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
        else {
            retained.push(attribute);
            continue;
        };
        if matches!(
            name.as_str(),
            "on_initialize"
                | "on_finalize"
                | "on_quiesce"
                | "on_pause"
                | "on_resume"
                | "on_migrate_export"
                | "on_migrate_import"
                | "on_migrate_abort"
                | "entry"
        ) {
            return Err(syn::Error::new_spanned(
                attribute,
                "生命周期和 entry 已由 ElmModule trait 方法定义，不得再添加 attribute",
            ));
        }
        let kind = match name.as_str() {
            "provider" => ModuleMethodKind::Provider,
            "provider_snapshot" => ModuleMethodKind::ProviderSnapshot,
            "export" => ModuleMethodKind::Export,
            "mixin_point" => ModuleMethodKind::MixinPoint,
            "mixin" => ModuleMethodKind::Mixin,
            _ => {
                retained.push(attribute);
                continue;
            }
        };
        if selected.is_some() {
            return Err(syn::Error::new_spanned(
                attribute,
                "同一个 ElmModule 方法只能声明一种 ELM 运行时角色",
            ));
        }
        let tokens = match attribute.meta {
            Meta::Path(_) => TokenStream2::new(),
            Meta::List(list) => list.tokens,
            Meta::NameValue(value) => {
                return Err(syn::Error::new_spanned(
                    value,
                    "ELM 方法 attribute 不接受 name-value 外层语法",
                ));
            }
        };
        selected = Some((kind, tokens));
    }
    method.attrs = retained;
    Ok(selected)
}

fn validate_module_callback_method(
    method: &ImplItemFn,
    argument_count: usize,
    allow_mutable_receiver: bool,
    require_output: bool,
) -> syn::Result<Vec<Ident>> {
    if method.sig.constness.is_some()
        || method.sig.asyncness.is_some()
        || method.sig.unsafety.is_some()
        || method.sig.abi.is_some()
        || method.sig.variadic.is_some()
        || !method.sig.generics.params.is_empty()
    {
        return Err(syn::Error::new_spanned(
            &method.sig,
            "ELM 模块回调必须是非泛型安全 Rust 方法，不能手写 extern ABI",
        ));
    }
    if method.sig.inputs.len() != argument_count + 1 {
        return Err(syn::Error::new_spanned(
            &method.sig.inputs,
            format!("该 ELM 模块回调要求一个 self 接收者和 {argument_count} 个普通参数"),
        ));
    }
    let Some(FnArg::Receiver(receiver)) = method.sig.inputs.first() else {
        return Err(syn::Error::new_spanned(
            &method.sig.inputs,
            "ElmModule 内的运行时回调必须使用 &self 接收者",
        ));
    };
    if receiver.reference.is_none()
        || receiver.colon_token.is_some()
        || (!allow_mutable_receiver && receiver.mutability.is_some())
    {
        let expected = if allow_mutable_receiver {
            "&self 或 &mut self"
        } else {
            "&self"
        };
        return Err(syn::Error::new_spanned(
            receiver,
            format!("该 ELM 模块回调的接收者必须是 {expected}"),
        ));
    }
    if require_output && matches!(method.sig.output, ReturnType::Default) {
        return Err(syn::Error::new_spanned(
            &method.sig,
            "ELM 模块回调必须显式返回对应的结果类型",
        ));
    }
    if method.attrs.iter().any(|attribute| {
        attribute
            .path()
            .segments
            .last()
            .is_some_and(|segment| matches!(segment.ident.to_string().as_str(), "cfg" | "cfg_attr"))
    }) {
        return Err(syn::Error::new_spanned(
            &method.sig,
            "已注册的 ElmModule 方法不得单独使用 cfg；镜像元数据必须在所有构建中保持闭合",
        ));
    }
    let mut arguments = Vec::with_capacity(argument_count);
    for input in method.sig.inputs.iter().skip(1) {
        let FnArg::Typed(argument) = input else {
            unreachable!();
        };
        let Pat::Ident(pattern) = argument.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &argument.pat,
                "ElmModule 回调参数必须使用简单标识符",
            ));
        };
        arguments.push(pattern.ident.clone());
    }
    Ok(arguments)
}

fn module_proxy_function(method: &ImplItemFn, body: TokenStream2) -> syn::Result<ItemFn> {
    let ident = &method.sig.ident;
    let inputs = method.sig.inputs.iter().skip(1);
    let output = &method.sig.output;
    syn::parse2(quote! {
        #[doc(hidden)]
        fn #ident(#(#inputs),*) #output {
            #body
        }
    })
}

fn module_result_proxy(method: &ImplItemFn, target: &Ident) -> syn::Result<ItemFn> {
    let arguments =
        validate_module_callback_method(method, method.sig.inputs.len() - 1, false, true)?;
    module_proxy_function(
        method,
        quote! {
            __ELM_MODULE_SLOT_V1.with_active(|module| module.#target(#(#arguments),*))?
        },
    )
}

fn export_mode(attr: &TokenStream2) -> syn::Result<String> {
    let args = MetaArgs::parse(attr.clone())?;
    args.string_or("mode", "managed")
}

fn expand_module_method(
    _module_ty: &Type,
    kind: ModuleMethodKind,
    attr: TokenStream2,
    mut method: ImplItemFn,
) -> syn::Result<ModuleMethodExpansion> {
    let ident = method.sig.ident.clone();
    let (mut inherent_methods, generated) = match kind {
        ModuleMethodKind::Provider => {
            validate_module_callback_method(&method, 1, false, true)?;
            let proxy = module_result_proxy(&method, &ident)?;
            (vec![method], provider_impl(attr, proxy)?)
        }
        ModuleMethodKind::ProviderSnapshot => {
            validate_module_callback_method(&method, 2, false, true)?;
            let proxy = module_result_proxy(&method, &ident)?;
            (vec![method], provider_snapshot_impl(attr, proxy)?)
        }
        ModuleMethodKind::Export => {
            let mode = export_mode(&attr)?;
            let proxy = if mode == "direct-pinned" {
                let arguments = validate_module_callback_method(
                    &method,
                    method.sig.inputs.len().saturating_sub(1),
                    false,
                    false,
                )?;
                module_proxy_function(
                    &method,
                    quote! {
                        match __ELM_MODULE_SLOT_V1
                            .with_active(|module| module.#ident(#(#arguments),*))
                        {
                            Ok(value) => value,
                            Err(_) => ::elm::runtime::abort_panic(),
                        }
                    },
                )?
            } else {
                validate_module_callback_method(&method, 1, false, true)?;
                module_result_proxy(&method, &ident)?
            };
            (vec![method], export_impl(attr, proxy)?)
        }
        ModuleMethodKind::Mixin => {
            let arguments = validate_module_callback_method(&method, 1, false, true)?;
            let proxy = module_proxy_function(
                &method,
                quote! {
                    __ELM_MODULE_SLOT_V1
                        .with_active(|module| module.#ident(#(#arguments),*))
                        .unwrap_or(::elm::MixinControl::Deny)
                },
            )?;
            (vec![method], mixin_impl(attr, proxy)?)
        }
        ModuleMethodKind::MixinPoint => {
            let arguments = validate_module_callback_method(&method, 1, false, true)?;
            let original_ident = format_ident!("__elm_module_original_{}", ident);
            let mut original = method.clone();
            original.sig.ident = original_ident.clone();
            method.block = syn::parse_quote!({
                #[cfg(feature = "elm-integrated")]
                let __elm_result = self.#original_ident(#(#arguments),*);
                #[cfg(not(feature = "elm-integrated"))]
                let __elm_result = #ident(#(#arguments),*);
                __elm_result
            });
            let proxy = module_result_proxy(&original, &original_ident).map(|mut proxy| {
                proxy.sig.ident = ident.clone();
                proxy
            })?;
            (vec![original, method], mixin_point_impl(attr, proxy)?)
        }
    };
    for method in &mut inherent_methods {
        method.attrs.push(syn::parse_quote!(#[cfg_attr(
            feature = "elm-integrated",
            allow(dead_code)
        )]));
    }
    Ok(ModuleMethodExpansion {
        inherent_methods,
        generated,
    })
}

fn module_impl(attr: TokenStream2, mut implementation: ItemImpl) -> syn::Result<TokenStream2> {
    require_empty_attr(attr)?;
    if implementation.unsafety.is_some() {
        return Err(syn::Error::new_spanned(
            &implementation.impl_token,
            "ElmModule 实现不能声明为 unsafe",
        ));
    }
    if !implementation.generics.params.is_empty() || implementation.generics.where_clause.is_some()
    {
        return Err(syn::Error::new_spanned(
            &implementation.generics,
            "#[elm::module] 不支持泛型模块实现",
        ));
    }
    let Some((negative, trait_path, _)) = &implementation.trait_ else {
        return Err(syn::Error::new_spanned(
            &implementation.self_ty,
            "#[elm::module] 必须标记 impl elm::ElmModule for T",
        ));
    };
    if negative.is_some()
        || trait_path
            .segments
            .last()
            .map(|segment| segment.ident != "ElmModule")
            .unwrap_or(true)
    {
        return Err(syn::Error::new_spanned(
            trait_path,
            "#[elm::module] 只能注册 ElmModule trait 实现",
        ));
    }
    for required in ["create", "initialize", "finalize"] {
        let found = implementation
            .items
            .iter()
            .any(|item| matches!(item, ImplItem::Fn(function) if function.sig.ident == required));
        if !found {
            return Err(syn::Error::new_spanned(
                &implementation.self_ty,
                format!("ElmModule 实现缺少必需方法 {required}"),
            ));
        }
    }

    let module_ty = implementation.self_ty.clone();
    let mut inherent_methods = Vec::new();
    let mut generated_methods = Vec::new();
    let mut trait_items = Vec::with_capacity(implementation.items.len());
    for item in core::mem::take(&mut implementation.items) {
        let ImplItem::Fn(mut method) = item else {
            trait_items.push(item);
            continue;
        };
        let Some((kind, method_attr)) = take_module_method_attribute(&mut method)? else {
            trait_items.push(ImplItem::Fn(method));
            continue;
        };
        let expansion = expand_module_method(&module_ty, kind, method_attr, method)?;
        inherent_methods.extend(expansion.inherent_methods);
        generated_methods.push(expansion.generated);
    }
    implementation.items = trait_items;
    let metadata_anchor = format_ident!("__elm_module");
    let integrated_profile_hash = integrated_profile_hash_tokens()?;
    let metadata = metadata_item(
        &metadata_anchor,
        "descriptor",
        metadata_record(
            KIND_MODULE,
            vec![MetaField::utf8(FIELD_SYMBOL, "__elm_module_descriptor_v1")],
        ),
    );

    Ok(quote! {
        #implementation

        impl #module_ty {
            #(#inherent_methods)*
        }

        #(#generated_methods)*

        #[doc(hidden)]
        static __ELM_MODULE_SLOT_V1: ::elm::ModuleSlot<#module_ty> = ::elm::ModuleSlot::new();

        #[doc(hidden)]
        #[cfg(not(feature = "elm-integrated"))]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn __elm_module_initialize_v1(
            context: *mut ::elm::ElmNativeHookContextV1,
        ) -> i32 {
            unsafe {
                ::elm::__private::module_initialize_trampoline(&__ELM_MODULE_SLOT_V1, context)
            }
        }

        #[doc(hidden)]
        #[cfg(not(feature = "elm-integrated"))]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn __elm_module_finalize_v1(
            context: *mut ::elm::ElmNativeHookContextV1,
        ) -> i32 {
            unsafe {
                ::elm::__private::module_finalize_trampoline(&__ELM_MODULE_SLOT_V1, context)
            }
        }

        #[doc(hidden)]
        #[cfg(not(feature = "elm-integrated"))]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn __elm_module_quiesce_v1(
            context: *mut ::elm::ElmNativeHookContextV1,
        ) -> i32 {
            unsafe {
                ::elm::__private::module_quiesce_trampoline(&__ELM_MODULE_SLOT_V1, context)
            }
        }

        #[doc(hidden)]
        #[cfg(not(feature = "elm-integrated"))]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn __elm_module_pause_v1(
            context: *mut ::elm::ElmNativeHookContextV1,
        ) -> i32 {
            unsafe {
                ::elm::__private::module_pause_trampoline(&__ELM_MODULE_SLOT_V1, context)
            }
        }

        #[doc(hidden)]
        #[cfg(not(feature = "elm-integrated"))]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn __elm_module_resume_v1(
            context: *mut ::elm::ElmNativeHookContextV1,
        ) -> i32 {
            unsafe {
                ::elm::__private::module_resume_trampoline(&__ELM_MODULE_SLOT_V1, context)
            }
        }

        #[doc(hidden)]
        #[cfg(not(feature = "elm-integrated"))]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn __elm_module_migrate_export_v1(
            context: *mut ::elm::ElmNativeMigrationContextV1,
        ) -> i32 {
            unsafe {
                ::elm::__private::module_migration_export_trampoline(
                    &__ELM_MODULE_SLOT_V1,
                    context,
                )
            }
        }

        #[doc(hidden)]
        #[cfg(not(feature = "elm-integrated"))]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn __elm_module_migrate_import_v1(
            context: *mut ::elm::ElmNativeMigrationContextV1,
        ) -> i32 {
            unsafe {
                ::elm::__private::module_migration_import_trampoline(
                    &__ELM_MODULE_SLOT_V1,
                    context,
                )
            }
        }

        #[doc(hidden)]
        #[cfg(not(feature = "elm-integrated"))]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn __elm_module_migrate_abort_v1(
            context: *mut ::elm::ElmNativeMigrationContextV1,
        ) -> i32 {
            unsafe {
                ::elm::__private::module_migration_abort_trampoline(
                    &__ELM_MODULE_SLOT_V1,
                    context,
                )
            }
        }

        #[doc(hidden)]
        #[cfg(not(feature = "elm-integrated"))]
        #[unsafe(export_name = "__elm_module_entry_v1")]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn __elm_module_entry_v1(
            context: *mut ::elm::ElmNativeEntryFrameV1,
        ) -> i32 {
            unsafe { ::elm::__private::module_entry_trampoline(&__ELM_MODULE_SLOT_V1, context) }
        }

        #[doc(hidden)]
        #[cfg(not(feature = "elm-integrated"))]
        #[used]
        #[unsafe(export_name = "__elm_module_descriptor_v1")]
        #[unsafe(link_section = ".rodata.elm.module")]
        pub static __ELM_MODULE_DESCRIPTOR_V1: ::elm::ElmModuleDescriptorV1 =
            ::elm::ElmModuleDescriptorV1::new::<#module_ty>(
                __elm_module_initialize_v1,
                __elm_module_finalize_v1,
                __elm_module_quiesce_v1,
                __elm_module_pause_v1,
                __elm_module_resume_v1,
                __elm_module_migrate_export_v1,
                __elm_module_migrate_import_v1,
                __elm_module_migrate_abort_v1,
                __elm_module_entry_v1,
            );

        #[cfg(not(feature = "elm-integrated"))]
        #metadata

        #[doc(hidden)]
        #[cfg(feature = "elm-integrated")]
        fn __elm_integrated_initialize_v1() -> i32 {
            let context = ::elm::LifecycleContext::integrated(1);
            match __ELM_MODULE_SLOT_V1.initialize(&context) {
                Ok(()) => 0,
                Err(error) => error.status(),
            }
        }

        #[doc(hidden)]
        #[cfg(feature = "elm-integrated")]
        fn __elm_integrated_finalize_v1() -> i32 {
            let context = ::elm::LifecycleContext::integrated(2);
            match __ELM_MODULE_SLOT_V1.finalize(&context) {
                Ok(()) => 0,
                Err(error) => error.status(),
            }
        }

        #[doc(hidden)]
        #[cfg(feature = "elm-integrated")]
        #[used]
        #[unsafe(link_section = ".kernel.integrated_components")]
        static __ELM_INTEGRATED_COMPONENT_V1: ::elm::KernelIntegratedComponentV1 =
            ::elm::KernelIntegratedComponentV1::new(
                __elm_integrated_initialize_v1,
                __elm_integrated_finalize_v1,
                #integrated_profile_hash,
                {
                    #[cfg(elm_integrated_phase = "device")]
                    {
                        ::elm::KERNEL_INTEGRATED_PHASE_DEVICE
                    }
                    #[cfg(not(elm_integrated_phase = "device"))]
                    {
                        ::elm::KERNEL_INTEGRATED_PHASE_RUNTIME
                    }
                },
            );
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
        #[cfg(not(feature = "elm-integrated"))]
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
        #[cfg(not(feature = "elm-integrated"))]
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

fn export_impl(attr: TokenStream2, mut function: ItemFn) -> syn::Result<TokenStream2> {
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
    let (mut flags, rust_abi) = match mode.as_str() {
        "managed" => {
            validate_function(&function, 1)?;
            (EXPORT_MANAGED, None)
        }
        "direct-pinned" => (
            EXPORT_DIRECT_PINNED,
            Some(canonical_function_abi(&function.sig)?),
        ),
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
    let mut fields = vec![
        MetaField::utf8(FIELD_SYMBOL, &name),
        MetaField::utf8(FIELD_NAME, &name),
        MetaField::utf8(FIELD_CONTRACT, &contract),
        MetaField::u32(FIELD_VERSION, version),
        MetaField::u32(FIELD_FLAGS, flags),
    ];
    if let Some(rust_abi) = &rust_abi {
        fields.push(MetaField::utf8(FIELD_RUST_ABI, rust_abi));
    }
    let metadata = metadata_item(ident, "export", metadata_record(KIND_EXPORT, fields));
    if rust_abi.is_some() {
        function.attrs.push(syn::parse_quote!(#[cfg_attr(
            not(feature = "elm-integrated"),
            unsafe(export_name = #name)
        )]));
        function.attrs.push(syn::parse_quote!(#[cfg_attr(
            not(feature = "elm-integrated"),
            unsafe(link_section = ".text.elm.abi")
        )]));
        Ok(quote! {
            #function
            #metadata
        })
    } else {
        let abi_ident = format_ident!("__elm_abi_export_{}", ident);
        Ok(quote! {
            #function

            #[doc(hidden)]
            #[cfg(not(feature = "elm-integrated"))]
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
    let rust_abi = validate_import_slot(&item, &mode)?;
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
    let mut fields = vec![
        MetaField::utf8(FIELD_SYMBOL, &symbol),
        MetaField::utf8(FIELD_NAME, &name),
        MetaField::utf8(FIELD_CONTRACT, &contract),
        MetaField::u32(FIELD_MIN_VERSION, min_version),
        MetaField::u32(FIELD_MAX_VERSION, max_version),
        MetaField::u32(FIELD_FLAGS, flags),
    ];
    if let Some(rust_abi) = rust_abi {
        fields.push(MetaField::utf8(FIELD_RUST_ABI, &rust_abi));
    }
    let metadata = metadata_item(&ident, "import", metadata_record(KIND_IMPORT, fields));
    Ok(quote! {
        #item
        #metadata
    })
}

fn kernel_symbol_impl(attr: TokenStream2, mut item: ItemStatic) -> syn::Result<TokenStream2> {
    let args = MetaArgs::parse(attr)?;
    let name = args.required_string("name")?;
    let contract = args.required_string("contract")?;
    let version = args.required_u32("version")?;
    let declared_rust_abi = args.required_string("abi")?;
    args.finish()?;
    validate_symbol(&name, "kernel symbol name")?;
    validate_contract(&contract)?;
    if version == 0 {
        return Err(syn::Error::new(
            Span::call_site(),
            "kernel symbol version 必须大于 0",
        ));
    }
    let rust_abi = validate_import_slot(&item, "direct-pinned")?
        .ok_or_else(|| syn::Error::new_spanned(&item.ty, "内核直接符号缺少 Rust ABI 类型"))?;
    if declared_rust_abi != rust_abi {
        return Err(syn::Error::new_spanned(
            &item.ty,
            format!("内核直接符号 ABI 声明不匹配：声明 {declared_rust_abi}，类型为 {rust_abi}"),
        ));
    }
    let ident = item.ident.clone();
    let symbol = format!(
        "__elm_kernel_symbol_{}",
        ident.to_string().to_ascii_lowercase()
    );
    let section = format!(
        ".data.elm_imports.{}",
        ident.to_string().to_ascii_lowercase()
    );
    validate_symbol(&symbol, "kernel symbol slot")?;
    // `linkage = "internal"` is nightly-only and makes ordinary Cargo users unable to
    // compile a bridge crate with the default toolchain.  The ELM import section and
    // metadata already provide the loader identity; a retained, exported zero slot is
    // sufficient for the loader to patch before activation.
    item.attrs.push(syn::parse_quote!(#[used]));
    item.attrs
        .push(syn::parse_quote!(#[unsafe(export_name = #symbol)]));
    item.attrs
        .push(syn::parse_quote!(#[unsafe(link_section = #section)]));
    let metadata = metadata_item(
        &ident,
        "kernel_symbol",
        metadata_record(
            KIND_IMPORT,
            vec![
                MetaField::utf8(FIELD_SYMBOL, &symbol),
                MetaField::utf8(FIELD_NAME, &name),
                MetaField::utf8(FIELD_CONTRACT, &contract),
                MetaField::u32(FIELD_MIN_VERSION, version),
                MetaField::u32(FIELD_MAX_VERSION, version),
                MetaField::u32(FIELD_FLAGS, IMPORT_KERNEL_SYMBOL),
                MetaField::utf8(FIELD_RUST_ABI, &declared_rust_abi),
            ],
        ),
    );
    Ok(quote! {
        #item
        #metadata
    })
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
        #[cfg(not(feature = "elm-integrated"))]
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

        #(#wrapper_attrs)*
        #[cfg(feature = "elm-integrated")]
        #visibility #signature {
            #original_ident(#argument)
        }

        #metadata
    })
}

#[derive(Clone, Copy)]
enum KernelMixinMethodKind {
    Inject,
    ModifyArgument,
    ModifyReturn,
    ModifyLocal,
    Redirect,
    WrapOperation,
    Overwrite,
}

impl KernelMixinMethodKind {
    const fn raw(self) -> u32 {
        match self {
            Self::Inject => 1,
            Self::ModifyArgument => 2,
            Self::ModifyReturn => 3,
            Self::ModifyLocal => 4,
            Self::Redirect => 5,
            Self::WrapOperation => 6,
            Self::Overwrite => 7,
        }
    }

    const fn flags(self) -> u32 {
        match self {
            Self::Inject | Self::ModifyArgument | Self::ModifyReturn | Self::ModifyLocal => 1,
            Self::Redirect | Self::WrapOperation | Self::Overwrite => 2,
        }
    }

    const fn default_selector(self) -> Option<&'static str> {
        match self {
            Self::Inject | Self::ModifyArgument | Self::Overwrite => Some("head"),
            Self::ModifyReturn => Some("return"),
            Self::ModifyLocal | Self::Redirect | Self::WrapOperation => None,
        }
    }
}

fn kernel_mixin_impl(
    attr: TokenStream2,
    mut implementation: ItemImpl,
) -> syn::Result<TokenStream2> {
    if implementation.trait_.is_some()
        || implementation.unsafety.is_some()
        || !implementation.generics.params.is_empty()
        || implementation.generics.where_clause.is_some()
    {
        return Err(syn::Error::new_spanned(
            &implementation,
            "#[elm::mixin] 必须标记模块类型的非泛型安全固有 impl",
        ));
    }
    let args = MetaArgs::parse(attr)?;
    let target = args.required_string("target")?;
    args.finish()?;
    validate_symbol(&target, "内核 Mixin target")?;

    let module_ty = implementation.self_ty.clone();
    let mut generated = Vec::new();
    for item in &mut implementation.items {
        let ImplItem::Fn(method) = item else {
            continue;
        };
        let Some((kind, method_attr)) = take_kernel_mixin_method_attribute(&mut method.attrs)?
        else {
            continue;
        };
        // TODO(ELM-MIR)：内部调用、局部变量和字段站点必须由带类型信息的 MIR pass 生成；
        // proc-macro 改写会破坏自动借用、临时值延寿、闭包类型和强制类型转换。
        if matches!(
            kind,
            KernelMixinMethodKind::ModifyLocal
                | KernelMixinMethodKind::Redirect
                | KernelMixinMethodKind::WrapOperation
        ) {
            return Err(syn::Error::new_spanned(
                &method.sig,
                "TODO(ELM-MIR)：modify_local、redirect 和 wrap_operation 等待 MIR 级站点织入器",
            ));
        }
        validate_kernel_mixin_method(method)?;
        let method_args = MetaArgs::parse(method_attr)?;
        let method_path = method_args.required_string("method")?;
        let selector = match kind.default_selector() {
            Some(default) => method_args.string_or("at", default)?,
            None => method_args.required_string("at")?,
        };
        let priority = method_args.i32_or("priority", 0)?;
        method_args.finish()?;
        let target_api = if method_path == target || method_path.starts_with(&format!("{target}."))
        {
            method_path
        } else {
            format!("{target}.{method_path}")
        };
        validate_symbol(&target_api, "内核 Mixin method")?;
        validate_kernel_mixin_selector(&selector)?;

        let ident = method.sig.ident.clone();
        let trampoline = format_ident!("__elm_kernel_mixin_{}", ident);
        let symbol = trampoline.to_string();
        validate_symbol(&symbol, "内核 Mixin handler symbol")?;
        let metadata = metadata_item(
            &ident,
            "kernel_mixin",
            metadata_record(
                KIND_KERNEL_MIXIN,
                vec![
                    MetaField::utf8(FIELD_SYMBOL, &symbol),
                    MetaField::utf8(FIELD_NAME, &target_api),
                    MetaField::u32(FIELD_FLAGS, kind.flags()),
                    MetaField::u32(FIELD_MODE, kind.raw()),
                    MetaField::utf8(FIELD_POINT, &selector),
                    MetaField::i32(FIELD_PRIORITY, priority),
                    MetaField::utf8(
                        FIELD_RUST_ABI,
                        "unsafeextern\"C\"fn(*mutkernel_symbols::KernelMixinFrameV1)->i32",
                    ),
                ],
            ),
        );
        generated.push(quote! {
            #[doc(hidden)]
            #[cfg(not(feature = "elm-integrated"))]
            #[unsafe(export_name = #symbol)]
            #[unsafe(link_section = ".text.elm.abi")]
            pub unsafe extern "C" fn #trampoline(
                frame: *mut ::elm::KernelMixinFrameV1,
            ) -> i32 {
                unsafe {
                    ::elm::__private::kernel_mixin_trampoline(
                        &__ELM_MODULE_SLOT_V1,
                        frame,
                        <#module_ty>::#ident,
                    )
                }
            }

            #metadata
        });
    }
    if generated.is_empty() {
        return Err(syn::Error::new_spanned(
            &implementation,
            "#[elm::mixin] impl 中没有 inject/modify/redirect/wrap/overwrite 方法",
        ));
    }
    Ok(quote! {
        #implementation
        #(#generated)*
    })
}

fn take_kernel_mixin_method_attribute(
    attributes: &mut Vec<Attribute>,
) -> syn::Result<Option<(KernelMixinMethodKind, TokenStream2)>> {
    let mut selected = None;
    let mut retained = Vec::with_capacity(attributes.len());
    for attribute in core::mem::take(attributes) {
        let kind = match attribute
            .path()
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
            .as_deref()
        {
            Some("inject") => Some(KernelMixinMethodKind::Inject),
            Some("modify_arg") => Some(KernelMixinMethodKind::ModifyArgument),
            Some("modify_return") => Some(KernelMixinMethodKind::ModifyReturn),
            Some("modify_local") => Some(KernelMixinMethodKind::ModifyLocal),
            Some("redirect") => Some(KernelMixinMethodKind::Redirect),
            Some("wrap_operation") => Some(KernelMixinMethodKind::WrapOperation),
            Some("overwrite") => Some(KernelMixinMethodKind::Overwrite),
            _ => None,
        };
        let Some(kind) = kind else {
            retained.push(attribute);
            continue;
        };
        if selected.is_some() {
            return Err(syn::Error::new_spanned(
                attribute,
                "同一个方法只能声明一种内核 Mixin 行为",
            ));
        }
        let Meta::List(list) = attribute.meta else {
            return Err(syn::Error::new_spanned(
                attribute,
                "内核 Mixin 方法必须提供 method/at 等参数",
            ));
        };
        selected = Some((kind, list.tokens));
    }
    *attributes = retained;
    Ok(selected)
}

fn validate_kernel_mixin_method(method: &ImplItemFn) -> syn::Result<()> {
    if method.sig.constness.is_some()
        || method.sig.asyncness.is_some()
        || method.sig.unsafety.is_some()
        || method.sig.abi.is_some()
        || method.sig.variadic.is_some()
        || !method.sig.generics.params.is_empty()
        || method.sig.generics.where_clause.is_some()
        || method.sig.inputs.len() != 2
        || matches!(method.sig.output, ReturnType::Default)
    {
        return Err(syn::Error::new_spanned(
            &method.sig,
            "内核 Mixin 方法必须是 fn(&self, &mut elm::KernelMixinContext<'_>) -> elm::HookResult",
        ));
    }
    let Some(FnArg::Receiver(receiver)) = method.sig.inputs.first() else {
        return Err(syn::Error::new_spanned(
            &method.sig.inputs,
            "内核 Mixin 方法缺少 &self 接收者",
        ));
    };
    if receiver.reference.is_none()
        || receiver.mutability.is_some()
        || receiver.colon_token.is_some()
    {
        return Err(syn::Error::new_spanned(
            receiver,
            "内核 Mixin 方法接收者必须是 &self",
        ));
    }
    let Some(FnArg::Typed(context)) = method.sig.inputs.iter().nth(1) else {
        unreachable!();
    };
    if !matches!(context.ty.as_ref(), Type::Reference(reference) if reference.mutability.is_some())
    {
        return Err(syn::Error::new_spanned(
            &context.ty,
            "内核 Mixin 上下文参数必须是可变借用",
        ));
    }
    Ok(())
}

fn validate_kernel_mixin_selector(selector: &str) -> syn::Result<()> {
    if selector.is_empty()
        || selector.len() > EBI_SYMBOL_NAME_LEN
        || selector.as_bytes().contains(&0)
        || !selector.is_ascii()
    {
        return Err(syn::Error::new(
            Span::call_site(),
            "内核 Mixin at selector 必须是长度不超过 128 的非空 ASCII 字符串",
        ));
    }
    Ok(())
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
        #[cfg(not(feature = "elm-integrated"))]
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

fn validate_import_slot(item: &ItemStatic, mode: &str) -> syn::Result<Option<String>> {
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
    let Type::Path(path) = item.ty.as_ref() else {
        return Err(syn::Error::new_spanned(
            &item.ty,
            "ELM import 槽必须使用框架定义的 import 类型",
        ));
    };
    let Some(segment) = path.path.segments.last() else {
        return Err(syn::Error::new_spanned(&item.ty, "ELM import 槽类型无效"));
    };
    if path.qself.is_some() {
        return Err(syn::Error::new_spanned(
            &item.ty,
            "ELM import 槽不能使用限定类型",
        ));
    }
    match mode {
        "managed"
            if segment.ident == "ManagedImport"
                && matches!(segment.arguments, PathArguments::None) =>
        {
            Ok(None)
        }
        "direct-pinned" if segment.ident == "DirectImport" => {
            let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
                return Err(syn::Error::new_spanned(
                    &item.ty,
                    "direct-pinned import 槽必须是 DirectImport<fn(...) -> ...>",
                ));
            };
            if arguments.args.len() != 1 {
                return Err(syn::Error::new_spanned(
                    &item.ty,
                    "DirectImport 必须且只能携带一个 Rust 函数指针类型",
                ));
            }
            let Some(GenericArgument::Type(Type::BareFn(function))) = arguments.args.first() else {
                return Err(syn::Error::new_spanned(
                    &item.ty,
                    "DirectImport 的类型参数必须是 Rust 函数指针",
                ));
            };
            canonical_bare_fn_abi(function).map(Some)
        }
        "managed" => Err(syn::Error::new_spanned(
            &item.ty,
            "managed import 槽类型必须是 ManagedImport",
        )),
        "direct-pinned" => Err(syn::Error::new_spanned(
            &item.ty,
            "direct-pinned import 槽类型必须是 DirectImport<fn(...) -> ...>",
        )),
        _ => Err(syn::Error::new(Span::call_site(), "未知 import mode")),
    }
}

fn canonical_bare_fn_abi(function: &TypeBareFn) -> syn::Result<String> {
    if function.abi.is_some() || function.variadic.is_some() {
        return Err(syn::Error::new_spanned(
            function,
            "直接固定 ABI 只接受非可变参数的 Rust 函数指针",
        ));
    }
    let lifetimes = &function.lifetimes;
    let unsafety = &function.unsafety;
    let arguments = function.inputs.iter().map(|argument| &argument.ty);
    let result: Type = match &function.output {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, result) => (**result).clone(),
    };
    Ok(normalize_abi_tokens(
        quote!(#lifetimes #unsafety fn(#(#arguments),*) -> #result),
    ))
}

fn canonical_function_abi(signature: &Signature) -> syn::Result<String> {
    if signature.constness.is_some()
        || signature.asyncness.is_some()
        || signature.abi.is_some()
        || signature.variadic.is_some()
        || !signature.generics.params.is_empty()
        || signature.generics.where_clause.is_some()
    {
        return Err(syn::Error::new_spanned(
            signature,
            "直接固定导出必须是非泛型、非 async、非 const 的 Rust 函数",
        ));
    }
    let mut arguments = Vec::with_capacity(signature.inputs.len());
    for argument in &signature.inputs {
        let FnArg::Typed(argument) = argument else {
            return Err(syn::Error::new_spanned(
                argument,
                "直接固定导出不能直接暴露 self 接收者",
            ));
        };
        arguments.push(argument.ty.as_ref());
    }
    let unsafety = &signature.unsafety;
    let result: Type = match &signature.output {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, result) => (**result).clone(),
    };
    Ok(normalize_abi_tokens(
        quote!(#unsafety fn(#(#arguments),*) -> #result),
    ))
}

fn normalize_abi_tokens(tokens: TokenStream2) -> String {
    tokens
        .to_string()
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect()
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
    #[allow(dead_code)]
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

    #[allow(dead_code)]
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
        // macro_rules 插值会用透明分组保留捕获片段的语法类别；该分组不改变字面量语义。
        Expr::Group(value) => parse_meta_value(*value.expr),
        Expr::Paren(value) => parse_meta_value(*value.expr),
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

    #[allow(dead_code)]
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
        #[cfg(not(feature = "elm-integrated"))]
        #[repr(C, align(8))]
        struct #align_ident([u8; #length]);

        #[doc(hidden)]
        #[cfg(not(feature = "elm-integrated"))]
        #[used]
        #[allow(non_upper_case_globals)]
        #[unsafe(link_section = ".elm.meta")]
        static #static_ident: #align_ident = #align_ident([#(#values),*]);
    }
}

fn integrated_profile_hash_tokens() -> syn::Result<TokenStream2> {
    let Some(value) = std::env::var_os("ELM_KERNEL_PROFILE_HASH") else {
        return Ok(quote!([0u8; 32]));
    };
    let value = value
        .into_string()
        .map_err(|_| syn::Error::new(Span::call_site(), "ELM_KERNEL_PROFILE_HASH 不是 UTF-8"))?;
    if value.len() != 64 {
        return Err(syn::Error::new(
            Span::call_site(),
            "ELM_KERNEL_PROFILE_HASH 必须包含 64 个十六进制字符",
        ));
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(|_| {
            syn::Error::new(
                Span::call_site(),
                "ELM_KERNEL_PROFILE_HASH 包含非十六进制字符",
            )
        })?;
    }
    let values = bytes.iter();
    Ok(quote!([#(#values),*]))
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
            static REMOTE: ::elm::DirectImport<fn(u64) -> u64> = ::elm::DirectImport::new();
        };
        assert!(validate_import_slot(&managed, "managed").is_ok());
        assert!(validate_import_slot(&direct, "direct-pinned").is_ok());
        assert!(validate_import_slot(&managed, "direct-pinned").is_err());
    }

    #[test]
    fn rejects_contract_with_empty_version_component() {
        assert!(validate_contract("test.contract@1.0").is_ok());
        assert!(validate_contract("test.contract@1..0").is_err());
        assert!(validate_contract("test.contract@.").is_err());
    }

    #[test]
    fn integrated_build_excludes_complete_metadata_item() {
        let tokens = metadata_item(
            &format_ident!("sample"),
            "descriptor",
            metadata_record(KIND_MODULE, Vec::new()),
        )
        .to_string();

        assert_eq!(tokens.matches("elm-integrated").count(), 2);
        assert!(tokens.contains(".elm.meta"));
    }

    #[test]
    fn integrated_mixin_point_calls_original_method_without_runtime_dispatch() {
        let implementation: ItemImpl = syn::parse_quote! {
            impl ::elm::ElmModule for Module {
                fn create(
                    _context: &::elm::LifecycleContext,
                ) -> Result<Self, ::elm::HookError> {
                    Ok(Self)
                }

                fn initialize(
                    &mut self,
                    _context: &::elm::LifecycleContext,
                ) -> ::elm::HookResult {
                    Ok(())
                }

                fn finalize(
                    &mut self,
                    _context: &::elm::LifecycleContext,
                ) -> ::elm::HookResult {
                    Ok(())
                }

                #[elm::mixin_point(
                    name = "demo.point",
                    contract = "demo.frame@1",
                    stages(ingress, observe)
                )]
                fn select(&self, frame: &mut Frame) -> ::elm::PointResult {
                    frame.value += 1;
                    Ok(())
                }
            }
        };
        let tokens = module_impl(TokenStream2::new(), implementation)
            .unwrap()
            .to_string();

        assert!(tokens.contains("self . __elm_module_original_select (frame)"));
        assert!(tokens.contains("cfg (feature = \"elm-integrated\")"));
        assert!(tokens.contains("cfg (not (feature = \"elm-integrated\"))"));
        assert!(tokens.contains(":: elm :: run_mixin_point"));
    }
}

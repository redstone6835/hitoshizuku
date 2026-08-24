#![no_std]
#![warn(missing_docs)]

//! ELM（Extensible Loadable Module，可拓展可加载单元）模型、协议与 Rust 开发框架。
//!
//! ELM 不是 Linux LKM 或 FreeBSD KLD 的兼容层。它把可加载代码建模为具有父子关系、
//! 依赖、受管导入/导出、能力端口、生命周期、资源所有权、热替换和可观测状态的运行时
//! 单元。内核只识别实现 EBI（ELM Binary Interface）协议的投影结果，不把 EKI、SOYO、
//! ELF 或其他容器格式写死为唯一输入。
//!
//! 本 crate 同时承担三类职责：
//!
//! - 定义架构无关、内核无关的 EBI、ELM API 和管理协议固定布局；
//! - 为外部 Rust ELM 提供安全上下文、固定载荷、受管导入、运行时 API 和 attribute；
//! - 为内核侧 ELM 运行时提供状态机、图、租约、策略、证明、快照和 provider 模型。
//!
//! 为保持依赖方向，本 crate 不能依赖 `kernel`、`general` 或 `arch`。具体子系统能力由各
//! 子系统 crate 定义并通过 provider/集成层接入；ELM 框架本身只提供稳定协议和运行时机制。
//!
//! # 功能开关
//!
//! - `module`：外部 ELM 的最小编译面，包含稳定 ABI 类型和安全开发包装；
//! - `macros`：重导出根模块、provider、import/export、payload 和 mixin attribute；
//! - `management`：仅供受授权 Manager 类型 ELM 使用的 `management::Client`；
//! - `runtime-model`：内核侧运行时模型，默认启用，不应成为普通外部 ELM 的依赖。
//! - `elm-integrated`：由 `cargo elm` 在 `mode = "y"` 时内部启用，模块作者不应手工选择。
//!
//! 普通外部工程通常使用 `default-features = false, features = ["module", "macros"]`；
//! Manager ELM 再增加 `management`。启用 feature 只让代码可编译，实际管理权限仍由内核
//! 按 ELM kind、镜像信任、当前代际、运行状态和 per-cell policy 在每次调用时重新鉴权。
//!
//! # 最小 ELM
//!
//! 每个 ELM 镜像必须注册一个 [`ElmModule`] 实现。根 attribute 生成唯一实例槽、统一描述符、
//! ABI trampoline 和 `.elm.meta`；模块作者不应手写 `extern "C"`。
//!
//! ```no_run
//! use elm::{ElmModule, HookError, HookResult, LifecycleContext};
//!
//! struct Demo;
//!
//! #[elm::module]
//! impl ElmModule for Demo {
//!     fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
//!         Ok(Self)
//!     }
//!
//!     fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
//!         elm::runtime::log(6, "demo.hello: initialized")
//!             .map_err(|_| HookError::new(-1))
//!     }
//!
//!     fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
//!         elm::runtime::log(6, "demo.hello: finalized")
//!             .map_err(|_| HookError::new(-1))
//!     }
//! }
//! ```
//!
//! 实际模块工程还需要 `#![no_std]`、`#![no_main]`、目标架构链接脚本、panic handler 和
//! `cargo elm` 打包步骤。panic handler 应调用 [`runtime::abort_panic`]，使运行时记录故障并
//! 走受保护的终止出口，而不是在模块代码中无限展开或越过 ABI 边界。
//!
//! # 开发 API 分层
//!
//! - [`runtime`]：所有普通 ELM 都可使用的当前上下文、日志和主动中止接口；
//! - `management`：Manager ELM 的类型化控制、查询、装载、热替换和策略接口；
//! - [`ElmPayload`]：跨单元固定载荷的编码协议，通常由 [`payload`] 生成；
//! - [`ManagedImport`]：推荐的受管 import 槽，提供代际路由和回复校验；
//! - [`ProviderRequest`]、[`ManagedRequest`] 和 [`SnapshotRequest`]：宏 trampoline 已校验后
//!   交给业务代码的安全请求视图；
//! - [`LifecycleContext`]、[`MigrationContext`] 和 [`EntryContext`]：生命周期只读上下文；
//! - [`MixinControl`] 与 [`mixin_point`]：可授权、可排序、可观测的分阶段补缀机制。
//!
//! 固定布局、状态码和线格式类型主要面向框架、打包器和内核实现。普通模块应优先使用上述
//! 安全包装，不要直接调用 [`ElmRuntimeApiV1`] 中的函数指针或自行构造原生 ABI frame。
//!
//! # ABI 与安全边界
//!
//! ELM API v1 使用显式版本、结构尺寸、保留字段、有效标志掩码和固定宽度整数。跨镜像边界
//! 不传递 Rust 引用、trait object、`Vec`、`String`、`usize` 或未固定布局的枚举。所有指针
//! 和借用只在对应调用期间有效；热替换后的旧 generation 句柄必须由运行时拒绝。
//!
//! [`payload`] 生成的载荷采用紧密排列的小端线格式，不依赖 Rust 内存布局。原生 ABI
//! trampoline 会在进入业务函数前检查版本、保留字段、长度、关联 id 和缓冲区边界；镜像
//! 装载器还会验证证明链、ABI 指纹、段权限、重定位、导入导出和元数据一致性。
//!
//! 未发布的旧管理路径不会进入 v1 公开面：
//!
//! ```compile_fail
//! use elm::mgr;
//! ```
//!
//! ```compile_fail
//! use elm::elmmgr;
//! ```
//!
//! ```compile_fail
//! use elm::developer;
//! ```

#[cfg(feature = "runtime-model")]
extern crate alloc;

pub mod context;
#[cfg(any(feature = "runtime-model", feature = "management"))]
pub mod ctl;
mod developer;
#[cfg(feature = "runtime-model")]
pub mod ebi;
#[cfg(any(feature = "runtime-model", feature = "management"))]
mod ebi_wire;
#[cfg(feature = "runtime-model")]
pub mod eki;
pub mod elmapi;
pub mod error;
#[cfg(any(feature = "runtime-model", feature = "management"))]
pub mod event;
pub mod frame;
#[cfg(feature = "runtime-model")]
pub mod graph;
pub mod ids;
pub mod kernel_mixin;
pub mod kind;
#[cfg(feature = "runtime-model")]
pub mod lease;
#[cfg(feature = "management")]
pub mod management;
#[cfg(feature = "runtime-model")]
pub mod manifest;
#[cfg(any(feature = "runtime-model", feature = "management"))]
pub mod menu;
#[cfg(feature = "runtime-model")]
pub mod metadata;
#[cfg(any(feature = "runtime-model", feature = "management"))]
mod mgr;
pub(crate) mod module_wire;
pub mod native;
#[cfg(feature = "runtime-model")]
pub mod nexus;
#[cfg(feature = "runtime-model")]
pub mod policy;
#[cfg(feature = "runtime-model")]
pub mod ports;
#[cfg(feature = "runtime-model")]
pub mod proof;
#[cfg(feature = "runtime-model")]
pub mod provider;
#[cfg(any(feature = "runtime-model", feature = "management"))]
pub mod resource;
pub mod runtime;
#[cfg(feature = "runtime-model")]
pub mod snapshot;
pub mod state;
#[cfg(feature = "runtime-model")]
pub mod topology;
pub mod wire;

#[cfg(not(any(feature = "runtime-model", feature = "module")))]
compile_error!("elm crate 必须启用 runtime-model 或 module 编译面");

#[cfg(feature = "macros")]
pub use elm_macros::{
    export, import, kernel_symbol, mixin, mixin_point, module, payload, provider, provider_snapshot,
};

pub use context::{
    ELM_CONTEXT_MAX_CPUS, ELM_CONTEXT_MAX_DEPTH, ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION,
    ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION, ElmContext, ElmCurrentContext,
    ElmCurrentContextGuard, ElmCurrentContextOps, ElmLifecyclePhase, ElmNativeHookContextV1,
    ElmNativeMigrationContextV1, current_cell, current_context, enter_current_context,
    register_current_context_ops, register_current_cpu_id, try_enter_current_context,
};
#[cfg(any(feature = "runtime-model", feature = "management"))]
pub use ctl::{
    ELM_CORE_CAP_EVENTS, ELM_CORE_CAP_MGR_CHANNEL, ELM_CORE_CAP_SNAPSHOT, ELM_CTL_ABI_VERSION,
    ELM_CTL_MAGIC, ElmCoreInfo, ElmCtlCommand, ElmCtlHeader, ElmCtlStatus,
};
pub use developer::{
    DeviceIrqResult, DirectImport, ELM_API_ROOT_SLOT_SYMBOL, ELM_INTEGRATED_EXPORT_CONTRACT_LEN,
    ELM_INTEGRATED_EXPORT_NAME_LEN, ELM_INTEGRATED_MANAGED_EXPORT_ABI_V1,
    ELM_INTEGRATED_MANAGED_EXPORT_MAGIC, ELM_INTEGRATED_PROVIDER_NAME_LEN,
    ELM_INTEGRATED_PROVIDER_VERSION_LEN, ELM_MIXIN_STAGE_EGRESS, ELM_MIXIN_STAGE_INGRESS,
    ELM_MIXIN_STAGE_OBSERVE, ELM_MIXIN_STAGE_SUBSTITUTE, ELM_MIXIN_STAGES_ALL,
    ELM_MODULE_DESCRIPTOR_ABI_VERSION, ELM_MODULE_DESCRIPTOR_FLAGS_MASK,
    ELM_MODULE_DESCRIPTOR_MAGIC, ELM_MODULE_DESCRIPTOR_SYMBOL,
    ElmIntegratedManagedExportInitialized, ElmIntegratedManagedExportInvoke,
    ElmIntegratedManagedExportV1, ElmModule, ElmModuleDescriptorV1, ElmModuleEntryV1,
    ElmModuleLifecycleEntryV1, ElmModuleMigrationEntryV1, ElmPayload, EntryContext, EntryResult,
    HookError, HookResult, LifecycleContext, ManagedImport, ManagedReply, ManagedRequest,
    ManagedResult, MigrationContext, MigrationExportResult, MixinControl, MixinPointDescriptor,
    ModuleSlot, PayloadError, PointResult, ProviderReply, ProviderRequest, ProviderResult,
    RuntimeApiError, SnapshotReply, SnapshotRequest, SnapshotResult, run_mixin_point,
};
#[cfg(any(feature = "runtime-model", feature = "management"))]
pub use ebi_wire::{
    ELM_EBI_PROJECTION_SOURCE_ABI_VERSION, ELM_EBI_PROJECTION_SOURCE_FLAG_IMAGE_SESSION,
    ELM_EBI_PROJECTION_SOURCE_FLAGS_MASK, ELM_EBI_PROJECTION_SOURCE_REQUEST_SIZE,
    ELM_EBI_SOURCE_ABI_VERSION, ELM_EBI_SOURCE_FLAG_AUTHORIZE_PRIVILEGED_SYMBOLS,
    ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT, ELM_EBI_SOURCE_FLAG_NONE, ELM_EBI_SOURCE_FLAGS_MASK,
    ELM_EBI_SOURCE_REQUEST_SIZE, ELM_IMAGE_SESSION_REFERENCE_ABI_VERSION, ElmEbiLoadStatus,
    ElmEbiSourceKind, ElmEbiSourceRequest, ElmImageSessionReferenceV1, ElmLoadCellResponse,
    ElmProjectionSourceRequest,
};
pub use kernel_symbols::{
    KERNEL_INTEGRATED_PHASE_DEVICE, KERNEL_INTEGRATED_PHASE_RUNTIME, KernelIntegratedComponentV1,
};

#[doc(hidden)]
pub mod __private {
    pub use crate::developer::__private::*;
    pub use crate::kernel_mixin::kernel_mixin_trampoline;
}
#[cfg(feature = "runtime-model")]
pub use ebi::{
    ELM_EBI_ABI_VERSION, ELM_EBI_EXPORT_FLAG_DEPENDENCY, ELM_EBI_EXPORT_FLAG_DIRECT_PINNED,
    ELM_EBI_EXPORT_FLAG_MANAGED, ELM_EBI_EXPORT_FLAG_PRIVATE, ELM_EBI_EXPORT_FLAG_SUBTREE,
    ELM_EBI_EXPORT_FLAGS_MASK, ELM_EBI_HOOK_FLAG_NONE, ELM_EBI_HOOK_ON_FINALIZE,
    ELM_EBI_HOOK_ON_INITIALIZE, ELM_EBI_HOOK_ON_MIGRATE_ABORT, ELM_EBI_HOOK_ON_MIGRATE_EXPORT,
    ELM_EBI_HOOK_ON_MIGRATE_IMPORT, ELM_EBI_HOOK_ON_PAUSE, ELM_EBI_HOOK_ON_QUIESCE,
    ELM_EBI_HOOK_ON_RESUME, ELM_EBI_IMPORT_FLAG_ALLOW_ANCESTOR, ELM_EBI_IMPORT_FLAG_ALLOW_BUILTIN,
    ELM_EBI_IMPORT_FLAG_DIRECT_PINNED, ELM_EBI_IMPORT_FLAG_EXACT_RUST_API,
    ELM_EBI_IMPORT_FLAG_KERNEL_STATIC, ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL,
    ELM_EBI_IMPORT_FLAG_MANAGED, ELM_EBI_IMPORT_FLAG_OPTIONAL, ELM_EBI_IMPORT_FLAGS_MASK,
    ELM_EBI_KERNEL_MIXIN_SELECTOR_LEN, ELM_EBI_MAX_DEPENDENCIES, ELM_EBI_MAX_EXPORTS,
    ELM_EBI_MAX_EXTENSION_POINTS, ELM_EBI_MAX_EXTENSIONS, ELM_EBI_MAX_IMPORTS,
    ELM_EBI_MAX_KERNEL_MIXINS, ELM_EBI_MAX_PROVIDER_PORTS, ELM_EBI_MAX_RELOCATIONS,
    ELM_EBI_MAX_SEGMENTS, ELM_EBI_MAX_SYMBOL_LOCATIONS, ELM_EBI_NAME_LEN,
    ELM_EBI_RELOCATION_FLAG_NONE, ELM_EBI_RUST_ABI_HASH_LEN, ELM_EBI_RUST_ABI_VERSION,
    ELM_EBI_SEGMENT_FLAG_EXECUTE, ELM_EBI_SEGMENT_FLAG_READ, ELM_EBI_SEGMENT_FLAG_RELOCATION_INPUT,
    ELM_EBI_SEGMENT_FLAG_WRITE, ELM_EBI_SEGMENT_FLAG_ZERO_FILL, ELM_EBI_SEGMENT_SOURCE_NONE,
    ELM_EBI_SYMBOL_FLAG_NONE, ELM_EBI_SYMBOL_LOCATION_FLAG_NONE, ELM_EBI_SYMBOL_NAME_LEN,
    ELM_EKI_PROJECTION_SOURCE_ID, ELM_MIGRATION_STATE_MAX, ElmEbiApiCompatibility, ElmEbiArch,
    ElmEbiDependencyDecl, ElmEbiEntry, ElmEbiExportDecl, ElmEbiExtensionDecl,
    ElmEbiExtensionPointDecl, ElmEbiImage, ElmEbiImportDecl, ElmEbiKernelMixinDecl,
    ElmEbiLifecycleHookDecl, ElmEbiLifecycleHookKind, ElmEbiLifecycleHooks, ElmEbiMenuDecl,
    ElmEbiProviderPortDecl, ElmEbiRelocationDecl, ElmEbiRelocationKind, ElmEbiRustHookSignature,
    ElmEbiSegment, ElmEbiSegmentKind, ElmEbiSegmentPayload, ElmEbiSymbolLocationDecl, ElmEbiTarget,
    ElmEbiUnit, ElmImageReader, ElmKernelMixinKind, ElmSliceImageReader, default_segment_flags,
    relocation_width,
};
#[cfg(feature = "runtime-model")]
pub use eki::{
    ELM_EKI_ABI_FINGERPRINT_BLOCK_SIZE, ELM_EKI_BLOCK_DESC_SIZE, ELM_EKI_BLOCK_FLAG_REQUIRED,
    ELM_EKI_ELMAPI_BLOCK_SIZE, ELM_EKI_ELMAPI_BLOCK_VERSION, ELM_EKI_ENTRY_SYMBOL_LEN,
    ELM_EKI_FORMAT_VERSION, ELM_EKI_HEADER_SIZE, ELM_EKI_IMAGE_HASH_SHA256_SIZE,
    ELM_EKI_KERNEL_MIXIN_RECORD_SIZE, ELM_EKI_MAGIC, ELM_EKI_MANIFEST_NAME_LEN,
    ELM_EKI_MANIFEST_VERSION_LEN, ELM_EKI_MAX_BLOCKS, ELM_EKI_MAX_VARIANTS,
    ELM_EKI_PROOF_ALGORITHM_ED25519, ELM_EKI_PROOF_BLOCK_SIZE, ELM_EKI_PROVIDER_PORT_RECORD_SIZE,
    ELM_EKI_RELOCATION_RECORD_SIZE, ELM_EKI_SYMBOL_LOCATION_RECORD_SIZE,
    ELM_EKI_VARIANT_DIRECTORY_VERSION, ELM_EKI_VARIANT_RECORD_SIZE, ElmEkiBlockDesc,
    ElmEkiBlockKind, ElmEkiHeader, ElmEkiSelector, ElmEkiVariantRecord, parse_eki_ebi_unit,
    parse_eki_image, parse_eki_image_for, parse_eki_variants,
};
#[cfg(feature = "runtime-model")]
pub use elmapi::kernel_interface_manifest_v1;
pub use elmapi::{
    ELM_API_ABORT_REASON_CANCEL, ELM_API_ABORT_REASON_PANIC, ELM_API_ABORT_REASON_TIMEOUT,
    ELM_API_CURRENT_VERSION, ELM_API_FEATURE_ABORT, ELM_API_FEATURE_CONTEXT, ELM_API_FEATURE_LOG,
    ELM_API_FEATURE_MANAGED_CALL, ELM_API_FEATURE_MIXIN_DISPATCH, ELM_API_FEATURE_NAMESPACE_QUERY,
    ELM_API_FEATURES_V1, ELM_API_MANAGEMENT_IDENTIFIER, ELM_API_MAX_COMPATIBLE_VERSIONS,
    ELM_API_NAMESPACE_FLAG_MANAGEMENT, ELM_API_NAMESPACE_FLAG_PUBLIC, ELM_API_NAMESPACE_FLAGS_V1,
    ELM_API_NAMESPACE_IDENTIFIER_MAX_LEN, ELM_API_ROOT_IMPORT_CONTRACT, ELM_API_ROOT_IMPORT_NAME,
    ELM_API_ROOT_MAGIC, ELM_API_RUNTIME_IDENTIFIER, ELM_API_STATUS_BUFFER_TOO_SMALL,
    ELM_API_STATUS_INVALID, ELM_API_STATUS_NOT_FOUND, ELM_API_STATUS_OK, ELM_API_STATUS_PERMISSION,
    ELM_API_STATUS_UNSUPPORTED, ELM_API_VERSION_V1, ElmApiAbortCurrentV1, ElmApiContextV1,
    ElmApiCurrentContextV1, ElmApiInvokeManagedV1, ElmApiLogV1, ElmApiMixinDispatchV1,
    ElmApiNamespaceDescriptorV1, ElmApiNamespaceV1, ElmApiQueryNamespaceV1, ElmApiRootV1,
    ElmManagementApiV1, ElmManagementDispatchV1, ElmRuntimeApiV1, is_valid_runtime_api_identifier,
};
pub use error::{ElmError, ElmResult};
#[cfg(any(feature = "runtime-model", feature = "management"))]
pub use event::{ElmEventRecord, ElmEventSequence};
pub use frame::{
    ELM_ACTION_OPCODE_INVOKE, ELM_ACTION_RESULT_HEALTH, ELM_CALL_FLAG_NONE, ELM_CALL_STATUS_BUSY,
    ELM_CALL_STATUS_INVALID, ELM_CALL_STATUS_NOT_FOUND, ELM_CALL_STATUS_OK,
    ELM_CALL_STATUS_PROVIDER_FAULT, ELM_CALL_STATUS_UNSUPPORTED, ELM_FRAME_PAYLOAD_LEN,
    ELM_NATIVE_ENTRY_ABI_VERSION, ELM_NATIVE_MANAGED_CALL_ABI_VERSION,
    ELM_NATIVE_PROVIDER_CALL_ABI_VERSION, ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE, ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK, ElmActionInvokeReply, ElmActionInvokeRequest,
    ElmCallFrame, ElmNativeEntryFrameV1, ElmNativeManagedCallV1, ElmNativeProviderCallV1,
    ElmNativeProviderSnapshotV1, ElmReplyFrame,
};
#[cfg(feature = "runtime-model")]
pub use graph::{
    BindingGraph, CapabilityBindingEdge, DependencyEdge, ExtensionEdge, ExtensionPoint,
    GraphRemovalReport, GraphValidationReport, ParentEdge,
};
pub use ids::{
    ActionId, BindingId, ELM_EKI_BUILTIN_ID, ELM_MGR_BUILTIN_ID, ElmId, Generation, LeaseId, PortId,
};
pub use kernel_mixin::{KernelMixinContext, KernelMixinFrameV1};
/// 内核直接符号按装载期策略划分的能力组。
pub use kernel_symbols::capability as kernel_symbol_capability;
pub use kind::ElmKind;
#[cfg(feature = "runtime-model")]
pub use lease::{LeaseKind, LeaseRegistry, LeaseRights, LeaseState, ResourceLease};
#[cfg(feature = "runtime-model")]
pub use manifest::{ElmManifest, ElmName, ElmVersion};
#[cfg(any(feature = "runtime-model", feature = "management"))]
pub use menu::{
    ELM_MENU_DESCRIPTION_LEN, ELM_MENU_FLAG_DISABLED, ELM_MENU_FLAG_REQUIRES_SYS_ADMIN,
    ELM_MENU_FLAG_TODO, ELM_MENU_LABEL_LEN, ELM_MENU_ROUTE_LEN, ElmMenuItemKind,
    ElmMenuItemSnapshot, ElmMenuSnapshotHeader,
};
#[cfg(feature = "runtime-model")]
pub use metadata::{
    ELM_META_FIELD_ACCESS, ELM_META_FIELD_CONTRACT, ELM_META_FIELD_DIRECTION, ELM_META_FIELD_FLAGS,
    ELM_META_FIELD_HANDLER_CONTRACT, ELM_META_FIELD_HOOK_KIND, ELM_META_FIELD_MAX_VERSION,
    ELM_META_FIELD_MIN_VERSION, ELM_META_FIELD_MODE, ELM_META_FIELD_NAME,
    ELM_META_FIELD_PAYLOAD_CONTRACT, ELM_META_FIELD_POINT, ELM_META_FIELD_PRIORITY,
    ELM_META_FIELD_RUST_ABI, ELM_META_FIELD_STAGE, ELM_META_FIELD_STAGES, ELM_META_FIELD_SYMBOL,
    ELM_META_FIELD_TARGET, ELM_META_FIELD_VERSION, ELM_META_FIELD_WIRE_SIZE,
    ELM_RUST_METADATA_ALIGNMENT, ELM_RUST_METADATA_FIELD_HEADER_SIZE,
    ELM_RUST_METADATA_HEADER_SIZE, ELM_RUST_METADATA_MAGIC, ELM_RUST_METADATA_MAX_RECORD_SIZE,
    ELM_RUST_METADATA_VERSION, ElmRustMetadataError, ElmRustMetadataField, ElmRustMetadataKind,
    ElmRustMetadataRecord, ElmRustMetadataValueKind, crc32, parse_rust_metadata_section,
};
#[cfg(any(feature = "runtime-model", feature = "management"))]
pub use mgr::api::{
    ELM_MGR_API_CONTRACT_LEN, ELM_MGR_API_FLAG_PROVIDER_OPS, ELM_MGR_API_FLAG_STABLE,
    ELM_MGR_API_FLAG_SYSCALL, ELM_MGR_API_FLAG_SYSFS, ELM_MGR_API_FLAG_TODO,
    ELM_MGR_API_KIND_CONTROL, ELM_MGR_API_KIND_EVENT, ELM_MGR_API_KIND_PROVIDER,
    ELM_MGR_API_KIND_SNAPSHOT, ELM_MGR_API_KIND_SUBSYSTEM, ELM_MGR_API_NAME_LEN,
    ELM_MGR_API_NAMESPACE_LEN, ELM_MGR_EVENT_FILTER_ANY, ELM_MGR_EVENT_READ_ABSOLUTE_MAX_RECORDS,
    ELM_MGR_EVENT_READ_DEFAULT_MAX_RECORDS, ELM_MGR_EVENT_READ_FLAG_ADVANCE,
    ELM_MGR_EVENT_SUBSCRIPTION_FLAG_ACTIVE, ELM_RUNTIME_LOG_EXPORT_CONTRACT,
    ELM_RUNTIME_LOG_EXPORT_NAME, ELM_RUNTIME_LOG_EXPORT_VERSION, ElmMgrApiDescriptor,
    ElmMgrApiDescriptorRecord, ElmMgrApiRegistryHeader, ElmMgrEventSubscribeRequest,
    ElmMgrEventSubscribeResponse, ElmMgrEventSubscriptionHeader, ElmMgrEventSubscriptionRecord,
    ElmMgrEventUnsubscribeRequest, ElmMgrEventUnsubscribeResponse, ElmMgrSubscribedEventReadHeader,
    ElmMgrSubscribedEventReadRequest,
};
#[cfg(any(feature = "runtime-model", feature = "management"))]
pub use mgr::{
    ELM_AUDIT_AUTHORITY_ANCESTOR, ELM_AUDIT_AUTHORITY_DELEGATED_MANAGER,
    ELM_AUDIT_AUTHORITY_KERNEL, ELM_AUDIT_AUTHORITY_MANAGER, ELM_AUDIT_AUTHORITY_SELF,
    ELM_AUDIT_AUTHORITY_USER_ADMIN, ELM_AUDIT_FLAG_AUTHORIZATION, ELM_AUDIT_FLAG_OPERATION,
    ELM_CELL_POLICY_ALLOW_ALL, ELM_CELL_POLICY_ALLOW_BIND, ELM_CELL_POLICY_ALLOW_EVENT,
    ELM_CELL_POLICY_ALLOW_EXTENSION, ELM_CELL_POLICY_ALLOW_LIFECYCLE,
    ELM_CELL_POLICY_ALLOW_MANAGEMENT, ELM_CELL_POLICY_ALLOW_NATIVE, ELM_CELL_POLICY_ALLOW_OBSERVE,
    ELM_CELL_POLICY_ALLOW_POLICY_UPDATE, ELM_CELL_POLICY_ALLOW_PROVIDER,
    ELM_CELL_POLICY_ALLOW_RESOURCE_UPDATE, ELM_CELL_POLICY_ALLOWED_ACTIONS_MASK,
    ELM_CELL_POLICY_FLAG_AUDIT_ALL, ELM_CELL_POLICY_FLAG_DENY_CHILD_ESCALATION,
    ELM_CELL_POLICY_FLAG_LOCKED, ELM_CELL_POLICY_FLAGS_MASK,
    ELM_EXTENSION_DISPATCH_FLAG_ALLOW_EMPTY, ELM_EXTENSION_DISPATCH_FLAG_REQUIRE_EXACT_EXTENSION,
    ELM_EXTENSION_DISPATCH_FLAGS_MASK, ELM_EXTENSION_POLICY_ACCEPT, ELM_EXTENSION_POLICY_ALL,
    ELM_EXTENSION_POLICY_ATTACH, ELM_EXTENSION_POLICY_DETACH, ELM_EXTENSION_POLICY_DISPATCH,
    ELM_EXTENSION_POLICY_MIXIN_PATCH, ELM_EXTENSION_RECORD_KIND_EDGE,
    ELM_EXTENSION_RECORD_KIND_POINT, ELM_HEALTH_CHECK_AUDITS, ELM_HEALTH_CHECK_BINDINGS,
    ELM_HEALTH_CHECK_CELLS, ELM_HEALTH_CHECK_EVENTS, ELM_HEALTH_CHECK_EXECUTIONS,
    ELM_HEALTH_CHECK_GRAPH, ELM_HEALTH_CHECK_JOURNAL, ELM_HEALTH_CHECK_MENU,
    ELM_HEALTH_CHECK_NATIVE_CAPABILITIES, ELM_HEALTH_CHECK_PORTS,
    ELM_HEALTH_CHECK_PROJECTION_SOURCES, ELM_HEALTH_CHECK_PROVIDERS, ELM_HEALTH_CHECK_RESOURCES,
    ELM_HEALTH_CHECK_RUNTIME_PORTS, ELM_HEALTH_CHECK_SEQUENCES, ELM_HEALTH_CHECK_TODO_REGISTRY,
    ELM_HEALTH_CHECK_TRUST, ELM_HEALTH_DETAIL_CONTRACT_INVALID,
    ELM_HEALTH_DETAIL_COUNTER_EXHAUSTED, ELM_HEALTH_DETAIL_DANGLING_REFERENCE,
    ELM_HEALTH_DETAIL_DROPPED_RECORDS, ELM_HEALTH_DETAIL_DUPLICATE_OBJECT,
    ELM_HEALTH_DETAIL_GRAPH_INVALID, ELM_HEALTH_DETAIL_KIND_MISMATCH,
    ELM_HEALTH_DETAIL_MISSING_OBJECT, ELM_HEALTH_DETAIL_NONE, ELM_HEALTH_DETAIL_PERSISTENCE_FAILED,
    ELM_HEALTH_DETAIL_RESOURCE_LEAK, ELM_HEALTH_DETAIL_SEQUENCE_INVALID,
    ELM_HEALTH_DETAIL_STATE_INVALID, ELM_HEALTH_DETAIL_STUCK_REFERENCE,
    ELM_HEALTH_FLAG_HAS_FAILURES, ELM_IMAGE_SESSION_ABI_VERSION, ELM_IMAGE_SESSION_DEFAULT_TTL_MS,
    ELM_IMAGE_SESSION_DIGEST_LEN, ELM_IMAGE_SESSION_FLAG_NONE, ELM_IMAGE_SESSION_HASH_SHA256,
    ELM_IMAGE_SESSION_MAX_ACTIVE, ELM_IMAGE_SESSION_MAX_CHUNK, ELM_IMAGE_SESSION_MAX_LENGTH,
    ELM_IMAGE_SESSION_MAX_PER_OWNER, ELM_IMAGE_SESSION_MAX_RESERVED_BYTES,
    ELM_IMAGE_SESSION_MAX_TTL_MS, ELM_LIFECYCLE_REASON_ABI_FINGERPRINT,
    ELM_LIFECYCLE_REASON_BUILTIN_PROTECTED, ELM_LIFECYCLE_REASON_CALLER_NOT_FOUND,
    ELM_LIFECYCLE_REASON_CALLER_STALE, ELM_LIFECYCLE_REASON_CELL_NOT_FOUND,
    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT, ELM_LIFECYCLE_REASON_HAS_CHILDREN,
    ELM_LIFECYCLE_REASON_HAS_DEPENDENTS, ELM_LIFECYCLE_REASON_HAS_EXTENSIONS,
    ELM_LIFECYCLE_REASON_HOOK_FAILED, ELM_LIFECYCLE_REASON_INVALID_STATE,
    ELM_LIFECYCLE_REASON_LEASE_BUSY, ELM_LIFECYCLE_REASON_NATIVE_TODO, ELM_LIFECYCLE_REASON_NONE,
    ELM_LIFECYCLE_REASON_POLICY_ESCALATION, ELM_LIFECYCLE_REASON_ROLLBACK_REJECTED,
    ELM_LIFECYCLE_REASON_SCOPE_DENIED, ELM_LIFECYCLE_REASON_UNTRUSTED_IMAGE,
    ELM_MGR_ACTION_API_QUERY, ELM_MGR_ACTION_BIND, ELM_MGR_ACTION_DETACH,
    ELM_MGR_ACTION_EVENT_READ, ELM_MGR_ACTION_EVENT_SUBSCRIBE, ELM_MGR_ACTION_EVENT_UNSUBSCRIBE,
    ELM_MGR_ACTION_EXTENSION_ATTACH, ELM_MGR_ACTION_EXTENSION_DETACH,
    ELM_MGR_ACTION_EXTENSION_DISPATCH, ELM_MGR_ACTION_EXTENSION_QUERY, ELM_MGR_ACTION_FAULT_QUERY,
    ELM_MGR_ACTION_HEALTH_QUERY, ELM_MGR_ACTION_IMAGE_SESSION,
    ELM_MGR_ACTION_NATIVE_CAPABILITY_QUERY, ELM_MGR_ACTION_PAUSE, ELM_MGR_ACTION_POLICY_UPDATE,
    ELM_MGR_ACTION_PROVIDER_ASYNC, ELM_MGR_ACTION_PROVIDER_INVOKE, ELM_MGR_ACTION_PROVIDER_QUERY,
    ELM_MGR_ACTION_PROVIDER_REGISTER, ELM_MGR_ACTION_PROVIDER_UNREGISTER, ELM_MGR_ACTION_REPLACE,
    ELM_MGR_ACTION_RESOURCE_UPDATE, ELM_MGR_ACTION_RESUME, ELM_MGR_ACTION_RUNTIME_EVENT_ACK,
    ELM_MGR_ACTION_RUNTIME_EVENT_READ, ELM_MGR_ACTION_RUNTIME_LOG, ELM_MGR_ACTION_TODO_QUERY,
    ELM_MGR_ACTION_TRACE_QUERY, ELM_MGR_ACTION_TRUST_QUERY, ELM_MGR_ACTION_UNBIND,
    ELM_MGR_EXTENSION_CONTRACT_LEN, ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN,
    ELM_MGR_EXTENSION_PAYLOAD_LEN, ELM_MGR_EXTENSION_POINT_LEN, ELM_MGR_MAX_INPUT,
    ELM_MGR_MAX_PAYLOAD, ELM_MGR_POLICY_API_REGISTRY, ELM_MGR_POLICY_AUDIT,
    ELM_MGR_POLICY_CELL_CAPABILITIES, ELM_MGR_POLICY_EVENT_SUBSCRIPTIONS,
    ELM_MGR_POLICY_EXTENSION_RUNTIME, ELM_MGR_POLICY_FAULT_OBSERVABILITY, ELM_MGR_POLICY_HEALTH,
    ELM_MGR_POLICY_HOT_REPLACE, ELM_MGR_POLICY_IMAGE_SESSIONS,
    ELM_MGR_POLICY_LOAD_REQUIRES_EBI_SOURCE, ELM_MGR_POLICY_MENU_BINDING,
    ELM_MGR_POLICY_NATIVE_CAPABILITIES, ELM_MGR_POLICY_NATIVE_LIFECYCLE,
    ELM_MGR_POLICY_NEXUS_BINDING, ELM_MGR_POLICY_PREFLIGHT, ELM_MGR_POLICY_PROVIDER_ASYNC,
    ELM_MGR_POLICY_PROVIDER_PORTS, ELM_MGR_POLICY_RESOURCE_BUDGET, ELM_MGR_POLICY_RUNTIME_JOURNAL,
    ELM_MGR_POLICY_TODO_REGISTRY, ELM_MGR_POLICY_TRACE_RINGS, ELM_MGR_POLICY_TRUST,
    ELM_MGR_RELATION_CONTRACT_LEN, ELM_MGR_RELATION_POINT_LEN, ELM_MGR_STATUS_BUSY,
    ELM_MGR_STATUS_EXPIRED, ELM_MGR_STATUS_INTEGRITY, ELM_MGR_STATUS_INVALID,
    ELM_MGR_STATUS_NO_MEMORY, ELM_MGR_STATUS_NOT_FOUND, ELM_MGR_STATUS_OK,
    ELM_MGR_STATUS_PERMISSION, ELM_MGR_STATUS_TODO, ELM_MGR_STATUS_UNSUPPORTED,
    ELM_MIXIN_REPLY_CONTINUE, ELM_MIXIN_REPLY_DENY, ELM_MIXIN_REPLY_FLAGS_MASK,
    ELM_MIXIN_REPLY_REPLACE, ELM_MIXIN_REPLY_STOP, ELM_NATIVE_CAPABILITY_FLAG_KERNEL_SYMBOL,
    ELM_NATIVE_CAPABILITY_FLAG_TRUNCATED, ELM_NATIVE_CAPABILITY_FLAG_VERSION_WILDCARD,
    ELM_NATIVE_CAPABILITY_KIND_EXPORT, ELM_NATIVE_CAPABILITY_KIND_IMPORT,
    ELM_NATIVE_CAPABILITY_NAME_LEN, ELM_NATIVE_POLICY_ALL, ELM_NATIVE_POLICY_EXECUTE,
    ELM_NATIVE_POLICY_EXPORT, ELM_NATIVE_POLICY_IMPORT, ELM_NATIVE_POLICY_MIXIN_PATCH,
    ELM_NATIVE_POLICY_REPLACE, ELM_NEXUS_CONTRACT_LEN, ELM_POLICY_BLOCK_ABI_FINGERPRINT,
    ELM_POLICY_BLOCK_BINDING_NOT_FOUND, ELM_POLICY_BLOCK_BINDING_PROTECTED,
    ELM_POLICY_BLOCK_BUILTIN_PROTECTED, ELM_POLICY_BLOCK_CALLER_NOT_FOUND,
    ELM_POLICY_BLOCK_CALLER_STALE, ELM_POLICY_BLOCK_CAPABILITY_DENIED,
    ELM_POLICY_BLOCK_CELL_NOT_FOUND, ELM_POLICY_BLOCK_CONTRACT_MISMATCH,
    ELM_POLICY_BLOCK_DUPLICATE_BINDING, ELM_POLICY_BLOCK_EXTENSION_DUPLICATE,
    ELM_POLICY_BLOCK_EXTENSION_NOT_FOUND, ELM_POLICY_BLOCK_GRAPH_INCONSISTENT,
    ELM_POLICY_BLOCK_HAS_CHILDREN, ELM_POLICY_BLOCK_HAS_DEPENDENTS,
    ELM_POLICY_BLOCK_HAS_EXTENSIONS, ELM_POLICY_BLOCK_INVALID_STATE,
    ELM_POLICY_BLOCK_JOURNAL_UNAVAILABLE, ELM_POLICY_BLOCK_LEASE_BUSY,
    ELM_POLICY_BLOCK_LIFECYCLE_HOOK_FAILED, ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE,
    ELM_POLICY_BLOCK_NATIVE_CALL_FAILED, ELM_POLICY_BLOCK_NATIVE_TODO,
    ELM_POLICY_BLOCK_POLICY_ESCALATION, ELM_POLICY_BLOCK_PORT_NOT_FOUND,
    ELM_POLICY_BLOCK_PORT_TODO, ELM_POLICY_BLOCK_PROVIDER_BUSY,
    ELM_POLICY_BLOCK_PROVIDER_CALL_CANCELED, ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED,
    ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED, ELM_POLICY_BLOCK_PROVIDER_NOT_FOUND,
    ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL, ELM_POLICY_BLOCK_RESOURCE_QUOTA,
    ELM_POLICY_BLOCK_ROLLBACK_REJECTED, ELM_POLICY_BLOCK_SCOPE_DENIED,
    ELM_POLICY_BLOCK_UNTRUSTED_IMAGE, ELM_PROVIDER_ASYNC_DEFAULT_RESULT_TTL_MS,
    ELM_PROVIDER_ASYNC_DEFAULT_TIMEOUT_MS, ELM_PROVIDER_ASYNC_MAX_TIMEOUT_MS,
    ELM_PROVIDER_ASYNC_QUEUE_LIMIT, ELM_PROVIDER_FLAG_DYNAMIC, ELM_PROVIDER_FLAG_KERNEL_BACKEND,
    ELM_PROVIDER_FLAG_NATIVE_BACKEND, ELM_PROVIDER_FLAG_TODO_BACKEND, ELM_PROVIDER_POLICY_ALL,
    ELM_PROVIDER_POLICY_ASYNC, ELM_PROVIDER_POLICY_INVOKE, ELM_PROVIDER_POLICY_REGISTER,
    ELM_PROVIDER_POLICY_SNAPSHOT, ELM_PROVIDER_POLICY_UNREGISTER, ELM_PROVIDER_PORT_FLAG_NONE,
    ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED, ELM_PROVIDER_SNAPSHOT_REQUEST_FLAGS_MASK,
    ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE, ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAGS_MASK,
    ELM_REPLACE_CELL_ABI_VERSION, ELM_REPLACE_CELL_FLAG_AUTHORIZE_PRIVILEGED_SYMBOLS,
    ELM_REPLACE_CELL_FLAGS_MASK, ELM_REPLACE_MIGRATION_STATE_MAX, ELM_RESOURCE_POLICY_ALL,
    ELM_RESOURCE_POLICY_OWN, ELM_RESOURCE_POLICY_QUERY, ELM_RESOURCE_POLICY_UPDATE,
    ELM_RUNTIME_LOG_MESSAGE_LEN, ELM_RUNTIME_TRACE_KIND_JOURNAL, ELM_RUNTIME_TRACE_KIND_LIFECYCLE,
    ELM_RUNTIME_TRACE_KIND_MIXIN_DISPATCH, ELM_RUNTIME_TRACE_KIND_POLICY,
    ELM_RUNTIME_TRACE_KIND_PROVIDER_CALL, ELM_RUNTIME_TRACE_KIND_REPLACE,
    ELM_RUNTIME_TRACE_KIND_RESOURCE, ELM_RUNTIME_TRACE_KIND_TRUST, ELM_TODO_DETAIL_LEN,
    ELM_TODO_FLAG_ACTIVE, ELM_TODO_FLAG_STATIC, ELM_TODO_KIND_FRAMEWORK, ELM_TODO_KIND_NATIVE,
    ELM_TODO_KIND_PROVIDER, ELM_TODO_KIND_RUNTIME, ELM_TODO_KIND_SOURCE, ELM_TODO_NAME_LEN,
    ELM_TODO_REGISTRY_FLAG_TRUNCATED, ELM_TRUST_FLAG_ALLOW_UNSIGNED, ELM_TRUST_FLAG_SEALED,
    ELM_TRUST_FLAG_UNSIGNED_ACTIVE, ElmCellPolicyRequest, ElmCellPolicyV1, ElmCoreHealthHeader,
    ElmCoreHealthRecord, ElmExtensionAttachRequest, ElmExtensionAttachResponse,
    ElmExtensionDetachRequest, ElmExtensionDetachResponse, ElmExtensionDispatchRequest,
    ElmExtensionDispatchResponse, ElmExtensionSnapshotHeader, ElmExtensionSnapshotRecord,
    ElmFaultDumpHeader, ElmFaultDumpRecord, ElmImageSessionBeginRequestV1, ElmImageSessionInfoV1,
    ElmImageSessionRequestV1, ElmImageSessionState, ElmImageSessionWriteRequestV1,
    ElmLifecycleAction, ElmLifecyclePlanRequest, ElmLifecyclePlanResponse, ElmLifecycleRequest,
    ElmLifecycleResponse, ElmMgrAuditHeader, ElmMgrAuditRecord, ElmMgrCallHeader, ElmMgrCallKind,
    ElmMgrPolicyInfo, ElmMgrRelationKind, ElmMgrRelationRecord, ElmMgrResponseHeader,
    ElmMgrTopologyHeader, ElmNativeCapabilityHeader, ElmNativeCapabilityRecord,
    ElmNexusBindPlanResponse, ElmNexusBindRequest, ElmNexusBindingRecord,
    ElmNexusBindingSnapshotHeader, ElmNexusUnbindRequest, ElmProviderAsyncCancelRequest,
    ElmProviderAsyncCancelResponse, ElmProviderAsyncPollRequest, ElmProviderAsyncPollResponse,
    ElmProviderAsyncState, ElmProviderAsyncSubmitRequest, ElmProviderAsyncSubmitResponse,
    ElmProviderInvokeRequest, ElmProviderInvokeResponse, ElmProviderPortRecord,
    ElmProviderPortRegisterRequest, ElmProviderPortRegisterResponse, ElmProviderPortStatsHeader,
    ElmProviderPortStatsRecord, ElmProviderPortUnregisterRequest, ElmProviderQueueStatsHeader,
    ElmProviderQueueStatsRecord, ElmProviderSnapshotHeader, ElmProviderSnapshotRequest,
    ElmReplaceCellRequestV1, ElmReplaceCellResponseV1, ElmResourceBudgetRequest,
    ElmResourceBudgetResponse, ElmResourceBudgetUpdateRequest, ElmRuntimeEventRequest,
    ElmRuntimeEventResponse, ElmRuntimeLogRequest, ElmRuntimeLogResponse,
    ElmRuntimePortStatsHeader, ElmRuntimePortStatsRecord, ElmRuntimeTraceHeader,
    ElmRuntimeTraceRecord, ElmTodoRegistryHeader, ElmTodoRegistryRecord, ElmTrustRuntimeInfoV1,
    first_lifecycle_reason, planned_final_state, status_from_blockers,
};
#[cfg(feature = "runtime-model")]
pub use nexus::{
    FlowBackpressure, FlowConcurrency, FlowContract, IntentKind, NexusIntent, NexusOffer,
};
#[cfg(feature = "runtime-model")]
pub use policy::{
    ElmPolicyCheck, ElmPrincipal, ElmPrincipalKind, check_current_cell, current_cell_allows,
};
#[cfg(feature = "runtime-model")]
pub use ports::{BuiltinPort, PortDescriptor, builtin_port_descriptors};
#[cfg(feature = "runtime-model")]
pub use proof::{
    ELM_PROOF_ABI_VERSION, ELM_PROOF_ED25519_PUBLIC_KEY_LEN, ELM_PROOF_ED25519_SIGNATURE_LEN,
    ELM_PROOF_SHA256_LEN, ELM_PROOF_SOURCE_IDENTIFIER_LEN, ELM_RUST_ABI_FINGERPRINT_VERSION,
    ELM_RUST_ABI_TARGET_FEATURE_FLOAT, ELM_RUST_ABI_TARGET_FEATURE_SIMD,
    ELM_RUST_ABI_TARGET_FEATURE_VECTOR, ElmEbiProofV1, ElmPanicStrategy, ElmRustAbiFingerprintV1,
    ElmTrustAcceptance, ElmTrustAnchor, ElmTrustError, ElmTrustStore, Sha256, canonical_ebi_digest,
    sha256, sha256_with_zeroed_range, sha256_with_zeroed_ranges,
};
#[cfg(feature = "runtime-model")]
pub use provider::{
    ELM_KERNEL_PROVIDER_FLAG_NONE, ELM_KERNEL_PROVIDER_FLAG_TODO, ElmKernelProviderInvoke,
    ElmKernelProviderRevoke, ElmKernelProviderSnapshot, ElmKernelProviderSnapshotPage,
    ElmKernelProviderSnapshotPaged, ElmKernelProviderSpec, elm_kernel_provider_unsupported,
};
#[cfg(any(feature = "runtime-model", feature = "management"))]
pub use resource::{
    ELM_OWNED_RESOURCE_ABI_VERSION, ELM_OWNED_RESOURCE_FLAG_NONE,
    ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED, ElmOwnedResourceKind, ElmOwnedResourceOp,
    ElmOwnedResourceOpsV1, ElmOwnedResourceSnapshotV1, ElmOwnedResourceState, ElmResourceBudget,
    ElmResourceKind, ElmResourceUsage,
};
#[cfg(feature = "runtime-model")]
pub use snapshot::{
    ELM_CELL_LIFECYCLE_EXECUTOR_READY, ELM_CELL_LIFECYCLE_FINALIZED,
    ELM_CELL_LIFECYCLE_HOOKS_DECLARED, ELM_CELL_LIFECYCLE_INITIALIZED, ELM_CELL_NAME_LEN,
    ELM_CELL_TRUST_INTERNAL, ELM_CELL_TRUST_SIGNED, ELM_CELL_TRUST_UNSIGNED, ELM_CONTRACT_NAME_LEN,
    ElmCellSnapshot, ElmPortSnapshot, ElmSnapshotHeader, state_code,
};
pub use state::{ElmState, ElmTransition};
#[cfg(feature = "runtime-model")]
pub use topology::{TopologyEvent, TopologyEventKind, TopologySnapshot};
pub use wire::{ElmMixinMode, ElmPortAccessPolicy, FlowDirection, FlowMode};

#[cfg(test)]
mod tests;

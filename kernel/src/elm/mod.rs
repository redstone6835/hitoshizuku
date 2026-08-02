//! ELM（可拓展内核单元）内核核心。
//!
//! 本模块只实现 ELM 自己的枢纽连接层和管理入口，不复用 Linux 模块系统调用。

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use elm_model::{
    ELM_MGR_BUILTIN_ID, ElmEbiLoadStatus, ElmEbiSourceKind, ElmPrincipal, ElmResourceBudget,
    ElmSliceImageReader, canonical_ebi_digest, sha256,
};
use sched::Task;
use sched::sync::Spinlock;

/// 不启用墙钟看门狗；调用仍受保护域取消、异常和 ABI 校验约束。
pub(crate) const NO_WATCHDOG_DEADLINE_NS: u64 = 0;

/// ELM 生命周期通知只描述运行时可观察的状态变化，不携带任何子系统资源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ElmLifecycleEvent {
    CellLoaded { cell: elm_model::ElmId },
}

type LifecycleObserver = fn(ElmLifecycleEvent);

#[derive(Clone, Copy)]
struct LifecycleObserverEntry {
    owner: &'static str,
    callback: LifecycleObserver,
}

static LIFECYCLE_OBSERVERS: Spinlock<Vec<LifecycleObserverEntry>> = Spinlock::new(Vec::new());

/// 注册常驻内核的 ELM 生命周期观察者；同一 owner 重复注册是幂等的。
pub(crate) fn register_lifecycle_observer(
    owner: &'static str,
    callback: LifecycleObserver,
) -> bool {
    let mut observers = LIFECYCLE_OBSERVERS.lock();
    if observers.iter().any(|observer| observer.owner == owner) {
        return true;
    }
    if observers.try_reserve(1).is_err() {
        return false;
    }
    observers.push(LifecycleObserverEntry { owner, callback });
    true
}

fn notify_lifecycle_event(event: ElmLifecycleEvent) {
    // 复制函数指针后再回调，避免观察者回调期间持有注册表锁。
    let registered = LIFECYCLE_OBSERVERS.lock();
    let mut observers = Vec::new();
    if observers.try_reserve(registered.len()).is_err() {
        log::error!("[elm] 生命周期观察者通知分配失败");
        return;
    }
    observers.extend(registered.iter().copied());
    drop(registered);
    for observer in observers {
        (observer.callback)(event);
    }
}

/// 常驻内核 consumer 对一个 ELM exact-Rust export 的代际固定描述。
///
/// 该对象保存复制到常驻内存中的身份和 ABI 摘要，并在首次成功解析后缓存当前 generation
/// 的不可变 export 路由；每次调用仍由 Core 重新校验 cell 状态和 generation，并取得
/// active-execution 引用。
pub(crate) struct PinnedNativeCall {
    owner: elm_model::ElmId,
    generation: elm_model::Generation,
    name: String,
    contract: elm_model::FlowContract,
    version: u32,
    rust_abi_hash: [u8; 32],
    stack: native::PinnedNativeStack,
    route: Spinlock<Option<PinnedNativeRoute>>,
}

#[derive(Clone, Copy)]
struct PinnedNativeRoute {
    address: usize,
    bounds: native::NativeExecutionBounds,
}

impl PinnedNativeCall {
    pub(crate) fn new(
        owner: elm_model::ElmId,
        generation: elm_model::Generation,
        name: &str,
        contract: &str,
        version: u32,
        rust_abi: &str,
    ) -> Result<Self, &'static str> {
        if owner.0 == 0 || generation.0 == 0 || version == 0 {
            return Err("invalid pinned native identity");
        }
        let stack = native::PinnedNativeStack::allocate()
            .map_err(|_| "failed to allocate pinned native stack")?;
        Ok(Self {
            owner,
            generation,
            name: name.to_string(),
            contract: elm_model::FlowContract::new(contract)
                .map_err(|_| "invalid pinned native contract")?,
            version,
            rust_abi_hash: elm_model::sha256(rust_abi.as_bytes()),
            stack,
            route: Spinlock::new(None),
        })
    }
}

mod api_registry;
mod core;
mod event;
mod executor;
mod journal;
mod kernel_mixin;
mod kernel_symbols;
mod menu;
mod mgr_channel;
mod native;
mod owned_resource;
mod ports;
mod resource_accounting;
mod snapshot;
mod source;
pub(crate) mod syscall;
#[cfg(feature = "kernel-tests")]
mod tests;
mod trust_config;

const _: () = assert!(
    elm_model::ELM_CONTEXT_MAX_CPUS >= sched::NR_CPUS,
    "ELM 执行上下文容量小于调度器 CPU 容量"
);
const _: () = assert!(
    sched::NR_CPUS <= u64::BITS as usize,
    "ELM worker 位图无法覆盖调度器 CPU 容量"
);
const _: () = assert!(
    allocator::MAX_CPUS >= sched::NR_CPUS,
    "分配器 CPU 本地槽位少于 ELM 可运行 CPU 数"
);

pub(crate) fn kernel_interface_profile_hash() -> Result<[u8; 32], &'static str> {
    kernel_symbols::catalog_profile_hash().map_err(|_| "内核符号目录无效")
}

pub(crate) fn init_builtin_mgr() {
    let _ = acpi::kernel_symbol_catalog_anchor();
    let _ = allocator::kernel_symbol_catalog_anchor();
    let _ = efi::kernel_symbol_catalog_anchor();
    let _ = elf::kernel_symbol_catalog_anchor();
    let _ = errno::kernel_symbol_catalog_anchor();
    let _ = extfs::kernel_symbol_catalog_anchor();
    let _ = fatfs::kernel_symbol_catalog_anchor();
    let _ = general::kernel_symbol_catalog_anchor();
    let _ = hal::kernel_symbol_catalog_anchor();
    let _ = mm::kernel_symbol_catalog_anchor();
    let _ = net::kernel_symbol_catalog_anchor();
    let _ = sched::kernel_symbol_catalog_anchor();
    let _ = vfs::kernel_symbol_catalog_anchor();
    let _ = elm_model::register_current_cpu_id(sched::current_cpu_id);
    if !general::elm_guard::register_task_context_backend() {
        log::error!("[elm] 无法注册任务级执行上下文后端");
        return;
    }
    if !resource_accounting::init() {
        log::error!("[elm] 资源账本初始化失败");
        return;
    }
    if !owned_resource::init() {
        log::error!("[elm] 所有权资源注册表初始化失败");
        return;
    }
    if !::kernel_symbols::install_runtime_hooks(&KERNEL_SYMBOL_RUNTIME_HOOKS) {
        log::error!("[elm] 无法安装直接内核符号资源归属钩子");
        return;
    }
    if let Err(err) = kernel_symbols::validate_catalog() {
        log::error!("[elm] 内核直接符号目录无效: {:?}", err);
        return;
    }
    if let Err(err) = kernel_mixin::validate_catalog() {
        log::error!("[elm] 内核 Mixin 站点目录无效: {:?}", err);
        return;
    }
    if !::kernel_symbols::install_mixin_runtime_hooks(&kernel_mixin::RUNTIME_HOOKS) {
        log::error!("[elm] 无法安装内核 Mixin 路由钩子");
        return;
    }
    if !api_registry::init() {
        log::error!("[elm] 运行时 API 注册表初始化失败");
        return;
    }
    if let Err(err) = journal::init() {
        log::error!("[elm] 持久日志初始化失败: {:?}", err);
        if !journal::runtime_info().initialized {
            return;
        }
    }
    let journal_info = journal::runtime_info();
    if journal_info.failed {
        log::warning!(
            "[elm] 持久日志以{}模式继续，last_error={}",
            if journal_info.required {
                "sealed"
            } else {
                "volatile-degraded"
            },
            journal_info.last_error
        );
    }
    match core::with_core(|core| {
        if core.initialized() {
            return Ok(0);
        }
        let configured_anchor_count = trust_config::register_configured_anchors(core)
            .map_err(|_| elm_model::ElmError::InvalidTransition)?;
        if let Some(cmdline) = general::start_cmdline() {
            match general::cmdline::Cmdline::new(cmdline).find("elm.allow_unsigned") {
                Some("1" | "true" | "yes") => {
                    core.set_allow_unsigned_external(true)
                        .map_err(|_| elm_model::ElmError::InvalidTransition)?;
                }
                Some("0" | "false" | "no") => {
                    core.set_allow_unsigned_external(false)
                        .map_err(|_| elm_model::ElmError::InvalidTransition)?;
                }
                Some(value) => {
                    log::warning!("[elm] ignored invalid elm.allow_unsigned={}", value);
                }
                None => {}
            }
        }
        core.init_builtin_mgr()?;
        core.mark_global_runtime_scope();
        Ok::<usize, elm_model::ElmError>(configured_anchor_count)
    }) {
        Ok(configured_anchor_count) => {
            if configured_anchor_count != 0 {
                log::info!(
                    "[elm] registered {} configured trust anchor(s)",
                    configured_anchor_count
                );
            }
            general::vfs::sysfs::register_elm_renderer(render_sysfs_file);
            let _ = executor::reconcile_provider_workers();
        }
        Err(err) => log::error!("[elm] init builtin elm-mgr failed: {:?}", err),
    }
}

/// 在架构完成 AP 启动后，使 ELM 后台执行器与调度器 active CPU 集合保持一致。
pub(crate) fn synchronize_smp_runtime() {
    if !core::with_core(|core| core.initialized()) {
        log::warning!("[elm][executor] elm-mgr 未初始化，跳过 SMP worker 同步");
        return;
    }
    let started = executor::reconcile_provider_workers();
    let snapshot = executor::snapshot();
    let missing = snapshot.active_mask & !snapshot.worker_mask;
    if missing != 0 {
        log::error!(
            "[elm][executor] SMP worker 集合不完整: active={:#x} workers={:#x} missing={:#x}",
            snapshot.active_mask,
            snapshot.worker_mask,
            missing
        );
        return;
    }
    log::info!(
        "[elm][executor] SMP runtime synchronized: online={:#x} active={:#x} workers={:#x} added={}",
        snapshot.online_mask,
        snapshot.active_mask,
        snapshot.worker_mask,
        started
    );
}

/// 执行一次 kernel-consumer pinned native call。
///
/// Core 锁只用于准备和完成阶段；模块代码运行期间不持有 Core 锁。
pub(crate) fn invoke_pinned_native<T>(
    call: &PinnedNativeCall,
    frame: &mut T,
    host_ranges: &[(usize, usize)],
    deadline_ns: u64,
) -> Result<i32, i32> {
    #[cfg(feature = "performance-profile")]
    let prepare_start = profiling::read_counter();
    let prepared = with_core(|core| core.prepare_pinned_native_call(call));
    #[cfg(feature = "performance-profile")]
    profiling::observe(
        profiling::Metric::PinnedCallPrepareCycles,
        profiling::read_counter().wrapping_sub(prepare_start),
    );
    let plan = prepared?;
    #[cfg(feature = "performance-profile")]
    let execution_start = profiling::read_counter();
    let status = native::invoke_pinned_export(
        plan.callee.cell,
        plan.callee.generation,
        plan.address,
        plan.bounds,
        frame,
        host_ranges,
        &call.stack,
        plan.callee.allowed_actions,
        deadline_ns,
    );
    #[cfg(feature = "performance-profile")]
    profiling::observe(
        profiling::Metric::PinnedCallExecutionCycles,
        profiling::read_counter().wrapping_sub(execution_start),
    );
    #[cfg(feature = "performance-profile")]
    let complete_start = profiling::read_counter();
    let completed = with_core(|core| core.complete_pinned_native_call(plan, status));
    #[cfg(feature = "performance-profile")]
    profiling::observe(
        profiling::Metric::PinnedCallCompleteCycles,
        profiling::read_counter().wrapping_sub(complete_start),
    );
    completed
}

pub(crate) fn load_build_bound_modules(init: &Arc<Task>) -> Result<usize, String> {
    let modules = trust_config::build_bound_modules();
    if modules.is_empty() {
        return Ok(0);
    }
    validate_build_bound_environment(init)?;

    let arch = current_ebi_arch();
    let mut loaded = 0usize;
    for module in modules {
        load_build_bound_module(init, module, arch)?;
        loaded += 1;
    }
    Ok(loaded)
}

fn validate_build_bound_environment(init: &Arc<Task>) -> Result<(), String> {
    let current_profile = kernel_interface_profile_hash()?;
    if current_profile != trust_config::build_profile_hash() {
        return Err("BuildBound Profile 与当前内核符号目录不一致".to_string());
    }
    let manifest = crate::user::load_file_from_task_vfs(init, "/lib/elm/modules.manifest")
        .map_err(|err| format!("读取 BuildBound 清单失败: {err:?}"))?;
    if sha256(&manifest) != trust_config::build_manifest_hash() {
        return Err("initramfs 中的 BuildBound 清单摘要不匹配".to_string());
    }
    Ok(())
}

fn load_build_bound_module(
    init: &Arc<Task>,
    module: &trust_config::ElmBuildBoundRecord,
    arch: elm_model::ElmEbiArch,
) -> Result<elm_model::ElmId, String> {
    let path = format!("/lib/elm/{}", module.file_name);
    let bytes = crate::user::load_file_from_task_vfs(init, &path)
        .map_err(|err| format!("读取 BuildBound 模块 {} 失败: {err:?}", module.name))?;
    if sha256(&bytes) != module.eki_hash {
        return Err(format!("BuildBound 模块 {} 的 EKI 摘要不匹配", module.name));
    }
    let reader = ElmSliceImageReader::new(&bytes);
    let image = source::project_ebi_image(module.provider_id, &reader, arch)
        .map_err(|status| format!("投影 BuildBound 模块 {} 失败: {status:?}", module.name))?;
    if image.unit.manifest.name.as_str() != module.name
        || canonical_ebi_digest(&image) != module.ebi_hash
    {
        return Err(format!("BuildBound 模块 {} 的 EBI 身份不匹配", module.name));
    }
    let budget = ElmResourceBudget::BUILD_BOUND;
    let mut authorization = with_core(|core| {
        core.authorize_mgr_call(
            ElmPrincipal::kernel(),
            elm_model::ElmMgrCallKind::LoadCell,
            core::ElmMgrAccessTarget::Load(ELM_MGR_BUILTIN_ID, budget),
        )
    });
    if !authorization.allowed() {
        return Err(format!(
            "BuildBound 模块 {} 未通过内核装载策略",
            module.name
        ));
    }
    let response = core::load_ebi_image_unlocked(
        image,
        arch,
        ElmEbiSourceKind::BuildBound,
        ELM_MGR_BUILTIN_ID,
        budget,
        false,
        true,
        &mut authorization,
    )
    .map_err(|status| format!("装载 BuildBound 模块 {} 失败: {status}", module.name))?;
    if response.status != ElmEbiLoadStatus::Ok as i32 {
        return Err(format!(
            "装载 BuildBound 模块 {} 被运行时拒绝: {}",
            module.name, response.status
        ));
    }
    log::info!(
        "[elm] BuildBound module active: name={} cell={} order={}",
        module.name,
        response.cell_id,
        module.order
    );
    Ok(elm_model::ElmId(response.cell_id))
}

#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub(crate) fn detach_build_bound_module_for_test(name: &str) -> Result<elm_model::ElmId, String> {
    let cell = with_core(|core| {
        core.cells()
            .iter()
            .find(|cell| cell.name == name && cell.state == elm_model::ElmState::Active)
            .map(|cell| cell.id)
    })
    .ok_or_else(|| format!("找不到活跃 BuildBound 模块 {name}"))?;
    let mut authorization = with_core(|core| {
        core.authorize_mgr_call(
            ElmPrincipal::kernel(),
            elm_model::ElmMgrCallKind::DetachCell,
            core::ElmMgrAccessTarget::Cell(cell),
        )
    });
    if !authorization.allowed() {
        return Err(format!("BuildBound 模块 {name} 未通过内核卸载策略"));
    }
    let response = core::detach_cell_unlocked(cell, &mut authorization)
        .map_err(|status| format!("卸载 BuildBound 模块 {name} 失败: {status}"))?;
    if response.status != elm_model::ELM_MGR_STATUS_OK {
        return Err(format!(
            "卸载 BuildBound 模块 {name} 被运行时拒绝: {}",
            response.status
        ));
    }
    Ok(cell)
}

#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
pub(crate) fn reload_build_bound_module_for_test(
    init: &Arc<Task>,
    name: &str,
) -> Result<elm_model::ElmId, String> {
    validate_build_bound_environment(init)?;
    let module = trust_config::build_bound_modules()
        .iter()
        .find(|module| module.name == name)
        .ok_or_else(|| format!("BuildBound 清单中不存在模块 {name}"))?;
    let cell = load_build_bound_module(init, module, current_ebi_arch())?;
    notify_lifecycle_event(ElmLifecycleEvent::CellLoaded { cell });
    Ok(cell)
}

fn current_ebi_arch() -> elm_model::ElmEbiArch {
    #[cfg(target_arch = "riscv64")]
    {
        elm_model::ElmEbiArch::Riscv64
    }
    #[cfg(target_arch = "loongarch64")]
    {
        elm_model::ElmEbiArch::LoongArch64
    }
}

#[allow(dead_code)]
pub(crate) fn register_trust_anchor(
    anchor: elm_model::ElmTrustAnchor,
) -> Result<(), elm_model::ElmTrustError> {
    core::with_core(|core| core.register_trust_anchor(anchor))
}

pub(crate) use core::with_core;
pub(crate) use journal::{ElmJournalBackendOps, JournalError as ElmJournalError};

/// 注册一个由 ELM 运行时自身实现的版本化函数表。
pub(crate) fn register_runtime_api_namespace(
    descriptor: &'static elm_model::ElmApiNamespaceDescriptorV1,
) -> Result<(), api_registry::ApiRegistryError> {
    api_registry::register(descriptor)
}

/// 由子系统 provider 为指定 ELM 登记一个可排空资源。
///
/// 操作表必须属于常驻内核子系统，不能指向可卸载 ELM 镜像。后续 provider 接入只需
/// 在成功创建任务、定时器或回调后调用此入口，并在自然销毁时调用配套释放入口。
#[allow(dead_code)]
pub(crate) fn register_owned_resource(
    owner: elm_model::ElmId,
    generation: elm_model::Generation,
    kind: elm_model::ElmOwnedResourceKind,
    handle: u64,
    ops: elm_model::ElmOwnedResourceOpsV1,
) -> elm_model::ElmResult<u64> {
    if !with_core(|core| core.allows_owned_resource_registration(owner, generation)) {
        return Err(elm_model::ElmError::PermissionDenied);
    }
    owned_resource::register(owner, generation, kind, handle, ops).map_err(map_owned_resource_error)
}

/// 在子系统已经自然销毁资源后解除 ELM 所有权记录。
#[allow(dead_code)]
pub(crate) fn release_owned_resource(
    resource_id: u64,
    owner: elm_model::ElmId,
    generation: elm_model::Generation,
) -> elm_model::ElmResult<()> {
    owned_resource::release(resource_id, owner, generation).map_err(map_owned_resource_error)
}

fn map_owned_resource_error(error: owned_resource::OwnedResourceError) -> elm_model::ElmError {
    match error {
        owned_resource::OwnedResourceError::NotFound => elm_model::ElmError::CellNotFound,
        owned_resource::OwnedResourceError::Invalid
        | owned_resource::OwnedResourceError::StaleGeneration
        | owned_resource::OwnedResourceError::OwnerQuiescing
        | owned_resource::OwnedResourceError::Callback(_)
        | owned_resource::OwnedResourceError::Rollback(_) => elm_model::ElmError::InvalidTransition,
        owned_resource::OwnedResourceError::Duplicate
        | owned_resource::OwnedResourceError::Busy
        | owned_resource::OwnedResourceError::Capacity => elm_model::ElmError::LeaseBusy,
    }
}

static KERNEL_SYMBOL_RUNTIME_HOOKS: ::kernel_symbols::KernelSymbolRuntimeHooksV1 =
    ::kernel_symbols::KernelSymbolRuntimeHooksV1 {
        abi_version: 1,
        struct_size: ::core::mem::size_of::<::kernel_symbols::KernelSymbolRuntimeHooksV1>() as u16,
        reserved: 0,
        register_owned_resource: register_kernel_symbol_owned_resource,
        release_owned_resource: release_kernel_symbol_owned_resource,
    };

fn register_kernel_symbol_owned_resource(
    kind: u32,
    handle: u64,
    ops: ::kernel_symbols::KernelSymbolOwnedResourceOpsV1,
) -> i32 {
    let Some(context) = elm_model::current_context() else {
        return ::kernel_symbols::KERNEL_SYMBOL_RESOURCE_STATUS_UNMANAGED;
    };
    let Some(kind) = elm_model::ElmOwnedResourceKind::from_raw(kind) else {
        return ::kernel_symbols::KERNEL_SYMBOL_RESOURCE_STATUS_FAILED;
    };
    let ops = elm_model::ElmOwnedResourceOpsV1::new(
        convert_kernel_symbol_resource_op(ops.suspend),
        convert_kernel_symbol_resource_op(ops.resume),
        convert_kernel_symbol_resource_op(ops.quiesce),
        convert_kernel_symbol_resource_op(ops.cancel),
        convert_kernel_symbol_resource_op(ops.drain),
        convert_kernel_symbol_resource_op(ops.release),
    );
    match register_owned_resource(context.cell_id, context.generation, kind, handle, ops) {
        Ok(_) => ::kernel_symbols::KERNEL_SYMBOL_RESOURCE_STATUS_TRACKED,
        Err(_) => ::kernel_symbols::KERNEL_SYMBOL_RESOURCE_STATUS_FAILED,
    }
}

fn release_kernel_symbol_owned_resource(kind: u32, handle: u64) -> i32 {
    let Some(context) = elm_model::current_context() else {
        return ::kernel_symbols::KERNEL_SYMBOL_RESOURCE_STATUS_UNMANAGED;
    };
    let Some(kind) = elm_model::ElmOwnedResourceKind::from_raw(kind) else {
        return ::kernel_symbols::KERNEL_SYMBOL_RESOURCE_STATUS_FAILED;
    };
    match owned_resource::release_by_handle(context.cell_id, context.generation, kind, handle) {
        Ok(()) => ::kernel_symbols::KERNEL_SYMBOL_RESOURCE_STATUS_TRACKED,
        Err(_) => ::kernel_symbols::KERNEL_SYMBOL_RESOURCE_STATUS_FAILED,
    }
}

fn convert_kernel_symbol_resource_op(
    operation: ::kernel_symbols::KernelSymbolOwnedResourceOp,
) -> elm_model::ElmOwnedResourceOp {
    const {
        assert!(::core::mem::size_of::<elm_model::ElmId>() == ::core::mem::size_of::<u64>());
        assert!(::core::mem::size_of::<elm_model::Generation>() == ::core::mem::size_of::<u64>());
    }
    // Safety: ElmId 和 Generation 均为 u64 的 repr(transparent) 包装，返回类型完全相同；
    // Rust 工具链和接口源码摘要还会在装载前验证调用双方来自同一 ABI 构建。
    unsafe {
        ::core::mem::transmute::<
            ::kernel_symbols::KernelSymbolOwnedResourceOp,
            elm_model::ElmOwnedResourceOp,
        >(operation)
    }
}

#[allow(dead_code)]
pub(crate) fn register_journal_backend(
    backend: &'static ElmJournalBackendOps,
    required: bool,
) -> Result<(), ElmJournalError> {
    journal::register_backend(backend, required)
}

fn render_sysfs_file(name: &str) -> String {
    core::with_core(|core| core.sysfs_text(name))
}

//! ELM（可拓展内核单元）内核核心。
//!
//! 本模块只实现 ELM 自己的枢纽连接层和管理入口，不复用 Linux 模块系统调用。

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;

use elm_model::{
    ELM_MGR_BUILTIN_ID, ElmEbiLoadStatus, ElmEbiSourceKind, ElmPrincipal, ElmResourceBudget,
    ElmSliceImageReader, canonical_ebi_digest, sha256,
};
use sched::Task;

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
            executor::start_provider_worker();
        }
        Err(err) => log::error!("[elm] init builtin elm-mgr failed: {:?}", err),
    }
}

pub(crate) fn load_build_bound_modules(init: &Arc<Task>) -> Result<usize, String> {
    let modules = trust_config::build_bound_modules();
    if modules.is_empty() {
        return Ok(0);
    }
    let current_profile = kernel_interface_profile_hash()?;
    if current_profile != trust_config::build_profile_hash() {
        return Err("BuildBound Profile 与当前内核符号目录不一致".to_string());
    }
    let manifest = crate::user::load_file_from_task_vfs(init, "/lib/elm/modules.manifest")
        .map_err(|err| format!("读取 BuildBound 清单失败: {err:?}"))?;
    if sha256(&manifest) != trust_config::build_manifest_hash() {
        return Err("initramfs 中的 BuildBound 清单摘要不匹配".to_string());
    }

    let arch = current_ebi_arch();
    let mut loaded = 0usize;
    for module in modules {
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
        let budget = ElmResourceBudget::DEFAULT;
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
        loaded += 1;
        log::info!(
            "[elm] BuildBound module active: name={} cell={} order={}",
            module.name,
            response.cell_id,
            module.order
        );
    }
    Ok(loaded)
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

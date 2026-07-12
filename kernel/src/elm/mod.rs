//! ELM（可拓展内核单元）内核核心。
//!
//! 本模块只实现 ELM 自己的枢纽连接层和管理入口，不复用 Linux 模块系统调用。

use alloc::string::String;

mod api_registry;
mod core;
mod event;
mod executor;
mod journal;
mod memory_api;
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

pub(crate) fn init_builtin_mgr() {
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
    if !api_registry::init() {
        log::error!("[elm] Kernel API 注册表初始化失败");
        return;
    }
    if let Err(err) = memory_api::init() {
        log::error!("[elm] kernel.memory@1 注册失败: {:?}", err);
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

#[allow(dead_code)]
pub(crate) fn register_trust_anchor(
    anchor: elm_model::ElmTrustAnchor,
) -> Result<(), elm_model::ElmTrustError> {
    core::with_core(|core| core.register_trust_anchor(anchor))
}

pub(crate) use core::with_core;
pub(crate) use journal::{ElmJournalBackendOps, JournalError as ElmJournalError};

/// 注册一个由常驻内核子系统实现的版本化 Kernel API 函数表。
pub(crate) fn register_kernel_api_namespace(
    descriptor: &'static elm_model::ElmApiNamespaceDescriptorV1,
) -> Result<(), api_registry::ApiRegistryError> {
    if descriptor.flags & elm_model::ELM_API_NAMESPACE_FLAG_REQUIRE_GRANT != 0 {
        let address = descriptor.table_address as usize;
        if descriptor.table_size < ::core::mem::size_of::<kernel_api::ApiTableHeaderV1>() as u32
            || address % ::core::mem::align_of::<kernel_api::ApiTableHeaderV1>() != 0
        {
            return Err(api_registry::ApiRegistryError::InvalidDescriptor);
        }
        // Safety: 描述符已经通过非空、尺寸和对齐检查，且只允许常驻静态函数表注册。
        let header = unsafe { &*(descriptor.table_address as *const kernel_api::ApiTableHeaderV1) };
        if header.struct_size != descriptor.table_size
            || header.abi_version != descriptor.version
            || header.reserved0 != 0
            || header.capabilities != descriptor.capabilities
        {
            return Err(api_registry::ApiRegistryError::InvalidDescriptor);
        }
    }
    api_registry::register(descriptor)
}

/// 在 Kernel API 入口处验证调用令牌并返回当前 ELM 身份。
///
/// 子系统 thunk 必须先进入 `ElmExecutionDomain::KernelCall`，再调用本函数；授权成功后仍要
/// 按具体资源执行所有权、参数范围和预算检查。函数表地址和令牌数值都不是权限真值。
#[allow(dead_code)]
pub(crate) fn authorize_kernel_api_call(
    token: kernel_api::ApiGrantTokenV1,
    identifier: &'static str,
    version: u16,
    required_capabilities: u64,
) -> Result<elm_model::ElmCurrentContext, api_registry::ApiRegistryError> {
    let context =
        elm_model::current_context().ok_or(api_registry::ApiRegistryError::CapabilityDenied)?;
    if !token.is_well_formed() || token.generation() != context.generation.0 {
        return Err(api_registry::ApiRegistryError::CapabilityDenied);
    }
    api_registry::authorize(
        token.grant_id(),
        context.cell_id,
        context.generation,
        identifier.as_bytes(),
        version,
        required_capabilities,
    )?;
    Ok(context)
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
        | owned_resource::OwnedResourceError::Callback(_) => elm_model::ElmError::InvalidTransition,
        owned_resource::OwnedResourceError::Duplicate
        | owned_resource::OwnedResourceError::Busy
        | owned_resource::OwnedResourceError::Capacity => elm_model::ElmError::LeaseBusy,
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

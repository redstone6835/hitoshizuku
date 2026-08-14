//! 内核符号级 Mixin 站点目录和零分配处理链。
//!
//! 装载、替换和卸载路径在 ELM Core 锁外准备镜像，在提交阶段重建受影响站点的不可变
//! 处理链。热路径只读取站点自身的原子路由槽，不读取 Core、注册表或动态容器。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::mem::{align_of, size_of};
use core::slice;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use elm_model::{
    ElmEbiKernelMixinDecl, ElmEbiLoadStatus, ElmEbiUnit, ElmId, ElmKernelMixinKind, Generation,
    sha256,
};
use kernel_symbols::{
    KERNEL_MIXIN_DISPATCH_INVALID, KERNEL_MIXIN_DISPATCH_OK, KERNEL_MIXIN_FRAME_CANCELLED,
    KERNEL_MIXIN_FRAME_FAULTED, KERNEL_MIXIN_FRAME_RESULT_READY, KERNEL_MIXIN_FRAME_STOP,
    KERNEL_MIXIN_HANDLER_FLAG_AUTO_CONTINUE, KERNEL_MIXIN_HANDLER_FLAG_CONTINUATION,
    KERNEL_MIXIN_HANDLER_RUST_ABI_V1, KernelMixinFrameV1, KernelMixinRuntimeHooksV1,
    KernelMixinSiteDescriptorV1,
};
use sched::sync::Spinlock;

use super::native::{LoadedElmImage, NativeExecutionBounds, invoke_kernel_mixin_handler};

unsafe extern "C" {
    static __elm_kernel_mixin_sites_start: u8;
    static __elm_kernel_mixin_sites_end: u8;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KernelMixinError {
    MalformedCatalog,
    DuplicateSite,
    InvalidDeclaration,
    SiteNotFound,
    HandlerNotFound,
    Conflict,
    Capacity,
}

struct HandlerState {
    disabled: AtomicBool,
    fault_count: AtomicU64,
}

impl HandlerState {
    const fn new() -> Self {
        Self {
            disabled: AtomicBool::new(false),
            fault_count: AtomicU64::new(0),
        }
    }

    fn disable(&self) {
        self.fault_count.fetch_add(1, Ordering::Relaxed);
        self.disabled.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
struct RegisteredHandler {
    owner: ElmId,
    generation: Generation,
    site: &'static KernelMixinSiteDescriptorV1,
    handler_symbol: String,
    handler_address: usize,
    kind: ElmKernelMixinKind,
    flags: u16,
    priority: i32,
    bounds: NativeExecutionBounds,
    state: Arc<HandlerState>,
}

#[derive(Clone)]
struct RouteEntry {
    owner: ElmId,
    generation: Generation,
    handler_address: usize,
    kind: ElmKernelMixinKind,
    flags: u16,
    priority: i32,
    bounds: NativeExecutionBounds,
    state: Arc<HandlerState>,
}

struct RouteSnapshot {
    site_hash: [u8; 32],
    entries: Box<[RouteEntry]>,
}

#[derive(Default)]
struct Registry {
    handlers: Vec<RegisteredHandler>,
    suspended_handlers: Vec<RegisteredHandler>,
}

/// 已完成镜像符号解析、可在提交阶段一次安装的处理器集合。
pub(crate) struct PreparedKernelMixins {
    handlers: Vec<RegisteredHandler>,
}

/// 暂停或卸载事务移出的处理器集合；失败路径可以原样恢复。
pub(crate) struct SuspendedKernelMixins {
    owner: ElmId,
    generation: Generation,
    was_active: bool,
}

static REGISTRY: Spinlock<Registry> = Spinlock::new(Registry {
    handlers: Vec::new(),
    suspended_handlers: Vec::new(),
});
static ACTIVE_READERS: AtomicUsize = AtomicUsize::new(0);

struct HexDigest<'a>(&'a [u8; 32]);

impl fmt::Display for HexDigest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

pub(crate) static RUNTIME_HOOKS: KernelMixinRuntimeHooksV1 = KernelMixinRuntimeHooksV1 {
    abi_version: 1,
    struct_size: size_of::<KernelMixinRuntimeHooksV1>() as u16,
    flags: 0,
    dispatch: dispatch,
};

fn catalog() -> Result<&'static [KernelMixinSiteDescriptorV1], KernelMixinError> {
    let start = core::ptr::addr_of!(__elm_kernel_mixin_sites_start) as usize;
    let end = core::ptr::addr_of!(__elm_kernel_mixin_sites_end) as usize;
    let bytes = end
        .checked_sub(start)
        .ok_or(KernelMixinError::MalformedCatalog)?;
    if start % align_of::<KernelMixinSiteDescriptorV1>() != 0
        || bytes % size_of::<KernelMixinSiteDescriptorV1>() != 0
    {
        return Err(KernelMixinError::MalformedCatalog);
    }
    let count = bytes / size_of::<KernelMixinSiteDescriptorV1>();
    // Safety: 链接脚本围住完整只读描述符数组；上面已经验证地址、对齐和元素长度。
    Ok(unsafe { slice::from_raw_parts(start as *const KernelMixinSiteDescriptorV1, count) })
}

pub(crate) fn validate_catalog() -> Result<(), KernelMixinError> {
    let sites = catalog()?;
    for (index, site) in sites.iter().enumerate() {
        if !site.valid()
            || site.source_hash != kernel_symbols::KERNEL_INTERFACE_SOURCE_SHA256
            || site.has_handlers()
        {
            return Err(KernelMixinError::MalformedCatalog);
        }
        if sites[..index].iter().any(|previous| {
            previous.site_hash == site.site_hash
                || previous.api_path == site.api_path
                    && previous.selector == site.selector
                    && previous.ordinal == site.ordinal
        }) {
            return Err(KernelMixinError::DuplicateSite);
        }
    }
    Ok(())
}

pub(crate) fn prepare(
    owner: ElmId,
    generation: Generation,
    unit: &ElmEbiUnit,
    loaded: &LoadedElmImage,
    profile_hash: [u8; 32],
) -> Result<PreparedKernelMixins, KernelMixinError> {
    if unit.kernel_mixins.is_empty() {
        return Ok(PreparedKernelMixins {
            handlers: Vec::new(),
        });
    }
    let bounds = loaded
        .execution_bounds()
        .map_err(|_| KernelMixinError::HandlerNotFound)?;
    let mut handlers = Vec::new();
    handlers
        .try_reserve_exact(unit.kernel_mixins.len())
        .map_err(|_| KernelMixinError::Capacity)?;
    let expected_handler_abi = sha256(KERNEL_MIXIN_HANDLER_RUST_ABI_V1.as_bytes());
    for (index, declaration) in unit.kernel_mixins.iter().enumerate() {
        if let Err(status) = declaration.validate() {
            log::error!(
                "[elm][kernel-mixin] 声明结构无效 index={} target={} selector={} handler={} status={:?}",
                index,
                declaration.target_api,
                declaration.selector,
                declaration.handler_symbol,
                status
            );
            return Err(KernelMixinError::InvalidDeclaration);
        }
        if declaration.profile_hash != profile_hash
            || declaration.handler_abi_hash != expected_handler_abi
        {
            log::error!(
                "[elm][kernel-mixin] 声明 ABI 身份不匹配 index={} target={} selector={} profile={} expected_profile={} handler_abi={} expected_handler_abi={}",
                index,
                declaration.target_api,
                declaration.selector,
                HexDigest(&declaration.profile_hash),
                HexDigest(&profile_hash),
                HexDigest(&declaration.handler_abi_hash),
                HexDigest(&expected_handler_abi)
            );
            return Err(KernelMixinError::InvalidDeclaration);
        }
        let site = match resolve_site(declaration) {
            Ok(site) => site,
            Err(error) => {
                log::error!(
                    "[elm][kernel-mixin] 站点解析失败 index={} target={} selector={} ordinal={} kind={:?} error={:?}",
                    index,
                    declaration.target_api,
                    declaration.selector,
                    declaration.ordinal,
                    declaration.kind,
                    error
                );
                return Err(error);
            }
        };
        let handler_address = match loaded.kernel_mixin_handler_for_decl(declaration) {
            Ok(address) => address,
            Err(status) => {
                log::error!(
                    "[elm][kernel-mixin] 处理器符号不可执行 index={} handler={} status={:?}",
                    index,
                    declaration.handler_symbol,
                    status
                );
                return Err(KernelMixinError::HandlerNotFound);
            }
        };
        handlers.push(RegisteredHandler {
            owner,
            generation,
            site,
            handler_symbol: declaration.handler_symbol.clone(),
            handler_address,
            kind: declaration.kind,
            flags: declaration.flags,
            priority: declaration.priority,
            bounds,
            state: Arc::new(HandlerState::new()),
        });
    }
    Ok(PreparedKernelMixins { handlers })
}

fn resolve_site(
    declaration: &ElmEbiKernelMixinDecl,
) -> Result<&'static KernelMixinSiteDescriptorV1, KernelMixinError> {
    let catalog = catalog()?;
    let mut selected = None;
    for site in catalog.iter().filter(|site| {
        site.api_path == declaration.target_api
            && site.selector == declaration.selector
            && site.ordinal == declaration.ordinal
            && site.source_hash == declaration.source_hash
            && site.function_hash == declaration.function_hash
            && site.site_hash == declaration.site_hash
            && site.frame_abi_hash == declaration.frame_abi_hash
            && declaration.kind.accepts_site(site.kind)
    }) {
        if selected.replace(site).is_some() {
            return Err(KernelMixinError::DuplicateSite);
        }
    }
    if let Some(site) = selected {
        return Ok(site);
    }

    let mut candidates = 0usize;
    for site in catalog.iter().filter(|site| {
        site.api_path == declaration.target_api
            && site.selector == declaration.selector
            && site.ordinal == declaration.ordinal
    }) {
        candidates += 1;
        log::error!(
            "[elm][kernel-mixin] 候选站点身份不匹配 target={} selector={} ordinal={} source_match={} function_match={} site_match={} frame_abi_match={} kind_match={} declared_source={} actual_source={} declared_function={} actual_function={} declared_site={} actual_site={} declared_frame_abi={} actual_frame_abi={}",
            declaration.target_api,
            declaration.selector,
            declaration.ordinal,
            site.source_hash == declaration.source_hash,
            site.function_hash == declaration.function_hash,
            site.site_hash == declaration.site_hash,
            site.frame_abi_hash == declaration.frame_abi_hash,
            declaration.kind.accepts_site(site.kind),
            HexDigest(&declaration.source_hash),
            HexDigest(&site.source_hash),
            HexDigest(&declaration.function_hash),
            HexDigest(&site.function_hash),
            HexDigest(&declaration.site_hash),
            HexDigest(&site.site_hash),
            HexDigest(&declaration.frame_abi_hash),
            HexDigest(&site.frame_abi_hash)
        );
    }
    if candidates == 0 {
        log::error!(
            "[elm][kernel-mixin] 目录中不存在基础站点 target={} selector={} ordinal={} catalog_sites={}",
            declaration.target_api,
            declaration.selector,
            declaration.ordinal,
            catalog.len()
        );
    }
    Err(KernelMixinError::SiteNotFound)
}

pub(crate) fn install(prepared: &PreparedKernelMixins) -> Result<(), KernelMixinError> {
    if prepared.handlers.is_empty() {
        return Ok(());
    }
    let mut registry = REGISTRY.lock();
    if prepared.handlers.iter().any(|handler| {
        registry
            .handlers
            .iter()
            .chain(registry.suspended_handlers.iter())
            .any(|current| {
                current.owner == handler.owner
                    && current.generation == handler.generation
                    && current.handler_symbol == handler.handler_symbol
            })
    }) {
        return Err(KernelMixinError::Conflict);
    }
    let mut next = registry.handlers.clone();
    next.try_reserve(prepared.handlers.len())
        .map_err(|_| KernelMixinError::Capacity)?;
    next.extend(prepared.handlers.iter().cloned());
    publish(&mut registry, next)
}

pub(crate) fn replace(
    owner: ElmId,
    old_generation: Generation,
    prepared: &PreparedKernelMixins,
    active: bool,
) -> Result<(), KernelMixinError> {
    if prepared
        .handlers
        .iter()
        .any(|handler| handler.owner != owner || handler.generation == old_generation)
    {
        return Err(KernelMixinError::InvalidDeclaration);
    }
    let mut registry = REGISTRY.lock();
    if registry
        .handlers
        .iter()
        .any(|handler| handler.owner == owner)
    {
        return Err(KernelMixinError::Conflict);
    }
    let mut next = registry
        .handlers
        .iter()
        .filter(|handler| handler.owner != owner)
        .cloned()
        .collect::<Vec<_>>();
    if active {
        next.try_reserve(prepared.handlers.len())
            .map_err(|_| KernelMixinError::Capacity)?;
        next.extend(prepared.handlers.iter().cloned());
    } else {
        registry
            .suspended_handlers
            .try_reserve(prepared.handlers.len())
            .map_err(|_| KernelMixinError::Capacity)?;
    }
    publish(&mut registry, next)?;
    if !active {
        registry
            .suspended_handlers
            .extend(prepared.handlers.iter().cloned());
    }
    Ok(())
}

pub(crate) fn suspend(
    owner: ElmId,
    generation: Generation,
) -> Result<SuspendedKernelMixins, KernelMixinError> {
    let mut registry = REGISTRY.lock();
    let suspended = registry
        .handlers
        .iter()
        .filter(|handler| handler.owner == owner && handler.generation == generation)
        .cloned()
        .collect::<Vec<_>>();
    if suspended.is_empty() {
        return Ok(SuspendedKernelMixins {
            owner,
            generation,
            was_active: false,
        });
    }
    registry
        .suspended_handlers
        .try_reserve(suspended.len())
        .map_err(|_| KernelMixinError::Capacity)?;
    let next = registry
        .handlers
        .iter()
        .filter(|handler| handler.owner != owner || handler.generation != generation)
        .cloned()
        .collect::<Vec<_>>();
    publish(&mut registry, next)?;
    registry.suspended_handlers.extend(suspended);
    Ok(SuspendedKernelMixins {
        owner,
        generation,
        was_active: true,
    })
}

impl SuspendedKernelMixins {
    pub(crate) fn restore(self) -> Result<(), KernelMixinError> {
        if !self.was_active {
            return Ok(());
        }
        let mut registry = REGISTRY.lock();
        let restored = registry
            .suspended_handlers
            .iter()
            .filter(|handler| handler.owner == self.owner && handler.generation == self.generation)
            .cloned()
            .collect::<Vec<_>>();
        if restored.is_empty() {
            return Err(KernelMixinError::Conflict);
        }
        let mut next = registry.handlers.clone();
        next.try_reserve(restored.len())
            .map_err(|_| KernelMixinError::Capacity)?;
        next.extend(restored);
        publish(&mut registry, next)?;
        registry
            .suspended_handlers
            .retain(|handler| handler.owner != self.owner || handler.generation != self.generation);
        Ok(())
    }

    pub(crate) fn retire(self) {
        let mut registry = REGISTRY.lock();
        registry
            .suspended_handlers
            .retain(|handler| handler.owner != self.owner || handler.generation != self.generation);
    }
}

pub(crate) fn resume(owner: ElmId, generation: Generation) -> Result<(), KernelMixinError> {
    let mut registry = REGISTRY.lock();
    let restored = registry
        .suspended_handlers
        .iter()
        .filter(|handler| handler.owner == owner && handler.generation == generation)
        .cloned()
        .collect::<Vec<_>>();
    if restored.is_empty() {
        return Ok(());
    }
    let mut next = registry.handlers.clone();
    next.try_reserve(restored.len())
        .map_err(|_| KernelMixinError::Capacity)?;
    next.extend(restored);
    publish(&mut registry, next)?;
    registry
        .suspended_handlers
        .retain(|handler| handler.owner != owner || handler.generation != generation);
    Ok(())
}

pub(crate) fn rollback_replace(
    suspended: SuspendedKernelMixins,
    new_generation: Generation,
    restore_old: bool,
) -> Result<(), KernelMixinError> {
    let mut registry = REGISTRY.lock();
    let old = registry
        .suspended_handlers
        .iter()
        .filter(|handler| {
            handler.owner == suspended.owner && handler.generation == suspended.generation
        })
        .cloned()
        .collect::<Vec<_>>();
    if suspended.was_active && restore_old && old.is_empty() {
        return Err(KernelMixinError::Conflict);
    }
    let mut next = registry
        .handlers
        .iter()
        .filter(|handler| handler.owner != suspended.owner || handler.generation != new_generation)
        .cloned()
        .collect::<Vec<_>>();
    if suspended.was_active && restore_old {
        next.try_reserve(old.len())
            .map_err(|_| KernelMixinError::Capacity)?;
        next.extend(old);
    }
    publish(&mut registry, next)?;
    registry.suspended_handlers.retain(|handler| {
        let old_generation = handler.owner == suspended.owner
            && handler.generation == suspended.generation
            && suspended.was_active
            && restore_old;
        let new_generation =
            handler.owner == suspended.owner && handler.generation == new_generation;
        !old_generation && !new_generation
    });
    Ok(())
}

pub(crate) fn commit_replace(suspended: SuspendedKernelMixins, new_generation: Generation) {
    let mut registry = REGISTRY.lock();
    let old_active = registry.handlers.iter().any(|handler| {
        handler.owner == suspended.owner && handler.generation == suspended.generation
    });
    let new_active = registry
        .handlers
        .iter()
        .any(|handler| handler.owner == suspended.owner && handler.generation == new_generation);
    let new_suspended = registry
        .suspended_handlers
        .iter()
        .any(|handler| handler.owner == suspended.owner && handler.generation == new_generation);
    debug_assert!(!old_active && !(new_active && new_suspended));
    registry.suspended_handlers.retain(|handler| {
        handler.owner != suspended.owner || handler.generation != suspended.generation
    });
}

fn publish(registry: &mut Registry, next: Vec<RegisteredHandler>) -> Result<(), KernelMixinError> {
    let mut sites = Vec::new();
    sites
        .try_reserve(registry.handlers.len().saturating_add(next.len()))
        .map_err(|_| KernelMixinError::Capacity)?;
    for site in registry
        .handlers
        .iter()
        .chain(next.iter())
        .map(|handler| handler.site)
    {
        if !sites
            .iter()
            .any(|current: &&KernelMixinSiteDescriptorV1| current.site_hash == site.site_hash)
        {
            sites.push(site);
        }
    }

    let mut updates = Vec::new();
    updates
        .try_reserve_exact(sites.len())
        .map_err(|_| KernelMixinError::Capacity)?;
    let mut retired = Vec::new();
    retired
        .try_reserve_exact(sites.len())
        .map_err(|_| KernelMixinError::Capacity)?;
    for site in sites {
        let mut entries = next
            .iter()
            .filter(|handler| handler.site.site_hash == site.site_hash)
            .map(|handler| RouteEntry {
                owner: handler.owner,
                generation: handler.generation,
                handler_address: handler.handler_address,
                kind: handler.kind,
                flags: handler.flags,
                priority: handler.priority,
                bounds: handler.bounds,
                state: Arc::clone(&handler.state),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.owner.0.cmp(&right.owner.0))
                .then_with(|| left.generation.0.cmp(&right.generation.0))
                .then_with(|| left.handler_address.cmp(&right.handler_address))
        });
        let pointer = if entries.is_empty() {
            core::ptr::null_mut()
        } else {
            Box::into_raw(Box::new(RouteSnapshot {
                site_hash: site.site_hash,
                entries: entries.into_boxed_slice(),
            }))
            .cast::<()>()
        };
        updates.push((site, pointer));
    }

    for (site, pointer) in &updates {
        retired.push(site.route.swap(*pointer, Ordering::AcqRel));
    }
    kernel_symbols::publish_mixin_runtime_active(!next.is_empty());
    registry.handlers = next;
    while ACTIVE_READERS.load(Ordering::Acquire) != 0 {
        core::hint::spin_loop();
    }
    for pointer in retired {
        if !pointer.is_null() {
            // Safety: 指针来自上一次 `Box::into_raw`，且读侧计数归零后不再存在引用。
            unsafe { drop(Box::from_raw(pointer.cast::<RouteSnapshot>())) };
        }
    }
    Ok(())
}

struct ReaderGuard;

impl ReaderGuard {
    fn enter() -> Self {
        ACTIVE_READERS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        ACTIVE_READERS.fetch_sub(1, Ordering::Release);
    }
}

unsafe extern "C" fn dispatch(
    site: *const KernelMixinSiteDescriptorV1,
    frame: *mut KernelMixinFrameV1,
) -> i32 {
    if site.is_null()
        || frame.is_null()
        || general::elm_guard::active_phase() == general::elm_guard::ELM_GUARD_PHASE_KERNEL_MIXIN
    {
        return KERNEL_MIXIN_DISPATCH_INVALID;
    }
    // Safety: 两个指针都由内核导出函数包装器在同步调用期间提供。
    let (site, frame) = unsafe { (&*site, &mut *frame) };
    if !site.valid() || !frame.valid() || site.kind != frame.site_kind {
        return KERNEL_MIXIN_DISPATCH_INVALID;
    }
    let _reader = ReaderGuard::enter();
    let route = site.route.load(Ordering::Acquire);
    if route.is_null() {
        return kernel_symbols::KERNEL_MIXIN_DISPATCH_UNHANDLED;
    }
    // Safety: 发布者只在读侧计数归零后回收该不可变快照。
    let route = unsafe { &*route.cast::<RouteSnapshot>() };
    if route.site_hash != site.site_hash {
        return KERNEL_MIXIN_DISPATCH_INVALID;
    }
    dispatch_chain(route, 0, frame)
}

struct ContinuationContext {
    route: *const RouteSnapshot,
    index: usize,
}

unsafe extern "C" fn continue_chain(context: *mut (), frame: *mut KernelMixinFrameV1) -> i32 {
    if context.is_null() || frame.is_null() {
        return KERNEL_MIXIN_DISPATCH_INVALID;
    }
    // Safety: 上下文和帧都位于仍未返回的 `dispatch_chain` 栈帧中。
    let context = unsafe { &mut *context.cast::<ContinuationContext>() };
    // Safety: 路由快照受最外层 ReaderGuard 保护，帧仍属于同一次同步调用。
    unsafe { dispatch_chain(&*context.route, context.index, &mut *frame) }
}

fn dispatch_chain(route: &RouteSnapshot, mut index: usize, frame: &mut KernelMixinFrameV1) -> i32 {
    while index < route.entries.len() && route.entries[index].state.disabled.load(Ordering::Acquire)
    {
        index += 1;
    }
    let Some(entry) = route.entries.get(index) else {
        return if frame.original.is_some() {
            // Safety: 原逻辑 continuation 由目标包装器绑定，并仍处于同一同步调用栈中。
            unsafe { frame.call_original() }
        } else {
            KERNEL_MIXIN_DISPATCH_OK
        };
    };

    let continuation = entry.flags & KERNEL_MIXIN_HANDLER_FLAG_CONTINUATION != 0;
    let auto_continue = entry.flags & KERNEL_MIXIN_HANDLER_FLAG_AUTO_CONTINUE != 0;
    if continuation == auto_continue || entry.flags != entry.kind.required_flags() {
        entry.state.disable();
        return dispatch_chain(route, index + 1, frame);
    }

    let mut next = ContinuationContext {
        route: core::ptr::from_ref(route),
        index: index + 1,
    };
    frame.next = continuation.then_some(continue_chain);
    frame.next_context = if continuation {
        core::ptr::from_mut(&mut next).cast()
    } else {
        core::ptr::null_mut()
    };
    let status = invoke_kernel_mixin_handler(
        entry.owner,
        entry.generation,
        entry.handler_address,
        entry.bounds,
        frame,
    );
    let continuation_consumed = continuation && frame.next.is_none();
    frame.next = None;
    frame.next_context = core::ptr::null_mut();

    if status != KERNEL_MIXIN_DISPATCH_OK {
        entry.state.disable();
        frame.flags |= KERNEL_MIXIN_FRAME_FAULTED;
        return if frame.flags & KERNEL_MIXIN_FRAME_RESULT_READY != 0 {
            KERNEL_MIXIN_DISPATCH_OK
        } else {
            dispatch_chain(route, index + 1, frame)
        };
    }
    if frame.flags & KERNEL_MIXIN_FRAME_CANCELLED != 0 {
        if frame.flags & KERNEL_MIXIN_FRAME_RESULT_READY != 0 {
            return KERNEL_MIXIN_DISPATCH_OK;
        }
        entry.state.disable();
        frame.flags &= !KERNEL_MIXIN_FRAME_CANCELLED;
        return dispatch_chain(route, index + 1, frame);
    }
    if auto_continue {
        if frame.flags & KERNEL_MIXIN_FRAME_STOP != 0 {
            frame.flags &= !KERNEL_MIXIN_FRAME_STOP;
            return dispatch_chain(route, route.entries.len(), frame);
        }
        return dispatch_chain(route, index + 1, frame);
    }
    if continuation_consumed || frame.flags & KERNEL_MIXIN_FRAME_RESULT_READY != 0 {
        KERNEL_MIXIN_DISPATCH_OK
    } else {
        entry.state.disable();
        dispatch_chain(route, index + 1, frame)
    }
}

pub(crate) fn map_load_status(error: KernelMixinError) -> ElmEbiLoadStatus {
    match error {
        KernelMixinError::Capacity => ElmEbiLoadStatus::RuntimeRejected,
        KernelMixinError::MalformedCatalog
        | KernelMixinError::DuplicateSite
        | KernelMixinError::InvalidDeclaration
        | KernelMixinError::SiteNotFound
        | KernelMixinError::HandlerNotFound
        | KernelMixinError::Conflict => ElmEbiLoadStatus::InvalidManifest,
    }
}

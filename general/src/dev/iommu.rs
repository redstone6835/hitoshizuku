//! 通用 IOMMU controller 与 DMA domain 注册层。
//!
//! 固件枚举层只把最终 IOMMU provider phandle、已应用 nexus map 的 specifier
//! 和 requester 身份交给本模块。具体 IOMMU ELM 负责解释 specifier 并创建隔离域；
//! consumer 获得的则始终是由常驻 `general` 层实现的 [`DmaMapper`] 包装对象。
//!
//! controller attach 和 domain 方法都在 registry 锁外调用。registry 分别跟踪
//! attach in-flight 与 active domain：前者保护 attach vtable，后者保护 domain
//! vtable。包装 mapper 析构时先销毁具体 domain，再递减 active 计数，因此 ELM
//! finalize 只有在不可能再进入模块代码后才能成功注销 controller。

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vfs::sync::Spinlock;

use super::dma::{DmaConstraints, DmaContext, DmaMappedRegion, DmaMapper, DmaSyncRegion};
use super::dt_provider::{DtbProviderKey, DtbProviderKind};
use super::pnp::{
    self, PnpDependency, PnpResource, PnpResourceKind, PnpResourceReleaseError,
    PnpResourceReleaseOrder,
};
use super::registry_id;

/// requester 所在的总线命名空间。
///
/// 使用数值 newtype 而不是封闭枚举，避免新增总线类型时改变跨 ELM 接口。零值表示
/// 调用方没有更具体的总线分类；controller 仍必须以 specifier 作为硬件域标识依据。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct IommuRequesterKind(u32);

impl IommuRequesterKind {
    pub const UNSPECIFIED: Self = Self(0);
    pub const PLATFORM: Self = Self(1);
    pub const PCI: Self = Self(2);

    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// 不借用 PnP、DT 或 PCI 临时对象的稳定 requester 描述。
///
/// `segment` 是总线实例或固件命名空间，`id` 是该命名空间中的 requester 标识。
/// PCI 使用 segment 与 16-bit requester ID；platform consumer 通常使用 DT phandle
/// 或 PnP runtime ID。硬件 stream ID 等 binding-specific 数据仍保留在 specifier 中。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IommuRequester {
    kind: IommuRequesterKind,
    segment: u32,
    id: u64,
}

impl IommuRequester {
    pub const fn new(kind: IommuRequesterKind, segment: u32, id: u64) -> Self {
        Self { kind, segment, id }
    }

    pub const fn platform(id: u64) -> Self {
        Self::new(IommuRequesterKind::PLATFORM, 0, id)
    }

    pub const fn pci(segment: u16, requester_id: u16) -> Self {
        Self::new(IommuRequesterKind::PCI, segment as u32, requester_id as u64)
    }

    pub const fn kind(self) -> IommuRequesterKind {
        self.kind
    }

    pub const fn segment(self) -> u32 {
        self.segment
    }

    pub const fn id(self) -> u64 {
        self.id
    }
}

/// 交给 IOMMU controller 的拥有型 attach 请求。
///
/// registry 在调用 ELM 前复制 specifier，controller 因而不会借用固件树、PCI map
/// 或调用者栈上的临时切片。controller 可以在本次同步 attach 期间自由拆分该请求。
pub struct IommuAttachRequest {
    requester: IommuRequester,
    specifier: Box<[u32]>,
}

/// 多 path 设备中一条 IOMMU provider 引用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IommuAttachment {
    phandle: u32,
    specifier: Box<[u32]>,
}

impl IommuAttachment {
    pub const fn new(phandle: u32, specifier: Box<[u32]>) -> Self {
        Self { phandle, specifier }
    }

    pub const fn phandle(&self) -> u32 {
        self.phandle
    }

    pub fn specifier(&self) -> &[u32] {
        &self.specifier
    }
}

impl IommuAttachRequest {
    pub fn from_boxed(requester: IommuRequester, specifier: Box<[u32]>) -> Self {
        Self {
            requester,
            specifier,
        }
    }

    pub fn try_from_cells(
        requester: IommuRequester,
        specifier: &[u32],
    ) -> Result<Self, IommuError> {
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(specifier.len())
            .map_err(|_| IommuError::OutOfMemory)?;
        owned.extend_from_slice(specifier);
        Ok(Self::from_boxed(requester, owned.into_boxed_slice()))
    }

    pub const fn requester(&self) -> IommuRequester {
        self.requester
    }

    pub fn specifier(&self) -> &[u32] {
        &self.specifier
    }

    pub fn into_parts(self) -> (IommuRequester, Box<[u32]>) {
        (self.requester, self.specifier)
    }
}

/// controller registry 或 domain attach 失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IommuError {
    Invalid,
    AlreadyRegistered,
    NotFound,
    NotReady(u32),
    Busy,
    OutOfMemory,
    InvalidSpecifier,
    AttachFailed,
    Unsupported,
    HardwareFailure,
}

/// 由具体 IOMMU ELM 实现的单 requester DMA domain。
///
/// domain 的 `Drop` 负责撤销硬件上下文以及残余映射。consumer 不直接持有本 trait
/// object；[`attach_iommu`] 会将它包装成 vtable 位于常驻 `general` 的 mapper。
pub trait IommuDomain: DmaMapper {}

/// 由具体 IOMMU ELM 实现的 controller。
pub trait IommuController: Send + Sync {
    /// 为一个拥有型 requester 请求创建 DMA domain。
    ///
    /// 实现不得依赖 registry 锁保护；该方法可以并发执行，也可以在内部串行化硬件
    /// 操作。成功返回的 domain 必须独立拥有 detach 所需的 controller 状态。
    fn attach(&self, request: IommuAttachRequest) -> Result<Arc<dyn IommuDomain>, IommuError>;
}

/// 区分同一 phandle 的不同注册生命周期。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IommuControllerHandle {
    phandle: u32,
    id: u64,
}

impl IommuControllerHandle {
    pub const fn phandle(self) -> u32 {
        self.phandle
    }

    pub const fn id(self) -> u64 {
        self.id
    }
}

struct IommuControllerRegistration {
    handle: IommuControllerHandle,
    controller: Arc<dyn IommuController>,
    attaches_in_flight: usize,
    next_domain_id: u64,
    active_domains: Vec<ActiveIommuDomain>,
    retiring: bool,
}

struct ActiveIommuDomain {
    id: u64,
    consumer: Option<Weak<IommuConsumerState>>,
}

struct IommuRegistry {
    next_id: u64,
    controllers: Vec<IommuControllerRegistration>,
}

impl IommuRegistry {
    const fn new() -> Self {
        Self {
            next_id: 1,
            controllers: Vec::new(),
        }
    }
}

static IOMMU_CONTROLLERS: Spinlock<IommuRegistry> = Spinlock::new(IommuRegistry::new());

/// 以 DT phandle 登记 IOMMU controller。
#[kernel_symbols::export(
    name = "general.dev.iommu.register_iommu_controller",
    contract = "kernel.general.iommu-controller@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DMA
        | kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 2u64
)]
pub fn register_iommu_controller(
    phandle: u32,
    controller: Arc<dyn IommuController>,
) -> Result<IommuControllerHandle, IommuError> {
    if phandle == 0 || phandle == u32::MAX {
        return Err(IommuError::Invalid);
    }

    let mut registry = IOMMU_CONTROLLERS.lock();
    if registry
        .controllers
        .iter()
        .any(|entry| entry.handle.phandle == phandle)
    {
        return Err(IommuError::AlreadyRegistered);
    }
    registry
        .controllers
        .try_reserve(1)
        .map_err(|_| IommuError::OutOfMemory)?;
    let id =
        registry_id::alloc_locked_id(&mut registry.next_id).map_err(|_| IommuError::OutOfMemory)?;
    let handle = IommuControllerHandle { phandle, id };
    registry.controllers.push(IommuControllerRegistration {
        handle,
        controller,
        attaches_in_flight: 0,
        next_domain_id: 1,
        active_domains: Vec::new(),
        retiring: false,
    });
    drop(registry);
    if super::elm_lifecycle::track_iommu_controller(handle).is_err() {
        let _ = unregister_iommu_controller(handle);
        return Err(IommuError::OutOfMemory);
    }
    pnp::notify_dependency_ready(DtbProviderKey::new(DtbProviderKind::Iommu, phandle).dependency());
    Ok(handle)
}

/// 注销 controller。
///
/// attach 正在执行或仍有 mapper 持有 active domain 时返回 [`IommuError::Busy`]。
/// 成功路径在 registry 锁外销毁 controller，避免 ELM destructor 重入 registry。
#[kernel_symbols::export(
    name = "general.dev.iommu.unregister_iommu_controller",
    contract = "kernel.general.iommu-controller@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DMA
        | kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_iommu_controller(handle: IommuControllerHandle) -> Result<(), IommuError> {
    prepare_unregister_iommu_controller(handle)?;
    match commit_unregister_iommu_controller(handle) {
        Ok(()) => Ok(()),
        Err(error) => {
            cancel_unregister_iommu_controller(handle);
            Err(error)
        }
    }
}

pub(crate) fn prepare_unregister_iommu_controller(
    handle: IommuControllerHandle,
) -> Result<(), IommuError> {
    let mut registry = IOMMU_CONTROLLERS.lock();
    let entry = registry
        .controllers
        .iter_mut()
        .find(|entry| entry.handle == handle)
        .ok_or(IommuError::NotFound)?;
    if entry.retiring {
        return Err(IommuError::Busy);
    }
    entry.retiring = true;
    let has_external_domain = entry.active_domains.iter().any(|domain| {
        domain
            .consumer
            .as_ref()
            .and_then(Weak::upgrade)
            .is_none_or(|consumer| !consumer.prepared_for_removal())
    });
    if entry.attaches_in_flight != 0 || has_external_domain {
        entry.retiring = false;
        return Err(IommuError::Busy);
    }
    Ok(())
}

pub(crate) fn cancel_unregister_iommu_controller(handle: IommuControllerHandle) {
    if let Some(entry) = IOMMU_CONTROLLERS
        .lock()
        .controllers
        .iter_mut()
        .find(|entry| entry.handle == handle)
    {
        entry.retiring = false;
    }
}

fn commit_unregister_iommu_controller(handle: IommuControllerHandle) -> Result<(), IommuError> {
    let controller = {
        let mut registry = IOMMU_CONTROLLERS.lock();
        let Some(index) = registry
            .controllers
            .iter()
            .position(|entry| entry.handle == handle)
        else {
            return Err(IommuError::NotFound);
        };
        let entry = &registry.controllers[index];
        if !entry.retiring || entry.attaches_in_flight != 0 || !entry.active_domains.is_empty() {
            return Err(IommuError::Busy);
        }
        registry.controllers.remove(index).controller
    };
    drop(controller);
    super::elm_lifecycle::forget_iommu_controller(handle);
    Ok(())
}

pub(crate) fn can_unregister_iommu_controller(
    handle: IommuControllerHandle,
) -> Result<(), IommuError> {
    let registry = IOMMU_CONTROLLERS.lock();
    let entry = registry
        .controllers
        .iter()
        .find(|entry| entry.handle == handle)
        .ok_or(IommuError::NotFound)?;
    if entry.retiring || entry.attaches_in_flight != 0 || !entry.active_domains.is_empty() {
        Err(IommuError::Busy)
    } else {
        Ok(())
    }
}

fn prepare_iommu_controller(handle: IommuControllerHandle) -> bool {
    matches!(
        prepare_unregister_iommu_controller(handle),
        Ok(()) | Err(IommuError::NotFound)
    )
}

fn cancel_iommu_controller(handle: IommuControllerHandle) {
    cancel_unregister_iommu_controller(handle);
}

fn release_iommu_controller(handle: IommuControllerHandle) -> bool {
    matches!(
        commit_unregister_iommu_controller(handle),
        Ok(()) | Err(IommuError::NotFound)
    )
}

/// IOMMU controller 的事务型 PnP provider 资源。
pub struct IommuControllerPnpResource {
    handle: IommuControllerHandle,
    label: &'static str,
    prepared: AtomicBool,
}

impl PnpResource for IommuControllerPnpResource {
    fn kind(&self) -> PnpResourceKind {
        PnpResourceKind::Other("iommu-controller")
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
        if self.prepared.load(Ordering::Acquire) {
            return Ok(());
        }
        if !prepare_iommu_controller(self.handle) {
            return Err(PnpResourceReleaseError::new(
                self.kind(),
                self.label,
                "IOMMU controller still has an external domain or attach in flight",
            ));
        }
        self.prepared.store(true, Ordering::Release);
        Ok(())
    }

    fn cancel_release(&self) {
        if self.prepared.swap(false, Ordering::AcqRel) {
            cancel_iommu_controller(self.handle);
        }
    }

    fn release_order(&self) -> PnpResourceReleaseOrder {
        PnpResourceReleaseOrder::Provider
    }

    fn provided_dependency(&self) -> Option<PnpDependency> {
        Some(DtbProviderKey::new(DtbProviderKind::Iommu, self.handle.phandle()).dependency())
    }

    fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        if !self.prepared.load(Ordering::Acquire) {
            self.prepare_release()?;
        }
        if release_iommu_controller(self.handle) {
            Ok(())
        } else {
            self.cancel_release();
            Err(PnpResourceReleaseError::new(
                self.kind(),
                self.label,
                "prepared IOMMU controller still has an active domain",
            ))
        }
    }
}

/// 将 controller registration 交给 IOMMU ELM 的 PnP 设备拥有。
#[kernel_symbols::export(
    name = "general.dev.iommu.controller_pnp_resource",
    contract = "kernel.general.iommu-controller@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn controller_pnp_resource(
    handle: IommuControllerHandle,
    label: &'static str,
) -> IommuControllerPnpResource {
    IommuControllerPnpResource {
        handle,
        label,
        prepared: AtomicBool::new(false),
    }
}

/// 在常驻内核侧构造完成类型擦除的 IOMMU provider 资源。
///
/// 动态 ELM 只传递 `Box<dyn PnpResource>`，无需在模块镜像中生成
/// [`IommuControllerPnpResource`] 的私有 trait vtable。
#[kernel_symbols::export(
    name = "general.dev.iommu.controller_pnp_resource_boxed",
    contract = "kernel.general.iommu-controller@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn controller_pnp_resource_boxed(
    handle: IommuControllerHandle,
    label: &'static str,
) -> Box<dyn PnpResource> {
    Box::new(controller_pnp_resource(handle, label))
}

/// 按 controller phandle attach requester，并返回常驻层包装的 per-device mapper。
///
/// 本入口会先把 specifier 复制为拥有型请求。已经持有 `Box<[u32]>` 的调用方可以用
/// [`attach_iommu_owned`] 避免再次复制。
#[kernel_symbols::export(
    name = "general.dev.iommu.attach_iommu",
    contract = "kernel.general.iommu-domain@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DMA
        | kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn attach_iommu(
    phandle: u32,
    requester: IommuRequester,
    specifier: &[u32],
) -> Result<Arc<dyn DmaMapper>, IommuError> {
    let request = IommuAttachRequest::try_from_cells(requester, specifier)?;
    attach_iommu_owned(phandle, request)
}

/// 使用已经拥有的 attach 请求创建 per-device mapper。
pub fn attach_iommu_owned(
    phandle: u32,
    request: IommuAttachRequest,
) -> Result<Arc<dyn DmaMapper>, IommuError> {
    attach_iommu_owned_for_consumer(phandle, request, None)
}

fn attach_iommu_owned_for_consumer(
    phandle: u32,
    request: IommuAttachRequest,
    consumer: Option<Weak<IommuConsumerState>>,
) -> Result<Arc<dyn DmaMapper>, IommuError> {
    if phandle == 0 || phandle == u32::MAX {
        return Err(IommuError::Invalid);
    }

    let (registration_id, domain_id, controller) = {
        let mut registry = IOMMU_CONTROLLERS.lock();
        let entry = registry
            .controllers
            .iter_mut()
            .find(|entry| entry.handle.phandle == phandle && !entry.retiring)
            .ok_or(IommuError::NotReady(phandle))?;
        entry
            .active_domains
            .try_reserve(1)
            .map_err(|_| IommuError::OutOfMemory)?;
        let domain_id = registry_id::alloc_locked_id(&mut entry.next_domain_id)
            .map_err(|_| IommuError::OutOfMemory)?;
        entry.attaches_in_flight = entry
            .attaches_in_flight
            .checked_add(1)
            .ok_or(IommuError::Busy)?;
        (entry.handle.id, domain_id, Arc::clone(&entry.controller))
    };

    let attached = controller.attach(request);
    let mut registry = IOMMU_CONTROLLERS.lock();
    let Some(entry) = registry
        .controllers
        .iter_mut()
        .find(|entry| entry.handle.phandle == phandle && entry.handle.id == registration_id)
    else {
        // unregister 在 in-flight 非零时必须返回 Busy；这里只可能由 registry 损坏触发。
        drop(registry);
        drop(attached);
        return Err(IommuError::NotReady(phandle));
    };
    entry.attaches_in_flight = entry.attaches_in_flight.saturating_sub(1);
    let domain = attached?;
    // attach 前已为本 in-flight 完成项预留容量。
    entry.active_domains.push(ActiveIommuDomain {
        id: domain_id,
        consumer,
    });
    drop(registry);

    Ok(Arc::new(AttachedIommuMapper {
        phandle,
        registration_id,
        domain_id,
        domain: Some(domain),
    }))
}

/// 为一个 requester 的全部标准 `iommus` path 建立共同 DMA mapper。
///
/// 第一条 path 分配 IOVA，其余 path 必须通过 [`DmaMapper::map_region_at`] 映射到
/// 同一地址。任一路径失败都会按逆序撤销已建立映射，不会退化为部分隔离。
pub fn attach_iommu_group(
    requester: IommuRequester,
    attachments: Vec<IommuAttachment>,
) -> Result<Arc<dyn DmaMapper>, IommuError> {
    attach_iommu_group_for_consumer(requester, attachments, None)
}

fn attach_iommu_group_for_consumer(
    requester: IommuRequester,
    attachments: Vec<IommuAttachment>,
    consumer: Option<Weak<IommuConsumerState>>,
) -> Result<Arc<dyn DmaMapper>, IommuError> {
    if attachments.is_empty() {
        return Err(IommuError::Invalid);
    }
    let mut domains = Vec::new();
    domains
        .try_reserve_exact(attachments.len())
        .map_err(|_| IommuError::OutOfMemory)?;
    for attachment in attachments {
        let request = IommuAttachRequest::from_boxed(requester, attachment.specifier);
        domains.push(attach_iommu_owned_for_consumer(
            attachment.phandle,
            request,
            consumer.clone(),
        )?);
    }
    if domains.len() == 1 {
        return Ok(domains.pop().expect("one attached domain remains"));
    }
    Ok(Arc::new(CompositeIommuMapper {
        domains,
        next_token: AtomicU64::new(1),
        mappings: Spinlock::new(CompositeMappingState {
            in_flight: 0,
            records: Vec::new(),
        }),
    }))
}

/// 构造一个在首次 DMA map 时 attach、provider 暂未注册时保持 fail-closed 的 mapper。
///
/// 平台枚举可以在 provider/consumer 文本顺序未知时先发布设备上下文；controller
/// 就绪后同一上下文会自动重试 attach。成功后 mapper 固定到该 domain generation。
pub fn lazy_iommu_group(
    requester: IommuRequester,
    attachments: Vec<IommuAttachment>,
) -> Result<Arc<dyn DmaMapper>, IommuError> {
    let (mapper, _) = new_lazy_iommu_group(requester, attachments)?;
    Ok(mapper)
}

/// 构造带 PnP consumer lease 的延迟 IOMMU DMA 上下文。
///
/// 总线层应优先使用本入口；对应 PnP 设备登记时会从 [`DmaContext`] 取出一次性的
/// bus resource，使 provider+consumer 同一移除事务可以先冻结 consumer，再安全
/// 注销 controller。没有把 lease 交给 PnP 的上下文仍按外部引用处理。
pub fn lazy_iommu_context(
    constraints: DmaConstraints,
    requester: IommuRequester,
    attachments: Vec<IommuAttachment>,
) -> Result<DmaContext, IommuError> {
    let (mapper, lease) = new_lazy_iommu_group(requester, attachments)?;
    Ok(DmaContext::with_iommu_mapper(constraints, mapper, lease))
}

fn new_lazy_iommu_group(
    requester: IommuRequester,
    attachments: Vec<IommuAttachment>,
) -> Result<(Arc<dyn DmaMapper>, IommuConsumerLease), IommuError> {
    if attachments.is_empty()
        || attachments
            .iter()
            .any(|attachment| attachment.phandle == 0 || attachment.phandle == u32::MAX)
    {
        return Err(IommuError::Invalid);
    }
    let state = Arc::new(IommuConsumerState {
        resource_claimed: AtomicBool::new(false),
        inner: Spinlock::new(IommuConsumerInner {
            attachments,
            mapper: None,
            attaching: false,
            operations_in_flight: 0,
            active_mappings: 0,
            retiring: false,
            released: false,
        }),
    });
    let mapper: Arc<dyn DmaMapper> = Arc::new(LazyIommuMapper {
        requester,
        state: Arc::clone(&state),
    });
    Ok((mapper, IommuConsumerLease { state }))
}

struct IommuConsumerInner {
    attachments: Vec<IommuAttachment>,
    mapper: Option<Arc<dyn DmaMapper>>,
    attaching: bool,
    operations_in_flight: usize,
    active_mappings: usize,
    retiring: bool,
    released: bool,
}

struct IommuConsumerState {
    resource_claimed: AtomicBool,
    inner: Spinlock<IommuConsumerInner>,
}

impl IommuConsumerState {
    fn begin_map(&self) -> bool {
        let mut inner = self.inner.lock();
        if inner.retiring || inner.released {
            return false;
        }
        let Some(next) = inner.operations_in_flight.checked_add(1) else {
            return false;
        };
        inner.operations_in_flight = next;
        true
    }

    fn finish_map(&self, mapped: bool) {
        let mut inner = self.inner.lock();
        inner.operations_in_flight = inner.operations_in_flight.saturating_sub(1);
        if mapped {
            inner.active_mappings = inner.active_mappings.saturating_add(1);
        }
    }

    fn begin_unmap(&self) -> bool {
        let mut inner = self.inner.lock();
        if inner.released || inner.active_mappings == 0 {
            return false;
        }
        let Some(next) = inner.operations_in_flight.checked_add(1) else {
            return false;
        };
        inner.operations_in_flight = next;
        true
    }

    fn finish_unmap(&self, unmapped: bool) {
        let mut inner = self.inner.lock();
        inner.operations_in_flight = inner.operations_in_flight.saturating_sub(1);
        if unmapped {
            inner.active_mappings = inner.active_mappings.saturating_sub(1);
        }
    }

    fn prepare_removal(&self) -> bool {
        let mut inner = self.inner.lock();
        if inner.released {
            return true;
        }
        if inner.retiring {
            return false;
        }
        inner.retiring = true;
        if inner.operations_in_flight != 0 || inner.attaching {
            inner.retiring = false;
            return false;
        }
        true
    }

    fn cancel_removal(&self) {
        let mut inner = self.inner.lock();
        if !inner.released {
            inner.retiring = false;
        }
    }

    fn prepared_for_removal(&self) -> bool {
        let inner = self.inner.lock();
        inner.retiring && !inner.released && inner.operations_in_flight == 0 && !inner.attaching
    }

    fn commit_removal(&self) -> bool {
        let mapper = {
            let mut inner = self.inner.lock();
            if inner.released {
                return true;
            }
            if !inner.retiring
                || inner.operations_in_flight != 0
                || inner.attaching
                || inner.active_mappings != 0
            {
                return false;
            }
            inner.released = true;
            inner.attachments.clear();
            inner.mapper.take()
        };
        // AttachedIommuMapper 的 Drop 会在 controller registry 中撤销 active domain。
        drop(mapper);
        true
    }

    fn consumes(&self, dependency: PnpDependency) -> bool {
        let PnpDependency::DtbProvider { kind, phandle } = dependency else {
            return false;
        };
        kind == DtbProviderKind::Iommu as u16
            && self
                .inner
                .lock()
                .attachments
                .iter()
                .any(|attachment| attachment.phandle == phandle)
    }
}

#[derive(Clone)]
pub(crate) struct IommuConsumerLease {
    state: Arc<IommuConsumerState>,
}

impl IommuConsumerLease {
    pub(crate) fn claim_pnp_resource(&self, label: &'static str) -> Option<Box<dyn PnpResource>> {
        self.state
            .resource_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Box::new(IommuConsumerPnpResource {
            state: Arc::clone(&self.state),
            label,
            prepared: AtomicBool::new(false),
            armed: true,
        }))
    }

    pub(crate) fn released(&self) -> bool {
        self.state.inner.lock().released
    }
}

struct IommuConsumerPnpResource {
    state: Arc<IommuConsumerState>,
    label: &'static str,
    prepared: AtomicBool,
    armed: bool,
}

impl Drop for IommuConsumerPnpResource {
    fn drop(&mut self) {
        if self.armed {
            self.state.resource_claimed.store(false, Ordering::Release);
        }
    }
}

impl PnpResource for IommuConsumerPnpResource {
    fn kind(&self) -> PnpResourceKind {
        PnpResourceKind::Dma
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
        if self.prepared.load(Ordering::Acquire) {
            return Ok(());
        }
        if !self.state.prepare_removal() {
            return Err(PnpResourceReleaseError::new(
                self.kind(),
                self.label,
                "IOMMU consumer has a map/attach operation in flight",
            ));
        }
        self.prepared.store(true, Ordering::Release);
        Ok(())
    }

    fn cancel_release(&self) {
        if self.prepared.swap(false, Ordering::AcqRel) {
            self.state.cancel_removal();
        }
    }

    fn release_order(&self) -> PnpResourceReleaseOrder {
        PnpResourceReleaseOrder::Consumer
    }

    fn consumes_dependency(&self, dependency: PnpDependency) -> bool {
        self.state.consumes(dependency)
    }

    fn release(mut self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        if !self.prepared.load(Ordering::Acquire) {
            self.prepare_release()?;
        }
        if !self.state.commit_removal() {
            return Err(PnpResourceReleaseError::new(
                self.kind(),
                self.label,
                "IOMMU consumer retained a DMA mapping after driver removal",
            ));
        }
        self.armed = false;
        Ok(())
    }
}

struct LazyIommuMapper {
    requester: IommuRequester,
    state: Arc<IommuConsumerState>,
}

impl LazyIommuMapper {
    fn mapper(&self) -> Option<Arc<dyn DmaMapper>> {
        loop {
            let attachments = {
                let mut state = self.state.inner.lock();
                if let Some(mapper) = state.mapper.as_ref() {
                    return Some(Arc::clone(mapper));
                }
                if state.retiring || state.released {
                    return None;
                }
                if state.attaching {
                    drop(state);
                    core::hint::spin_loop();
                    continue;
                }
                state.attaching = true;
                state.attachments.clone()
            };
            let attached = attach_iommu_group_for_consumer(
                self.requester,
                attachments,
                Some(Arc::downgrade(&self.state)),
            )
            .ok();
            let mut state = self.state.inner.lock();
            state.attaching = false;
            if state.retiring || state.released {
                drop(state);
                drop(attached);
                return None;
            }
            if let Some(mapper) = attached {
                state.mapper = Some(Arc::clone(&mapper));
                return Some(mapper);
            }
            return None;
        }
    }

    fn attached_mapper(&self) -> Option<Arc<dyn DmaMapper>> {
        self.state.inner.lock().mapper.as_ref().map(Arc::clone)
    }
}

impl DmaMapper for LazyIommuMapper {
    fn sync_for_device(&self, region: DmaSyncRegion) {
        if let Some(mapper) = self.attached_mapper() {
            mapper.sync_for_device(region);
        }
    }

    fn sync_for_cpu(&self, region: DmaSyncRegion) {
        if let Some(mapper) = self.attached_mapper() {
            mapper.sync_for_cpu(region);
        }
    }

    fn phys_to_dma(&self, _region: DmaSyncRegion, _constraints: DmaConstraints) -> Option<usize> {
        None
    }

    fn map_region(
        &self,
        region: DmaSyncRegion,
        constraints: DmaConstraints,
    ) -> Option<DmaMappedRegion> {
        if !self.state.begin_map() {
            return None;
        }
        let mapped = self
            .mapper()
            .and_then(|mapper| mapper.map_region(region, constraints));
        self.state.finish_map(mapped.is_some());
        mapped
    }

    fn map_region_at(
        &self,
        region: DmaSyncRegion,
        constraints: DmaConstraints,
        dma_addr: usize,
    ) -> Option<DmaMappedRegion> {
        if !self.state.begin_map() {
            return None;
        }
        let mapped = self
            .mapper()
            .and_then(|mapper| mapper.map_region_at(region, constraints, dma_addr));
        self.state.finish_map(mapped.is_some());
        mapped
    }

    fn unmap_region(&self, region: DmaSyncRegion, mapping: DmaMappedRegion) -> bool {
        if !self.state.begin_unmap() {
            return false;
        }
        let unmapped = self
            .attached_mapper()
            .is_some_and(|mapper| mapper.unmap_region(region, mapping));
        self.state.finish_unmap(unmapped);
        unmapped
    }
}

struct CompositeMappingRecord {
    token: u64,
    dma_addr: usize,
    /// 每个 path 尚未完成撤销的底层映射。成功撤销后立即置空，使失败后的
    /// 重试不会再次 unmap 已经失效的 token。
    parts: Box<[Option<DmaMappedRegion>]>,
    unmapping: bool,
}

struct CompositeMappingState {
    in_flight: usize,
    records: Vec<CompositeMappingRecord>,
}

struct CompositeIommuMapper {
    domains: Vec<Arc<dyn DmaMapper>>,
    next_token: AtomicU64,
    mappings: Spinlock<CompositeMappingState>,
}

impl CompositeIommuMapper {
    fn reserve_mapping(&self) -> Option<u64> {
        let token = registry_id::alloc_atomic_id(&self.next_token).ok()?;
        let mut state = self.mappings.lock();
        let required = state.in_flight.checked_add(1)?;
        let additional = state
            .records
            .len()
            .checked_add(required)?
            .saturating_sub(state.records.capacity());
        if additional != 0 && state.records.try_reserve_exact(additional).is_err() {
            return None;
        }
        state.in_flight = required;
        Some(token)
    }

    fn cancel_mapping(&self) {
        let mut state = self.mappings.lock();
        state.in_flight = state.in_flight.saturating_sub(1);
    }

    fn publish_mapping(&self, record: CompositeMappingRecord) {
        let mut state = self.mappings.lock();
        state.in_flight = state.in_flight.saturating_sub(1);
        // reserve_mapping 为所有 in-flight 完成项预留了容量。
        state.records.push(record);
    }

    fn map_all(
        &self,
        region: DmaSyncRegion,
        constraints: DmaConstraints,
        requested: Option<usize>,
    ) -> Option<DmaMappedRegion> {
        let token = self.reserve_mapping()?;
        let mut parts = Vec::new();
        if parts.try_reserve_exact(self.domains.len()).is_err() {
            self.cancel_mapping();
            return None;
        }
        for (index, domain) in self.domains.iter().enumerate() {
            let dma_addr =
                requested.or_else(|| parts.first().map(|part: &DmaMappedRegion| part.dma_addr));
            let mapped = match dma_addr {
                Some(dma_addr) => domain.map_region_at(region, constraints, dma_addr),
                None => domain.map_region(region, constraints),
            };
            let Some(mapped) = mapped else {
                let mut rollback_complete = true;
                for (domain, mapped) in self.domains[..index].iter().zip(&parts).rev() {
                    rollback_complete &= domain.unmap_region(region, *mapped);
                }
                if !rollback_complete {
                    // map API 只能返回 Option，无法把“部分建图仍存在”的物理页
                    // 所有权交还调用方。继续返回 None 会让 DmaBuffer 释放仍可被
                    // 设备访问的页，因此这里必须 fail-stop。
                    panic!("composite IOMMU map rollback left a live DMA mapping");
                }
                self.cancel_mapping();
                return None;
            };
            parts.push(mapped);
        }
        let dma_addr = parts[0].dma_addr;
        self.publish_mapping(CompositeMappingRecord {
            token,
            dma_addr,
            parts: parts.into_iter().map(Some).collect(),
            unmapping: false,
        });
        Some(DmaMappedRegion { dma_addr, token })
    }
}

impl DmaMapper for CompositeIommuMapper {
    fn sync_for_device(&self, region: DmaSyncRegion) {
        for domain in &self.domains {
            domain.sync_for_device(region);
        }
    }

    fn sync_for_cpu(&self, region: DmaSyncRegion) {
        for domain in &self.domains {
            domain.sync_for_cpu(region);
        }
    }

    fn phys_to_dma(&self, _region: DmaSyncRegion, _constraints: DmaConstraints) -> Option<usize> {
        None
    }

    fn map_region(
        &self,
        region: DmaSyncRegion,
        constraints: DmaConstraints,
    ) -> Option<DmaMappedRegion> {
        self.map_all(region, constraints, None)
    }

    fn map_region_at(
        &self,
        region: DmaSyncRegion,
        constraints: DmaConstraints,
        dma_addr: usize,
    ) -> Option<DmaMappedRegion> {
        self.map_all(region, constraints, Some(dma_addr))
    }

    fn unmap_region(&self, region: DmaSyncRegion, mapping: DmaMappedRegion) -> bool {
        {
            let mut state = self.mappings.lock();
            let Some(index) = state.records.iter().position(|record| {
                record.token == mapping.token && record.dma_addr == mapping.dma_addr
            }) else {
                return false;
            };
            if state.records[index].unmapping {
                return false;
            }
            state.records[index].unmapping = true;
        }

        loop {
            let next = {
                let state = self.mappings.lock();
                let Some(record) = state.records.iter().find(|record| {
                    record.token == mapping.token && record.dma_addr == mapping.dma_addr
                }) else {
                    return true;
                };
                record
                    .parts
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(index, part)| part.map(|part| (index, part)))
            };

            let Some((part_index, part)) = next else {
                let mut state = self.mappings.lock();
                let Some(index) = state.records.iter().position(|record| {
                    record.token == mapping.token && record.dma_addr == mapping.dma_addr
                }) else {
                    return true;
                };
                state.records.swap_remove(index);
                return true;
            };

            let unmapped = self.domains[part_index].unmap_region(region, part);
            let mut state = self.mappings.lock();
            let Some(record) = state.records.iter_mut().find(|record| {
                record.token == mapping.token && record.dma_addr == mapping.dma_addr
            }) else {
                return false;
            };
            if !unmapped {
                record.unmapping = false;
                return false;
            }
            if record.parts[part_index] == Some(part) {
                record.parts[part_index] = None;
            } else {
                record.unmapping = false;
                return false;
            }
        }
    }
}

/// vtable 位于常驻 general 的 domain 包装器。
struct AttachedIommuMapper {
    phandle: u32,
    registration_id: u64,
    domain_id: u64,
    domain: Option<Arc<dyn IommuDomain>>,
}

impl AttachedIommuMapper {
    fn domain(&self) -> &dyn IommuDomain {
        self.domain
            .as_deref()
            .expect("live IOMMU mapper always owns its domain")
    }
}

impl DmaMapper for AttachedIommuMapper {
    fn sync_for_device(&self, region: DmaSyncRegion) {
        self.domain().sync_for_device(region);
    }

    fn sync_for_cpu(&self, region: DmaSyncRegion) {
        self.domain().sync_for_cpu(region);
    }

    fn phys_to_dma(&self, region: DmaSyncRegion, constraints: DmaConstraints) -> Option<usize> {
        self.domain().phys_to_dma(region, constraints)
    }

    fn map_region(
        &self,
        region: DmaSyncRegion,
        constraints: DmaConstraints,
    ) -> Option<DmaMappedRegion> {
        self.domain().map_region(region, constraints)
    }

    fn map_region_at(
        &self,
        region: DmaSyncRegion,
        constraints: DmaConstraints,
        dma_addr: usize,
    ) -> Option<DmaMappedRegion> {
        self.domain().map_region_at(region, constraints, dma_addr)
    }

    fn unmap_region(&self, region: DmaSyncRegion, mapping: DmaMappedRegion) -> bool {
        self.domain().unmap_region(region, mapping)
    }
}

impl Drop for AttachedIommuMapper {
    fn drop(&mut self) {
        // domain Drop 可能执行 ELM detach；active 必须在该 vtable 调用完成前保持非零。
        drop(self.domain.take());

        let mut registry = IOMMU_CONTROLLERS.lock();
        let Some(entry) = registry.controllers.iter_mut().find(|entry| {
            entry.handle.phandle == self.phandle && entry.handle.id == self.registration_id
        }) else {
            log::error!(
                "[iommu] domain outlived controller registration: phandle={:#x} id={}",
                self.phandle,
                self.registration_id
            );
            return;
        };
        let Some(index) = entry
            .active_domains
            .iter()
            .position(|domain| domain.id == self.domain_id)
        else {
            log::error!(
                "[iommu] active domain missing: phandle={:#x} registration={} domain={}",
                self.phandle,
                self.registration_id,
                self.domain_id
            );
            return;
        };
        entry.active_domains.swap_remove(index);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::dev::dma::{DmaBouncePolicy, DmaDirection};
    use crate::dev::pnp::{
        BusType, PnpBusInfo, PnpDevice, PnpId, PnpRemovalTransaction, PnpResource, PnpState,
    };

    fn constraints() -> DmaConstraints {
        DmaConstraints {
            address_mask: usize::MAX,
            max_segment_size: usize::MAX,
            max_segments: 1,
            coherent: true,
            supports_scatter_gather: false,
            bounce: DmaBouncePolicy::Disabled,
        }
    }

    fn region() -> DmaSyncRegion {
        DmaSyncRegion {
            paddr: 0x4000,
            vaddr: 0x8000,
            len: 0x100,
            direction: DmaDirection::Bidirectional,
        }
    }

    struct MappingState {
        maps: AtomicUsize,
        unmaps: AtomicUsize,
        domain_drops: AtomicUsize,
        drop_observed_busy: AtomicBool,
    }

    impl MappingState {
        fn new() -> Self {
            Self {
                maps: AtomicUsize::new(0),
                unmaps: AtomicUsize::new(0),
                domain_drops: AtomicUsize::new(0),
                drop_observed_busy: AtomicBool::new(false),
            }
        }
    }

    struct MappingDomain {
        state: Arc<MappingState>,
        handle: Arc<Spinlock<Option<IommuControllerHandle>>>,
    }

    impl DmaMapper for MappingDomain {
        fn sync_for_device(&self, _region: DmaSyncRegion) {}

        fn sync_for_cpu(&self, _region: DmaSyncRegion) {}

        fn phys_to_dma(
            &self,
            _region: DmaSyncRegion,
            _constraints: DmaConstraints,
        ) -> Option<usize> {
            None
        }

        fn map_region(
            &self,
            region: DmaSyncRegion,
            constraints: DmaConstraints,
        ) -> Option<DmaMappedRegion> {
            self.state.maps.fetch_add(1, Ordering::Relaxed);
            let dma_addr = 0x9000usize;
            constraints
                .accepts_dma_addr(dma_addr, region.len)
                .then_some(DmaMappedRegion {
                    dma_addr,
                    token: 0x1234,
                })
        }

        fn unmap_region(&self, _region: DmaSyncRegion, mapping: DmaMappedRegion) -> bool {
            assert_eq!(mapping.dma_addr, 0x9000);
            assert_eq!(mapping.token, 0x1234);
            self.state.unmaps.fetch_add(1, Ordering::Relaxed);
            true
        }
    }

    impl IommuDomain for MappingDomain {}

    impl Drop for MappingDomain {
        fn drop(&mut self) {
            self.state.domain_drops.fetch_add(1, Ordering::Relaxed);
            let handle = self
                .handle
                .lock()
                .expect("test stores controller handle before attaching");
            self.state.drop_observed_busy.store(
                unregister_iommu_controller(handle) == Err(IommuError::Busy),
                Ordering::Release,
            );
        }
    }

    struct MappingController {
        state: Arc<MappingState>,
        handle: Arc<Spinlock<Option<IommuControllerHandle>>>,
    }

    impl IommuController for MappingController {
        fn attach(&self, request: IommuAttachRequest) -> Result<Arc<dyn IommuDomain>, IommuError> {
            if request.requester() != IommuRequester::pci(3, 0x108)
                || request.specifier() != [0x55, 7]
            {
                return Err(IommuError::InvalidSpecifier);
            }
            Ok(Arc::new(MappingDomain {
                state: Arc::clone(&self.state),
                handle: Arc::clone(&self.handle),
            }))
        }
    }

    #[test]
    fn mapper_forwards_mapping_and_holds_domain_until_last_arc_drop() {
        const PHANDLE: u32 = 0x7ff0_0101;

        let state = Arc::new(MappingState::new());
        let handle_slot = Arc::new(Spinlock::new(None));
        let controller = Arc::new(MappingController {
            state: Arc::clone(&state),
            handle: Arc::clone(&handle_slot),
        });
        let handle = register_iommu_controller(PHANDLE, controller).unwrap();
        *handle_slot.lock() = Some(handle);

        let mapper = attach_iommu(PHANDLE, IommuRequester::pci(3, 0x108), &[0x55, 7]).unwrap();
        let mapping = mapper.map_region(region(), constraints()).unwrap();
        assert_eq!(mapping.dma_addr, 0x9000);
        assert_eq!(mapping.token, 0x1234);
        assert!(mapper.unmap_region(region(), mapping));
        assert_eq!(state.maps.load(Ordering::Relaxed), 1);
        assert_eq!(state.unmaps.load(Ordering::Relaxed), 1);
        assert_eq!(unregister_iommu_controller(handle), Err(IommuError::Busy));

        let mapper_clone = Arc::clone(&mapper);
        drop(mapper);
        assert_eq!(state.domain_drops.load(Ordering::Relaxed), 0);
        drop(mapper_clone);
        assert_eq!(state.domain_drops.load(Ordering::Relaxed), 1);
        assert!(state.drop_observed_busy.load(Ordering::Acquire));
        assert_eq!(unregister_iommu_controller(handle), Ok(()));
    }

    struct PassthroughDomain;

    impl DmaMapper for PassthroughDomain {
        fn sync_for_device(&self, _region: DmaSyncRegion) {}

        fn sync_for_cpu(&self, _region: DmaSyncRegion) {}

        fn phys_to_dma(&self, region: DmaSyncRegion, constraints: DmaConstraints) -> Option<usize> {
            constraints
                .accepts_dma_addr(region.paddr, region.len)
                .then_some(region.paddr)
        }
    }

    impl IommuDomain for PassthroughDomain {}

    struct BlockingController {
        entered: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
    }

    impl IommuController for BlockingController {
        fn attach(&self, _request: IommuAttachRequest) -> Result<Arc<dyn IommuDomain>, IommuError> {
            self.entered.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            Ok(Arc::new(PassthroughDomain))
        }
    }

    #[test]
    fn unregister_is_busy_during_attach_and_while_domain_is_active() {
        const PHANDLE: u32 = 0x7ff0_0102;

        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let handle = register_iommu_controller(
            PHANDLE,
            Arc::new(BlockingController {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            }),
        )
        .unwrap();
        let owned = controller_pnp_resource(handle, "test-iommu");

        let attach_thread =
            std::thread::spawn(|| attach_iommu(PHANDLE, IommuRequester::platform(42), &[9]));
        while !entered.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        assert!(super::super::pnp::PnpResource::prepare_release(&owned).is_err());
        assert_eq!(unregister_iommu_controller(handle), Err(IommuError::Busy));

        release.store(true, Ordering::Release);
        let mapper = attach_thread.join().unwrap().unwrap();
        assert!(super::super::pnp::PnpResource::prepare_release(&owned).is_err());
        assert_eq!(unregister_iommu_controller(handle), Err(IommuError::Busy));
        drop(mapper);
        assert!(super::super::pnp::PnpResource::prepare_release(&owned).is_ok());
        assert!(Box::new(owned).release().is_ok());
    }

    struct CompositeTestState {
        autonomous_dma: usize,
        fail_map_at: AtomicBool,
        fail_unmap_once: AtomicBool,
        maps: AtomicUsize,
        maps_at: AtomicUsize,
        unmaps: AtomicUsize,
        last_dma: AtomicUsize,
        next_token: AtomicU64,
    }

    impl CompositeTestState {
        fn new(autonomous_dma: usize) -> Self {
            Self {
                autonomous_dma,
                fail_map_at: AtomicBool::new(false),
                fail_unmap_once: AtomicBool::new(false),
                maps: AtomicUsize::new(0),
                maps_at: AtomicUsize::new(0),
                unmaps: AtomicUsize::new(0),
                last_dma: AtomicUsize::new(0),
                next_token: AtomicU64::new(1),
            }
        }

        fn mapping(&self, dma_addr: usize) -> DmaMappedRegion {
            self.last_dma.store(dma_addr, Ordering::Relaxed);
            DmaMappedRegion {
                dma_addr,
                token: self.next_token.fetch_add(1, Ordering::Relaxed),
            }
        }
    }

    struct CompositeTestDomain {
        state: Arc<CompositeTestState>,
    }

    impl DmaMapper for CompositeTestDomain {
        fn sync_for_device(&self, _region: DmaSyncRegion) {}

        fn sync_for_cpu(&self, _region: DmaSyncRegion) {}

        fn phys_to_dma(
            &self,
            _region: DmaSyncRegion,
            _constraints: DmaConstraints,
        ) -> Option<usize> {
            None
        }

        fn map_region(
            &self,
            region: DmaSyncRegion,
            constraints: DmaConstraints,
        ) -> Option<DmaMappedRegion> {
            self.state.maps.fetch_add(1, Ordering::Relaxed);
            constraints
                .accepts_dma_addr(self.state.autonomous_dma, region.len)
                .then(|| self.state.mapping(self.state.autonomous_dma))
        }

        fn map_region_at(
            &self,
            region: DmaSyncRegion,
            constraints: DmaConstraints,
            dma_addr: usize,
        ) -> Option<DmaMappedRegion> {
            self.state.maps_at.fetch_add(1, Ordering::Relaxed);
            if self.state.fail_map_at.load(Ordering::Acquire)
                || !constraints.accepts_dma_addr(dma_addr, region.len)
            {
                return None;
            }
            Some(self.state.mapping(dma_addr))
        }

        fn unmap_region(&self, _region: DmaSyncRegion, mapping: DmaMappedRegion) -> bool {
            assert_eq!(
                mapping.dma_addr,
                self.state.last_dma.load(Ordering::Relaxed)
            );
            self.state.unmaps.fetch_add(1, Ordering::Relaxed);
            if self.state.fail_unmap_once.swap(false, Ordering::AcqRel) {
                return false;
            }
            true
        }
    }

    impl IommuDomain for CompositeTestDomain {}

    struct CompositeTestController {
        state: Arc<CompositeTestState>,
    }

    impl IommuController for CompositeTestController {
        fn attach(&self, _request: IommuAttachRequest) -> Result<Arc<dyn IommuDomain>, IommuError> {
            Ok(Arc::new(CompositeTestDomain {
                state: Arc::clone(&self.state),
            }))
        }
    }

    #[test]
    fn composite_mapper_shares_iova_and_rolls_back_partial_mapping() {
        const FIRST_PHANDLE: u32 = 0x7ff0_0104;
        const SECOND_PHANDLE: u32 = 0x7ff0_0105;

        let first = Arc::new(CompositeTestState::new(0xa000));
        let second = Arc::new(CompositeTestState::new(0xc000));
        let first_handle = register_iommu_controller(
            FIRST_PHANDLE,
            Arc::new(CompositeTestController {
                state: Arc::clone(&first),
            }),
        )
        .unwrap();
        let second_handle = register_iommu_controller(
            SECOND_PHANDLE,
            Arc::new(CompositeTestController {
                state: Arc::clone(&second),
            }),
        )
        .unwrap();
        let mapper = attach_iommu_group(
            IommuRequester::platform(77),
            alloc::vec![
                IommuAttachment::new(FIRST_PHANDLE, alloc::vec![1].into_boxed_slice()),
                IommuAttachment::new(SECOND_PHANDLE, alloc::vec![2].into_boxed_slice()),
            ],
        )
        .unwrap();

        let mapping = mapper.map_region(region(), constraints()).unwrap();
        assert_eq!(mapping.dma_addr, 0xa000);
        assert_eq!(second.last_dma.load(Ordering::Relaxed), 0xa000);
        assert!(mapper.unmap_region(region(), mapping));

        let fixed = mapper
            .map_region_at(region(), constraints(), 0xb000)
            .unwrap();
        assert_eq!(fixed.dma_addr, 0xb000);
        assert_eq!(first.last_dma.load(Ordering::Relaxed), 0xb000);
        assert_eq!(second.last_dma.load(Ordering::Relaxed), 0xb000);
        assert!(mapper.unmap_region(region(), fixed));

        second.fail_map_at.store(true, Ordering::Release);
        assert!(mapper.map_region(region(), constraints()).is_none());
        assert_eq!(first.maps.load(Ordering::Relaxed), 2);
        assert_eq!(first.maps_at.load(Ordering::Relaxed), 1);
        assert_eq!(first.unmaps.load(Ordering::Relaxed), 3);
        assert_eq!(second.maps.load(Ordering::Relaxed), 0);
        assert_eq!(second.maps_at.load(Ordering::Relaxed), 3);
        assert_eq!(second.unmaps.load(Ordering::Relaxed), 2);
        assert_eq!(
            unregister_iommu_controller(first_handle),
            Err(IommuError::Busy)
        );
        assert_eq!(
            unregister_iommu_controller(second_handle),
            Err(IommuError::Busy)
        );

        drop(mapper);
        assert_eq!(unregister_iommu_controller(first_handle), Ok(()));
        assert_eq!(unregister_iommu_controller(second_handle), Ok(()));
    }

    #[test]
    fn composite_unmap_retries_only_the_remaining_paths() {
        const FIRST_PHANDLE: u32 = 0x7ff0_0110;
        const SECOND_PHANDLE: u32 = 0x7ff0_0111;

        let first = Arc::new(CompositeTestState::new(0xa000));
        let second = Arc::new(CompositeTestState::new(0xc000));
        let first_handle = register_iommu_controller(
            FIRST_PHANDLE,
            Arc::new(CompositeTestController {
                state: Arc::clone(&first),
            }),
        )
        .unwrap();
        let second_handle = register_iommu_controller(
            SECOND_PHANDLE,
            Arc::new(CompositeTestController {
                state: Arc::clone(&second),
            }),
        )
        .unwrap();
        let mapper = attach_iommu_group(
            IommuRequester::platform(78),
            alloc::vec![
                IommuAttachment::new(FIRST_PHANDLE, alloc::vec![1].into_boxed_slice()),
                IommuAttachment::new(SECOND_PHANDLE, alloc::vec![2].into_boxed_slice()),
            ],
        )
        .unwrap();

        let mapping = mapper.map_region(region(), constraints()).unwrap();
        first.fail_unmap_once.store(true, Ordering::Release);
        assert!(!mapper.unmap_region(region(), mapping));
        assert_eq!(first.unmaps.load(Ordering::Relaxed), 1);
        assert_eq!(second.unmaps.load(Ordering::Relaxed), 1);

        assert!(mapper.unmap_region(region(), mapping));
        assert_eq!(first.unmaps.load(Ordering::Relaxed), 2);
        assert_eq!(second.unmaps.load(Ordering::Relaxed), 1);

        drop(mapper);
        assert_eq!(unregister_iommu_controller(first_handle), Ok(()));
        assert_eq!(unregister_iommu_controller(second_handle), Ok(()));
    }

    #[test]
    fn composite_map_fail_stops_when_partial_rollback_cannot_revoke_dma() {
        const FIRST_PHANDLE: u32 = 0x7ff0_0112;
        const SECOND_PHANDLE: u32 = 0x7ff0_0113;

        let first = Arc::new(CompositeTestState::new(0xa000));
        let second = Arc::new(CompositeTestState::new(0xc000));
        let first_handle = register_iommu_controller(
            FIRST_PHANDLE,
            Arc::new(CompositeTestController {
                state: Arc::clone(&first),
            }),
        )
        .unwrap();
        let second_handle = register_iommu_controller(
            SECOND_PHANDLE,
            Arc::new(CompositeTestController {
                state: Arc::clone(&second),
            }),
        )
        .unwrap();
        let mapper = attach_iommu_group(
            IommuRequester::platform(79),
            alloc::vec![
                IommuAttachment::new(FIRST_PHANDLE, alloc::vec![1].into_boxed_slice()),
                IommuAttachment::new(SECOND_PHANDLE, alloc::vec![2].into_boxed_slice()),
            ],
        )
        .unwrap();

        first.fail_unmap_once.store(true, Ordering::Release);
        second.fail_map_at.store(true, Ordering::Release);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = mapper.map_region(region(), constraints());
        }));
        assert!(result.is_err());

        drop(mapper);
        assert_eq!(unregister_iommu_controller(first_handle), Ok(()));
        assert_eq!(unregister_iommu_controller(second_handle), Ok(()));
    }

    #[test]
    fn lazy_mapper_retries_after_controller_becomes_ready() {
        const PHANDLE: u32 = 0x7ff0_0106;

        let mapper = lazy_iommu_group(
            IommuRequester::platform(88),
            alloc::vec![IommuAttachment::new(
                PHANDLE,
                alloc::vec![5].into_boxed_slice(),
            )],
        )
        .unwrap();
        assert!(mapper.map_region(region(), constraints()).is_none());

        let state = Arc::new(CompositeTestState::new(0xd000));
        let handle = register_iommu_controller(
            PHANDLE,
            Arc::new(CompositeTestController {
                state: Arc::clone(&state),
            }),
        )
        .unwrap();
        let mapping = mapper.map_region(region(), constraints()).unwrap();
        assert_eq!(mapping.dma_addr, 0xd000);
        assert!(mapper.unmap_region(region(), mapping));
        assert_eq!(unregister_iommu_controller(handle), Err(IommuError::Busy));

        drop(mapper);
        assert_eq!(unregister_iommu_controller(handle), Ok(()));
    }

    #[test]
    fn registration_validates_phandle_and_generation() {
        const PHANDLE: u32 = 0x7ff0_0103;

        assert_eq!(
            register_iommu_controller(
                0,
                Arc::new(BlockingController {
                    entered: Arc::new(AtomicBool::new(false)),
                    release: Arc::new(AtomicBool::new(true)),
                })
            )
            .err(),
            Some(IommuError::Invalid)
        );
        let handle = register_iommu_controller(
            PHANDLE,
            Arc::new(BlockingController {
                entered: Arc::new(AtomicBool::new(false)),
                release: Arc::new(AtomicBool::new(true)),
            }),
        )
        .unwrap();
        assert_eq!(
            register_iommu_controller(
                PHANDLE,
                Arc::new(BlockingController {
                    entered: Arc::new(AtomicBool::new(false)),
                    release: Arc::new(AtomicBool::new(true)),
                })
            )
            .err(),
            Some(IommuError::AlreadyRegistered)
        );
        assert_eq!(unregister_iommu_controller(handle), Ok(()));
        assert_eq!(
            unregister_iommu_controller(handle),
            Err(IommuError::NotFound)
        );
        assert_eq!(
            attach_iommu(PHANDLE, IommuRequester::platform(1), &[]).err(),
            Some(IommuError::NotReady(PHANDLE))
        );
    }

    struct GroupState {
        maps: AtomicUsize,
        maps_at: AtomicUsize,
        unmaps: AtomicUsize,
    }

    struct GroupDomain {
        native_iova: usize,
        state: Arc<GroupState>,
    }

    impl DmaMapper for GroupDomain {
        fn sync_for_device(&self, _region: DmaSyncRegion) {}

        fn sync_for_cpu(&self, _region: DmaSyncRegion) {}

        fn phys_to_dma(
            &self,
            _region: DmaSyncRegion,
            _constraints: DmaConstraints,
        ) -> Option<usize> {
            None
        }

        fn map_region(
            &self,
            _region: DmaSyncRegion,
            _constraints: DmaConstraints,
        ) -> Option<DmaMappedRegion> {
            self.state.maps.fetch_add(1, Ordering::Relaxed);
            Some(DmaMappedRegion {
                dma_addr: self.native_iova,
                token: self.native_iova as u64,
            })
        }

        fn map_region_at(
            &self,
            _region: DmaSyncRegion,
            _constraints: DmaConstraints,
            dma_addr: usize,
        ) -> Option<DmaMappedRegion> {
            self.state.maps_at.fetch_add(1, Ordering::Relaxed);
            Some(DmaMappedRegion {
                dma_addr,
                token: (self.native_iova | 1) as u64,
            })
        }

        fn unmap_region(&self, _region: DmaSyncRegion, _mapping: DmaMappedRegion) -> bool {
            self.state.unmaps.fetch_add(1, Ordering::Relaxed);
            true
        }
    }

    impl IommuDomain for GroupDomain {}

    struct GroupController {
        native_iova: usize,
        state: Arc<GroupState>,
    }

    impl IommuController for GroupController {
        fn attach(&self, _request: IommuAttachRequest) -> Result<Arc<dyn IommuDomain>, IommuError> {
            Ok(Arc::new(GroupDomain {
                native_iova: self.native_iova,
                state: Arc::clone(&self.state),
            }))
        }
    }

    #[test]
    fn lazy_multi_path_mapper_retries_and_uses_one_iova() {
        const FIRST: u32 = 0x7ff0_0114;
        const SECOND: u32 = 0x7ff0_0115;

        let mapper = lazy_iommu_group(
            IommuRequester::platform(99),
            alloc::vec![
                IommuAttachment::new(FIRST, alloc::vec![1].into_boxed_slice()),
                IommuAttachment::new(SECOND, alloc::vec![2].into_boxed_slice()),
            ],
        )
        .unwrap();
        assert!(mapper.map_region(region(), constraints()).is_none());

        let first_state = Arc::new(GroupState {
            maps: AtomicUsize::new(0),
            maps_at: AtomicUsize::new(0),
            unmaps: AtomicUsize::new(0),
        });
        let second_state = Arc::new(GroupState {
            maps: AtomicUsize::new(0),
            maps_at: AtomicUsize::new(0),
            unmaps: AtomicUsize::new(0),
        });
        let first = register_iommu_controller(
            FIRST,
            Arc::new(GroupController {
                native_iova: 0xa000,
                state: Arc::clone(&first_state),
            }),
        )
        .unwrap();
        let second = register_iommu_controller(
            SECOND,
            Arc::new(GroupController {
                native_iova: 0xb000,
                state: Arc::clone(&second_state),
            }),
        )
        .unwrap();

        let mapped = mapper.map_region(region(), constraints()).unwrap();
        assert_eq!(mapped.dma_addr, 0xa000);
        assert_eq!(first_state.maps.load(Ordering::Relaxed), 1);
        assert_eq!(second_state.maps_at.load(Ordering::Relaxed), 1);
        assert!(mapper.unmap_region(region(), mapped));
        assert_eq!(first_state.unmaps.load(Ordering::Relaxed), 1);
        assert_eq!(second_state.unmaps.load(Ordering::Relaxed), 1);
        drop(mapper);
        assert_eq!(unregister_iommu_controller(second), Ok(()));
        assert_eq!(unregister_iommu_controller(first), Ok(()));
    }

    #[derive(Debug)]
    struct LifecycleBusInfo;

    impl PnpBusInfo for LifecycleBusInfo {
        fn bus_type(&self) -> BusType {
            BusType::GENERIC
        }

        fn as_any(&self) -> &dyn core::any::Any {
            self
        }
    }

    fn lifecycle_device(id: u64) -> Arc<PnpDevice> {
        PnpDevice::new(
            PnpId::Dynamic {
                fingerprint: id,
                bus: BusType::GENERIC,
                contract: "test-iommu-lifecycle@1".into(),
                identity: id.to_ne_bytes().into(),
            },
            alloc::format!("iommu-lifecycle-{id:x}").into_boxed_str(),
            Box::new(LifecycleBusInfo),
        )
        .unwrap()
    }

    struct ToggleProviderResource {
        busy: Arc<AtomicBool>,
        prepared: AtomicBool,
    }

    impl PnpResource for ToggleProviderResource {
        fn kind(&self) -> PnpResourceKind {
            PnpResourceKind::Other("test-provider-gate")
        }

        fn label(&self) -> &'static str {
            "test-provider-gate"
        }

        fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
            if self.busy.load(Ordering::Acquire) {
                return Err(PnpResourceReleaseError::new(
                    self.kind(),
                    self.label(),
                    "test provider remains busy",
                ));
            }
            self.prepared.store(true, Ordering::Release);
            Ok(())
        }

        fn cancel_release(&self) {
            self.prepared.store(false, Ordering::Release);
        }

        fn release_order(&self) -> PnpResourceReleaseOrder {
            PnpResourceReleaseOrder::Provider
        }

        fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
            self.prepare_release()
        }
    }

    #[test]
    fn provider_first_input_is_reordered_after_iommu_consumer() {
        const PHANDLE: u32 = 0x7ff0_0120;

        let state = Arc::new(CompositeTestState::new(0x1_0000));
        let handle = register_iommu_controller(
            PHANDLE,
            Arc::new(CompositeTestController {
                state: Arc::clone(&state),
            }),
        )
        .unwrap();
        let provider = lifecycle_device(0x1200);
        provider
            .own_bus_resource(Box::new(controller_pnp_resource(handle, "test-controller")))
            .unwrap();

        let consumer = lifecycle_device(0x1201);
        let (mapper, lease) = new_lazy_iommu_group(
            IommuRequester::pci(0, 8),
            alloc::vec![IommuAttachment::new(
                PHANDLE,
                alloc::vec![8].into_boxed_slice(),
            )],
        )
        .unwrap();
        consumer
            .own_bus_resource(
                lease
                    .claim_pnp_resource("test-consumer")
                    .expect("consumer resource is claimed exactly once"),
            )
            .unwrap();
        let mapped = mapper.map_region(region(), constraints()).unwrap();
        assert!(mapper.unmap_region(region(), mapped));

        // 输入刻意把 provider 放在前面；依赖拓扑必须把 consumer 提交在前。
        PnpRemovalTransaction::prepare(&[Arc::clone(&provider), Arc::clone(&consumer)])
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(consumer.state(), PnpState::Gone);
        assert_eq!(provider.state(), PnpState::Gone);
        assert!(mapper.map_region(region(), constraints()).is_none());
        assert_eq!(
            unregister_iommu_controller(handle),
            Err(IommuError::NotFound)
        );
    }

    #[test]
    fn canceled_provider_transaction_restores_consumer_and_controller() {
        const PHANDLE: u32 = 0x7ff0_0121;

        let state = Arc::new(CompositeTestState::new(0x1_1000));
        let handle = register_iommu_controller(
            PHANDLE,
            Arc::new(CompositeTestController {
                state: Arc::clone(&state),
            }),
        )
        .unwrap();
        let provider = lifecycle_device(0x1210);
        let busy = Arc::new(AtomicBool::new(true));
        // LIFO provider prepare：controller 先成功冻结，gate 随后失败，覆盖 cancel。
        provider
            .own_bus_resource(Box::new(ToggleProviderResource {
                busy: Arc::clone(&busy),
                prepared: AtomicBool::new(false),
            }))
            .unwrap();
        provider
            .own_bus_resource(Box::new(controller_pnp_resource(handle, "test-controller")))
            .unwrap();

        let consumer = lifecycle_device(0x1211);
        let (mapper, lease) = new_lazy_iommu_group(
            IommuRequester::platform(0x1211),
            alloc::vec![IommuAttachment::new(
                PHANDLE,
                alloc::vec![0x11].into_boxed_slice(),
            )],
        )
        .unwrap();
        consumer
            .own_bus_resource(lease.claim_pnp_resource("test-consumer").unwrap())
            .unwrap();
        let first = mapper.map_region(region(), constraints()).unwrap();
        assert!(mapper.unmap_region(region(), first));

        assert!(
            PnpRemovalTransaction::prepare(&[Arc::clone(&provider), Arc::clone(&consumer),])
                .is_err()
        );
        assert_eq!(provider.state(), PnpState::Discovered);
        assert_eq!(consumer.state(), PnpState::Discovered);
        let after_cancel = mapper.map_region(region(), constraints()).unwrap();
        assert!(mapper.unmap_region(region(), after_cancel));

        busy.store(false, Ordering::Release);
        PnpRemovalTransaction::prepare(&[provider, consumer])
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(
            unregister_iommu_controller(handle),
            Err(IommuError::NotFound)
        );
    }

    #[test]
    fn removing_provider_without_consumer_remains_busy() {
        const PHANDLE: u32 = 0x7ff0_0122;

        let state = Arc::new(CompositeTestState::new(0x1_2000));
        let handle = register_iommu_controller(
            PHANDLE,
            Arc::new(CompositeTestController {
                state: Arc::clone(&state),
            }),
        )
        .unwrap();
        let provider = lifecycle_device(0x1220);
        provider
            .own_bus_resource(Box::new(controller_pnp_resource(handle, "test-controller")))
            .unwrap();
        let consumer = lifecycle_device(0x1221);
        let (mapper, lease) = new_lazy_iommu_group(
            IommuRequester::pci(0, 0x10),
            alloc::vec![IommuAttachment::new(
                PHANDLE,
                alloc::vec![0x10].into_boxed_slice(),
            )],
        )
        .unwrap();
        consumer
            .own_bus_resource(lease.claim_pnp_resource("test-consumer").unwrap())
            .unwrap();
        let first = mapper.map_region(region(), constraints()).unwrap();
        assert!(mapper.unmap_region(region(), first));

        assert!(matches!(
            PnpRemovalTransaction::prepare(core::slice::from_ref(&provider)),
            Err(crate::dev::pnp::PnpError::ResourceBusy { .. })
        ));
        let second = mapper.map_region(region(), constraints()).unwrap();
        assert!(mapper.unmap_region(region(), second));

        PnpRemovalTransaction::prepare(&[provider, consumer])
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(
            unregister_iommu_controller(handle),
            Err(IommuError::NotFound)
        );
    }
}

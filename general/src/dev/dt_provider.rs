//! Device Tree provider 资源注册、获取与生命周期管理。
//!
//! DT 解析层已经按各 provider 的 `#*-cells` 切分 specifier；本模块把这些稳定
//! 引用连接到由 ELM 驱动注册的运行期 provider。消费者只按属性名和 phandle 获取
//! lease，不再解析原始属性，也不能在 provider 尚未就绪时退化为隐式默认值。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use vfs::sync::Spinlock;

use crate::dev::pnp::{
    self, PnpDependency, PnpError, PnpHandleResource, PnpResource, PnpResourceKind,
    PnpResourceReleaseError, PnpResourceReleaseOrder,
};
use crate::firmware::dtb::DtbProviderReference;

use super::registry_id;

/// 标准 DT provider 类型。
///
/// 数值进入 deferred dependency 诊断 ABI，已有值不得重排或复用。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum DtbProviderKind {
    Clock = 1,
    Reset = 2,
    Dma = 3,
    Iommu = 4,
    Phy = 5,
    PowerDomain = 6,
    Interconnect = 7,
    Pwm = 8,
    Mailbox = 9,
    IoChannel = 10,
    ThermalSensor = 11,
    SoundDai = 12,
    ReservedMemory = 13,
    Nvmem = 14,
    OperatingPoint = 15,
    InterruptAffinity = 16,
    Wakeup = 17,
    Msi = 18,
    Gpio = 19,
    Regulator = 20,
    Pinctrl = 21,
}

impl DtbProviderKind {
    /// 从解析器保留的原始 consumer 属性名恢复 provider 类型。
    pub fn from_property(property: &str) -> Option<Self> {
        Some(match property {
            "clocks" | "assigned-clocks" | "assigned-clock-parents" => Self::Clock,
            "resets" => Self::Reset,
            "dmas" => Self::Dma,
            "iommus" => Self::Iommu,
            "phys" => Self::Phy,
            "power-domains" => Self::PowerDomain,
            "interconnects" => Self::Interconnect,
            "pwms" => Self::Pwm,
            "mboxes" => Self::Mailbox,
            "io-channels" => Self::IoChannel,
            "thermal-sensors" => Self::ThermalSensor,
            "sound-dai" => Self::SoundDai,
            // memory-region 需要 consumer 身份、物理范围和池分配语义，只能走
            // firmware::dtb 的专用 reserved-memory API。
            "memory-region" => return None,
            "nvmem-cells" => Self::Nvmem,
            "operating-points-v2" => Self::OperatingPoint,
            "interrupt-affinity" => Self::InterruptAffinity,
            "wakeup-parent" => Self::Wakeup,
            "msi-parent" => Self::Msi,
            "gpios" => Self::Gpio,
            property if property.ends_with("-gpios") => Self::Gpio,
            property if property.ends_with("-supply") => Self::Regulator,
            property if pinctrl_state_property(property) => Self::Pinctrl,
            _ => return None,
        })
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Clock => "clock",
            Self::Reset => "reset",
            Self::Dma => "dma",
            Self::Iommu => "iommu",
            Self::Phy => "phy",
            Self::PowerDomain => "power-domain",
            Self::Interconnect => "interconnect",
            Self::Pwm => "pwm",
            Self::Mailbox => "mailbox",
            Self::IoChannel => "io-channel",
            Self::ThermalSensor => "thermal-sensor",
            Self::SoundDai => "sound-dai",
            Self::ReservedMemory => "reserved-memory",
            Self::Nvmem => "nvmem",
            Self::OperatingPoint => "operating-point",
            Self::InterruptAffinity => "interrupt-affinity",
            Self::Wakeup => "wakeup",
            Self::Msi => "msi",
            Self::Gpio => "gpio",
            Self::Regulator => "regulator",
            Self::Pinctrl => "pinctrl",
        }
    }
}

fn pinctrl_state_property(property: &str) -> bool {
    let Some(index) = property.strip_prefix("pinctrl-") else {
        return false;
    };
    !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
}

/// provider registry 中不依赖瞬时树内 NodeId 的稳定键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DtbProviderKey {
    pub kind: DtbProviderKind,
    pub phandle: u32,
}

impl DtbProviderKey {
    pub const fn new(kind: DtbProviderKind, phandle: u32) -> Self {
        Self { kind, phandle }
    }

    pub const fn dependency(self) -> PnpDependency {
        PnpDependency::DtbProvider {
            kind: self.kind as u16,
            phandle: self.phandle,
        }
    }
}

/// provider 或资源操作失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbProviderError {
    Invalid,
    Disabled,
    UnsupportedProperty,
    NotReady(DtbProviderKey),
    AlreadyRegistered,
    NotFound,
    Busy,
    OutOfMemory,
    AcquireFailed,
    UnsupportedOperation,
    HardwareFailure,
}

impl DtbProviderError {
    pub const fn dependency(self) -> Option<PnpDependency> {
        match self {
            Self::NotReady(key) => Some(key.dependency()),
            _ => None,
        }
    }

    /// 转换为 PnP probe 语义；未就绪必须保留精确 provider 键。
    pub const fn into_pnp_error(self) -> PnpError {
        match self {
            Self::NotReady(key) => PnpError::dependency(key.dependency()),
            Self::OutOfMemory => PnpError::OutOfMemory,
            Self::Disabled => PnpError::missing(
                PnpResourceKind::Other("dt-provider"),
                "referenced DT provider is disabled",
            ),
            Self::UnsupportedProperty => PnpError::malformed(
                PnpResourceKind::Other("dt-provider"),
                "unsupported DT provider property",
            ),
            Self::Invalid | Self::AcquireFailed => PnpError::malformed(
                PnpResourceKind::Other("dt-provider"),
                "invalid DT provider reference or specifier",
            ),
            Self::UnsupportedOperation => PnpError::unsupported("DT provider operation"),
            Self::HardwareFailure => PnpError::hardware_failure("DT provider operation failed"),
            Self::AlreadyRegistered | Self::NotFound | Self::Busy => PnpError::registration_failed(
                PnpResourceKind::Other("dt-provider"),
                "DT provider registry state rejected the operation",
            ),
        }
    }
}

/// 通用 provider 资源控制请求。
///
/// specifier 的 binding-specific 语义由 provider 在 `acquire()` 时解释；后续控制
/// 操作使用跨 provider 共用的资源语义。无法表达的 vendor 操作必须由对应总线/驱动
/// 契约扩展，不能把未校验字节塞入这里。
pub enum DtbResourceRequest<'a> {
    Enable,
    Disable,
    Assert,
    Deassert,
    Reset,
    PowerOn,
    PowerOff,
    ReadValue,
    WriteValue(u64),
    GetRate,
    SetRate(u64),
    SetVoltage { min_uv: u32, max_uv: u32 },
    SetBandwidth { average: u64, peak: u64 },
    Configure(&'a [u32]),
    ReadBytes { offset: usize, output: &'a mut [u8] },
    WriteBytes { offset: usize, input: &'a [u8] },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbResourceReply {
    Done,
    Value(u64),
    Count(usize),
    Range { minimum: u64, maximum: u64 },
}

/// provider 为一个已验证 specifier 返回的独占逻辑资源。
pub trait DtbResource: Send + Sync {
    fn control(
        &self,
        request: DtbResourceRequest<'_>,
    ) -> Result<DtbResourceReply, DtbProviderError>;
}

/// 由 ELM 驱动实现的 DT provider。
pub trait DtbProvider: Send + Sync {
    fn acquire(&self, specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbProviderHandle {
    key: DtbProviderKey,
    id: u64,
}

impl DtbProviderHandle {
    pub const fn key(self) -> DtbProviderKey {
        self.key
    }

    pub const fn id(self) -> u64 {
        self.id
    }
}

struct ProviderRegistration {
    handle: DtbProviderHandle,
    provider: Arc<dyn DtbProvider>,
    active_leases: usize,
    prepared_leases: usize,
    acquires_in_flight: usize,
    controls_in_flight: usize,
    retiring: bool,
}

struct ProviderRegistry {
    next_id: u64,
    providers: Vec<ProviderRegistration>,
}

impl ProviderRegistry {
    const fn new() -> Self {
        Self {
            next_id: 1,
            providers: Vec::new(),
        }
    }
}

static PROVIDERS: Spinlock<ProviderRegistry> = Spinlock::new(ProviderRegistry::new());

/// 登记一个由 DT phandle 标识的 provider。
#[kernel_symbols::export(
    name = "general.dev.dt_provider.register",
    contract = "kernel.general.dt-provider@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 2u64
)]
pub fn register(
    key: DtbProviderKey,
    provider: Arc<dyn DtbProvider>,
) -> Result<DtbProviderHandle, DtbProviderError> {
    if key.phandle == 0 || key.phandle == u32::MAX {
        return Err(DtbProviderError::Invalid);
    }
    let mut registry = PROVIDERS.lock();
    if registry
        .providers
        .iter()
        .any(|entry| entry.handle.key == key)
    {
        return Err(DtbProviderError::AlreadyRegistered);
    }
    registry
        .providers
        .try_reserve(1)
        .map_err(|_| DtbProviderError::OutOfMemory)?;
    let id = registry_id::alloc_locked_id(&mut registry.next_id)
        .map_err(|_| DtbProviderError::OutOfMemory)?;
    let handle = DtbProviderHandle { key, id };
    registry.providers.push(ProviderRegistration {
        handle,
        provider,
        active_leases: 0,
        prepared_leases: 0,
        acquires_in_flight: 0,
        controls_in_flight: 0,
        retiring: false,
    });
    drop(registry);
    if super::elm_lifecycle::track_dtb_provider(handle).is_err() {
        let _ = unregister(handle);
        return Err(DtbProviderError::OutOfMemory);
    }
    pnp::notify_dependency_ready(key.dependency());
    Ok(handle)
}

/// 注销 provider；仍有 consumer lease 时必须保留 ELM 代码与对象。
#[kernel_symbols::export(
    name = "general.dev.dt_provider.unregister",
    contract = "kernel.general.dt-provider@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister(handle: DtbProviderHandle) -> Result<(), DtbProviderError> {
    prepare_unregister(handle)?;
    match commit_unregister(handle) {
        Ok(()) => Ok(()),
        Err(error) => {
            cancel_unregister(handle);
            Err(error)
        }
    }
}

pub(crate) fn prepare_unregister(handle: DtbProviderHandle) -> Result<(), DtbProviderError> {
    let mut registry = PROVIDERS.lock();
    let entry = registry
        .providers
        .iter_mut()
        .find(|entry| entry.handle == handle)
        .ok_or(DtbProviderError::NotFound)?;
    if entry.retiring {
        return Err(DtbProviderError::Busy);
    }
    entry.retiring = true;
    if entry.active_leases != entry.prepared_leases
        || entry.acquires_in_flight != 0
        || entry.controls_in_flight != 0
    {
        entry.retiring = false;
        return Err(DtbProviderError::Busy);
    }
    Ok(())
}

pub(crate) fn cancel_unregister(handle: DtbProviderHandle) {
    if let Some(entry) = PROVIDERS
        .lock()
        .providers
        .iter_mut()
        .find(|entry| entry.handle == handle)
    {
        entry.retiring = false;
    }
}

fn commit_unregister(handle: DtbProviderHandle) -> Result<(), DtbProviderError> {
    let provider = {
        let mut registry = PROVIDERS.lock();
        let Some(index) = registry
            .providers
            .iter()
            .position(|entry| entry.handle == handle)
        else {
            return Err(DtbProviderError::NotFound);
        };
        let entry = &registry.providers[index];
        if !entry.retiring
            || entry.active_leases != 0
            || entry.prepared_leases != 0
            || entry.acquires_in_flight != 0
            || entry.controls_in_flight != 0
        {
            return Err(DtbProviderError::Busy);
        }
        registry.providers.remove(index).provider
    };
    // provider 的 vtable 位于 ELM 内；在返回给卸载事务前明确于锁外销毁最后一个 Arc。
    drop(provider);
    super::elm_lifecycle::forget_dtb_provider(handle);
    Ok(())
}

pub(crate) fn can_unregister(handle: DtbProviderHandle) -> Result<(), DtbProviderError> {
    let registry = PROVIDERS.lock();
    let entry = registry
        .providers
        .iter()
        .find(|entry| entry.handle == handle)
        .ok_or(DtbProviderError::NotFound)?;
    if entry.retiring
        || entry.active_leases != 0
        || entry.acquires_in_flight != 0
        || entry.controls_in_flight != 0
    {
        Err(DtbProviderError::Busy)
    } else {
        Ok(())
    }
}

fn prepare_provider_handle(handle: DtbProviderHandle) -> bool {
    matches!(
        prepare_unregister(handle),
        Ok(()) | Err(DtbProviderError::NotFound)
    )
}

fn cancel_provider_handle(handle: DtbProviderHandle) {
    cancel_unregister(handle);
}

fn release_provider_handle(handle: DtbProviderHandle) -> bool {
    matches!(
        commit_unregister(handle),
        Ok(()) | Err(DtbProviderError::NotFound)
    )
}

/// 将 provider registration 交给 PnP 设备拥有。
#[kernel_symbols::export(
    name = "general.dev.dt_provider.provider_pnp_resource",
    contract = "kernel.general.dt-provider@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn provider_pnp_resource(
    handle: DtbProviderHandle,
    label: &'static str,
) -> PnpHandleResource<DtbProviderHandle> {
    PnpHandleResource::new_checked(
        PnpResourceKind::Other("dt-provider"),
        label,
        handle,
        prepare_provider_handle,
        cancel_provider_handle,
        PnpResourceReleaseOrder::Provider,
        release_provider_handle,
    )
}

/// 一个 consumer 对 provider 逻辑资源的拥有型 lease。
pub struct DtbResourceLease {
    key: DtbProviderKey,
    registration_id: u64,
    resource: Option<Arc<dyn DtbResource>>,
    prepared: AtomicBool,
}

#[kernel_symbols::export]
impl DtbResourceLease {
    pub const fn key(&self) -> DtbProviderKey {
        self.key
    }

    #[kernel_symbols::export(
        name = "general.dev.dt_provider.DtbResourceLease.control",
        contract = "kernel.general.dt-provider@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn control(
        &self,
        request: DtbResourceRequest<'_>,
    ) -> Result<DtbResourceReply, DtbProviderError> {
        let _call = ProviderControlCall::begin(self)?;
        self.resource
            .as_ref()
            .expect("live DT resource lease always owns its resource")
            .control(request)
    }

    /// 供持有共享 consumer 状态的 PnP 资源冻结本 lease。
    #[kernel_symbols::export(
        name = "general.dev.dt_provider.DtbResourceLease.prepare_pnp_release",
        contract = "kernel.general.dt-provider@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn prepare_pnp_release(&self) -> Result<(), DtbProviderError> {
        self.prepare_for_release()
    }

    /// 撤销 [`Self::prepare_pnp_release`] 建立的冻结。
    #[kernel_symbols::export(
        name = "general.dev.dt_provider.DtbResourceLease.cancel_pnp_release",
        contract = "kernel.general.dt-provider@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn cancel_pnp_release(&self) {
        self.cancel_release();
    }

    fn prepare_for_release(&self) -> Result<(), DtbProviderError> {
        let mut registry = PROVIDERS.lock();
        let entry = registry
            .providers
            .iter_mut()
            .find(|entry| entry.handle.key == self.key && entry.handle.id == self.registration_id)
            .ok_or(DtbProviderError::NotReady(self.key))?;
        if self.prepared.load(Ordering::Acquire) {
            return Ok(());
        }
        if entry.retiring {
            return Err(DtbProviderError::Busy);
        }
        entry.prepared_leases = entry
            .prepared_leases
            .checked_add(1)
            .ok_or(DtbProviderError::Busy)?;
        self.prepared.store(true, Ordering::Release);
        Ok(())
    }

    fn cancel_release(&self) {
        let mut registry = PROVIDERS.lock();
        if !self.prepared.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some(entry) = registry
            .providers
            .iter_mut()
            .find(|entry| entry.handle.key == self.key && entry.handle.id == self.registration_id)
        {
            entry.prepared_leases = entry.prepared_leases.saturating_sub(1);
        }
    }
}

struct ProviderControlCall {
    key: DtbProviderKey,
    registration_id: u64,
}

impl ProviderControlCall {
    fn begin(lease: &DtbResourceLease) -> Result<Self, DtbProviderError> {
        let mut registry = PROVIDERS.lock();
        let entry = registry
            .providers
            .iter_mut()
            .find(|entry| entry.handle.key == lease.key && entry.handle.id == lease.registration_id)
            .ok_or(DtbProviderError::NotReady(lease.key))?;
        if entry.retiring || lease.prepared.load(Ordering::Acquire) {
            return Err(DtbProviderError::Busy);
        }
        entry.controls_in_flight = entry
            .controls_in_flight
            .checked_add(1)
            .ok_or(DtbProviderError::Busy)?;
        Ok(Self {
            key: lease.key,
            registration_id: lease.registration_id,
        })
    }
}

impl Drop for ProviderControlCall {
    fn drop(&mut self) {
        let mut registry = PROVIDERS.lock();
        if let Some(entry) = registry
            .providers
            .iter_mut()
            .find(|entry| entry.handle.key == self.key && entry.handle.id == self.registration_id)
        {
            entry.controls_in_flight = entry.controls_in_flight.saturating_sub(1);
        }
    }
}

#[kernel_symbols::export]
impl Drop for DtbResourceLease {
    #[kernel_symbols::export(
        name = "general.dev.dt_provider.DtbResourceLease.drop",
        contract = "kernel.general.dt-provider@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    fn drop(&mut self) {
        // 资源对象的 Drop 可能执行 ELM 代码；provider registration 必须在它完成前
        // 仍保持 busy，防止模块卸载与 vtable 调用并发。
        drop(self.resource.take());
        let mut registry = PROVIDERS.lock();
        let Some(entry) = registry
            .providers
            .iter_mut()
            .find(|entry| entry.handle.key == self.key && entry.handle.id == self.registration_id)
        else {
            // 注销在 active lease 存在时会返回 Busy，因此仅 registry 损坏才会到这里。
            log::error!(
                "[dt-provider] lease outlived registration: kind={} phandle={:#x} id={}",
                self.key.kind.name(),
                self.key.phandle,
                self.registration_id
            );
            return;
        };
        entry.active_leases = entry.active_leases.saturating_sub(1);
        if self.prepared.load(Ordering::Acquire) {
            entry.prepared_leases = entry.prepared_leases.saturating_sub(1);
        }
    }
}

/// 按稳定键和已切分 specifier 获取 provider 资源。
#[kernel_symbols::export(
    name = "general.dev.dt_provider.acquire",
    contract = "kernel.general.dt-provider@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn acquire(
    key: DtbProviderKey,
    specifier: &[u32],
) -> Result<DtbResourceLease, DtbProviderError> {
    let (registration_id, provider) = {
        let mut registry = PROVIDERS.lock();
        let entry = registry
            .providers
            .iter_mut()
            .find(|entry| entry.handle.key == key && !entry.retiring)
            .ok_or(DtbProviderError::NotReady(key))?;
        entry.acquires_in_flight = entry
            .acquires_in_flight
            .checked_add(1)
            .ok_or(DtbProviderError::Busy)?;
        (entry.handle.id, Arc::clone(&entry.provider))
    };

    let acquired = provider.acquire(specifier);
    let mut registry = PROVIDERS.lock();
    let Some(entry) = registry
        .providers
        .iter_mut()
        .find(|entry| entry.handle.key == key && entry.handle.id == registration_id)
    else {
        drop(registry);
        drop(acquired);
        return Err(DtbProviderError::NotReady(key));
    };
    entry.acquires_in_flight = entry.acquires_in_flight.saturating_sub(1);
    let resource = acquired?;
    if entry.retiring {
        drop(registry);
        drop(resource);
        return Err(DtbProviderError::NotReady(key));
    }
    let Some(active_leases) = entry.active_leases.checked_add(1) else {
        drop(registry);
        drop(resource);
        return Err(DtbProviderError::Busy);
    };
    entry.active_leases = active_leases;
    drop(registry);

    Ok(DtbResourceLease {
        key,
        registration_id,
        resource: Some(resource),
        prepared: AtomicBool::new(false),
    })
}

/// 将 consumer lease 交给 PnP 设备拥有，probe 回滚与 remove 会自动释放。
pub struct DtbLeasePnpResource {
    lease: Option<DtbResourceLease>,
    label: &'static str,
}

impl PnpResource for DtbLeasePnpResource {
    fn kind(&self) -> PnpResourceKind {
        PnpResourceKind::Other("dt-provider-lease")
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
        self.lease
            .as_ref()
            .ok_or_else(|| {
                PnpResourceReleaseError::new(
                    PnpResourceKind::Other("dt-provider-lease"),
                    self.label,
                    "provider lease was already released",
                )
            })?
            .prepare_for_release()
            .map_err(|_| {
                PnpResourceReleaseError::new(
                    PnpResourceKind::Other("dt-provider-lease"),
                    self.label,
                    "provider lease cannot be frozen",
                )
            })
    }

    fn cancel_release(&self) {
        if let Some(lease) = self.lease.as_ref() {
            lease.cancel_release();
        }
    }

    fn release_order(&self) -> PnpResourceReleaseOrder {
        PnpResourceReleaseOrder::Consumer
    }

    fn release(mut self: alloc::boxed::Box<Self>) -> Result<(), PnpResourceReleaseError> {
        drop(self.lease.take());
        Ok(())
    }
}

#[kernel_symbols::export(
    name = "general.dev.dt_provider.lease_pnp_resource",
    contract = "kernel.general.dt-provider@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn lease_pnp_resource(lease: DtbResourceLease, label: &'static str) -> DtbLeasePnpResource {
    DtbLeasePnpResource {
        lease: Some(lease),
        label,
    }
}

/// 直接消费解析器产生的规范化 provider 引用。
pub fn acquire_reference(
    reference: &DtbProviderReference,
) -> Result<DtbResourceLease, DtbProviderError> {
    let kind = DtbProviderKind::from_property(&reference.property)
        .ok_or(DtbProviderError::UnsupportedProperty)?;
    if reference.phandle == 0 {
        return Err(DtbProviderError::Invalid);
    }
    if reference.provider_available == Some(false) {
        return Err(DtbProviderError::Disabled);
    }
    acquire(
        DtbProviderKey::new(kind, reference.phandle),
        &reference.args,
    )
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TestResource;

    impl DtbResource for TestResource {
        fn control(
            &self,
            request: DtbResourceRequest<'_>,
        ) -> Result<DtbResourceReply, DtbProviderError> {
            match request {
                DtbResourceRequest::GetRate => Ok(DtbResourceReply::Value(24_000_000)),
                _ => Err(DtbProviderError::UnsupportedOperation),
            }
        }
    }

    struct TestProvider {
        expected: Vec<u32>,
        acquires: Arc<AtomicUsize>,
    }

    impl DtbProvider for TestProvider {
        fn acquire(&self, specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError> {
            if specifier != self.expected {
                return Err(DtbProviderError::AcquireFailed);
            }
            self.acquires.fetch_add(1, Ordering::Relaxed);
            Ok(Arc::new(TestResource))
        }
    }

    #[test]
    fn property_mapping_covers_dynamic_provider_bindings() {
        assert_eq!(
            DtbProviderKind::from_property("assigned-clock-parents"),
            Some(DtbProviderKind::Clock)
        );
        assert_eq!(
            DtbProviderKind::from_property("reset-gpios"),
            Some(DtbProviderKind::Gpio)
        );
        assert_eq!(
            DtbProviderKind::from_property("vdd-supply"),
            Some(DtbProviderKind::Regulator)
        );
        assert_eq!(
            DtbProviderKind::from_property("pinctrl-17"),
            Some(DtbProviderKind::Pinctrl)
        );
        assert_eq!(DtbProviderKind::from_property("pinctrl-name"), None);
        assert_eq!(DtbProviderKind::from_property("memory-region"), None);
    }

    #[test]
    fn active_lease_prevents_provider_unregistration() {
        let key = DtbProviderKey::new(DtbProviderKind::Clock, 0xfeed);
        let acquires = Arc::new(AtomicUsize::new(0));
        let handle = register(
            key,
            Arc::new(TestProvider {
                expected: alloc::vec![3, 7],
                acquires: Arc::clone(&acquires),
            }),
        )
        .unwrap();
        let lease = acquire(key, &[3, 7]).unwrap();
        assert_eq!(acquires.load(Ordering::Relaxed), 1);
        assert_eq!(
            lease.control(DtbResourceRequest::GetRate),
            Ok(DtbResourceReply::Value(24_000_000))
        );
        let owned = provider_pnp_resource(handle, "test-provider");
        assert!(owned.prepare_release().is_err());
        assert_eq!(unregister(handle), Err(DtbProviderError::Busy));
        drop(lease);
        assert!(owned.prepare_release().is_ok());
        assert_eq!(
            acquire(key, &[3, 7]).err(),
            Some(DtbProviderError::NotReady(key))
        );
        assert!(Box::new(owned).release().is_ok());
    }

    #[test]
    fn prepared_owned_lease_allows_same_transaction_provider_release() {
        let key = DtbProviderKey::new(DtbProviderKind::Reset, 0xfef0);
        let handle = register(
            key,
            Arc::new(TestProvider {
                expected: alloc::vec![1],
                acquires: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .unwrap();
        let lease = lease_pnp_resource(acquire(key, &[1]).unwrap(), "owned-consumer");
        let provider = provider_pnp_resource(handle, "owned-provider");

        lease.prepare_release().unwrap();
        provider.prepare_release().unwrap();
        assert_eq!(
            acquire(key, &[1]).err(),
            Some(DtbProviderError::NotReady(key))
        );
        Box::new(lease).release().unwrap();
        Box::new(provider).release().unwrap();
    }

    #[test]
    fn external_lease_cancels_provider_prepare_without_poisoning_registry() {
        let key = DtbProviderKey::new(DtbProviderKind::Clock, 0xfef1);
        let handle = register(
            key,
            Arc::new(TestProvider {
                expected: alloc::vec![2],
                acquires: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .unwrap();
        let owned = lease_pnp_resource(acquire(key, &[2]).unwrap(), "owned-consumer");
        let external = acquire(key, &[2]).unwrap();
        let provider = provider_pnp_resource(handle, "owned-provider");

        owned.prepare_release().unwrap();
        assert!(provider.prepare_release().is_err());
        owned.cancel_release();
        assert_eq!(
            external.control(DtbResourceRequest::GetRate),
            Ok(DtbResourceReply::Value(24_000_000))
        );
        drop(owned);
        drop(external);
        drop(provider);
        unregister(handle).unwrap();
    }
}

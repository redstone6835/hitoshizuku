//! 动态 ELM 设备对象的所有权和卸载收口。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

use super::dma::{DmaOps, replace_dma_ops};
use super::dt_bus::{DtbBusControllerHandle, unregister_controller as unregister_dtb_bus};
use super::dt_provider::{DtbProviderHandle, unregister as unregister_dtb_provider};
use super::firmware_bus::{FirmwareBusHandle, unregister as unregister_firmware_bus};
use super::function::{DeviceClassId, unregister_function_class};
use super::iommu::{IommuControllerHandle, unregister_iommu_controller};
use super::irq::{
    DefaultIrqDomainHandle, IocsrOps, IrqDomainHandle, IrqHandle, IrqLineOps, replace_iocsr_ops,
    replace_irq_line_ops, unregister_default_irq_domain, unregister_irq_domain,
    unregister_irq_handler,
};
use super::msi::{MsiControllerHandle, MsiHandle, free_msi, unregister_msi_controller};
use super::pci::{
    PciBarMapper, PciConfigAccess, PciHostBridgeHandle, replace_pci_access_pair,
    replace_pci_bar_mapper, replace_pci_config_access, unregister_host_bridge,
};
use super::pnp::{
    DriverHandle, PNP_DRIVERS, PnpDevice, PnpError, PreparedDriverDetach, unregister_driver,
    unsubscribe_device_events,
};

const RESOURCE_CAPACITY: usize = 4096;
const SUSPEND_UNSUPPORTED: i32 = -0x45_4c_44;

#[derive(Clone)]
enum DeviceResource {
    FunctionClass(DeviceClassId),
    Driver(DriverHandle),
    Device(Arc<PnpDevice>),
    DeviceFunction {
        device: Arc<PnpDevice>,
        class_id: DeviceClassId,
        name: Box<str>,
    },
    EventSubscription {
        owner: &'static str,
        name: Box<str>,
    },
    DtbBusController(DtbBusControllerHandle),
    DtbProvider(DtbProviderHandle),
    FirmwareBus(FirmwareBusHandle),
    IommuController(IommuControllerHandle),
    IrqHandler(IrqHandle),
    IrqDomain(IrqDomainHandle),
    DefaultIrqDomain(DefaultIrqDomainHandle),
    MsiController(MsiControllerHandle),
    MsiVector(MsiHandle),
    PciHostBridge(PciHostBridgeHandle),
    DmaOps(DmaOps),
    IrqLineOps(Option<IrqLineOps>),
    IocsrOps(Option<IocsrOps>),
    PciConfigAccess(Option<PciConfigAccess>),
    PciBarMapper(Option<PciBarMapper>),
    PciAccessPair {
        config: Option<PciConfigAccess>,
        bar_mapper: Option<PciBarMapper>,
    },
}

impl DeviceResource {
    fn release_phase(&self) -> ResourceReleasePhase {
        match self {
            // 驱动注销会同步解绑设备，并由 PnP core 深度优先移除 probe
            // 期间枚举出的子设备。它必须先于这些设备依赖的 host/backend。
            Self::Driver(_) => ResourceReleasePhase::Driver,
            Self::Device(_) | Self::DeviceFunction { .. } => ResourceReleasePhase::Device,
            Self::DmaOps(_)
            | Self::IrqLineOps(_)
            | Self::IocsrOps(_)
            | Self::PciConfigAccess(_)
            | Self::PciBarMapper(_)
            | Self::PciAccessPair { .. } => ResourceReleasePhase::GlobalBackend,
            _ => ResourceReleasePhase::Registration,
        }
    }

    fn matches(&self, key: ResourceKey<'_>) -> bool {
        match (self, key) {
            (Self::FunctionClass(left), ResourceKey::FunctionClass(right)) => *left == right,
            (Self::Driver(left), ResourceKey::Driver(right)) => *left == right,
            (Self::Device(left), ResourceKey::Device(right)) => left.runtime_id() == right,
            (
                Self::DeviceFunction {
                    device,
                    class_id,
                    name,
                },
                ResourceKey::DeviceFunction {
                    device_id,
                    class_id: expected_class,
                    name: expected_name,
                },
            ) => {
                device.runtime_id() == device_id
                    && *class_id == expected_class
                    && name.as_ref() == expected_name
            }
            (
                Self::EventSubscription { owner, name },
                ResourceKey::EventSubscription {
                    owner: expected_owner,
                    name: expected_name,
                },
            ) => *owner == expected_owner && name.as_ref() == expected_name,
            (Self::DtbBusController(left), ResourceKey::DtbBusController(right)) => *left == right,
            (Self::DtbProvider(left), ResourceKey::DtbProvider(right)) => *left == right,
            (Self::FirmwareBus(left), ResourceKey::FirmwareBus(right)) => *left == right,
            (Self::IommuController(left), ResourceKey::IommuController(right)) => *left == right,
            (Self::IrqHandler(left), ResourceKey::IrqHandler(right)) => *left == right,
            (Self::IrqDomain(left), ResourceKey::IrqDomain(right)) => *left == right,
            (Self::DefaultIrqDomain(left), ResourceKey::DefaultIrqDomain(right)) => *left == right,
            (Self::MsiController(left), ResourceKey::MsiController(right)) => *left == right,
            (Self::MsiVector(left), ResourceKey::MsiVector(right)) => *left == right,
            (Self::PciHostBridge(left), ResourceKey::PciHostBridge(right)) => *left == right,
            _ => false,
        }
    }

    fn release(&self) -> Result<(), i32> {
        match self {
            Self::FunctionClass(handle) => unregister_function_class(*handle).map_err(|_| -1),
            Self::Driver(handle) => match unregister_driver(*handle) {
                Ok(()) | Err(PnpError::NoDriver) => Ok(()),
                Err(_) => Err(-1),
            },
            Self::Device(device) => device.try_remove_device().map_err(|_| -1),
            Self::DeviceFunction {
                device,
                class_id,
                name,
            } => device
                .unregister_function(*class_id, name.as_ref())
                .map_err(|_| -1),
            Self::EventSubscription { owner, name } => {
                unsubscribe_device_events(*owner, name.as_ref()).map_err(|_| -1)
            }
            Self::DtbBusController(handle) => unregister_dtb_bus(*handle).map_err(|_| -1),
            Self::DtbProvider(handle) => unregister_dtb_provider(*handle).map_err(|_| -1),
            Self::FirmwareBus(handle) => unregister_firmware_bus(*handle).map_err(|_| -1),
            Self::IommuController(handle) => unregister_iommu_controller(*handle).map_err(|_| -1),
            Self::IrqHandler(handle) => unregister_irq_handler(*handle).map_err(|_| -1),
            Self::IrqDomain(handle) => unregister_irq_domain(*handle).map_err(|_| -1),
            Self::DefaultIrqDomain(handle) => {
                unregister_default_irq_domain(*handle).map_err(|_| -1)
            }
            Self::MsiController(handle) => unregister_msi_controller(*handle).map_err(|_| -1),
            Self::MsiVector(handle) => free_msi(*handle).map_err(|_| -1),
            Self::PciHostBridge(handle) => unregister_host_bridge(*handle).map_err(|_| -1),
            Self::DmaOps(previous) => {
                let _ = replace_dma_ops(*previous);
                Ok(())
            }
            Self::IrqLineOps(previous) => {
                let _ = replace_irq_line_ops(*previous);
                Ok(())
            }
            Self::IocsrOps(previous) => {
                let _ = replace_iocsr_ops(*previous);
                Ok(())
            }
            Self::PciConfigAccess(previous) => {
                let _ = replace_pci_config_access(*previous);
                Ok(())
            }
            Self::PciBarMapper(previous) => {
                let _ = replace_pci_bar_mapper(*previous);
                Ok(())
            }
            Self::PciAccessPair { config, bar_mapper } => {
                let _ = replace_pci_access_pair(*config, *bar_mapper);
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ResourceReleasePhase {
    Driver,
    Device,
    Registration,
    GlobalBackend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResourceOwner {
    id: u64,
    generation: u64,
}

impl ResourceOwner {
    fn from_context(context: elm_model::ElmCurrentContext) -> Self {
        Self {
            id: context.cell_id.0,
            generation: context.generation.0,
        }
    }

    fn current() -> Option<Self> {
        elm_model::current_context().map(Self::from_context)
    }
}

#[derive(Clone, Copy)]
enum ResourceKey<'a> {
    FunctionClass(DeviceClassId),
    Driver(DriverHandle),
    Device(u64),
    DeviceFunction {
        device_id: u64,
        class_id: DeviceClassId,
        name: &'a str,
    },
    EventSubscription {
        owner: &'static str,
        name: &'a str,
    },
    DtbBusController(DtbBusControllerHandle),
    DtbProvider(DtbProviderHandle),
    FirmwareBus(FirmwareBusHandle),
    IommuController(IommuControllerHandle),
    IrqHandler(IrqHandle),
    IrqDomain(IrqDomainHandle),
    DefaultIrqDomain(DefaultIrqDomainHandle),
    MsiController(MsiControllerHandle),
    MsiVector(MsiHandle),
    PciHostBridge(PciHostBridgeHandle),
}

struct ResourceRecord {
    id: u64,
    owner: Option<ResourceOwner>,
    release_phase: ResourceReleasePhase,
    resource: Option<DeviceResource>,
}

struct ResourceRegistry {
    next_id: u64,
    records: Vec<ResourceRecord>,
    prepared_owners: Vec<PreparedOwnerDetach>,
    failed_owners: Vec<ResourceOwner>,
}

struct PreparedOwnerDetach {
    owner: ResourceOwner,
    detach: PreparedDriverDetach<'static>,
}

impl ResourceRegistry {
    const fn new() -> Self {
        Self {
            next_id: 1,
            records: Vec::new(),
            prepared_owners: Vec::new(),
            failed_owners: Vec::new(),
        }
    }

    fn allocate_id(&mut self) -> Option<u64> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1)?;
        (id != 0).then_some(id)
    }

    fn insert(
        &mut self,
        owner: Option<ResourceOwner>,
        resource: DeviceResource,
    ) -> Result<u64, ()> {
        if self.records.len() >= RESOURCE_CAPACITY {
            log::error!("[elm-device] 设备资源归属表已满");
            return Err(());
        }
        {
            // 归属表容量属于常驻内核元数据，不能计入触发注册的动态单元。
            let _accounting =
                allocator::suspend_implicit_allocation_accounting().ok_or_else(|| {
                    log::error!("[elm-device] 无法暂停设备资源归属表的隐式分配计量");
                })?;
            self.records.try_reserve(1).map_err(|_| {
                log::error!("[elm-device] 无法扩展设备资源归属表");
            })?;
        }
        let id = self.allocate_id().ok_or_else(|| {
            log::error!("[elm-device] 设备资源归属编号耗尽");
        })?;
        let release_phase = resource.release_phase();
        self.records.push(ResourceRecord {
            id,
            owner,
            release_phase,
            resource: Some(resource),
        });
        Ok(id)
    }

    fn take_next_owned(&mut self, owner: ResourceOwner) -> Option<(u64, DeviceResource)> {
        let mut selected = None;
        for (index, record) in self.records.iter().enumerate() {
            if record.owner != Some(owner) || record.resource.is_none() {
                continue;
            }
            let replace = selected.is_none_or(|best_index| {
                let best: &ResourceRecord = &self.records[best_index];
                record.release_phase < best.release_phase
                    || (record.release_phase == best.release_phase && record.id > best.id)
            });
            if replace {
                selected = Some(index);
            }
        }
        let record = &mut self.records[selected?];
        Some((record.id, record.resource.take()?))
    }

    fn restore(&mut self, id: u64, resource: DeviceResource) -> Result<(), DeviceResource> {
        let Some(record) = self.records.iter_mut().find(|record| record.id == id) else {
            return Err(resource);
        };
        if record.resource.is_some() {
            return Err(resource);
        }
        record.resource = Some(resource);
        Ok(())
    }

    fn owner_is_prepared(&self, owner: ResourceOwner) -> bool {
        self.prepared_owners
            .iter()
            .any(|prepared| prepared.owner == owner)
    }

    fn owner_failed(&self, owner: ResourceOwner) -> bool {
        self.failed_owners.iter().any(|failed| *failed == owner)
    }

    fn take_prepared_owner(&mut self, owner: ResourceOwner) -> Option<PreparedOwnerDetach> {
        let index = self
            .prepared_owners
            .iter()
            .position(|prepared| prepared.owner == owner)?;
        Some(self.prepared_owners.swap_remove(index))
    }
}

static RESOURCES: Spinlock<ResourceRegistry> = Spinlock::new(ResourceRegistry::new());

const RESOURCE_OPS: kernel_symbols::KernelSymbolOwnedResourceOpsV1 =
    kernel_symbols::KernelSymbolOwnedResourceOpsV1::new(
        suspend_resource,
        resume_resource,
        quiesce_resource,
        cancel_resource,
        drain_resource,
        release_resource,
    );

fn track(resource: DeviceResource) -> Result<(), ()> {
    if !kernel_symbols::runtime_hooks_installed() {
        return Ok(());
    }
    let id = RESOURCES
        .lock()
        .insert(ResourceOwner::current(), resource)?;
    commit_tracking(id)
}

fn commit_tracking(id: u64) -> Result<(), ()> {
    match kernel_symbols::track_owned_resource(
        kernel_symbols::KERNEL_SYMBOL_RESOURCE_KIND_DEVICE,
        id,
        RESOURCE_OPS,
    ) {
        kernel_symbols::KERNEL_SYMBOL_RESOURCE_STATUS_TRACKED => Ok(()),
        kernel_symbols::KERNEL_SYMBOL_RESOURCE_STATUS_UNMANAGED => {
            remove_record(id);
            Ok(())
        }
        status => {
            log::error!("[elm-device] ELM 运行时拒绝设备资源登记: status={}", status);
            remove_record(id);
            Err(())
        }
    }
}

fn forget(key: ResourceKey<'_>) {
    let id = {
        let registry = RESOURCES.lock();
        registry
            .records
            .iter()
            .find(|record| {
                record
                    .resource
                    .as_ref()
                    .is_some_and(|resource| resource.matches(key))
            })
            .map(|record| record.id)
    };
    let Some(id) = id else {
        return;
    };
    let _ = kernel_symbols::untrack_owned_resource(
        kernel_symbols::KERNEL_SYMBOL_RESOURCE_KIND_DEVICE,
        id,
    );
    remove_record(id);
}

fn remove_record(id: u64) -> Option<DeviceResource> {
    let mut registry = RESOURCES.lock();
    let index = registry.records.iter().position(|record| record.id == id)?;
    registry.records.swap_remove(index).resource
}

fn suspend_resource(_owner: u64, _generation: u64, _handle: u64) -> Result<(), i32> {
    // 设备回调资源尚未建立可恢复的 shadow registration，暂停必须明确失败而不能留下悬空入口。
    Err(SUSPEND_UNSUPPORTED)
}

fn resume_resource(_owner: u64, _generation: u64, _handle: u64) -> Result<(), i32> {
    Ok(())
}

fn quiesce_resource(owner: u64, generation: u64, handle: u64) -> Result<(), i32> {
    let owner = ResourceOwner {
        id: owner,
        generation,
    };
    let grouped = {
        let registry = RESOURCES.lock();
        if registry.owner_failed(owner) {
            return Err(elm_model::ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED);
        }
        registry
            .records
            .iter()
            .any(|record| record.id == handle && record.owner == Some(owner))
    };
    if !grouped {
        return Ok(());
    }
    prepare_owner_detach(owner)
}

fn prepare_owner_detach(owner: ResourceOwner) -> Result<(), i32> {
    let (drivers, devices) = {
        let mut registry = RESOURCES.lock();
        if registry.owner_failed(owner) {
            return Err(elm_model::ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED);
        }
        if registry.owner_is_prepared(owner) {
            return Ok(());
        }
        let count = registry
            .records
            .iter()
            .filter(|record| record.owner == Some(owner) && record.resource.is_some())
            .count();
        let mut drivers = Vec::new();
        let mut devices = Vec::new();
        drivers.try_reserve(count).map_err(|_| -1)?;
        devices.try_reserve(count).map_err(|_| -1)?;
        for record in &registry.records {
            if record.owner != Some(owner) {
                continue;
            }
            match record.resource.as_ref() {
                Some(DeviceResource::Driver(handle)) => drivers.push(*handle),
                Some(DeviceResource::Device(device)) => devices.push(Arc::clone(device)),
                _ => {}
            }
        }
        registry.prepared_owners.try_reserve(1).map_err(|_| -1)?;
        registry.failed_owners.try_reserve(1).map_err(|_| -1)?;
        (drivers, devices)
    };

    let detach = PNP_DRIVERS
        .prepare_detach(&drivers, &devices)
        .map_err(|_| -1)?;
    let mut registry = RESOURCES.lock();
    if registry.owner_is_prepared(owner) {
        drop(registry);
        drop(detach);
        return Ok(());
    }
    registry
        .prepared_owners
        .push(PreparedOwnerDetach { owner, detach });
    Ok(())
}

fn cancel_resource(owner: u64, generation: u64, _handle: u64) -> Result<(), i32> {
    let owner = ResourceOwner {
        id: owner,
        generation,
    };
    // 同一 owner 的所有设备资源共享一条 PreparedDriverDetach。owned-resource core
    // 可能为组内多个 handle 逐一调用 cancel；第一条负责取出并 drop 事务，后续调用
    // 幂等返回。PreparedDriverDetach::drop 会先解冻设备/资源，再重新开放 driver probe。
    let prepared = RESOURCES.lock().take_prepared_owner(owner);
    drop(prepared);
    Ok(())
}

fn drain_resource(_owner: u64, _generation: u64, _handle: u64) -> Result<(), i32> {
    let owner = ResourceOwner {
        id: _owner,
        generation: _generation,
    };
    {
        let registry = RESOURCES.lock();
        if registry.owner_failed(owner) {
            return Err(elm_model::ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED);
        }
    }
    let prepared = RESOURCES.lock().take_prepared_owner(owner);
    let Some(prepared) = prepared else {
        return Ok(());
    };
    if prepared.detach.commit().is_err() {
        mark_owner_failed(owner);
        return Err(elm_model::ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED);
    }
    if RESOURCES.lock().owner_failed(owner) {
        return Err(elm_model::ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED);
    }

    while let Some((id, resource)) = RESOURCES.lock().take_next_owned(owner) {
        if let Err(status) = resource.release() {
            if RESOURCES.lock().restore(id, resource).is_err() {
                log::error!("[elm-device] 无法恢复释放失败的设备资源: id={}", id);
            }
            mark_owner_failed(owner);
            return Err(
                if status == elm_model::ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED {
                    status
                } else {
                    elm_model::ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED
                },
            );
        }
    }
    Ok(())
}

fn mark_owner_failed(owner: ResourceOwner) {
    let mut registry = RESOURCES.lock();
    if !registry.owner_failed(owner) {
        if registry.failed_owners.try_reserve(1).is_err() {
            log::error!(
                "[elm-device] 无法记录设备 owner 失败: owner={} generation={}",
                owner.id,
                owner.generation
            );
            return;
        }
        registry.failed_owners.push(owner);
    }
}

pub(crate) fn mark_context_failed(context: elm_model::ElmCurrentContext) {
    if kernel_symbols::runtime_hooks_installed() {
        mark_owner_failed(ResourceOwner::from_context(context));
    }
}

fn release_resource(owner: u64, generation: u64, handle: u64) -> Result<(), i32> {
    if RESOURCES.lock().owner_failed(ResourceOwner {
        id: owner,
        generation,
    }) {
        return Err(elm_model::ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED);
    }
    match remove_record(handle) {
        Some(resource) => resource.release(),
        None => Ok(()),
    }
}

pub(crate) fn track_function_class(handle: DeviceClassId) -> Result<(), ()> {
    track(DeviceResource::FunctionClass(handle))
}

pub(crate) fn forget_function_class(handle: DeviceClassId) {
    forget(ResourceKey::FunctionClass(handle));
}

pub(crate) fn track_driver(handle: DriverHandle) -> Result<(), ()> {
    track(DeviceResource::Driver(handle))
}

pub(crate) fn forget_driver(handle: DriverHandle) {
    forget(ResourceKey::Driver(handle));
}

pub(crate) fn track_device(device: Arc<PnpDevice>) -> Result<(), ()> {
    track(DeviceResource::Device(device))
}

pub(crate) fn forget_device(device: &Arc<PnpDevice>) {
    forget(ResourceKey::Device(device.runtime_id()));
}

pub(crate) fn track_device_function(
    device: Arc<PnpDevice>,
    class_id: DeviceClassId,
    name: Box<str>,
) -> Result<(), ()> {
    track(DeviceResource::DeviceFunction {
        device,
        class_id,
        name,
    })
}

pub(crate) fn forget_device_function(device: &PnpDevice, class_id: DeviceClassId, name: &str) {
    forget(ResourceKey::DeviceFunction {
        device_id: device.runtime_id(),
        class_id,
        name,
    });
}

pub(crate) fn track_event_subscription(owner: &'static str, name: Box<str>) -> Result<(), ()> {
    track(DeviceResource::EventSubscription { owner, name })
}

pub(crate) fn forget_event_subscription(owner: &'static str, name: &str) {
    forget(ResourceKey::EventSubscription { owner, name });
}

fn install_exclusive_global(
    occupied: fn(&DeviceResource) -> bool,
    install: impl FnOnce() -> DeviceResource,
) -> Result<(), ()> {
    if !kernel_symbols::runtime_hooks_installed() {
        let _ = install();
        return Ok(());
    }

    let (id, rollback) = {
        let mut registry = RESOURCES.lock();
        if registry
            .records
            .iter()
            .filter_map(|record| record.resource.as_ref())
            .any(occupied)
        {
            return Err(());
        }
        if registry.records.len() >= RESOURCE_CAPACITY {
            log::error!("[elm-device] 设备资源归属表已满");
            return Err(());
        }
        {
            // 归属表容量属于常驻内核元数据，不能计入安装后端的动态单元。
            let _accounting =
                allocator::suspend_implicit_allocation_accounting().ok_or_else(|| {
                    log::error!("[elm-device] 无法暂停设备资源归属表的隐式分配计量");
                })?;
            registry.records.try_reserve(1).map_err(|_| {
                log::error!("[elm-device] 无法扩展设备资源归属表");
            })?;
        }
        let id = registry.allocate_id().ok_or_else(|| {
            log::error!("[elm-device] 设备资源归属编号耗尽");
        })?;
        let resource = install();
        let rollback = resource.clone();
        let release_phase = resource.release_phase();
        registry.records.push(ResourceRecord {
            id,
            owner: ResourceOwner::current(),
            release_phase,
            resource: Some(resource),
        });
        (id, rollback)
    };

    if commit_tracking(id).is_ok() {
        return Ok(());
    }
    if let Err(status) = rollback.release() {
        log::error!(
            "[elm-device] 独占设备后端登记失败且回滚失败: status={}",
            status
        );
    }
    Err(())
}

pub(crate) fn install_dma_ops(ops: DmaOps) -> Result<(), ()> {
    install_exclusive_global(
        |resource| matches!(resource, DeviceResource::DmaOps(_)),
        || DeviceResource::DmaOps(replace_dma_ops(ops)),
    )
}

pub(crate) fn install_irq_line_ops(ops: IrqLineOps) -> Result<(), ()> {
    install_exclusive_global(
        |resource| matches!(resource, DeviceResource::IrqLineOps(_)),
        || DeviceResource::IrqLineOps(replace_irq_line_ops(Some(ops))),
    )
}

pub(crate) fn install_iocsr_ops(ops: IocsrOps) -> Result<(), ()> {
    install_exclusive_global(
        |resource| matches!(resource, DeviceResource::IocsrOps(_)),
        || DeviceResource::IocsrOps(replace_iocsr_ops(Some(ops))),
    )
}

pub(crate) fn install_pci_config_access(access: PciConfigAccess) -> Result<(), ()> {
    install_exclusive_global(
        |resource| {
            matches!(
                resource,
                DeviceResource::PciConfigAccess(_) | DeviceResource::PciAccessPair { .. }
            )
        },
        || DeviceResource::PciConfigAccess(replace_pci_config_access(Some(access))),
    )
}

pub(crate) fn install_pci_bar_mapper(mapper: Option<PciBarMapper>) -> Result<(), ()> {
    install_exclusive_global(
        |resource| {
            matches!(
                resource,
                DeviceResource::PciBarMapper(_) | DeviceResource::PciAccessPair { .. }
            )
        },
        || DeviceResource::PciBarMapper(replace_pci_bar_mapper(mapper)),
    )
}

/// 原子安装 PCI 配置访问与 BAR 地址翻译后端。
///
/// 两个回调共享同一个 ELM owned-resource；跟踪失败时会一起恢复，避免只发布
/// config access 却遗漏 BAR mapper 的半安装状态。
pub(crate) fn install_pci_access_pair(
    access: PciConfigAccess,
    mapper: PciBarMapper,
) -> Result<(), ()> {
    install_exclusive_global(
        |resource| {
            matches!(
                resource,
                DeviceResource::PciConfigAccess(_)
                    | DeviceResource::PciBarMapper(_)
                    | DeviceResource::PciAccessPair { .. }
            )
        },
        || {
            let (config, bar_mapper) = replace_pci_access_pair(Some(access), Some(mapper));
            DeviceResource::PciAccessPair { config, bar_mapper }
        },
    )
}

macro_rules! simple_resource_helpers {
    ($track:ident, $forget:ident, $variant:ident, $key:ident, $type:ty) => {
        pub(crate) fn $track(handle: $type) -> Result<(), ()> {
            track(DeviceResource::$variant(handle))
        }

        pub(crate) fn $forget(handle: $type) {
            forget(ResourceKey::$key(handle));
        }
    };
}

simple_resource_helpers!(
    track_dtb_bus_controller,
    forget_dtb_bus_controller,
    DtbBusController,
    DtbBusController,
    DtbBusControllerHandle
);
simple_resource_helpers!(
    track_dtb_provider,
    forget_dtb_provider,
    DtbProvider,
    DtbProvider,
    DtbProviderHandle
);
simple_resource_helpers!(
    track_firmware_bus,
    forget_firmware_bus,
    FirmwareBus,
    FirmwareBus,
    FirmwareBusHandle
);
simple_resource_helpers!(
    track_iommu_controller,
    forget_iommu_controller,
    IommuController,
    IommuController,
    IommuControllerHandle
);
simple_resource_helpers!(
    track_irq_handler,
    forget_irq_handler,
    IrqHandler,
    IrqHandler,
    IrqHandle
);
simple_resource_helpers!(
    track_irq_domain,
    forget_irq_domain,
    IrqDomain,
    IrqDomain,
    IrqDomainHandle
);
simple_resource_helpers!(
    track_default_irq_domain,
    forget_default_irq_domain,
    DefaultIrqDomain,
    DefaultIrqDomain,
    DefaultIrqDomainHandle
);
simple_resource_helpers!(
    track_msi_controller,
    forget_msi_controller,
    MsiController,
    MsiController,
    MsiControllerHandle
);
simple_resource_helpers!(
    track_msi_vector,
    forget_msi_vector,
    MsiVector,
    MsiVector,
    MsiHandle
);
simple_resource_helpers!(
    track_pci_host_bridge,
    forget_pci_host_bridge,
    PciHostBridge,
    PciHostBridge,
    PciHostBridgeHandle
);

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn inert_record(
        id: u64,
        owner: ResourceOwner,
        release_phase: ResourceReleasePhase,
    ) -> ResourceRecord {
        ResourceRecord {
            id,
            owner: Some(owner),
            release_phase,
            resource: Some(DeviceResource::PciConfigAccess(None)),
        }
    }

    fn planned_ids(mut registry: ResourceRegistry, owner: ResourceOwner) -> Vec<u64> {
        let mut ids = Vec::new();
        while let Some((id, _)) = registry.take_next_owned(owner) {
            ids.push(id);
        }
        ids
    }

    #[test]
    fn detach_plan_is_stable_for_device_before_or_after_driver() {
        let owner = ResourceOwner {
            id: 7,
            generation: 3,
        };
        let other = ResourceOwner {
            id: 8,
            generation: 1,
        };

        // platform device 已存在：probe 产生的 backend/host/endpoint 先登记，
        // register_driver_factory 返回后 driver 才登记。
        let platform_first = ResourceRegistry {
            next_id: 6,
            records: vec![
                inert_record(1, owner, ResourceReleasePhase::GlobalBackend),
                inert_record(2, owner, ResourceReleasePhase::Registration),
                inert_record(3, owner, ResourceReleasePhase::Device),
                inert_record(4, owner, ResourceReleasePhase::Driver),
                inert_record(5, other, ResourceReleasePhase::Driver),
            ],
            prepared_owners: Vec::new(),
            failed_owners: Vec::new(),
        };
        assert_eq!(planned_ids(platform_first, owner), vec![4, 3, 2, 1]);

        // driver 已存在：后续 platform probe 把 backend/host/endpoint 追加在它后面。
        let driver_first = ResourceRegistry {
            next_id: 5,
            records: vec![
                inert_record(1, owner, ResourceReleasePhase::Driver),
                inert_record(2, owner, ResourceReleasePhase::GlobalBackend),
                inert_record(3, owner, ResourceReleasePhase::Registration),
                inert_record(4, owner, ResourceReleasePhase::Device),
            ],
            prepared_owners: Vec::new(),
            failed_owners: Vec::new(),
        };
        assert_eq!(planned_ids(driver_first, owner), vec![1, 4, 3, 2]);
    }

    #[test]
    fn pci_access_pair_is_a_last_phase_backend() {
        let resource = DeviceResource::PciAccessPair {
            config: None,
            bar_mapper: None,
        };
        assert_eq!(
            resource.release_phase(),
            ResourceReleasePhase::GlobalBackend
        );
    }
}

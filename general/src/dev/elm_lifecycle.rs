//! 动态 ELM 设备对象的所有权和卸载收口。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

use super::dma::{DmaOps, replace_dma_ops};
use super::firmware_bus::{FirmwareBusHandle, unregister as unregister_firmware_bus};
use super::function::{DeviceClassId, unregister_function_class};
use super::irq::{
    DefaultIrqDomainHandle, IocsrOps, IrqDomainHandle, IrqHandle, IrqLineOps, replace_iocsr_ops,
    replace_irq_line_ops, unregister_default_irq_domain, unregister_irq_domain,
    unregister_irq_handler,
};
use super::msi::{MsiControllerHandle, MsiHandle, free_msi, unregister_msi_controller};
use super::pci::{
    PciConfigAccess, PciHostBridgeHandle, replace_pci_config_access, unregister_host_bridge,
};
use super::pnp::{DriverHandle, PnpDevice, unregister_driver, unsubscribe_device_events};

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
    FirmwareBus(FirmwareBusHandle),
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
}

impl DeviceResource {
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
            (Self::FirmwareBus(left), ResourceKey::FirmwareBus(right)) => *left == right,
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
            Self::Driver(handle) => unregister_driver(*handle).map_err(|_| -1),
            Self::Device(device) => {
                device.remove_device();
                Ok(())
            }
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
            Self::FirmwareBus(handle) => unregister_firmware_bus(*handle).map_err(|_| -1),
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
        }
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
    FirmwareBus(FirmwareBusHandle),
    IrqHandler(IrqHandle),
    IrqDomain(IrqDomainHandle),
    DefaultIrqDomain(DefaultIrqDomainHandle),
    MsiController(MsiControllerHandle),
    MsiVector(MsiHandle),
    PciHostBridge(PciHostBridgeHandle),
}

struct ResourceRecord {
    id: u64,
    resource: Option<DeviceResource>,
}

struct ResourceRegistry {
    next_id: u64,
    records: Vec<ResourceRecord>,
}

impl ResourceRegistry {
    const fn new() -> Self {
        Self {
            next_id: 1,
            records: Vec::new(),
        }
    }

    fn allocate_id(&mut self) -> Option<u64> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1)?;
        (id != 0).then_some(id)
    }

    fn insert(&mut self, resource: DeviceResource) -> Result<u64, ()> {
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
        self.records.push(ResourceRecord {
            id,
            resource: Some(resource),
        });
        Ok(id)
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
    let id = RESOURCES.lock().insert(resource)?;
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

fn quiesce_resource(_owner: u64, _generation: u64, handle: u64) -> Result<(), i32> {
    let resource = {
        let registry = RESOURCES.lock();
        let Some(record) = registry.records.iter().find(|record| record.id == handle) else {
            return Ok(());
        };
        record.resource.clone()
    };
    let Some(resource) = resource else {
        return Ok(());
    };
    resource.release()?;
    let mut registry = RESOURCES.lock();
    if let Some(record) = registry
        .records
        .iter_mut()
        .find(|record| record.id == handle)
    {
        record.resource = None;
    }
    Ok(())
}

fn cancel_resource(_owner: u64, _generation: u64, _handle: u64) -> Result<(), i32> {
    Ok(())
}

fn drain_resource(_owner: u64, _generation: u64, _handle: u64) -> Result<(), i32> {
    Ok(())
}

fn release_resource(_owner: u64, _generation: u64, handle: u64) -> Result<(), i32> {
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
        registry.records.push(ResourceRecord {
            id,
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
        |resource| matches!(resource, DeviceResource::PciConfigAccess(_)),
        || DeviceResource::PciConfigAccess(replace_pci_config_access(Some(access))),
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
    track_firmware_bus,
    forget_firmware_bus,
    FirmwareBus,
    FirmwareBus,
    FirmwareBusHandle
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

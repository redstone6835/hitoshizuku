//! `kernel.device@1` 的内核集成与 ELM 设备代理。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use elm_model::{
    ELM_API_NAMESPACE_FLAG_REQUIRE_GRANT, ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED,
    ElmApiNamespaceDescriptorV1, ElmCurrentContext, ElmId, ElmOwnedResourceKind,
    ElmOwnedResourceOpsV1, Generation,
};
use general::dev::dma::{DmaBouncePolicy, DmaBuffer, DmaConstraints, DmaContext, DmaDirection};
use general::dev::function::{
    DeviceClassId, DeviceFunction, DeviceFunctionInvokeError, FunctionClassRegistration,
};
use general::dev::irq::{
    IrqHandle, IrqHandler, IrqLine, IrqRequest, IrqStatus, register_irq_request,
    unregister_irq_handler,
};
use general::dev::msi::{MsiError, MsiHandle, allocate_msi, free_msi};
use general::dev::pci::{PciBarType, PciDevice, PciMsiError, PciMsiHandle};
use general::dev::platform::{DeviceResource, PlatformDeviceInfo};
use general::dev::pnp::{
    BusType, DriverFactory, DriverHandle, DynamicPnpBusInfo, DynamicPnpResource, PNP_DEVICES,
    PNP_DRIVERS, PnpBusInfo, PnpDevice, PnpDriver, PnpDriverPriority, PnpError, PnpId, PnpResource,
    PnpResourceKind, PnpResourceReleaseError, PnpState,
};
use kernel_api::device::*;
use kernel_api::{ApiGrantTokenV1, ApiTableHeaderV1};
use sched::sync::Spinlock;

use super::api_registry::ApiRegistryError;
use super::native::{
    NativeExecutionBounds, NativeIrqStackSet, current_callback_bounds, invoke_device_callback,
};

const DEVICE_RUNTIME_CAPACITY: usize = 256;

static KERNEL_DEVICE_API_V1: KernelDeviceApiV1 = KernelDeviceApiV1 {
    header: ApiTableHeaderV1::new::<KernelDeviceApiV1>(KERNEL_DEVICE_CAPABILITIES),
    enumerate: device_enumerate_v1,
    query_device: device_query_v1,
    query_resource: device_query_resource_v1,
    query_property: device_query_property_v1,
    enumerate_function: device_enumerate_function_v1,
    query_function: device_query_function_v1,
    invoke_function: device_invoke_function_v1,
    register_bus: device_register_bus_v1,
    unregister_bus: device_unregister_bus_v1,
    register_driver: device_register_driver_v1,
    unregister_driver: device_unregister_driver_v1,
    publish_device: device_publish_v1,
    remove_device: device_remove_v1,
    register_function_class: device_register_function_class_v1,
    unregister_function_class: device_unregister_function_class_v1,
    register_function: device_register_function_v1,
    unregister_function: device_unregister_function_v1,
    map_mmio: device_map_mmio_v1,
    unmap_mmio: device_unmap_mmio_v1,
    mmio_read: device_mmio_read_v1,
    mmio_write: device_mmio_write_v1,
    request_irq: device_request_irq_v1,
    release_irq: device_release_irq_v1,
    allocate_msi: device_allocate_msi_v1,
    release_msi: device_release_msi_v1,
    allocate_dma: device_allocate_dma_v1,
    sync_dma: device_sync_dma_v1,
    release_dma: device_release_dma_v1,
};

static KERNEL_DEVICE_NAMESPACE_V1: ElmApiNamespaceDescriptorV1 = ElmApiNamespaceDescriptorV1::new(
    KERNEL_DEVICE_API_IDENTIFIER,
    KERNEL_DEVICE_API_VERSION,
    ELM_API_NAMESPACE_FLAG_REQUIRE_GRANT,
    KERNEL_DEVICE_CAPABILITIES,
    &KERNEL_DEVICE_API_V1,
    KERNEL_DEVICE_LAYOUT_HASH_V1,
);

pub(crate) fn init() -> Result<(), ApiRegistryError> {
    DEVICE_RUNTIME.lock().initialize_capacity()?;
    general::dev::enumerate::subscribe_function_events(
        "elm",
        "device-runtime",
        device_function_event,
    )
    .map_err(|_| ApiRegistryError::OutOfMemory)?;
    general::dev::pnp::subscribe_device_events("elm", "device-runtime", device_pnp_event)
        .map_err(|_| ApiRegistryError::OutOfMemory)?;
    super::register_kernel_api_namespace(&KERNEL_DEVICE_NAMESPACE_V1)?;
    start_deferred_irq_worker();
    Ok(())
}

#[derive(Clone)]
struct CallbackRoute {
    owner: ElmId,
    generation: Generation,
    context: ElmCurrentContext,
    bounds: NativeExecutionBounds,
}

impl CallbackRoute {
    fn current(addresses: &[u64]) -> Result<Self, i32> {
        let context = elm_model::current_context().ok_or(KERNEL_DEVICE_STATUS_PERMISSION)?;
        let mut bounds = None;
        for address in addresses.iter().copied() {
            if address == 0 {
                continue;
            }
            let address = usize::try_from(address).map_err(|_| KERNEL_DEVICE_STATUS_INVALID)?;
            let candidate = current_callback_bounds(address).ok_or(KERNEL_DEVICE_STATUS_INVALID)?;
            if bounds.is_some_and(|existing: NativeExecutionBounds| {
                existing.code_start != candidate.code_start
                    || existing.code_end != candidate.code_end
                    || existing.image_start != candidate.image_start
                    || existing.image_end != candidate.image_end
            }) {
                return Err(KERNEL_DEVICE_STATUS_INVALID);
            }
            bounds = Some(candidate);
        }
        Ok(Self {
            owner: context.cell_id,
            generation: context.generation,
            context,
            bounds: bounds.ok_or(KERNEL_DEVICE_STATUS_INVALID)?,
        })
    }

    fn invoke<T>(&self, address: u64, phase: u32, frame: &mut T) -> i32 {
        let Ok(address) = usize::try_from(address) else {
            return KERNEL_DEVICE_STATUS_FAULT;
        };
        let Some((execution, context)) = super::core::try_reserve_device_callback_execution(
            self.owner,
            self.generation,
            self.context.phase,
        ) else {
            return KERNEL_DEVICE_STATUS_BUSY;
        };
        let status = invoke_device_callback(address, self.bounds, context, phase, frame);
        drop(execution);
        if status == KERNEL_DEVICE_STATUS_FAULT {
            self.report_fault();
        }
        status
    }

    fn report_fault(&self) {
        super::core::record_device_callback_fault(self.owner, self.generation);
        schedule_device_owner_fault_cleanup(self.owner, self.generation);
    }
}

#[derive(Clone)]
struct BusRecord {
    handle: KernelDeviceBusHandleV1,
    owner: ElmId,
    bus_type: BusType,
    identifier: Box<str>,
    device_contract: Box<str>,
    accepting: bool,
    in_flight: usize,
}

struct BusUseLease {
    handle: KernelDeviceBusHandleV1,
    owner: ElmId,
}

impl BusUseLease {
    fn acquire(bus: &mut BusRecord) -> Result<Self, i32> {
        bus.in_flight = bus
            .in_flight
            .checked_add(1)
            .ok_or(KERNEL_DEVICE_STATUS_BUSY)?;
        Ok(Self {
            handle: bus.handle,
            owner: bus.owner,
        })
    }
}

impl Drop for BusUseLease {
    fn drop(&mut self) {
        let mut runtime = DEVICE_RUNTIME.lock();
        let Some(bus) = runtime
            .buses
            .iter_mut()
            .find(|bus| bus.handle == self.handle && bus.owner == self.owner)
        else {
            log::error!(
                "[elm][device] dynamic bus disappeared with an active lease id={} generation={}",
                self.handle.id,
                self.handle.generation
            );
            return;
        };
        let Some(remaining) = bus.in_flight.checked_sub(1) else {
            log::error!(
                "[elm][device] dynamic bus lease underflow id={} generation={}",
                self.handle.id,
                self.handle.generation
            );
            return;
        };
        bus.in_flight = remaining;
    }
}

struct DriverRecord {
    handle: KernelDeviceDriverHandleV1,
    owner: ElmId,
    name: Box<str>,
    bus_type: BusType,
    pnp_handle: Option<DriverHandle>,
    proxy: Arc<ElmPnpDriver>,
}

#[derive(Clone)]
struct DeviceViewRecord {
    handle: KernelDeviceHandleV1,
    owner: ElmId,
    device: Arc<PnpDevice>,
}

#[derive(Clone)]
struct PublishedDeviceRecord {
    owner: ElmId,
    generation: Generation,
    bus_handle: KernelDeviceBusHandleV1,
    device: Arc<PnpDevice>,
}

#[derive(Clone)]
struct FunctionClassRecord {
    handle: KernelDeviceFunctionClassHandleV1,
    owner: ElmId,
    operation_contract: Box<str>,
    registration: FunctionClassRegistration,
}

struct FunctionRecord {
    handle: KernelDeviceFunctionHandleV1,
    owner: ElmId,
    device: Arc<PnpDevice>,
    proxy: Arc<ElmDeviceFunction>,
}

#[derive(Clone)]
struct FunctionViewRecord {
    handle: KernelDeviceFunctionHandleV1,
    owner: ElmId,
    device: Arc<PnpDevice>,
    function: Arc<dyn DeviceFunction>,
}

#[derive(Clone)]
struct MmioRecord {
    handle: KernelDeviceMmioHandleV1,
    owner: ElmId,
    device: Arc<PnpDevice>,
    virtual_address: usize,
    length: usize,
}

struct IrqRecord {
    handle: KernelDeviceIrqHandleV1,
    owner: ElmId,
    device: Arc<PnpDevice>,
    irq_handle: Option<IrqHandle>,
    line: IrqLine,
    shared: bool,
    proxy: Arc<ElmIrqHandler>,
    msi_source: Option<KernelDeviceMsiHandleV1>,
}

struct MsiRecord {
    handle: KernelDeviceMsiHandleV1,
    owner: ElmId,
    device: Arc<PnpDevice>,
    allocation: ElmMsiAllocation,
    line: IrqLine,
    irq_runtime_handle: Option<u64>,
    irq_detaching: bool,
    allocation_releasing: bool,
}

#[derive(Clone)]
enum ElmMsiAllocation {
    Generic(MsiHandle),
    Pci {
        device: PciDevice,
        handle: PciMsiHandle,
    },
}

impl ElmMsiAllocation {
    fn enable_irq(&self) -> Result<(), i32> {
        match self {
            Self::Generic(_) => Ok(()),
            Self::Pci { device, handle } => device
                .try_enable_configured_msi(*handle)
                .map_err(map_pci_msi_error),
        }
    }

    fn disable_irq(&self) -> Result<(), i32> {
        match self {
            Self::Generic(_) => Ok(()),
            Self::Pci { device, handle } => device
                .try_disable_configured_msi(*handle)
                .map_err(map_pci_msi_error),
        }
    }
}

struct DmaRecord {
    handle: KernelDeviceDmaHandleV1,
    owner: ElmId,
    device: Arc<PnpDevice>,
    buffer: DmaBuffer,
}

struct DeviceOwnerRecord {
    owner: ElmId,
    generation: Generation,
    resource_id: u64,
    registering: bool,
    fault_cleanup_pending: bool,
}

struct DeviceRuntime {
    next_id: u64,
    buses: Vec<BusRecord>,
    drivers: Vec<DriverRecord>,
    device_views: Vec<DeviceViewRecord>,
    published_devices: Vec<PublishedDeviceRecord>,
    function_classes: Vec<FunctionClassRecord>,
    functions: Vec<FunctionRecord>,
    function_views: Vec<FunctionViewRecord>,
    mmio: Vec<MmioRecord>,
    irqs: Vec<IrqRecord>,
    msi: Vec<MsiRecord>,
    dma: Vec<DmaRecord>,
    owners: Vec<DeviceOwnerRecord>,
}

impl DeviceRuntime {
    const fn new() -> Self {
        Self {
            next_id: 1,
            buses: Vec::new(),
            drivers: Vec::new(),
            device_views: Vec::new(),
            published_devices: Vec::new(),
            function_classes: Vec::new(),
            functions: Vec::new(),
            function_views: Vec::new(),
            mmio: Vec::new(),
            irqs: Vec::new(),
            msi: Vec::new(),
            dma: Vec::new(),
            owners: Vec::new(),
        }
    }

    fn initialize_capacity(&mut self) -> Result<(), ApiRegistryError> {
        reserve_runtime(&mut self.buses)?;
        reserve_runtime(&mut self.drivers)?;
        reserve_runtime(&mut self.device_views)?;
        reserve_runtime(&mut self.published_devices)?;
        reserve_runtime(&mut self.function_classes)?;
        reserve_runtime(&mut self.functions)?;
        reserve_runtime(&mut self.function_views)?;
        reserve_runtime(&mut self.mmio)?;
        reserve_runtime(&mut self.irqs)?;
        reserve_runtime(&mut self.msi)?;
        reserve_runtime(&mut self.dma)?;
        reserve_runtime(&mut self.owners)?;
        Ok(())
    }

    fn alloc_handle(&mut self, generation: Generation) -> Result<KernelDeviceHandleV1, i32> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(KERNEL_DEVICE_STATUS_NO_MEMORY)?;
        Ok(KernelDeviceHandleV1 {
            id,
            generation: generation.0,
        })
    }

    /// 为运行时记录取得一个有界槽位。
    ///
    /// 所有设备对象都可能由 ELM 动态创建，不能依赖 `Vec::push` 在低内存或超额
    /// 请求时隐式扩容。槽位预留必须在产生外部副作用前完成，调用方随后才可以
    /// 把已经建立的硬件对象提交到对应表中。
    fn reserve_slot<T>(records: &mut Vec<T>) -> Result<(), i32> {
        if records.len() >= DEVICE_RUNTIME_CAPACITY {
            return Err(KERNEL_DEVICE_STATUS_NO_MEMORY);
        }
        records
            .try_reserve(1)
            .map_err(|_| KERNEL_DEVICE_STATUS_NO_MEMORY)
    }

    fn begin_bus_use_by_identifier(
        &mut self,
        identifier: &str,
    ) -> Result<(BusType, Option<BusUseLease>), i32> {
        let builtin = match identifier {
            "pci" => Some(BusType::PCI),
            "usb" => Some(BusType::USB),
            "platform" => Some(BusType::PLATFORM),
            "generic" => Some(BusType::GENERIC),
            _ => None,
        };
        if let Some(bus_type) = builtin {
            return Ok((bus_type, None));
        }
        let bus = self
            .buses
            .iter_mut()
            .find(|bus| bus.accepting && bus.identifier.as_ref() == identifier)
            .ok_or(KERNEL_DEVICE_STATUS_NOT_FOUND)?;
        Ok((bus.bus_type, Some(BusUseLease::acquire(bus)?)))
    }

    fn device_view(
        &mut self,
        owner: ElmId,
        generation: Generation,
        device: &Arc<PnpDevice>,
    ) -> Result<KernelDeviceHandleV1, i32> {
        if let Some(view) = self.device_views.iter().find(|view| {
            view.owner == owner
                && view.handle.generation == generation.0
                && Arc::ptr_eq(&view.device, device)
        }) {
            return Ok(view.handle);
        }
        Self::reserve_slot(&mut self.device_views)?;
        let handle = self.alloc_handle(generation)?;
        self.device_views.push(DeviceViewRecord {
            handle,
            owner,
            device: Arc::clone(device),
        });
        Ok(handle)
    }

    fn resolve_device(
        &self,
        context: ElmCurrentContext,
        handle: KernelDeviceHandleV1,
    ) -> Result<Arc<PnpDevice>, i32> {
        let view = self
            .device_views
            .iter()
            .find(|view| {
                view.handle == handle
                    && view.owner == context.cell_id
                    && handle.generation == context.generation.0
            })
            .ok_or(KERNEL_DEVICE_STATUS_NOT_FOUND)?;
        if matches!(view.device.state(), PnpState::Removing | PnpState::Gone) {
            return Err(KERNEL_DEVICE_STATUS_NOT_FOUND);
        }
        Ok(Arc::clone(&view.device))
    }

    fn function_view(
        &mut self,
        owner: ElmId,
        generation: Generation,
        device: &Arc<PnpDevice>,
        function: &Arc<dyn DeviceFunction>,
    ) -> Result<KernelDeviceFunctionHandleV1, i32> {
        if let Some(view) = self.function_views.iter().find(|view| {
            view.owner == owner
                && view.handle.generation == generation.0
                && Arc::ptr_eq(&view.device, device)
                && Arc::ptr_eq(&view.function, function)
        }) {
            return Ok(view.handle);
        }
        Self::reserve_slot(&mut self.function_views)?;
        let handle = self.alloc_handle(generation)?;
        self.function_views.push(FunctionViewRecord {
            handle,
            owner,
            device: Arc::clone(device),
            function: Arc::clone(function),
        });
        Ok(handle)
    }

    fn resolve_function(
        &self,
        context: ElmCurrentContext,
        handle: KernelDeviceFunctionHandleV1,
    ) -> Result<FunctionViewRecord, i32> {
        let view = self
            .function_views
            .iter()
            .find(|view| {
                view.handle == handle
                    && view.owner == context.cell_id
                    && handle.generation == context.generation.0
            })
            .ok_or(KERNEL_DEVICE_STATUS_NOT_FOUND)?;
        Ok(view.clone())
    }

    fn forget_device(&mut self, device: &Arc<PnpDevice>) {
        self.device_views
            .retain(|view| !Arc::ptr_eq(&view.device, device));
        self.function_views
            .retain(|view| !Arc::ptr_eq(&view.device, device));
    }
}

static DEVICE_RUNTIME: Spinlock<DeviceRuntime> = Spinlock::new(DeviceRuntime::new());

fn device_function_event(event: &general::dev::enumerate::DeviceFunctionEvent) {
    if event.kind() != general::dev::enumerate::DeviceFunctionEventKind::Unregistered {
        return;
    }
    let mut runtime = DEVICE_RUNTIME.lock();
    if let Some(function) = event
        .function()
        .as_any()
        .downcast_ref::<ElmDeviceFunction>()
    {
        runtime
            .functions
            .retain(|record| record.handle != function.handle);
    }
    runtime
        .function_views
        .retain(|view| !Arc::ptr_eq(&view.function, event.function()));
}

fn device_pnp_event(event: &general::dev::pnp::PnpDeviceEvent) {
    if event.kind() != general::dev::pnp::PnpDeviceEventKind::Removed {
        return;
    }
    let mut runtime = DEVICE_RUNTIME.lock();
    runtime
        .published_devices
        .retain(|published| !Arc::ptr_eq(&published.device, event.device()));
    runtime.forget_device(event.device());
}

fn reserve_runtime<T>(records: &mut Vec<T>) -> Result<(), ApiRegistryError> {
    if records.capacity() < DEVICE_RUNTIME_CAPACITY {
        records
            .try_reserve_exact(DEVICE_RUNTIME_CAPACITY - records.capacity())
            .map_err(|_| ApiRegistryError::OutOfMemory)?;
    }
    Ok(())
}

fn try_collect_runtime<T>(iterator: impl Iterator<Item = T>) -> Result<Vec<T>, i32> {
    let mut iterator = iterator;
    let mut output = Vec::new();
    if let Some(upper) = iterator.size_hint().1 {
        output
            .try_reserve_exact(upper)
            .map_err(|_| KERNEL_DEVICE_STATUS_NO_MEMORY)?;
    }
    for item in iterator.by_ref() {
        if output.len() == output.capacity() {
            output
                .try_reserve(1)
                .map_err(|_| KERNEL_DEVICE_STATUS_NO_MEMORY)?;
        }
        output.push(item);
    }
    Ok(output)
}

fn copy_boxed(value: &str) -> Result<Box<str>, i32> {
    let mut out = String::new();
    out.try_reserve(value.len())
        .map_err(|_| KERNEL_DEVICE_STATUS_NO_MEMORY)?;
    out.push_str(value);
    Ok(out.into_boxed_str())
}

fn retire_published_device(device: &Arc<PnpDevice>) {
    device.remove_device();
    let mut runtime = DEVICE_RUNTIME.lock();
    runtime
        .published_devices
        .retain(|published| !Arc::ptr_eq(&published.device, device));
    runtime.forget_device(device);
}

fn with_authorized_device_call(
    token: ApiGrantTokenV1,
    capability: u64,
    call: impl FnOnce(ElmCurrentContext) -> i32,
) -> i32 {
    let Some(_domain) = general::elm_guard::enter_current_domain(
        general::elm_guard::ElmExecutionDomain::KernelCall,
    ) else {
        return KERNEL_DEVICE_STATUS_PERMISSION;
    };
    let context = match super::authorize_kernel_api_call(
        token,
        KERNEL_DEVICE_API_IDENTIFIER,
        KERNEL_DEVICE_API_VERSION,
        capability,
    ) {
        Ok(context) => context,
        Err(_) => return KERNEL_DEVICE_STATUS_PERMISSION,
    };
    call(context)
}

fn ensure_device_owner(context: ElmCurrentContext) -> i32 {
    {
        let mut runtime = DEVICE_RUNTIME.lock();
        if let Some(owner) = runtime
            .owners
            .iter()
            .find(|owner| owner.owner == context.cell_id && owner.generation == context.generation)
        {
            return if owner.registering {
                KERNEL_DEVICE_STATUS_BUSY
            } else {
                KERNEL_DEVICE_STATUS_OK
            };
        }
        if runtime.owners.len() >= DEVICE_RUNTIME_CAPACITY || runtime.owners.try_reserve(1).is_err()
        {
            return KERNEL_DEVICE_STATUS_NO_MEMORY;
        }
        runtime.owners.push(DeviceOwnerRecord {
            owner: context.cell_id,
            generation: context.generation,
            resource_id: 0,
            registering: true,
            fault_cleanup_pending: false,
        });
    }

    let resource_id = match super::register_owned_resource(
        context.cell_id,
        context.generation,
        ElmOwnedResourceKind::Device,
        context.cell_id.0,
        ElmOwnedResourceOpsV1::new(
            device_owner_suspend,
            device_owner_resume,
            device_owner_quiesce,
            device_owner_cancel,
            device_owner_drain,
            device_owner_release,
        ),
    ) {
        Ok(resource_id) => resource_id,
        Err(_) => {
            DEVICE_RUNTIME.lock().owners.retain(|owner| {
                owner.owner != context.cell_id || owner.generation != context.generation
            });
            return KERNEL_DEVICE_STATUS_PERMISSION;
        }
    };
    let mut runtime = DEVICE_RUNTIME.lock();
    let Some(owner) = runtime
        .owners
        .iter_mut()
        .find(|owner| owner.owner == context.cell_id && owner.generation == context.generation)
    else {
        drop(runtime);
        let _ = super::release_owned_resource(resource_id, context.cell_id, context.generation);
        return KERNEL_DEVICE_STATUS_BUSY;
    };
    owner.resource_id = resource_id;
    owner.registering = false;
    KERNEL_DEVICE_STATUS_OK
}

fn owner_records(owner: ElmId, generation: Generation) -> bool {
    DEVICE_RUNTIME.lock().owners.iter().any(|record| {
        record.owner == owner && record.generation == generation && !record.registering
    })
}

fn schedule_device_owner_fault_cleanup(owner: ElmId, generation: Generation) {
    let mut runtime = DEVICE_RUNTIME.lock();
    let Some(record) = runtime
        .owners
        .iter_mut()
        .find(|record| record.owner == owner && record.generation == generation)
    else {
        return;
    };
    record.fault_cleanup_pending = true;
    for bus in runtime
        .buses
        .iter_mut()
        .filter(|bus| bus.owner == owner && bus.handle.generation == generation.0)
    {
        bus.accepting = false;
    }
    for driver in runtime
        .drivers
        .iter()
        .filter(|driver| driver.owner == owner && driver.handle.generation == generation.0)
    {
        driver.proxy.accepting.store(false, Ordering::Release);
    }
    for function in runtime
        .functions
        .iter()
        .filter(|function| function.owner == owner && function.handle.generation == generation.0)
    {
        function.proxy.active.store(false, Ordering::Release);
    }
    for irq in runtime
        .irqs
        .iter()
        .filter(|irq| irq.owner == owner && irq.handle.generation == generation.0)
    {
        irq.proxy.active.store(false, Ordering::Release);
        irq.proxy.pending.store(0, Ordering::Release);
    }
    drop(runtime);
    DEVICE_IRQ_WORK_QUEUE.wake_one_default();
}

fn reentrant_device_teardown(context: ElmCurrentContext) -> bool {
    general::elm_guard::active_cell() == context.cell_id.0
        && matches!(
            general::elm_guard::active_phase(),
            general::elm_guard::ELM_GUARD_PHASE_DEVICE_MATCH
                | general::elm_guard::ELM_GUARD_PHASE_DEVICE_PROBE
                | general::elm_guard::ELM_GUARD_PHASE_DEVICE_REMOVE
                | general::elm_guard::ELM_GUARD_PHASE_DEVICE_IO
                | general::elm_guard::ELM_GUARD_PHASE_DEVICE_IRQ
                | general::elm_guard::ELM_GUARD_PHASE_DEVICE_DISCOVERY
        )
}

#[derive(Clone)]
struct ElmIrqSuspendRecord {
    runtime_handle: u64,
    kernel_handle: Option<IrqHandle>,
    line: IrqLine,
    shared: bool,
    proxy: Arc<ElmIrqHandler>,
    allocation: Option<ElmMsiAllocation>,
}

fn device_owner_irq_records(
    owner: ElmId,
    generation: Generation,
) -> Result<Vec<ElmIrqSuspendRecord>, i32> {
    let runtime = DEVICE_RUNTIME.lock();
    try_collect_runtime(
        runtime
            .irqs
            .iter()
            .filter(|irq| irq.owner == owner && irq.handle.generation == generation.0)
            .map(|irq| ElmIrqSuspendRecord {
                runtime_handle: irq.handle.id,
                kernel_handle: irq.irq_handle,
                line: irq.line,
                shared: irq.shared,
                proxy: Arc::clone(&irq.proxy),
                allocation: irq.msi_source.and_then(|msi| {
                    runtime
                        .msi
                        .iter()
                        .find(|record| record.handle == msi)
                        .map(|record| record.allocation.clone())
                }),
            }),
    )
}

fn set_runtime_irq_handle(runtime_handle: u64, handle: Option<IrqHandle>) -> Result<(), i32> {
    let mut runtime = DEVICE_RUNTIME.lock();
    let Some(record) = runtime
        .irqs
        .iter_mut()
        .find(|record| record.handle.id == runtime_handle)
    else {
        return Err(KERNEL_DEVICE_STATUS_NOT_FOUND);
    };
    record.irq_handle = handle;
    Ok(())
}

fn device_resource_rollback_failed(operation: &str, primary: i32, rollback: i32) -> i32 {
    log::error!(
        "[elm][device] {} rollback failed primary={} rollback={}",
        operation,
        primary,
        rollback
    );
    ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED
}

fn restore_irq_proxy_after_failed_suspend(record: &ElmIrqSuspendRecord, pending: u64) {
    record.proxy.pending.store(pending, Ordering::Release);
    record.proxy.active.store(true, Ordering::Release);
    if pending != 0 {
        DEVICE_IRQ_WORK_QUEUE.wake_one_default();
    }
}

fn register_suspended_irq(record: &ElmIrqSuspendRecord) -> Result<IrqHandle, i32> {
    if record.kernel_handle.is_some() {
        return Err(KERNEL_DEVICE_STATUS_BUSY);
    }
    record.proxy.active.store(true, Ordering::Release);
    let handler: Arc<dyn IrqHandler> = record.proxy.clone();
    let request = if record.shared {
        IrqRequest::shared(record.line, "elm-device", handler)
    } else {
        IrqRequest::exclusive(record.line, "elm-device", handler)
    };
    let handle = match register_irq_request(request) {
        Ok(handle) => handle,
        Err(_) => {
            record.proxy.active.store(false, Ordering::Release);
            return Err(KERNEL_DEVICE_STATUS_BUSY);
        }
    };
    if let Some(allocation) = record.allocation.as_ref()
        && let Err(status) = allocation.enable_irq()
    {
        record.proxy.active.store(false, Ordering::Release);
        if unregister_irq_handler(handle).is_err() {
            return Err(device_resource_rollback_failed(
                "enable IRQ",
                status,
                KERNEL_DEVICE_STATUS_BUSY,
            ));
        }
        return Err(status);
    }
    if let Err(status) = set_runtime_irq_handle(record.runtime_handle, Some(handle)) {
        let disable_status = record
            .allocation
            .as_ref()
            .map(ElmMsiAllocation::disable_irq)
            .unwrap_or(Ok(()));
        record.proxy.active.store(false, Ordering::Release);
        let unregister_status = unregister_irq_handler(handle);
        if let Err(error) = disable_status {
            return Err(device_resource_rollback_failed(
                "publish IRQ runtime handle",
                status,
                error,
            ));
        }
        if unregister_status.is_err() {
            return Err(device_resource_rollback_failed(
                "publish IRQ runtime handle",
                status,
                KERNEL_DEVICE_STATUS_BUSY,
            ));
        }
        return Err(status);
    }
    Ok(handle)
}

fn suspend_registered_irq(record: &ElmIrqSuspendRecord) -> Result<(), i32> {
    let pending = record.proxy.stop_and_drain();
    if let Some(allocation) = record.allocation.as_ref()
        && let Err(status) = allocation.disable_irq()
    {
        restore_irq_proxy_after_failed_suspend(record, pending);
        return Err(status);
    }
    let Some(handle) = record.kernel_handle else {
        restore_irq_proxy_after_failed_suspend(record, pending);
        return Err(KERNEL_DEVICE_STATUS_NOT_FOUND);
    };
    if unregister_irq_handler(handle).is_err() {
        let enable_status = record
            .allocation
            .as_ref()
            .map(ElmMsiAllocation::enable_irq)
            .unwrap_or(Ok(()));
        if let Err(error) = enable_status {
            return Err(device_resource_rollback_failed(
                "unregister IRQ",
                KERNEL_DEVICE_STATUS_BUSY,
                error,
            ));
        }
        restore_irq_proxy_after_failed_suspend(record, pending);
        return Err(KERNEL_DEVICE_STATUS_BUSY);
    }
    if let Err(status) = set_runtime_irq_handle(record.runtime_handle, None) {
        return Err(device_resource_rollback_failed(
            "clear IRQ runtime handle",
            status,
            KERNEL_DEVICE_STATUS_NOT_FOUND,
        ));
    }
    Ok(())
}

fn resume_irq_records(records: &mut [ElmIrqSuspendRecord]) -> Result<(), i32> {
    for index in 0..records.len() {
        match register_suspended_irq(&records[index]) {
            Ok(handle) => records[index].kernel_handle = Some(handle),
            Err(status) => {
                let mut rollback_failed = false;
                for rollback in records[..index].iter().rev() {
                    rollback_failed |= suspend_registered_irq(rollback).is_err();
                }
                if rollback_failed {
                    return Err(device_resource_rollback_failed(
                        "resume IRQ set",
                        status,
                        ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED,
                    ));
                }
                return Err(status);
            }
        }
    }
    Ok(())
}

fn set_device_owner_accepting(
    owner: ElmId,
    generation: Generation,
    accepting: bool,
) -> Result<(), i32> {
    let (drivers, functions) = {
        let mut runtime = DEVICE_RUNTIME.lock();
        let drivers = try_collect_runtime(
            runtime
                .drivers
                .iter()
                .filter(|driver| driver.owner == owner && driver.handle.generation == generation.0)
                .map(|driver| Arc::clone(&driver.proxy)),
        )?;
        let functions = try_collect_runtime(
            runtime
                .functions
                .iter()
                .filter(|function| {
                    function.owner == owner && function.handle.generation == generation.0
                })
                .map(|function| Arc::clone(&function.proxy)),
        )?;
        for bus in runtime
            .buses
            .iter_mut()
            .filter(|bus| bus.owner == owner && bus.handle.generation == generation.0)
        {
            bus.accepting = accepting;
        }
        (drivers, functions)
    };
    for driver in drivers {
        driver.accepting.store(accepting, Ordering::Release);
    }
    for function in functions {
        if accepting {
            function.resume_runtime();
        } else {
            function.suspend_runtime();
        }
    }
    Ok(())
}

fn device_owner_suspend(owner: ElmId, generation: Generation, _handle: u64) -> Result<(), i32> {
    if !owner_records(owner, generation) {
        return Ok(());
    }
    let mut irqs = device_owner_irq_records(owner, generation)?;
    set_device_owner_accepting(owner, generation, false)?;
    for index in 0..irqs.len() {
        if let Err(status) = suspend_registered_irq(&irqs[index]) {
            let rollback = resume_irq_records(&mut irqs[..index]);
            if status == ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED || rollback.is_err() {
                return Err(device_resource_rollback_failed(
                    "suspend device IRQ set",
                    status,
                    rollback
                        .err()
                        .unwrap_or(ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED),
                ));
            }
            if let Err(error) = set_device_owner_accepting(owner, generation, true) {
                return Err(device_resource_rollback_failed(
                    "restore device acceptance",
                    status,
                    error,
                ));
            }
            return Err(status);
        }
        irqs[index].kernel_handle = None;
    }
    Ok(())
}

fn device_owner_resume(owner: ElmId, generation: Generation, _handle: u64) -> Result<(), i32> {
    if !owner_records(owner, generation) {
        return Ok(());
    }
    let mut irqs = device_owner_irq_records(owner, generation)?;
    resume_irq_records(&mut irqs)?;
    if let Err(status) = set_device_owner_accepting(owner, generation, true) {
        let mut rollback_status = None;
        for irq in irqs.iter().rev() {
            if let Err(error) = suspend_registered_irq(irq) {
                rollback_status.get_or_insert(error);
            }
        }
        return Err(match rollback_status {
            Some(error) => {
                device_resource_rollback_failed("restore resumed device IRQ set", status, error)
            }
            None => status,
        });
    }
    Ok(())
}

fn device_owner_quiesce(owner: ElmId, generation: Generation, _handle: u64) -> Result<(), i32> {
    if !owner_records(owner, generation) {
        return Ok(());
    }
    let (functions, irqs) = {
        let mut runtime = DEVICE_RUNTIME.lock();
        let functions = try_collect_runtime(
            runtime
                .functions
                .iter()
                .filter(|function| {
                    function.owner == owner && function.handle.generation == generation.0
                })
                .map(|function| Arc::clone(&function.proxy)),
        )?;
        let irqs = try_collect_runtime(
            runtime
                .irqs
                .iter()
                .filter(|irq| irq.owner == owner && irq.handle.generation == generation.0)
                .map(|irq| Arc::clone(&irq.proxy)),
        )?;
        for bus in runtime
            .buses
            .iter_mut()
            .filter(|bus| bus.owner == owner && bus.handle.generation == generation.0)
        {
            bus.accepting = false;
        }
        for driver in runtime
            .drivers
            .iter_mut()
            .filter(|driver| driver.owner == owner && driver.handle.generation == generation.0)
        {
            driver.proxy.accepting.store(false, Ordering::Release);
        }
        (functions, irqs)
    };
    for function in functions {
        function.mark_gone();
    }
    for irq in irqs {
        irq.active.store(false, Ordering::Release);
        irq.pending.store(0, Ordering::Release);
    }
    Ok(())
}

fn device_owner_cancel(owner: ElmId, generation: Generation, _handle: u64) -> Result<(), i32> {
    if !owner_records(owner, generation) {
        return Ok(());
    }
    let drivers = {
        let runtime = DEVICE_RUNTIME.lock();
        try_collect_runtime(
            runtime
                .drivers
                .iter()
                .filter(|driver| driver.owner == owner && driver.handle.generation == generation.0)
                .map(|driver| driver.handle),
        )?
    };
    for handle in drivers {
        let status = release_driver(owner, generation, handle);
        if status != KERNEL_DEVICE_STATUS_OK && status != KERNEL_DEVICE_STATUS_NOT_FOUND {
            return Err(status);
        }
    }

    let devices = {
        let runtime = DEVICE_RUNTIME.lock();
        try_collect_runtime(
            runtime
                .published_devices
                .iter()
                .filter(|device| device.owner == owner && device.generation == generation)
                .map(|device| Arc::clone(&device.device)),
        )?
    };
    for device in devices {
        device.remove_device();
        DEVICE_RUNTIME
            .lock()
            .published_devices
            .retain(|published| !Arc::ptr_eq(&published.device, &device));
    }

    let irqs = {
        let runtime = DEVICE_RUNTIME.lock();
        try_collect_runtime(
            runtime
                .irqs
                .iter()
                .filter(|irq| irq.owner == owner && irq.handle.generation == generation.0)
                .map(|irq| irq.handle),
        )?
    };
    for handle in irqs {
        let _ =
            release_device_attached_resource(owner, generation, AttachedResourceKind::Irq, handle);
    }

    let functions = {
        let runtime = DEVICE_RUNTIME.lock();
        try_collect_runtime(
            runtime
                .functions
                .iter()
                .filter(|function| {
                    function.owner == owner && function.handle.generation == generation.0
                })
                .map(|function| {
                    (
                        function.handle,
                        Arc::clone(&function.device),
                        function.proxy.class_id,
                        function.proxy.name.clone(),
                    )
                }),
        )?
    };
    for (_handle, device, class_id, name) in functions {
        let _ = device.unregister_function(class_id, &name);
    }
    DEVICE_RUNTIME
        .lock()
        .functions
        .retain(|function| function.owner != owner || function.handle.generation != generation.0);
    DEVICE_RUNTIME
        .lock()
        .function_views
        .retain(|view| view.owner != owner || view.handle.generation != generation.0);

    let resources = {
        let runtime = DEVICE_RUNTIME.lock();
        try_collect_runtime(
            runtime
                .mmio
                .iter()
                .filter(|record| record.owner == owner && record.handle.generation == generation.0)
                .map(|record| {
                    (
                        AttachedResourceKind::Mmio,
                        record.handle,
                        Arc::clone(&record.device),
                    )
                })
                .chain(
                    runtime
                        .msi
                        .iter()
                        .filter(|record| {
                            record.owner == owner && record.handle.generation == generation.0
                        })
                        .map(|record| {
                            (
                                AttachedResourceKind::Msi,
                                record.handle,
                                Arc::clone(&record.device),
                            )
                        }),
                )
                .chain(
                    runtime
                        .dma
                        .iter()
                        .filter(|record| {
                            record.owner == owner && record.handle.generation == generation.0
                        })
                        .map(|record| {
                            (
                                AttachedResourceKind::Dma,
                                record.handle,
                                Arc::clone(&record.device),
                            )
                        }),
                ),
        )?
    };
    for (kind, handle, device) in resources {
        let _ = device.release_owned_resource(handle.id);
        release_attached_resource(kind, handle.id);
    }

    let classes = {
        let runtime = DEVICE_RUNTIME.lock();
        try_collect_runtime(
            runtime
                .function_classes
                .iter()
                .filter(|class| class.owner == owner && class.handle.generation == generation.0)
                .map(|class| (class.handle, class.registration.class_id())),
        )?
    };
    for (_handle, class_id) in classes {
        let _ = general::dev::function::unregister_function_class(class_id);
    }

    DEVICE_RUNTIME
        .lock()
        .function_classes
        .retain(|class| class.owner != owner || class.handle.generation != generation.0);
    let buses = {
        let runtime = DEVICE_RUNTIME.lock();
        try_collect_runtime(
            runtime
                .buses
                .iter()
                .filter(|bus| bus.owner == owner && bus.handle.generation == generation.0)
                .map(|bus| bus.handle),
        )?
    };
    for handle in buses {
        let status = release_bus(owner, generation, handle, false);
        if status != KERNEL_DEVICE_STATUS_OK && status != KERNEL_DEVICE_STATUS_NOT_FOUND {
            return Err(status);
        }
    }
    Ok(())
}

fn device_owner_drain(owner: ElmId, generation: Generation, _handle: u64) -> Result<(), i32> {
    let (drivers, functions, irqs) = {
        let runtime = DEVICE_RUNTIME.lock();
        let drivers = try_collect_runtime(
            runtime
                .drivers
                .iter()
                .filter(|driver| driver.owner == owner && driver.handle.generation == generation.0)
                .map(|driver| Arc::clone(&driver.proxy)),
        )?;
        let functions = try_collect_runtime(
            runtime
                .functions
                .iter()
                .filter(|function| {
                    function.owner == owner && function.handle.generation == generation.0
                })
                .map(|function| Arc::clone(&function.proxy)),
        )?;
        let irqs = try_collect_runtime(
            runtime
                .irqs
                .iter()
                .filter(|irq| irq.owner == owner && irq.handle.generation == generation.0)
                .map(|irq| Arc::clone(&irq.proxy)),
        )?;
        (drivers, functions, irqs)
    };
    for driver in drivers {
        driver.drain_callbacks();
    }
    for function in functions {
        function.drain_io();
    }
    for irq in irqs {
        let _ = irq.stop_and_drain();
    }
    Ok(())
}

fn device_owner_release(owner: ElmId, generation: Generation, _handle: u64) -> Result<(), i32> {
    release_owner_runtime_resources(owner, generation, AttachedResourceKind::Irq);
    release_owner_runtime_resources(owner, generation, AttachedResourceKind::Msi);
    release_owner_runtime_resources(owner, generation, AttachedResourceKind::Dma);
    release_owner_runtime_resources(owner, generation, AttachedResourceKind::Mmio);
    DEVICE_RUNTIME
        .lock()
        .device_views
        .retain(|view| view.owner != owner || view.handle.generation != generation.0);
    DEVICE_RUNTIME
        .lock()
        .owners
        .retain(|record| record.owner != owner || record.generation != generation);
    Ok(())
}

fn release_owner_runtime_resources(
    owner: ElmId,
    generation: Generation,
    kind: AttachedResourceKind,
) {
    loop {
        let runtime_handle = {
            let runtime = DEVICE_RUNTIME.lock();
            match kind {
                AttachedResourceKind::Mmio => runtime
                    .mmio
                    .iter()
                    .find(|record| {
                        record.owner == owner && record.handle.generation == generation.0
                    })
                    .map(|record| record.handle.id),
                AttachedResourceKind::Irq => runtime
                    .irqs
                    .iter()
                    .find(|record| {
                        record.owner == owner && record.handle.generation == generation.0
                    })
                    .map(|record| record.handle.id),
                AttachedResourceKind::Msi => runtime
                    .msi
                    .iter()
                    .find(|record| {
                        record.owner == owner && record.handle.generation == generation.0
                    })
                    .map(|record| record.handle.id),
                AttachedResourceKind::Dma => runtime
                    .dma
                    .iter()
                    .find(|record| {
                        record.owner == owner && record.handle.generation == generation.0
                    })
                    .map(|record| record.handle.id),
            }
        };
        let Some(runtime_handle) = runtime_handle else {
            break;
        };
        release_attached_resource(kind, runtime_handle);
    }
}

fn valid_range<T>(pointer: *const T, write: bool) -> bool {
    !pointer.is_null()
        && (pointer as usize) % core::mem::align_of::<T>() == 0
        && general::elm_guard::validate_current_memory_range(
            pointer as usize,
            core::mem::size_of::<T>(),
            write,
        )
}

fn read_input<T: Copy>(pointer: *const T) -> Result<T, i32> {
    if !valid_range(pointer, false) {
        return Err(KERNEL_DEVICE_STATUS_INVALID);
    }
    // Safety: 输入指针已经通过当前 ELM 可读范围、尺寸和对齐校验。
    Ok(unsafe { pointer.read() })
}

fn write_output<T>(pointer: *mut T, value: T) -> Result<(), i32> {
    if !valid_range(pointer.cast_const(), true) {
        return Err(KERNEL_DEVICE_STATUS_INVALID);
    }
    // Safety: 输出指针已经通过当前 ELM 可写范围、尺寸和对齐校验。
    unsafe { pointer.write(value) };
    Ok(())
}

fn pnp_state_code(state: PnpState) -> u32 {
    match state {
        PnpState::Discovered => 1,
        PnpState::Probing => 2,
        PnpState::Bound => 3,
        PnpState::Removing => 4,
        PnpState::Gone => 5,
    }
}

fn snapshot_for(
    owner: ElmId,
    generation: Generation,
    device: &Arc<PnpDevice>,
) -> Result<KernelDeviceSnapshotV1, i32> {
    let (handle, parent) = {
        let mut runtime = DEVICE_RUNTIME.lock();
        let handle = runtime.device_view(owner, generation, device)?;
        let parent = match device.parent() {
            Some(parent) => runtime.device_view(owner, generation, &parent)?,
            None => KernelDeviceHandleV1::default(),
        };
        (handle, parent)
    };
    let bus = KernelDeviceIdentifierV1::new(device.info.bus_name())
        .ok_or(KERNEL_DEVICE_STATUS_INVALID)?;
    let name = KernelDeviceNameV1::new(&device.name).ok_or(KERNEL_DEVICE_STATUS_INVALID)?;
    let identity_contract = match &device.id {
        PnpId::Pci { .. } => "kernel.device.identity.pci",
        PnpId::Usb { .. } => "kernel.device.identity.usb",
        PnpId::Platform { .. } => "kernel.device.identity.platform",
        PnpId::Dynamic { contract, .. } => contract,
    };
    let identity_contract =
        KernelDeviceIdentifierV1::new(identity_contract).ok_or(KERNEL_DEVICE_STATUS_INVALID)?;
    let (identity, identity_len) = device_identity(&device)?;
    let property_count = match &device.id {
        PnpId::Dynamic { .. } => device
            .info
            .as_any()
            .downcast_ref::<DynamicPnpBusInfo>()
            .map(|info| info.properties().len())
            .unwrap_or(0),
        _ => 0,
    };
    let function_count = device.function_count();
    Ok(KernelDeviceSnapshotV1 {
        struct_size: core::mem::size_of::<KernelDeviceSnapshotV1>() as u32,
        state: pnp_state_code(device.state()),
        handle,
        parent,
        bus,
        name,
        identity_contract,
        resource_count: device_resource_count(device).min(u32::MAX as usize) as u32,
        function_count: function_count.min(u32::MAX as usize) as u32,
        identity_len,
        identity,
        bound: u32::from(device.is_bound()),
        property_count: property_count.min(u32::MAX as usize) as u32,
        reserved0: 0,
    })
}

fn device_identity(
    device: &Arc<PnpDevice>,
) -> Result<([u8; KERNEL_DEVICE_IDENTITY_LEN], u32), i32> {
    let mut identity = [0u8; KERNEL_DEVICE_IDENTITY_LEN];
    let length = match &device.id {
        PnpId::Pci {
            segment,
            bus,
            device: slot,
            function,
        } => {
            identity[0..2].copy_from_slice(&segment.to_le_bytes());
            identity[2] = *bus;
            identity[3] = *slot;
            identity[4] = *function;
            if let Some(info) = device
                .info
                .as_any()
                .downcast_ref::<general::dev::pci::PciInfo>()
            {
                identity[5..7].copy_from_slice(&info.vendor.to_le_bytes());
                identity[7..9].copy_from_slice(&info.device_id.to_le_bytes());
                identity[9] = info.revision;
                identity[10..14].copy_from_slice(&info.class.to_le_bytes());
                identity[14] = info.subclass;
                identity[15] = info.prog_if;
                identity[16..18].copy_from_slice(&info.subsystem_vendor.to_le_bytes());
                identity[18..20].copy_from_slice(&info.subsystem_id.to_le_bytes());
                identity[20] = info.header_type;
                21
            } else {
                5
            }
        }
        PnpId::Usb {
            bus_id,
            address,
            interface,
        } => {
            identity[0] = *bus_id;
            identity[1] = *address;
            identity[2] = interface.unwrap_or(u8::MAX);
            3
        }
        PnpId::Platform { name, .. } => {
            if name.len() > KERNEL_DEVICE_IDENTITY_LEN {
                return Err(KERNEL_DEVICE_STATUS_INVALID);
            }
            identity[..name.len()].copy_from_slice(name.as_bytes());
            name.len()
        }
        PnpId::Dynamic {
            identity: source, ..
        } => {
            if source.len() > KERNEL_DEVICE_IDENTITY_LEN {
                return Err(KERNEL_DEVICE_STATUS_INVALID);
            }
            identity[..source.len()].copy_from_slice(source);
            source.len()
        }
    };
    Ok((identity, length as u32))
}

fn function_snapshot_for(
    owner: ElmId,
    generation: Generation,
    device: &Arc<PnpDevice>,
    function: &Arc<dyn DeviceFunction>,
) -> Result<KernelDeviceFunctionSnapshotV1, i32> {
    let (device_handle, function_handle) = {
        let mut runtime = DEVICE_RUNTIME.lock();
        let device_handle = runtime.device_view(owner, generation, device)?;
        let function_handle = runtime.function_view(owner, generation, device, function)?;
        (device_handle, function_handle)
    };
    let class =
        KernelDeviceIdentifierV1::new(function.class_name()).ok_or(KERNEL_DEVICE_STATUS_INVALID)?;
    let name = KernelDeviceNameV1::new(function.dev_name()).ok_or(KERNEL_DEVICE_STATUS_INVALID)?;
    let operation_contract = match function.operation_contract() {
        Some(contract) => {
            KernelDeviceIdentifierV1::new(contract).ok_or(KERNEL_DEVICE_STATUS_INVALID)?
        }
        None => KernelDeviceIdentifierV1::empty(),
    };
    Ok(KernelDeviceFunctionSnapshotV1 {
        struct_size: core::mem::size_of::<KernelDeviceFunctionSnapshotV1>() as u32,
        flags: 0,
        handle: function_handle,
        device: device_handle,
        class,
        name,
        operation_contract,
        active: 1,
        reserved0: 0,
    })
}

fn function_view_is_active(view: &FunctionViewRecord) -> Result<bool, i32> {
    if matches!(view.device.state(), PnpState::Removing | PnpState::Gone) {
        return Ok(false);
    }
    let functions = view
        .device
        .try_functions()
        .ok_or(KERNEL_DEVICE_STATUS_NO_MEMORY)?;
    Ok(functions
        .iter()
        .any(|function| Arc::ptr_eq(function, &view.function)))
}

fn device_resource_count(device: &Arc<PnpDevice>) -> usize {
    match &device.id {
        PnpId::Platform { .. } => device
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .map(|info| info.resources.len())
            .unwrap_or(0),
        PnpId::Pci { .. } => PciDevice::from_pnp(device)
            .map(|pci| {
                (0..6)
                    .filter_map(|index| pci.map_bar(index))
                    .filter(|bar| matches!(bar.bar_type, PciBarType::Memory))
                    .count()
            })
            .unwrap_or(0),
        PnpId::Dynamic { .. } => device
            .info
            .as_any()
            .downcast_ref::<DynamicPnpBusInfo>()
            .map(|info| info.resources().len())
            .unwrap_or(0),
        PnpId::Usb { .. } => 0,
    }
}

fn resource_at(device: &Arc<PnpDevice>, ordinal: usize) -> Result<KernelDeviceResourceV1, i32> {
    match &device.id {
        PnpId::Platform { .. } => {
            let info = device
                .info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .ok_or(KERNEL_DEVICE_STATUS_INVALID)?;
            let resource = info
                .resources
                .get(ordinal)
                .ok_or(KERNEL_DEVICE_STATUS_NOT_FOUND)?;
            platform_resource(resource, ordinal)
        }
        PnpId::Pci { .. } => {
            let pci = PciDevice::from_pnp(device).ok_or(KERNEL_DEVICE_STATUS_INVALID)?;
            let mut current = 0usize;
            for bar_index in 0..6 {
                let Some(bar) = pci.map_bar(bar_index) else {
                    continue;
                };
                if !matches!(bar.bar_type, PciBarType::Memory) {
                    continue;
                }
                if current == ordinal {
                    let mut out = KernelDeviceResourceV1::empty();
                    out.kind = KERNEL_DEVICE_RESOURCE_MMIO;
                    out.index = bar_index as u32;
                    out.start = bar.phys_addr;
                    out.length = bar.size;
                    out.flags = u64::from(bar.prefetchable);
                    return Ok(out);
                }
                current += 1;
            }
            Err(KERNEL_DEVICE_STATUS_NOT_FOUND)
        }
        PnpId::Dynamic { .. } => {
            let info = device
                .info
                .as_any()
                .downcast_ref::<DynamicPnpBusInfo>()
                .ok_or(KERNEL_DEVICE_STATUS_INVALID)?;
            let resource = info
                .resources()
                .get(ordinal)
                .ok_or(KERNEL_DEVICE_STATUS_NOT_FOUND)?;
            dynamic_resource(resource)
        }
        PnpId::Usb { .. } => Err(KERNEL_DEVICE_STATUS_NOT_FOUND),
    }
}

fn property_at(device: &Arc<PnpDevice>, ordinal: usize) -> Result<KernelDevicePropertyV1, i32> {
    let info = device
        .info
        .as_any()
        .downcast_ref::<DynamicPnpBusInfo>()
        .ok_or(KERNEL_DEVICE_STATUS_NOT_FOUND)?;
    let property = info
        .properties()
        .get(ordinal)
        .ok_or(KERNEL_DEVICE_STATUS_NOT_FOUND)?;
    if property.name.len() > KERNEL_DEVICE_IDENTIFIER_LEN
        || property.value.len() > KERNEL_DEVICE_PROPERTY_VALUE_LEN
    {
        return Err(KERNEL_DEVICE_STATUS_INVALID);
    }
    let name = KernelDeviceIdentifierV1::new(&property.name).ok_or(KERNEL_DEVICE_STATUS_INVALID)?;
    let mut output = KernelDevicePropertyV1::default();
    output.name = name;
    output.value_len = property.value.len() as u32;
    output.value[..property.value.len()].copy_from_slice(&property.value);
    Ok(output)
}

fn platform_resource(
    resource: &DeviceResource,
    ordinal: usize,
) -> Result<KernelDeviceResourceV1, i32> {
    let mut out = KernelDeviceResourceV1::empty();
    out.index = ordinal as u32;
    match resource {
        DeviceResource::Mmio { phys, size } => {
            out.kind = KERNEL_DEVICE_RESOURCE_MMIO;
            out.start = *phys as u64;
            out.length = *size as u64;
        }
        DeviceResource::Irq {
            controller,
            cells,
            attributes,
        } => {
            out.kind = KERNEL_DEVICE_RESOURCE_IRQ;
            out.start = controller.map(u64::from).unwrap_or(u64::MAX);
            out.flags = u64::from(attributes.wake_capable);
            let bytes = cells.len().saturating_mul(core::mem::size_of::<u32>());
            if bytes > out.payload.len() {
                return Err(KERNEL_DEVICE_STATUS_INVALID);
            }
            out.payload_len = bytes as u32;
            for (index, cell) in cells.iter().enumerate() {
                let start = index * 4;
                out.payload[start..start + 4].copy_from_slice(&cell.to_le_bytes());
            }
        }
    }
    Ok(out)
}

fn dynamic_resource(resource: &DynamicPnpResource) -> Result<KernelDeviceResourceV1, i32> {
    if resource.payload.len() > KERNEL_DEVICE_RESOURCE_PAYLOAD_LEN {
        return Err(KERNEL_DEVICE_STATUS_INVALID);
    }
    let mut out = KernelDeviceResourceV1::empty();
    out.kind = resource.kind;
    out.index = resource.index;
    out.start = resource.start;
    out.length = resource.length;
    out.flags = resource.flags;
    out.payload_len = resource.payload.len() as u32;
    out.payload[..resource.payload.len()].copy_from_slice(&resource.payload);
    Ok(out)
}

fn validate_published_resource(resource: &KernelDeviceResourceV1) -> bool {
    resource.has_valid_dynamic_encoding()
}

struct ElmPnpDriver {
    name: Box<str>,
    bus_type: BusType,
    priority: PnpDriverPriority,
    route: CallbackRoute,
    match_callback: u64,
    probe_callback: u64,
    remove_callback: u64,
    accepting: AtomicBool,
    in_flight: AtomicU64,
}

struct ElmDriverCallbackGuard<'a>(&'a AtomicU64);

impl Drop for ElmDriverCallbackGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ElmPnpDriver {
    fn begin_callback(&self, require_accepting: bool) -> Option<ElmDriverCallbackGuard<'_>> {
        if require_accepting && !self.accepting.load(Ordering::Acquire) {
            return None;
        }
        self.in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |in_flight| {
                in_flight.checked_add(1)
            })
            .ok()?;
        if require_accepting && !self.accepting.load(Ordering::Acquire) {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(ElmDriverCallbackGuard(&self.in_flight))
    }

    fn drain_callbacks(&self) {
        while self.in_flight.load(Ordering::Acquire) != 0 {
            let _ = sched::operation::sched_yield();
        }
    }

    fn match_frame(&self, device: &Arc<PnpDevice>) -> Result<KernelDeviceMatchFrameV1, i32> {
        Ok(KernelDeviceMatchFrameV1 {
            struct_size: core::mem::size_of::<KernelDeviceMatchFrameV1>() as u32,
            flags: 0,
            cell_id: self.route.owner.0,
            generation: self.route.generation.0,
            device: snapshot_for(self.route.owner, self.route.generation, device)?,
            matched: 0,
            reserved0: 0,
        })
    }

    fn matches_device_callback(&self, device: &Arc<PnpDevice>) -> bool {
        if !self.accepting.load(Ordering::Acquire)
            || (self.bus_type != BusType::GENERIC && device.info.bus_type() != self.bus_type)
        {
            return false;
        }
        let Some(_in_flight) = self.begin_callback(true) else {
            return false;
        };
        let Ok(mut frame) = self.match_frame(device) else {
            return false;
        };
        let status = self.route.invoke(
            self.match_callback,
            general::elm_guard::ELM_GUARD_PHASE_DEVICE_MATCH,
            &mut frame,
        );
        let frame_valid = frame.struct_size
            == core::mem::size_of::<KernelDeviceMatchFrameV1>() as u32
            && frame.flags == 0
            && frame.cell_id == self.route.owner.0
            && frame.generation == self.route.generation.0
            && frame.reserved0 == 0
            && frame.matched <= 1;
        if status == KERNEL_DEVICE_STATUS_OK && !frame_valid {
            self.route.report_fault();
        }
        status == KERNEL_DEVICE_STATUS_OK && frame_valid && frame.matched == 1
    }
}

impl PnpDriver for ElmPnpDriver {
    fn name(&self) -> &str {
        &self.name
    }

    fn bus_type(&self) -> BusType {
        self.bus_type
    }

    fn priority(&self) -> PnpDriverPriority {
        self.priority
    }

    fn matches(&self, _id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        if !self.accepting.load(Ordering::Acquire)
            || (self.bus_type != BusType::GENERIC && info.bus_type() != self.bus_type)
        {
            return false;
        }
        let Some(device) = PNP_DEVICES.try_list().and_then(|devices| {
            devices
                .into_iter()
                .find(|device| core::ptr::eq(device.info.as_ref(), info))
        }) else {
            return false;
        };
        self.matches_device_callback(&device)
    }

    fn matches_device(&self, device: &Arc<PnpDevice>) -> bool {
        self.matches_device_callback(device)
    }

    fn probe(&self, device: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let Some(_in_flight) = self.begin_callback(true) else {
            return Err(PnpError::InvalidState);
        };
        let snapshot = snapshot_for(self.route.owner, self.route.generation, device)
            .map_err(|_| PnpError::OutOfMemory)?;
        let mut frame = KernelDeviceProbeFrameV1 {
            struct_size: core::mem::size_of::<KernelDeviceProbeFrameV1>() as u32,
            flags: 0,
            cell_id: self.route.owner.0,
            generation: self.route.generation.0,
            device: snapshot,
            status: KERNEL_DEVICE_STATUS_FAULT,
            reserved0: 0,
        };
        let call_status = self.route.invoke(
            self.probe_callback,
            general::elm_guard::ELM_GUARD_PHASE_DEVICE_PROBE,
            &mut frame,
        );
        let frame_valid = frame.struct_size
            == core::mem::size_of::<KernelDeviceProbeFrameV1>() as u32
            && frame.flags == 0
            && frame.cell_id == self.route.owner.0
            && frame.generation == self.route.generation.0
            && frame.reserved0 == 0;
        if call_status == KERNEL_DEVICE_STATUS_OK && !frame_valid {
            self.route.report_fault();
        }
        if call_status != KERNEL_DEVICE_STATUS_OK || !frame_valid {
            return Err(PnpError::hardware_failure(
                "ELM device probe callback fault",
            ));
        }
        map_probe_status(frame.status)
    }

    fn remove(&self, device: &Arc<PnpDevice>) {
        let Some(_in_flight) = self.begin_callback(false) else {
            log::error!("[elm][device] driver callback counter exhausted during remove");
            return;
        };
        let Ok(snapshot) = snapshot_for(self.route.owner, self.route.generation, device) else {
            return;
        };
        let mut frame = KernelDeviceRemoveFrameV1 {
            struct_size: core::mem::size_of::<KernelDeviceRemoveFrameV1>() as u32,
            flags: 0,
            cell_id: self.route.owner.0,
            generation: self.route.generation.0,
            device: snapshot,
            status: KERNEL_DEVICE_STATUS_FAULT,
            reserved0: 0,
        };
        let status = self.route.invoke(
            self.remove_callback,
            general::elm_guard::ELM_GUARD_PHASE_DEVICE_REMOVE,
            &mut frame,
        );
        let frame_valid = frame.struct_size
            == core::mem::size_of::<KernelDeviceRemoveFrameV1>() as u32
            && frame.flags == 0
            && frame.cell_id == self.route.owner.0
            && frame.generation == self.route.generation.0
            && frame.reserved0 == 0;
        if status == KERNEL_DEVICE_STATUS_OK && !frame_valid {
            self.route.report_fault();
        }
        if status != KERNEL_DEVICE_STATUS_OK
            || !frame_valid
            || frame.status != KERNEL_DEVICE_STATUS_OK
        {
            log::error!(
                "[elm][device] remove callback failed driver={} cell={} generation={} status={} callback_status={}",
                self.name,
                self.route.owner.0,
                self.route.generation.0,
                frame.status,
                status
            );
        }
    }
}

struct ElmDriverFactory {
    driver: Arc<ElmPnpDriver>,
}

impl DriverFactory for ElmDriverFactory {
    fn name(&self) -> &str {
        &self.driver.name
    }

    fn create(
        &self,
        _context: &general::dev::pnp::DevInitContext,
    ) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::clone(&self.driver) as Arc<dyn PnpDriver>)
    }
}

fn map_probe_status(status: i32) -> Result<(), PnpError> {
    match status {
        KERNEL_DEVICE_STATUS_OK => Ok(()),
        KERNEL_DEVICE_STATUS_DEFERRED => Err(PnpError::ProbeDeferred),
        KERNEL_DEVICE_STATUS_NO_MEMORY => Err(PnpError::OutOfMemory),
        KERNEL_DEVICE_STATUS_UNSUPPORTED => Err(PnpError::unsupported("ELM device probe")),
        KERNEL_DEVICE_STATUS_NOT_FOUND => Err(PnpError::missing(
            PnpResourceKind::Other("elm-device"),
            "ELM probe resource missing",
        )),
        _ => Err(PnpError::ProbeFailed),
    }
}

struct ElmDeviceFunction {
    handle: KernelDeviceFunctionHandleV1,
    class_id: DeviceClassId,
    class_name: Box<str>,
    operation_contract: Box<str>,
    name: Box<str>,
    route: CallbackRoute,
    invoke_callback: u64,
    quiesce_callback: u64,
    drain_callback: u64,
    active: AtomicBool,
    in_flight: AtomicU64,
}

impl ElmDeviceFunction {
    fn suspend_runtime(&self) {
        self.active.store(false, Ordering::Release);
        while self.in_flight.load(Ordering::Acquire) != 0 {
            let _ = sched::operation::sched_yield();
        }
    }

    fn resume_runtime(&self) {
        self.active.store(true, Ordering::Release);
    }

    fn call_control(&self, callback: u64, opcode: u32) -> Result<(), DeviceFunctionInvokeError> {
        if callback == 0 {
            return Ok(());
        }
        let mut frame = KernelDeviceIoFrameV1 {
            struct_size: core::mem::size_of::<KernelDeviceIoFrameV1>() as u32,
            flags: 0,
            function: self.handle,
            opcode,
            input_len: 0,
            output_capacity: 0,
            output_len: 0,
            payload: [0; KERNEL_DEVICE_IO_PAYLOAD_LEN],
            status: KERNEL_DEVICE_STATUS_FAULT,
            reserved0: 0,
        };
        let status = self.route.invoke(
            callback,
            general::elm_guard::ELM_GUARD_PHASE_DEVICE_IO,
            &mut frame,
        );
        let frame_valid = frame.struct_size == core::mem::size_of::<KernelDeviceIoFrameV1>() as u32
            && frame.flags == 0
            && frame.function == self.handle
            && frame.opcode == opcode
            && frame.input_len == 0
            && frame.output_capacity == 0
            && frame.output_len == 0
            && frame.reserved0 == 0;
        if status == KERNEL_DEVICE_STATUS_OK && !frame_valid {
            self.route.report_fault();
        }
        if status == KERNEL_DEVICE_STATUS_OK
            && frame_valid
            && frame.status == KERNEL_DEVICE_STATUS_OK
        {
            Ok(())
        } else {
            Err(DeviceFunctionInvokeError::Fault)
        }
    }
}

impl DeviceFunction for ElmDeviceFunction {
    fn class_id(&self) -> DeviceClassId {
        self.class_id
    }

    fn dev_name(&self) -> &str {
        &self.name
    }

    fn class_name(&self) -> &str {
        &self.class_name
    }

    fn operation_contract(&self) -> Option<&str> {
        Some(&self.operation_contract)
    }

    fn invoke(
        &self,
        opcode: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, DeviceFunctionInvokeError> {
        if !self.active.load(Ordering::Acquire) {
            return Err(DeviceFunctionInvokeError::Gone);
        }
        if input.len() > KERNEL_DEVICE_IO_PAYLOAD_LEN || output.len() > KERNEL_DEVICE_IO_PAYLOAD_LEN
        {
            return Err(DeviceFunctionInvokeError::Invalid);
        }
        if self
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |in_flight| {
                in_flight.checked_add(1)
            })
            .is_err()
        {
            return Err(DeviceFunctionInvokeError::Busy);
        }
        if !self.active.load(Ordering::Acquire) {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(DeviceFunctionInvokeError::Gone);
        }
        let mut frame = KernelDeviceIoFrameV1 {
            struct_size: core::mem::size_of::<KernelDeviceIoFrameV1>() as u32,
            flags: 0,
            function: self.handle,
            opcode,
            input_len: input.len() as u32,
            output_capacity: output.len() as u32,
            output_len: 0,
            payload: [0; KERNEL_DEVICE_IO_PAYLOAD_LEN],
            status: KERNEL_DEVICE_STATUS_FAULT,
            reserved0: 0,
        };
        frame.payload[..input.len()].copy_from_slice(input);
        let call_status = self.route.invoke(
            self.invoke_callback,
            general::elm_guard::ELM_GUARD_PHASE_DEVICE_IO,
            &mut frame,
        );
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
        let frame_valid = frame.struct_size == core::mem::size_of::<KernelDeviceIoFrameV1>() as u32
            && frame.flags == 0
            && frame.function == self.handle
            && frame.opcode == opcode
            && frame.input_len as usize == input.len()
            && frame.output_capacity as usize == output.len()
            && frame.output_len <= frame.output_capacity
            && frame.output_len as usize <= output.len()
            && frame.reserved0 == 0;
        if call_status == KERNEL_DEVICE_STATUS_OK && !frame_valid {
            self.route.report_fault();
        }
        if call_status != KERNEL_DEVICE_STATUS_OK || !frame_valid {
            return Err(DeviceFunctionInvokeError::Fault);
        }
        if frame.status != KERNEL_DEVICE_STATUS_OK {
            return Err(map_function_status(frame.status));
        }
        let output_len = frame.output_len as usize;
        output[..output_len].copy_from_slice(&frame.payload[..output_len]);
        Ok(output_len)
    }

    fn mark_gone(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            let _ = self.call_control(self.quiesce_callback, u32::MAX - 1);
        }
    }

    fn drain_io(&self) {
        while self.in_flight.load(Ordering::Acquire) != 0 {
            let _ = sched::operation::sched_yield();
        }
        let _ = self.call_control(self.drain_callback, u32::MAX);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn map_function_status(status: i32) -> DeviceFunctionInvokeError {
    match status {
        KERNEL_DEVICE_STATUS_INVALID => DeviceFunctionInvokeError::Invalid,
        KERNEL_DEVICE_STATUS_NOT_FOUND => DeviceFunctionInvokeError::Gone,
        KERNEL_DEVICE_STATUS_BUSY => DeviceFunctionInvokeError::Busy,
        KERNEL_DEVICE_STATUS_UNSUPPORTED => DeviceFunctionInvokeError::Unsupported,
        KERNEL_DEVICE_STATUS_NO_MEMORY => DeviceFunctionInvokeError::NoMemory,
        _ => DeviceFunctionInvokeError::Fault,
    }
}

fn function_error_status(error: DeviceFunctionInvokeError) -> i32 {
    match error {
        DeviceFunctionInvokeError::Invalid => KERNEL_DEVICE_STATUS_INVALID,
        DeviceFunctionInvokeError::Gone => KERNEL_DEVICE_STATUS_NOT_FOUND,
        DeviceFunctionInvokeError::Busy => KERNEL_DEVICE_STATUS_BUSY,
        DeviceFunctionInvokeError::Unsupported => KERNEL_DEVICE_STATUS_UNSUPPORTED,
        DeviceFunctionInvokeError::Fault => KERNEL_DEVICE_STATUS_FAULT,
        DeviceFunctionInvokeError::NoMemory => KERNEL_DEVICE_STATUS_NO_MEMORY,
    }
}

struct ElmIrqHandler {
    handle: KernelDeviceIrqHandleV1,
    route: CallbackRoute,
    callback: u64,
    mode: u32,
    active: AtomicBool,
    pending: AtomicU64,
    in_flight: AtomicU64,
    last_line_kind: AtomicU64,
    last_line_domain: AtomicU64,
    last_line_number: AtomicU64,
    irq_stacks: Option<NativeIrqStackSet>,
}

impl ElmIrqHandler {
    fn invoke(&self, line: IrqLine, top_half: bool) -> IrqStatus {
        if !self.active.load(Ordering::Acquire) {
            return IrqStatus::Unhandled;
        }
        if self
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |in_flight| {
                in_flight.checked_add(1)
            })
            .is_err()
        {
            return IrqStatus::Unhandled;
        }
        struct InFlightGuard<'a>(&'a AtomicU64);
        impl Drop for InFlightGuard<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _in_flight = InFlightGuard(&self.in_flight);
        let (kind, domain, number) = encode_irq_line(line);
        let mut frame = KernelDeviceIrqFrameV1 {
            struct_size: core::mem::size_of::<KernelDeviceIrqFrameV1>() as u32,
            flags: 0,
            irq: self.handle,
            line_kind: kind,
            line_domain: domain,
            line_number: number,
            result: KERNEL_DEVICE_IRQ_UNHANDLED,
            reserved0: 0,
        };
        let status = if top_half {
            let Some(stacks) = self.irq_stacks.as_ref() else {
                return IrqStatus::Unhandled;
            };
            let Some((_execution, context)) = super::core::try_reserve_device_callback_execution(
                self.route.owner,
                self.route.generation,
                self.route.context.phase,
            ) else {
                return IrqStatus::Unhandled;
            };
            stacks.invoke(self.callback, self.route.bounds, context, &mut frame)
        } else {
            self.route.invoke(
                self.callback,
                general::elm_guard::ELM_GUARD_PHASE_DEVICE_IRQ,
                &mut frame,
            )
        };
        if top_half && status == KERNEL_DEVICE_STATUS_FAULT {
            self.route.report_fault();
        }
        let frame_valid = frame.struct_size
            == core::mem::size_of::<KernelDeviceIrqFrameV1>() as u32
            && frame.flags == 0
            && frame.irq == self.handle
            && frame.reserved0 == 0
            && matches!(
                frame.result,
                KERNEL_DEVICE_IRQ_UNHANDLED | KERNEL_DEVICE_IRQ_HANDLED
            );
        if status == KERNEL_DEVICE_STATUS_OK && !frame_valid {
            self.route.report_fault();
        }
        if status == KERNEL_DEVICE_STATUS_OK
            && frame_valid
            && frame.result == KERNEL_DEVICE_IRQ_HANDLED
        {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }

    fn stop_and_drain(&self) -> u64 {
        self.active.store(false, Ordering::Release);
        let pending = self.pending.swap(0, Ordering::AcqRel);
        while self.in_flight.load(Ordering::Acquire) != 0 {
            let _ = sched::operation::sched_yield();
        }
        pending
    }

    fn queue_deferred(&self, line: IrqLine) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        let (kind, domain, number) = encode_irq_line(line);
        self.last_line_kind
            .store(u64::from(kind), Ordering::Release);
        self.last_line_domain
            .store(u64::from(domain), Ordering::Release);
        self.last_line_number.store(number, Ordering::Release);
        let _ = self
            .pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                Some(pending.saturating_add(1))
            });
        if !self.active.load(Ordering::Acquire) {
            let _ = self
                .pending
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                    pending.checked_sub(1)
                });
            return false;
        }
        DEVICE_IRQ_WORK_QUEUE.wake_one_default();
        true
    }

    fn run_one_deferred(&self) -> bool {
        if self
            .pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                pending.checked_sub(1)
            })
            .is_err()
        {
            return false;
        }
        let line = decode_irq_line(
            self.last_line_kind.load(Ordering::Acquire) as u32,
            self.last_line_domain.load(Ordering::Acquire) as u32,
            self.last_line_number.load(Ordering::Acquire),
        );
        let _ = self.invoke(line, false);
        true
    }
}

impl IrqHandler for ElmIrqHandler {
    fn handle_irq(&self, line: IrqLine) -> IrqStatus {
        if !self.active.load(Ordering::Acquire) {
            return IrqStatus::Unhandled;
        }
        if self.mode == KERNEL_DEVICE_IRQ_MODE_TOP_HALF {
            self.invoke(line, true)
        } else if self.queue_deferred(line) {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }
}

fn encode_irq_line(line: IrqLine) -> (u32, u32, u64) {
    match line {
        IrqLine::Ipi => (KERNEL_DEVICE_IRQ_LINE_KIND_IPI, 0, 0),
        IrqLine::Hardware(number) => (KERNEL_DEVICE_IRQ_LINE_KIND_HARDWARE, 0, number as u64),
        IrqLine::Controller { controller, hwirq } => (
            KERNEL_DEVICE_IRQ_LINE_KIND_CONTROLLER,
            controller,
            u64::from(hwirq),
        ),
        IrqLine::Other(number) => (KERNEL_DEVICE_IRQ_LINE_KIND_OTHER, 0, number as u64),
    }
}

fn decode_irq_line(kind: u32, domain: u32, number: u64) -> IrqLine {
    match kind {
        KERNEL_DEVICE_IRQ_LINE_KIND_IPI => IrqLine::Ipi,
        KERNEL_DEVICE_IRQ_LINE_KIND_HARDWARE => IrqLine::Hardware(number as usize),
        KERNEL_DEVICE_IRQ_LINE_KIND_CONTROLLER => IrqLine::Controller {
            controller: domain,
            hwirq: number as u32,
        },
        _ => IrqLine::Other(number as usize),
    }
}

static DEVICE_IRQ_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static DEVICE_IRQ_WORK_QUEUE: sched::WaitQueue = sched::WaitQueue::new();

fn start_deferred_irq_worker() {
    if DEVICE_IRQ_WORKER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = sched::kthread_spawn(
        deferred_irq_worker,
        0,
        sched::SchedParams {
            nice: -10,
            slice_ns: 0,
        },
    );
}

unsafe extern "C" fn deferred_irq_worker(_argument: usize) -> ! {
    loop {
        while run_one_deferred_irq() {}
        let current = sched::current_task();
        DEVICE_IRQ_WORK_QUEUE.wait_event(&current, has_deferred_irq);
    }
}

fn has_deferred_irq() -> bool {
    let runtime = DEVICE_RUNTIME.lock();
    runtime
        .owners
        .iter()
        .any(|owner| owner.fault_cleanup_pending && !owner.registering)
        || runtime.irqs.iter().any(|record| {
            record.proxy.active.load(Ordering::Acquire)
                && record.proxy.pending.load(Ordering::Acquire) != 0
        })
}

fn run_one_deferred_irq() -> bool {
    if run_one_device_fault_cleanup() {
        return true;
    }
    let proxy = {
        DEVICE_RUNTIME
            .lock()
            .irqs
            .iter()
            .find(|record| {
                record.proxy.active.load(Ordering::Acquire)
                    && record.proxy.pending.load(Ordering::Acquire) != 0
            })
            .map(|record| Arc::clone(&record.proxy))
    };
    proxy.is_some_and(|proxy| proxy.run_one_deferred())
}

fn run_one_device_fault_cleanup() -> bool {
    let (owner, generation) = {
        let mut runtime = DEVICE_RUNTIME.lock();
        let Some(record) = runtime
            .owners
            .iter_mut()
            .find(|owner| owner.fault_cleanup_pending && !owner.registering)
        else {
            return false;
        };
        record.fault_cleanup_pending = false;
        (record.owner, record.generation)
    };
    let result = device_owner_quiesce(owner, generation, owner.0)
        .and_then(|_| device_owner_cancel(owner, generation, owner.0))
        .and_then(|_| device_owner_drain(owner, generation, owner.0))
        .and_then(|_| device_owner_release(owner, generation, owner.0));
    if let Err(status) = result {
        log::error!(
            "[elm][device] fault cleanup failed cell={} generation={} status={}",
            owner.0,
            generation.0,
            status
        );
    }
    true
}

#[derive(Clone, Copy)]
enum AttachedResourceKind {
    Mmio,
    Irq,
    Msi,
    Dma,
}

struct ElmAttachedPnpResource {
    kind: AttachedResourceKind,
    runtime_handle: u64,
}

impl PnpResource for ElmAttachedPnpResource {
    fn kind(&self) -> PnpResourceKind {
        match self.kind {
            AttachedResourceKind::Mmio => PnpResourceKind::Mmio,
            AttachedResourceKind::Irq => PnpResourceKind::Irq,
            AttachedResourceKind::Msi => PnpResourceKind::Msi,
            AttachedResourceKind::Dma => PnpResourceKind::Dma,
        }
    }

    fn label(&self) -> &'static str {
        match self.kind {
            AttachedResourceKind::Mmio => "elm-mmio",
            AttachedResourceKind::Irq => "elm-irq",
            AttachedResourceKind::Msi => "elm-msi",
            AttachedResourceKind::Dma => "elm-dma",
        }
    }

    fn identity(&self) -> Option<u64> {
        Some(self.runtime_handle)
    }

    fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        release_attached_resource(self.kind, self.runtime_handle);
        Ok(())
    }
}

fn attach_runtime_resource(
    device: &Arc<PnpDevice>,
    kind: AttachedResourceKind,
    runtime_handle: u64,
) -> Result<(), i32> {
    device
        .own_resource(ElmAttachedPnpResource {
            kind,
            runtime_handle,
        })
        .map_err(map_pnp_error)
}

fn release_attached_resource(kind: AttachedResourceKind, runtime_handle: u64) {
    match kind {
        AttachedResourceKind::Mmio => {
            DEVICE_RUNTIME
                .lock()
                .mmio
                .retain(|record| record.handle.id != runtime_handle);
        }
        AttachedResourceKind::Irq => {
            let record = {
                let mut runtime = DEVICE_RUNTIME.lock();
                runtime
                    .irqs
                    .iter()
                    .position(|record| record.handle.id == runtime_handle)
                    .map(|index| runtime.irqs.swap_remove(index))
            };
            if let Some(mut record) = record {
                if let Some(msi) = record.msi_source {
                    release_msi_irq_link(msi, record.handle.id);
                }
                let _ = record.proxy.stop_and_drain();
                if let Some(handle) = record.irq_handle.take() {
                    let _ = unregister_irq_handler(handle);
                }
            }
        }
        AttachedResourceKind::Msi => {
            let irq_runtime_handle = {
                let mut runtime = DEVICE_RUNTIME.lock();
                runtime
                    .msi
                    .iter_mut()
                    .find(|record| record.handle.id == runtime_handle)
                    .map(|record| {
                        record.allocation_releasing = true;
                        record.irq_runtime_handle
                    })
                    .flatten()
            };
            if let Some(irq_runtime_handle) = irq_runtime_handle {
                release_attached_resource(AttachedResourceKind::Irq, irq_runtime_handle);
            }
            let record = {
                let mut runtime = DEVICE_RUNTIME.lock();
                runtime
                    .msi
                    .iter()
                    .position(|record| record.handle.id == runtime_handle)
                    .map(|index| runtime.msi.swap_remove(index))
            };
            if let Some(record) = record {
                release_msi_allocation(record.allocation);
            }
        }
        AttachedResourceKind::Dma => {
            DEVICE_RUNTIME
                .lock()
                .dma
                .retain(|record| record.handle.id != runtime_handle);
        }
    }
}

fn release_device_attached_resource(
    owner: ElmId,
    generation: Generation,
    kind: AttachedResourceKind,
    handle: KernelDeviceHandleV1,
) -> i32 {
    let device = {
        let runtime = DEVICE_RUNTIME.lock();
        match kind {
            AttachedResourceKind::Mmio => runtime
                .mmio
                .iter()
                .find(|record| {
                    record.handle == handle
                        && record.owner == owner
                        && handle.generation == generation.0
                })
                .map(|record| Arc::clone(&record.device)),
            AttachedResourceKind::Irq => runtime
                .irqs
                .iter()
                .find(|record| {
                    record.handle == handle
                        && record.owner == owner
                        && handle.generation == generation.0
                })
                .map(|record| Arc::clone(&record.device)),
            AttachedResourceKind::Msi => runtime
                .msi
                .iter()
                .find(|record| {
                    record.handle == handle
                        && record.owner == owner
                        && handle.generation == generation.0
                })
                .map(|record| Arc::clone(&record.device)),
            AttachedResourceKind::Dma => runtime
                .dma
                .iter()
                .find(|record| {
                    record.handle == handle
                        && record.owner == owner
                        && handle.generation == generation.0
                })
                .map(|record| Arc::clone(&record.device)),
        }
    };
    let Some(device) = device else {
        return KERNEL_DEVICE_STATUS_NOT_FOUND;
    };
    device
        .release_owned_resource(handle.id)
        .map(|_| KERNEL_DEVICE_STATUS_OK)
        .unwrap_or_else(map_pnp_error)
}

fn release_msi_allocation(allocation: ElmMsiAllocation) {
    match allocation {
        ElmMsiAllocation::Generic(handle) => {
            let _ = free_msi(handle);
        }
        ElmMsiAllocation::Pci { device, handle } => device.release_configured_msi(handle),
    }
}

fn release_msi_irq_link(msi_handle: KernelDeviceMsiHandleV1, irq_runtime_handle: u64) {
    let allocation = {
        let mut runtime = DEVICE_RUNTIME.lock();
        let Some(record) = runtime.msi.iter_mut().find(|record| {
            record.handle == msi_handle && record.irq_runtime_handle == Some(irq_runtime_handle)
        }) else {
            return;
        };
        record.irq_runtime_handle = None;
        record.irq_detaching = true;
        record.allocation.clone()
    };
    if let Err(status) = allocation.disable_irq() {
        log::error!(
            "[elm][device] failed to disable MSI before IRQ detach msi={} irq={} status={}",
            msi_handle.id,
            irq_runtime_handle,
            status
        );
    }
    if let Some(record) = DEVICE_RUNTIME
        .lock()
        .msi
        .iter_mut()
        .find(|record| record.handle == msi_handle && record.irq_runtime_handle.is_none())
    {
        record.irq_detaching = false;
    }
}

extern "C" fn device_enumerate_v1(
    token: ApiGrantTokenV1,
    cursor: u64,
    output: *mut KernelDeviceSnapshotV1,
    next_cursor: *mut u64,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_OBSERVE, |context| {
        if !valid_range(output.cast_const(), true) || !valid_range(next_cursor.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let device = PNP_DEVICES
            .try_list()
            .and_then(|devices| {
                devices
                    .into_iter()
                    .filter(|device| device.runtime_id() > cursor)
                    .min_by_key(|device| device.runtime_id())
            })
            .ok_or(KERNEL_DEVICE_STATUS_NOT_FOUND);
        let device = match device {
            Ok(device) => device,
            Err(status) => return status,
        };
        let snapshot = match snapshot_for(context.cell_id, context.generation, &device) {
            Ok(snapshot) => snapshot,
            Err(status) => return status,
        };
        if write_output(output, snapshot).is_err()
            || write_output(next_cursor, device.runtime_id()).is_err()
        {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_query_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceHandleV1,
    output: *mut KernelDeviceSnapshotV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_OBSERVE, |context| {
        let device = match DEVICE_RUNTIME.lock().resolve_device(context, handle) {
            Ok(device) => device,
            Err(status) => return status,
        };
        let snapshot = match snapshot_for(context.cell_id, context.generation, &device) {
            Ok(snapshot) => snapshot,
            Err(status) => return status,
        };
        write_output(output, snapshot)
            .map(|_| KERNEL_DEVICE_STATUS_OK)
            .unwrap_or_else(|status| status)
    })
}

extern "C" fn device_query_resource_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceHandleV1,
    ordinal: u32,
    output: *mut KernelDeviceResourceV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_OBSERVE, |context| {
        let device = match DEVICE_RUNTIME.lock().resolve_device(context, handle) {
            Ok(device) => device,
            Err(status) => return status,
        };
        let resource = match resource_at(&device, ordinal as usize) {
            Ok(resource) => resource,
            Err(status) => return status,
        };
        write_output(output, resource)
            .map(|_| KERNEL_DEVICE_STATUS_OK)
            .unwrap_or_else(|status| status)
    })
}

extern "C" fn device_query_property_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceHandleV1,
    ordinal: u32,
    output: *mut KernelDevicePropertyV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_OBSERVE, |context| {
        let device = match DEVICE_RUNTIME.lock().resolve_device(context, handle) {
            Ok(device) => device,
            Err(status) => return status,
        };
        let property = match property_at(&device, ordinal as usize) {
            Ok(property) => property,
            Err(status) => return status,
        };
        write_output(output, property)
            .map(|_| KERNEL_DEVICE_STATUS_OK)
            .unwrap_or_else(|status| status)
    })
}

extern "C" fn device_enumerate_function_v1(
    token: ApiGrantTokenV1,
    device_handle: KernelDeviceHandleV1,
    cursor: u64,
    output: *mut KernelDeviceFunctionSnapshotV1,
    next_cursor: *mut u64,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_OBSERVE, |context| {
        if !valid_range(output.cast_const(), true) || !valid_range(next_cursor.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let device = match DEVICE_RUNTIME.lock().resolve_device(context, device_handle) {
            Ok(device) => device,
            Err(status) => return status,
        };
        let functions = match device.try_functions() {
            Some(functions) => functions,
            None => return KERNEL_DEVICE_STATUS_NO_MEMORY,
        };
        let mut selected: Option<(KernelDeviceFunctionHandleV1, Arc<dyn DeviceFunction>)> = None;
        for function in functions {
            let handle = match DEVICE_RUNTIME.lock().function_view(
                context.cell_id,
                context.generation,
                &device,
                &function,
            ) {
                Ok(handle) => handle,
                Err(status) => return status,
            };
            if handle.id <= cursor
                || selected
                    .as_ref()
                    .is_some_and(|(selected_handle, _)| selected_handle.id <= handle.id)
            {
                continue;
            }
            selected = Some((handle, function));
        }
        let Some((handle, function)) = selected else {
            return KERNEL_DEVICE_STATUS_NOT_FOUND;
        };
        let snapshot =
            match function_snapshot_for(context.cell_id, context.generation, &device, &function) {
                Ok(snapshot) => snapshot,
                Err(status) => return status,
            };
        if write_output(output, snapshot).is_err() || write_output(next_cursor, handle.id).is_err()
        {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_query_function_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceFunctionHandleV1,
    output: *mut KernelDeviceFunctionSnapshotV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_OBSERVE, |context| {
        let view = match DEVICE_RUNTIME.lock().resolve_function(context, handle) {
            Ok(view) => view,
            Err(status) => return status,
        };
        match function_view_is_active(&view) {
            Ok(true) => {}
            Ok(false) => return KERNEL_DEVICE_STATUS_NOT_FOUND,
            Err(status) => return status,
        }
        let snapshot = match function_snapshot_for(
            context.cell_id,
            context.generation,
            &view.device,
            &view.function,
        ) {
            Ok(snapshot) => snapshot,
            Err(status) => return status,
        };
        write_output(output, snapshot)
            .map(|_| KERNEL_DEVICE_STATUS_OK)
            .unwrap_or_else(|status| status)
    })
}

extern "C" fn device_invoke_function_v1(
    token: ApiGrantTokenV1,
    frame: *mut KernelDeviceIoFrameV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_INVOKE, |context| {
        if !valid_range(frame.cast_const(), false) || !valid_range(frame.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let mut frame_value = match read_input(frame.cast_const()) {
            Ok(frame) if frame.is_well_formed_request() => frame,
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        };
        let view = match DEVICE_RUNTIME
            .lock()
            .resolve_function(context, frame_value.function)
        {
            Ok(view) => view,
            Err(status) => return status,
        };
        match function_view_is_active(&view) {
            Ok(true) => {}
            Ok(false) => return KERNEL_DEVICE_STATUS_NOT_FOUND,
            Err(status) => return status,
        }
        if view.function.operation_contract().is_none() {
            frame_value.status = KERNEL_DEVICE_STATUS_UNSUPPORTED;
            return write_output(frame, frame_value)
                .map(|_| KERNEL_DEVICE_STATUS_OK)
                .unwrap_or_else(|status| status);
        }
        let input_len = frame_value.input_len as usize;
        let output_capacity = frame_value.output_capacity as usize;
        let mut output_bytes = [0u8; KERNEL_DEVICE_IO_PAYLOAD_LEN];
        let result = view.function.invoke(
            frame_value.opcode,
            &frame_value.payload[..input_len],
            &mut output_bytes[..output_capacity],
        );
        match result {
            Ok(output_len) if output_len <= output_capacity => {
                frame_value.payload.fill(0);
                frame_value.payload[..output_len].copy_from_slice(&output_bytes[..output_len]);
                frame_value.output_len = output_len as u32;
                frame_value.status = KERNEL_DEVICE_STATUS_OK;
            }
            Ok(_) => {
                frame_value.output_len = 0;
                frame_value.status = KERNEL_DEVICE_STATUS_FAULT;
            }
            Err(error) => {
                frame_value.output_len = 0;
                frame_value.status = function_error_status(error);
            }
        }
        write_output(frame, frame_value)
            .map(|_| KERNEL_DEVICE_STATUS_OK)
            .unwrap_or_else(|status| status)
    })
}

extern "C" fn device_register_bus_v1(
    token: ApiGrantTokenV1,
    request: *const KernelDeviceBusRequestV1,
    output: *mut KernelDeviceBusHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_DISCOVERY, |context| {
        let request = match read_input(request) {
            Ok(request) if request.is_well_formed() => request,
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        };
        if !valid_range(output.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let owner_status = ensure_device_owner(context);
        if owner_status != KERNEL_DEVICE_STATUS_OK {
            return owner_status;
        }
        let identifier = request.identifier.as_str().unwrap();
        let contract = request.device_contract.as_str().unwrap();
        if matches!(identifier, "pci" | "usb" | "platform" | "generic") {
            return KERNEL_DEVICE_STATUS_EXISTS;
        }
        let mut runtime = DEVICE_RUNTIME.lock();
        if runtime
            .buses
            .iter()
            .any(|bus| bus.accepting && bus.identifier.as_ref() == identifier)
        {
            return KERNEL_DEVICE_STATUS_EXISTS;
        }
        if let Err(status) = DeviceRuntime::reserve_slot(&mut runtime.buses) {
            return status;
        }
        let handle = match runtime.alloc_handle(context.generation) {
            Ok(handle) => handle,
            Err(status) => return status,
        };
        let identifier = match copy_boxed(identifier) {
            Ok(identifier) => identifier,
            Err(status) => return status,
        };
        let device_contract = match copy_boxed(contract) {
            Ok(contract) => contract,
            Err(status) => return status,
        };
        runtime.buses.push(BusRecord {
            handle,
            owner: context.cell_id,
            bus_type: BusType::dynamic(handle.id | (1u64 << 63)),
            identifier,
            device_contract,
            accepting: true,
            in_flight: 0,
        });
        drop(runtime);
        if let Err(status) = write_output(output, handle) {
            let _ = release_bus(context.cell_id, context.generation, handle, false);
            return status;
        }
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_unregister_bus_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceBusHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_DISCOVERY, |context| {
        release_bus(context.cell_id, context.generation, handle, true)
    })
}

fn release_bus(
    owner: ElmId,
    generation: Generation,
    handle: KernelDeviceBusHandleV1,
    restore_on_busy: bool,
) -> i32 {
    let published = {
        let mut runtime = DEVICE_RUNTIME.lock();
        let Some(index) = runtime.buses.iter().position(|bus| {
            bus.handle == handle && bus.owner == owner && handle.generation == generation.0
        }) else {
            return KERNEL_DEVICE_STATUS_NOT_FOUND;
        };
        let bus_type = runtime.buses[index].bus_type;
        runtime.buses[index].accepting = false;
        if runtime.buses[index].in_flight != 0
            || runtime
                .drivers
                .iter()
                .any(|driver| driver.bus_type == bus_type)
        {
            if restore_on_busy {
                runtime.buses[index].accepting = true;
            }
            return KERNEL_DEVICE_STATUS_BUSY;
        }
        let count = runtime
            .published_devices
            .iter()
            .filter(|device| device.bus_handle == handle)
            .count();
        let mut published = Vec::new();
        if published.try_reserve_exact(count).is_err() {
            if restore_on_busy {
                runtime.buses[index].accepting = true;
            }
            return KERNEL_DEVICE_STATUS_NO_MEMORY;
        }
        published.extend(
            runtime
                .published_devices
                .iter()
                .filter(|device| device.bus_handle == handle)
                .map(|device| Arc::clone(&device.device)),
        );
        published
    };
    if published
        .iter()
        .any(|device| device.state() != PnpState::Gone)
    {
        if restore_on_busy
            && let Some(bus) = DEVICE_RUNTIME
                .lock()
                .buses
                .iter_mut()
                .find(|bus| bus.handle == handle && bus.owner == owner)
        {
            bus.accepting = true;
        }
        return KERNEL_DEVICE_STATUS_BUSY;
    }
    let mut runtime = DEVICE_RUNTIME.lock();
    let Some(index) = runtime.buses.iter().position(|bus| {
        bus.handle == handle && bus.owner == owner && handle.generation == generation.0
    }) else {
        return KERNEL_DEVICE_STATUS_NOT_FOUND;
    };
    if runtime.buses[index].accepting || runtime.buses[index].in_flight != 0 {
        return KERNEL_DEVICE_STATUS_BUSY;
    }
    let bus_type = runtime.buses[index].bus_type;
    if runtime
        .drivers
        .iter()
        .any(|driver| driver.bus_type == bus_type)
    {
        if restore_on_busy {
            runtime.buses[index].accepting = true;
        }
        return KERNEL_DEVICE_STATUS_BUSY;
    }
    runtime
        .published_devices
        .retain(|device| device.bus_handle != handle);
    runtime.buses.swap_remove(index);
    KERNEL_DEVICE_STATUS_OK
}

extern "C" fn device_register_driver_v1(
    token: ApiGrantTokenV1,
    request: *const KernelDeviceDriverRequestV1,
    output: *mut KernelDeviceDriverHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_DRIVER, |context| {
        let request = match read_input(request) {
            Ok(request) if request.is_well_formed() => request,
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        };
        if !valid_range(output.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let owner_status = ensure_device_owner(context);
        if owner_status != KERNEL_DEVICE_STATUS_OK {
            return owner_status;
        }
        let route = match CallbackRoute::current(&[
            request.match_callback,
            request.probe_callback,
            request.remove_callback,
        ]) {
            Ok(route) => route,
            Err(status) => return status,
        };
        if route.owner != context.cell_id || route.generation != context.generation {
            return KERNEL_DEVICE_STATUS_PERMISSION;
        }
        let name = request.name.as_str().unwrap();
        let bus_identifier = request.bus.as_str().unwrap();
        let (handle, bus_type, _bus_lease) = {
            let mut runtime = DEVICE_RUNTIME.lock();
            if runtime
                .drivers
                .iter()
                .any(|driver| driver.name.as_ref() == name)
            {
                return KERNEL_DEVICE_STATUS_EXISTS;
            }
            let handle = match runtime.alloc_handle(context.generation) {
                Ok(handle) => handle,
                Err(status) => return status,
            };
            let (bus_type, bus_lease) = match runtime.begin_bus_use_by_identifier(bus_identifier) {
                Ok(result) => result,
                Err(status) => return status,
            };
            (handle, bus_type, bus_lease)
        };
        let name = match copy_boxed(name) {
            Ok(name) => name,
            Err(status) => return status,
        };
        let proxy = Arc::new(ElmPnpDriver {
            name: name.clone(),
            bus_type: if request.flags & KERNEL_DEVICE_DRIVER_FLAG_GENERIC != 0 {
                BusType::GENERIC
            } else {
                bus_type
            },
            priority: PnpDriverPriority::new(request.priority),
            route,
            match_callback: request.match_callback,
            probe_callback: request.probe_callback,
            remove_callback: request.remove_callback,
            accepting: AtomicBool::new(true),
            in_flight: AtomicU64::new(0),
        });
        let factory = Arc::new(ElmDriverFactory {
            driver: Arc::clone(&proxy),
        });
        let pnp_handle = match PNP_DRIVERS.register_factory(factory) {
            Ok(handle) => handle,
            Err(error) => return map_pnp_error(error),
        };
        let mut runtime = DEVICE_RUNTIME.lock();
        if let Err(status) = DeviceRuntime::reserve_slot(&mut runtime.drivers) {
            drop(runtime);
            let _ = PNP_DRIVERS.unregister(pnp_handle);
            return status;
        }
        runtime.drivers.push(DriverRecord {
            handle,
            owner: context.cell_id,
            name,
            bus_type,
            pnp_handle: Some(pnp_handle),
            proxy,
        });
        drop(runtime);
        if let Err(status) = write_output(output, handle) {
            let _ = release_driver(context.cell_id, context.generation, handle);
            return status;
        }
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_unregister_driver_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceDriverHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_DRIVER, |context| {
        if reentrant_device_teardown(context) {
            return KERNEL_DEVICE_STATUS_BUSY;
        }
        release_driver(context.cell_id, context.generation, handle)
    })
}

fn release_driver(owner: ElmId, generation: Generation, handle: KernelDeviceDriverHandleV1) -> i32 {
    let (pnp_handle, proxy) = {
        let runtime = DEVICE_RUNTIME.lock();
        let Some(record) = runtime.drivers.iter().find(|driver| {
            driver.handle == handle && driver.owner == owner && handle.generation == generation.0
        }) else {
            return KERNEL_DEVICE_STATUS_NOT_FOUND;
        };
        (record.pnp_handle, Arc::clone(&record.proxy))
    };
    proxy.accepting.store(false, Ordering::Release);
    if let Some(pnp_handle) = pnp_handle
        && let Err(error) = PNP_DRIVERS.unregister(pnp_handle)
    {
        // PnP 注销失败时，注册表会保留并恢复自己的 accepting 状态；ELM 代理也
        // 必须恢复，否则后续重试虽然能找到记录，却会被代理永久拒绝。
        proxy.accepting.store(true, Ordering::Release);
        return map_pnp_error(error);
    }
    proxy.drain_callbacks();
    DEVICE_RUNTIME
        .lock()
        .drivers
        .retain(|driver| driver.handle != handle);
    KERNEL_DEVICE_STATUS_OK
}

extern "C" fn device_publish_v1(
    token: ApiGrantTokenV1,
    request: *const KernelDevicePublishRequestV1,
    output: *mut KernelDeviceHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_DISCOVERY, |context| {
        let request = match read_input(request) {
            Ok(request) if request.is_well_formed() => request,
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        };
        if !valid_range(output.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let owner_status = ensure_device_owner(context);
        if owner_status != KERNEL_DEVICE_STATUS_OK {
            return owner_status;
        }
        let (bus_type, bus_name, bus_contract, parent, _bus_lease) = {
            let mut runtime = DEVICE_RUNTIME.lock();
            let Some(bus_index) = runtime.buses.iter().position(|bus| {
                bus.handle == request.bus
                    && bus.owner == context.cell_id
                    && request.bus.generation == context.generation.0
                    && bus.accepting
            }) else {
                return KERNEL_DEVICE_STATUS_NOT_FOUND;
            };
            if request.identity_contract.as_str()
                != Some(runtime.buses[bus_index].device_contract.as_ref())
            {
                return KERNEL_DEVICE_STATUS_INVALID;
            }
            let parent = if request.parent.id == 0 {
                None
            } else {
                match runtime.resolve_device(context, request.parent) {
                    Ok(parent) => Some(parent),
                    Err(status) => return status,
                }
            };
            let bus = &mut runtime.buses[bus_index];
            let bus_type = bus.bus_type;
            let bus_name = bus.identifier.clone();
            let bus_contract = bus.device_contract.clone();
            let bus_lease = match BusUseLease::acquire(bus) {
                Ok(lease) => lease,
                Err(status) => return status,
            };
            (bus_type, bus_name, bus_contract, parent, bus_lease)
        };
        let identity = &request.identity[..request.identity_len as usize];
        let id = match PnpId::dynamic(bus_type, &bus_contract, identity) {
            Ok(id) => id,
            Err(error) => return map_pnp_error(error),
        };
        let mut resources = Vec::new();
        if resources
            .try_reserve_exact(request.resource_count as usize)
            .is_err()
        {
            return KERNEL_DEVICE_STATUS_NO_MEMORY;
        }
        for resource in &request.resources[..request.resource_count as usize] {
            if !validate_published_resource(resource)
                || request.resources[..request.resource_count as usize]
                    .iter()
                    .filter(|other| other.kind == resource.kind && other.index == resource.index)
                    .count()
                    != 1
            {
                return KERNEL_DEVICE_STATUS_INVALID;
            }
            let mut payload = Vec::new();
            if payload
                .try_reserve_exact(resource.payload_len as usize)
                .is_err()
            {
                return KERNEL_DEVICE_STATUS_NO_MEMORY;
            }
            payload.extend_from_slice(&resource.payload[..resource.payload_len as usize]);
            resources.push(DynamicPnpResource {
                kind: resource.kind,
                index: resource.index,
                start: resource.start,
                length: resource.length,
                flags: resource.flags,
                payload: payload.into_boxed_slice(),
            });
        }
        let mut properties = Vec::new();
        if properties
            .try_reserve_exact(request.property_count as usize)
            .is_err()
        {
            return KERNEL_DEVICE_STATUS_NO_MEMORY;
        }
        for property in &request.properties[..request.property_count as usize] {
            if request.properties[..request.property_count as usize]
                .iter()
                .filter(|other| other.name == property.name)
                .count()
                != 1
            {
                return KERNEL_DEVICE_STATUS_INVALID;
            }
            let name = match property.name.as_str() {
                Some(name) => name,
                None => return KERNEL_DEVICE_STATUS_INVALID,
            };
            let mut value = Vec::new();
            if value
                .try_reserve_exact(property.value_len as usize)
                .is_err()
            {
                return KERNEL_DEVICE_STATUS_NO_MEMORY;
            }
            value.extend_from_slice(&property.value[..property.value_len as usize]);
            let name = match copy_boxed(name) {
                Ok(name) => name,
                Err(status) => return status,
            };
            properties.push(general::dev::pnp::DynamicPnpProperty {
                name,
                value: value.into_boxed_slice(),
            });
        }
        let info =
            match DynamicPnpBusInfo::new(bus_type, &bus_name, &bus_contract, properties, resources)
            {
                Ok(info) => info,
                Err(error) => return map_pnp_error(error),
            };
        let name = match copy_boxed(request.name.as_str().unwrap()) {
            Ok(name) => name,
            Err(status) => return status,
        };
        let new_device = match PnpDevice::new(id, name, Box::new(info)) {
            Ok(device) => device,
            Err(error) => return map_pnp_error(error),
        };
        if let Some(parent) = parent.as_ref()
            && let Err(error) = parent.attach_child(&new_device)
        {
            return map_pnp_error(error);
        }
        let registration = match PNP_DEVICES.get_or_insert(Arc::clone(&new_device)) {
            Ok(registration) => registration,
            Err(error) => {
                if let Some(parent) = parent.as_ref() {
                    parent.detach_child(&new_device);
                }
                return map_pnp_error(error);
            }
        };
        if !registration.inserted {
            if let Some(parent) = parent.as_ref() {
                parent.detach_child(&new_device);
            }
            let mut runtime = DEVICE_RUNTIME.lock();
            let owned = runtime.published_devices.iter().any(|published| {
                published.owner == context.cell_id
                    && published.generation == context.generation
                    && Arc::ptr_eq(&published.device, &registration.device)
            });
            if !owned {
                return KERNEL_DEVICE_STATUS_EXISTS;
            }
            let handle = match runtime.device_view(
                context.cell_id,
                context.generation,
                &registration.device,
            ) {
                Ok(handle) => handle,
                Err(status) => return status,
            };
            drop(runtime);
            return write_output(output, handle)
                .map(|_| KERNEL_DEVICE_STATUS_OK)
                .unwrap_or_else(|status| status);
        }
        let handle = {
            let mut runtime = DEVICE_RUNTIME.lock();
            if let Err(status) = DeviceRuntime::reserve_slot(&mut runtime.published_devices) {
                drop(runtime);
                retire_published_device(&registration.device);
                return status;
            }
            let handle = match runtime.device_view(
                context.cell_id,
                context.generation,
                &registration.device,
            ) {
                Ok(handle) => handle,
                Err(status) => {
                    drop(runtime);
                    retire_published_device(&registration.device);
                    return status;
                }
            };
            runtime.published_devices.push(PublishedDeviceRecord {
                owner: context.cell_id,
                generation: context.generation,
                bus_handle: request.bus,
                device: Arc::clone(&registration.device),
            });
            handle
        };
        match PNP_DRIVERS.probe_device(&registration.device) {
            Ok(()) | Err(PnpError::NoDriver | PnpError::ProbeDeferred) => {}
            Err(error) if error.is_deferred() => {}
            Err(error) => {
                retire_published_device(&registration.device);
                return map_pnp_error(error);
            }
        }
        if let Err(status) = write_output(output, handle) {
            retire_published_device(&registration.device);
            return status;
        }
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_remove_v1(token: ApiGrantTokenV1, handle: KernelDeviceHandleV1) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_DISCOVERY, |context| {
        if reentrant_device_teardown(context) {
            return KERNEL_DEVICE_STATUS_BUSY;
        }
        let device = {
            let mut runtime = DEVICE_RUNTIME.lock();
            let Some(index) = runtime.published_devices.iter().position(|published| {
                published.owner == context.cell_id
                    && published.generation == context.generation
                    && runtime.device_views.iter().any(|view| {
                        view.handle == handle && Arc::ptr_eq(&view.device, &published.device)
                    })
            }) else {
                return KERNEL_DEVICE_STATUS_NOT_FOUND;
            };
            runtime.published_devices.swap_remove(index).device
        };
        retire_published_device(&device);
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_register_function_class_v1(
    token: ApiGrantTokenV1,
    request: *const KernelDeviceFunctionClassRequestV1,
    output: *mut KernelDeviceFunctionClassHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_FUNCTION, |context| {
        let request = match read_input(request) {
            Ok(request) if request.is_well_formed() => request,
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        };
        if !valid_range(output.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let owner_status = ensure_device_owner(context);
        if owner_status != KERNEL_DEVICE_STATUS_OK {
            return owner_status;
        }
        let identifier = request.identifier.as_str().unwrap();
        let registration = match general::dev::function::register_function_class(identifier) {
            Ok(registration) => registration,
            Err(error) => return map_function_registry_error(error),
        };
        let operation_contract = match copy_boxed(request.operation_contract.as_str().unwrap()) {
            Ok(contract) => contract,
            Err(status) => {
                let _ = general::dev::function::unregister_function_class(registration.class_id());
                return status;
            }
        };
        let mut runtime = DEVICE_RUNTIME.lock();
        let handle = match runtime.alloc_handle(context.generation) {
            Ok(handle) => handle,
            Err(status) => {
                let _ = general::dev::function::unregister_function_class(registration.class_id());
                return status;
            }
        };
        if let Err(status) = DeviceRuntime::reserve_slot(&mut runtime.function_classes) {
            drop(runtime);
            let _ = general::dev::function::unregister_function_class(registration.class_id());
            return status;
        }
        runtime.function_classes.push(FunctionClassRecord {
            handle,
            owner: context.cell_id,
            operation_contract,
            registration,
        });
        drop(runtime);
        if let Err(status) = write_output(output, handle) {
            let registration = {
                let mut runtime = DEVICE_RUNTIME.lock();
                runtime
                    .function_classes
                    .iter()
                    .position(|class| class.handle == handle && class.owner == context.cell_id)
                    .map(|index| runtime.function_classes.swap_remove(index).registration)
            };
            if let Some(registration) = registration {
                let _ = general::dev::function::unregister_function_class(registration.class_id());
            }
            return status;
        }
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_unregister_function_class_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceFunctionClassHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_FUNCTION, |context| {
        let mut runtime = DEVICE_RUNTIME.lock();
        let Some(index) = runtime.function_classes.iter().position(|class| {
            class.handle == handle
                && class.owner == context.cell_id
                && handle.generation == context.generation.0
        }) else {
            return KERNEL_DEVICE_STATUS_NOT_FOUND;
        };
        let class_id = runtime.function_classes[index].registration.class_id();
        if runtime
            .functions
            .iter()
            .any(|function| function.proxy.class_id == class_id)
            || general::dev::enumerate::DEVICES
                .functions
                .contains_class(class_id)
        {
            return KERNEL_DEVICE_STATUS_BUSY;
        }
        match general::dev::function::unregister_function_class(class_id) {
            Ok(()) => {
                runtime.function_classes.swap_remove(index);
                KERNEL_DEVICE_STATUS_OK
            }
            Err(error) => map_function_registry_error(error),
        }
    })
}

extern "C" fn device_register_function_v1(
    token: ApiGrantTokenV1,
    request: *const KernelDeviceFunctionRequestV1,
    output: *mut KernelDeviceFunctionHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_FUNCTION, |context| {
        let request = match read_input(request) {
            Ok(request) if request.is_well_formed() => request,
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        };
        if !valid_range(output.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let owner_status = ensure_device_owner(context);
        if owner_status != KERNEL_DEVICE_STATUS_OK {
            return owner_status;
        }
        let route = match CallbackRoute::current(&[
            request.invoke_callback,
            request.quiesce_callback,
            request.drain_callback,
        ]) {
            Ok(route) => route,
            Err(status) => return status,
        };
        let (device, class, handle) = {
            let mut runtime = DEVICE_RUNTIME.lock();
            let device = match runtime.resolve_device(context, request.device) {
                Ok(device) => device,
                Err(status) => return status,
            };
            let Some(class) = runtime
                .function_classes
                .iter()
                .find(|class| {
                    class.handle == request.class
                        && class.owner == context.cell_id
                        && request.class.generation == context.generation.0
                })
                .cloned()
            else {
                return KERNEL_DEVICE_STATUS_NOT_FOUND;
            };
            let handle = match runtime.alloc_handle(context.generation) {
                Ok(handle) => handle,
                Err(status) => return status,
            };
            (device, class, handle)
        };
        let name = match copy_boxed(request.name.as_str().unwrap()) {
            Ok(name) => name,
            Err(status) => return status,
        };
        let proxy = Arc::new(ElmDeviceFunction {
            handle,
            class_id: class.registration.class_id(),
            class_name: match copy_boxed(class.registration.name()) {
                Ok(name) => name,
                Err(status) => return status,
            },
            operation_contract: class.operation_contract,
            name,
            route,
            invoke_callback: request.invoke_callback,
            quiesce_callback: request.quiesce_callback,
            drain_callback: request.drain_callback,
            active: AtomicBool::new(true),
            in_flight: AtomicU64::new(0),
        });
        let function: Arc<dyn DeviceFunction> = proxy.clone();
        if let Err(error) = device.register_function(Arc::clone(&function)) {
            return map_pnp_error(error);
        }
        let function_class_id = proxy.class_id;
        let function_name = proxy.name.clone();
        {
            let mut runtime = DEVICE_RUNTIME.lock();
            if let Err(status) = DeviceRuntime::reserve_slot(&mut runtime.function_views) {
                drop(runtime);
                let _ = device.unregister_function(function_class_id, &function_name);
                return status;
            }
            if let Err(status) = DeviceRuntime::reserve_slot(&mut runtime.functions) {
                drop(runtime);
                let _ = device.unregister_function(function_class_id, &function_name);
                return status;
            }
            runtime.function_views.push(FunctionViewRecord {
                handle,
                owner: context.cell_id,
                device: Arc::clone(&device),
                function,
            });
            runtime.functions.push(FunctionRecord {
                handle,
                owner: context.cell_id,
                device: Arc::clone(&device),
                proxy,
            });
        }
        if let Err(status) = write_output(output, handle) {
            let _ = device.unregister_function(function_class_id, &function_name);
            let mut runtime = DEVICE_RUNTIME.lock();
            runtime
                .functions
                .retain(|function| function.handle != handle);
            runtime.function_views.retain(|view| view.handle != handle);
            return status;
        }
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_unregister_function_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceFunctionHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_FUNCTION, |context| {
        if reentrant_device_teardown(context) {
            return KERNEL_DEVICE_STATUS_BUSY;
        }
        let (device, proxy) = {
            let runtime = DEVICE_RUNTIME.lock();
            let Some(record) = runtime.functions.iter().find(|function| {
                function.handle == handle
                    && function.owner == context.cell_id
                    && handle.generation == context.generation.0
            }) else {
                return KERNEL_DEVICE_STATUS_NOT_FOUND;
            };
            (Arc::clone(&record.device), Arc::clone(&record.proxy))
        };
        if let Err(error) = device.unregister_function(proxy.class_id, &proxy.name) {
            return map_pnp_error(error);
        }
        let proxy_function: Arc<dyn DeviceFunction> = proxy;
        let mut runtime = DEVICE_RUNTIME.lock();
        runtime
            .functions
            .retain(|function| function.handle != handle);
        runtime
            .function_views
            .retain(|view| !Arc::ptr_eq(&view.function, &proxy_function));
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_map_mmio_v1(
    token: ApiGrantTokenV1,
    device_handle: KernelDeviceHandleV1,
    resource_ordinal: u32,
    output: *mut KernelDeviceMmioMappingV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_MMIO, |context| {
        if !valid_range(output.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let owner_status = ensure_device_owner(context);
        if owner_status != KERNEL_DEVICE_STATUS_OK {
            return owner_status;
        }
        let device = match DEVICE_RUNTIME.lock().resolve_device(context, device_handle) {
            Ok(device) => device,
            Err(status) => return status,
        };
        let resource = match resource_at(&device, resource_ordinal as usize) {
            Ok(resource) if resource.kind == KERNEL_DEVICE_RESOURCE_MMIO => resource,
            Ok(_) => return KERNEL_DEVICE_STATUS_UNSUPPORTED,
            Err(status) => return status,
        };
        let physical_address = match usize::try_from(resource.start) {
            Ok(address) => address,
            Err(_) => return KERNEL_DEVICE_STATUS_INVALID,
        };
        let length = match usize::try_from(resource.length) {
            Ok(length) if length != 0 => length,
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        };
        if physical_address.checked_add(length).is_none() {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let virtual_address = match general::dev::pnp::device_mmio_to_virt(physical_address) {
            Ok(address) if address != 0 => address,
            _ => return KERNEL_DEVICE_STATUS_UNSUPPORTED,
        };
        if virtual_address.checked_add(length).is_none() {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let handle = {
            let mut runtime = DEVICE_RUNTIME.lock();
            let handle = match runtime.alloc_handle(context.generation) {
                Ok(handle) => handle,
                Err(status) => return status,
            };
            if let Err(status) = DeviceRuntime::reserve_slot(&mut runtime.mmio) {
                return status;
            }
            runtime.mmio.push(MmioRecord {
                handle,
                owner: context.cell_id,
                device: Arc::clone(&device),
                virtual_address,
                length,
            });
            handle
        };
        if let Err(status) = attach_runtime_resource(&device, AttachedResourceKind::Mmio, handle.id)
        {
            release_attached_resource(AttachedResourceKind::Mmio, handle.id);
            return status;
        }
        let mapping = KernelDeviceMmioMappingV1 {
            struct_size: core::mem::size_of::<KernelDeviceMmioMappingV1>() as u32,
            flags: 0,
            handle,
            physical_address: physical_address as u64,
            virtual_address: virtual_address as u64,
            length: length as u64,
        };
        if let Err(status) = write_output(output, mapping) {
            let _ = device.release_owned_resource(handle.id);
            return status;
        }
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_unmap_mmio_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceMmioHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_MMIO, |context| {
        let exists = DEVICE_RUNTIME.lock().mmio.iter().any(|record| {
            record.handle == handle
                && record.owner == context.cell_id
                && handle.generation == context.generation.0
        });
        if !exists {
            return KERNEL_DEVICE_STATUS_NOT_FOUND;
        }
        release_device_attached_resource(
            context.cell_id,
            context.generation,
            AttachedResourceKind::Mmio,
            handle,
        )
    })
}

extern "C" fn device_mmio_read_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceMmioHandleV1,
    offset: u64,
    width: u32,
    output: *mut u64,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_MMIO, |context| {
        if !valid_range(output.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let record = {
            let runtime = DEVICE_RUNTIME.lock();
            runtime
                .mmio
                .iter()
                .find(|record| {
                    record.handle == handle
                        && record.owner == context.cell_id
                        && handle.generation == context.generation.0
                })
                .cloned()
        };
        let Some(record) = record else {
            return KERNEL_DEVICE_STATUS_NOT_FOUND;
        };
        let address = match checked_mmio_address(&record, offset, width) {
            Ok(address) => address,
            Err(status) => return status,
        };
        // Safety: 映射记录来自设备已声明 MMIO 窗口，地址、宽度、对齐和范围均已校验。
        let value = unsafe {
            match width {
                1 => core::ptr::read_volatile(address as *const u8) as u64,
                2 => core::ptr::read_volatile(address as *const u16) as u64,
                4 => core::ptr::read_volatile(address as *const u32) as u64,
                8 => core::ptr::read_volatile(address as *const u64),
                _ => return KERNEL_DEVICE_STATUS_INVALID,
            }
        };
        write_output(output, value)
            .map(|_| KERNEL_DEVICE_STATUS_OK)
            .unwrap_or_else(|status| status)
    })
}

extern "C" fn device_mmio_write_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceMmioHandleV1,
    offset: u64,
    width: u32,
    value: u64,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_MMIO, |context| {
        let record = {
            let runtime = DEVICE_RUNTIME.lock();
            runtime
                .mmio
                .iter()
                .find(|record| {
                    record.handle == handle
                        && record.owner == context.cell_id
                        && handle.generation == context.generation.0
                })
                .cloned()
        };
        let Some(record) = record else {
            return KERNEL_DEVICE_STATUS_NOT_FOUND;
        };
        let address = match checked_mmio_address(&record, offset, width) {
            Ok(address) => address,
            Err(status) => return status,
        };
        // Safety: 映射记录来自设备已声明 MMIO 窗口，地址、宽度、对齐和范围均已校验。
        unsafe {
            match width {
                1 => core::ptr::write_volatile(address as *mut u8, value as u8),
                2 => core::ptr::write_volatile(address as *mut u16, value as u16),
                4 => core::ptr::write_volatile(address as *mut u32, value as u32),
                8 => core::ptr::write_volatile(address as *mut u64, value),
                _ => return KERNEL_DEVICE_STATUS_INVALID,
            }
        };
        KERNEL_DEVICE_STATUS_OK
    })
}

fn checked_mmio_address(record: &MmioRecord, offset: u64, width: u32) -> Result<usize, i32> {
    if !matches!(width, 1 | 2 | 4 | 8) {
        return Err(KERNEL_DEVICE_STATUS_INVALID);
    }
    let offset = usize::try_from(offset).map_err(|_| KERNEL_DEVICE_STATUS_INVALID)?;
    let width = width as usize;
    let end = offset
        .checked_add(width)
        .ok_or(KERNEL_DEVICE_STATUS_INVALID)?;
    if end > record.length || offset % width != 0 {
        return Err(KERNEL_DEVICE_STATUS_INVALID);
    }
    record
        .virtual_address
        .checked_add(offset)
        .ok_or(KERNEL_DEVICE_STATUS_INVALID)
}

extern "C" fn device_request_irq_v1(
    token: ApiGrantTokenV1,
    request: *const KernelDeviceIrqRequestV1,
    output: *mut KernelDeviceIrqHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_IRQ, |context| {
        let request = match read_input(request) {
            Ok(request) if request.is_well_formed() => request,
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        };
        if !valid_range(output.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let owner_status = ensure_device_owner(context);
        if owner_status != KERNEL_DEVICE_STATUS_OK {
            return owner_status;
        }
        let route = match CallbackRoute::current(&[request.callback]) {
            Ok(route) => route,
            Err(status) => return status,
        };
        let device = match DEVICE_RUNTIME
            .lock()
            .resolve_device(context, request.device)
        {
            Ok(device) => device,
            Err(status) => return status,
        };
        let handle = {
            let mut runtime = DEVICE_RUNTIME.lock();
            match runtime.alloc_handle(context.generation) {
                Ok(handle) => handle,
                Err(status) => return status,
            }
        };
        let (line, msi_source, msi_allocation) = match request.source_kind {
            KERNEL_DEVICE_IRQ_SOURCE_RESOURCE => {
                let line = match irq_line_for_device(&device, request.resource_index as usize) {
                    Ok(line) => line,
                    Err(status) => return status,
                };
                (line, None, None)
            }
            KERNEL_DEVICE_IRQ_SOURCE_MSI => {
                let mut runtime = DEVICE_RUNTIME.lock();
                let Some(record) = runtime.msi.iter_mut().find(|record| {
                    record.handle == request.msi
                        && record.owner == context.cell_id
                        && request.msi.generation == context.generation.0
                        && Arc::ptr_eq(&record.device, &device)
                }) else {
                    return KERNEL_DEVICE_STATUS_NOT_FOUND;
                };
                if record.irq_runtime_handle.is_some()
                    || record.irq_detaching
                    || record.allocation_releasing
                {
                    return KERNEL_DEVICE_STATUS_BUSY;
                }
                record.irq_runtime_handle = Some(handle.id);
                (
                    record.line,
                    Some(request.msi),
                    Some(record.allocation.clone()),
                )
            }
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        };
        let irq_stacks = if request.mode == KERNEL_DEVICE_IRQ_MODE_TOP_HALF {
            match NativeIrqStackSet::allocate(context.cell_id) {
                Ok(stacks) => Some(stacks),
                Err(status) => {
                    if let Some(msi) = msi_source {
                        release_msi_irq_link(msi, handle.id);
                    }
                    return status;
                }
            }
        } else {
            None
        };
        let proxy = Arc::new(ElmIrqHandler {
            handle,
            route,
            callback: request.callback,
            mode: request.mode,
            active: AtomicBool::new(true),
            pending: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            last_line_kind: AtomicU64::new(0),
            last_line_domain: AtomicU64::new(0),
            last_line_number: AtomicU64::new(0),
            irq_stacks,
        });
        let handler: Arc<dyn IrqHandler> = proxy.clone();
        let irq_request = if request.shared != 0 {
            IrqRequest::shared(line, "elm-device", handler)
        } else {
            IrqRequest::exclusive(line, "elm-device", handler)
        };
        let irq_handle = match register_irq_request(irq_request) {
            Ok(handle) => handle,
            Err(_) => {
                if let Some(msi) = msi_source {
                    release_msi_irq_link(msi, handle.id);
                }
                return KERNEL_DEVICE_STATUS_BUSY;
            }
        };
        if let Some(allocation) = msi_allocation.as_ref()
            && let Err(status) = allocation.enable_irq()
        {
            let _ = proxy.stop_and_drain();
            let _ = unregister_irq_handler(irq_handle);
            if let Some(msi) = msi_source {
                release_msi_irq_link(msi, handle.id);
            }
            return status;
        }
        let mut runtime = DEVICE_RUNTIME.lock();
        if let Err(status) = DeviceRuntime::reserve_slot(&mut runtime.irqs) {
            drop(runtime);
            let _ = proxy.stop_and_drain();
            let _ = unregister_irq_handler(irq_handle);
            if let Some(msi) = msi_source {
                release_msi_irq_link(msi, handle.id);
            }
            return status;
        }
        runtime.irqs.push(IrqRecord {
            handle,
            owner: context.cell_id,
            device: Arc::clone(&device),
            irq_handle: Some(irq_handle),
            line,
            shared: request.shared != 0,
            proxy,
            msi_source,
        });
        drop(runtime);
        if let Err(status) = attach_runtime_resource(&device, AttachedResourceKind::Irq, handle.id)
        {
            release_attached_resource(AttachedResourceKind::Irq, handle.id);
            return status;
        }
        if let Err(status) = write_output(output, handle) {
            let _ = device.release_owned_resource(handle.id);
            return status;
        }
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_release_irq_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceIrqHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_IRQ, |context| {
        if reentrant_device_teardown(context) {
            return KERNEL_DEVICE_STATUS_BUSY;
        }
        let exists = DEVICE_RUNTIME.lock().irqs.iter().any(|record| {
            record.handle == handle
                && record.owner == context.cell_id
                && handle.generation == context.generation.0
        });
        if !exists {
            return KERNEL_DEVICE_STATUS_NOT_FOUND;
        }
        release_device_attached_resource(
            context.cell_id,
            context.generation,
            AttachedResourceKind::Irq,
            handle,
        )
    })
}

fn irq_line_for_device(device: &Arc<PnpDevice>, resource_index: usize) -> Result<IrqLine, i32> {
    match &device.id {
        PnpId::Platform { .. } => device
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .and_then(|info| info.resources.get(resource_index))
            .and_then(DeviceResource::as_irq)
            .and_then(|resource| resource.resolve_line())
            .ok_or(KERNEL_DEVICE_STATUS_DEFERRED),
        PnpId::Dynamic { .. } => {
            let info = device
                .info
                .as_any()
                .downcast_ref::<DynamicPnpBusInfo>()
                .ok_or(KERNEL_DEVICE_STATUS_INVALID)?;
            let resource = info
                .resources()
                .iter()
                .find(|resource| {
                    resource.kind == KERNEL_DEVICE_RESOURCE_IRQ
                        && resource.index as usize == resource_index
                })
                .ok_or(KERNEL_DEVICE_STATUS_NOT_FOUND)?;
            decode_dynamic_irq_resource(resource)
        }
        PnpId::Pci { .. } if resource_index == 0 => PciDevice::from_pnp(device)
            .and_then(|device| device.routed_irq_line())
            .ok_or(KERNEL_DEVICE_STATUS_DEFERRED),
        _ => Err(KERNEL_DEVICE_STATUS_UNSUPPORTED),
    }
}

fn decode_dynamic_irq_resource(resource: &DynamicPnpResource) -> Result<IrqLine, i32> {
    if resource.kind != KERNEL_DEVICE_RESOURCE_IRQ
        || resource.length != 0
        || !resource.payload.is_empty()
        || !valid_dynamic_irq_resource_encoding(resource.start, resource.flags)
    {
        return Err(KERNEL_DEVICE_STATUS_INVALID);
    }
    let kind = (resource.flags & KERNEL_DEVICE_IRQ_RESOURCE_LINE_KIND_MASK) as u32;
    let domain = (resource.flags >> KERNEL_DEVICE_IRQ_RESOURCE_DOMAIN_SHIFT) as u32;
    match kind {
        KERNEL_DEVICE_IRQ_LINE_KIND_IPI => Ok(IrqLine::Ipi),
        KERNEL_DEVICE_IRQ_LINE_KIND_HARDWARE => usize::try_from(resource.start)
            .map(IrqLine::Hardware)
            .map_err(|_| KERNEL_DEVICE_STATUS_INVALID),
        KERNEL_DEVICE_IRQ_LINE_KIND_CONTROLLER => u32::try_from(resource.start)
            .map(|hwirq| IrqLine::Controller {
                controller: domain,
                hwirq,
            })
            .map_err(|_| KERNEL_DEVICE_STATUS_INVALID),
        KERNEL_DEVICE_IRQ_LINE_KIND_OTHER => usize::try_from(resource.start)
            .map(IrqLine::Other)
            .map_err(|_| KERNEL_DEVICE_STATUS_INVALID),
        _ => Err(KERNEL_DEVICE_STATUS_INVALID),
    }
}

extern "C" fn device_allocate_msi_v1(
    token: ApiGrantTokenV1,
    request: *const KernelDeviceMsiRequestV1,
    output: *mut KernelDeviceMsiAllocationV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_MSI, |context| {
        let request = match read_input(request) {
            Ok(request) if request.is_well_formed() => request,
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        };
        if !valid_range(output.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let owner_status = ensure_device_owner(context);
        if owner_status != KERNEL_DEVICE_STATUS_OK {
            return owner_status;
        }
        let device = match DEVICE_RUNTIME
            .lock()
            .resolve_device(context, request.device)
        {
            Ok(device) => device,
            Err(status) => return status,
        };
        let (allocation, message, line) = match &device.id {
            PnpId::Pci { .. } => {
                let Some(pci) = PciDevice::from_pnp(&device) else {
                    return KERNEL_DEVICE_STATUS_INVALID;
                };
                let handle = match pci.try_configure_single_msi() {
                    Ok(handle) => handle,
                    Err(error) => return map_pci_msi_error(error),
                };
                (
                    ElmMsiAllocation::Pci {
                        device: pci,
                        handle,
                    },
                    handle.message(),
                    handle.line(),
                )
            }
            PnpId::Dynamic { .. } => {
                let resource_status =
                    dynamic_msi_resource_status(&device, request.controller, request.requester);
                if resource_status != KERNEL_DEVICE_STATUS_OK {
                    return resource_status;
                }
                let handle = match allocate_msi(request.controller, request.requester) {
                    Ok(handle) => handle,
                    Err(error) => return map_msi_error(error),
                };
                (
                    ElmMsiAllocation::Generic(handle),
                    handle.message(),
                    handle.line(),
                )
            }
            PnpId::Platform { .. } | PnpId::Usb { .. } => {
                return KERNEL_DEVICE_STATUS_UNSUPPORTED;
            }
        };
        let handle = {
            let mut runtime = DEVICE_RUNTIME.lock();
            let handle = match runtime.alloc_handle(context.generation) {
                Ok(handle) => handle,
                Err(status) => {
                    release_msi_allocation(allocation);
                    return status;
                }
            };
            if let Err(status) = DeviceRuntime::reserve_slot(&mut runtime.msi) {
                drop(runtime);
                release_msi_allocation(allocation);
                return status;
            }
            runtime.msi.push(MsiRecord {
                handle,
                owner: context.cell_id,
                device: Arc::clone(&device),
                allocation,
                line,
                irq_runtime_handle: None,
                irq_detaching: false,
                allocation_releasing: false,
            });
            handle
        };
        if let Err(status) = attach_runtime_resource(&device, AttachedResourceKind::Msi, handle.id)
        {
            release_attached_resource(AttachedResourceKind::Msi, handle.id);
            return status;
        }
        let (line_kind, line_domain, line_number) = encode_irq_line(line);
        let response = KernelDeviceMsiAllocationV1 {
            struct_size: core::mem::size_of::<KernelDeviceMsiAllocationV1>() as u32,
            flags: 0,
            handle,
            message_address: message.address,
            message_data: message.data,
            line_kind,
            line_domain,
            reserved0: 0,
            line_number,
        };
        if let Err(status) = write_output(output, response) {
            let _ = device.release_owned_resource(handle.id);
            return status;
        }
        KERNEL_DEVICE_STATUS_OK
    })
}

fn dynamic_msi_resource_status(device: &Arc<PnpDevice>, controller: u32, requester: u32) -> i32 {
    let Some(info) = device.info.as_any().downcast_ref::<DynamicPnpBusInfo>() else {
        return KERNEL_DEVICE_STATUS_INVALID;
    };
    let mut matching = info.resources().iter().filter(|resource| {
        resource.kind == KERNEL_DEVICE_RESOURCE_MSI
            && resource.start == u64::from(controller)
            && resource.length == u64::from(requester)
    });
    let Some(resource) = matching.next() else {
        return KERNEL_DEVICE_STATUS_UNSUPPORTED;
    };
    if matching.next().is_some()
        || resource.flags != 0
        || !resource.payload.is_empty()
        || resource.start > u32::MAX as u64
        || resource.length > u32::MAX as u64
    {
        return KERNEL_DEVICE_STATUS_INVALID;
    }
    KERNEL_DEVICE_STATUS_OK
}

fn map_msi_error(error: MsiError) -> i32 {
    match error {
        MsiError::NotFound => KERNEL_DEVICE_STATUS_DEFERRED,
        MsiError::OutOfMemory | MsiError::AllocationFailed => KERNEL_DEVICE_STATUS_NO_MEMORY,
        MsiError::AlreadyRegistered | MsiError::Busy => KERNEL_DEVICE_STATUS_BUSY,
    }
}

fn map_pci_msi_error(error: PciMsiError) -> i32 {
    match error {
        PciMsiError::NotSupported
        | PciMsiError::AddressUnsupported
        | PciMsiError::DataUnsupported => KERNEL_DEVICE_STATUS_UNSUPPORTED,
        PciMsiError::NoAllocator => KERNEL_DEVICE_STATUS_DEFERRED,
        PciMsiError::AllocationFailed => KERNEL_DEVICE_STATUS_NO_MEMORY,
        PciMsiError::Config(_) => KERNEL_DEVICE_STATUS_FAULT,
    }
}

extern "C" fn device_release_msi_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceMsiHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_MSI, |context| {
        {
            let mut runtime = DEVICE_RUNTIME.lock();
            let Some(record) = runtime.msi.iter_mut().find(|record| {
                record.handle == handle
                    && record.owner == context.cell_id
                    && handle.generation == context.generation.0
            }) else {
                return KERNEL_DEVICE_STATUS_NOT_FOUND;
            };
            if record.irq_runtime_handle.is_some()
                || record.irq_detaching
                || record.allocation_releasing
            {
                return KERNEL_DEVICE_STATUS_BUSY;
            }
            record.allocation_releasing = true;
        }
        let status = release_device_attached_resource(
            context.cell_id,
            context.generation,
            AttachedResourceKind::Msi,
            handle,
        );
        if status != KERNEL_DEVICE_STATUS_OK {
            if let Some(record) = DEVICE_RUNTIME
                .lock()
                .msi
                .iter_mut()
                .find(|record| record.handle == handle)
            {
                record.allocation_releasing = false;
            }
        }
        status
    })
}

extern "C" fn device_allocate_dma_v1(
    token: ApiGrantTokenV1,
    request: *const KernelDeviceDmaRequestV1,
    output: *mut KernelDeviceDmaBufferV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_DMA, |context| {
        let request = match read_input(request) {
            Ok(request) if request.is_well_formed() => request,
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        };
        if !valid_range(output.cast_const(), true) {
            return KERNEL_DEVICE_STATUS_INVALID;
        }
        let owner_status = ensure_device_owner(context);
        if owner_status != KERNEL_DEVICE_STATUS_OK {
            return owner_status;
        }
        let device = match DEVICE_RUNTIME
            .lock()
            .resolve_device(context, request.device)
        {
            Ok(device) => device,
            Err(status) => return status,
        };
        let dma_context = match dma_context_for_device(&device, request.resource_index) {
            Ok(context) => context,
            Err(status) => return status,
        };
        let direction = match request.direction {
            KERNEL_DEVICE_DMA_TO_DEVICE => DmaDirection::ToDevice,
            KERNEL_DEVICE_DMA_FROM_DEVICE => DmaDirection::FromDevice,
            KERNEL_DEVICE_DMA_BIDIRECTIONAL => DmaDirection::Bidirectional,
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        };
        let length = match usize::try_from(request.length) {
            Ok(length) => length,
            Err(_) => return KERNEL_DEVICE_STATUS_INVALID,
        };
        let align = match usize::try_from(request.align) {
            Ok(align) => align,
            Err(_) => return KERNEL_DEVICE_STATUS_INVALID,
        };
        let buffer = match DmaBuffer::new_in(dma_context, length, align, direction) {
            Ok(buffer) => buffer,
            Err("failed to allocate DMA buffer") => return KERNEL_DEVICE_STATUS_NO_MEMORY,
            Err("phys_to_virt hook is not installed") => return KERNEL_DEVICE_STATUS_DEFERRED,
            Err("DMA buffer is outside device DMA constraints") => {
                return KERNEL_DEVICE_STATUS_UNSUPPORTED;
            }
            Err(_) => return KERNEL_DEVICE_STATUS_FAULT,
        };
        let response_fields = (
            buffer.vaddr() as u64,
            buffer.dma_addr() as u64,
            buffer.len() as u64,
        );
        let handle = {
            let mut runtime = DEVICE_RUNTIME.lock();
            let handle = match runtime.alloc_handle(context.generation) {
                Ok(handle) => handle,
                Err(status) => return status,
            };
            if let Err(status) = DeviceRuntime::reserve_slot(&mut runtime.dma) {
                return status;
            }
            runtime.dma.push(DmaRecord {
                handle,
                owner: context.cell_id,
                device: Arc::clone(&device),
                buffer,
            });
            handle
        };
        if let Err(status) = attach_runtime_resource(&device, AttachedResourceKind::Dma, handle.id)
        {
            release_attached_resource(AttachedResourceKind::Dma, handle.id);
            return status;
        }
        let response = KernelDeviceDmaBufferV1 {
            struct_size: core::mem::size_of::<KernelDeviceDmaBufferV1>() as u32,
            flags: 0,
            handle,
            virtual_address: response_fields.0,
            dma_address: response_fields.1,
            length: response_fields.2,
            direction: request.direction,
            reserved0: 0,
        };
        if let Err(status) = write_output(output, response) {
            let _ = device.release_owned_resource(handle.id);
            return status;
        }
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_sync_dma_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceDmaHandleV1,
    operation: u32,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_DMA, |context| {
        let runtime = DEVICE_RUNTIME.lock();
        let Some(record) = runtime.dma.iter().find(|record| {
            record.handle == handle
                && record.owner == context.cell_id
                && handle.generation == context.generation.0
        }) else {
            return KERNEL_DEVICE_STATUS_NOT_FOUND;
        };
        match operation {
            KERNEL_DEVICE_DMA_SYNC_FOR_DEVICE => record.buffer.sync_for_device(),
            KERNEL_DEVICE_DMA_SYNC_FOR_CPU => record.buffer.sync_for_cpu(),
            _ => return KERNEL_DEVICE_STATUS_INVALID,
        }
        KERNEL_DEVICE_STATUS_OK
    })
}

extern "C" fn device_release_dma_v1(
    token: ApiGrantTokenV1,
    handle: KernelDeviceDmaHandleV1,
) -> i32 {
    with_authorized_device_call(token, KERNEL_DEVICE_CAP_DMA, |context| {
        let exists = DEVICE_RUNTIME.lock().dma.iter().any(|record| {
            record.handle == handle
                && record.owner == context.cell_id
                && handle.generation == context.generation.0
        });
        if !exists {
            return KERNEL_DEVICE_STATUS_NOT_FOUND;
        }
        release_device_attached_resource(
            context.cell_id,
            context.generation,
            AttachedResourceKind::Dma,
            handle,
        )
    })
}

fn dma_context_for_device(device: &Arc<PnpDevice>, resource_index: u32) -> Result<DmaContext, i32> {
    if let Some(pci) = PciDevice::from_pnp(device) {
        return if resource_index == 0 {
            Ok(pci.dma_context())
        } else {
            Err(KERNEL_DEVICE_STATUS_NOT_FOUND)
        };
    }
    if let Some(platform) = device.info.as_any().downcast_ref::<PlatformDeviceInfo>() {
        return if resource_index == 0 {
            Ok(platform.dma_context())
        } else {
            Err(KERNEL_DEVICE_STATUS_NOT_FOUND)
        };
    }
    if let Some(dynamic) = device.info.as_any().downcast_ref::<DynamicPnpBusInfo>() {
        let mut matching = dynamic.resources().iter().filter(|resource| {
            resource.kind == KERNEL_DEVICE_RESOURCE_DMA && resource.index == resource_index
        });
        let resource = matching.next().ok_or(KERNEL_DEVICE_STATUS_NOT_FOUND)?;
        if matching.next().is_some() || !resource.payload.is_empty() {
            return Err(KERNEL_DEVICE_STATUS_INVALID);
        }
        let allowed_flags = KERNEL_DEVICE_DMA_RESOURCE_COHERENT
            | KERNEL_DEVICE_DMA_RESOURCE_SCATTER_GATHER
            | KERNEL_DEVICE_DMA_RESOURCE_ALLOW_BOUNCE
            | KERNEL_DEVICE_DMA_RESOURCE_MAX_SEGMENTS_MASK;
        let max_segments = (resource.flags & KERNEL_DEVICE_DMA_RESOURCE_MAX_SEGMENTS_MASK)
            >> KERNEL_DEVICE_DMA_RESOURCE_MAX_SEGMENTS_SHIFT;
        if resource.flags & !allowed_flags != 0 || max_segments == 0 || resource.length == 0 {
            return Err(KERNEL_DEVICE_STATUS_INVALID);
        }
        let address_mask =
            usize::try_from(resource.start).map_err(|_| KERNEL_DEVICE_STATUS_INVALID)?;
        let max_segment_size =
            usize::try_from(resource.length).map_err(|_| KERNEL_DEVICE_STATUS_INVALID)?;
        let max_segments =
            usize::try_from(max_segments).map_err(|_| KERNEL_DEVICE_STATUS_INVALID)?;
        let coherent = resource.flags & KERNEL_DEVICE_DMA_RESOURCE_COHERENT != 0;
        let supports_scatter_gather =
            resource.flags & KERNEL_DEVICE_DMA_RESOURCE_SCATTER_GATHER != 0;
        let bounce = if resource.flags & KERNEL_DEVICE_DMA_RESOURCE_ALLOW_BOUNCE != 0 {
            DmaBouncePolicy::Allowed
        } else {
            DmaBouncePolicy::Disabled
        };
        return Ok(DmaContext::with_constraints(DmaConstraints {
            address_mask,
            max_segment_size,
            max_segments,
            coherent,
            supports_scatter_gather,
            bounce,
        }));
    }
    Err(KERNEL_DEVICE_STATUS_UNSUPPORTED)
}

fn map_pnp_error(error: PnpError) -> i32 {
    match error {
        PnpError::InvalidState | PnpError::InvalidTransition => KERNEL_DEVICE_STATUS_BUSY,
        PnpError::NoDriver => KERNEL_DEVICE_STATUS_NO_DRIVER,
        PnpError::ProbeDeferred | PnpError::DependencyNotReady(_) => KERNEL_DEVICE_STATUS_DEFERRED,
        PnpError::DriverAmbiguous | PnpError::FunctionExists | PnpError::NameConflict => {
            KERNEL_DEVICE_STATUS_EXISTS
        }
        PnpError::OutOfMemory => KERNEL_DEVICE_STATUS_NO_MEMORY,
        PnpError::MissingResource { .. } => KERNEL_DEVICE_STATUS_NOT_FOUND,
        PnpError::Unsupported { .. } => KERNEL_DEVICE_STATUS_UNSUPPORTED,
        PnpError::MalformedResource { .. } => KERNEL_DEVICE_STATUS_INVALID,
        PnpError::ProbeFailed
        | PnpError::RegistrationFailed { .. }
        | PnpError::HardwareFailure { .. } => KERNEL_DEVICE_STATUS_FAULT,
    }
}

fn map_function_registry_error(error: general::dev::function::FunctionRegistryError) -> i32 {
    match error {
        general::dev::function::FunctionRegistryError::NameExists => KERNEL_DEVICE_STATUS_EXISTS,
        general::dev::function::FunctionRegistryError::NotFound => KERNEL_DEVICE_STATUS_NOT_FOUND,
        general::dev::function::FunctionRegistryError::OutOfMemory => {
            KERNEL_DEVICE_STATUS_NO_MEMORY
        }
        general::dev::function::FunctionRegistryError::InvalidName => KERNEL_DEVICE_STATUS_INVALID,
        general::dev::function::FunctionRegistryError::IdExhausted => {
            KERNEL_DEVICE_STATUS_NO_MEMORY
        }
    }
}

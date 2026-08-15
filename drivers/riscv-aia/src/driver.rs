//! RISC-V AIA IMSIC/APLIC 平台 ELM 驱动。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use vfs::sync::Spinlock;

use crate::config::{
    APLIC_CLRIENUM, APLIC_DOMAINCFG, APLIC_DOMAINCFG_DM, APLIC_DOMAINCFG_IE, APLIC_IDC_CLAIMI,
    APLIC_IDC_IDELIVERY, APLIC_IDC_ITHRESHOLD, APLIC_SETIENUM, APLIC_SETIPNUM_LE, AiaConfigError,
    AplicDirectLayout, AplicLayout, AplicSourceMode, ImsicAddressScheme, ImsicInterruptContext,
    ImsicLayout, MmioRange, aplic_service_hart_index, aplic_source_mode, parse_aplic_hart_indexes,
    parse_msi_parent, parse_num_ids, parse_num_sources, parse_optional_u32,
    select_supervisor_contexts,
};
use crate::dev::irq::{self, IrqDomain, IrqHandle, IrqHandler, IrqLine, IrqStatus};
use crate::dev::msi::{self, MsiController, MsiError, MsiHandle, MsiMessage, MsiVector};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDependency, PnpDevice,
    PnpDriver, PnpError, PnpHandleResource, PnpId, PnpProviderResourceScope, PnpResource,
    PnpResourceKind, PnpResourceReleaseError, PnpResourceReleaseOrder, register_driver_factory,
    unregister_driver,
};
use crate::vector::ImsicVectorLayout;

const COMPAT_IMSICS: &str = "riscv,imsics";
const COMPAT_QEMU_IMSICS: &str = "qemu,imsics";
const COMPAT_APLIC: &str = "riscv,aplic";
const COMPAT_QEMU_APLIC: &str = "qemu,aplic";

#[derive(Clone, Copy)]
struct ImsicSchemeEntry {
    controller: u32,
    scheme: ImsicAddressScheme,
}

static IMSIC_SCHEMES: Spinlock<Vec<ImsicSchemeEntry>> = Spinlock::new(Vec::new());

fn publish_imsic_scheme(controller: u32, scheme: ImsicAddressScheme) -> Result<(), PnpError> {
    let mut schemes = IMSIC_SCHEMES.lock();
    if schemes.iter().any(|entry| entry.controller == controller) {
        return Err(PnpError::NameConflict);
    }
    schemes.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
    schemes.push(ImsicSchemeEntry { controller, scheme });
    Ok(())
}

fn remove_imsic_scheme(controller: u32) {
    IMSIC_SCHEMES
        .lock()
        .retain(|entry| entry.controller != controller);
}

fn imsic_scheme(controller: u32) -> Option<ImsicAddressScheme> {
    IMSIC_SCHEMES
        .lock()
        .iter()
        .find(|entry| entry.controller == controller)
        .map(|entry| entry.scheme)
}

#[derive(Clone, Copy)]
struct ImsicCpuFile {
    logical_cpu: usize,
    hart_id: u64,
    msi_address: u64,
}

struct ImsicAllocator {
    allocated: Vec<bool>,
    next_ordinal: usize,
}

struct Imsic {
    controller: u32,
    num_ids: u32,
    arch_handle: hal::interrupt::RiscvImsicHandle,
    files: Vec<ImsicCpuFile>,
    vector_layout: ImsicVectorLayout,
    allocator: Spinlock<ImsicAllocator>,
    quiesced: AtomicBool,
}

impl Imsic {
    fn new(
        controller: u32,
        num_ids: u32,
        arch_handle: hal::interrupt::RiscvImsicHandle,
        files: Vec<ImsicCpuFile>,
    ) -> Result<Self, PnpError> {
        let vector_layout = ImsicVectorLayout::new(files.len(), num_ids).ok_or_else(|| {
            PnpError::malformed(
                PnpResourceKind::Msi,
                "imsic vector namespace exceeds generic hwirq width",
            )
        })?;
        let slots = files.len() * (num_ids as usize + 1);
        let mut allocated = Vec::new();
        allocated
            .try_reserve(slots)
            .map_err(|_| PnpError::OutOfMemory)?;
        allocated.resize(slots, false);
        Ok(Self {
            controller,
            num_ids,
            arch_handle,
            files,
            vector_layout,
            allocator: Spinlock::new(ImsicAllocator {
                allocated,
                next_ordinal: 0,
            }),
            quiesced: AtomicBool::new(false),
        })
    }

    fn slot(&self, file_index: usize, id: u32) -> Option<usize> {
        self.vector_layout.slot(file_index, id)
    }

    fn decode_hwirq(&self, hwirq: u32) -> Option<(usize, u32, usize)> {
        self.vector_layout.decode(hwirq)
    }

    fn claimed_hwirq(&self, logical_cpu: usize, id: u32) -> Option<u32> {
        let file_index = self
            .files
            .iter()
            .position(|file| file.logical_cpu == logical_cpu)?;
        let slot = self.slot(file_index, id)?;
        self.allocator
            .lock()
            .allocated
            .get(slot)
            .copied()
            .unwrap_or(false)
            .then(|| self.vector_layout.hwirq(file_index, id))
            .flatten()
    }

    fn set_identity_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        if enabled && self.quiesced.load(Ordering::Acquire) {
            return false;
        }
        let Some((file_index, id, slot)) = self.decode_hwirq(hwirq) else {
            return false;
        };
        let Some(file) = self.files.get(file_index) else {
            return false;
        };
        let allocator = self.allocator.lock();
        if !allocator.allocated.get(slot).copied().unwrap_or(false) {
            return false;
        }
        hal::interrupt::riscv_imsic_set_identity_enabled(
            self.arch_handle,
            file.logical_cpu,
            id,
            enabled,
        )
    }

    fn quiesce(&self) {
        if self.quiesced.swap(true, Ordering::AcqRel) {
            return;
        }
        let allocator = self.allocator.lock();
        for (file_index, file) in self.files.iter().enumerate() {
            for id in 1..=self.num_ids {
                let Some(slot) = self.slot(file_index, id) else {
                    continue;
                };
                if !allocator.allocated[slot] {
                    continue;
                }
                let _ = hal::interrupt::riscv_imsic_set_identity_enabled(
                    self.arch_handle,
                    file.logical_cpu,
                    id,
                    false,
                );
            }
        }
        drop(allocator);
        hal::interrupt::riscv_imsic_sync_current();
    }

    fn dispatch_pending(&self) -> IrqStatus {
        let mut saw_identity = false;
        let logical_cpu = sched::current_cpu_id();
        for _ in 0..=self.num_ids {
            let Some(id) = hal::interrupt::riscv_imsic_claim() else {
                break;
            };
            saw_identity = true;
            let Some(hwirq) = self.claimed_hwirq(logical_cpu, id) else {
                log::warning!(
                    "[platform-riscv-aia] IMSIC returned unknown identity {} on cpu {} (num-ids={})",
                    id,
                    logical_cpu,
                    self.num_ids
                );
                continue;
            };
            if !self.quiesced.load(Ordering::Acquire) {
                irq::dispatch_irq_line(IrqLine::Controller {
                    controller: self.controller,
                    hwirq,
                });
            }
        }
        if saw_identity {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }
}

impl MsiController for Imsic {
    fn allocate_vector(&self, _requester: u32) -> Option<MsiVector> {
        if self.quiesced.load(Ordering::Acquire) || self.files.is_empty() {
            return None;
        }
        let mut allocator = self.allocator.lock();
        let capacity = self.vector_layout.capacity();
        let mut selected = None;
        for offset in 0..capacity {
            let ordinal = (allocator.next_ordinal + offset) % capacity;
            let (file_index, id, slot, hwirq) = self.vector_layout.ordinal(ordinal)?;
            if !allocator.allocated[slot] {
                selected = Some((ordinal, file_index, id, slot, hwirq));
                break;
            }
        }
        let (ordinal, file_index, id, slot, hwirq) = selected?;
        allocator.next_ordinal = (ordinal + 1) % capacity;
        allocator.allocated[slot] = true;
        let message = MsiMessage {
            address: self.files[file_index].msi_address,
            data: id,
        };
        Some(MsiVector {
            hwirq,
            line: IrqLine::Controller {
                controller: self.controller,
                hwirq,
            },
            message,
        })
    }

    fn free_vector(&self, hwirq: u32) {
        let Some((file_index, id, slot)) = self.decode_hwirq(hwirq) else {
            return;
        };
        let Some(file) = self.files.get(file_index) else {
            return;
        };
        let mut allocator = self.allocator.lock();
        let Some(allocated) = allocator.allocated.get_mut(slot) else {
            return;
        };
        if !*allocated {
            return;
        }
        // slot 在硬件 disable/clear 完成前始终保持 allocated，阻止并发分配者
        // 复用同一 identity 后又被这条旧 free 路径清掉。
        if hal::interrupt::riscv_imsic_set_identity_enabled(
            self.arch_handle,
            file.logical_cpu,
            id,
            false,
        ) && hal::interrupt::riscv_imsic_clear_identity(self.arch_handle, file.logical_cpu, id)
        {
            *allocated = false;
        }
    }
}

struct ImsicDomain {
    imsic: Arc<Imsic>,
}

impl IrqDomain for ImsicDomain {
    fn translate(&self, _cells: &[u32]) -> Option<IrqLine> {
        None
    }

    fn set_line_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        self.imsic.set_identity_enabled(hwirq, enabled)
    }
}

struct ImsicCascadeHandler {
    imsic: Arc<Imsic>,
}

impl IrqHandler for ImsicCascadeHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        self.imsic.dispatch_pending()
    }
}

struct ImsicBinding {
    controller: u32,
    imsic: Arc<Imsic>,
}

fn release_arch_config(handle: hal::interrupt::RiscvImsicHandle) -> bool {
    let released = hal::interrupt::riscv_imsic_uninstall(handle);
    hal::interrupt::riscv_imsic_sync_current();
    released
}

struct ImsicDriver;

impl ImsicDriver {
    const fn new() -> Self {
        Self
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.properties.interrupt_controller
            && info.bool_property("msi-controller")
            && (info.has_id(COMPAT_IMSICS) || info.has_id(COMPAT_QEMU_IMSICS))
            && info.irq_resources().any(|irq| irq.cells() == [9])
    }
}

impl PnpDriver for ImsicDriver {
    fn name(&self) -> &'static str {
        "platform-riscv-imsic"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = platform_info(dev)?;
        let controller = info.properties.fw_phandle.ok_or_else(|| {
            PnpError::missing(PnpResourceKind::FirmwareBus, "imsic phandle missing")
        })?;
        let num_ids =
            parse_num_ids(info.bytes_property("riscv,num-ids")).map_err(map_imsic_config_error)?;
        let contexts = select_supervisor_contexts(
            info.irq_resources().map(|irq| ImsicInterruptContext {
                controller: irq.controller(),
                cells: irq.cells(),
            }),
            crate::dev::cpu::cpu_reg_for_interrupt_controller,
            crate::dev::cpu::cpu_logical_id_for_interrupt_controller,
        )
        .map_err(map_imsic_config_error)?;
        let ranges: Vec<MmioRange> = info
            .mmio_resources()
            .map(|(phys, size)| MmioRange {
                phys: phys as u64,
                size: size as u64,
            })
            .collect();
        let layout = ImsicLayout::new(
            &ranges,
            contexts.len(),
            parse_optional_u32(info.bytes_property("riscv,guest-index-bits"))
                .map_err(map_imsic_config_error)?,
            parse_optional_u32(info.bytes_property("riscv,hart-index-bits"))
                .map_err(map_imsic_config_error)?,
            parse_optional_u32(info.bytes_property("riscv,group-index-bits"))
                .map_err(map_imsic_config_error)?,
            parse_optional_u32(info.bytes_property("riscv,group-index-shift"))
                .map_err(map_imsic_config_error)?,
        )
        .map_err(map_imsic_config_error)?;
        let mut cpu_mask = 0u64;
        let mut files = Vec::new();
        files
            .try_reserve(contexts.len())
            .map_err(|_| PnpError::OutOfMemory)?;
        for context in &contexts {
            if context.logical_cpu >= sched::NR_CPUS || context.logical_cpu >= u64::BITS as usize {
                return Err(PnpError::malformed(
                    PnpResourceKind::Irq,
                    "imsic logical CPU exceeds CSR bridge mask",
                ));
            }
            let msi_address = *layout
                .interrupt_files
                .get(context.file_index)
                .ok_or_else(|| {
                    PnpError::malformed(
                        PnpResourceKind::Mmio,
                        "imsic interrupt file index exceeds MMIO layout",
                    )
                })?;
            cpu_mask |= 1u64 << context.logical_cpu;
            files.push(ImsicCpuFile {
                logical_cpu: context.logical_cpu,
                hart_id: context.hart_id,
                msi_address,
            });
        }
        // domain/controller 注册会同步唤醒 deferred consumer；先预留全部资源槽，
        // 保证 provider 一旦发布就一定能进入同一个 PnP 回滚事务。
        dev.reserve_owned_resources(4)?;
        let arch_handle =
            hal::interrupt::riscv_imsic_install(num_ids, cpu_mask).ok_or_else(|| {
                PnpError::registration_failed(PnpResourceKind::IrqDomain, "imsic CSR bridge busy")
            })?;
        if let Err(error) = dev.own_resource(PnpHandleResource::new(
            PnpResourceKind::IrqDomain,
            "platform-riscv-imsic-arch",
            arch_handle,
            release_arch_config,
        )) {
            let _ = release_arch_config(arch_handle);
            return Err(error);
        }

        let imsic = Arc::new(Imsic::new(controller, num_ids, arch_handle, files)?);
        let cascade: Arc<dyn IrqHandler> = Arc::new(ImsicCascadeHandler {
            imsic: Arc::clone(&imsic),
        });
        let cascade_handle = irq::register_irq_handler(IrqLine::Hardware(0), cascade)
            .map_err(map_irq_registration_error)?;
        if let Err(error) = dev.own_resource(irq::irq_handler_pnp_resource(
            cascade_handle,
            "platform-riscv-imsic-cascade",
        )) {
            let _ = irq::unregister_irq_handler(cascade_handle);
            return Err(error);
        }

        let domain: Arc<dyn IrqDomain> = Arc::new(ImsicDomain {
            imsic: Arc::clone(&imsic),
        });
        let domain_handle =
            irq::register_irq_domain(controller, domain).map_err(map_irq_registration_error)?;
        if let Err(error) = dev.own_resource(irq::irq_domain_pnp_resource(
            domain_handle,
            "platform-riscv-imsic-domain",
        )) {
            let _ = irq::unregister_irq_domain(domain_handle);
            return Err(error);
        }

        publish_imsic_scheme(controller, layout.scheme)?;
        let msi_driver: Arc<dyn MsiController> = imsic.clone();
        let msi_handle = match msi::register_msi_controller(controller, msi_driver) {
            Ok(handle) => handle,
            Err(error) => {
                remove_imsic_scheme(controller);
                return Err(map_msi_registration_error(error, controller));
            }
        };
        if let Err(error) = dev.own_resource(msi::controller_pnp_resource(
            msi_handle,
            "platform-riscv-imsic-msi",
        )) {
            remove_imsic_scheme(controller);
            let _ = msi::unregister_msi_controller(msi_handle);
            return Err(error);
        }
        dev.set_driver_data(Arc::new(ImsicBinding {
            controller,
            imsic: Arc::clone(&imsic),
        }));
        hal::interrupt::riscv_imsic_sync_current();
        log::printk!(
            "[platform-riscv-aia] IMSIC bound {} controller={} num-ids={} contexts={} cpu-mask={:#x}",
            dev.id,
            controller,
            num_ids,
            contexts.len(),
            cpu_mask
        );
        for file in &imsic.files {
            log::debug!(
                "[platform-riscv-aia] IMSIC hart={} logical-cpu={} msi={:#x}",
                file.hart_id,
                file.logical_cpu,
                file.msi_address
            );
        }
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<ImsicBinding>()
        {
            binding.imsic.quiesce();
            remove_imsic_scheme(binding.controller);
        }
        log::printk!("[platform-riscv-aia] IMSIC removed {}", dev.id);
    }
}

struct Aplic {
    controller: u32,
    mmio_base: usize,
    layout: AplicLayout,
    num_sources: u32,
    sources: Spinlock<Vec<AplicSourceState>>,
    delivery: AplicDelivery,
    quiesced: AtomicBool,
}

#[derive(Clone, Copy)]
struct AplicSourceState {
    mode: AplicSourceMode,
    target: u32,
}

struct AplicMsiParent {
    controller: u32,
    scheme: ImsicAddressScheme,
    resource_scope: PnpProviderResourceScope,
}

enum AplicDelivery {
    Msi(AplicMsiParent),
    Direct(AplicDirectLayout),
}

struct AplicMsiRouteResource {
    parent: u32,
    irq: Option<PnpHandleResource<IrqHandle>>,
    vector: Option<PnpHandleResource<MsiHandle>>,
}

impl PnpResource for AplicMsiRouteResource {
    fn kind(&self) -> PnpResourceKind {
        PnpResourceKind::Msi
    }

    fn label(&self) -> &'static str {
        "platform-riscv-aplic-msi-route"
    }

    fn prepare_release(&self) -> Result<(), PnpResourceReleaseError> {
        self.irq
            .as_ref()
            .expect("live APLIC route owns IRQ resource")
            .prepare_release()?;
        if let Err(error) = self
            .vector
            .as_ref()
            .expect("live APLIC route owns MSI resource")
            .prepare_release()
        {
            self.irq
                .as_ref()
                .expect("live APLIC route owns IRQ resource")
                .cancel_release();
            return Err(error);
        }
        Ok(())
    }

    fn cancel_release(&self) {
        self.vector
            .as_ref()
            .expect("live APLIC route owns MSI resource")
            .cancel_release();
        self.irq
            .as_ref()
            .expect("live APLIC route owns IRQ resource")
            .cancel_release();
    }

    fn release_order(&self) -> PnpResourceReleaseOrder {
        PnpResourceReleaseOrder::Consumer
    }

    fn consumes_dependency(&self, dependency: PnpDependency) -> bool {
        dependency == PnpDependency::MsiController(self.parent)
    }

    fn release(mut self: alloc::boxed::Box<Self>) -> Result<(), PnpResourceReleaseError> {
        let irq = self.irq.take().expect("live APLIC route owns IRQ resource");
        alloc::boxed::Box::new(irq).release()?;
        let vector = self
            .vector
            .take()
            .expect("live APLIC route owns MSI resource");
        alloc::boxed::Box::new(vector).release()
    }
}

impl Aplic {
    fn register_address(&self, offset: usize) -> usize {
        self.mmio_base
            .checked_add(offset)
            .expect("validated APLIC offset must remain in MMIO window")
    }

    fn write32(&self, offset: usize, value: u32) {
        let address = self.register_address(offset);
        // Safety: `AplicLayout` 已验证全部固定寄存器和 per-source 数组落在映射窗口
        // 内；调用方只传入对应的 32 位对齐偏移。
        unsafe { core::ptr::write_volatile(address as *mut u32, value) };
    }

    fn read32(&self, offset: usize) -> u32 {
        let address = self.register_address(offset);
        // Safety: 与 `write32` 相同，地址属于已验证且按 32 位对齐的 APLIC 窗口。
        unsafe { core::ptr::read_volatile(address as *const u32) }
    }

    fn initialize(&self) {
        self.write32(APLIC_DOMAINCFG, 0);
        if let AplicDelivery::Direct(layout) = &self.delivery {
            if let Some(offset) = layout.service_idc_offset(APLIC_IDC_IDELIVERY) {
                self.write32(offset, 0);
            }
            if let Some(offset) = layout.service_idc_offset(APLIC_IDC_ITHRESHOLD) {
                self.write32(offset, 0);
            }
        }
        for source in 1..=self.num_sources {
            self.write32(APLIC_CLRIENUM, source);
            self.write32(
                self.layout
                    .sourcecfg_offset(source)
                    .expect("validated APLIC source"),
                0,
            );
            self.write32(
                self.layout
                    .target_offset(source)
                    .expect("validated APLIC source"),
                0,
            );
        }
        hal::memory::device_io_barrier();
    }

    fn activate(&self) -> bool {
        let domaincfg = match &self.delivery {
            AplicDelivery::Msi(_) => APLIC_DOMAINCFG_DM | APLIC_DOMAINCFG_IE,
            AplicDelivery::Direct(layout) => {
                let Some(threshold) = layout.service_idc_offset(APLIC_IDC_ITHRESHOLD) else {
                    return self.activate_unserviceable_direct();
                };
                let Some(delivery) = layout.service_idc_offset(APLIC_IDC_IDELIVERY) else {
                    return self.activate_unserviceable_direct();
                };
                self.write32(threshold, 0);
                self.write32(delivery, 1);
                APLIC_DOMAINCFG_IE
            }
        };
        self.write32(APLIC_DOMAINCFG, domaincfg);
        hal::memory::device_io_barrier();
        let configured = self.read32(APLIC_DOMAINCFG);
        configured & (APLIC_DOMAINCFG_DM | APLIC_DOMAINCFG_IE) == domaincfg
    }

    fn activate_unserviceable_direct(&self) -> bool {
        self.write32(APLIC_DOMAINCFG, 0);
        hal::memory::device_io_barrier();
        self.read32(APLIC_DOMAINCFG) & (APLIC_DOMAINCFG_DM | APLIC_DOMAINCFG_IE) == 0
    }

    fn allocate_msi_target(self: &Arc<Self>, source: u32, parent: &AplicMsiParent) -> Option<u32> {
        // translate 可能由动态 VirtIO consumer 触发。整个 lazy-route 事务必须在
        // APLIC provider 上下文内完成，避免 MSI/IRQ 与分配账户归到 consumer。
        let _provider_context = parent.resource_scope.enter_context().ok()?;
        parent.resource_scope.reserve_owned_resources(1).ok()?;
        let vector = msi::allocate_msi(parent.controller, source).ok()?;
        let message = vector.message();
        let Some(target) = parent
            .scheme
            .encode_aplic_target(message.address, message.data)
        else {
            let _ = msi::free_msi(vector);
            return None;
        };
        let handler: Arc<dyn IrqHandler> = Arc::new(AplicParentHandler {
            aplic: Arc::clone(self),
            source,
        });
        let irq_handle = match irq::register_irq_handler(vector.line(), handler) {
            Ok(handle) => handle,
            Err(_) => {
                let _ = msi::free_msi(vector);
                return None;
            }
        };
        let resource = AplicMsiRouteResource {
            parent: parent.controller,
            irq: Some(irq::irq_handler_pnp_resource(
                irq_handle,
                "platform-riscv-aplic-parent-irq",
            )),
            vector: Some(msi::vector_pnp_resource(
                vector,
                "platform-riscv-aplic-parent-vector",
            )),
        };
        if parent.resource_scope.own_resource(resource).is_err() {
            // handler 一旦发布便可能已经在其它 CPU 执行。只有成功撤销 handler
            // 后才能复用其 vector；否则保留两者并让 ELM owner 进入 fail-closed。
            if irq::unregister_irq_handler(irq_handle).is_ok() {
                let _ = msi::free_msi(vector);
            }
            return None;
        }
        Some(target)
    }

    fn configure_source(self: &Arc<Self>, source: u32, mode: AplicSourceMode) -> bool {
        if self.quiesced.load(Ordering::Acquire)
            || matches!(
                &self.delivery,
                AplicDelivery::Direct(layout) if layout.service_hart_index().is_none()
            )
            || source == 0
            || source > self.num_sources
        {
            return false;
        }
        let mut sources = self.sources.lock();
        let Some(state) = sources.get_mut(source as usize) else {
            return false;
        };
        if state.mode != AplicSourceMode::Inactive {
            return state.mode == mode;
        }
        if state.target == 0 {
            let AplicDelivery::Msi(parent) = &self.delivery else {
                return false;
            };
            let Some(target) = self.allocate_msi_target(source, parent) else {
                return false;
            };
            state.target = target;
        }
        self.write32(
            self.layout
                .sourcecfg_offset(source)
                .expect("validated APLIC source"),
            mode as u32,
        );
        self.write32(
            self.layout
                .target_offset(source)
                .expect("validated APLIC source"),
            state.target,
        );
        hal::memory::device_io_barrier();
        // 只有 sourcecfg/target 已经对设备可见后才发布 mode；并发 enable 因此
        // 不会观察到半配置 source 并提前写 SETIENUM。
        state.mode = mode;
        true
    }

    fn set_source_enabled(&self, source: u32, enabled: bool) -> bool {
        if source == 0 || source > self.num_sources {
            return false;
        }
        let sources = self.sources.lock();
        if enabled
            && (self.quiesced.load(Ordering::Acquire)
                || sources[source as usize].mode == AplicSourceMode::Inactive)
        {
            return false;
        }
        self.write32(
            if enabled {
                APLIC_SETIENUM
            } else {
                APLIC_CLRIENUM
            },
            source,
        );
        hal::memory::device_io_barrier();
        true
    }

    fn retrigger_level(&self, source: u32) {
        let mode = self.sources.lock()[source as usize].mode;
        if mode == AplicSourceMode::LevelHigh || mode == AplicSourceMode::LevelLow {
            self.write32(APLIC_SETIPNUM_LE, source);
            hal::memory::device_io_barrier();
        }
    }

    fn quiesce(&self) {
        if self.quiesced.swap(true, Ordering::AcqRel) {
            return;
        }
        self.write32(APLIC_DOMAINCFG, 0);
        if let AplicDelivery::Direct(layout) = &self.delivery {
            if let Some(offset) = layout.service_idc_offset(APLIC_IDC_IDELIVERY) {
                self.write32(offset, 0);
            }
        }
        for source in 1..=self.num_sources {
            self.write32(APLIC_CLRIENUM, source);
            self.write32(
                self.layout
                    .sourcecfg_offset(source)
                    .expect("validated APLIC source"),
                0,
            );
            self.write32(
                self.layout
                    .target_offset(source)
                    .expect("validated APLIC source"),
                0,
            );
        }
        hal::memory::device_io_barrier();
        let mut sources = self.sources.lock();
        for state in sources.iter_mut().skip(1) {
            state.mode = AplicSourceMode::Inactive;
            state.target = 0;
        }
    }

    fn claim_direct(&self) -> Option<u32> {
        let AplicDelivery::Direct(layout) = &self.delivery else {
            return None;
        };
        let claim = self.read32(layout.service_idc_offset(APLIC_IDC_CLAIMI)?);
        AplicDirectLayout::claimed_source(claim)
    }
}

struct AplicDomain {
    aplic: Arc<Aplic>,
}

impl IrqDomain for AplicDomain {
    fn translate(&self, cells: &[u32]) -> Option<IrqLine> {
        let [source, flags] = cells else {
            return None;
        };
        let mode = aplic_source_mode(*flags).ok()?;
        self.aplic
            .configure_source(*source, mode)
            .then_some(IrqLine::Controller {
                controller: self.aplic.controller,
                hwirq: *source,
            })
    }

    fn set_line_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        self.aplic.set_source_enabled(hwirq, enabled)
    }

    fn configure_line(
        &self,
        hwirq: u32,
        trigger: Option<irq::IrqTrigger>,
        polarity: Option<irq::IrqPolarity>,
    ) -> bool {
        let mode = match (trigger, polarity) {
            (None, None) => return true,
            (Some(irq::IrqTrigger::Edge), Some(irq::IrqPolarity::High)) => {
                AplicSourceMode::EdgeRise
            }
            (Some(irq::IrqTrigger::Edge), Some(irq::IrqPolarity::Low)) => AplicSourceMode::EdgeFall,
            (Some(irq::IrqTrigger::Level), Some(irq::IrqPolarity::High)) => {
                AplicSourceMode::LevelHigh
            }
            (Some(irq::IrqTrigger::Level), Some(irq::IrqPolarity::Low)) => {
                AplicSourceMode::LevelLow
            }
            _ => return false,
        };
        self.aplic.configure_source(hwirq, mode)
    }
}

struct AplicParentHandler {
    aplic: Arc<Aplic>,
    source: u32,
}

impl IrqHandler for AplicParentHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        if self.aplic.quiesced.load(Ordering::Acquire) {
            return IrqStatus::Handled;
        }
        irq::dispatch_irq_line(IrqLine::Controller {
            controller: self.aplic.controller,
            hwirq: self.source,
        });
        self.aplic.retrigger_level(self.source);
        IrqStatus::Handled
    }
}

/// APLIC direct-delivery 共用的 S-mode external interrupt 级联入口。
struct AplicDirectCascadeHandler {
    aplic: Arc<Aplic>,
}

impl IrqHandler for AplicDirectCascadeHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        if self.aplic.quiesced.load(Ordering::Acquire) {
            return IrqStatus::Handled;
        }
        // CLAIMI 每次返回并完成一个最高优先级 source；限制循环次数可在损坏硬件
        // 持续返回同一 identity 时避免把 trap 上下文永久困住。
        for _ in 0..self.aplic.num_sources {
            let Some(source) = self.aplic.claim_direct() else {
                break;
            };
            irq::dispatch_irq_line(IrqLine::Controller {
                controller: self.aplic.controller,
                hwirq: source,
            });
        }
        IrqStatus::Handled
    }
}

struct AplicBinding {
    aplic: Arc<Aplic>,
}

struct AplicDriver {
    device_mmio_to_virt: fn(usize) -> usize,
    boot_hart_id: usize,
}

impl AplicDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize, boot_hart_id: usize) -> Self {
        Self {
            device_mmio_to_virt,
            boot_hart_id,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.properties.interrupt_controller
            && (info.has_id(COMPAT_APLIC) || info.has_id(COMPAT_QEMU_APLIC))
            // 上级 APLIC 通过 delegation/children 把 S-mode sources 委托给子
            // APLIC，归 machine firmware 所有；即使它有 msi-parent 也不能绑定。
            && info.bytes_property("riscv,children").is_none()
            && info.bytes_property("riscv,delegate").is_none()
            && info.bytes_property("riscv,delegation").is_none()
            && (info.bytes_property("msi-parent").is_some()
                || info.irq_resources().any(|irq| irq.cells() == [9]))
    }
}

impl PnpDriver for AplicDriver {
    fn name(&self) -> &'static str {
        "platform-riscv-aplic"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = platform_info(dev)?;
        let controller = info.properties.fw_phandle.ok_or_else(|| {
            PnpError::missing(PnpResourceKind::FirmwareBus, "aplic phandle missing")
        })?;
        let num_sources = parse_num_sources(info.bytes_property("riscv,num-sources"))
            .map_err(map_aplic_config_error)?;
        let Some((phys, size)) = info.first_mmio() else {
            return Err(PnpError::missing(
                PnpResourceKind::Mmio,
                "aplic reg missing",
            ));
        };
        let mmio_base = (self.device_mmio_to_virt)(phys);
        let layout =
            AplicLayout::new(num_sources, phys, size, mmio_base).map_err(map_aplic_config_error)?;
        let (delivery, owned_resources) = if let Some(raw_parent) =
            info.bytes_property("msi-parent")
        {
            let parent = parse_msi_parent(Some(raw_parent)).map_err(map_aplic_config_error)?;
            let scheme = imsic_scheme(parent).ok_or_else(|| {
                PnpError::dependency(crate::dev::pnp::PnpDependency::MsiController(parent))
            })?;
            // domain 本身占一个资源槽；每个真正被固件 consumer 引用的 source
            // 再懒分配一个组合 IRQ+MSI route 资源。预留 Vec 容量不占 IMSIC ID。
            let resources = (num_sources as usize)
                .checked_add(1)
                .ok_or(PnpError::OutOfMemory)?;
            (
                AplicDelivery::Msi(AplicMsiParent {
                    controller: parent,
                    scheme,
                    resource_scope: dev.provider_resource_scope()?,
                }),
                resources,
            )
        } else {
            let contexts = select_supervisor_contexts(
                info.irq_resources().map(|irq| ImsicInterruptContext {
                    controller: irq.controller(),
                    cells: irq.cells(),
                }),
                crate::dev::cpu::cpu_reg_for_interrupt_controller,
                crate::dev::cpu::cpu_logical_id_for_interrupt_controller,
            )
            .map_err(map_aplic_config_error)?;
            let hart_indexes =
                parse_aplic_hart_indexes(info.bytes_property("riscv,hart-indexes"), contexts.len())
                    .map_err(map_aplic_config_error)?;
            let service_hart_index =
                aplic_service_hart_index(&contexts, &hart_indexes, self.boot_hart_id as u64)
                    .map_err(map_aplic_config_error)?;
            let direct = AplicDirectLayout::new(&hart_indexes, service_hart_index, size, mmio_base)
                .map_err(map_aplic_config_error)?;
            let resources = if service_hart_index.is_some() { 2 } else { 1 };
            (AplicDelivery::Direct(direct), resources)
        };
        dev.reserve_owned_resources(owned_resources)?;

        let initial_target = match &delivery {
            AplicDelivery::Msi(_) => 0,
            AplicDelivery::Direct(direct) => direct.target().unwrap_or(0),
        };
        let mut sources = Vec::new();
        sources
            .try_reserve(num_sources as usize + 1)
            .map_err(|_| PnpError::OutOfMemory)?;
        sources.resize(
            num_sources as usize + 1,
            AplicSourceState {
                mode: AplicSourceMode::Inactive,
                target: initial_target,
            },
        );
        sources[0].target = 0;
        let aplic = Arc::new(Aplic {
            controller,
            mmio_base,
            layout,
            num_sources,
            sources: Spinlock::new(sources),
            delivery,
            quiesced: AtomicBool::new(false),
        });
        aplic.initialize();

        match &aplic.delivery {
            AplicDelivery::Msi(_) => {}
            AplicDelivery::Direct(direct) if direct.service_hart_index().is_some() => {
                let handler: Arc<dyn IrqHandler> = Arc::new(AplicDirectCascadeHandler {
                    aplic: Arc::clone(&aplic),
                });
                let irq_handle = irq::register_irq_handler(IrqLine::Hardware(0), handler)
                    .map_err(map_irq_registration_error)?;
                if let Err(error) = dev.own_resource(irq::irq_handler_pnp_resource(
                    irq_handle,
                    "platform-riscv-aplic-direct-cascade",
                )) {
                    let _ = irq::unregister_irq_handler(irq_handle);
                    return Err(error);
                }
            }
            AplicDelivery::Direct(_) => {}
        }

        if !aplic.activate() {
            aplic.quiesce();
            return Err(PnpError::HardwareFailure {
                detail: "APLIC rejected delivery mode",
            });
        }

        let domain: Arc<dyn IrqDomain> = Arc::new(AplicDomain {
            aplic: Arc::clone(&aplic),
        });
        let domain_handle = irq::register_irq_domain(controller, domain).map_err(|error| {
            aplic.quiesce();
            map_irq_registration_error(error)
        })?;
        if let Err(error) = dev.own_resource(irq::irq_domain_pnp_resource(
            domain_handle,
            "platform-riscv-aplic-domain",
        )) {
            let _ = irq::unregister_irq_domain(domain_handle);
            aplic.quiesce();
            return Err(error);
        }
        dev.set_driver_data(Arc::new(AplicBinding {
            aplic: Arc::clone(&aplic),
        }));
        match &aplic.delivery {
            AplicDelivery::Msi(parent) => {
                hal::interrupt::riscv_imsic_sync_current();
                log::printk!(
                    "[platform-riscv-aia] APLIC bound {} controller={} mode=msi msi-parent={} sources={} phys={:#x}",
                    dev.id,
                    controller,
                    parent.controller,
                    num_sources,
                    phys
                );
            }
            AplicDelivery::Direct(direct) => log::printk!(
                "[platform-riscv-aia] APLIC bound {} controller={} mode=direct service-hart-index={:?} sources={} phys={:#x}",
                dev.id,
                controller,
                direct.service_hart_index(),
                num_sources,
                phys
            ),
        }
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<AplicBinding>()
        {
            binding.aplic.quiesce();
        }
        log::printk!("[platform-riscv-aia] APLIC removed {}", dev.id);
    }
}

fn platform_info(dev: &Arc<PnpDevice>) -> Result<&PlatformDeviceInfo, PnpError> {
    dev.info
        .as_any()
        .downcast_ref::<PlatformDeviceInfo>()
        .ok_or(PnpError::InvalidState)
}

fn map_imsic_config_error(error: AiaConfigError) -> PnpError {
    log::warning!("[platform-riscv-aia] rejected IMSIC binding: {:?}", error);
    match error {
        AiaConfigError::MissingNumIds => {
            PnpError::missing(PnpResourceKind::FirmwareBus, "imsic riscv,num-ids missing")
        }
        AiaConfigError::MissingInterruptContexts => {
            PnpError::missing(PnpResourceKind::Irq, "imsic interrupts-extended missing")
        }
        AiaConfigError::MissingMmio => {
            PnpError::missing(PnpResourceKind::Mmio, "imsic reg missing")
        }
        AiaConfigError::OutOfMemory => PnpError::OutOfMemory,
        _ => PnpError::malformed(PnpResourceKind::IrqDomain, "invalid IMSIC DT binding"),
    }
}

fn map_aplic_config_error(error: AiaConfigError) -> PnpError {
    log::warning!("[platform-riscv-aia] rejected APLIC binding: {:?}", error);
    match error {
        AiaConfigError::MissingNumSources => PnpError::missing(
            PnpResourceKind::FirmwareBus,
            "aplic riscv,num-sources missing",
        ),
        AiaConfigError::MissingMsiParent => {
            PnpError::missing(PnpResourceKind::MsiController, "aplic msi-parent missing")
        }
        AiaConfigError::MissingInterruptContexts => {
            PnpError::missing(PnpResourceKind::Irq, "aplic interrupts-extended missing")
        }
        AiaConfigError::OutOfMemory => PnpError::OutOfMemory,
        _ => PnpError::malformed(PnpResourceKind::IrqDomain, "invalid APLIC DT binding"),
    }
}

fn map_irq_registration_error(error: irq::IrqError) -> PnpError {
    match error {
        irq::IrqError::OutOfMemory => PnpError::OutOfMemory,
        irq::IrqError::AlreadyRegistered | irq::IrqError::NotFound => {
            PnpError::registration_failed(PnpResourceKind::Irq, "AIA IRQ registration failed")
        }
    }
}

fn map_msi_registration_error(error: MsiError, controller: u32) -> PnpError {
    match error {
        MsiError::OutOfMemory => PnpError::OutOfMemory,
        MsiError::NotFound => {
            PnpError::dependency(crate::dev::pnp::PnpDependency::MsiController(controller))
        }
        _ => PnpError::registration_failed(
            PnpResourceKind::MsiController,
            "IMSIC MSI controller registration failed",
        ),
    }
}

struct ImsicFactory;

impl DriverFactory for ImsicFactory {
    fn name(&self) -> &'static str {
        "platform-riscv-imsic"
    }

    fn create(&self, _ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(ImsicDriver::new()))
    }
}

struct AplicFactory;

impl DriverFactory for AplicFactory {
    fn name(&self) -> &'static str {
        "platform-riscv-aplic"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(AplicDriver::new(
            ctx.device_mmio_to_virt,
            ctx.boot_cpu_id,
        )))
    }
}

pub(super) fn register_builtin_drivers() -> Result<[DriverHandle; 2], PnpError> {
    let imsic = register_driver_factory(Arc::new(ImsicFactory))?;
    match register_driver_factory(Arc::new(AplicFactory)) {
        Ok(aplic) => Ok([imsic, aplic]),
        Err(error) => {
            let _ = unregister_driver(imsic);
            Err(error)
        }
    }
}

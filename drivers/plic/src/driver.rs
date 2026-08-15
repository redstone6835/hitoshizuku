//! RISC-V PLIC (Platform-Level Interrupt Controller) 平台 ELM 驱动。
//!
//! 本模块匹配 DTB 中的 `sifive,plic-1.0.0` / `riscv,plic0` 中断控制器节点，
//! 将固件 IRQ specifier 翻译为 [`IrqLine::Controller`]，并提供 PLIC 硬件
//! enable/disable 操作。外部中断通过级联 handler 从 CPU `IrqLine::Hardware(0)`
//! 转入，claim → dispatch → complete。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use vfs::sync::Spinlock;

use crate::config::{
    PlicConfigError, PlicInterruptContext, PlicLayout, PlicSupervisorContext, parse_ndev,
    select_supervisor_contexts,
};
use crate::dev::irq::{self, IrqDomain, IrqHandler, IrqLine, IrqStatus};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};

const COMPAT_SIFIVE_PLIC: &str = "sifive,plic-1.0.0";
const COMPAT_RISCV_PLIC0: &str = "riscv,plic0";

const PLIC_DEFAULT_PRIORITY: u32 = 1;

struct PlicInner {
    mmio_base: usize,
    ndev: u32,
    contexts: Vec<PlicCpuLayout>,
}

#[derive(Clone, Copy)]
struct PlicCpuLayout {
    logical_cpu: usize,
    layout: PlicLayout,
}

struct Plic {
    inner: Spinlock<PlicInner>,
    dispatch_lock: Spinlock<()>,
    quiesced: AtomicBool,
}

impl Plic {
    fn register_address(inner: &PlicInner, offset: usize) -> usize {
        inner
            .mmio_base
            .checked_add(offset)
            .expect("validated PLIC MMIO offset must remain in range")
    }

    fn initialize(&self) {
        let inner = self.inner.lock();
        let layout = inner
            .contexts
            .first()
            .expect("validated PLIC must have a boot context")
            .layout;
        for hwirq in 1..=inner.ndev {
            let offset = layout
                .priority_offset(hwirq)
                .expect("validated PLIC source");
            let addr = Self::register_address(&inner, offset);
            // Safety: layout 已校验 priority 数组完整落在 MMIO 窗口内。
            unsafe { core::ptr::write_volatile(addr as *mut u32, 0) };
        }
        for context in &inner.contexts {
            for word in 0..context.layout.source_words() {
                let offset = context
                    .layout
                    .enable_word_offset(word)
                    .expect("validated PLIC enable word");
                let addr = Self::register_address(&inner, offset);
                // Safety: 每个 layout 都已校验对应 context 的 enable 数组。
                unsafe { core::ptr::write_volatile(addr as *mut u32, 0) };
            }
            let threshold = Self::register_address(&inner, context.layout.threshold_offset());
            // Safety: layout 已校验当前 context 的 threshold 寄存器。
            unsafe { core::ptr::write_volatile(threshold as *mut u32, 0) };
        }
        hal::memory::device_io_barrier();
    }

    fn set_priority(&self, hwirq: u32, priority: u32) -> bool {
        if self.quiesced.load(Ordering::Acquire) {
            return false;
        }
        let inner = self.inner.lock();
        let Some(offset) = inner
            .contexts
            .first()
            .and_then(|context| context.layout.priority_offset(hwirq))
        else {
            return false;
        };
        let addr = Self::register_address(&inner, offset);
        // Safety: `hwirq` 已验证处于固件声明的 PLIC 中断源范围，所得优先级寄存器
        // 地址位于已映射窗口内并按 32 位对齐。
        unsafe { core::ptr::write_volatile(addr as *mut u32, priority) };
        true
    }

    fn set_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        if self.quiesced.load(Ordering::Acquire) {
            return !enabled;
        }
        let inner = self.inner.lock();
        if hwirq == 0 || hwirq > inner.ndev {
            return false;
        }
        for context in &inner.contexts {
            let (offset, bit) = context
                .layout
                .enable_offset(hwirq)
                .expect("validated PLIC source must exist in every context");
            let addr = Self::register_address(&inner, offset);
            // Safety: `hwirq` 和 context 已校验，地址指向对应的对齐使能寄存器。
            let mut val = unsafe { core::ptr::read_volatile(addr as *const u32) };
            if enabled {
                val |= 1u32 << bit;
            } else {
                val &= !(1u32 << bit);
            }
            // Safety: 与上面的读取访问同一有效寄存器，并由 `inner` 锁串行化修改。
            unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
        }
        hal::memory::device_io_barrier();
        true
    }

    fn dispatch_one(&self, controller: u32) -> IrqStatus {
        let _dispatch = self.dispatch_lock.lock();
        if self.quiesced.load(Ordering::Acquire) {
            return IrqStatus::Unhandled;
        }
        let logical_cpu = sched::current_cpu_id();
        let inner = self.inner.lock();
        let Some(layout) = inner
            .contexts
            .iter()
            .find(|context| context.logical_cpu == logical_cpu)
            .map(|context| context.layout)
        else {
            log::error!(
                "[platform-riscv-plic] CPU {} has no supervisor context",
                logical_cpu
            );
            return IrqStatus::Unhandled;
        };
        let claim = Self::register_address(&inner, layout.claim_offset());
        let ndev = inner.ndev;
        // Safety: layout 已校验 claim/complete 寄存器完整落在 MMIO 窗口内。
        let hwirq = unsafe { core::ptr::read_volatile(claim as *const u32) };
        drop(inner);
        if hwirq == 0 {
            return IrqStatus::Unhandled;
        }
        let valid = hwirq <= ndev;
        if valid {
            irq::dispatch_irq_line(IrqLine::Controller { controller, hwirq });
        } else {
            log::error!(
                "[platform-riscv-plic] hardware returned out-of-range claim {} (ndev={})",
                hwirq,
                ndev
            );
        }
        // Safety: 与上面的 claim 读取访问同一个已校验寄存器。
        unsafe { core::ptr::write_volatile(claim as *mut u32, hwirq) };
        if valid {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }

    fn quiesce(&self) {
        if self.quiesced.swap(true, Ordering::AcqRel) {
            return;
        }
        // 等待已进入的 cascade dispatch 完成，并阻止新 dispatch 越过屏蔽点。
        let _dispatch = self.dispatch_lock.lock();
        let inner = self.inner.lock();
        for context in &inner.contexts {
            let threshold = Self::register_address(&inner, context.layout.threshold_offset());
            // Safety: layout 已校验 threshold 寄存器。最大阈值先阻断新上报。
            unsafe { core::ptr::write_volatile(threshold as *mut u32, u32::MAX) };
            for word in 0..context.layout.source_words() {
                let offset = context
                    .layout
                    .enable_word_offset(word)
                    .expect("validated PLIC enable word");
                let addr = Self::register_address(&inner, offset);
                // Safety: layout 已校验对应 context 的 enable 数组。
                unsafe { core::ptr::write_volatile(addr as *mut u32, 0) };
            }
        }
        let layout = inner
            .contexts
            .first()
            .expect("validated PLIC must have a boot context")
            .layout;
        for hwirq in 1..=inner.ndev {
            let offset = layout
                .priority_offset(hwirq)
                .expect("validated PLIC source");
            let addr = Self::register_address(&inner, offset);
            // Safety: layout 已校验 priority 数组。
            unsafe { core::ptr::write_volatile(addr as *mut u32, 0) };
        }
        hal::memory::device_io_barrier();
    }
}

struct PlicDomain {
    plic: Arc<Plic>,
    controller: u32,
    ndev: u32,
}

impl IrqDomain for PlicDomain {
    fn translate(&self, cells: &[u32]) -> Option<IrqLine> {
        let [hwirq] = cells else {
            return None;
        };
        let hwirq = *hwirq;
        if hwirq == 0 || hwirq > self.ndev {
            return None;
        }
        Some(IrqLine::Controller {
            controller: self.controller,
            hwirq,
        })
    }

    fn set_line_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        if enabled && !self.plic.set_priority(hwirq, PLIC_DEFAULT_PRIORITY) {
            return false;
        }
        self.plic.set_enabled(hwirq, enabled)
    }

    fn configure_line(
        &self,
        _hwirq: u32,
        _trigger: Option<irq::IrqTrigger>,
        _polarity: Option<irq::IrqPolarity>,
    ) -> bool {
        true // PLIC 支持 level-triggered，固件已描述配置
    }
}

struct PlicCascadeHandler {
    plic: Arc<Plic>,
    controller: u32,
}

impl IrqHandler for PlicCascadeHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        self.plic.dispatch_one(self.controller)
    }
}

struct PlicDriver {
    device_mmio_to_virt: fn(usize) -> usize,
    boot_hart_id: usize,
}

impl PlicDriver {
    fn new(device_mmio_to_virt: fn(usize) -> usize, boot_hart_id: usize) -> Self {
        Self {
            device_mmio_to_virt,
            boot_hart_id,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.properties.interrupt_controller
            && (info.has_id(COMPAT_SIFIVE_PLIC) || info.has_id(COMPAT_RISCV_PLIC0))
    }

    fn supervisor_contexts(
        info: &PlatformDeviceInfo,
        boot_hart_id: usize,
    ) -> Result<Vec<PlicSupervisorContext>, PnpError> {
        let contexts = info.irq_resources().map(|irq| PlicInterruptContext {
            controller: irq.controller(),
            cells: irq.cells(),
        });
        select_supervisor_contexts(
            contexts,
            boot_hart_id as u64,
            crate::dev::cpu::cpu_reg_for_interrupt_controller,
            crate::dev::cpu::cpu_logical_id_for_interrupt_controller,
        )
        .map_err(map_plic_config_error)
    }
}

fn map_plic_config_error(error: PlicConfigError) -> PnpError {
    log::warning!("[platform-riscv-plic] rejected DT binding: {:?}", error);
    match error {
        PlicConfigError::MissingNdev => {
            PnpError::missing(PnpResourceKind::FirmwareBus, "plic riscv,ndev missing")
        }
        PlicConfigError::MissingInterruptContexts => {
            PnpError::missing(PnpResourceKind::Irq, "plic interrupts-extended missing")
        }
        PlicConfigError::MalformedNdev | PlicConfigError::InvalidNdev => {
            PnpError::malformed(PnpResourceKind::FirmwareBus, "invalid plic riscv,ndev")
        }
        PlicConfigError::MalformedInterruptContext
        | PlicConfigError::UnknownSupervisorContextCpu
        | PlicConfigError::MissingBootSupervisorContext
        | PlicConfigError::DuplicateSupervisorContext => PnpError::malformed(
            PnpResourceKind::Irq,
            "invalid plic supervisor interrupt context",
        ),
        PlicConfigError::OutOfMemory => PnpError::OutOfMemory,
        PlicConfigError::UnalignedMmio
        | PlicConfigError::AddressOverflow
        | PlicConfigError::MmioWindowTooSmall => {
            PnpError::malformed(PnpResourceKind::Mmio, "invalid plic MMIO window")
        }
    }
}

struct PlicBinding {
    plic: Arc<Plic>,
}

impl PnpDriver for PlicDriver {
    fn name(&self) -> &'static str {
        "platform-riscv-plic"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        if !matches!(id, PnpId::Platform { .. }) {
            return false;
        }
        info.as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let controller = info
            .properties
            .fw_phandle
            .ok_or_else(|| PnpError::missing(PnpResourceKind::FirmwareBus, "plic phandle"))?;
        let ndev = parse_ndev(info.bytes_property("riscv,ndev")).map_err(map_plic_config_error)?;
        let Some((phys, size)) = info.first_mmio() else {
            return Err(PnpError::missing(PnpResourceKind::Mmio, "plic reg missing"));
        };
        let supervisor_contexts = Self::supervisor_contexts(info, self.boot_hart_id)?;
        let mmio_base = (self.device_mmio_to_virt)(phys);
        let mut contexts = Vec::new();
        contexts
            .try_reserve(supervisor_contexts.len())
            .map_err(|_| PnpError::OutOfMemory)?;
        for context in &supervisor_contexts {
            let layout = PlicLayout::new(ndev, context.context).map_err(map_plic_config_error)?;
            layout
                .validate_window(phys, size, mmio_base)
                .map_err(map_plic_config_error)?;
            contexts.push(PlicCpuLayout {
                logical_cpu: context.logical_cpu,
                layout,
            });
        }
        let boot_context = supervisor_contexts
            .iter()
            .find(|context| context.hart_id == self.boot_hart_id as u64)
            .expect("context selection requires the boot hart")
            .context;

        let plic = Arc::new(Plic {
            inner: Spinlock::new(PlicInner {
                mmio_base,
                ndev,
                contexts,
            }),
            dispatch_lock: Spinlock::new(()),
            quiesced: AtomicBool::new(false),
        });

        // 初始化：清空全部 S-mode context 的 enable，所有源 priority=0、threshold=0。
        plic.initialize();

        // 先在 CPU 外部中断线上安装级联 handler，再发布 IRQ domain。
        // `register_irq_domain()` 会唤醒等待该 provider 的设备；发布时级联路径必须
        // 已经完整可用，不能让子设备在 probe 事务尚未提交时提前 enable source。
        // 两个外部 handle 发布前一次性预留所有权槽位，避免 domain 已唤醒 consumer
        // 后才因 PnP 资源 Vec 扩容失败而出现不可事务回滚的可见窗口。
        dev.reserve_owned_resources(2)?;
        let cascade = Arc::new(PlicCascadeHandler {
            plic: Arc::clone(&plic),
            controller,
        });
        let irq_handle = irq::register_irq_handler(IrqLine::Hardware(0), cascade)
            .map_err(|_| PnpError::InvalidState)?;
        if let Err(err) = dev.own_resource(irq::irq_handler_pnp_resource(
            irq_handle,
            "platform-riscv-plic-cascade",
        )) {
            let _ = irq::unregister_irq_handler(irq_handle);
            return Err(err);
        }

        let domain: Arc<dyn IrqDomain> = Arc::new(PlicDomain {
            plic: Arc::clone(&plic),
            controller,
            ndev,
        });
        let domain_handle =
            irq::register_irq_domain(controller, domain).map_err(|_| PnpError::InvalidState)?;
        if let Err(err) = dev.own_resource(irq::irq_domain_pnp_resource(
            domain_handle,
            "platform-riscv-plic-domain",
        )) {
            let _ = irq::unregister_irq_domain(domain_handle);
            return Err(err);
        }

        dev.set_driver_data(Arc::new(PlicBinding {
            plic: Arc::clone(&plic),
        }));

        log::printk!(
            "[platform-riscv-plic] bound {} phys={:#x} ndev={} contexts={} boot-context={}",
            dev.id,
            phys,
            ndev,
            supervisor_contexts.len(),
            boot_context
        );
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<PlicBinding>()
        {
            binding.plic.quiesce();
        }
        log::printk!("[platform-riscv-plic] removed {}", dev.id);
    }
}

struct PlicFactory;

impl DriverFactory for PlicFactory {
    fn name(&self) -> &'static str {
        "platform-riscv-plic"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(PlicDriver::new(
            ctx.device_mmio_to_virt,
            ctx.boot_cpu_id,
        )))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(PlicFactory))
}

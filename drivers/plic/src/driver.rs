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
use crate::dispatch::{DispatchGate, drain_pending, mark_unhandled_once};

const COMPAT_SIFIVE_PLIC: &str = "sifive,plic-1.0.0";
const COMPAT_RISCV_PLIC0: &str = "riscv,plic0";

const PLIC_DEFAULT_PRIORITY: u32 = 1;
const PLIC_DRAIN_BUDGET: usize = 64;

struct PlicInner {
    mmio_base: usize,
    ndev: u32,
    contexts: Vec<PlicCpuLayout>,
    boot_context_index: usize,
}

#[derive(Clone, Copy)]
struct PlicCpuLayout {
    layout: PlicLayout,
}

struct Plic {
    inner: PlicInner,
    config_lock: Spinlock<()>,
    dispatch_gate: DispatchGate,
    unhandled_reported: Vec<AtomicBool>,
}

impl Plic {
    fn register_address(inner: &PlicInner, offset: usize) -> usize {
        inner
            .mmio_base
            .checked_add(offset)
            .expect("validated PLIC MMIO offset must remain in range")
    }

    fn initialize(&self) {
        let _config = self.config_lock.lock();
        let inner = &self.inner;
        let layout = inner
            .contexts
            .first()
            .expect("validated PLIC must have a boot context")
            .layout;
        for hwirq in 1..=inner.ndev {
            let offset = layout
                .priority_offset(hwirq)
                .expect("validated PLIC source");
            let addr = Self::register_address(inner, offset);
            // Safety: layout 已校验 priority 数组完整落在 MMIO 窗口内。
            unsafe { core::ptr::write_volatile(addr as *mut u32, 0) };
        }
        for context in &inner.contexts {
            for word in 0..context.layout.source_words() {
                let offset = context
                    .layout
                    .enable_word_offset(word)
                    .expect("validated PLIC enable word");
                let addr = Self::register_address(inner, offset);
                // Safety: 每个 layout 都已校验对应 context 的 enable 数组。
                unsafe { core::ptr::write_volatile(addr as *mut u32, 0) };
            }
            let threshold = Self::register_address(inner, context.layout.threshold_offset());
            // Safety: layout 已校验当前 context 的 threshold 寄存器。
            unsafe { core::ptr::write_volatile(threshold as *mut u32, 0) };
        }
        hal::memory::device_io_barrier();
    }

    fn set_priority(&self, hwirq: u32, priority: u32) -> bool {
        if self.dispatch_gate.is_closed() {
            return false;
        }
        let _config = self.config_lock.lock();
        if self.dispatch_gate.is_closed() {
            return false;
        }
        let inner = &self.inner;
        let Some(offset) = inner
            .contexts
            .first()
            .and_then(|context| context.layout.priority_offset(hwirq))
        else {
            return false;
        };
        let addr = Self::register_address(inner, offset);
        // Safety: `hwirq` 已验证处于固件声明的 PLIC 中断源范围，所得优先级寄存器
        // 地址位于已映射窗口内并按 32 位对齐。
        unsafe { core::ptr::write_volatile(addr as *mut u32, priority) };
        true
    }

    fn set_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        if self.dispatch_gate.is_closed() {
            return !enabled;
        }
        let _config = self.config_lock.lock();
        if self.dispatch_gate.is_closed() {
            return !enabled;
        }
        let inner = &self.inner;
        if hwirq == 0 || hwirq > inner.ndev {
            return false;
        }
        // 启动期固件拓扑中的 logical_cpu 会在 SMP 启动时按 boot-first 重排，
        // 因而不能作为长期路由键。固定投递到由 hart ID 识别出的 boot context；
        // boot hart 的运行期逻辑编号稳定为 0。
        let target = enabled.then_some(inner.boot_context_index);
        if enabled {
            self.unhandled_reported[hwirq as usize].store(false, Ordering::Relaxed);
        }
        for (index, context) in inner.contexts.iter().enumerate() {
            let (offset, bit) = context
                .layout
                .enable_offset(hwirq)
                .expect("validated PLIC source must exist in every context");
            let addr = Self::register_address(inner, offset);
            // Safety: `hwirq` 和 context 已校验，地址指向对应的对齐使能寄存器。
            let mut val = unsafe { core::ptr::read_volatile(addr as *const u32) };
            if target == Some(index) {
                val |= 1u32 << bit;
            } else {
                val &= !(1u32 << bit);
            }
            // Safety: 与上面的读取访问同一有效寄存器，并由配置锁串行化修改。
            unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
        }
        hal::memory::device_io_barrier();
        true
    }

    fn dispatch_pending(&self, controller: u32) -> IrqStatus {
        let Some(_active) = self.dispatch_gate.try_enter() else {
            return IrqStatus::Handled;
        };
        let logical_cpu = sched::current_cpu_id();
        if logical_cpu != 0 {
            // CPU 外部中断线允许由 PLIC 与 IMSIC 共享；PLIC source 仅路由到
            // boot context，其他 CPU 上的外部中断应交给另一个级联控制器。
            return IrqStatus::Unhandled;
        }
        let inner = &self.inner;
        let layout = inner.contexts[inner.boot_context_index].layout;
        let claim = Self::register_address(inner, layout.claim_offset());
        let ndev = inner.ndev;

        let result = drain_pending(
            ndev,
            PLIC_DRAIN_BUDGET,
            || {
                // Safety: layout 已校验 claim/complete 寄存器完整落在 MMIO 窗口内。
                let hwirq = unsafe { core::ptr::read_volatile(claim as *const u32) };
                // claim 必须先于设备状态读取生效。
                hal::memory::device_io_barrier();
                hwirq
            },
            |hwirq| {
                let handled = irq::dispatch_irq_line(IrqLine::Controller { controller, hwirq });
                // 设备 ACK 必须在 PLIC complete 前对总线可见，避免 level source
                // 在去断言传播完成前被当前批次立即重新 claim。
                hal::memory::device_io_barrier();
                let reported = &self.unhandled_reported[hwirq as usize];
                if !handled && mark_unhandled_once(reported) {
                    log::error!(
                        "[platform-riscv-plic] unhandled source {} on CPU {}",
                        hwirq,
                        logical_cpu
                    );
                }
                handled
            },
            |hwirq| {
                // Safety: 与 claim 读取访问同一个已校验寄存器。
                unsafe { core::ptr::write_volatile(claim as *mut u32, hwirq) };
                // complete 必须在下一次 claim 前完成。
                hal::memory::device_io_barrier();
            },
        );
        if let Some(hwirq) = result.invalid {
            log::error!(
                "[platform-riscv-plic] hardware returned out-of-range claim {} (ndev={})",
                hwirq,
                ndev
            );
        }
        if result.claimed != 0 {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }

    fn quiesce(&self) {
        if !self.dispatch_gate.close() {
            return;
        }
        // 先阻止新配置和 dispatch，再屏蔽全部 source，最后等待在途处理完成。
        let config = self.config_lock.lock();
        let inner = &self.inner;
        for context in &inner.contexts {
            let threshold = Self::register_address(inner, context.layout.threshold_offset());
            // Safety: layout 已校验 threshold 寄存器。最大阈值先阻断新上报。
            unsafe { core::ptr::write_volatile(threshold as *mut u32, u32::MAX) };
            for word in 0..context.layout.source_words() {
                let offset = context
                    .layout
                    .enable_word_offset(word)
                    .expect("validated PLIC enable word");
                let addr = Self::register_address(inner, offset);
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
            let addr = Self::register_address(inner, offset);
            // Safety: layout 已校验 priority 数组。
            unsafe { core::ptr::write_volatile(addr as *mut u32, 0) };
        }
        hal::memory::device_io_barrier();
        drop(config);
        self.dispatch_gate.wait_for_idle();
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
        self.plic.dispatch_pending(self.controller)
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
            contexts.push(PlicCpuLayout { layout });
        }
        let boot_context = supervisor_contexts
            .iter()
            .find(|context| context.hart_id == self.boot_hart_id as u64)
            .expect("context selection requires the boot hart")
            .context;
        let boot_context_index = supervisor_contexts
            .iter()
            .position(|context| context.hart_id == self.boot_hart_id as u64)
            .expect("context selection requires the boot hart");
        let mut unhandled_reported = Vec::new();
        unhandled_reported
            .try_reserve_exact(ndev as usize + 1)
            .map_err(|_| PnpError::OutOfMemory)?;
        unhandled_reported.resize_with(ndev as usize + 1, || AtomicBool::new(false));

        let plic = Arc::new(Plic {
            inner: PlicInner {
                mmio_base,
                ndev,
                contexts,
                boot_context_index,
            },
            config_lock: Spinlock::new(()),
            dispatch_gate: DispatchGate::new(),
            unhandled_reported,
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

//! RISC-V PLIC (Platform-Level Interrupt Controller) 平台 ELM 驱动。
//!
//! 本模块匹配 DTB 中的 `sifive,plic-1.0.0` / `riscv,plic0` 中断控制器节点，
//! 将固件 IRQ specifier 翻译为 [`IrqLine::Controller`]，并提供 PLIC 硬件
//! enable/disable 操作。外部中断通过级联 handler 从 CPU `IrqLine::Hardware(0)`
//! 转入，claim → dispatch → complete。

use alloc::sync::Arc;

use vfs::sync::Spinlock;

use crate::dev::irq::{self, IrqDomain, IrqHandler, IrqLine, IrqStatus};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};

const COMPAT_SIFIVE_PLIC: &str = "sifive,plic-1.0.0";
const COMPAT_RISCV_PLIC0: &str = "riscv,plic0";

const PLIC_PRIORITY_BASE: usize = 0x000000;
const PLIC_ENABLE_BASE: usize = 0x002000;
const PLIC_ENABLE_STRIDE: usize = 0x80;
const PLIC_THRESHOLD_BASE: usize = 0x200000;
const PLIC_CLAIM_BASE: usize = 0x200004;
const PLIC_CONTEXT_STRIDE: usize = 0x1000;

const RISCV_SUPERVISOR_EXTERNAL_IRQ: u32 = 9;
const PLIC_DEFAULT_PRIORITY: u32 = 1;

struct PlicInner {
    mmio_base: usize,
    ndev: u32,
}

struct Plic {
    inner: Spinlock<PlicInner>,
    context: usize,
}

impl Plic {
    fn claim(&self) -> u32 {
        let inner = self.inner.lock();
        let addr = inner.mmio_base + PLIC_CLAIM_BASE + PLIC_CONTEXT_STRIDE * self.context;
        // Safety: PLIC 实例由 platform probe 在固件声明的 MMIO 窗口上映射创建，
        // `context` 来自已校验的 supervisor 中断上下文。
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }

    fn complete(&self, hwirq: u32) {
        let inner = self.inner.lock();
        let addr = inner.mmio_base + PLIC_CLAIM_BASE + PLIC_CONTEXT_STRIDE * self.context;
        // Safety: 安全条件与 `claim` 相同，claim/complete 寄存器允许 32 位易失写入。
        unsafe { core::ptr::write_volatile(addr as *mut u32, hwirq) };
    }

    fn set_priority(&self, hwirq: u32, priority: u32) -> bool {
        let inner = self.inner.lock();
        if hwirq == 0 || hwirq > inner.ndev {
            return false;
        }
        let addr = inner.mmio_base + PLIC_PRIORITY_BASE + 4 * hwirq as usize;
        // Safety: `hwirq` 已验证处于固件声明的 PLIC 中断源范围，所得优先级寄存器
        // 地址位于已映射窗口内并按 32 位对齐。
        unsafe { core::ptr::write_volatile(addr as *mut u32, priority) };
        true
    }

    fn set_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        let inner = self.inner.lock();
        if hwirq == 0 || hwirq > inner.ndev {
            return false;
        }
        let reg_idx = hwirq as usize / 32;
        let bit = hwirq % 32;
        let addr =
            inner.mmio_base + PLIC_ENABLE_BASE + PLIC_ENABLE_STRIDE * self.context + 4 * reg_idx;
        // Safety: `hwirq` 和 `context` 已校验，地址指向当前上下文的对齐使能寄存器。
        let mut val = unsafe { core::ptr::read_volatile(addr as *const u32) };
        if enabled {
            val |= 1u32 << bit;
        } else {
            val &= !(1u32 << bit);
        }
        // Safety: 与上面的读取访问同一有效使能寄存器，并由 `inner` 锁串行化修改。
        unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
        true
    }

    fn set_threshold(&self, threshold: u32) {
        let inner = self.inner.lock();
        let addr = inner.mmio_base + PLIC_THRESHOLD_BASE + PLIC_CONTEXT_STRIDE * self.context;
        // Safety: `context` 已校验，地址指向已映射窗口内的对齐阈值寄存器。
        unsafe { core::ptr::write_volatile(addr as *mut u32, threshold) };
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
        let hwirq = self.plic.claim();
        if hwirq == 0 {
            return IrqStatus::Unhandled;
        }
        irq::dispatch_irq_line(IrqLine::Controller {
            controller: self.controller,
            hwirq,
        });
        self.plic.complete(hwirq);
        IrqStatus::Handled
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

    fn supervisor_context(
        info: &PlatformDeviceInfo,
        boot_hart_id: usize,
    ) -> Result<usize, PnpError> {
        let mut saw_irq = false;
        let mut supervisor_index = 0usize;
        for (index, irq) in info.irq_resources().enumerate() {
            saw_irq = true;
            if irq.cells().first().copied() == Some(RISCV_SUPERVISOR_EXTERNAL_IRQ) {
                if supervisor_index == boot_hart_id {
                    return Ok(index);
                }
                supervisor_index += 1;
            }
        }
        if saw_irq {
            Err(PnpError::malformed(
                PnpResourceKind::Irq,
                "plic supervisor external context missing",
            ))
        } else {
            Err(PnpError::missing(
                PnpResourceKind::Irq,
                "plic interrupts-extended missing",
            ))
        }
    }
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
        let ndev = info.u32_property("riscv,ndev").unwrap_or(0x5f); // 默认 95 源（QEMU virt）
        let Some((phys, _size)) = info.first_mmio() else {
            return Err(PnpError::missing(PnpResourceKind::Mmio, "plic reg missing"));
        };
        let mmio_base = (self.device_mmio_to_virt)(phys);
        let context = Self::supervisor_context(info, self.boot_hart_id)?;

        let plic = Arc::new(Plic {
            inner: Spinlock::new(PlicInner { mmio_base, ndev }),
            context,
        });

        // 初始化：所有源 priority=0（禁用），threshold=0
        for hwirq in 1..=ndev {
            let _ = plic.set_priority(hwirq, 0);
        }
        plic.set_threshold(0);

        // 注册 IRQ domain
        let domain: Arc<dyn IrqDomain> = Arc::new(PlicDomain {
            plic: Arc::clone(&plic),
            controller,
            ndev,
        });
        let domain_handle =
            irq::register_irq_domain(controller, domain).map_err(|_| PnpError::InvalidState)?;
        dev.own_resource(irq::irq_domain_pnp_resource(
            domain_handle,
            "platform-riscv-plic-domain",
        ))?;

        // 在 CPU 外部中断线上注册级联 handler
        let cascade = Arc::new(PlicCascadeHandler {
            plic: Arc::clone(&plic),
            controller,
        });
        let irq_handle = irq::register_irq_handler(IrqLine::Hardware(0), cascade)
            .map_err(|_| PnpError::InvalidState)?;
        dev.own_resource(irq::irq_handler_pnp_resource(
            irq_handle,
            "platform-riscv-plic-cascade",
        ))?;

        log::printk!(
            "[platform-riscv-plic] bound {} phys={:#x} ndev={} context={}",
            dev.id,
            phys,
            ndev,
            context
        );
        Ok(())
    }

    fn remove(&self, _dev: &Arc<PnpDevice>) {
        log::printk!("[platform-riscv-plic] removed");
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

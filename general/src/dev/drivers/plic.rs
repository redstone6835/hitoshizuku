//! RISC-V PLIC (Platform-Level Interrupt Controller) 平台驱动。
//!
//! 本模块匹配 DTB 中的 `sifive,plic-1.0.0` / `riscv,plic0` 中断控制器节点，
//! 将固件 IRQ specifier 翻译为 [`IrqLine::Controller`]，并提供 PLIC 硬件
//! enable/disable 操作。外部中断通过级联 handler 从 CPU `IrqLine::Hardware(0)`
//! 转入，claim → dispatch → complete。

use alloc::sync::Arc;

use vfs::sync::Spinlock;

use crate::dev::irq::{self, IrqDomain, IrqHandler, IrqLine, IrqStatus};
use crate::dev::platform::{FirmwarePropertyValue, PlatformDeviceInfo};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    PnpResourceKind, register_driver_factory,
};

const COMPAT_SIFIVE_PLIC: &str = "sifive,plic-1.0.0";
const COMPAT_RISCV_PLIC0: &str = "riscv,plic0";

const PLIC_PRIORITY_BASE: usize = 0x000000;
const PLIC_ENABLE_BASE: usize = 0x002000;
const PLIC_ENABLE_STRIDE: usize = 0x80;
const PLIC_THRESHOLD_BASE: usize = 0x200000;
const PLIC_CLAIM_BASE: usize = 0x200004;
const PLIC_CONTEXT_STRIDE: usize = 0x1000;

const PLIC_HART_CTX: usize = 0; // 单核，只用 hart context 0

struct PlicInner {
    mmio_base: usize,
    ndev: u32,
}

struct Plic {
    inner: Spinlock<PlicInner>,
    controller: u32,
}

impl Plic {
    fn claim(&self) -> u32 {
        let inner = self.inner.lock();
        let addr = inner.mmio_base + PLIC_CLAIM_BASE + PLIC_CONTEXT_STRIDE * PLIC_HART_CTX;
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }

    fn complete(&self, hwirq: u32) {
        let inner = self.inner.lock();
        let addr = inner.mmio_base + PLIC_CLAIM_BASE + PLIC_CONTEXT_STRIDE * PLIC_HART_CTX;
        unsafe { core::ptr::write_volatile(addr as *mut u32, hwirq) };
    }

    fn set_priority(&self, hwirq: u32, priority: u32) {
        let inner = self.inner.lock();
        let addr = inner.mmio_base + PLIC_PRIORITY_BASE + 4 * hwirq as usize;
        unsafe { core::ptr::write_volatile(addr as *mut u32, priority) };
    }

    fn set_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        if hwirq == 0 || hwirq > self.inner.lock().ndev {
            return false;
        }
        let inner = self.inner.lock();
        let reg_idx = hwirq as usize / 32;
        let bit = hwirq % 32;
        let addr =
            inner.mmio_base + PLIC_ENABLE_BASE + PLIC_ENABLE_STRIDE * PLIC_HART_CTX + 4 * reg_idx;
        let mut val = unsafe { core::ptr::read_volatile(addr as *const u32) };
        if enabled {
            val |= 1u32 << bit;
        } else {
            val &= !(1u32 << bit);
        }
        unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
        true
    }

    fn set_threshold(&self, threshold: u32) {
        let inner = self.inner.lock();
        let addr = inner.mmio_base + PLIC_THRESHOLD_BASE + PLIC_CONTEXT_STRIDE * PLIC_HART_CTX;
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
}

impl PlicDriver {
    fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.properties.interrupt_controller
            && (info.has_id(COMPAT_SIFIVE_PLIC) || info.has_id(COMPAT_RISCV_PLIC0))
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
        let ndev = info
            .fw_properties
            .iter()
            .find_map(|p| {
                if p.name.as_ref() == "riscv,ndev" {
                    match &p.value {
                        FirmwarePropertyValue::U32(v) => Some(*v),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .unwrap_or(0x5f); // 默认 95 源（QEMU virt）
        let Some((phys, _size)) = info.first_mmio() else {
            return Err(PnpError::missing(PnpResourceKind::Mmio, "plic reg missing"));
        };
        let mmio_base = (self.device_mmio_to_virt)(phys);

        let plic = Arc::new(Plic {
            inner: Spinlock::new(PlicInner { mmio_base, ndev }),
            controller,
        });

        // 初始化：所有源 priority=0（禁用），threshold=0
        for hwirq in 1..=ndev {
            plic.set_priority(hwirq, 0);
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
            "[platform-riscv-plic] bound {} phys={:#x} ndev={}",
            dev.id,
            phys,
            ndev
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
        Ok(Arc::new(PlicDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(PlicFactory)).map(|_| ())
}

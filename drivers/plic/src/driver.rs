//! RISC-V PLIC (Platform-Level Interrupt Controller) 平台 ELM 驱动。
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
const INVALID_CONTEXT: usize = usize::MAX;
/// 一次外部中断入口最多处理的 pending source 数。
///
/// 正常情况下 claim 返回 0 就会结束；上限只用于防止设备未清除 level IRQ 时，
/// 单个 hart 在一个 trap 中无限循环而饿死调度器。达到上限后保持 source pending，
/// PLIC 会在返回后重新触发外部中断。
const PLIC_MAX_CLAIMS_PER_ENTRY: usize = 64;

#[derive(Clone, Copy)]
struct ContextBinding {
    /// DTB `interrupts-extended` 中的 PLIC context 编号。
    context: usize,
    /// 对应 CPU 的固件 hart ID，仅用于启动期把 DTB 顺序映射到调度逻辑 CPU。
    hart_id: u64,
}

struct Plic {
    /// MMIO 基址、源数量和每 CPU context 在 probe 后不会变化；claim/complete
    /// 位于硬中断热路径，不能为读取这些只读字段获取配置锁。
    mmio_base: usize,
    ndev: u32,
    contexts: [usize; sched::NR_CPUS],
    config_lock: Spinlock<()>,
}

impl Plic {
    #[inline(always)]
    fn current_context(&self) -> Option<usize> {
        let cpu = sched::current_cpu_id();
        let context = *self.contexts.get(cpu)?;
        (context != INVALID_CONTEXT).then_some(context)
    }

    #[inline(always)]
    fn claim(&self) -> Option<(usize, u32)> {
        let context = self.current_context()?;
        let addr = self.mmio_base + PLIC_CLAIM_BASE + PLIC_CONTEXT_STRIDE * context;
        // Safety: PLIC 实例由 platform probe 在固件声明的 MMIO 窗口上映射创建，
        // `context` 来自已校验的 supervisor 中断上下文。
        let hwirq = unsafe { core::ptr::read_volatile(addr as *const u32) };
        Some((context, hwirq))
    }

    #[inline(always)]
    fn complete(&self, context: usize, hwirq: u32) {
        let addr = self.mmio_base + PLIC_CLAIM_BASE + PLIC_CONTEXT_STRIDE * context;
        // Safety: 安全条件与 `claim` 相同，claim/complete 寄存器允许 32 位易失写入。
        unsafe { core::ptr::write_volatile(addr as *mut u32, hwirq) };
    }

    fn set_priority(&self, hwirq: u32, priority: u32) -> bool {
        let _config = self.config_lock.lock();
        if hwirq == 0 || hwirq > self.ndev {
            return false;
        }
        let addr = self.mmio_base + PLIC_PRIORITY_BASE + 4 * hwirq as usize;
        // Safety: `hwirq` 已验证处于固件声明的 PLIC 中断源范围，所得优先级寄存器
        // 地址位于已映射窗口内并按 32 位对齐。
        unsafe { core::ptr::write_volatile(addr as *mut u32, priority) };
        true
    }

    fn set_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        let _config = self.config_lock.lock();
        if hwirq == 0 || hwirq > self.ndev {
            return false;
        }
        let reg_idx = hwirq as usize / 32;
        let bit = hwirq % 32;
        // PLIC 的 enable 位是每 context 一份。配置锁只覆盖冷路径的
        // read-modify-write；claim/complete 不读取这把锁。
        for &context in &self.contexts {
            if context == INVALID_CONTEXT {
                continue;
            }
            let addr =
                self.mmio_base + PLIC_ENABLE_BASE + PLIC_ENABLE_STRIDE * context + 4 * reg_idx;
            // Safety: `hwirq` 和 `context` 已由 probe 校验，地址指向当前上下文的
            // 对齐使能寄存器。
            let mut val = unsafe { core::ptr::read_volatile(addr as *const u32) };
            if enabled {
                val |= 1u32 << bit;
            } else {
                val &= !(1u32 << bit);
            }
            // Safety: 与上面的读取访问同一有效使能寄存器，并由配置锁串行化修改。
            unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
        }
        true
    }

    fn set_threshold(&self, threshold: u32) {
        let _config = self.config_lock.lock();
        for &context in &self.contexts {
            if context == INVALID_CONTEXT {
                continue;
            }
            let addr = self.mmio_base + PLIC_THRESHOLD_BASE + PLIC_CONTEXT_STRIDE * context;
            // Safety: `context` 已校验，地址指向已映射窗口内的对齐阈值寄存器。
            unsafe { core::ptr::write_volatile(addr as *mut u32, threshold) };
        }
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
        let mut claimed = false;
        for _ in 0..PLIC_MAX_CLAIMS_PER_ENTRY {
            let Some((context, hwirq)) = self.plic.claim() else {
                break;
            };
            if hwirq == 0 {
                break;
            }
            claimed = true;
            let _ = irq::dispatch_irq_line(IrqLine::Controller {
                controller: self.controller,
                hwirq,
            });
            // 必须使用 claim 返回的同一个 context 完成；不能重新按 current CPU
            // 查询，否则极端的 hart 迁移/嵌套窗口会把完成写到另一份寄存器。
            self.plic.complete(context, hwirq);
        }
        if claimed {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
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
    ) -> Result<[usize; sched::NR_CPUS], PnpError> {
        let topology = general::dev::cpu::snapshot_topology();
        let mut supervisor = [INVALID_CONTEXT; sched::NR_CPUS];
        let mut saw_irq = false;
        let mut count = 0usize;
        for (index, irq) in info.irq_resources().enumerate() {
            saw_irq = true;
            if irq.cells().first().copied() == Some(RISCV_SUPERVISOR_EXTERNAL_IRQ) {
                if count == sched::NR_CPUS {
                    break;
                }
                supervisor[count] = index;
                count += 1;
            }
        }
        if count == 0 {
            return if saw_irq {
                Err(PnpError::malformed(
                    PnpResourceKind::Irq,
                    "plic supervisor external context missing",
                ))
            } else {
                Err(PnpError::missing(
                    PnpResourceKind::Irq,
                    "plic interrupts-extended missing",
                ))
            };
        }

        // RISC-V DTB binding places one M/S pair per hart in `interrupts-extended`.
        // The normalized platform layer intentionally keeps the parent phandle opaque,
        // so use the same boot-first, hart-id order as riscv64 SMP startup to translate
        // the preserved DTB order into scheduler logical CPU IDs. This handles a
        // non-zero boot hart while retaining a fixed, allocation-free hot-path table.
        let mut bindings = [ContextBinding {
            context: INVALID_CONTEXT,
            hart_id: u64::MAX,
        }; sched::NR_CPUS];
        for (binding_index, (resource_index, irq)) in info
            .irq_resources()
            .enumerate()
            .filter(|(_, irq)| irq.cells().first().copied() == Some(RISCV_SUPERVISOR_EXTERNAL_IRQ))
            .take(count)
            .enumerate()
        {
            // `interrupts-extended` preserves the CPU interrupt-controller phandle.
            // Prefer that identity over the raw resource position, because valid DTBs
            // may list CPU nodes in an order different from the scheduler's boot-first
            // order. The resource position remains the PLIC context number fallback,
            // matching the standard one-M/S-pair-per-hart binding used by QEMU and
            // Linux's PLIC context enumeration.
            let matched_cpu = irq.controller().and_then(|phandle| {
                topology
                    .iter()
                    .find(|entry| entry.phandle == Some(phandle))
            });
            let fallback_cpu = topology.get(binding_index);
            bindings[binding_index] = ContextBinding {
                context: resource_index,
                hart_id: matched_cpu
                    .or(fallback_cpu)
                    .map(|entry| entry.reg)
                    .unwrap_or(binding_index as u64),
            };
        }
        bindings[..count].sort_unstable_by_key(|binding| {
            (binding.hart_id != boot_hart_id as u64, binding.hart_id)
        });
        let mut contexts = [INVALID_CONTEXT; sched::NR_CPUS];
        for (logical, binding) in bindings[..count].iter().enumerate() {
            contexts[logical] = binding.context;
        }
        Ok(contexts)
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
        let Some((phys, mmio_size)) = info.first_mmio() else {
            return Err(PnpError::missing(PnpResourceKind::Mmio, "plic reg missing"));
        };
        let mmio_base = (self.device_mmio_to_virt)(phys);
        let contexts = Self::supervisor_contexts(info, self.boot_hart_id)?;
        if mmio_size != 0 {
            for &context in contexts
                .iter()
                .filter(|context| **context != INVALID_CONTEXT)
            {
                let Some(offset) =
                    PLIC_CLAIM_BASE.checked_add(PLIC_CONTEXT_STRIDE.saturating_mul(context))
                else {
                    return Err(PnpError::malformed(
                        PnpResourceKind::Mmio,
                        "plic context offset overflow",
                    ));
                };
                if offset
                    .checked_add(core::mem::size_of::<u32>())
                    .is_none_or(|end| end > mmio_size)
                {
                    return Err(PnpError::malformed(
                        PnpResourceKind::Mmio,
                        "plic supervisor context outside reg window",
                    ));
                }
            }
        }

        let plic = Arc::new(Plic {
            mmio_base,
            ndev,
            contexts,
            config_lock: Spinlock::new(()),
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
            "[platform-riscv-plic] bound {} phys={:#x} ndev={} contexts={:?}",
            dev.id,
            phys,
            ndev,
            contexts
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

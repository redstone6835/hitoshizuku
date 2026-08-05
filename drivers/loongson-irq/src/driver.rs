//! Loongson 固件中断控制器 platform ELM 驱动。
//!
//! 本模块按固件描述的 interrupt-controller 层级注册 IRQ domain，而不是让设备
//! 驱动猜测自己挂在哪一级控制器下：
//!
//! 1. CPU interrupt-controller 把 DTB CPU interrupt specifier 翻译成架构层
//!    使用的 [`IrqLine::Hardware`]；
//! 2. EIOINTC 通过 LoongArch IOCSR pending 位图 demux 外部向量，再分发自己的
//!    [`IrqLine::Controller`] 子线；
//! 3. PCH PIC 把设备子中断源映射到父 EIOINTC vector，完成 mask/type/ack 后再
//!    分发 PCH domain 子线。
//!
//! 这样 RTC、串口或未来其它设备只消费固件 IRQ 资源，不需要知道 `interrupts`
//! 里的 cell 含义，也不需要在设备驱动里硬编码 PCH/EIOINTC 路径。

use alloc::sync::Arc;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

use crate::dev::irq::{self, IrqDomain, IrqError, IrqHandle, IrqHandler, IrqLine, IrqStatus};
use crate::dev::msi::{self, MsiController, MsiError, MsiMessage, MsiVector};
use crate::dev::platform::{PlatformDeviceInfo, PlatformIrqResolveError};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDependency, PnpDevice,
    PnpDriver, PnpError, PnpId, PnpResourceKind, register_driver_factory, unregister_driver,
};
use crate::eio_layout::EioIntcIocsrWindow;

const COMPAT_LOONGSON_CPUIC: &str = "loongson,cpu-interrupt-controller";
const COMPAT_LOONGSON_EIOINTC: &str = "loongson,ls2k2000-eiointc";
const COMPAT_LOONGSON_EIOINTC_GENERIC: &str = "loongson,eiointc-1.0";
const COMPAT_LOONGSON_PCH_PIC: &str = "loongson,pch-pic-1.0";
const COMPAT_LOONGSON_PCH_MSI: &str = "loongson,pch-msi-1.0";

const LOONGARCH_CPU_HWI_BASE: u32 = 2;
const LOONGARCH_CPU_HWI_COUNT: u32 = 6;

const LOONGARCH_IOCSR_MISC_FUNC: usize = 0x420;
const IOCSR_MISC_FUNC_EXT_IOI_EN: u64 = 1u64 << 48;

const EIOINTC_VECTOR_COUNT: u32 = 256;
const EIOINTC_VECTOR_BITS_PER_REG: u32 = 32;
const EIOINTC_VECTOR_BITS_PER_ISR: u32 = 64;
const EIOINTC_IPMAP_PARENT_LIMIT: usize = 4;
const EIOINTC_PACKED_FIELDS_PER_REG: u32 = 4;
const EIOINTC_PACKED_FIELD_BITS: u32 = 8;
const EIOINTC_NODEMAP_GROUP_STRIDE_BITS: u32 = 2;
const EIOINTC_NODEMAP_MIRROR_SHIFT: u32 = 16;
const EIOINTC_DEFAULT_ROUTE_CPU: u8 = 1;
const EIOINTC_PROP_ROUTE_CPU: &str = "loongson,eiointc-route-cpu";

const PCH_PIC_IRQ_COUNT: u32 = 64;
const PCH_PIC_IRQ_COUNT_USIZE: usize = PCH_PIC_IRQ_COUNT as usize;
const PCH_PIC_REG_MASK: usize = 0x20;
const PCH_PIC_REG_HTMSI_EN: usize = 0x40;
const PCH_PIC_REG_EDGE: usize = 0x60;
const PCH_PIC_REG_CLEAR: usize = 0x80;
const PCH_PIC_REG_AUTO_CTRL0: usize = 0xc0;
const PCH_PIC_REG_AUTO_CTRL1: usize = 0xe0;
const PCH_PIC_REG_ROUTE: usize = 0x100;
const PCH_PIC_REG_HTVEC: usize = 0x200;
const PCH_PIC_REG_POL: usize = 0x3e0;
/// 覆盖最后一个 polarity 寄存器（offset 0x3e4，32-bit）的最小 MMIO 窗口。
const PCH_PIC_REQUIRED_MMIO_SIZE: usize = PCH_PIC_REG_POL + 2 * core::mem::size_of::<u32>();
const PCH_PIC_DEFAULT_ROUTE_TARGET: u8 = 1;
const PCH_PIC_PROP_BASE_VECTOR: &str = "loongson,pic-base-vec";
const PCH_PIC_PROP_ROUTE_TARGET: &str = "loongson,pic-route-target";

const PCH_MSI_PROP_BASE_VECTOR: &str = "loongson,msi-base-vec";
const PCH_MSI_PROP_NUM_VECS: &str = "loongson,msi-num-vecs";

const IRQ_TYPE_EDGE_RISING: u32 = 1;
const IRQ_TYPE_EDGE_FALLING: u32 = 2;
const IRQ_TYPE_LEVEL_HIGH: u32 = 4;
const IRQ_TYPE_LEVEL_LOW: u32 = 8;

struct LoongsonCpuIrqDomain;

impl IrqDomain for LoongsonCpuIrqDomain {
    fn translate(&self, cells: &[u32]) -> Option<IrqLine> {
        let [line] = cells else {
            return None;
        };
        if !(LOONGARCH_CPU_HWI_BASE..LOONGARCH_CPU_HWI_BASE + LOONGARCH_CPU_HWI_COUNT)
            .contains(line)
        {
            return None;
        }
        Some(IrqLine::Hardware((line - LOONGARCH_CPU_HWI_BASE) as usize))
    }
}

pub struct LoongsonCpuIrqDriver;

impl LoongsonCpuIrqDriver {
    const fn new() -> Self {
        Self
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.properties.interrupt_controller && info.has_id(COMPAT_LOONGSON_CPUIC)
    }
}

impl PnpDriver for LoongsonCpuIrqDriver {
    fn name(&self) -> &'static str {
        "platform-loongson-cpuic"
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
        let info = platform_info(dev)?;
        let controller = info.properties.fw_phandle.ok_or(PnpError::missing(
            PnpResourceKind::IrqDomain,
            "cpuic phandle missing",
        ))?;
        dev.reserve_owned_resources(1)?;
        let handle = irq::register_irq_domain(controller, Arc::new(LoongsonCpuIrqDomain))
            .map_err(map_irq_error)?;
        if let Err(err) = dev.own_resource(irq::irq_domain_pnp_resource(
            handle,
            "loongson-cpuic-domain",
        )) {
            let _ = irq::unregister_irq_domain(handle);
            return Err(err);
        }
        Ok(())
    }

    fn remove(&self, _dev: &Arc<PnpDevice>) {}
}

#[derive(Clone, Copy)]
struct EioIntcRouteConfig {
    parent_hwi: usize,
    route_cpu: u8,
}

impl EioIntcRouteConfig {
    fn from_platform(parent_hwi: usize, info: &PlatformDeviceInfo) -> Option<Self> {
        if parent_hwi >= EIOINTC_IPMAP_PARENT_LIMIT {
            return None;
        }
        let route_cpu = match info.u32_property(EIOINTC_PROP_ROUTE_CPU) {
            Some(value) => u8::try_from(value).ok()?,
            None => EIOINTC_DEFAULT_ROUTE_CPU,
        };
        Some(Self {
            parent_hwi,
            route_cpu,
        })
    }

    fn ipmap_value(self) -> u32 {
        let parent_bit = 1u8 << self.parent_hwi;
        repeat_byte(parent_bit)
    }

    fn route_value(self) -> u32 {
        repeat_byte(self.route_cpu)
    }

    fn nodemap_value(self, reg: u32) -> u32 {
        // 当前固件只提供单节点路由信息时，按硬件分组格式生成节点选择位。
        // 该逻辑集中在配置对象内，后续多节点拓扑只需要扩展这里的策略。
        let node_bit = 1u32 << (reg * EIOINTC_NODEMAP_GROUP_STRIDE_BITS);
        node_bit | (node_bit << EIOINTC_NODEMAP_MIRROR_SHIFT)
    }
}

struct EioIntc {
    controller: u32,
    route: EioIntcRouteConfig,
    registers: EioIntcIocsrWindow,
}

impl EioIntc {
    fn new(controller: u32, route: EioIntcRouteConfig, registers: EioIntcIocsrWindow) -> Self {
        Self {
            controller,
            route,
            registers,
        }
    }

    fn initialize(&self) -> Result<(), PnpError> {
        let misc = iocsr_read64(LOONGARCH_IOCSR_MISC_FUNC)?;
        iocsr_write64(LOONGARCH_IOCSR_MISC_FUNC, misc | IOCSR_MISC_FUNC_EXT_IOI_EN)?;
        self.program_route_tables()?;
        self.set_all_vectors_enabled(false)?;
        self.clear_pending()?;
        Ok(())
    }

    fn program_route_tables(&self) -> Result<(), PnpError> {
        let ipmap = self.route.ipmap_value();
        for reg in
            0..EIOINTC_VECTOR_COUNT / EIOINTC_VECTOR_BITS_PER_REG / EIOINTC_PACKED_FIELDS_PER_REG
        {
            iocsr_write32(self.registers.ipmap(reg), ipmap)?;
        }

        for reg in 0..EIOINTC_VECTOR_COUNT / EIOINTC_VECTOR_BITS_PER_REG {
            iocsr_write32(self.registers.nodemap(reg), self.route.nodemap_value(reg))?;
        }

        let route = self.route.route_value();
        for reg in 0..EIOINTC_VECTOR_COUNT / EIOINTC_PACKED_FIELDS_PER_REG {
            iocsr_write32(self.registers.route(reg), route)?;
        }
        Ok(())
    }

    fn set_all_vectors_enabled(&self, enabled: bool) -> Result<(), PnpError> {
        let value = if enabled { u32::MAX } else { 0 };
        for reg in 0..EIOINTC_VECTOR_COUNT / EIOINTC_VECTOR_BITS_PER_REG {
            iocsr_write32(self.registers.enable(reg), value)?;
            iocsr_write32(self.registers.bounce(reg), 0)?;
        }
        Ok(())
    }

    fn clear_pending(&self) -> Result<(), PnpError> {
        for reg in 0..EIOINTC_VECTOR_COUNT / EIOINTC_VECTOR_BITS_PER_ISR {
            iocsr_write64(self.registers.isr64(reg), u64::MAX)?;
        }
        Ok(())
    }

    fn set_vector_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        if hwirq >= EIOINTC_VECTOR_COUNT {
            return false;
        }
        let reg = hwirq / EIOINTC_VECTOR_BITS_PER_REG;
        let bit = hwirq % EIOINTC_VECTOR_BITS_PER_REG;
        let offset = self.registers.enable(reg);
        let Some(mut value) = irq::iocsr_read32(offset) else {
            return false;
        };
        if enabled {
            value |= 1u32 << bit;
        } else {
            value &= !(1u32 << bit);
        }
        irq::iocsr_write32(offset, value)
    }

    fn dispatch_pending(&self) -> IrqStatus {
        let mut handled = false;
        for reg in 0..EIOINTC_VECTOR_COUNT / EIOINTC_VECTOR_BITS_PER_ISR {
            let offset = self.registers.isr64(reg);
            let Some(mut pending) = irq::iocsr_read64(offset) else {
                continue;
            };
            if pending == 0 {
                continue;
            }
            let _ = irq::iocsr_write64(offset, pending);
            while pending != 0 {
                let bit = pending.trailing_zeros();
                pending &= !(1u64 << bit);
                let hwirq = reg * EIOINTC_VECTOR_BITS_PER_ISR + bit;
                handled |= irq::dispatch_irq_line(IrqLine::Controller {
                    controller: self.controller,
                    hwirq,
                });
            }
        }
        if handled {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }
}

impl IrqDomain for EioIntc {
    fn translate(&self, cells: &[u32]) -> Option<IrqLine> {
        let [hwirq] = cells else {
            return None;
        };
        if *hwirq >= EIOINTC_VECTOR_COUNT {
            return None;
        }
        Some(IrqLine::Controller {
            controller: self.controller,
            hwirq: *hwirq,
        })
    }

    fn set_line_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        self.set_vector_enabled(hwirq, enabled)
    }
}

struct EioIntcIrqHandler {
    intc: Arc<EioIntc>,
}

impl IrqHandler for EioIntcIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        self.intc.dispatch_pending()
    }
}

fn repeat_byte(value: u8) -> u32 {
    let mut out = 0u32;
    for field in 0..EIOINTC_PACKED_FIELDS_PER_REG {
        out |= (value as u32) << (field * EIOINTC_PACKED_FIELD_BITS);
    }
    out
}

pub struct EioIntcDriver;

impl EioIntcDriver {
    const fn new() -> Self {
        Self
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.properties.interrupt_controller
            && (info.has_id(COMPAT_LOONGSON_EIOINTC)
                || info.has_id(COMPAT_LOONGSON_EIOINTC_GENERIC))
    }
}

impl PnpDriver for EioIntcDriver {
    fn name(&self) -> &'static str {
        "platform-loongson-eiointc"
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
        let info = platform_info(dev)?;
        let controller = info.properties.fw_phandle.ok_or(PnpError::missing(
            PnpResourceKind::IrqDomain,
            "eiointc phandle missing",
        ))?;
        let parent_line = match info.resolve_first_irq_line() {
            Ok(line) => line,
            Err(PlatformIrqResolveError::Unresolved) => {
                return Err(PnpError::dependency(first_irq_dependency(info)));
            }
            Err(PlatformIrqResolveError::NoResource) => {
                return Err(PnpError::missing(
                    PnpResourceKind::Irq,
                    "eiointc parent irq",
                ));
            }
        };
        let IrqLine::Hardware(parent_hwi) = parent_line else {
            return Err(PnpError::malformed(
                PnpResourceKind::Irq,
                "eiointc parent is not cpu hardware irq",
            ));
        };
        let route = EioIntcRouteConfig::from_platform(parent_hwi, info).ok_or(
            PnpError::malformed(PnpResourceKind::IrqDomain, "invalid eiointc route"),
        )?;
        // 该 binding 的 `reg` 是 IOCSR 寄存器编号窗口。platform core 只负责保留
        // DT `reg` tuple，因此这里不做 phys_to_virt，而是由 EIOINTC 布局解释地址。
        let mut iocsr_resources = info.mmio_resources();
        let (iocsr_base, iocsr_size) = iocsr_resources.next().ok_or(PnpError::missing(
            PnpResourceKind::Mmio,
            "eiointc IOCSR reg missing",
        ))?;
        if iocsr_resources.next().is_some() {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "eiointc requires exactly one IOCSR reg window",
            ));
        }
        let registers = EioIntcIocsrWindow::new(iocsr_base, iocsr_size).map_err(|_| {
            PnpError::malformed(PnpResourceKind::Mmio, "invalid eiointc IOCSR reg window")
        })?;
        if registers.uses_qemu_legacy_route_extension() {
            log::warning!(
                "[loongson-eiointc] DT IOCSR window {:#x}+{:#x} omits route table; using QEMU legacy adjacent route extension",
                registers.base(),
                registers.declared_size()
            );
        }
        let intc = Arc::new(EioIntc::new(controller, route, registers));
        intc.initialize()?;
        dev.reserve_owned_resources(2)?;
        let handler: Arc<dyn IrqHandler> = Arc::new(EioIntcIrqHandler {
            intc: Arc::clone(&intc),
        });
        let parent_irq = match irq::register_irq_handler(parent_line, handler) {
            Ok(handle) => handle,
            Err(err) => {
                return Err(map_irq_error(err));
            }
        };
        if let Err(err) = dev.own_resource(irq::irq_handler_pnp_resource(
            parent_irq,
            "loongson-eiointc-parent",
        )) {
            let _ = irq::unregister_irq_handler(parent_irq);
            return Err(err);
        }
        // IRQ domain 登记会同步唤醒 deferred consumer；必须等父级级联 handler
        // 已经可用后再发布 provider，避免 consumer 提前打开子中断却无人分发。
        let domain = irq::register_irq_domain(controller, intc.clone()).map_err(map_irq_error)?;
        if let Err(err) = dev.own_resource(irq::irq_domain_pnp_resource(
            domain,
            "loongson-eiointc-domain",
        )) {
            let _ = irq::unregister_irq_domain(domain);
            return Err(err);
        }
        Ok(())
    }

    fn remove(&self, _dev: &Arc<PnpDevice>) {}
}

#[derive(Clone, Copy)]
struct PchPicMapping {
    source: u32,
    irq_type: u32,
}

impl PchPicMapping {
    const fn is_edge(self) -> bool {
        matches!(self.irq_type, IRQ_TYPE_EDGE_RISING | IRQ_TYPE_EDGE_FALLING)
    }
}

#[derive(Clone, Copy)]
struct PchPicSlot {
    /// 已被固件设备引用的 PCH source 映射。
    mapping: PchPicMapping,
    /// 对应父级 vector 上注册的级联 handler。slot 释放时必须同步注销。
    parent_irq: IrqHandle,
}

struct PchPicInner {
    slots: [Option<PchPicSlot>; PCH_PIC_IRQ_COUNT_USIZE],
}

impl PchPicInner {
    const fn new() -> Self {
        Self {
            slots: [None; PCH_PIC_IRQ_COUNT_USIZE],
        }
    }
}

#[derive(Clone, Copy)]
struct PchPicRouteTarget {
    raw: u8,
}

impl PchPicRouteTarget {
    fn from_platform(info: &PlatformDeviceInfo) -> Option<Self> {
        let raw = match info.u32_property(PCH_PIC_PROP_ROUTE_TARGET) {
            Some(value) => u8::try_from(value).ok()?,
            None => PCH_PIC_DEFAULT_ROUTE_TARGET,
        };
        Some(Self { raw })
    }

    const fn raw(self) -> u8 {
        self.raw
    }
}

struct PchPic {
    controller: u32,
    parent_controller: u32,
    base_vector: u32,
    mmio_base: usize,
    route_target: PchPicRouteTarget,
    inner: Spinlock<PchPicInner>,
}

impl PchPic {
    fn new(
        controller: u32,
        parent_controller: u32,
        base_vector: u32,
        mmio_base: usize,
        route_target: PchPicRouteTarget,
    ) -> Self {
        Self {
            controller,
            parent_controller,
            base_vector,
            mmio_base,
            route_target,
            inner: Spinlock::new(PchPicInner::new()),
        }
    }

    fn reset(&self) {
        for reg in 0..PCH_PIC_IRQ_COUNT / 32 {
            let offset = reg as usize * 4;
            // PCH_PIC_MASK 为 1 表示屏蔽。复位阶段先屏蔽所有源，后续由
            // set_line_enabled() 按设备实际注册状态逐条 unmask，避免未绑定设备
            // 或旧 pending 在控制器初始化期间产生中断风暴。
            self.write32(PCH_PIC_REG_MASK + offset, u32::MAX);
            self.write32(PCH_PIC_REG_HTMSI_EN + offset, u32::MAX);
            self.write32(PCH_PIC_REG_EDGE + offset, 0);
            self.write32(PCH_PIC_REG_POL + offset, 0);
            self.write32(PCH_PIC_REG_CLEAR + offset, u32::MAX);
            self.write32(PCH_PIC_REG_AUTO_CTRL0 + offset, 0);
            self.write32(PCH_PIC_REG_AUTO_CTRL1 + offset, 0);
        }
        for source in 0..PCH_PIC_IRQ_COUNT {
            self.write8(PCH_PIC_REG_ROUTE + source as usize, self.route_target.raw());
            self.write8(PCH_PIC_REG_HTVEC + source as usize, source as u8);
        }
    }

    fn parent_line(&self, slot: u32) -> Option<IrqLine> {
        let vector = self.base_vector.checked_add(slot)?;
        irq::translate_firmware_irq(Some(self.parent_controller), &[vector])
    }

    fn translate_source(self: &Arc<Self>, cells: &[u32]) -> Option<IrqLine> {
        let (source, irq_type) = match cells {
            [source, irq_type] => (*source, *irq_type),
            [source] => (*source, IRQ_TYPE_LEVEL_HIGH),
            _ => return None,
        };
        if source >= PCH_PIC_IRQ_COUNT {
            return None;
        }
        let inner = self.inner.lock();
        if let Some((slot, _)) = inner
            .slots
            .iter()
            .enumerate()
            .find(|(_, entry)| entry.is_some_and(|entry| entry.mapping.source == source))
        {
            return Some(IrqLine::Controller {
                controller: self.controller,
                hwirq: slot as u32,
            });
        }
        let slot = inner.slots.iter().position(Option::is_none)? as u32;
        drop(inner);

        // 只有当某个设备 IRQ specifier 真正分配到本 slot 时，才在父级
        // domain 上安装级联 handler。这样未使用的 PCH source 不会提前打开
        // 上游 vector，也让热移除时可以按 slot 精确释放资源。
        let parent_irq = self.register_parent_handler(slot)?;
        let vector = self.base_vector.checked_add(slot)?;
        if vector > u8::MAX as u32 {
            let _ = irq::unregister_irq_handler(parent_irq);
            return None;
        }
        let mapping = PchPicMapping { source, irq_type };
        if self.program_source(slot, mapping).is_none() {
            let _ = irq::unregister_irq_handler(parent_irq);
            return None;
        }

        let mut inner = self.inner.lock();
        if inner.slots[slot as usize].is_some() {
            let _ = irq::unregister_irq_handler(parent_irq);
            return inner
                .slots
                .iter()
                .enumerate()
                .find(|(_, entry)| entry.is_some_and(|entry| entry.mapping.source == source))
                .map(|(existing_slot, _)| IrqLine::Controller {
                    controller: self.controller,
                    hwirq: existing_slot as u32,
                });
        }
        inner.slots[slot as usize] = Some(PchPicSlot {
            mapping,
            parent_irq,
        });
        Some(IrqLine::Controller {
            controller: self.controller,
            hwirq: slot,
        })
    }

    fn register_parent_handler(self: &Arc<Self>, slot: u32) -> Option<IrqHandle> {
        let parent_line = self.parent_line(slot)?;
        let handler: Arc<dyn IrqHandler> = Arc::new(PchPicCascadeHandler {
            pic: Arc::clone(self),
            slot,
        });
        irq::register_irq_handler(parent_line, handler).ok()
    }

    fn unregister_parent_handlers(&self) {
        // 先从 slot 表里取走所有 handler 句柄，再在锁外注销。注销过程会回调
        // IRQ registry 和父 domain，不能持有 PCH 内部锁进入外层基础设施。
        let handles: Vec<IrqHandle> = {
            let mut inner = self.inner.lock();
            let mut handles = Vec::new();
            for entry in inner.slots.iter_mut() {
                if let Some(slot) = entry.take() {
                    handles.push(slot.parent_irq);
                }
            }
            handles
        };
        for handle in handles {
            let _ = irq::unregister_irq_handler(handle);
        }
    }

    fn program_source(&self, slot: u32, mapping: PchPicMapping) -> Option<()> {
        let vector = self.base_vector.checked_add(slot)?;
        self.write8(
            PCH_PIC_REG_HTVEC + mapping.source as usize,
            u8::try_from(vector).ok()?,
        );
        self.write8(
            PCH_PIC_REG_ROUTE + mapping.source as usize,
            self.route_target.raw(),
        );
        self.configure_type(mapping.source, mapping.irq_type);
        self.set_source_enabled(mapping.source, false);
        self.clear_source(mapping.source);
        Some(())
    }

    fn configure_type(&self, source: u32, irq_type: u32) {
        let edge = matches!(irq_type, IRQ_TYPE_EDGE_RISING | IRQ_TYPE_EDGE_FALLING);
        let active_low = matches!(irq_type, IRQ_TYPE_EDGE_FALLING | IRQ_TYPE_LEVEL_LOW);
        self.write_bit_state(PCH_PIC_REG_EDGE, source, edge);
        self.write_bit_state(PCH_PIC_REG_POL, source, active_low);
    }

    fn set_slot_enabled(&self, slot: u32, enabled: bool) -> bool {
        let Some(mapping) = self.slot_mapping(slot) else {
            return false;
        };
        self.set_source_enabled(mapping.source, enabled);
        self.clear_source(mapping.source);
        if let Some(parent) = self.parent_line(slot) {
            let _ = irq::set_irq_line_enabled(parent, enabled);
        }
        true
    }

    fn handle_slot(&self, slot: u32) -> IrqStatus {
        let Some(mapping) = self.slot_mapping(slot) else {
            return IrqStatus::Unhandled;
        };
        if mapping.is_edge() {
            self.clear_source(mapping.source);
        }
        if irq::dispatch_irq_line(IrqLine::Controller {
            controller: self.controller,
            hwirq: slot,
        }) {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }

    fn slot_mapping(&self, slot: u32) -> Option<PchPicMapping> {
        if slot >= PCH_PIC_IRQ_COUNT {
            return None;
        }
        self.inner.lock().slots[slot as usize].map(|entry| entry.mapping)
    }

    fn set_source_enabled(&self, source: u32, enabled: bool) {
        // MASK bit 的硬件语义与 enabled 相反：1 = masked，0 = unmasked。
        self.write_bit_state(PCH_PIC_REG_MASK, source, !enabled);
    }

    fn clear_source(&self, source: u32) {
        let reg = source / 32;
        let bit = source % 32;
        self.write32(PCH_PIC_REG_CLEAR + reg as usize * 4, 1u32 << bit);
    }

    fn write_bit_state(&self, base: usize, source: u32, set: bool) {
        let reg = source / 32;
        let bit = source % 32;
        let offset = base + reg as usize * 4;
        let mut value = self.read32(offset);
        if set {
            value |= 1u32 << bit;
        } else {
            value &= !(1u32 << bit);
        }
        self.write32(offset, value);
    }

    fn read32(&self, offset: usize) -> u32 {
        // Safety: PCH PIC 实例只在固件声明的控制器 MMIO 窗口上映射创建，所有调用
        // 偏移均由该控制器的固定寄存器布局和已校验的中断源编号计算得到。
        unsafe { core::ptr::read_volatile((self.mmio_base + offset) as *const u32) }
    }

    fn write32(&self, offset: usize, value: u32) {
        // Safety: 安全条件与 `read32` 相同，目标寄存器支持对齐的 32 位易失写入。
        unsafe { core::ptr::write_volatile((self.mmio_base + offset) as *mut u32, value) };
    }

    fn write8(&self, offset: usize, value: u8) {
        // Safety: 地址位于已映射的 PCH PIC 窗口，路由和向量寄存器按规范支持单字节写入。
        unsafe { core::ptr::write_volatile((self.mmio_base + offset) as *mut u8, value) };
    }
}

struct PchPicDomain {
    pic: Arc<PchPic>,
}

impl IrqDomain for PchPicDomain {
    fn translate(&self, cells: &[u32]) -> Option<IrqLine> {
        self.pic.translate_source(cells)
    }

    fn set_line_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        self.pic.set_slot_enabled(hwirq, enabled)
    }
}

struct PchPicCascadeHandler {
    pic: Arc<PchPic>,
    slot: u32,
}

impl IrqHandler for PchPicCascadeHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        self.pic.handle_slot(self.slot)
    }
}

struct PchPicBinding {
    pic: Arc<PchPic>,
}

pub struct PchPicDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl PchPicDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.properties.interrupt_controller && info.has_id(COMPAT_LOONGSON_PCH_PIC)
    }
}

impl PnpDriver for PchPicDriver {
    fn name(&self) -> &'static str {
        "platform-loongson-pch-pic"
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
        let info = platform_info(dev)?;
        let controller = info.properties.fw_phandle.ok_or(PnpError::missing(
            PnpResourceKind::IrqDomain,
            "pch-pic phandle missing",
        ))?;
        let parent_controller = info
            .properties
            .fw_interrupt_parent
            .ok_or(PnpError::missing(
                PnpResourceKind::IrqDomain,
                "pch-pic interrupt-parent missing",
            ))?;
        let Some((phys, size)) = info.first_mmio() else {
            return Err(PnpError::missing(
                PnpResourceKind::Mmio,
                "pch-pic reg missing",
            ));
        };
        if !phys.is_multiple_of(core::mem::align_of::<u32>())
            || size < PCH_PIC_REQUIRED_MMIO_SIZE
            || phys.checked_add(size).is_none()
        {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "pch-pic reg window is truncated or misaligned",
            ));
        }
        let base_vector = info.u32_property(PCH_PIC_PROP_BASE_VECTOR).unwrap_or(0);
        if base_vector
            .checked_add(PCH_PIC_IRQ_COUNT - 1)
            .is_none_or(|last| last > u8::MAX as u32)
        {
            return Err(PnpError::malformed(
                PnpResourceKind::IrqDomain,
                "pch-pic vector range exceeds the hardware table",
            ));
        }
        let route_target = PchPicRouteTarget::from_platform(info).ok_or(PnpError::malformed(
            PnpResourceKind::IrqDomain,
            "invalid pch-pic route target",
        ))?;
        let pic = Arc::new(PchPic::new(
            controller,
            parent_controller,
            base_vector,
            (self.device_mmio_to_virt)(phys),
            route_target,
        ));
        pic.reset();
        dev.reserve_owned_resources(1)?;
        let domain = irq::register_irq_domain(
            controller,
            Arc::new(PchPicDomain {
                pic: Arc::clone(&pic),
            }),
        )
        .map_err(map_irq_error)?;
        if let Err(err) = dev.own_resource(irq::irq_domain_pnp_resource(
            domain,
            "loongson-pch-pic-domain",
        )) {
            let _ = irq::unregister_irq_domain(domain);
            return Err(err);
        }
        dev.set_driver_data(Arc::new(PchPicBinding { pic }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<PchPicBinding>()
        {
            // 移除控制器时先把所有 source 重新屏蔽并清 pending，再拆掉父级
            // handler/domain。这样即使设备侧仍有旧电平或残留边沿，也不会在
            // 注销过程中继续向上游控制器冒泡。
            binding.pic.reset();
            binding.pic.unregister_parent_handlers();
        }
    }
}

struct PchMsi {
    parent_controller: u32,
    message_addr: u64,
    base_vector: u32,
    vector_count: usize,
    allocated: Spinlock<Vec<bool>>,
}

impl PchMsi {
    fn new(
        parent_controller: u32,
        message_addr: u64,
        base_vector: u32,
        vector_count: usize,
        allocated: Vec<bool>,
    ) -> Self {
        Self {
            parent_controller,
            message_addr,
            base_vector,
            vector_count,
            allocated: Spinlock::new(allocated),
        }
    }

    fn alloc_slot(&self, requester: u32) -> Option<u32> {
        if self.vector_count == 0 {
            return None;
        }
        let start = requester as usize % self.vector_count;
        let mut allocated = self.allocated.lock();
        for offset in 0..self.vector_count {
            let index = (start + offset) % self.vector_count;
            if !allocated[index] {
                allocated[index] = true;
                return u32::try_from(index).ok();
            }
        }
        None
    }

    fn free_slot(&self, hwirq: u32) {
        let index = hwirq as usize;
        if index >= self.vector_count {
            return;
        }
        self.allocated.lock()[index] = false;
    }
}

impl MsiController for PchMsi {
    fn allocate_vector(&self, requester: u32) -> Option<MsiVector> {
        let slot = self.alloc_slot(requester)?;
        let Some(vector) = self.base_vector.checked_add(slot) else {
            self.free_slot(slot);
            return None;
        };
        let Some(line) = irq::translate_firmware_irq(Some(self.parent_controller), &[vector])
        else {
            self.free_slot(slot);
            return None;
        };
        Some(MsiVector {
            hwirq: slot,
            line,
            message: MsiMessage {
                address: self.message_addr,
                data: vector,
            },
        })
    }

    fn free_vector(&self, hwirq: u32) {
        self.free_slot(hwirq);
    }
}

pub struct PchMsiDriver;

impl PchMsiDriver {
    const fn new() -> Self {
        Self
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.properties.interrupt_controller && info.has_id(COMPAT_LOONGSON_PCH_MSI)
    }
}

impl PnpDriver for PchMsiDriver {
    fn name(&self) -> &'static str {
        "platform-loongson-pch-msi"
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
        let info = platform_info(dev)?;
        let controller = info.properties.fw_phandle.ok_or(PnpError::missing(
            PnpResourceKind::MsiController,
            "pch-msi phandle missing",
        ))?;
        let parent_controller = info
            .properties
            .fw_interrupt_parent
            .ok_or(PnpError::missing(
                PnpResourceKind::IrqDomain,
                "pch-msi interrupt-parent missing",
            ))?;
        let (phys, _size) = info.first_mmio().ok_or(PnpError::missing(
            PnpResourceKind::Mmio,
            "pch-msi reg missing",
        ))?;
        let base_vector = info
            .u32_property(PCH_MSI_PROP_BASE_VECTOR)
            .ok_or(PnpError::missing(
                PnpResourceKind::Msi,
                "msi-base-vec missing",
            ))?;
        let vector_count = info
            .u32_property(PCH_MSI_PROP_NUM_VECS)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(PnpError::missing(
                PnpResourceKind::Msi,
                "msi-num-vecs missing",
            ))?;
        if vector_count == 0 {
            return Err(PnpError::malformed(
                PnpResourceKind::Msi,
                "zero MSI vectors",
            ));
        }
        let mut allocated = Vec::new();
        allocated
            .try_reserve(vector_count)
            .map_err(|_| PnpError::OutOfMemory)?;
        allocated.resize(vector_count, false);

        let msi = Arc::new(PchMsi::new(
            parent_controller,
            phys as u64,
            base_vector,
            vector_count,
            allocated,
        ));
        dev.reserve_owned_resources(1)?;
        let handle = msi::register_msi_controller(controller, msi).map_err(map_msi_error)?;
        if let Err(err) = dev.own_resource(msi::controller_pnp_resource(
            handle,
            "loongson-pch-msi-controller",
        )) {
            let _ = msi::unregister_msi_controller(handle);
            return Err(err);
        }
        Ok(())
    }

    fn remove(&self, _dev: &Arc<PnpDevice>) {}
}

fn platform_info(dev: &Arc<PnpDevice>) -> Result<&PlatformDeviceInfo, PnpError> {
    dev.info
        .as_any()
        .downcast_ref::<PlatformDeviceInfo>()
        .ok_or(PnpError::InvalidState)
}

fn first_irq_dependency(info: &PlatformDeviceInfo) -> PnpDependency {
    info.irq_resources()
        .find_map(|irq| irq.controller())
        .map(PnpDependency::IrqController)
        .unwrap_or(PnpDependency::DefaultIrqDomain)
}

fn iocsr_read64(offset: usize) -> Result<u64, PnpError> {
    irq::iocsr_read64(offset).ok_or(PnpError::hardware_failure("iocsr read64 failed"))
}

fn iocsr_write32(offset: usize, value: u32) -> Result<(), PnpError> {
    irq::iocsr_write32(offset, value)
        .then_some(())
        .ok_or(PnpError::hardware_failure("iocsr write32 failed"))
}

fn iocsr_write64(offset: usize, value: u64) -> Result<(), PnpError> {
    irq::iocsr_write64(offset, value)
        .then_some(())
        .ok_or(PnpError::hardware_failure("iocsr write64 failed"))
}

fn map_irq_error(err: IrqError) -> PnpError {
    match err {
        IrqError::OutOfMemory => PnpError::OutOfMemory,
        IrqError::AlreadyRegistered => {
            PnpError::registration_failed(PnpResourceKind::Irq, "irq already registered")
        }
        IrqError::NotFound => {
            PnpError::registration_failed(PnpResourceKind::Irq, "irq line not found")
        }
    }
}

fn map_msi_error(err: MsiError) -> PnpError {
    match err {
        MsiError::OutOfMemory => PnpError::OutOfMemory,
        MsiError::AlreadyRegistered => PnpError::registration_failed(
            PnpResourceKind::MsiController,
            "msi controller already registered",
        ),
        MsiError::NotFound => {
            PnpError::registration_failed(PnpResourceKind::MsiController, "msi controller missing")
        }
        MsiError::AllocationFailed => {
            PnpError::registration_failed(PnpResourceKind::Msi, "msi allocation failed")
        }
        MsiError::Busy => PnpError::InvalidState,
    }
}

struct LoongsonCpuIrqFactory;

impl DriverFactory for LoongsonCpuIrqFactory {
    fn name(&self) -> &'static str {
        "platform-loongson-cpuic"
    }

    fn create(&self, _ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(LoongsonCpuIrqDriver::new()))
    }
}

struct EioIntcFactory;

impl DriverFactory for EioIntcFactory {
    fn name(&self) -> &'static str {
        "platform-loongson-eiointc"
    }

    fn create(&self, _ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(EioIntcDriver::new()))
    }
}

struct PchPicFactory;

impl DriverFactory for PchPicFactory {
    fn name(&self) -> &'static str {
        "platform-loongson-pch-pic"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(PchPicDriver::new(ctx.device_mmio_to_virt)))
    }
}

struct PchMsiFactory;

impl DriverFactory for PchMsiFactory {
    fn name(&self) -> &'static str {
        "platform-loongson-pch-msi"
    }

    fn create(&self, _ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(PchMsiDriver::new()))
    }
}

pub fn register_builtin_driver() -> Result<[DriverHandle; 4], PnpError> {
    let cpu_irq = register_driver_factory(Arc::new(LoongsonCpuIrqFactory))?;
    let eiointc = match register_driver_factory(Arc::new(EioIntcFactory)) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = unregister_driver(cpu_irq);
            return Err(error);
        }
    };
    let pch_pic = match register_driver_factory(Arc::new(PchPicFactory)) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = unregister_driver(eiointc);
            let _ = unregister_driver(cpu_irq);
            return Err(error);
        }
    };
    match register_driver_factory(Arc::new(PchMsiFactory)) {
        Ok(pch_msi) => Ok([cpu_irq, eiointc, pch_pic, pch_msi]),
        Err(error) => {
            let _ = unregister_driver(pch_pic);
            let _ = unregister_driver(eiointc);
            let _ = unregister_driver(cpu_irq);
            Err(error)
        }
    }
}

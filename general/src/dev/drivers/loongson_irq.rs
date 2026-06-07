//! Loongson 固件中断控制器 platform 驱动。
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

use crate::dev::irq::{
    self, IrqDomain, IrqDomainHandle, IrqError, IrqHandle, IrqHandler, IrqLine, IrqStatus,
};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    register_driver_factory,
};

const COMPAT_LOONGSON_CPUIC: &str = "loongson,cpu-interrupt-controller";
const COMPAT_LOONGSON_EIOINTC: &str = "loongson,ls2k2000-eiointc";
const COMPAT_LOONGSON_EIOINTC_GENERIC: &str = "loongson,eiointc-1.0";
const COMPAT_LOONGSON_PCH_PIC: &str = "loongson,pch-pic-1.0";

const LOONGARCH_CPU_HWI_BASE: u32 = 2;
const LOONGARCH_CPU_HWI_COUNT: u32 = 6;

const LOONGARCH_IOCSR_MISC_FUNC: usize = 0x420;
const IOCSR_MISC_FUNC_EXT_IOI_EN: u64 = 1u64 << 48;

const EIOINTC_VECTOR_COUNT: u32 = 256;
const EIOINTC_VECTOR_BITS_PER_REG: u32 = 32;
const EIOINTC_VECTOR_BITS_PER_ISR: u32 = 64;
const EIOINTC_REG_NODEMAP: usize = 0x14a0;
const EIOINTC_REG_IPMAP: usize = 0x14c0;
const EIOINTC_REG_ENABLE: usize = 0x1600;
const EIOINTC_REG_BOUNCE: usize = 0x1680;
const EIOINTC_REG_ISR: usize = 0x1800;
const EIOINTC_REG_ROUTE: usize = 0x1c00;

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
const PCH_PIC_ROUTE_HT0_LO: u8 = 1;

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

struct LoongsonCpuIrqBinding {
    domain: IrqDomainHandle,
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
        let controller = info.properties.fw_phandle.ok_or(PnpError::ProbeFailed)?;
        let handle = irq::register_irq_domain(controller, Arc::new(LoongsonCpuIrqDomain))
            .map_err(map_irq_error)?;
        dev.set_driver_data(Arc::new(LoongsonCpuIrqBinding { domain: handle }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<LoongsonCpuIrqBinding>()
        {
            let _ = irq::unregister_irq_domain(binding.domain);
        }
    }
}

struct EioIntc {
    controller: u32,
    parent_hwi: usize,
}

impl EioIntc {
    fn new(controller: u32, parent_hwi: usize) -> Self {
        Self {
            controller,
            parent_hwi,
        }
    }

    fn initialize(&self) -> Result<(), PnpError> {
        let misc = iocsr_read64(LOONGARCH_IOCSR_MISC_FUNC)?;
        iocsr_write64(LOONGARCH_IOCSR_MISC_FUNC, misc | IOCSR_MISC_FUNC_EXT_IOI_EN)?;
        self.route_vectors_to_boot_cpu()?;
        self.set_all_vectors_enabled(false)?;
        self.clear_pending()?;
        Ok(())
    }

    fn route_vectors_to_boot_cpu(&self) -> Result<(), PnpError> {
        if self.parent_hwi >= 4 {
            return Err(PnpError::ProbeFailed);
        }
        let ip_bit = 1u32 << self.parent_hwi;
        let ipmap = ip_bit | (ip_bit << 8) | (ip_bit << 16) | (ip_bit << 24);
        for reg in 0..EIOINTC_VECTOR_COUNT / EIOINTC_VECTOR_BITS_PER_REG / 4 {
            iocsr_write32(EIOINTC_REG_IPMAP + reg as usize * 4, ipmap)?;
        }

        for reg in 0..EIOINTC_VECTOR_COUNT / EIOINTC_VECTOR_BITS_PER_REG {
            let node_bit = 1u32 << (reg * 2);
            let nodemap = node_bit | (node_bit << 16);
            iocsr_write32(EIOINTC_REG_NODEMAP + reg as usize * 4, nodemap)?;
        }

        let boot_cpu_route = 1u32 | (1u32 << 8) | (1u32 << 16) | (1u32 << 24);
        for reg in 0..EIOINTC_VECTOR_COUNT / 4 {
            iocsr_write32(EIOINTC_REG_ROUTE + reg as usize * 4, boot_cpu_route)?;
        }
        Ok(())
    }

    fn set_all_vectors_enabled(&self, enabled: bool) -> Result<(), PnpError> {
        let value = if enabled { u32::MAX } else { 0 };
        for reg in 0..EIOINTC_VECTOR_COUNT / EIOINTC_VECTOR_BITS_PER_REG {
            let offset = reg as usize * 4;
            iocsr_write32(EIOINTC_REG_ENABLE + offset, value)?;
            iocsr_write32(EIOINTC_REG_BOUNCE + offset, 0)?;
        }
        Ok(())
    }

    fn clear_pending(&self) -> Result<(), PnpError> {
        for reg in 0..EIOINTC_VECTOR_COUNT / EIOINTC_VECTOR_BITS_PER_ISR {
            iocsr_write64(EIOINTC_REG_ISR + reg as usize * 8, u64::MAX)?;
        }
        Ok(())
    }

    fn set_vector_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        if hwirq >= EIOINTC_VECTOR_COUNT {
            return false;
        }
        let reg = hwirq / EIOINTC_VECTOR_BITS_PER_REG;
        let bit = hwirq % EIOINTC_VECTOR_BITS_PER_REG;
        let offset = EIOINTC_REG_ENABLE + reg as usize * 4;
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
            let offset = EIOINTC_REG_ISR + reg as usize * 8;
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

struct EioIntcBinding {
    domain: IrqDomainHandle,
    parent_irq: IrqHandle,
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
        let controller = info.properties.fw_phandle.ok_or(PnpError::ProbeFailed)?;
        let parent_line = info.first_irq_line().ok_or(PnpError::ProbeFailed)?;
        let IrqLine::Hardware(parent_hwi) = parent_line else {
            return Err(PnpError::ProbeFailed);
        };
        let intc = Arc::new(EioIntc::new(controller, parent_hwi));
        intc.initialize()?;
        let domain = irq::register_irq_domain(controller, intc.clone()).map_err(map_irq_error)?;
        let handler: Arc<dyn IrqHandler> = Arc::new(EioIntcIrqHandler {
            intc: Arc::clone(&intc),
        });
        let parent_irq = match irq::register_irq_handler(parent_line, handler) {
            Ok(handle) => handle,
            Err(err) => {
                let _ = irq::unregister_irq_domain(domain);
                return Err(map_irq_error(err));
            }
        };
        dev.set_driver_data(Arc::new(EioIntcBinding { domain, parent_irq }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<EioIntcBinding>()
        {
            let _ = irq::unregister_irq_handler(binding.parent_irq);
            let _ = irq::unregister_irq_domain(binding.domain);
        }
    }
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

struct PchPicInner {
    slots: [Option<PchPicMapping>; PCH_PIC_IRQ_COUNT_USIZE],
}

impl PchPicInner {
    const fn new() -> Self {
        Self {
            slots: [None; PCH_PIC_IRQ_COUNT_USIZE],
        }
    }
}

struct PchPic {
    controller: u32,
    parent_controller: u32,
    base_vector: u32,
    mmio_base: usize,
    inner: Spinlock<PchPicInner>,
}

impl PchPic {
    fn new(controller: u32, parent_controller: u32, base_vector: u32, mmio_base: usize) -> Self {
        Self {
            controller,
            parent_controller,
            base_vector,
            mmio_base,
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
            self.write8(PCH_PIC_REG_ROUTE + source as usize, PCH_PIC_ROUTE_HT0_LO);
            self.write8(PCH_PIC_REG_HTVEC + source as usize, source as u8);
        }
    }

    fn parent_line(&self, slot: u32) -> Option<IrqLine> {
        let vector = self.base_vector.checked_add(slot)?;
        irq::translate_firmware_irq(Some(self.parent_controller), &[vector])
    }

    fn translate_source(&self, cells: &[u32]) -> Option<IrqLine> {
        let (source, irq_type) = match cells {
            [source, irq_type] => (*source, *irq_type),
            [source] => (*source, IRQ_TYPE_LEVEL_HIGH),
            _ => return None,
        };
        if source >= PCH_PIC_IRQ_COUNT {
            return None;
        }
        let mut inner = self.inner.lock();
        if let Some((slot, _)) = inner
            .slots
            .iter()
            .enumerate()
            .find(|(_, mapping)| mapping.is_some_and(|mapping| mapping.source == source))
        {
            return Some(IrqLine::Controller {
                controller: self.controller,
                hwirq: slot as u32,
            });
        }
        let slot = inner.slots.iter().position(Option::is_none)? as u32;
        let vector = self.base_vector.checked_add(slot)?;
        if vector > u8::MAX as u32 {
            return None;
        }
        let mapping = PchPicMapping { source, irq_type };
        self.program_source(slot, mapping)?;
        inner.slots[slot as usize] = Some(mapping);
        Some(IrqLine::Controller {
            controller: self.controller,
            hwirq: slot,
        })
    }

    fn program_source(&self, slot: u32, mapping: PchPicMapping) -> Option<()> {
        let vector = self.base_vector.checked_add(slot)?;
        self.write8(
            PCH_PIC_REG_HTVEC + mapping.source as usize,
            u8::try_from(vector).ok()?,
        );
        self.write8(
            PCH_PIC_REG_ROUTE + mapping.source as usize,
            PCH_PIC_ROUTE_HT0_LO,
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
        self.inner.lock().slots[slot as usize]
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
        unsafe { core::ptr::read_volatile((self.mmio_base + offset) as *const u32) }
    }

    fn write32(&self, offset: usize, value: u32) {
        unsafe { core::ptr::write_volatile((self.mmio_base + offset) as *mut u32, value) };
    }

    fn write8(&self, offset: usize, value: u8) {
        unsafe { core::ptr::write_volatile((self.mmio_base + offset) as *mut u8, value) };
    }
}

impl IrqDomain for PchPic {
    fn translate(&self, cells: &[u32]) -> Option<IrqLine> {
        self.translate_source(cells)
    }

    fn set_line_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        self.set_slot_enabled(hwirq, enabled)
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
    domain: IrqDomainHandle,
    parent_irqs: Vec<IrqHandle>,
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

    fn register_parent_handlers(pic: &Arc<PchPic>) -> Result<Vec<IrqHandle>, PnpError> {
        let mut handles = Vec::new();
        handles
            .try_reserve(PCH_PIC_IRQ_COUNT_USIZE)
            .map_err(|_| PnpError::OutOfMemory)?;
        for slot in 0..PCH_PIC_IRQ_COUNT {
            let parent_line = match pic.parent_line(slot) {
                Some(line) => line,
                None => {
                    for handle in handles {
                        let _ = irq::unregister_irq_handler(handle);
                    }
                    return Err(PnpError::ProbeFailed);
                }
            };
            let handler: Arc<dyn IrqHandler> = Arc::new(PchPicCascadeHandler {
                pic: Arc::clone(pic),
                slot,
            });
            match irq::register_irq_handler(parent_line, handler) {
                Ok(handle) => handles.push(handle),
                Err(err) => {
                    for handle in handles {
                        let _ = irq::unregister_irq_handler(handle);
                    }
                    return Err(map_irq_error(err));
                }
            }
        }
        Ok(handles)
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
        let controller = info.properties.fw_phandle.ok_or(PnpError::ProbeFailed)?;
        let parent_controller = info
            .properties
            .fw_interrupt_parent
            .ok_or(PnpError::ProbeFailed)?;
        let Some((phys, _size)) = info.first_mmio() else {
            return Err(PnpError::ProbeFailed);
        };
        let base_vector = info.u32_property("loongson,pic-base-vec").unwrap_or(0);
        let pic = Arc::new(PchPic::new(
            controller,
            parent_controller,
            base_vector,
            (self.device_mmio_to_virt)(phys),
        ));
        pic.reset();
        let domain = irq::register_irq_domain(controller, pic.clone()).map_err(map_irq_error)?;
        let parent_irqs = match Self::register_parent_handlers(&pic) {
            Ok(handles) => handles,
            Err(err) => {
                let _ = irq::unregister_irq_domain(domain);
                return Err(err);
            }
        };
        dev.set_driver_data(Arc::new(PchPicBinding {
            domain,
            parent_irqs,
        }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<PchPicBinding>()
        {
            for handle in binding.parent_irqs.iter().copied() {
                let _ = irq::unregister_irq_handler(handle);
            }
            let _ = irq::unregister_irq_domain(binding.domain);
        }
    }
}

fn platform_info(dev: &Arc<PnpDevice>) -> Result<&PlatformDeviceInfo, PnpError> {
    dev.info
        .as_any()
        .downcast_ref::<PlatformDeviceInfo>()
        .ok_or(PnpError::InvalidState)
}

fn iocsr_read64(offset: usize) -> Result<u64, PnpError> {
    irq::iocsr_read64(offset).ok_or(PnpError::ProbeFailed)
}

fn iocsr_write32(offset: usize, value: u32) -> Result<(), PnpError> {
    irq::iocsr_write32(offset, value)
        .then_some(())
        .ok_or(PnpError::ProbeFailed)
}

fn iocsr_write64(offset: usize, value: u64) -> Result<(), PnpError> {
    irq::iocsr_write64(offset, value)
        .then_some(())
        .ok_or(PnpError::ProbeFailed)
}

fn map_irq_error(err: IrqError) -> PnpError {
    match err {
        IrqError::OutOfMemory => PnpError::OutOfMemory,
        IrqError::AlreadyRegistered => PnpError::NameConflict,
        IrqError::NotFound => PnpError::ProbeFailed,
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

pub fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(LoongsonCpuIrqFactory))?;
    register_driver_factory(Arc::new(EioIntcFactory))?;
    register_driver_factory(Arc::new(PchPicFactory)).map(|_| ())
}

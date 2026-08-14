//! LS2K1000 USB 主机控制器 PnP 驱动。
//!
//! 匹配工厂 DTB 的三个 USB 节点：otg@40000000（loongson,loongson2-dwc2，
//! dr_mode="host"）、ehci@40060000（loongson,ls2k-ehci）、
//! ohci@40070000（loongson,ls2k-ohci）。每个控制器绑定后创建
//! [`UsbBus`]（HCD 实现 + 枚举），上电端口并做首次扫描；IRQ（端口变化）
//! 触发后续扫描。EHCI 端口复位后把 FS/LS 设备移交给伴生 OHCI。

use alloc::sync::{Arc, Weak};

use general::dev::irq::{self, IrqHandle, IrqHandler, IrqLine, IrqStatus};
use general::dev::platform::{PlatformDeviceInfo, PlatformIrqRegistrationError};
use general::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};

use crate::core::{UsbBus, UsbHcd};
use crate::dwc2::Dwc2Hcd;
use crate::ehci::EhciHcd;
use crate::ohci::OhciHcd;
use crate::regs::*;

const COMPAT_DWC2: &str = "loongson,loongson2-dwc2";
const COMPAT_EHCI: &str = "loongson,ls2k-ehci";
const COMPAT_OHCI: &str = "loongson,ls2k-ohci";

/// IRQ 端口变化 → 清除变化位并触发总线扫描。
struct UsbScanHandler {
    bus: Weak<UsbBus>,
    kind: ScanKind,
    base: usize,
}

#[derive(Clone, Copy, Debug)]
enum ScanKind {
    Ehci,
    Ohci,
    Dwc2,
}

impl UsbScanHandler {
    fn acknowledge(&self) {
        match self.kind {
            ScanKind::Ehci => {
                // 清除全部端口连接/使能变化位（PORTSC 变化位写 1 清除）。
                // Safety: base 由 platform probe 映射，窗口已校验。
                for port in 0..8 {
                    let reg = self.base + EHCI_PORTSC + port * 4;
                    let value = unsafe { core::ptr::read_volatile(reg as *const u32) };
                    if value & (EHCI_PORTSC_CSC | EHCI_PORTSC_OCC | (1 << 16) | (1 << 17) | (1 << 19) | (1 << 20)) != 0 {
                        unsafe {
                            core::ptr::write_volatile(reg as *mut u32, value)
                        };
                    }
                }
            }
            ScanKind::Ohci => {
                // Safety: 同 Ehci。
                let status = unsafe {
                    core::ptr::read_volatile((self.base + OHCI_HcInterruptStatus) as *const u32)
                };
                if status & OHCI_INTR_RHSC != 0 {
                    unsafe {
                        core::ptr::write_volatile(
                            (self.base + OHCI_HcInterruptStatus) as *mut u32,
                            OHCI_INTR_RHSC,
                        )
                    };
                }
            }
            ScanKind::Dwc2 => {
                // Safety: 同 Ehci。
                let hprt = unsafe { core::ptr::read_volatile((self.base + DWC2_HPRT) as *const u32) };
                if hprt & (DWC2_HPRT_PRTCONNDET | DWC2_HPRT_PRTENCHNG | DWC2_HPRT_PRTOVRCURRCHNG) != 0 {
                    unsafe {
                        core::ptr::write_volatile(
                            (self.base + DWC2_HPRT) as *mut u32,
                            hprt
                                | DWC2_HPRT_PRTCONNDET
                                | DWC2_HPRT_PRTENCHNG
                                | DWC2_HPRT_PRTOVRCURRCHNG,
                        )
                    };
                }
            }
        }
    }
}

impl IrqHandler for UsbScanHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        self.acknowledge();
        if let Some(bus) = self.bus.upgrade() {
            bus.scan_ports();
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }
}

struct UsbBinding {
    bus: Arc<UsbBus>,
    irq_handle: Option<IrqHandle>,
}

pub struct Ls2kUsbDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl Ls2kUsbDriver {
    pub const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self { device_mmio_to_virt }
    }

    fn kind_of(info: &PlatformDeviceInfo) -> Option<ScanKind> {
        if info.has_id(COMPAT_DWC2) {
            Some(ScanKind::Dwc2)
        } else if info.has_id(COMPAT_EHCI) {
            Some(ScanKind::Ehci)
        } else if info.has_id(COMPAT_OHCI) {
            Some(ScanKind::Ohci)
        } else {
            None
        }
    }

    fn register_scan_irq(
        &self,
        bus: &Arc<UsbBus>,
        kind: ScanKind,
        base: usize,
        info: &PlatformDeviceInfo,
    ) -> Result<Option<IrqHandle>, PnpError> {
        let handler: Arc<dyn IrqHandler> = Arc::new(UsbScanHandler {
            bus: Arc::downgrade(bus),
            kind,
            base,
        });
        match info.register_first_irq_handler(handler) {
            Ok(handle) => Ok(Some(handle)),
            Err(PlatformIrqRegistrationError::NoResource) => Ok(None),
            Err(PlatformIrqRegistrationError::Unresolved) => {
                Ok(None) // 端口变化走首次扫描 + 未来轮询，IRQ 缺失不致命
            }
            Err(PlatformIrqRegistrationError::RegistrationFailed { .. }) => Ok(None),
        }
    }

    fn probe_hcd(
        &self,
        dev: &Arc<PnpDevice>,
        info: &PlatformDeviceInfo,
        kind: ScanKind,
        bus_id: u8,
    ) -> Result<(), PnpError> {
        let Some((phys, size)) = info.first_mmio() else {
            return Err(PnpError::missing(PnpResourceKind::Mmio, "usb reg missing"));
        };
        let base = (self.device_mmio_to_virt)(phys);
        let context = info.dma_context();
        let hcd: Arc<dyn UsbHcd> = match kind {
            ScanKind::Ehci => {
                if size < 0x100 {
                    return Err(PnpError::malformed(PnpResourceKind::Mmio, "ehci window too small"));
                }
                // EHCI 能力寄存器在基址，操作寄存器在 CAPLENGTH 偏移。
                // Safety: 窗口由 platform probe 校验。
                let caplength =
                    unsafe { core::ptr::read_volatile(base as *const u32) } & 0xff;
                Arc::new(
                    EhciHcd::new(base + caplength as usize, base, context)
                        .map_err(|_| PnpError::hardware_failure("ehci init failed"))?,
                )
            }
            ScanKind::Ohci => Arc::new(
                OhciHcd::new(base, context).map_err(|_| PnpError::hardware_failure("ohci init failed"))?,
            ),
            ScanKind::Dwc2 => Arc::new(
                Dwc2Hcd::new(base, context).map_err(|_| PnpError::hardware_failure("dwc2 init failed"))?,
            ),
        };
        let bus = UsbBus::new(bus_id, hcd);
        for port in 0..bus.hcd().port_count() {
            bus.hcd()
                .port_power_on(port)
                .map_err(|_| PnpError::hardware_failure("usb port power failed"))?;
        }
        let irq_handle = self.register_scan_irq(&bus, kind, base, info)?;
        // 首次扫描（枚举可能耗时，只记录结果）。
        bus.scan_ports();
        log::printk!(
            "[ls2k-usb] bound {} phys={:#x} size={:#x} kind={:?} ports={}",
            dev.id,
            phys,
            size,
            kind,
            bus.hcd().port_count(),
        );
        dev.set_driver_data(Arc::new(UsbBinding { bus, irq_handle }));
        Ok(())
    }
}

impl PnpDriver for Ls2kUsbDriver {
    fn name(&self) -> &'static str {
        "platform-ls2k-usb"
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
            .is_some_and(|info| Self::kind_of(info).is_some())
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let Some(kind) = Self::kind_of(info) else {
            return Err(PnpError::NoDriver);
        };
        let bus_id = info.u32_property("bus_id").unwrap_or(0) as u8;
        self.probe_hcd(dev, info, kind, bus_id)
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<UsbBinding>()
            && let Some(handle) = binding.irq_handle
        {
            let _ = irq::unregister_irq_handler(handle);
        }
        log::printk!("[ls2k-usb] removed {}", dev.id);
    }
}

struct Ls2kUsbFactory;

impl DriverFactory for Ls2kUsbFactory {
    fn name(&self) -> &'static str {
        "platform-ls2k-usb"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Ls2kUsbDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(Ls2kUsbFactory))
}

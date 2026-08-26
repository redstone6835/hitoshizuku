//! LS2K1000 USB 主机控制器 PnP 驱动。
//!
//! 匹配工厂 DTB 的三个 USB 节点：otg@40000000（loongson,loongson2-dwc2，
//! dr_mode="host"）、ehci@40060000（loongson,ls2k-ehci）、
//! ohci@40070000（loongson,ls2k-ohci）。每个控制器绑定后创建
//! [`UsbBus`]（HCD 实现 + 枚举），上电端口并做首次扫描。当前三个 HCD 的传输
//! 都采用同步轮询；热插拔扫描必须由可睡眠 worker 承担，不能放进硬中断上下文。
//! EHCI 端口复位后把 FS/LS 设备移交给伴生 OHCI。

use alloc::sync::Arc;

use general::dev::dt_provider::{self, DtbProviderError, DtbResourceRequest};
use general::dev::platform::PlatformDeviceInfo;
use general::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};

use crate::core::{UsbBus, UsbHcd};
use crate::dwc2::Dwc2Hcd;
use crate::ehci::EhciHcd;
use crate::ohci::OhciHcd;
const COMPAT_DWC2: &str = "loongson,loongson2-dwc2";
const COMPAT_EHCI: &str = "loongson,ls2k-ehci";
const COMPAT_OHCI: &str = "loongson,ls2k-ohci";
const PROP_PINCTRL_DEFAULT: &str = "pinctrl-0";
const PROP_GPIOS: &str = "gpios";

// 2K1000 板级代码在任何 USB 控制器启动前都会关闭硬件预取。该位不属于
// EHCI 标准寄存器窗口，不能由通用 EHCI 初始化流程代替。
const LS2K1000_GENERAL_CFG1_PHYS: usize = 0x1fe0_0428;
const LS2K1000_USB_PREFETCH: u32 = 1 << 19;

#[derive(Clone, Copy, Debug)]
enum ScanKind {
    Ehci,
    Ohci,
    Dwc2,
}

struct UsbBinding {
    bus: Arc<UsbBus>,
}

pub struct Ls2kUsbDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl Ls2kUsbDriver {
    pub const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
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

    fn acquire_optional_resource(
        &self,
        dev: &Arc<PnpDevice>,
        info: &PlatformDeviceInfo,
        property: &str,
        request: DtbResourceRequest<'_>,
        label: &'static str,
    ) -> Result<(), PnpError> {
        let lease = match info.acquire_dtb_resource_at(property, 0) {
            Ok(lease) => lease,
            Err(DtbProviderError::Disabled | DtbProviderError::Invalid) => return Ok(()),
            Err(error) => return Err(error.into_pnp_error()),
        };
        lease
            .control(request)
            .map_err(DtbProviderError::into_pnp_error)?;
        dev.own_boxed_resource(dt_provider::lease_pnp_resource_boxed(lease, label))
    }

    /// 应用 2K1000-DP 板级 EHCI 引脚复用，并拉高外部 VBUS 使能。
    ///
    /// 两项资源均由 DT 描述；其它板型没有对应属性时保持现有固件状态。
    fn prepare_ehci_board_resources(
        &self,
        dev: &Arc<PnpDevice>,
        info: &PlatformDeviceInfo,
    ) -> Result<(), PnpError> {
        dev.reserve_owned_resources(2)?;
        self.acquire_optional_resource(
            dev,
            info,
            PROP_PINCTRL_DEFAULT,
            DtbResourceRequest::Enable,
            "ls2k-ehci-pinctrl",
        )?;
        self.acquire_optional_resource(
            dev,
            info,
            PROP_GPIOS,
            DtbResourceRequest::Assert,
            "ls2k-ehci-vbus",
        )
    }

    /// 应用厂商 2K1000 U-Boot `dev_fixup()` 中的 USB 预取修正。
    fn disable_ehci_prefetch(&self, dev: &PnpDevice) -> Result<(), PnpError> {
        let register = (self.device_mmio_to_virt)(LS2K1000_GENERAL_CFG1_PHYS);
        hal::memory::device_io_barrier();
        // Safety: `register` 是 2K1000 GENERAL_CFG1 的非缓存设备映射，寄存器
        // 宽度为 32 位；probe 在单核启动阶段以读改写方式保留其它功能位。
        let before = unsafe { core::ptr::read_volatile(register as *const u32) };
        let expected = before & !LS2K1000_USB_PREFETCH;
        if expected != before {
            // Safety: 地址与访问宽度同上，写入值仅清除厂商指定的 bit 19。
            unsafe { core::ptr::write_volatile(register as *mut u32, expected) };
        }
        hal::memory::device_io_barrier();
        // Safety: 回读同一有效 MMIO 寄存器，用于确认 posted write 已经生效。
        let after = unsafe { core::ptr::read_volatile(register as *const u32) };
        hal::memory::device_io_barrier();
        log::printk!(
            "[ls2k-usb] EHCI platform fixup for {} GENERAL_CFG1={:#010x}->{:#010x}",
            dev.id,
            before,
            after
        );
        if after & LS2K1000_USB_PREFETCH != 0 {
            return Err(PnpError::hardware_failure(
                "ls2k1000 usb prefetch disable did not latch",
            ));
        }
        Ok(())
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
                self.prepare_ehci_board_resources(dev, info)?;
                self.disable_ehci_prefetch(dev)?;
                if size < 0x100 {
                    return Err(PnpError::malformed(
                        PnpResourceKind::Mmio,
                        "ehci window too small",
                    ));
                }
                // EHCI 能力寄存器在基址，操作寄存器在 CAPLENGTH 偏移。
                // Safety: 窗口由 platform probe 校验。
                let capability = unsafe { core::ptr::read_volatile(base as *const u32) };
                let caplength = capability & 0xff;
                if capability == u32::MAX || caplength < 0x10 || caplength as usize >= size {
                    log::printk!(
                        "[ls2k-usb] EHCI capability invalid for {} phys={:#x}: {:#010x}; check BAR0 and PCI memory decode",
                        dev.id,
                        phys,
                        capability
                    );
                    return Err(PnpError::hardware_failure("ehci capability header invalid"));
                }
                let op_base = base + caplength as usize;
                let hcd: Arc<dyn UsbHcd> =
                    Arc::new(EhciHcd::new(op_base, base, context).map_err(|error| {
                        log::printk!(
                            "[ls2k-usb] EHCI init failed for {} phys={:#x}: {}",
                            dev.id,
                            phys,
                            error
                        );
                        PnpError::hardware_failure("ehci init failed")
                    })?);
                hcd
            }
            ScanKind::Ohci => Arc::new(
                OhciHcd::new(base, context)
                    .map_err(|_| PnpError::hardware_failure("ohci init failed"))?,
            ),
            ScanKind::Dwc2 => Arc::new(
                Dwc2Hcd::new(base, context)
                    .map_err(|_| PnpError::hardware_failure("dwc2 init failed"))?,
            ),
        };
        let bus = UsbBus::new(bus_id, hcd);
        for port in 0..bus.hcd().port_count() {
            bus.hcd()
                .port_power_on(port)
                .map_err(|_| PnpError::hardware_failure("usb port power failed"))?;
        }
        // 枚举包含端口复位、同步传输和 PnP probe，只能在进程上下文执行。
        // 当前 HCD 使用轮询完成，不注册 IRQ；运行期热插拔后续由专用 worker 接入。
        bus.scan_ports();
        log::printk!(
            "[ls2k-usb] bound {} phys={:#x} size={:#x} kind={:?} ports={}",
            dev.id,
            phys,
            size,
            kind,
            bus.hcd().port_count(),
        );
        dev.set_driver_data(Arc::new(UsbBinding { bus }));
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
            && let Err(error) = binding.bus.hcd().shutdown()
        {
            log::error!(
                "[ls2k-usb] cannot stop {} safely: {}; retaining DMA objects",
                dev.id,
                error,
            );
            // 控制器可能仍持有 DMA 地址。此时泄漏整个 binding 比释放后形成
            // DMA use-after-free 更安全，后续板级复位才能重新接管设备。
            core::mem::forget(binding);
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

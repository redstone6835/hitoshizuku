//! LS2K1000 Pinctrl 与 GPIO 的 Device Tree provider 驱动。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::str;

use vfs::sync::Spinlock;

use crate::dev::dt_provider::{
    self, DtbProvider, DtbProviderError, DtbProviderKey, DtbProviderKind, DtbResource,
    DtbResourceReply, DtbResourceRequest,
};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory, unregister_driver,
};
use crate::firmware::dtb::{DtbDeviceProperty, DtbNodeInfo};
use crate::gpio::{
    GpioError, GpioIrqMap, GpioLayout, GpioLineAllocator, GpioOffsets, GpioSpecifier,
    RegisterUpdate,
};
use crate::pinctrl::{
    MuxUpdate, PinctrlError, PinctrlMmioLayout, decode_named_state, merge_updates,
};

const COMPAT_LS2K1000_PINCTRL: &str = "loongson,2k1000-pinctrl";
const COMPAT_LOONGSON3_GPIO: &str = "loongson,loongson3-gpio";
const PROP_GROUPS: &str = "groups";
const PROP_FUNCTION: &str = "function";
const PROP_GPIO_CELLS: &str = "#gpio-cells";
const PROP_GPIO_CONTROLLER: &str = "gpio-controller";
const PROP_NGPIOS: &str = "ngpios";
const PROP_CONF_OFFSET: &str = "conf_offset";
const PROP_OUT_OFFSET: &str = "out_offset";
const PROP_IN_OFFSET: &str = "in_offset";
const PROP_INT_OFFSET: &str = "int_offset";
const PROP_IN_START_BIT: &str = "in_start_bit";
const PROP_SUPPORT_IRQ: &str = "support_irq";
const PROVIDER_KIND: PnpResourceKind = PnpResourceKind::Other("dt-provider");

struct PinctrlHardware {
    layout: PinctrlMmioLayout,
    lock: Spinlock<()>,
}

impl PinctrlHardware {
    fn apply(&self, updates: &[MuxUpdate]) -> Result<(), DtbProviderError> {
        let _guard = self.lock.lock();
        for &update in updates {
            let address = self
                .layout
                .address(update)
                .map_err(|_| DtbProviderError::HardwareFailure)?;
            // Safety: probe 已验证 32 位对齐、完整 MMIO 窗口和每个更新的范围；
            // 锁保证同一控制器的读改写不会交错。
            unsafe {
                let current = read_volatile(address as *const u32);
                write_volatile(address as *mut u32, update.apply(current));
            }
        }
        Ok(())
    }
}

struct PinctrlStateResource {
    hardware: Arc<PinctrlHardware>,
    updates: Arc<[MuxUpdate]>,
}

impl DtbResource for PinctrlStateResource {
    fn control(
        &self,
        request: DtbResourceRequest<'_>,
    ) -> Result<DtbResourceReply, DtbProviderError> {
        match request {
            DtbResourceRequest::Enable => {
                self.hardware.apply(&self.updates)?;
                Ok(DtbResourceReply::Done)
            }
            // 引脚复用状态在 consumer 释放后保持不变，和固件默认状态语义一致。
            DtbResourceRequest::Disable => Ok(DtbResourceReply::Done),
            _ => Err(DtbProviderError::UnsupportedOperation),
        }
    }
}

struct PinctrlStateProvider {
    hardware: Arc<PinctrlHardware>,
    updates: Arc<[MuxUpdate]>,
}

impl DtbProvider for PinctrlStateProvider {
    fn acquire(&self, specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError> {
        if !specifier.is_empty() {
            return Err(DtbProviderError::AcquireFailed);
        }
        Ok(Arc::new(PinctrlStateResource {
            hardware: Arc::clone(&self.hardware),
            updates: Arc::clone(&self.updates),
        }))
    }
}

struct GpioHardware {
    layout: GpioLayout,
    operation_lock: Spinlock<()>,
    allocator: Spinlock<GpioLineAllocator>,
    irq_map: Option<GpioIrqMap>,
}

impl GpioHardware {
    fn apply_updates(&self, updates: &[RegisterUpdate]) {
        let _guard = self.operation_lock.lock();
        for update in updates {
            // Safety: GpioLayout 已验证全部寄存器地址、对齐和位宽；锁保证同一
            // 64 位寄存器上的读改写不互相覆盖。
            unsafe {
                let current = read_volatile(update.address as *const u64);
                let value = (current & !update.clear_mask) | update.set_mask;
                write_volatile(update.address as *mut u64, value);
            }
        }
    }

    fn configure_input(&self, line: u32) -> Result<(), DtbProviderError> {
        let update = self.layout.input_update(line).map_err(gpio_control_error)?;
        self.apply_updates(core::slice::from_ref(&update));
        Ok(())
    }

    fn configure_output(&self, line: u32, high: bool) -> Result<(), DtbProviderError> {
        let updates = self
            .layout
            .output_sequence(line, high)
            .map_err(gpio_control_error)?;
        self.apply_updates(&updates);
        Ok(())
    }

    fn configure_interrupt(&self, line: u32, enabled: bool) -> Result<(), DtbProviderError> {
        self.irq_map
            .as_ref()
            .ok_or(DtbProviderError::UnsupportedOperation)?
            .source_for_line(line)
            .map_err(gpio_control_error)?;
        let update = self
            .layout
            .interrupt_update(line, enabled)
            .map_err(gpio_control_error)?;
        self.apply_updates(core::slice::from_ref(&update));
        Ok(())
    }

    fn read(&self, specifier: GpioSpecifier) -> Result<bool, DtbProviderError> {
        let line = self
            .layout
            .line(specifier.line)
            .map_err(gpio_control_error)?;
        let _guard = self.operation_lock.lock();
        // Safety: line 来自已验证布局，input_register 8 字节对齐且位于 MMIO 窗口内。
        let value = unsafe { read_volatile(line.input_register as *const u64) };
        Ok(specifier.logical_level(value & line.input_mask != 0))
    }
}

struct GpioResource {
    hardware: Arc<GpioHardware>,
    specifier: GpioSpecifier,
}

impl DtbResource for GpioResource {
    fn control(
        &self,
        request: DtbResourceRequest<'_>,
    ) -> Result<DtbResourceReply, DtbProviderError> {
        match request {
            DtbResourceRequest::Enable => {
                self.hardware.configure_input(self.specifier.line)?;
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::Disable => {
                if self.hardware.irq_map.is_some() {
                    self.hardware
                        .configure_interrupt(self.specifier.line, false)?;
                }
                self.hardware.configure_input(self.specifier.line)?;
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::Assert => {
                self.hardware
                    .configure_output(self.specifier.line, self.specifier.physical_level(true))?;
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::Deassert => {
                self.hardware
                    .configure_output(self.specifier.line, self.specifier.physical_level(false))?;
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::ReadValue => self
                .hardware
                .read(self.specifier)
                .map(|high| DtbResourceReply::Value(u64::from(high))),
            DtbResourceRequest::WriteValue(value) if value <= 1 => {
                self.hardware.configure_output(
                    self.specifier.line,
                    self.specifier.physical_level(value != 0),
                )?;
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::Configure([0]) => {
                self.hardware.configure_input(self.specifier.line)?;
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::Configure([1, value]) if *value <= 1 => {
                self.hardware.configure_output(
                    self.specifier.line,
                    self.specifier.physical_level(*value != 0),
                )?;
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::Configure([2, enabled]) if *enabled <= 1 => {
                self.hardware
                    .configure_interrupt(self.specifier.line, *enabled != 0)?;
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::WriteValue(_) | DtbResourceRequest::Configure(_) => {
                Err(DtbProviderError::Invalid)
            }
            _ => Err(DtbProviderError::UnsupportedOperation),
        }
    }
}

impl Drop for GpioResource {
    fn drop(&mut self) {
        if self.hardware.irq_map.is_some() {
            let _ = self
                .hardware
                .configure_interrupt(self.specifier.line, false);
        }
        let _ = self.hardware.configure_input(self.specifier.line);
        let _ = self.hardware.allocator.lock().release(self.specifier.line);
    }
}

struct GpioProvider {
    hardware: Arc<GpioHardware>,
}

impl DtbProvider for GpioProvider {
    fn acquire(&self, specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError> {
        let specifier = GpioSpecifier::decode(specifier, self.hardware.layout.ngpios())
            .map_err(gpio_acquire_error)?;
        self.hardware
            .allocator
            .lock()
            .acquire(specifier.line)
            .map_err(gpio_acquire_error)?;
        Ok(Arc::new(GpioResource {
            hardware: Arc::clone(&self.hardware),
            specifier,
        }))
    }
}

struct Ls2kPinctrlDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl PnpDriver for Ls2kPinctrlDriver {
    fn name(&self) -> &'static str {
        "platform-loongson-ls2k1000-pinctrl"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .is_some_and(|info| info.has_id(COMPAT_LS2K1000_PINCTRL))
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = platform_info(dev)?;
        let (phys, size) = exact_mmio(info, "LS2K1000 pinctrl")?;
        PinctrlMmioLayout::new(phys, size).map_err(pinctrl_layout_error)?;
        let layout = PinctrlMmioLayout::new((self.device_mmio_to_virt)(phys), size)
            .map_err(pinctrl_layout_error)?;
        let states = parse_pinctrl_states(info)?;
        if states.is_empty() {
            return Err(PnpError::missing(
                PROVIDER_KIND,
                "LS2K1000 pinctrl exposes no phandle states",
            ));
        }

        dev.reserve_owned_resources(states.len())?;
        let hardware = Arc::new(PinctrlHardware {
            layout,
            lock: Spinlock::new(()),
        });
        for (phandle, updates) in states {
            let handle = dt_provider::register(
                DtbProviderKey::new(DtbProviderKind::Pinctrl, phandle),
                Arc::new(PinctrlStateProvider {
                    hardware: Arc::clone(&hardware),
                    updates: Arc::from(updates),
                }),
            )
            .map_err(DtbProviderError::into_pnp_error)?;
            if let Err(error) = dev.own_resource(dt_provider::provider_pnp_resource(
                handle,
                "loongson-ls2k1000-pinctrl-state",
            )) {
                let _ = dt_provider::unregister(handle);
                return Err(error);
            }
        }
        log::printk!(
            "[loongson-pinctrl] bound {} phys={:#x} size={:#x}",
            dev.name,
            phys,
            size
        );
        Ok(())
    }

    fn remove(&self, _dev: &Arc<PnpDevice>) {}
}

struct LoongsonGpioDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl PnpDriver for LoongsonGpioDriver {
    fn name(&self) -> &'static str {
        "platform-loongson3-gpio"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .is_some_and(|info| info.has_id(COMPAT_LOONGSON3_GPIO))
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = platform_info(dev)?;
        validate_gpio_binding(info)?;
        let phandle = info.properties.fw_phandle.ok_or(PnpError::missing(
            PROVIDER_KIND,
            "Loongson GPIO controller is missing a phandle",
        ))?;
        let (phys, size) = exact_mmio(info, "Loongson GPIO")?;
        let offsets = GpioOffsets {
            direction: required_u32(info, PROP_CONF_OFFSET)? as usize,
            output: required_u32(info, PROP_OUT_OFFSET)? as usize,
            input: required_u32(info, PROP_IN_OFFSET)? as usize,
            interrupt: required_u32(info, PROP_INT_OFFSET)? as usize,
        };
        let ngpios = required_u32(info, PROP_NGPIOS)?;
        let input_start = required_u32(info, PROP_IN_START_BIT)?;
        GpioLayout::new(phys, size, ngpios, offsets, input_start).map_err(gpio_layout_error)?;
        let layout = GpioLayout::new(
            (self.device_mmio_to_virt)(phys),
            size,
            ngpios,
            offsets,
            input_start,
        )
        .map_err(gpio_layout_error)?;
        let irq_map = gpio_irq_map(info, ngpios)?;

        let hardware = Arc::new(GpioHardware {
            layout,
            operation_lock: Spinlock::new(()),
            allocator: Spinlock::new(GpioLineAllocator::new(ngpios).map_err(gpio_layout_error)?),
            irq_map,
        });
        if hardware.irq_map.is_some() {
            let update = hardware
                .layout
                .interrupt_update(0, false)
                .map_err(gpio_layout_error)?;
            hardware.apply_updates(&[RegisterUpdate {
                address: update.address,
                clear_mask: u64::MAX,
                set_mask: 0,
            }]);
        }

        dev.reserve_owned_resources(1)?;
        let handle = dt_provider::register(
            DtbProviderKey::new(DtbProviderKind::Gpio, phandle),
            Arc::new(GpioProvider { hardware }),
        )
        .map_err(DtbProviderError::into_pnp_error)?;
        if let Err(error) = dev.own_resource(dt_provider::provider_pnp_resource(
            handle,
            "loongson3-gpio-provider",
        )) {
            let _ = dt_provider::unregister(handle);
            return Err(error);
        }
        log::printk!(
            "[loongson-gpio] bound {} phandle={:#x} lines={} phys={:#x}",
            dev.name,
            phandle,
            ngpios,
            phys
        );
        Ok(())
    }

    fn remove(&self, _dev: &Arc<PnpDevice>) {}
}

fn parse_pinctrl_states(
    info: &PlatformDeviceInfo,
) -> Result<Vec<(u32, Box<[MuxUpdate]>)>, PnpError> {
    let controller_path = info.fw_path.as_deref().ok_or(PnpError::malformed(
        PROVIDER_KIND,
        "LS2K1000 pinctrl has no firmware path",
    ))?;
    let nodes = info.dtb_owned_nodes().ok_or(PnpError::missing(
        PROVIDER_KIND,
        "LS2K1000 pinctrl has no owned DT subtree",
    ))?;
    let mut states = Vec::new();
    for state in nodes.iter().filter(|node| {
        node.enabled
            && node.phandle.is_some()
            && node.parent_path.as_deref() == Some(controller_path)
    }) {
        let phandle = state.phandle.expect("filtered pinctrl state has a phandle");
        if states.iter().any(|(registered, _)| *registered == phandle) {
            return Err(PnpError::malformed(
                PROVIDER_KIND,
                "duplicate pinctrl state phandle",
            ));
        }
        let mut updates = Vec::new();
        let mut saw_mux = false;
        for mux in nodes
            .iter()
            .filter(|node| node.enabled && node.parent_path.as_deref() == Some(state.path.as_ref()))
        {
            let groups = string_list_property(mux, PROP_GROUPS)?;
            let function = single_string_property(mux, PROP_FUNCTION)?;
            let decoded = decode_named_state(&groups, function).map_err(pinctrl_state_error)?;
            merge_updates(&mut updates, &decoded).map_err(pinctrl_state_error)?;
            saw_mux = true;
        }
        if !saw_mux || updates.is_empty() {
            return Err(PnpError::malformed(
                PROVIDER_KIND,
                "pinctrl state has no valid mux child",
            ));
        }
        states.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
        states.push((phandle, updates.into_boxed_slice()));
    }
    Ok(states)
}

fn property<'a>(node: &'a DtbNodeInfo, name: &str) -> Option<&'a [u8]> {
    node.properties
        .iter()
        .find(|property| property.name.as_ref() == name)
        .map(|property: &DtbDeviceProperty| property.value.as_ref())
}

fn string_list_property<'a>(node: &'a DtbNodeInfo, name: &str) -> Result<Vec<&'a str>, PnpError> {
    let raw = property(node, name).ok_or(PnpError::missing(
        PROVIDER_KIND,
        "pinctrl mux is missing groups",
    ))?;
    if raw.is_empty() || raw.last() != Some(&0) {
        return Err(PnpError::malformed(
            PROVIDER_KIND,
            "pinctrl string list is not NUL terminated",
        ));
    }
    let mut values = Vec::new();
    for bytes in raw[..raw.len() - 1].split(|byte| *byte == 0) {
        if bytes.is_empty() {
            return Err(PnpError::malformed(
                PROVIDER_KIND,
                "pinctrl string list contains an empty item",
            ));
        }
        values.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
        values.push(str::from_utf8(bytes).map_err(|_| {
            PnpError::malformed(PROVIDER_KIND, "pinctrl string is not valid UTF-8")
        })?);
    }
    if values.is_empty() {
        return Err(PnpError::malformed(
            PROVIDER_KIND,
            "pinctrl string list is empty",
        ));
    }
    Ok(values)
}

fn single_string_property<'a>(node: &'a DtbNodeInfo, name: &str) -> Result<&'a str, PnpError> {
    let values = string_list_property(node, name)?;
    let [value] = values.as_slice() else {
        return Err(PnpError::malformed(
            PROVIDER_KIND,
            "pinctrl function must contain exactly one string",
        ));
    };
    Ok(*value)
}

fn gpio_irq_map(info: &PlatformDeviceInfo, ngpios: u32) -> Result<Option<GpioIrqMap>, PnpError> {
    if !info.bool_property(PROP_SUPPORT_IRQ) {
        return Ok(None);
    }
    let mut sources = Vec::new();
    for irq in info.irq_resources() {
        let [source] = irq.cells() else {
            return Err(PnpError::malformed(
                PnpResourceKind::Irq,
                "Loongson GPIO IRQ specifier must have one cell",
            ));
        };
        sources.try_reserve(1).map_err(|_| PnpError::OutOfMemory)?;
        sources.push(*source);
    }
    GpioIrqMap::new(ngpios, &sources, true)
        .map(Some)
        .map_err(|_| {
            PnpError::malformed(
                PnpResourceKind::Irq,
                "Loongson GPIO requires one IRQ source per line",
            )
        })
}

fn validate_gpio_binding(info: &PlatformDeviceInfo) -> Result<(), PnpError> {
    if !info.bool_property(PROP_GPIO_CONTROLLER) {
        return Err(PnpError::missing(
            PROVIDER_KIND,
            "Loongson GPIO is missing gpio-controller",
        ));
    }
    if info.u32_property(PROP_GPIO_CELLS) != Some(2) {
        return Err(PnpError::malformed(
            PROVIDER_KIND,
            "Loongson GPIO #gpio-cells must be two",
        ));
    }
    Ok(())
}

fn required_u32(info: &PlatformDeviceInfo, name: &str) -> Result<u32, PnpError> {
    info.u32_property(name).ok_or(PnpError::missing(
        PnpResourceKind::Other("property"),
        "Loongson GPIO required u32 property missing",
    ))
}

fn exact_mmio(info: &PlatformDeviceInfo, label: &'static str) -> Result<(usize, usize), PnpError> {
    let mut windows = info.mmio_resources();
    let window = windows.next().ok_or(PnpError::missing(
        PnpResourceKind::Mmio,
        "platform register window missing",
    ))?;
    if windows.next().is_some() {
        return Err(PnpError::malformed(PnpResourceKind::Mmio, label));
    }
    Ok(window)
}

fn platform_info(dev: &Arc<PnpDevice>) -> Result<&PlatformDeviceInfo, PnpError> {
    dev.info
        .as_any()
        .downcast_ref::<PlatformDeviceInfo>()
        .ok_or(PnpError::InvalidState)
}

fn pinctrl_layout_error(_error: PinctrlError) -> PnpError {
    PnpError::malformed(
        PnpResourceKind::Mmio,
        "invalid LS2K1000 pinctrl register window",
    )
}

fn pinctrl_state_error(error: PinctrlError) -> PnpError {
    match error {
        PinctrlError::OutOfMemory => PnpError::OutOfMemory,
        _ => PnpError::malformed(PROVIDER_KIND, "invalid LS2K1000 pinctrl state"),
    }
}

fn gpio_layout_error(_error: GpioError) -> PnpError {
    PnpError::malformed(
        PnpResourceKind::Mmio,
        "invalid Loongson GPIO register layout",
    )
}

fn gpio_acquire_error(error: GpioError) -> DtbProviderError {
    match error {
        GpioError::LineBusy => DtbProviderError::Busy,
        _ => DtbProviderError::AcquireFailed,
    }
}

fn gpio_control_error(_error: GpioError) -> DtbProviderError {
    DtbProviderError::HardwareFailure
}

struct Ls2kPinctrlFactory;

impl DriverFactory for Ls2kPinctrlFactory {
    fn name(&self) -> &'static str {
        "platform-loongson-ls2k1000-pinctrl"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Ls2kPinctrlDriver {
            device_mmio_to_virt: ctx.device_mmio_to_virt,
        }))
    }
}

struct LoongsonGpioFactory;

impl DriverFactory for LoongsonGpioFactory {
    fn name(&self) -> &'static str {
        "platform-loongson3-gpio"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(LoongsonGpioDriver {
            device_mmio_to_virt: ctx.device_mmio_to_virt,
        }))
    }
}

pub(super) fn register_builtin_drivers() -> Result<[DriverHandle; 2], PnpError> {
    let pinctrl = register_driver_factory(Arc::new(Ls2kPinctrlFactory))?;
    match register_driver_factory(Arc::new(LoongsonGpioFactory)) {
        Ok(gpio) => Ok([pinctrl, gpio]),
        Err(error) => {
            let _ = unregister_driver(pinctrl);
            Err(error)
        }
    }
}

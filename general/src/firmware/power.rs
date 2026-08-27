//! 平台无关的固件电源控制描述、动态注册表与执行入口。

use alloc::vec::Vec;

use crate::{StartAcpiIoOps, StartAcpiPciOps};
use log::printk;
use vfs::sync::Spinlock;

use crate::dev::pnp::{PnpHandleResource, PnpResourceKind, PnpResourceReleaseOrder};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerRegisterSpace {
    SystemMemory,
    SystemIo,
    PciConfig {
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAccessWidth {
    U8,
    U16,
    U32,
    U64,
}

impl PowerAccessWidth {
    pub fn from_bytes(bytes: usize) -> Option<Self> {
        match bytes {
            1 => Some(Self::U8),
            2 => Some(Self::U16),
            4 => Some(Self::U32),
            8 => Some(Self::U64),
            _ => None,
        }
    }

    pub fn from_bits(bits: u8, access_size: u8) -> Option<Self> {
        match bits {
            8 => Some(Self::U8),
            16 => Some(Self::U16),
            32 => Some(Self::U32),
            64 => Some(Self::U64),
            _ => match access_size {
                1 => Some(Self::U8),
                2 => Some(Self::U16),
                3 => Some(Self::U32),
                4 => Some(Self::U64),
                _ => None,
            },
        }
    }

    pub const fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
            Self::U64 => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerRegister {
    pub space: PowerRegisterSpace,
    pub address: usize,
    pub access_width: PowerAccessWidth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerControlMethod {
    RegisterWrite {
        register: PowerRegister,
        value: u64,
    },
    AcpiPm1Sleep {
        pm1a_control: PowerRegister,
        pm1b_control: Option<PowerRegister>,
        sleep_type_a: u8,
        sleep_type_b: u8,
    },
    AcpiSleepControl {
        sleep_control: PowerRegister,
        sleep_type: u8,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PowerControlInfo {
    pub shutdown: Option<PowerControlMethod>,
    pub reboot: Option<PowerControlMethod>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerError {
    NotInstalled,
    NotFound,
    OutOfMemory,
    UnsupportedAddressSpace(PowerRegisterSpace),
    InvalidRegister,
}

#[derive(Clone, Copy, Default)]
struct RuntimePowerControlInfo {
    shutdown: Option<RuntimePowerControlMethod>,
    reboot: Option<RuntimePowerControlMethod>,
}

#[derive(Clone, Copy)]
enum RuntimePowerControlMethod {
    RegisterWrite {
        register: RuntimePowerRegister,
        value: u64,
    },
    AcpiPm1Sleep {
        pm1a_control: RuntimePowerRegister,
        pm1b_control: Option<RuntimePowerRegister>,
        sleep_type_a: u8,
        sleep_type_b: u8,
    },
    AcpiSleepControl {
        sleep_control: RuntimePowerRegister,
        sleep_type: u8,
    },
}

#[derive(Clone, Copy)]
struct RuntimePowerRegister {
    space: PowerRegisterSpace,
    address: usize,
    access_width: PowerAccessWidth,
    io_ops: Option<StartAcpiIoOps>,
    pci_ops: Option<StartAcpiPciOps>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PowerControlAction {
    Shutdown,
    Reboot,
}

/// 动态 power handler 的稳定所有权句柄。
///
/// 句柄编号单调递增且不会在清空注册表后复用，因此旧 ELM 或失败回滚路径不能
/// 误注销后续加载的新 handler。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerControlHandle {
    action: PowerControlAction,
    id: u64,
}

#[derive(Clone, Copy)]
struct RegisteredPowerControl {
    id: u64,
    method: RuntimePowerControlMethod,
}

struct PowerControlRegistry {
    /// 启动固件（DT/ACPI）提供的常驻兜底入口。
    fallback: RuntimePowerControlInfo,
    /// 动态驱动按登记顺序保存；最后登记且仍存活的 handler 优先。
    shutdown: Vec<RegisteredPowerControl>,
    reboot: Vec<RegisteredPowerControl>,
    next_id: u64,
}

impl PowerControlRegistry {
    const fn new() -> Self {
        Self {
            fallback: RuntimePowerControlInfo {
                shutdown: None,
                reboot: None,
            },
            shutdown: Vec::new(),
            reboot: Vec::new(),
            next_id: 1,
        }
    }

    fn effective(&self) -> RuntimePowerControlInfo {
        RuntimePowerControlInfo {
            shutdown: self
                .shutdown
                .last()
                .map(|entry| entry.method)
                .or(self.fallback.shutdown),
            reboot: self
                .reboot
                .last()
                .map(|entry| entry.method)
                .or(self.fallback.reboot),
        }
    }

    fn register(
        &mut self,
        action: PowerControlAction,
        method: RuntimePowerControlMethod,
    ) -> Result<PowerControlHandle, PowerError> {
        match action {
            PowerControlAction::Shutdown => self.shutdown.try_reserve(1),
            PowerControlAction::Reboot => self.reboot.try_reserve(1),
        }
        .map_err(|_| PowerError::OutOfMemory)?;
        let id = self.next_id;
        if id == 0 {
            return Err(PowerError::OutOfMemory);
        }
        self.next_id = id.checked_add(1).unwrap_or(0);
        let entry = RegisteredPowerControl { id, method };
        match action {
            PowerControlAction::Shutdown => self.shutdown.push(entry),
            PowerControlAction::Reboot => self.reboot.push(entry),
        }
        Ok(PowerControlHandle { action, id })
    }

    fn unregister(&mut self, handle: PowerControlHandle) -> Result<(), PowerError> {
        let entries = match handle.action {
            PowerControlAction::Shutdown => &mut self.shutdown,
            PowerControlAction::Reboot => &mut self.reboot,
        };
        let index = entries
            .iter()
            .position(|entry| entry.id == handle.id)
            .ok_or(PowerError::NotFound)?;
        // 保持注册顺序，确保移除非栈顶 handler 后“最后登记者优先”的语义不变。
        entries.remove(index);
        Ok(())
    }
}

static POWER_CONTROLS: Spinlock<PowerControlRegistry> = Spinlock::new(PowerControlRegistry::new());

#[kernel_symbols::export(name = "general.firmware.power.clear", contract = "kernel.firmware.power@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn clear() {
    let mut controls = POWER_CONTROLS.lock();
    controls.fallback = RuntimePowerControlInfo::default();
    controls.shutdown.clear();
    controls.reboot.clear();
}

#[kernel_symbols::export(name = "general.firmware.power.install", contract = "kernel.firmware.power@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE, retained_args = 1 << 1)]
pub fn install(info: PowerControlInfo, phys_to_virt: fn(usize) -> usize) {
    install_with_platform_ops(info, phys_to_virt, None);
}

/// Install firmware power controls with architecture-specific MMIO and I/O
/// operations. SystemMemory addresses are mapped through the device-MMIO
/// callback; SystemIo requires an explicit port-I/O backend.
pub fn install_with_platform_ops(
    info: PowerControlInfo,
    device_mmio_to_virt: fn(usize) -> usize,
    io_ops: Option<StartAcpiIoOps>,
) {
    install_with_acpi_ops(info, device_mmio_to_virt, io_ops, None);
}

/// 安装同时支持 SystemIO 与 PCIConfig GAS 的 ACPI 电源控制。
pub fn install_with_acpi_ops(
    info: PowerControlInfo,
    device_mmio_to_virt: fn(usize) -> usize,
    io_ops: Option<StartAcpiIoOps>,
    pci_ops: Option<StartAcpiPciOps>,
) {
    let runtime = RuntimePowerControlInfo {
        shutdown: info
            .shutdown
            .map(|method| runtime_method(method, device_mmio_to_virt, io_ops, pci_ops)),
        reboot: info
            .reboot
            .map(|method| runtime_method(method, device_mmio_to_virt, io_ops, pci_ops)),
    };

    POWER_CONTROLS.lock().fallback = runtime;

    printk!(
        "[firmware][power] firmware fallback installed: shutdown={} reboot={}",
        runtime.shutdown.is_some() as usize,
        runtime.reboot.is_some() as usize
    );
}

#[kernel_symbols::export(name = "general.firmware.power.install_shutdown", contract = "kernel.firmware.power@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE, retained_args = 1 << 1)]
pub fn install_shutdown(method: PowerControlMethod, phys_to_virt: fn(usize) -> usize) {
    install_one(Some(runtime_method(method, phys_to_virt, None, None)), None);
}

#[kernel_symbols::export(name = "general.firmware.power.install_reboot", contract = "kernel.firmware.power@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE, retained_args = 1 << 1)]
pub fn install_reboot(method: PowerControlMethod, phys_to_virt: fn(usize) -> usize) {
    install_one(None, Some(runtime_method(method, phys_to_virt, None, None)));
}

fn install_one(
    shutdown: Option<RuntimePowerControlMethod>,
    reboot: Option<RuntimePowerControlMethod>,
) {
    let mut registry = POWER_CONTROLS.lock();
    if let Some(method) = shutdown {
        registry.fallback.shutdown = Some(method);
    }
    if let Some(method) = reboot {
        registry.fallback.reboot = Some(method);
    }
    let controls = registry.effective();
    drop(registry);
    printk!(
        "[firmware][power] firmware fallback updated: shutdown={} reboot={}",
        controls.shutdown.is_some() as usize,
        controls.reboot.is_some() as usize
    );
}

/// 登记一个可随驱动卸载撤销的关机入口。
#[kernel_symbols::export(
    name = "general.firmware.power.register_shutdown",
    contract = "kernel.firmware.power@1",
    version = 1,
    capabilities = kernel_symbols::capability::FIRMWARE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 1 << 1
)]
pub fn register_shutdown(
    method: PowerControlMethod,
    phys_to_virt: fn(usize) -> usize,
) -> Result<PowerControlHandle, PowerError> {
    register_dynamic(
        PowerControlAction::Shutdown,
        runtime_method(method, phys_to_virt, None, None),
    )
}

/// 登记一个可随驱动卸载撤销的重启入口。
#[kernel_symbols::export(
    name = "general.firmware.power.register_reboot",
    contract = "kernel.firmware.power@1",
    version = 1,
    capabilities = kernel_symbols::capability::FIRMWARE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 1 << 1
)]
pub fn register_reboot(
    method: PowerControlMethod,
    phys_to_virt: fn(usize) -> usize,
) -> Result<PowerControlHandle, PowerError> {
    register_dynamic(
        PowerControlAction::Reboot,
        runtime_method(method, phys_to_virt, None, None),
    )
}

fn register_dynamic(
    action: PowerControlAction,
    method: RuntimePowerControlMethod,
) -> Result<PowerControlHandle, PowerError> {
    let handle = POWER_CONTROLS.lock().register(action, method)?;
    printk!(
        "[firmware][power] dynamic {:?} handler registered: id={}",
        action,
        handle.id
    );
    Ok(handle)
}

/// 撤销一个动态 power handler；若它是当前入口，会自动恢复前一个动态 handler
/// 或启动固件提供的 fallback。
#[kernel_symbols::export(
    name = "general.firmware.power.unregister",
    contract = "kernel.firmware.power@1",
    version = 1,
    capabilities = kernel_symbols::capability::FIRMWARE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister(handle: PowerControlHandle) -> Result<(), PowerError> {
    POWER_CONTROLS.lock().unregister(handle)?;
    printk!(
        "[firmware][power] dynamic {:?} handler unregistered: id={}",
        handle.action,
        handle.id
    );
    Ok(())
}

fn prepare_power_control_resource(_handle: PowerControlHandle) -> bool {
    // handler 没有外发 lease 或在途回调；即使已由错误回滚提前撤销，提交也可幂等完成。
    true
}

fn cancel_power_control_resource(_handle: PowerControlHandle) {}

fn release_power_control_resource(handle: PowerControlHandle) -> bool {
    matches!(unregister(handle), Ok(()) | Err(PowerError::NotFound))
}

/// 把动态 power handler 交给 PnP 设备拥有。
///
/// 该资源按 consumer 顺序释放，使同一热移除事务中的 syscon 等 provider 在全局
/// 电源入口撤销后才进入提交阶段。
#[kernel_symbols::export(
    name = "general.firmware.power.pnp_resource",
    contract = "kernel.firmware.power@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn pnp_resource(
    handle: PowerControlHandle,
    label: &'static str,
) -> PnpHandleResource<PowerControlHandle> {
    PnpHandleResource::new_checked(
        PnpResourceKind::Other("power-control"),
        label,
        handle,
        prepare_power_control_resource,
        cancel_power_control_resource,
        PnpResourceReleaseOrder::Consumer,
        release_power_control_resource,
    )
}

/// 在常驻 General 侧构造完成类型擦除的 power-control 资源。
#[kernel_symbols::export(
    name = "general.firmware.power.pnp_resource_boxed",
    contract = "kernel.firmware.power@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn pnp_resource_boxed(
    handle: PowerControlHandle,
    label: &'static str,
) -> alloc::boxed::Box<dyn crate::dev::pnp::PnpResource> {
    alloc::boxed::Box::new(pnp_resource(handle, label))
}

#[kernel_symbols::export(name = "general.firmware.power.shutdown", contract = "kernel.firmware.power@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn shutdown() -> Result<(), PowerError> {
    let controls = load_controls()?;
    let Some(method) = controls.shutdown else {
        return Err(PowerError::NotInstalled);
    };
    printk!("[firmware][power] requesting shutdown");
    execute(method)
}

#[kernel_symbols::export(name = "general.firmware.power.reboot", contract = "kernel.firmware.power@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn reboot() -> Result<(), PowerError> {
    let controls = load_controls()?;
    let Some(method) = controls.reboot else {
        return Err(PowerError::NotInstalled);
    };
    printk!("[firmware][power] requesting reboot");
    execute(method)
}

fn load_controls() -> Result<RuntimePowerControlInfo, PowerError> {
    let controls = POWER_CONTROLS.lock().effective();
    if controls.shutdown.is_none() && controls.reboot.is_none() {
        Err(PowerError::NotInstalled)
    } else {
        Ok(controls)
    }
}

fn runtime_method(
    method: PowerControlMethod,
    device_mmio_to_virt: fn(usize) -> usize,
    io_ops: Option<StartAcpiIoOps>,
    pci_ops: Option<StartAcpiPciOps>,
) -> RuntimePowerControlMethod {
    match method {
        PowerControlMethod::RegisterWrite { register, value } => {
            RuntimePowerControlMethod::RegisterWrite {
                register: runtime_register(register, device_mmio_to_virt, io_ops, pci_ops),
                value,
            }
        }
        PowerControlMethod::AcpiPm1Sleep {
            pm1a_control,
            pm1b_control,
            sleep_type_a,
            sleep_type_b,
        } => RuntimePowerControlMethod::AcpiPm1Sleep {
            pm1a_control: runtime_register(pm1a_control, device_mmio_to_virt, io_ops, pci_ops),
            pm1b_control: pm1b_control
                .map(|register| runtime_register(register, device_mmio_to_virt, io_ops, pci_ops)),
            sleep_type_a,
            sleep_type_b,
        },
        PowerControlMethod::AcpiSleepControl {
            sleep_control,
            sleep_type,
        } => RuntimePowerControlMethod::AcpiSleepControl {
            sleep_control: runtime_register(sleep_control, device_mmio_to_virt, io_ops, pci_ops),
            sleep_type,
        },
    }
}

fn runtime_register(
    register: PowerRegister,
    device_mmio_to_virt: fn(usize) -> usize,
    io_ops: Option<StartAcpiIoOps>,
    pci_ops: Option<StartAcpiPciOps>,
) -> RuntimePowerRegister {
    let address = match register.space {
        PowerRegisterSpace::SystemMemory => {
            if memory_address_valid(register.address, register.access_width) {
                device_mmio_to_virt(register.address)
            } else {
                0
            }
        }
        PowerRegisterSpace::SystemIo => register.address,
        PowerRegisterSpace::PciConfig { .. } => register.address,
    };
    RuntimePowerRegister {
        space: register.space,
        address,
        access_width: register.access_width,
        io_ops,
        pci_ops,
    }
}

fn execute(method: RuntimePowerControlMethod) -> Result<(), PowerError> {
    match method {
        RuntimePowerControlMethod::RegisterWrite { register, value } => {
            write_register(register, value)
        }
        RuntimePowerControlMethod::AcpiPm1Sleep {
            pm1a_control,
            pm1b_control,
            sleep_type_a,
            sleep_type_b,
        } => {
            write_pm1_sleep(pm1a_control, sleep_type_a)?;
            if let Some(pm1b_control) = pm1b_control {
                write_pm1_sleep(pm1b_control, sleep_type_b)?;
            }
            Ok(())
        }
        RuntimePowerControlMethod::AcpiSleepControl {
            sleep_control,
            sleep_type,
        } => {
            let value = ((sleep_type as u64 & 0x7) << 2) | (1 << 5);
            write_register(sleep_control, value)
        }
    }
}

fn write_pm1_sleep(register: RuntimePowerRegister, sleep_type: u8) -> Result<(), PowerError> {
    let current = read_register(register)?;
    let value = (current & !(0x7 << 10)) | ((sleep_type as u64 & 0x7) << 10) | (1 << 13);
    write_register(register, value)
}

fn read_register(register: RuntimePowerRegister) -> Result<u64, PowerError> {
    match register.space {
        PowerRegisterSpace::SystemMemory => read_memory_register(register),
        PowerRegisterSpace::SystemIo => {
            let port = system_io_port(register)?;
            let ops = register.io_ops.ok_or(PowerError::UnsupportedAddressSpace(
                PowerRegisterSpace::SystemIo,
            ))?;
            Ok(match register.access_width {
                PowerAccessWidth::U8 => (ops.read_u8)(port) as u64,
                PowerAccessWidth::U16 => (ops.read_u16)(port) as u64,
                PowerAccessWidth::U32 => (ops.read_u32)(port) as u64,
                PowerAccessWidth::U64 => return Err(PowerError::InvalidRegister),
            })
        }
        PowerRegisterSpace::PciConfig {
            segment,
            bus,
            device,
            function,
        } => {
            let offset = pci_config_offset(register)?;
            let ops = register
                .pci_ops
                .ok_or(PowerError::UnsupportedAddressSpace(register.space))?;
            Ok(match register.access_width {
                PowerAccessWidth::U8 => {
                    (ops.read_u8)(segment, bus, device, function, offset) as u64
                }
                PowerAccessWidth::U16 => {
                    (ops.read_u16)(segment, bus, device, function, offset) as u64
                }
                PowerAccessWidth::U32 => {
                    (ops.read_u32)(segment, bus, device, function, offset) as u64
                }
                PowerAccessWidth::U64 => return Err(PowerError::InvalidRegister),
            })
        }
    }
}

fn write_register(register: RuntimePowerRegister, value: u64) -> Result<(), PowerError> {
    match register.space {
        PowerRegisterSpace::SystemMemory => write_memory_register(register, value),
        PowerRegisterSpace::SystemIo => {
            let port = system_io_port(register)?;
            let ops = register.io_ops.ok_or(PowerError::UnsupportedAddressSpace(
                PowerRegisterSpace::SystemIo,
            ))?;
            match register.access_width {
                PowerAccessWidth::U8 => (ops.write_u8)(port, value as u8),
                PowerAccessWidth::U16 => (ops.write_u16)(port, value as u16),
                PowerAccessWidth::U32 => (ops.write_u32)(port, value as u32),
                PowerAccessWidth::U64 => return Err(PowerError::InvalidRegister),
            }
            Ok(())
        }
        PowerRegisterSpace::PciConfig {
            segment,
            bus,
            device,
            function,
        } => {
            let offset = pci_config_offset(register)?;
            let ops = register
                .pci_ops
                .ok_or(PowerError::UnsupportedAddressSpace(register.space))?;
            match register.access_width {
                PowerAccessWidth::U8 => {
                    (ops.write_u8)(segment, bus, device, function, offset, value as u8)
                }
                PowerAccessWidth::U16 => {
                    (ops.write_u16)(segment, bus, device, function, offset, value as u16)
                }
                PowerAccessWidth::U32 => {
                    (ops.write_u32)(segment, bus, device, function, offset, value as u32)
                }
                PowerAccessWidth::U64 => return Err(PowerError::InvalidRegister),
            }
            Ok(())
        }
    }
}

fn pci_config_offset(register: RuntimePowerRegister) -> Result<u16, PowerError> {
    let offset = u16::try_from(register.address).map_err(|_| PowerError::InvalidRegister)?;
    let width = register.access_width.bytes();
    if usize::from(offset) % width != 0
        || usize::from(offset)
            .checked_add(width)
            .is_none_or(|end| end > 4096)
    {
        return Err(PowerError::InvalidRegister);
    }
    Ok(offset)
}

fn read_memory_register(register: RuntimePowerRegister) -> Result<u64, PowerError> {
    validate_memory_register(register)?;
    // Safety: addresses originate from firmware or a probed platform device and
    // are validated for non-zero, naturally aligned volatile access below.
    let value = unsafe {
        match register.access_width {
            PowerAccessWidth::U8 => core::ptr::read_volatile(register.address as *const u8) as u64,
            PowerAccessWidth::U16 => {
                core::ptr::read_volatile(register.address as *const u16) as u64
            }
            PowerAccessWidth::U32 => {
                core::ptr::read_volatile(register.address as *const u32) as u64
            }
            PowerAccessWidth::U64 => core::ptr::read_volatile(register.address as *const u64),
        }
    };
    Ok(value)
}

fn write_memory_register(register: RuntimePowerRegister, value: u64) -> Result<(), PowerError> {
    validate_memory_register(register)?;
    // Safety: same validation and matching pointer widths as `read_memory_register`.
    unsafe {
        match register.access_width {
            PowerAccessWidth::U8 => {
                core::ptr::write_volatile(register.address as *mut u8, value as u8)
            }
            PowerAccessWidth::U16 => {
                core::ptr::write_volatile(register.address as *mut u16, value as u16)
            }
            PowerAccessWidth::U32 => {
                core::ptr::write_volatile(register.address as *mut u32, value as u32)
            }
            PowerAccessWidth::U64 => core::ptr::write_volatile(register.address as *mut u64, value),
        }
    }
    Ok(())
}

fn validate_memory_register(register: RuntimePowerRegister) -> Result<(), PowerError> {
    if !memory_address_valid(register.address, register.access_width) {
        return Err(PowerError::InvalidRegister);
    }
    Ok(())
}

fn system_io_port(register: RuntimePowerRegister) -> Result<u16, PowerError> {
    // ACPI has no 64-bit SystemIo transaction; the backend exposes u8/u16/u32.
    if register.access_width == PowerAccessWidth::U64 {
        return Err(PowerError::InvalidRegister);
    }
    u16::try_from(register.address).map_err(|_| PowerError::InvalidRegister)
}

fn memory_address_valid(address: usize, width: PowerAccessWidth) -> bool {
    let width = width.bytes();
    address != 0 && address.is_multiple_of(width) && address.checked_add(width - 1).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::pnp::PnpResource;
    use alloc::boxed::Box;
    extern crate std;
    use std::sync::{Arc, Barrier};
    use std::thread;

    static TEST_LOCK: Spinlock<()> = Spinlock::new(());

    fn identity(address: usize) -> usize {
        address
    }

    fn translated(address: usize) -> usize {
        address + 0x1000
    }

    const TEST_IO_OPS: StartAcpiIoOps = StartAcpiIoOps {
        read_u8: test_read_u8,
        read_u16: test_read_u16,
        read_u32: test_read_u32,
        write_u8: test_write_u8,
        write_u16: test_write_u16,
        write_u32: test_write_u32,
    };

    const TEST_PCI_OPS: StartAcpiPciOps = StartAcpiPciOps {
        read_u8: test_pci_read_u8,
        read_u16: test_pci_read_u16,
        read_u32: test_pci_read_u32,
        write_u8: test_pci_write_u8,
        write_u16: test_pci_write_u16,
        write_u32: test_pci_write_u32,
    };

    fn map_device_mmio(address: usize) -> usize {
        address + 0x1000
    }

    fn test_read_u8(port: u16) -> u8 {
        port as u8
    }

    fn test_read_u16(port: u16) -> u16 {
        port ^ 0x55aa
    }

    fn test_read_u32(port: u16) -> u32 {
        u32::from(port) | 0xa5a5_0000
    }

    fn test_write_u8(port: u16, value: u8) {
        assert_eq!(port, 0x64);
        assert_eq!(value, 1);
    }

    fn test_write_u16(port: u16, value: u16) {
        assert_eq!(port, 0x1234);
        assert_eq!(value, 2);
    }

    fn test_write_u32(port: u16, value: u32) {
        assert_eq!(port, 0xcf8);
        assert_eq!(value, 3);
    }

    fn assert_test_bdf(segment: u16, bus: u8, device: u8, function: u8, offset: u16) {
        assert_eq!((segment, bus, device, function, offset), (0, 0, 7, 2, 0x44));
    }

    fn test_pci_read_u8(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u8 {
        assert_test_bdf(segment, bus, device, function, offset);
        0x5a
    }

    fn test_pci_read_u16(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u16 {
        assert_test_bdf(segment, bus, device, function, offset);
        0x5a5a
    }

    fn test_pci_read_u32(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
        assert_test_bdf(segment, bus, device, function, offset);
        0x5a5a_5a5a
    }

    fn test_pci_write_u8(segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u8) {
        assert_test_bdf(segment, bus, device, function, offset);
        assert_eq!(value, 0xcf);
    }

    fn test_pci_write_u16(
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        _value: u16,
    ) {
        assert_test_bdf(segment, bus, device, function, offset);
    }

    fn test_pci_write_u32(
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        _value: u32,
    ) {
        assert_test_bdf(segment, bus, device, function, offset);
    }

    fn io_register(address: usize, access_width: PowerAccessWidth) -> RuntimePowerRegister {
        RuntimePowerRegister {
            space: PowerRegisterSpace::SystemIo,
            address,
            access_width,
            io_ops: Some(TEST_IO_OPS),
            pci_ops: None,
        }
    }

    fn register_write(address: usize, value: u64) -> PowerControlMethod {
        PowerControlMethod::RegisterWrite {
            register: PowerRegister {
                space: PowerRegisterSpace::SystemMemory,
                address,
                access_width: PowerAccessWidth::U32,
            },
            value,
        }
    }

    fn shutdown_register() -> Option<(usize, u64)> {
        match load_controls().ok()?.shutdown? {
            RuntimePowerControlMethod::RegisterWrite { register, value } => {
                Some((register.address, value))
            }
            _ => None,
        }
    }

    fn reboot_register() -> Option<(usize, u64)> {
        match load_controls().ok()?.reboot? {
            RuntimePowerControlMethod::RegisterWrite { register, value } => {
                Some((register.address, value))
            }
            _ => None,
        }
    }

    #[test]
    fn concurrent_firmware_updates_do_not_lose_an_action() {
        let _guard = TEST_LOCK.lock();
        clear();
        let barrier = Arc::new(Barrier::new(3));
        let shutdown_barrier = Arc::clone(&barrier);
        let shutdown = thread::spawn(move || {
            shutdown_barrier.wait();
            install_shutdown(register_write(0x600, 6), identity);
        });
        let reboot_barrier = Arc::clone(&barrier);
        let reboot = thread::spawn(move || {
            reboot_barrier.wait();
            install_reboot(register_write(0x700, 7), identity);
        });
        barrier.wait();
        shutdown.join().unwrap();
        reboot.join().unwrap();

        assert_eq!(shutdown_register(), Some((0x600, 6)));
        assert_eq!(reboot_register(), Some((0x700, 7)));
        clear();
    }

    #[test]
    fn dynamic_handler_overrides_and_restores_firmware_fallback() {
        let _guard = TEST_LOCK.lock();
        clear();
        install(
            PowerControlInfo {
                shutdown: Some(register_write(0x100, 1)),
                reboot: None,
            },
            identity,
        );
        let first = register_shutdown(register_write(0x200, 2), translated).unwrap();
        let second = register_shutdown(register_write(0x300, 3), identity).unwrap();

        assert_eq!(shutdown_register(), Some((0x300, 3)));
        unregister(first).unwrap();
        assert_eq!(shutdown_register(), Some((0x300, 3)));
        unregister(second).unwrap();
        assert_eq!(shutdown_register(), Some((0x100, 1)));
        clear();
    }

    #[test]
    fn pnp_resource_release_restores_fallback() {
        let _guard = TEST_LOCK.lock();
        clear();
        install_shutdown(register_write(0x400, 4), identity);
        let handle = register_shutdown(register_write(0x500, 5), identity).unwrap();
        let resource = pnp_resource(handle, "test-power-control");

        resource.prepare_release().unwrap();
        Box::new(resource).release().unwrap();
        assert_eq!(shutdown_register(), Some((0x400, 4)));
        assert_eq!(unregister(handle), Err(PowerError::NotFound));
        clear();
    }

    #[test]
    fn runtime_register_maps_memory_and_preserves_io_ports() {
        let memory = runtime_register(
            PowerRegister {
                space: PowerRegisterSpace::SystemMemory,
                address: 0x2000,
                access_width: PowerAccessWidth::U32,
            },
            map_device_mmio,
            Some(TEST_IO_OPS),
            None,
        );
        assert_eq!(memory.address, 0x3000);
        assert_eq!(memory.io_ops.map(|_| ()), Some(()));

        let invalid_memory = runtime_register(
            PowerRegister {
                space: PowerRegisterSpace::SystemMemory,
                address: 0x2001,
                access_width: PowerAccessWidth::U16,
            },
            map_device_mmio,
            None,
            None,
        );
        assert_eq!(invalid_memory.address, 0);

        let io = runtime_register(
            PowerRegister {
                space: PowerRegisterSpace::SystemIo,
                address: 0x64,
                access_width: PowerAccessWidth::U8,
            },
            map_device_mmio,
            Some(TEST_IO_OPS),
            None,
        );
        assert_eq!(io.address, 0x64);
    }

    #[test]
    fn system_memory_requires_natural_alignment() {
        let mut storage = 0u64;
        let address = core::ptr::addr_of_mut!(storage) as usize;
        let register = RuntimePowerRegister {
            space: PowerRegisterSpace::SystemMemory,
            address,
            access_width: PowerAccessWidth::U64,
            io_ops: None,
            pci_ops: None,
        };
        write_register(register, 0x1122_3344_5566_7788).unwrap();
        assert_eq!(read_register(register), Ok(0x1122_3344_5566_7788));

        let unaligned = RuntimePowerRegister {
            address: address + 1,
            access_width: PowerAccessWidth::U16,
            ..register
        };
        assert_eq!(read_register(unaligned), Err(PowerError::InvalidRegister));
        assert_eq!(
            write_register(unaligned, 0),
            Err(PowerError::InvalidRegister)
        );
    }

    #[test]
    fn system_io_supports_u8_u16_and_u32() {
        let u8_register = io_register(0x64, PowerAccessWidth::U8);
        let u16_register = io_register(0x1234, PowerAccessWidth::U16);
        let u32_register = io_register(0xcf8, PowerAccessWidth::U32);

        assert_eq!(read_register(u8_register), Ok(0x64));
        assert_eq!(read_register(u16_register), Ok(0x1234 ^ 0x55aa));
        assert_eq!(read_register(u32_register), Ok(0xa5a5_0cf8));
        assert_eq!(write_register(u8_register, 1), Ok(()));
        assert_eq!(write_register(u16_register, 2), Ok(()));
        assert_eq!(write_register(u32_register, 3), Ok(()));
    }

    #[test]
    fn system_io_validates_range_backend_and_width() {
        let maximum_port = io_register(usize::from(u16::MAX), PowerAccessWidth::U16);
        assert_eq!(
            read_register(maximum_port),
            Ok(u64::from(u16::MAX ^ 0x55aa))
        );

        let out_of_range = io_register(usize::from(u16::MAX) + 1, PowerAccessWidth::U8);
        assert_eq!(
            read_register(out_of_range),
            Err(PowerError::InvalidRegister)
        );

        let unsupported_width = io_register(0x64, PowerAccessWidth::U64);
        assert_eq!(
            write_register(unsupported_width, 0),
            Err(PowerError::InvalidRegister)
        );

        let no_backend = RuntimePowerRegister {
            io_ops: None,
            ..io_register(0x64, PowerAccessWidth::U8)
        };
        assert_eq!(
            read_register(no_backend),
            Err(PowerError::UnsupportedAddressSpace(
                PowerRegisterSpace::SystemIo
            ))
        );
    }

    #[test]
    fn pci_config_register_uses_full_bdf_and_validates_offset() {
        let register = RuntimePowerRegister {
            space: PowerRegisterSpace::PciConfig {
                segment: 0,
                bus: 0,
                device: 7,
                function: 2,
            },
            address: 0x44,
            access_width: PowerAccessWidth::U8,
            io_ops: None,
            pci_ops: Some(TEST_PCI_OPS),
        };
        assert_eq!(read_register(register), Ok(0x5a));
        assert_eq!(write_register(register, 0xcf), Ok(()));

        let invalid = RuntimePowerRegister {
            address: 4096,
            ..register
        };
        assert_eq!(read_register(invalid), Err(PowerError::InvalidRegister));
    }
}

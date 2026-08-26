//! Platform-neutral firmware power-control descriptors and executor.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::StartAcpiIoOps;
use log::printk;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerRegisterSpace {
    SystemMemory,
    SystemIo,
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
}

static POWER_CONTROLS_VALID: AtomicBool = AtomicBool::new(false);
static mut POWER_CONTROLS: RuntimePowerControlInfo = RuntimePowerControlInfo {
    shutdown: None,
    reboot: None,
};

#[kernel_symbols::export(name = "general.firmware.power.clear", contract = "kernel.firmware.power@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn clear() {
    POWER_CONTROLS_VALID.store(false, Ordering::Release);
    unsafe {
        POWER_CONTROLS = RuntimePowerControlInfo::default();
    }
}

#[kernel_symbols::export(name = "general.firmware.power.install", contract = "kernel.firmware.power@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE, retained_args = 1 << 1)]
pub fn install(info: PowerControlInfo, phys_to_virt: fn(usize) -> usize) {
    install_with_platform_ops(info, phys_to_virt, None);
}

/// 安装内核启动路径解析出的电源控制信息。
///
/// 该入口不是 ELM 导出 ABI。设备 MMIO 必须使用架构提供的设备地址转换，SystemIO
/// 则只在 ACPI 启动路径提供了真实端口访问回调时可用。
pub fn install_with_platform_ops(
    info: PowerControlInfo,
    device_mmio_to_virt: fn(usize) -> usize,
    io_ops: Option<StartAcpiIoOps>,
) {
    let runtime = RuntimePowerControlInfo {
        shutdown: info
            .shutdown
            .map(|method| runtime_method(method, device_mmio_to_virt, io_ops)),
        reboot: info
            .reboot
            .map(|method| runtime_method(method, device_mmio_to_virt, io_ops)),
    };

    unsafe {
        POWER_CONTROLS = runtime;
    }
    POWER_CONTROLS_VALID.store(true, Ordering::Release);

    printk!(
        "[firmware][power] installed: shutdown={} reboot={}",
        runtime.shutdown.is_some() as usize,
        runtime.reboot.is_some() as usize
    );
}

#[kernel_symbols::export(name = "general.firmware.power.install_shutdown", contract = "kernel.firmware.power@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE, retained_args = 1 << 1)]
pub fn install_shutdown(method: PowerControlMethod, phys_to_virt: fn(usize) -> usize) {
    install_one(Some(runtime_method(method, phys_to_virt, None)), None);
}

#[kernel_symbols::export(name = "general.firmware.power.install_reboot", contract = "kernel.firmware.power@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE, retained_args = 1 << 1)]
pub fn install_reboot(method: PowerControlMethod, phys_to_virt: fn(usize) -> usize) {
    install_one(None, Some(runtime_method(method, phys_to_virt, None)));
}

fn install_one(
    shutdown: Option<RuntimePowerControlMethod>,
    reboot: Option<RuntimePowerControlMethod>,
) {
    let mut controls = if POWER_CONTROLS_VALID.load(Ordering::Acquire) {
        unsafe { POWER_CONTROLS }
    } else {
        RuntimePowerControlInfo::default()
    };
    if let Some(method) = shutdown {
        controls.shutdown = Some(method);
    }
    if let Some(method) = reboot {
        controls.reboot = Some(method);
    }
    unsafe {
        POWER_CONTROLS = controls;
    }
    POWER_CONTROLS_VALID.store(true, Ordering::Release);
    printk!(
        "[firmware][power] updated: shutdown={} reboot={}",
        controls.shutdown.is_some() as usize,
        controls.reboot.is_some() as usize
    );
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
    if !POWER_CONTROLS_VALID.load(Ordering::Acquire) {
        return Err(PowerError::NotInstalled);
    }
    Ok(unsafe { POWER_CONTROLS })
}

fn runtime_method(
    method: PowerControlMethod,
    device_mmio_to_virt: fn(usize) -> usize,
    io_ops: Option<StartAcpiIoOps>,
) -> RuntimePowerControlMethod {
    match method {
        PowerControlMethod::RegisterWrite { register, value } => {
            RuntimePowerControlMethod::RegisterWrite {
                register: runtime_register(register, device_mmio_to_virt, io_ops),
                value,
            }
        }
        PowerControlMethod::AcpiPm1Sleep {
            pm1a_control,
            pm1b_control,
            sleep_type_a,
            sleep_type_b,
        } => RuntimePowerControlMethod::AcpiPm1Sleep {
            pm1a_control: runtime_register(pm1a_control, device_mmio_to_virt, io_ops),
            pm1b_control: pm1b_control
                .map(|register| runtime_register(register, device_mmio_to_virt, io_ops)),
            sleep_type_a,
            sleep_type_b,
        },
        PowerControlMethod::AcpiSleepControl {
            sleep_control,
            sleep_type,
        } => RuntimePowerControlMethod::AcpiSleepControl {
            sleep_control: runtime_register(sleep_control, device_mmio_to_virt, io_ops),
            sleep_type,
        },
    }
}

fn runtime_register(
    register: PowerRegister,
    device_mmio_to_virt: fn(usize) -> usize,
    io_ops: Option<StartAcpiIoOps>,
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
    };
    RuntimePowerRegister {
        space: register.space,
        address,
        access_width: register.access_width,
        io_ops,
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
    }
}

fn read_memory_register(register: RuntimePowerRegister) -> Result<u64, PowerError> {
    validate_memory_register(register)?;
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
    // ACPI defines no 64-bit SystemIO transaction and the architecture backend
    // intentionally exposes only the three widths implementable by port I/O.
    if register.access_width == PowerAccessWidth::U64 {
        return Err(PowerError::InvalidRegister);
    }
    u16::try_from(register.address).map_err(|_| PowerError::InvalidRegister)
}

fn memory_address_valid(address: usize, width: PowerAccessWidth) -> bool {
    let width = access_width_bytes(width);
    address != 0 && address.is_multiple_of(width) && address.checked_add(width - 1).is_some()
}

const fn access_width_bytes(width: PowerAccessWidth) -> usize {
    match width {
        PowerAccessWidth::U8 => 1,
        PowerAccessWidth::U16 => 2,
        PowerAccessWidth::U32 => 4,
        PowerAccessWidth::U64 => 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_IO_OPS: StartAcpiIoOps = StartAcpiIoOps {
        read_u8: test_read_u8,
        read_u16: test_read_u16,
        read_u32: test_read_u32,
        write_u8: test_write_u8,
        write_u16: test_write_u16,
        write_u32: test_write_u32,
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

    fn io_register(address: usize, access_width: PowerAccessWidth) -> RuntimePowerRegister {
        RuntimePowerRegister {
            space: PowerRegisterSpace::SystemIo,
            address,
            access_width,
            io_ops: Some(TEST_IO_OPS),
        }
    }

    #[test]
    fn runtime_register_uses_device_mapping_only_for_system_memory() {
        let memory = runtime_register(
            PowerRegister {
                space: PowerRegisterSpace::SystemMemory,
                address: 0x2000,
                access_width: PowerAccessWidth::U32,
            },
            map_device_mmio,
            Some(TEST_IO_OPS),
        );
        assert_eq!(memory.address, 0x3000);

        let invalid_memory = runtime_register(
            PowerRegister {
                space: PowerRegisterSpace::SystemMemory,
                address: 0x2001,
                access_width: PowerAccessWidth::U16,
            },
            map_device_mmio,
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
}

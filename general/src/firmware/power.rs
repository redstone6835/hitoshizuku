//! Platform-neutral firmware power-control descriptors and executor.

use core::sync::atomic::{AtomicBool, Ordering};

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

#[derive(Clone, Copy, Debug, Default)]
struct RuntimePowerControlInfo {
    shutdown: Option<RuntimePowerControlMethod>,
    reboot: Option<RuntimePowerControlMethod>,
}

#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Copy, Debug)]
struct RuntimePowerRegister {
    space: PowerRegisterSpace,
    address: usize,
    access_width: PowerAccessWidth,
}

static POWER_CONTROLS_VALID: AtomicBool = AtomicBool::new(false);
static mut POWER_CONTROLS: RuntimePowerControlInfo = RuntimePowerControlInfo {
    shutdown: None,
    reboot: None,
};

pub fn clear() {
    POWER_CONTROLS_VALID.store(false, Ordering::Release);
    unsafe {
        POWER_CONTROLS = RuntimePowerControlInfo::default();
    }
}

pub fn install(info: PowerControlInfo, phys_to_virt: fn(usize) -> usize) {
    let runtime = RuntimePowerControlInfo {
        shutdown: info
            .shutdown
            .map(|method| runtime_method(method, phys_to_virt)),
        reboot: info
            .reboot
            .map(|method| runtime_method(method, phys_to_virt)),
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

pub fn install_shutdown(method: PowerControlMethod, phys_to_virt: fn(usize) -> usize) {
    install_one(Some(runtime_method(method, phys_to_virt)), None);
}

pub fn install_reboot(method: PowerControlMethod, phys_to_virt: fn(usize) -> usize) {
    install_one(None, Some(runtime_method(method, phys_to_virt)));
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

pub fn shutdown() -> Result<(), PowerError> {
    let controls = load_controls()?;
    let Some(method) = controls.shutdown else {
        return Err(PowerError::NotInstalled);
    };
    printk!("[firmware][power] requesting shutdown");
    execute(method)
}

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
    phys_to_virt: fn(usize) -> usize,
) -> RuntimePowerControlMethod {
    match method {
        PowerControlMethod::RegisterWrite { register, value } => {
            RuntimePowerControlMethod::RegisterWrite {
                register: runtime_register(register, phys_to_virt),
                value,
            }
        }
        PowerControlMethod::AcpiPm1Sleep {
            pm1a_control,
            pm1b_control,
            sleep_type_a,
            sleep_type_b,
        } => RuntimePowerControlMethod::AcpiPm1Sleep {
            pm1a_control: runtime_register(pm1a_control, phys_to_virt),
            pm1b_control: pm1b_control.map(|register| runtime_register(register, phys_to_virt)),
            sleep_type_a,
            sleep_type_b,
        },
        PowerControlMethod::AcpiSleepControl {
            sleep_control,
            sleep_type,
        } => RuntimePowerControlMethod::AcpiSleepControl {
            sleep_control: runtime_register(sleep_control, phys_to_virt),
            sleep_type,
        },
    }
}

fn runtime_register(
    register: PowerRegister,
    phys_to_virt: fn(usize) -> usize,
) -> RuntimePowerRegister {
    let address = match register.space {
        PowerRegisterSpace::SystemMemory => phys_to_virt(register.address),
        PowerRegisterSpace::SystemIo => register.address,
    };
    RuntimePowerRegister {
        space: register.space,
        address,
        access_width: register.access_width,
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
    if register.space != PowerRegisterSpace::SystemMemory {
        return Err(PowerError::UnsupportedAddressSpace(register.space));
    }
    if register.address == 0 {
        return Err(PowerError::InvalidRegister);
    }

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

fn write_register(register: RuntimePowerRegister, value: u64) -> Result<(), PowerError> {
    if register.space != PowerRegisterSpace::SystemMemory {
        return Err(PowerError::UnsupportedAddressSpace(register.space));
    }
    if register.address == 0 {
        return Err(PowerError::InvalidRegister);
    }

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

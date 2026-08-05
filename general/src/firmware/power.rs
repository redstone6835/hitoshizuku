//! 平台无关的固件电源控制描述、动态注册表与执行入口。

use alloc::vec::Vec;

use log::printk;
use vfs::sync::Spinlock;

use crate::dev::pnp::{PnpHandleResource, PnpResourceKind, PnpResourceReleaseOrder};

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RuntimePowerControlInfo {
    shutdown: Option<RuntimePowerControlMethod>,
    reboot: Option<RuntimePowerControlMethod>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RuntimePowerRegister {
    space: PowerRegisterSpace,
    address: usize,
    access_width: PowerAccessWidth,
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

#[derive(Clone, Copy, Debug)]
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
    let runtime = RuntimePowerControlInfo {
        shutdown: info
            .shutdown
            .map(|method| runtime_method(method, phys_to_virt)),
        reboot: info
            .reboot
            .map(|method| runtime_method(method, phys_to_virt)),
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
    install_one(Some(runtime_method(method, phys_to_virt)), None);
}

#[kernel_symbols::export(name = "general.firmware.power.install_reboot", contract = "kernel.firmware.power@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_ADMIN, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE, retained_args = 1 << 1)]
pub fn install_reboot(method: PowerControlMethod, phys_to_virt: fn(usize) -> usize) {
    install_one(None, Some(runtime_method(method, phys_to_virt)));
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
        runtime_method(method, phys_to_virt),
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
        runtime_method(method, phys_to_virt),
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
    if register.address == 0 || register.address % register.access_width.bytes() != 0 {
        return Err(PowerError::InvalidRegister);
    }

    // Safety: 地址来自启动固件或已完成 probe 的平台驱动，并在下方按访问宽度验证
    // 非零与自然对齐；电源控制寄存器要求易失访问。
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
    if register.address == 0 || register.address % register.access_width.bytes() != 0 {
        return Err(PowerError::InvalidRegister);
    }

    // Safety: 安全条件与 `read_register` 相同，各分支的指针类型与访问宽度一致。
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
}

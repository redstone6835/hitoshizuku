//! 通用 MSI controller 注册与分配层。
//!
//! MSI 是一种“设备向一段平台定义的物理地址写入消息数据，从而触发中断”的
//! 机制。PCI、platform 或未来其它总线只需要拿到 [`MsiMessage`] 和对应的
//! [`IrqLine`]；具体 message 地址、data 编码、vector 池管理都属于 MSI
//! controller driver 的职责。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU64;

use vfs::sync::Spinlock;

use super::registry_id;
use crate::dev::irq::IrqLine;
use crate::dev::pnp::{self, PnpDependency, PnpHandleResource, PnpResourceKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiMessage {
    pub address: u64,
    pub data: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiVector {
    pub hwirq: u32,
    pub line: IrqLine,
    pub message: MsiMessage,
}

pub trait MsiController: Send + Sync {
    /// 为一个 requester 分配 MSI vector。
    ///
    /// `requester` 的含义由上层总线定义。PCI 使用 requester id 经 `msi-map`
    /// 映射后的值；controller 可以用它做亲和性、稳定分配或直接忽略。
    fn allocate_vector(&self, requester: u32) -> Option<MsiVector>;

    /// 释放先前分配的 vector。实现必须允许重复释放未知 hwirq 并安全返回。
    fn free_vector(&self, hwirq: u32);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsiError {
    OutOfMemory,
    AlreadyRegistered,
    NotFound,
    AllocationFailed,
    Busy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiControllerHandle {
    controller: u32,
    id: u64,
}

impl MsiControllerHandle {
    pub const fn controller(self) -> u32 {
        self.controller
    }

    pub const fn id(self) -> u64 {
        self.id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiHandle {
    controller: u32,
    controller_id: u64,
    hwirq: u32,
    line: IrqLine,
    message: MsiMessage,
}

impl MsiHandle {
    pub const fn controller(self) -> u32 {
        self.controller
    }

    pub const fn controller_id(self) -> u64 {
        self.controller_id
    }

    pub const fn hwirq(self) -> u32 {
        self.hwirq
    }

    pub const fn line(self) -> IrqLine {
        self.line
    }

    pub const fn message(self) -> MsiMessage {
        self.message
    }
}

struct MsiControllerRegistration {
    controller: u32,
    id: u64,
    driver: Arc<dyn MsiController>,
    vectors: Vec<MsiVectorRegistration>,
    allocations_in_flight: usize,
    frees_in_flight: usize,
    retiring: bool,
}

struct MsiVectorRegistration {
    hwirq: u32,
    releasing: bool,
}

static MSI_CONTROLLERS: Spinlock<Vec<MsiControllerRegistration>> = Spinlock::new(Vec::new());
static NEXT_MSI_CONTROLLER_ID: AtomicU64 = AtomicU64::new(1);

#[kernel_symbols::export(
    name = "general.dev.msi.register_msi_controller",
    contract = "kernel.general.msi-controller@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn register_msi_controller(
    controller: u32,
    driver: Arc<dyn MsiController>,
) -> Result<MsiControllerHandle, MsiError> {
    let mut controllers = MSI_CONTROLLERS.lock();
    if controllers
        .iter()
        .any(|entry| entry.controller == controller)
    {
        return Err(MsiError::AlreadyRegistered);
    }
    controllers
        .try_reserve(1)
        .map_err(|_| MsiError::OutOfMemory)?;
    let id =
        registry_id::alloc_atomic_id(&NEXT_MSI_CONTROLLER_ID).map_err(|_| MsiError::OutOfMemory)?;
    // controller 编号来自固件路由，可能在卸载后重新出现；handle ID 区分每一次注册生命周期。
    controllers.push(MsiControllerRegistration {
        controller,
        id,
        driver,
        vectors: Vec::new(),
        allocations_in_flight: 0,
        frees_in_flight: 0,
        retiring: false,
    });
    let handle = MsiControllerHandle { controller, id };
    drop(controllers);
    if super::elm_lifecycle::track_msi_controller(handle).is_err() {
        let _ = unregister_msi_controller(handle);
        return Err(MsiError::OutOfMemory);
    }
    pnp::notify_dependency_ready(PnpDependency::MsiController(controller));
    Ok(handle)
}

#[kernel_symbols::export(
    name = "general.dev.msi.unregister_msi_controller",
    contract = "kernel.general.msi-controller@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_msi_controller(handle: MsiControllerHandle) -> Result<(), MsiError> {
    let mut controllers = MSI_CONTROLLERS.lock();
    let Some(index) = controllers
        .iter()
        .position(|entry| entry.controller == handle.controller && entry.id == handle.id)
    else {
        return Err(MsiError::NotFound);
    };
    if !controllers[index].vectors.is_empty()
        || controllers[index].allocations_in_flight != 0
        || controllers[index].frees_in_flight != 0
    {
        return Err(MsiError::Busy);
    }
    controllers.remove(index);
    drop(controllers);
    super::elm_lifecycle::forget_msi_controller(handle);
    Ok(())
}

#[kernel_symbols::export(
    name = "general.dev.msi.allocate_msi",
    contract = "kernel.general.msi-vector@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn allocate_msi(controller: u32, requester: u32) -> Result<MsiHandle, MsiError> {
    let (controller_id, driver) = {
        let mut controllers = MSI_CONTROLLERS.lock();
        let index = controllers
            .iter()
            .position(|entry| entry.controller == controller && !entry.retiring)
            .ok_or(MsiError::NotFound)?;
        let next_in_flight = controllers[index]
            .allocations_in_flight
            .checked_add(1)
            .ok_or(MsiError::Busy)?;
        {
            // vector 注册表容量由常驻 MSI controller 长期复用，不能把扩容
            // 记到触发本次分配的动态 ELM 名下。
            let _accounting =
                allocator::suspend_implicit_allocation_accounting().ok_or(MsiError::OutOfMemory)?;
            controllers[index]
                .vectors
                .try_reserve(next_in_flight)
                .map_err(|_| MsiError::OutOfMemory)?;
        }
        controllers[index].allocations_in_flight = next_in_flight;
        (
            controllers[index].id,
            Arc::clone(&controllers[index].driver),
        )
    };

    let vector = driver.allocate_vector(requester);
    let mut controllers = MSI_CONTROLLERS.lock();
    let Some(index) = controllers
        .iter()
        .position(|entry| entry.controller == controller && entry.id == controller_id)
    else {
        drop(controllers);
        if let Some(vector) = vector {
            driver.free_vector(vector.hwirq);
        }
        return Err(MsiError::NotFound);
    };
    let entry = &mut controllers[index];
    entry.allocations_in_flight = entry
        .allocations_in_flight
        .checked_sub(1)
        .ok_or(MsiError::Busy)?;
    let Some(vector) = vector else {
        if controller_ready_to_retire(entry) {
            controllers.remove(index);
        }
        return Err(MsiError::AllocationFailed);
    };
    if entry
        .vectors
        .iter()
        .any(|active| active.hwirq == vector.hwirq)
    {
        log::error!(
            "[msi] controller {} returned duplicate active hwirq {}",
            controller,
            vector.hwirq
        );
        // 无法区分 controller 是重复返回既有 vector，还是错误地创建了别名。
        // 此时调用 free_vector 可能破坏仍在使用的旧分配，因此只拒绝新句柄。
        entry.retiring = true;
        return Err(MsiError::AllocationFailed);
    }
    entry.vectors.push(MsiVectorRegistration {
        hwirq: vector.hwirq,
        releasing: false,
    });
    let handle = MsiHandle {
        controller,
        controller_id,
        hwirq: vector.hwirq,
        line: vector.line,
        message: vector.message,
    };
    drop(controllers);
    if super::elm_lifecycle::track_msi_vector(handle).is_err() {
        let _ = free_msi(handle);
        return Err(MsiError::OutOfMemory);
    }
    Ok(handle)
}

#[kernel_symbols::export(
    name = "general.dev.msi.free_msi",
    contract = "kernel.general.msi-vector@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn free_msi(handle: MsiHandle) -> Result<(), MsiError> {
    let driver = {
        let mut controllers = MSI_CONTROLLERS.lock();
        let index = controllers
            .iter()
            .position(|entry| {
                entry.controller == handle.controller && entry.id == handle.controller_id
            })
            .ok_or(MsiError::NotFound)?;
        let vector_index = controllers[index]
            .vectors
            .iter()
            .position(|vector| vector.hwirq == handle.hwirq)
            .ok_or(MsiError::NotFound)?;
        if controllers[index].vectors[vector_index].releasing {
            return Err(MsiError::Busy);
        }
        controllers[index].frees_in_flight = controllers[index]
            .frees_in_flight
            .checked_add(1)
            .ok_or(MsiError::Busy)?;
        controllers[index].vectors[vector_index].releasing = true;
        Arc::clone(&controllers[index].driver)
    };
    driver.free_vector(handle.hwirq);
    let mut controllers = MSI_CONTROLLERS.lock();
    let index = controllers
        .iter()
        .position(|entry| entry.controller == handle.controller && entry.id == handle.controller_id)
        .ok_or(MsiError::NotFound)?;
    let entry = &mut controllers[index];
    let vector_index = entry
        .vectors
        .iter()
        .position(|vector| vector.hwirq == handle.hwirq && vector.releasing)
        .ok_or(MsiError::NotFound)?;
    entry.vectors.remove(vector_index);
    entry.frees_in_flight = entry.frees_in_flight.checked_sub(1).ok_or(MsiError::Busy)?;
    if controller_ready_to_retire(entry) {
        controllers.remove(index);
    }
    drop(controllers);
    super::elm_lifecycle::forget_msi_vector(handle);
    Ok(())
}

fn controller_ready_to_retire(entry: &MsiControllerRegistration) -> bool {
    entry.retiring
        && entry.vectors.is_empty()
        && entry.allocations_in_flight == 0
        && entry.frees_in_flight == 0
}

fn retire_msi_controller(handle: MsiControllerHandle) -> Result<(), MsiError> {
    let mut controllers = MSI_CONTROLLERS.lock();
    let Some(index) = controllers
        .iter()
        .position(|entry| entry.controller == handle.controller && entry.id == handle.id)
    else {
        return Err(MsiError::NotFound);
    };
    controllers[index].retiring = true;
    if controller_ready_to_retire(&controllers[index]) {
        controllers.remove(index);
    }
    Ok(())
}

fn release_msi_controller_resource(handle: MsiControllerHandle) -> bool {
    retire_msi_controller(handle).is_ok()
}

fn release_msi_vector_resource(handle: MsiHandle) -> bool {
    free_msi(handle).is_ok()
}

/// 将 MSI controller 注册 handle 包装成 PnP-owned resource。
#[kernel_symbols::export(
    name = "general.dev.msi.controller_pnp_resource",
    contract = "kernel.general.device-resource@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn controller_pnp_resource(
    handle: MsiControllerHandle,
    label: &'static str,
) -> PnpHandleResource<MsiControllerHandle> {
    PnpHandleResource::new(
        PnpResourceKind::MsiController,
        label,
        handle,
        release_msi_controller_resource,
    )
}

/// 将单个 MSI vector 分配 handle 包装成 PnP-owned resource。
#[kernel_symbols::export(
    name = "general.dev.msi.vector_pnp_resource",
    contract = "kernel.general.device-resource@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn vector_pnp_resource(handle: MsiHandle, label: &'static str) -> PnpHandleResource<MsiHandle> {
    PnpHandleResource::new(
        PnpResourceKind::Msi,
        label,
        handle,
        release_msi_vector_resource,
    )
}

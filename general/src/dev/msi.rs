//! 通用 MSI controller 注册与分配层。
//!
//! MSI 是一种“设备向一段平台定义的物理地址写入消息数据，从而触发中断”的
//! 机制。PCI、platform 或未来其它总线只需要拿到 [`MsiMessage`] 和对应的
//! [`IrqLine`]；具体 message 地址、data 编码、vector 池管理都属于 MSI
//! controller driver 的职责。

use alloc::sync::Arc;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

use crate::dev::irq::IrqLine;

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiControllerHandle {
    controller: u32,
}

impl MsiControllerHandle {
    pub const fn controller(self) -> u32 {
        self.controller
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiHandle {
    controller: u32,
    hwirq: u32,
    line: IrqLine,
    message: MsiMessage,
}

impl MsiHandle {
    pub const fn controller(self) -> u32 {
        self.controller
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
    driver: Arc<dyn MsiController>,
}

static MSI_CONTROLLERS: Spinlock<Vec<MsiControllerRegistration>> = Spinlock::new(Vec::new());

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
    controllers.push(MsiControllerRegistration { controller, driver });
    Ok(MsiControllerHandle { controller })
}

pub fn unregister_msi_controller(handle: MsiControllerHandle) -> Result<(), MsiError> {
    let mut controllers = MSI_CONTROLLERS.lock();
    let Some(index) = controllers
        .iter()
        .position(|entry| entry.controller == handle.controller)
    else {
        return Err(MsiError::NotFound);
    };
    controllers.remove(index);
    Ok(())
}

pub fn allocate_msi(controller: u32, requester: u32) -> Result<MsiHandle, MsiError> {
    let driver = {
        let controllers = MSI_CONTROLLERS.lock();
        controllers
            .iter()
            .find(|entry| entry.controller == controller)
            .map(|entry| Arc::clone(&entry.driver))
    }
    .ok_or(MsiError::NotFound)?;

    let vector = driver
        .allocate_vector(requester)
        .ok_or(MsiError::AllocationFailed)?;
    Ok(MsiHandle {
        controller,
        hwirq: vector.hwirq,
        line: vector.line,
        message: vector.message,
    })
}

pub fn free_msi(handle: MsiHandle) -> Result<(), MsiError> {
    let driver = {
        let controllers = MSI_CONTROLLERS.lock();
        controllers
            .iter()
            .find(|entry| entry.controller == handle.controller)
            .map(|entry| Arc::clone(&entry.driver))
    }
    .ok_or(MsiError::NotFound)?;
    driver.free_vector(handle.hwirq);
    Ok(())
}

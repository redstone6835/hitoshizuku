//! 固件配置数据源抽象。
//!
//! fw_cfg 这类设备向内核暴露固件生成的只读配置项。设备驱动负责具体传输方式
//! （MMIO、PIO 或 DMA），上层只通过 selector/key 读取字节流，不直接接触寄存器。

use alloc::sync::Arc;
use core::sync::atomic::AtomicU64;

use vfs::sync::Spinlock;

use crate::dev::pnp::{self, PnpDependency, PnpHandleResource, PnpResourceKind};

use super::registry_id;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FwCfgError {
    Invalid,
    Io,
    NotInstalled,
    AlreadyInstalled,
    NotFound,
    OutOfMemory,
}

pub trait FwCfgDevice: Send + Sync {
    /// 读取一个 fw_cfg selector 对应的数据流。
    ///
    /// 每次读取前 driver 必须重新选择 selector，使读偏移从 0 开始。调用方提供的
    /// buffer 长度决定读取字节数；短读或设备不可用应返回错误，不能静默填零。
    fn read_item(&self, selector: u16, out: &mut [u8]) -> Result<(), FwCfgError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FwCfgHandle {
    id: u64,
}

impl FwCfgHandle {
    pub const fn id(self) -> u64 {
        self.id
    }
}

struct FwCfgRegistration {
    id: u64,
    dev: Arc<dyn FwCfgDevice>,
}

static FW_CFG: Spinlock<Option<FwCfgRegistration>> = Spinlock::new(None);
static NEXT_FW_CFG_ID: AtomicU64 = AtomicU64::new(1);

pub fn install(dev: Arc<dyn FwCfgDevice>) -> Result<FwCfgHandle, FwCfgError> {
    let mut current = FW_CFG.lock();
    if current.is_some() {
        return Err(FwCfgError::AlreadyInstalled);
    }
    let id = registry_id::alloc_atomic_id(&NEXT_FW_CFG_ID).map_err(|_| FwCfgError::OutOfMemory)?;
    // fw_cfg 当前是单实例资源，但 handle 仍表示一次安装生命周期，不能在卸载后复用。
    *current = Some(FwCfgRegistration { id, dev });
    drop(current);
    pnp::notify_dependency_ready(PnpDependency::FwCfg);
    Ok(FwCfgHandle { id })
}

pub fn uninstall(handle: FwCfgHandle) -> Result<(), FwCfgError> {
    let mut current = FW_CFG.lock();
    let Some(registration) = current.as_ref() else {
        return Err(FwCfgError::NotFound);
    };
    if registration.id != handle.id {
        return Err(FwCfgError::NotFound);
    }
    *current = None;
    Ok(())
}

fn release_fwcfg_resource(handle: FwCfgHandle) -> bool {
    uninstall(handle).is_ok()
}

/// 将 fw_cfg 安装 handle 包装成 PnP-owned resource。
pub fn pnp_resource(handle: FwCfgHandle, label: &'static str) -> PnpHandleResource<FwCfgHandle> {
    PnpHandleResource::new(
        PnpResourceKind::FwCfg,
        label,
        handle,
        release_fwcfg_resource,
    )
}

pub fn read_item(selector: u16, out: &mut [u8]) -> Result<(), FwCfgError> {
    let dev = {
        let current = FW_CFG.lock();
        current
            .as_ref()
            .map(|registration| Arc::clone(&registration.dev))
    }
    .ok_or(FwCfgError::NotInstalled)?;
    dev.read_item(selector, out)
}

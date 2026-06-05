//! 全局设备枚举。
//!
//! # 概览
//!
//! ```text
//! devices()               → &'static DeviceList
//!   └── .functions        → FunctionRegistry（开放设备 function 注册表）
//! ```
//!
//! # PnP 设备与驱动
//!
//! ```text
//! PNP_DEVICES             → PnpDeviceList （PnP 设备全局注册表）
//! PNP_DRIVERS             → PnpDriverRegistry（PnP 驱动注册表）
//! ```
//!
//! 固件或总线发现路径只注册 `PnpDevice`。具体驱动通过 PnP probe 认领设备，
//! 创建对应 function 并调用 `PnpDevice::register_function()`。

// ─────────────────────────── 顶层设备列表 ─────────────────────────────────

use alloc::sync::Arc;

use crate::dev::function::{DeviceFunction, FunctionRegistry, FunctionRegistryError};

/// 全局设备列表，包含各类设备的对象注册表。
///
/// 此层只管理"已注册的设备对象"，不再维护设备号分配或
/// `major/minor -> driver` 映射。
pub struct DeviceList {
    /// 开放设备 function 注册表。
    pub functions: FunctionRegistry,
}

impl DeviceList {
    const fn new() -> Self {
        Self {
            functions: FunctionRegistry::new(),
        }
    }

    pub fn register_function(
        &self,
        func: Arc<dyn DeviceFunction>,
    ) -> Result<(), FunctionRegistryError> {
        self.functions.push(func)
    }

    pub fn unregister_function(&self, func: &Arc<dyn DeviceFunction>) {
        let _ = self.functions.remove(func.class_id(), func.dev_name());
    }
}

pub static DEVICES: DeviceList = DeviceList::new();

pub use crate::dev::pnp::{PNP_DEVICES, PNP_DRIVERS};

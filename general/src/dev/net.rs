//! 网络设备 PnP 集成。
//!
//! 本模块把 [`net::NetDevice`] 适配到 `general` 层的 PnP/`DeviceFunction`
//! 框架，使网络设备能像字符/块设备一样通过 `FunctionRegistry` 统一管理。
//!
//! 与字符/块设备不同，网络设备**不在 `/dev` 下创建节点**——用户进程
//! 通过 `socket()` syscall 访问网络栈。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;

use crate::dev::function::{DeviceClassId, DeviceFunction, DevNodeSpec};

/// 网络设备 function 类别 ID。
pub const NET_CLASS: DeviceClassId = DeviceClassId::new("net");

/// 把 [`net::NetDevice`] 适配为通用 [`DeviceFunction`]。
pub struct NetFunction {
    dev_name: Box<str>,
    dev: Arc<net::NetDevice>,
}

impl NetFunction {
    pub fn new(dev_name: &str, dev: Arc<net::NetDevice>) -> Self {
        Self {
            dev_name: dev_name.into(),
            dev,
        }
    }

    pub fn net_device(&self) -> &Arc<net::NetDevice> {
        &self.dev
    }
}

impl DeviceFunction for NetFunction {
    fn class_id(&self) -> DeviceClassId {
        NET_CLASS
    }

    fn dev_name(&self) -> &str {
        &self.dev_name
    }

    fn mark_gone(&self) {
        self.dev.mark_gone();
    }

    fn devnode(&self) -> Option<DevNodeSpec> {
        None
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

//! 网络设备的 PnP function 投影。

use alloc::boxed::Box;
use core::any::Any;

use crate::dev::function::{DeviceClassId, DeviceFunction};

pub const NET_CLASS: DeviceClassId = DeviceClassId::new("net");

pub struct NetFunction {
    dev_name: Box<str>,
}

impl NetFunction {
    pub fn new(dev_name: &str) -> Self {
        Self {
            dev_name: dev_name.into(),
        }
    }
}

impl DeviceFunction for NetFunction {
    fn class_id(&self) -> DeviceClassId {
        NET_CLASS
    }

    fn dev_name(&self) -> &str {
        &self.dev_name
    }

    fn mark_gone(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

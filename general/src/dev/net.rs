//! 网络设备 PnP 集成。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;

use crate::dev::control::{ControlError, DriverControl, NetControlRequest, NetControlResponse};
use crate::dev::function::{DeviceClassId, DeviceFunction};

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

    pub fn control(&self, req: NetControlRequest) -> Result<NetControlResponse, ControlError> {
        control_net_device(&self.dev, req)
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

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl DriverControl for NetFunction {
    type Request = NetControlRequest;
    type Response = NetControlResponse;
    type Error = ControlError;

    fn control(&self, req: Self::Request) -> Result<Self::Response, Self::Error> {
        NetFunction::control(self, req)
    }
}

impl DriverControl for Arc<net::NetDevice> {
    type Request = NetControlRequest;
    type Response = NetControlResponse;
    type Error = ControlError;

    fn control(&self, req: Self::Request) -> Result<Self::Response, Self::Error> {
        control_net_device(self, req)
    }
}

fn control_net_device(
    dev: &Arc<net::NetDevice>,
    req: NetControlRequest,
) -> Result<NetControlResponse, ControlError> {
    if !dev.is_active() {
        return Err(ControlError::NoDevice);
    }

    match req {
        NetControlRequest::GetInterfaceId => Ok(NetControlResponse::U32(dev.id().raw())),
        NetControlRequest::GetName => Ok(NetControlResponse::Name(dev.name().into())),
        NetControlRequest::GetMedium => Ok(NetControlResponse::Medium(dev.driver().medium())),
        NetControlRequest::GetLinkState => {
            Ok(NetControlResponse::LinkState(dev.driver().link_state()))
        }
        NetControlRequest::GetMacAddress => {
            Ok(NetControlResponse::MacAddress(dev.driver().mac_address()))
        }
        NetControlRequest::GetMtu => Ok(NetControlResponse::Usize(dev.mtu())),
        NetControlRequest::GetTxDropped => Ok(NetControlResponse::U64(dev.tx_dropped())),
        NetControlRequest::GetStats => Ok(NetControlResponse::Stats(dev.driver().stats())),
        NetControlRequest::SetMtu { mtu } => {
            // MTU 由 NetDevice 保存为软件上限，驱动仍只声明硬件能力。
            dev.set_mtu(mtu).map_err(map_net_control_error)?;
            Ok(NetControlResponse::Done)
        }
        NetControlRequest::SetAdminUp { up } => {
            // 管理启停属于协议栈内的接口状态，不直接改写驱动硬件寄存器。
            // ioctl 层只负责把兼容 flags 翻译为这个布尔语义。
            net::stack()
                .set_iface_admin_up(dev.id(), up)
                .map_err(map_net_control_error)?;
            Ok(NetControlResponse::Done)
        }
    }
}

fn map_net_control_error(err: net::NetError) -> ControlError {
    match err {
        net::NetError::InterfaceNotFound => ControlError::NoDevice,
        net::NetError::InvalidArgument => ControlError::Invalid,
        net::NetError::ResourceExhausted => ControlError::Busy,
        net::NetError::LinkDown
        | net::NetError::TimedOut
        | net::NetError::ConnectionReset
        | net::NetError::Unreachable
        | net::NetError::Closed => ControlError::Io,
        net::NetError::InterfaceExists | net::NetError::AddressInUse => ControlError::Busy,
        net::NetError::ConnectionRefused
        | net::NetError::WouldBlock
        | net::NetError::BufferTooSmall => ControlError::Invalid,
    }
}

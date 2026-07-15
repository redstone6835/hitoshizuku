//! 网络对象的启动期身份。

/// 网络设备在一次启动内的稳定编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct NetDeviceId(pub u32);

impl NetDeviceId {
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// 同一设备内 queue pair 的稳定编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct QueuePairId(pub u16);

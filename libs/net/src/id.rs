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

/// 网络接口在配置快照中的稳定编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct InterfaceId(pub u32);

/// 协议执行分片编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct ShardId(pub u16);

/// 一次启动内不复用的监听组编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct ListenGroupId(pub u64);

/// 单个分片内可稳定定位的流编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct FlowId(pub u32);

/// 一次启动内永不复用的 socket 身份。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SocketId {
    pub boot_nonce: u64,
    pub counter: u64,
}

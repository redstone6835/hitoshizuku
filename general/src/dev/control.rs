//! 类型安全的设备控制接口。
//!
//! `ioctl` 只应该存在于 VFS/ABI 适配层；驱动层通过这里的 typed
//! request/response 表达控制动作，避免底层驱动解析用户指针或 ioctl number。

use alloc::boxed::Box;

use errno::Errno;

/// 类型安全的设备控制接口（字符设备与块设备共用）。
///
/// 每种驱动自行定义 `Request`、`Response`、`Error` 关联类型，不使用中心化
/// “所有驱动命令”枚举。编译器在调用端即可验证请求与响应类型匹配。
pub trait DriverControl {
    /// 控制请求类型（每种驱动独立定义）。
    type Request;
    /// 控制响应类型。
    type Response;
    /// 控制错误类型。
    type Error;

    /// 发送一条控制请求并返回响应。
    fn control(&self, req: Self::Request) -> Result<Self::Response, Self::Error>;
}

/// 设备类 control 的通用错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlError {
    Unsupported,
    Invalid,
    NoDevice,
    Busy,
    Io,
    Permission,
}

impl ControlError {
    pub const fn to_errno(self) -> Errno {
        match self {
            Self::Unsupported => Errno::ENOTTY,
            Self::Invalid => Errno::EINVAL,
            Self::NoDevice => Errno::ENODEV,
            Self::Busy => Errno::EBUSY,
            Self::Io => Errno::EIO,
            Self::Permission => Errno::EPERM,
        }
    }
}

/// 字符设备类的通用控制请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharControlRequest {
    FlushTx,
    /// TODO(char-control): 需要字符驱动暴露接收 FIFO/line discipline 清空能力后完整实现。
    FlushRx,
    FlushBoth,
    /// 配置串口类硬件。`baud == None` 表示调用方只同步其它行规程状态。
    SetSerialConfig {
        baud: Option<u32>,
    },
    /// TODO(char-control): 需要底层驱动提供 break 条件保持时间与发送完成语义。
    SendBreak,
    GetInputQueueLen,
    GetOutputQueueLen,
}

/// 字符设备类的通用控制响应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharControlResponse {
    Done,
    U32(u32),
}

/// 块设备 I/O hint 的 typed 表示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockIoHints {
    pub min_io_size: u32,
    pub optimal_io_size: u32,
    pub alignment_offset: i32,
    pub discard_zeroes: bool,
    pub rotational: bool,
}

/// 块设备类的通用控制请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockControlRequest {
    GetReadOnly,
    GetCapacityBytes,
    GetLogicalBlockSize,
    GetPhysicalBlockSize,
    GetIoHints,
    /// TODO(block-control): 只有提供稳定 diskseq 的设备才应返回成功。
    GetDiskSeq,
    Flush,
}

/// 块设备类的通用控制响应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockControlResponse {
    Done,
    Bool(bool),
    U32(u32),
    U64(u64),
    IoHints(BlockIoHints),
}

/// 网络设备类的通用控制请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetControlRequest {
    GetInterfaceId,
    GetName,
    GetMedium,
    GetLinkState,
    GetMacAddress,
    GetMtu,
    GetTxDropped,
    GetStats,
    /// TODO(net-control): 底层 [`net::NetDriver`] 还没有运行期 MTU 修改契约。
    SetMtu {
        mtu: usize,
    },
    /// TODO(net-control): 需要先定义 flags 与协议栈/驱动状态同步规则。
    SetFlags {
        flags: u32,
    },
    /// TODO(net-control): 需要先定义 MAC 地址修改对 ARP/NDP cache 的失效语义。
    SetMacAddress {
        mac: [u8; 6],
    },
}

/// 网络设备类的通用控制响应。
#[derive(Debug, Clone)]
pub enum NetControlResponse {
    Done,
    U32(u32),
    U64(u64),
    Usize(usize),
    Name(Box<str>),
    Medium(net::LinkMedium),
    LinkState(net::LinkState),
    MacAddress([u8; 6]),
    Stats(net::NetStats),
}

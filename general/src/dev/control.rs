//! 类型安全的设备控制接口。
//!
//! `ioctl` 只应该存在于 VFS/ABI 适配层；驱动层通过这里的 typed
//! request/response 表达控制动作，避免底层驱动解析用户指针或 ioctl number。

#[cfg(feature = "block-profile")]
use alloc::string::String;

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

/// 字符设备类的通用控制请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharControlRequest {
    /// 等待已经提交到设备的输出全部发送完成。
    DrainTx,
    /// 丢弃尚未发送完成的输出队列。
    FlushTx,
    /// 丢弃尚未被上层消费的输入队列。
    FlushRx,
    /// 同时丢弃输入队列和输出队列。
    FlushBoth,
    /// 配置串口类硬件。`baud == None` 表示调用方只同步其它行规程状态。
    SetSerialConfig {
        baud: Option<u32>,
    },
    /// 让发送线进入 break 条件并保持指定时长。
    SendBreak {
        duration_ms: u32,
    },
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
    #[cfg(feature = "block-profile")]
    GetDebugProfile,
    /// 返回块设备对象的稳定实例序列号。
    GetDiskSeq,
    Flush,
}

/// 块设备类的通用控制响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockControlResponse {
    Done,
    Bool(bool),
    U32(u32),
    U64(u64),
    IoHints(BlockIoHints),
    #[cfg(feature = "block-profile")]
    DebugText(String),
}

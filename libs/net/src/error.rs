//! 网络子系统统一错误类型。
//!
//! 所有网络层操作（接口管理、socket I/O、协议栈内部）最终归结为
//! [`NetError`] 变体。设计上覆盖 POSIX errno 语义，使 syscall 层
//! 可以无歧义地映射到 `ECONNREFUSED`、`ETIMEDOUT` 等标准错误码。

/// 网络子系统错误。
///
/// 每个变体对应一种可恢复或不可恢复的网络错误状态。
/// 新增错误类型只需在此枚举加变体，不影响已有匹配分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    /// 指定的网络接口不存在（已被 detach 或从未 attach）。
    InterfaceNotFound,
    /// 接口 ID 冲突（重复 attach 同一设备）。
    InterfaceExists,
    /// 物理链路未就绪（网线未插、驱动未 link up）。
    LinkDown,
    /// 远端主动拒绝连接（收到 TCP RST / ICMP port unreachable）。
    ConnectionRefused,
    /// 操作超时（TCP 重传耗尽、connect 超时等）。
    TimedOut,
    /// 本地地址/端口已被占用。
    AddressInUse,
    /// 操作会阻塞但调用方要求非阻塞（EAGAIN / EWOULDBLOCK）。
    WouldBlock,
    /// 连接被远端重置（收到 RST）。
    ConnectionReset,
    /// 无路由到达目标地址。
    Unreachable,
    /// Socket 已关闭（本端或对端 shutdown）。
    Closed,
    /// 提供的缓冲区不足以容纳数据。
    BufferTooSmall,
    /// 参数无效（端口为 0、地址格式错误等）。
    InvalidArgument,
    /// 内部资源耗尽（socket 表满、内存不足等）。
    ResourceExhausted,
}

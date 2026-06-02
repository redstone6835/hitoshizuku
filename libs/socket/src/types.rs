//! 套接字公共类型定义。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

/// 套接字操作错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketError {
    /// 操作不支持
    Unsupported,
    /// 地址族不支持
    UnsupportedAddressSpace,
    /// 套接字类型不支持
    UnsupportedType,
    /// 参数无效
    InvalidInput,
    /// 绑定名称过长
    NameTooLong,
    /// 名称已被绑定
    NameAlreadyBound,
    /// 名称不可用
    NameUnavailable,
    /// 当前状态不允许此操作
    StateMismatch,
    /// 已建立连接,不可重复连接
    AlreadyConnected,
    /// 未建立连接
    ConnectionMissing,
    /// 需要先调用 listen
    ListenerRequired,
    /// 需要指定目标地址
    DestinationRequired,
    /// 资源暂时不可用(EAGAIN)
    TemporaryUnavailable,
    /// 被信号中断(EINTR)
    Interrupted,
    /// 对端已关闭
    PeerClosed,
    /// 连接被拒绝
    ConnectionRejected,
    /// 消息载荷超过上限
    PayloadTooLarge,
    /// 资源耗尽
    ResourceExhausted,
    /// 权限不足
    AccessDenied,
}

/// 套接字类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketType {
    /// 面向连接的字节流(SOCK_STREAM)
    Stream,
    /// 无连接的数据报(SOCK_DGRAM)
    Datagram,
    /// 面向连接的有序报文(SOCK_SEQPACKET)
    Sequenced,
}

/// shutdown 方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketShutdown {
    /// 关闭读端(SHUT_RD)
    Read,
    /// 关闭写端(SHUT_WR)
    Write,
    /// 同时关闭读写(SHUT_RDWR)
    Both,
}

/// 就绪状态位掩码,用于 poll/epoll 事件通知。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Readiness(u8);

impl Readiness {
    /// 可读(有数据待接收或有待 accept 的连接)
    pub const READABLE: Self = Self(1 << 0);
    /// 可写(发送缓冲区有空间)
    pub const WRITABLE: Self = Self(1 << 1);
    /// 挂起(对端关闭或本端 shutdown)
    pub const HANGUP: Self = Self(1 << 2);
    /// 错误(对端读端已关)
    pub const FAULT: Self = Self(1 << 3);

    /// 空就绪集合
    pub const fn empty() -> Self {
        Self(0)
    }

    /// 合并两个就绪标志
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// 检测是否包含指定标志
    pub const fn has(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    /// 返回原始位表示
    pub const fn bits(self) -> u8 {
        self.0
    }
}

/// 对端身份凭证,用于 SCM_CREDENTIALS 辅助消息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerIdentity {
    /// 进程 PID
    pub process: u32,
    /// 用户 UID
    pub user: u32,
    /// 组 GID
    pub group: u32,
}

/// 可通过套接字传递的内核句柄(如文件描述符)。
pub trait SocketHandle: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;
}

/// 共享句柄的引用计数指针。
pub type SharedHandle = Arc<dyn SocketHandle>;

/// 路径绑定的唯一标识(文件系统号 + inode 号)。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathKey {
    /// 文件系统标识
    pub fs: u64,
    /// inode 编号
    pub ino: u64,
}

/// 注册表查找键:抽象名称或路径。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BindingKey {
    /// 抽象命名空间(以 NUL 开头的名称)
    Abstract(Vec<u8>),
    /// 文件系统路径绑定
    Path(PathKey),
}

/// Unix 域套接字地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnixAddress {
    /// 未命名(匿名)套接字
    Unnamed,
    /// 抽象命名空间地址
    Abstract(Vec<u8>),
    /// 文件系统路径地址
    Path { key: PathKey, display: Vec<u8> },
}

impl UnixAddress {
    /// 转换为注册表查找键(Unnamed 无法查找)。
    pub(crate) fn binding_key(&self) -> Option<BindingKey> {
        match self {
            Self::Unnamed => None,
            Self::Abstract(name) => Some(BindingKey::Abstract(name.clone())),
            Self::Path { key, .. } => Some(BindingKey::Path(key.clone())),
        }
    }
}

/// 发送选项。
#[derive(Clone, Copy, Debug, Default)]
pub struct SendOptions {
    /// 非阻塞模式
    pub nonblocking: bool,
    /// 显式指定发送方身份(用于 SO_PASSCRED)
    pub sender_identity: Option<PeerIdentity>,
    /// 是否在辅助消息中附带凭证
    pub explicit_credentials: bool,
    /// 标记消息记录结束(用于 SEQPACKET)
    pub end_of_record: bool,
    /// 超时截止时间(纳秒级绝对时间戳)
    pub deadline_ns: Option<u64>,
}

/// 接收选项。
#[derive(Clone, Copy, Debug, Default)]
pub struct ReceiveOptions {
    /// 非阻塞模式
    pub nonblocking: bool,
    /// 窥视模式:读取但不消费数据
    pub peek: bool,
    /// 阻塞直到缓冲区填满(MSG_WAITALL)
    pub wait_all: bool,
    /// 超时截止时间(纳秒级绝对时间戳)
    pub deadline_ns: Option<u64>,
}

/// 接收操作返回结果。
pub struct ReceiveResult {
    /// 实际拷贝的字节数
    pub length: usize,
    /// 发送方地址(数据报或 SEQPACKET 可能携带)
    pub sender: Option<UnixAddress>,
    /// 发送方凭证(启用 SO_PASSCRED 时填充)
    pub sender_identity: Option<PeerIdentity>,
    /// 随消息传递的句柄(文件描述符等)
    pub handles: Vec<SharedHandle>,
    /// 数据报被截断(用户缓冲区不足)
    pub data_truncated: bool,
}

/// SO_SNDTIMEO / SO_RCVTIMEO 超时值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketTimeval {
    pub secs: i64,
    pub micros: i64,
}

/// SO_LINGER 选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketLinger {
    /// 是否启用 linger
    pub enabled: bool,
    /// linger 超时秒数
    pub seconds: u32,
}

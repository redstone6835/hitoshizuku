//! 内核与 elm-mgr 用户态管理工具之间的顶层控制协议。
//!
//! `ElmCtlHeader` 包围 core query、管理调用、事件读取/确认、快照和 debug dump。它只负责
//! 选择控制命令和描述输入/输出缓冲区，不直接定义每个 elm-mgr 动作的 payload；具体管理
//! 请求和回复由 [`crate::management`] 以及 `ElmMgr*` 固定布局类型定义。
//!
//! 用户态必须先校验 magic、ABI 版本、命令和长度。内核返回的 [`ElmCtlStatus`] 只描述控制
//! 传输层结果，管理动作自身状态仍位于管理回复头中。

use crate::error::ElmError;

/// `ELM_CTL_MAGIC` 的固定魔数；解析器必须先校验该值，再解释后续布局。
pub const ELM_CTL_MAGIC: u32 = 0x314d_4c45;
/// `ELM_CTL_ABI_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_CTL_ABI_VERSION: u16 = 1;

/// `ELM_CORE_CAP_SNAPSHOT` 能力位；协商成功前调用方不得假定对应功能可用。
pub const ELM_CORE_CAP_SNAPSHOT: u64 = 1 << 0;
/// `ELM_CORE_CAP_EVENTS` 能力位；协商成功前调用方不得假定对应功能可用。
pub const ELM_CORE_CAP_EVENTS: u64 = 1 << 1;
/// `ELM_CORE_CAP_MGR_CHANNEL` 能力位；协商成功前调用方不得假定对应功能可用。
pub const ELM_CORE_CAP_MGR_CHANNEL: u64 = 1 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// `ElmCtlCommand` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmCtlCommand {
    /// `CoreQuery` 表示 `ElmCtlCommand` 的控制命令：`core query`。
    CoreQuery = 1,
    /// `MgrCall` 表示 `ElmCtlCommand` 的控制命令：`mgr call`。
    MgrCall = 2,
    /// `EventRead` 表示 `ElmCtlCommand` 的控制命令：`event read`。
    EventRead = 3,
    /// `EventAck` 表示 `ElmCtlCommand` 的控制命令：`event ack`。
    EventAck = 4,
    /// `SnapshotRead` 表示 `ElmCtlCommand` 的控制命令：`snapshot read`。
    SnapshotRead = 5,
    /// `DebugDump` 表示 `ElmCtlCommand` 的控制命令：`debug dump`。
    DebugDump = 6,
}

impl ElmCtlCommand {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
    pub const fn from_raw(raw: usize) -> Option<Self> {
        match raw {
            1 => Some(Self::CoreQuery),
            2 => Some(Self::MgrCall),
            3 => Some(Self::EventRead),
            4 => Some(Self::EventAck),
            5 => Some(Self::SnapshotRead),
            6 => Some(Self::DebugDump),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
/// `ElmCtlStatus` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmCtlStatus {
    /// `Ok` 表示 `ElmCtlStatus` 的结果状态：`ok`。
    Ok = 0,
    /// `Permission` 表示 `ElmCtlStatus` 的结果状态：`permission`。
    Permission = -1,
    /// `NotFound` 表示 `ElmCtlStatus` 的结果状态：`not found`。
    NotFound = -2,
    /// `Invalid` 表示 `ElmCtlStatus` 的结果状态：`invalid`。
    Invalid = -22,
    /// `Busy` 表示 `ElmCtlStatus` 的结果状态：`busy`。
    Busy = -16,
    /// `NoMemory` 表示 `ElmCtlStatus` 的结果状态：`no memory`。
    NoMemory = -12,
    /// `MessageTooLarge` 表示 `ElmCtlStatus` 的结果状态：`message too large`。
    MessageTooLarge = -90,
    /// `Unsupported` 表示 `ElmCtlStatus` 的结果状态：`unsupported`。
    Unsupported = -95,
}

impl ElmCtlStatus {
    /// 把框架内部错误映射为该协议层可传输的稳定状态。
    pub const fn from_error(error: &ElmError) -> Self {
        match error {
            ElmError::CellNotFound
            | ElmError::PortNotFound
            | ElmError::BindingNotFound
            | ElmError::ExtensionPointNotFound => Self::NotFound,
            ElmError::LeaseBusy => Self::Busy,
            ElmError::PermissionDenied => Self::Permission,
            _ => Self::Invalid,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmCtlHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmCtlHeader {
    /// 识别该线格式的固定魔数。
    pub magic: u32,
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `command` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub command: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 输入缓冲区中可读取的有效字节数。
    pub input_len: u32,
    /// 输出缓冲区中已写入或调用方所需的字节数。
    pub output_len: u32,
    /// 单调递增的序列号，用于排序、游标推进和丢失检测。
    pub sequence: u64,
}

impl ElmCtlHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(command: ElmCtlCommand, input_len: u32, output_len: u32) -> Self {
        Self {
            magic: ELM_CTL_MAGIC,
            abi_version: ELM_CTL_ABI_VERSION,
            command: command as u16,
            flags: 0,
            input_len,
            output_len,
            sequence: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// core query 返回的 ABI、能力、对象计数和当前事件序列摘要。
pub struct ElmCoreInfo {
    /// 识别该线格式的固定魔数。
    pub magic: u32,
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `core_version` 是该对象、ABI 或契约的版本值，用于装载和协商兼容性。
    pub core_version: u16,
    /// 协商得到的能力位集合；调用可选入口前必须先检查对应位。
    pub capabilities: u64,
    /// `cell_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub cell_count: u32,
    /// `port_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub port_count: u32,
    /// `lease_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub lease_count: u32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
}

impl ElmCoreInfo {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(
        cell_count: u32,
        port_count: u32,
        lease_count: u32,
        event_sequence: u64,
    ) -> Self {
        Self {
            magic: ELM_CTL_MAGIC,
            abi_version: ELM_CTL_ABI_VERSION,
            core_version: 1,
            capabilities: ELM_CORE_CAP_SNAPSHOT | ELM_CORE_CAP_EVENTS | ELM_CORE_CAP_MGR_CHANNEL,
            cell_count,
            port_count,
            lease_count,
            event_sequence,
        }
    }
}

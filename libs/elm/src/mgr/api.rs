//! `elm-mgr` 对外 API 与事件订阅协议。
//!
//! 本模块只定义稳定的 Rust ABI 固定布局，并作为 `elm` crate 的私有实现细节存在。
//! 对外管理操作统一通过 `elm::management::Client`，普通运行时操作统一通过
//! `elm::runtime`；这里描述的是 ELM 运行时自身能力，不是访问 VFS、调度、内存分配
//! 等子系统的唯一入口。

use crate::ctl::ELM_CTL_ABI_VERSION;
use crate::event::ElmEventRecord;

/// `ELM_MGR_API_NAMESPACE_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_MGR_API_NAMESPACE_LEN: usize = 32;
/// `ELM_MGR_API_NAME_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_MGR_API_NAME_LEN: usize = 48;
/// `ELM_MGR_API_CONTRACT_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_MGR_API_CONTRACT_LEN: usize = 48;

/// `ELM_MGR_API_KIND_CONTROL` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_MGR_API_KIND_CONTROL: u32 = 1;
/// `ELM_MGR_API_KIND_SNAPSHOT` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_MGR_API_KIND_SNAPSHOT: u32 = 2;
/// `ELM_MGR_API_KIND_EVENT` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_MGR_API_KIND_EVENT: u32 = 3;
/// `ELM_MGR_API_KIND_PROVIDER` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_MGR_API_KIND_PROVIDER: u32 = 4;
/// `ELM_MGR_API_KIND_SUBSYSTEM` 稳定类别编号，用于在线格式中区分对应记录或对象。
pub const ELM_MGR_API_KIND_SUBSYSTEM: u32 = 5;

/// `ELM_MGR_API_FLAG_STABLE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_MGR_API_FLAG_STABLE: u32 = 1 << 0;
/// `ELM_MGR_API_FLAG_TODO` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_MGR_API_FLAG_TODO: u32 = 1 << 1;
/// `ELM_MGR_API_FLAG_SYSCALL` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_MGR_API_FLAG_SYSCALL: u32 = 1 << 2;
/// `ELM_MGR_API_FLAG_SYSFS` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_MGR_API_FLAG_SYSFS: u32 = 1 << 3;
/// `ELM_MGR_API_FLAG_PROVIDER_OPS` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_MGR_API_FLAG_PROVIDER_OPS: u32 = 1 << 4;

/// `ELM_RUNTIME_LOG_EXPORT_NAME` 的规范 identifier 或契约名称；比较时使用完整字节串而不是截断哈希。
pub const ELM_RUNTIME_LOG_EXPORT_NAME: &str = "elm.runtime.log";
/// `ELM_RUNTIME_LOG_EXPORT_CONTRACT` 的规范 identifier 或契约名称；比较时使用完整字节串而不是截断哈希。
pub const ELM_RUNTIME_LOG_EXPORT_CONTRACT: &str = "elm.runtime.log@1";
/// `ELM_RUNTIME_LOG_EXPORT_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_RUNTIME_LOG_EXPORT_VERSION: u32 = 1;

/// 事件订阅中过滤全部事件 kind 的通配值。
pub const ELM_MGR_EVENT_FILTER_ANY: u32 = 0;
/// `ELM_MGR_EVENT_SUBSCRIPTION_FLAG_ACTIVE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_MGR_EVENT_SUBSCRIPTION_FLAG_ACTIVE: u32 = 1 << 0;
/// `ELM_MGR_EVENT_READ_FLAG_ADVANCE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_MGR_EVENT_READ_FLAG_ADVANCE: u32 = 1 << 0;
/// `ELM_MGR_EVENT_READ_DEFAULT_MAX_RECORDS` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_MGR_EVENT_READ_DEFAULT_MAX_RECORDS: u32 = 32;
/// `ELM_MGR_EVENT_READ_ABSOLUTE_MAX_RECORDS` 当前 ABI 允许的硬上限；构造器和解析器必须在分配或复制前检查该限制。
pub const ELM_MGR_EVENT_READ_ABSOLUTE_MAX_RECORDS: u32 = 64;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMgrApiRegistryHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmMgrApiRegistryHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: u64,
}

impl ElmMgrApiRegistryHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(record_count: u32, generation: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmMgrApiDescriptor>() as u16,
            record_count,
            flags: 0,
            reserved: 0,
            generation,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// elm-mgr 注册表中的一个 API 名称、命名空间、契约、kind、版本和 flags 描述。
pub struct ElmMgrApiDescriptor {
    /// 该对象在所属表或运行时注册表中的稳定标识符。
    pub id: u64,
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// 该记录、资源或关系的类别编码。
    pub kind: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `call_kind` 是所属枚举的稳定判别值；未知值必须拒绝。
    pub call_kind: u32,
    /// `min_abi_version` 是该对象、ABI 或契约的版本值，用于装载和协商兼容性。
    pub min_abi_version: u16,
    /// `current_abi_version` 是该对象、ABI 或契约的版本值，用于装载和协商兼容性。
    pub current_abi_version: u16,
    /// `namespace_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub namespace_len: u16,
    /// `name_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub name_len: u16,
    /// `contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub contract_len: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// 协商得到的能力位集合；调用可选入口前必须先检查对应位。
    pub capabilities: u64,
    /// API 或能力所在的命名空间 identifier。
    pub namespace: [u8; ELM_MGR_API_NAMESPACE_LEN],
    /// 对象的固定长度名称缓冲区；实际字符串以首个零字节结束。
    pub name: [u8; ELM_MGR_API_NAME_LEN],
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: [u8; ELM_MGR_API_CONTRACT_LEN],
}

impl ElmMgrApiDescriptor {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub fn new(
        id: u64,
        owner_cell_id: u64,
        kind: u32,
        flags: u32,
        call_kind: u32,
        namespace: &str,
        name: &str,
        contract: &str,
    ) -> Self {
        let mut out = Self {
            id,
            owner_cell_id,
            kind,
            flags,
            call_kind,
            min_abi_version: ELM_CTL_ABI_VERSION,
            current_abi_version: ELM_CTL_ABI_VERSION,
            namespace_len: 0,
            name_len: 0,
            contract_len: 0,
            reserved0: 0,
            capabilities: 0,
            namespace: [0; ELM_MGR_API_NAMESPACE_LEN],
            name: [0; ELM_MGR_API_NAME_LEN],
            contract: [0; ELM_MGR_API_CONTRACT_LEN],
        };
        out.namespace_len = copy_str(namespace, &mut out.namespace) as u16;
        out.name_len = copy_str(name, &mut out.name) as u16;
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out
    }

    /// 设置 `capabilities` 并返回更新后的值，便于构建器式初始化。
    pub const fn with_capabilities(mut self, capabilities: u64) -> Self {
        self.capabilities = capabilities;
        self
    }
}

/// `ElmMgrApiDescriptorRecord` 为该调用路径使用的规范类型别名，统一公开签名并避免重复表达底层布局。
pub type ElmMgrApiDescriptorRecord = ElmMgrApiDescriptor;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMgrEventSubscribeRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmMgrEventSubscribeRequest {
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// `kind_filter` 限制查询返回的记录范围；零值通常表示不过滤。
    pub kind_filter: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `cell_filter` 限制查询返回的记录范围；零值通常表示不过滤。
    pub cell_filter: u64,
    /// `port_filter` 限制查询返回的记录范围；零值通常表示不过滤。
    pub port_filter: u64,
    /// `binding_filter` 限制查询返回的记录范围；零值通常表示不过滤。
    pub binding_filter: u64,
    /// `lease_filter` 限制查询返回的记录范围；零值通常表示不过滤。
    pub lease_filter: u64,
}

impl ElmMgrEventSubscribeRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(owner_cell_id: u64) -> Self {
        Self {
            owner_cell_id,
            kind_filter: ELM_MGR_EVENT_FILTER_ANY,
            flags: 0,
            cell_filter: 0,
            port_filter: 0,
            binding_filter: 0,
            lease_filter: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMgrEventSubscribeResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmMgrEventSubscribeResponse {
    /// `subscription_id` 所指对象的稳定运行时标识符。
    pub subscription_id: u64,
    /// 保护对应调用或资源生命周期的租约标识符。
    pub lease_id: u64,
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// 分页或事件读取游标；其语义由对应请求类型定义。
    pub cursor: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `dropped_events` 保存所属对象声明或快照中的有序记录集合。
    pub dropped_events: u64,
}

impl ElmMgrEventSubscribeResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(
        subscription_id: u64,
        lease_id: u64,
        owner_cell_id: u64,
        cursor: u64,
        status: i32,
        dropped_events: u64,
    ) -> Self {
        Self {
            subscription_id,
            lease_id,
            owner_cell_id,
            cursor,
            status,
            flags: if status == 0 {
                ELM_MGR_EVENT_SUBSCRIPTION_FLAG_ACTIVE
            } else {
                0
            },
            dropped_events,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMgrEventUnsubscribeRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmMgrEventUnsubscribeRequest {
    /// `subscription_id` 所指对象的稳定运行时标识符。
    pub subscription_id: u64,
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmMgrEventUnsubscribeRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(subscription_id: u64, owner_cell_id: u64) -> Self {
        Self {
            subscription_id,
            owner_cell_id,
            flags: 0,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMgrEventUnsubscribeResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmMgrEventUnsubscribeResponse {
    /// `subscription_id` 所指对象的稳定运行时标识符。
    pub subscription_id: u64,
    /// 保护对应调用或资源生命周期的租约标识符。
    pub lease_id: u64,
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// `revoked` 表示该条件在当前快照或计划中是否成立。
    pub revoked: u32,
    /// `delivered_events` 保存所属对象声明或快照中的有序记录集合。
    pub delivered_events: u64,
    /// `dropped_events` 保存所属对象声明或快照中的有序记录集合。
    pub dropped_events: u64,
}

impl ElmMgrEventUnsubscribeResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(
        subscription_id: u64,
        lease_id: u64,
        owner_cell_id: u64,
        status: i32,
        revoked: bool,
        delivered_events: u64,
        dropped_events: u64,
    ) -> Self {
        Self {
            subscription_id,
            lease_id,
            owner_cell_id,
            status,
            revoked: if revoked { 1 } else { 0 },
            delivered_events,
            dropped_events,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMgrEventSubscriptionHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmMgrEventSubscriptionHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
}

impl ElmMgrEventSubscriptionHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(record_count: u32, event_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmMgrEventSubscriptionRecord>() as u16,
            record_count,
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMgrEventSubscriptionRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmMgrEventSubscriptionRecord {
    /// `subscription_id` 所指对象的稳定运行时标识符。
    pub subscription_id: u64,
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// 保护对应调用或资源生命周期的租约标识符。
    pub lease_id: u64,
    /// 分页或事件读取游标；其语义由对应请求类型定义。
    pub cursor: u64,
    /// `kind_filter` 限制查询返回的记录范围；零值通常表示不过滤。
    pub kind_filter: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `cell_filter` 限制查询返回的记录范围；零值通常表示不过滤。
    pub cell_filter: u64,
    /// `port_filter` 限制查询返回的记录范围；零值通常表示不过滤。
    pub port_filter: u64,
    /// `binding_filter` 限制查询返回的记录范围；零值通常表示不过滤。
    pub binding_filter: u64,
    /// `lease_filter` 限制查询返回的记录范围；零值通常表示不过滤。
    pub lease_filter: u64,
    /// `delivered_events` 保存所属对象声明或快照中的有序记录集合。
    pub delivered_events: u64,
    /// `dropped_events` 保存所属对象声明或快照中的有序记录集合。
    pub dropped_events: u64,
}

impl ElmMgrEventSubscriptionRecord {
    #[allow(clippy::too_many_arguments)]
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(
        subscription_id: u64,
        owner_cell_id: u64,
        lease_id: u64,
        cursor: u64,
        kind_filter: u32,
        active: bool,
        cell_filter: u64,
        port_filter: u64,
        binding_filter: u64,
        lease_filter: u64,
        delivered_events: u64,
        dropped_events: u64,
    ) -> Self {
        Self {
            subscription_id,
            owner_cell_id,
            lease_id,
            cursor,
            kind_filter,
            flags: if active {
                ELM_MGR_EVENT_SUBSCRIPTION_FLAG_ACTIVE
            } else {
                0
            },
            cell_filter,
            port_filter,
            binding_filter,
            lease_filter,
            delivered_events,
            dropped_events,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMgrSubscribedEventReadRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmMgrSubscribedEventReadRequest {
    /// `subscription_id` 所指对象的稳定运行时标识符。
    pub subscription_id: u64,
    /// 分页或事件读取游标；其语义由对应请求类型定义。
    pub cursor: u64,
    /// `max_records` 对应资源预算的硬上限；零值语义由所属预算结构定义。
    pub max_records: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
}

impl ElmMgrSubscribedEventReadRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(subscription_id: u64, cursor: u64, max_records: u32) -> Self {
        Self {
            subscription_id,
            cursor,
            max_records,
            flags: ELM_MGR_EVENT_READ_FLAG_ADVANCE,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMgrSubscribedEventReadHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmMgrSubscribedEventReadHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `record_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub record_entry_size: u16,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `subscription_id` 所指对象的稳定运行时标识符。
    pub subscription_id: u64,
    /// 分页或事件读取游标；其语义由对应请求类型定义。
    pub cursor: u64,
    /// 下一页或下一批记录的游标。
    pub next_cursor: u64,
    /// `dropped_events` 保存所属对象声明或快照中的有序记录集合。
    pub dropped_events: u64,
}

impl ElmMgrSubscribedEventReadHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(
        record_count: u32,
        status: i32,
        flags: u32,
        subscription_id: u64,
        cursor: u64,
        next_cursor: u64,
        dropped_events: u64,
    ) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmEventRecord>() as u16,
            record_count,
            status,
            flags,
            subscription_id,
            cursor,
            next_cursor,
            dropped_events,
        }
    }
}

fn copy_str(value: &str, out: &mut [u8]) -> usize {
    let bytes = value.as_bytes();
    let len = core::cmp::min(bytes.len(), out.len());
    out[..len].copy_from_slice(&bytes[..len]);
    len
}

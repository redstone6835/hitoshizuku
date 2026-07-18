//! 用户态和管理 API 使用的固定布局 ELM 运行快照。
//!
//! snapshot 把内部 `String`、枚举和集合投影为显式版本、记录尺寸、固定字符串缓冲区和稳定
//! 数值。它适合系统调用/sysfs/management channel 传输，不暴露内核地址，也不允许用户态
//! 通过修改缓冲区改变运行时对象。
//!
//! 每个 header 都给出记录数量和尺寸；消费者必须验证总长度。cell 记录同时包含状态、信任、
//! 来源、资源预算/用量和生命周期能力，port 记录描述契约、方向、模式和 owner generation。

use crate::ebi::{ElmEbiArch, ElmEbiLoadStatus, ElmEbiSourceKind};
use crate::ids::{ElmId, Generation, PortId};
use crate::manifest::ElmKind;
use crate::nexus::{FlowDirection, FlowMode};
use crate::resource::{ElmResourceBudget, ElmResourceUsage};
use crate::state::ElmState;

/// `ELM_CELL_NAME_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_CELL_NAME_LEN: usize = 64;
/// `ELM_CONTRACT_NAME_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_CONTRACT_NAME_LEN: usize = 64;
/// cell 快照 lifecycle flags 中表示 `hooks_declared` 条件已成立的位。
pub const ELM_CELL_LIFECYCLE_HOOKS_DECLARED: u32 = 1 << 0;
/// cell 快照 lifecycle flags 中表示 `executor_ready` 条件已成立的位。
pub const ELM_CELL_LIFECYCLE_EXECUTOR_READY: u32 = 1 << 1;
/// cell 快照 lifecycle flags 中表示 `initialized` 条件已成立的位。
pub const ELM_CELL_LIFECYCLE_INITIALIZED: u32 = 1 << 2;
/// cell 快照 lifecycle flags 中表示 `finalized` 条件已成立的位。
pub const ELM_CELL_LIFECYCLE_FINALIZED: u32 = 1 << 3;
/// cell 快照中表示 `internal` 信任来源的稳定编码。
pub const ELM_CELL_TRUST_INTERNAL: u32 = 1 << 0;
/// cell 快照中表示 `signed` 信任来源的稳定编码。
pub const ELM_CELL_TRUST_SIGNED: u32 = 1 << 1;
/// cell 快照中表示 `unsigned` 信任来源的稳定编码。
pub const ELM_CELL_TRUST_UNSIGNED: u32 = 1 << 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmSnapshotHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmSnapshotHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `cell_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub cell_entry_size: u16,
    /// `port_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub port_entry_size: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u16,
    /// `cell_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub cell_count: u32,
    /// `port_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub port_count: u32,
    /// `lease_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub lease_count: u32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
}

impl ElmSnapshotHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(
        cell_count: u32,
        port_count: u32,
        lease_count: u32,
        event_sequence: u64,
    ) -> Self {
        Self {
            abi_version: crate::ctl::ELM_CTL_ABI_VERSION,
            cell_entry_size: core::mem::size_of::<ElmCellSnapshot>() as u16,
            port_entry_size: core::mem::size_of::<ElmPortSnapshot>() as u16,
            reserved: 0,
            cell_count,
            port_count,
            lease_count,
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmCellSnapshot` 是某一时刻的只读快照表示，不授予对原对象的所有权或长期引用。
pub struct ElmCellSnapshot {
    /// 该对象在所属表或运行时注册表中的稳定标识符。
    pub id: u64,
    /// 父对象或父 cell 的标识符，用于建立层级关系。
    pub parent: u64,
    /// 对象或单元的当前状态编码。
    pub state: u32,
    /// 该记录、资源或关系的类别编码。
    pub kind: u32,
    /// `ebi_arch` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub ebi_arch: u32,
    /// `ebi_status` 保存所属对象声明或快照中的有序记录集合。
    pub ebi_status: i32,
    /// `native_code` 表示该条件在当前快照或计划中是否成立。
    pub native_code: u32,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u32,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: u64,
    /// `name_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub name_len: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u16,
    /// 对象的固定长度名称缓冲区；实际字符串以首个零字节结束。
    pub name: [u8; ELM_CELL_NAME_LEN],
    /// `ebi_source` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub ebi_source: u32,
    /// `lifecycle_flags` 标志位集合；必须拒绝相应有效掩码之外的未知位。
    pub lifecycle_flags: u32,
    /// `native_segment_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub native_segment_count: u16,
    /// `native_import_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub native_import_count: u16,
    /// `native_export_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub native_export_count: u16,
    /// `native_faults` 保存所属对象声明或快照中的有序记录集合。
    pub native_faults: u16,
    /// `isolated` 表示该条件在当前快照或计划中是否成立。
    pub isolated: u32,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u32,
    /// 触发或维持单元隔离的原因位。
    pub isolation_blocker: u64,
    /// 该回复记录的 `provider ports` 资源预算上限。
    pub budget_max_provider_ports: u16,
    /// 该回复记录的 `provider queue` 资源预算上限。
    pub budget_max_provider_queue: u16,
    /// 该回复记录的 `event subscriptions` 资源预算上限。
    pub budget_max_event_subscriptions: u16,
    /// 该回复记录的 `pending loads` 资源预算上限。
    pub budget_max_pending_loads: u16,
    /// 该回复记录的 `native images` 资源预算上限。
    pub budget_max_native_images: u16,
    /// 该回复记录的 `native faults` 资源预算上限。
    pub budget_max_native_faults: u16,
    /// 该回复记录的 `audit records` 资源预算上限。
    pub budget_max_audit_records: u16,
    /// 该 cell 当前已使用的 `provider ports` 资源量。
    pub usage_provider_ports: u16,
    /// 该 cell 当前已使用的 `provider queue` 资源量。
    pub usage_provider_queue: u16,
    /// 该 cell 当前已使用的 `event subscriptions` 资源量。
    pub usage_event_subscriptions: u16,
    /// 该 cell 当前已使用的 `pending loads` 资源量。
    pub usage_pending_loads: u16,
    /// 该 cell 当前已使用的 `native images` 资源量。
    pub usage_native_images: u16,
    /// 该 cell 当前已使用的 `native faults` 资源量。
    pub usage_native_faults: u16,
    /// 该 cell 当前已使用的 `audit records` 资源量。
    pub usage_audit_records: u16,
    /// `trust_flags` 标志位集合；必须拒绝相应有效掩码之外的未知位。
    pub trust_flags: u32,
    /// `release_epoch` 是单调发布或策略纪元，用于拒绝回滚和陈旧更新。
    pub release_epoch: u64,
    /// `signer_key_id` 所指对象的稳定运行时标识符。
    pub signer_key_id: [u8; 32],
}

impl ElmCellSnapshot {
    #[allow(clippy::too_many_arguments)]
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub fn new(
        id: ElmId,
        parent: Option<ElmId>,
        state: ElmState,
        kind: ElmKind,
        generation: Generation,
        name: &str,
        ebi_arch: ElmEbiArch,
        ebi_status: ElmEbiLoadStatus,
        native_code: bool,
        ebi_source: ElmEbiSourceKind,
        native_segment_count: u16,
        native_import_count: u16,
        native_export_count: u16,
        lifecycle_hooks_declared: bool,
        lifecycle_executor_ready: bool,
        lifecycle_initialized: bool,
        lifecycle_finalized: bool,
        budget: ElmResourceBudget,
        usage: ElmResourceUsage,
        isolated: bool,
        native_faults: u16,
        isolation_blocker: u64,
        trust_unsigned: bool,
        signer_key_id: [u8; 32],
        release_epoch: u64,
    ) -> Self {
        let trust_flags = if trust_unsigned {
            ELM_CELL_TRUST_UNSIGNED
        } else if signer_key_id != [0; 32] {
            ELM_CELL_TRUST_SIGNED
        } else {
            ELM_CELL_TRUST_INTERNAL
        };
        let mut out = Self {
            id: id.0,
            parent: parent.map(|id| id.0).unwrap_or(0),
            state: state_code(state),
            kind: kind_code(kind),
            ebi_arch: ebi_arch as u32,
            ebi_status: ebi_status as i32,
            native_code: u32::from(native_code),
            reserved0: 0,
            generation: generation.0,
            name_len: 0,
            reserved: 0,
            name: [0; ELM_CELL_NAME_LEN],
            ebi_source: ebi_source as u32,
            lifecycle_flags: lifecycle_flags(
                lifecycle_hooks_declared,
                lifecycle_executor_ready,
                lifecycle_initialized,
                lifecycle_finalized,
            ),
            native_segment_count,
            native_import_count,
            native_export_count,
            native_faults,
            isolated: u32::from(isolated),
            reserved1: 0,
            isolation_blocker,
            budget_max_provider_ports: budget.max_provider_ports,
            budget_max_provider_queue: budget.max_provider_queue,
            budget_max_event_subscriptions: budget.max_event_subscriptions,
            budget_max_pending_loads: budget.max_pending_loads,
            budget_max_native_images: budget.max_native_images,
            budget_max_native_faults: budget.max_native_faults,
            budget_max_audit_records: budget.max_audit_records,
            usage_provider_ports: usage.provider_ports,
            usage_provider_queue: usage.provider_queue,
            usage_event_subscriptions: usage.event_subscriptions,
            usage_pending_loads: usage.pending_loads,
            usage_native_images: usage.native_images,
            usage_native_faults: usage.native_faults,
            usage_audit_records: usage.audit_records,
            trust_flags,
            release_epoch,
            signer_key_id,
        };
        let bytes = name.as_bytes();
        let n = bytes.len().min(ELM_CELL_NAME_LEN);
        out.name[..n].copy_from_slice(&bytes[..n]);
        out.name_len = n as u16;
        out
    }
}

const fn lifecycle_flags(
    hooks_declared: bool,
    executor_ready: bool,
    initialized: bool,
    finalized: bool,
) -> u32 {
    (if hooks_declared {
        ELM_CELL_LIFECYCLE_HOOKS_DECLARED
    } else {
        0
    }) | (if executor_ready {
        ELM_CELL_LIFECYCLE_EXECUTOR_READY
    } else {
        0
    }) | (if initialized {
        ELM_CELL_LIFECYCLE_INITIALIZED
    } else {
        0
    }) | (if finalized {
        ELM_CELL_LIFECYCLE_FINALIZED
    } else {
        0
    })
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmPortSnapshot` 是某一时刻的只读快照表示，不授予对原对象的所有权或长期引用。
pub struct ElmPortSnapshot {
    /// 该对象在所属表或运行时注册表中的稳定标识符。
    pub id: u64,
    /// 拥有该对象的 cell id；所有生命周期和权限检查都归属于该 owner。
    pub owner: u64,
    /// 端口的数据流方向编码。
    pub direction: u32,
    /// 端口、绑定或扩展点采用的并发/分发模式编码。
    pub mode: u32,
    /// `implemented` 表示该条件在当前快照或计划中是否成立。
    pub implemented: u32,
    /// `contract_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub contract_len: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u16,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: [u8; ELM_CONTRACT_NAME_LEN],
}

impl ElmPortSnapshot {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub fn new(
        id: PortId,
        owner: Option<ElmId>,
        contract: &str,
        direction: FlowDirection,
        mode: FlowMode,
        implemented: bool,
    ) -> Self {
        let mut out = Self {
            id: id.0,
            owner: owner.map(|id| id.0).unwrap_or(0),
            direction: direction_code(direction),
            mode: mode_code(mode),
            implemented: u32::from(implemented),
            contract_len: 0,
            reserved: 0,
            contract: [0; ELM_CONTRACT_NAME_LEN],
        };
        let bytes = contract.as_bytes();
        let n = bytes.len().min(ELM_CONTRACT_NAME_LEN);
        out.contract[..n].copy_from_slice(&bytes[..n]);
        out.contract_len = n as u16;
        out
    }
}

/// 把 cell 状态转换为当前 ABI 的稳定数值编码。
pub const fn state_code(state: ElmState) -> u32 {
    match state {
        ElmState::Discovered => 1,
        ElmState::Verified => 2,
        ElmState::Loaded => 3,
        ElmState::Linked => 4,
        ElmState::Ready => 5,
        ElmState::Active => 6,
        ElmState::Quiescing => 7,
        ElmState::Paused => 8,
        ElmState::Detached => 9,
        ElmState::Retired => 10,
        ElmState::Faulted => 11,
        ElmState::Quarantined => 12,
    }
}

/// 执行 `kind_code` 定义的模型或协议操作；返回值反映校验后的结果。
pub const fn kind_code(kind: ElmKind) -> u32 {
    match kind {
        ElmKind::Manager => 1,
        ElmKind::Service => 2,
        ElmKind::Driver => 3,
        ElmKind::Extension => 4,
        ElmKind::Filesystem => 5,
        ElmKind::Network => 6,
        ElmKind::Debug => 7,
        ElmKind::Other => 255,
    }
}

/// 执行 `direction_code` 定义的模型或协议操作；返回值反映校验后的结果。
pub const fn direction_code(direction: FlowDirection) -> u32 {
    match direction {
        FlowDirection::Source => 1,
        FlowDirection::Sink => 2,
        FlowDirection::Duplex => 3,
        FlowDirection::Control => 4,
    }
}

/// 执行 `mode_code` 定义的模型或协议操作；返回值反映校验后的结果。
pub const fn mode_code(mode: FlowMode) -> u32 {
    match mode {
        FlowMode::Exclusive => 1,
        FlowMode::Shared => 2,
        FlowMode::Ordered => 3,
        FlowMode::Pipeline => 4,
        FlowMode::Broadcast => 5,
    }
}

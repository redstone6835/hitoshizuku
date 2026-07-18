//! ELM 运行时事件序列和固定事件记录。
//!
//! 事件用于向 elm-mgr、订阅者和用户态工具报告 cell、port、binding、lease、provider、
//! policy 与生命周期变化。sequence 单调递增，订阅者用游标读取并显式确认；事件记录是
//! 可观测事实，不取代事务状态和审计记录。
//!
//! 读取方必须检查丢失计数与 sequence 连续性。ring buffer 覆盖旧记录后，不能通过猜测
//! sequence 重建缺失状态，应重新读取完整 snapshot。

#[cfg(feature = "runtime-model")]
use crate::ids::{BindingId, ElmId, LeaseId, PortId};
#[cfg(feature = "runtime-model")]
use crate::topology::TopologyEventKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
/// 事件 ring 中单调递增且零值保留的强类型序列号。
pub struct ElmEventSequence(pub u64);

impl ElmEventSequence {
    /// `FIRST` 是该强类型序列允许分配的第一个非零值。
    pub const FIRST: Self = Self(1);

    /// 计算下一个非零标识符或代际，发生整数溢出时返回 `None`。
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) if value != 0 => Some(Self(value)),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmEventRecord` 是可观测快照或协议表中的单条固定布局记录。
pub struct ElmEventRecord {
    /// 单调递增的序列号，用于排序、游标推进和丢失检测。
    pub sequence: u64,
    /// 该记录、资源或关系的类别编码。
    pub kind: u32,
    /// 该记录关联的 cell id。
    pub cell: u64,
    /// 该记录关联的 port id。
    pub port: u64,
    /// 该记录关联的 binding id。
    pub binding: u64,
    /// 该记录关联的 lease id。
    pub lease: u64,
}

impl ElmEventRecord {
    /// 执行 `zero` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn zero() -> Self {
        Self {
            sequence: 0,
            kind: 0,
            cell: 0,
            port: 0,
            binding: 0,
            lease: 0,
        }
    }

    #[cfg(feature = "runtime-model")]
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(
        sequence: ElmEventSequence,
        kind: TopologyEventKind,
        cell: Option<ElmId>,
        port: Option<PortId>,
        binding: Option<BindingId>,
        lease: Option<LeaseId>,
    ) -> Self {
        Self {
            sequence: sequence.0,
            kind: event_kind_code(kind),
            cell: match cell {
                Some(id) => id.0,
                None => 0,
            },
            port: match port {
                Some(id) => id.0,
                None => 0,
            },
            binding: match binding {
                Some(id) => id.0,
                None => 0,
            },
            lease: match lease {
                Some(id) => id.0,
                None => 0,
            },
        }
    }
}

#[cfg(feature = "runtime-model")]
/// 执行 `event_kind_code` 定义的模型或协议操作；返回值反映校验后的结果。
pub const fn event_kind_code(kind: TopologyEventKind) -> u32 {
    match kind {
        TopologyEventKind::CellAdded => 1,
        TopologyEventKind::CellStateChanged => 2,
        TopologyEventKind::BindingAdded => 3,
        TopologyEventKind::BindingRemoved => 4,
        TopologyEventKind::LeaseAdded => 5,
        TopologyEventKind::LeaseRevoked => 6,
        TopologyEventKind::PortAdded => 7,
        TopologyEventKind::MenuItemAdded => 8,
        TopologyEventKind::MenuItemRemoved => 9,
        TopologyEventKind::CellRemoved => 10,
    }
}

//! ELM 运行拓扑事件与聚合快照模型。
//!
//! [`TopologyEvent`] 表示 cell、port、binding、lease 和菜单变化的增量事实；
//! [`TopologySnapshot`] 是某一 sequence 上的完整聚合视图。消费者发现事件丢失、序列不连续
//! 或订阅游标过期时，应重新读取 snapshot，而不是继续应用不完整增量。

use alloc::vec::Vec;

use crate::ids::{BindingId, ElmId, LeaseId};
use crate::state::ElmState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `TopologyEventKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum TopologyEventKind {
    /// `CellAdded` 表示 `TopologyEventKind` 的对象类别：`cell added`。
    CellAdded,
    /// `CellStateChanged` 表示 `TopologyEventKind` 的对象类别：`cell state changed`。
    CellStateChanged,
    /// `BindingAdded` 表示 `TopologyEventKind` 的对象类别：`binding added`。
    BindingAdded,
    /// `BindingRemoved` 表示 `TopologyEventKind` 的对象类别：`binding removed`。
    BindingRemoved,
    /// `LeaseAdded` 表示 `TopologyEventKind` 的对象类别：`lease added`。
    LeaseAdded,
    /// `LeaseRevoked` 表示 `TopologyEventKind` 的对象类别：`lease revoked`。
    LeaseRevoked,
    /// `PortAdded` 表示 `TopologyEventKind` 的对象类别：`port added`。
    PortAdded,
    /// `MenuItemAdded` 表示 `TopologyEventKind` 的对象类别：`menu item added`。
    MenuItemAdded,
    /// `MenuItemRemoved` 表示 `TopologyEventKind` 的对象类别：`menu item removed`。
    MenuItemRemoved,
    /// `CellRemoved` 表示 `TopologyEventKind` 的对象类别：`cell removed`。
    CellRemoved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 一条带 sequence、事件 kind 和相关对象 id 的运行拓扑增量记录。
pub struct TopologyEvent {
    /// 该记录、资源或关系的类别编码。
    pub kind: TopologyEventKind,
    /// 该记录关联的 cell id。
    pub cell: Option<ElmId>,
    /// 该记录关联的 port id。
    pub port: Option<crate::PortId>,
    /// 该记录关联的 binding id。
    pub binding: Option<BindingId>,
    /// 该记录关联的 lease id。
    pub lease: Option<LeaseId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
/// `TopologySnapshot` 是某一时刻的只读快照表示，不授予对原对象的所有权或长期引用。
pub struct TopologySnapshot {
    /// 当前快照或移除报告包含的 cell 集合。
    pub cells: Vec<(ElmId, ElmState)>,
    /// 当前图或快照包含的能力绑定集合。
    pub bindings: Vec<BindingId>,
    /// 当前注册或受该操作影响的租约集合。
    pub leases: Vec<LeaseId>,
}

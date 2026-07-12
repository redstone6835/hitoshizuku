//! 启动期 elm-mgr 内建 Nexus 端口描述。
//!
//! 根管理器固定发布 menu item、action invoke、core log 和 core event 四类基础端口，使用户态
//! 工具和子 ELM 能在其他子系统 provider 尚未接入时使用管理、日志和事件能力。端口 id、契约、
//! 方向、模式和访问策略必须与启动快照保持稳定。

use crate::ids::{ELM_MGR_BUILTIN_ID, ElmId, PortId};
use crate::nexus::{FlowDirection, FlowMode};
pub use crate::wire::ElmPortAccessPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `BuiltinPort` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum BuiltinPort {
    /// `CoreLog` 表示启动期由 elm-mgr 注册的内建运行时端口。
    CoreLog,
    /// `CoreEvent` 表示启动期由 elm-mgr 注册的内建运行时端口。
    CoreEvent,
    /// `MgrMenuItem` 表示启动期由 elm-mgr 注册的内建运行时端口。
    MgrMenuItem,
    /// `MgrActionInvoke` 表示启动期由 elm-mgr 注册的内建运行时端口。
    MgrActionInvoke,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 一个内建端口的稳定 id、owner、契约、方向、模式和访问策略描述。
pub struct PortDescriptor {
    /// 该对象在所属表或运行时注册表中的稳定标识符。
    pub id: PortId,
    /// 拥有该对象的 cell id；所有生命周期和权限检查都归属于该 owner。
    pub owner: Option<ElmId>,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: &'static str,
    /// 端口的数据流方向编码。
    pub direction: FlowDirection,
    /// 端口、绑定或扩展点采用的并发/分发模式编码。
    pub mode: FlowMode,
    /// 端口的访问范围策略编码。
    pub access: ElmPortAccessPolicy,
    /// `invokable` 表示该条件在当前快照或计划中是否成立。
    pub invokable: bool,
    /// `implemented` 表示该条件在当前快照或计划中是否成立。
    pub implemented: bool,
}

/// 执行 `builtin_port_descriptors` 定义的模型或协议操作；返回值反映校验后的结果。
pub const fn builtin_port_descriptors() -> [PortDescriptor; 4] {
    [
        desc(
            1,
            "core.log@1",
            FlowDirection::Sink,
            FlowMode::Shared,
            ElmPortAccessPolicy::Public,
            false,
            true,
        ),
        desc(
            2,
            "core.event@1",
            FlowDirection::Source,
            FlowMode::Broadcast,
            ElmPortAccessPolicy::Public,
            false,
            true,
        ),
        desc(
            3,
            "mgr.menu.item@1",
            FlowDirection::Sink,
            FlowMode::Ordered,
            ElmPortAccessPolicy::Public,
            false,
            true,
        ),
        desc_owned(
            4,
            ELM_MGR_BUILTIN_ID,
            "mgr.action.invoke@1",
            FlowDirection::Control,
            FlowMode::Shared,
            ElmPortAccessPolicy::Internal,
            true,
            true,
        ),
    ]
}

const fn desc(
    id: u64,
    contract: &'static str,
    direction: FlowDirection,
    mode: FlowMode,
    access: ElmPortAccessPolicy,
    invokable: bool,
    implemented: bool,
) -> PortDescriptor {
    PortDescriptor {
        id: PortId(id),
        owner: None,
        contract,
        direction,
        mode,
        access,
        invokable,
        implemented,
    }
}

const fn desc_owned(
    id: u64,
    owner: ElmId,
    contract: &'static str,
    direction: FlowDirection,
    mode: FlowMode,
    access: ElmPortAccessPolicy,
    invokable: bool,
    implemented: bool,
) -> PortDescriptor {
    PortDescriptor {
        id: PortId(id),
        owner: Some(owner),
        contract,
        direction,
        mode,
        access,
        invokable,
        implemented,
    }
}

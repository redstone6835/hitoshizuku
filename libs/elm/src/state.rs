//! ELM cell 的强制生命周期状态机。
//!
//! 状态迁移不是展示信息，而是装载、调用、暂停、热替换、卸载和故障隔离的共同门禁。所有
//! 管理操作必须先通过预检，再按本模块允许的边提交；不能通过直接写字段跳过中间状态。

use crate::error::{ElmError, ElmResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 一个 ELM cell 的生命周期状态。
pub enum ElmState {
    /// 已发现来源或声明，但尚未完成证明、格式和策略验证。
    Discovered,
    /// EBI、来源证明、目标架构、ABI 指纹和基础策略已经验证。
    Verified,
    /// 镜像或声明式对象已进入运行时所有权，但尚未完成链接。
    Loaded,
    /// 段、重定位、根 API、import 和原生入口已经链接完成。
    Linked,
    /// 必需初始化已成功，等待最终拓扑和公开提交。
    Ready,
    /// 单元已公开并可发起或承载策略允许的调用。
    Active,
    /// 正在阻止新工作并等待调用、租约和受托资源排空。
    Quiescing,
    /// 已完成排空并保留镜像与状态，但不能继续服务新调用。
    Paused,
    /// 已从公开拓扑和能力路由中移除，正在等待最终退役。
    Detached,
    /// 生命周期已经闭合，资源和镜像均可回收；该 cell 不会再次激活。
    Retired,
    /// 装载、钩子、原生执行或运行时不变量发生故障，等待隔离决策。
    Faulted,
    /// 已隔离故障单元；仅允许诊断和受控 detach，不允许恢复普通调用。
    Quarantined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 一次已经通过状态机规则验证的状态迁移。
pub struct ElmTransition {
    /// 迁移前状态。
    pub from: ElmState,
    /// 迁移后状态。
    pub to: ElmState,
}

impl ElmState {
    /// 纯函数判断状态机是否存在从 `self` 到 `to` 的直接边。
    ///
    /// 此函数不检查 graph、lease、policy、hook 或资源条件；这些条件由管理预检在提交迁移前
    /// 额外验证。返回 `true` 也不表示调用方可以绕过预检直接切换状态。
    pub const fn can_transition_to(self, to: ElmState) -> bool {
        matches!(
            (self, to),
            (Self::Discovered, Self::Verified)
                | (Self::Verified, Self::Loaded)
                | (Self::Loaded, Self::Linked)
                | (Self::Linked, Self::Ready)
                | (Self::Ready, Self::Active)
                | (Self::Loaded, Self::Detached)
                | (Self::Active, Self::Quiescing)
                | (Self::Quiescing, Self::Active)
                | (Self::Quiescing, Self::Paused)
                | (Self::Paused, Self::Active)
                | (Self::Paused, Self::Detached)
                | (Self::Quiescing, Self::Detached)
                | (Self::Detached, Self::Retired)
                | (Self::Loaded, Self::Faulted)
                | (Self::Discovered, Self::Faulted)
                | (Self::Verified, Self::Faulted)
                | (Self::Linked, Self::Faulted)
                | (Self::Ready, Self::Faulted)
                | (Self::Active, Self::Faulted)
                | (Self::Quiescing, Self::Faulted)
                | (Self::Paused, Self::Faulted)
                | (Self::Faulted, Self::Quarantined)
                | (Self::Quarantined, Self::Detached)
        )
    }

    /// 验证直接迁移并返回可记录的 [`ElmTransition`]。
    ///
    /// 不存在直接边时返回 [`ElmError::InvalidTransition`]。
    pub fn transition_to(self, to: ElmState) -> ElmResult<ElmTransition> {
        if self.can_transition_to(to) {
            Ok(ElmTransition { from: self, to })
        } else {
            Err(ElmError::InvalidTransition)
        }
    }
}

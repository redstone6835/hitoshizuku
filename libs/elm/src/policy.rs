//! ELM 开发侧策略检查抽象。
//!
//! 子系统 crate 可以调用这里的函数检查当前 ELM 是否具备某类运行时能力。
//! 本模块不持有 `ElmCore`，只读取 `context` 中的当前执行快照，避免反向依赖
//! `kernel` 或在 ELM 原生调用期间重入核心锁。
//!
//! 子系统入口应把所需动作位传给 [`check_current_cell`] 或 [`current_cell_allows`]，再结合
//! 自身对象 ACL、资源预算和参数校验做最终授权。当前上下文只是一层快速门禁，不替代
//! elm-mgr 在管理事务和跨 cell 调用中的完整策略求值。

use crate::context::current_context;
use crate::ids::{ElmId, Generation};
use crate::mgr::{
    ELM_MGR_STATUS_OK, ELM_MGR_STATUS_PERMISSION, ELM_POLICY_BLOCK_CAPABILITY_DENIED,
};
pub use crate::wire::ElmPrincipalKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmPolicyCheck` 表示 ELM 的授权和约束策略；更新必须经过管理权限、代际与审计检查。
pub struct ElmPolicyCheck {
    /// `allowed` 表示该条件在当前快照或计划中是否成立。
    pub allowed: bool,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 阻止操作提交的原因位集合；非零表示预检未通过。
    pub blockers: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 参与策略求值的内核、用户管理员或具体 cell/generation 主体。
pub struct ElmPrincipal {
    /// 该记录、资源或关系的类别编码。
    pub kind: ElmPrincipalKind,
    /// `actor_id` 所指对象的稳定运行时标识符。
    pub actor_id: u64,
    /// `credential_id` 所指对象的稳定运行时标识符。
    pub credential_id: u64,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: Generation,
}

impl ElmPrincipal {
    /// 执行 `kernel` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn kernel() -> Self {
        Self {
            kind: ElmPrincipalKind::Kernel,
            actor_id: 0,
            credential_id: 0,
            generation: Generation(0),
        }
    }

    /// 执行 `user_admin` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn user_admin(task_id: u64, credential_id: u64) -> Self {
        Self {
            kind: ElmPrincipalKind::UserAdmin,
            actor_id: task_id,
            credential_id,
            generation: Generation(0),
        }
    }

    /// 执行 `elm_cell` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn elm_cell(cell_id: ElmId, generation: Generation) -> Self {
        Self {
            kind: ElmPrincipalKind::ElmCell,
            actor_id: cell_id.0,
            credential_id: 0,
            generation,
        }
    }

    /// 执行 `current_elm_cell` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn current_elm_cell() -> Option<Self> {
        current_context().map(|context| Self::elm_cell(context.cell_id, context.generation))
    }
}

impl ElmPolicyCheck {
    /// 执行 `allow` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn allow() -> Self {
        Self {
            allowed: true,
            status: ELM_MGR_STATUS_OK,
            blockers: 0,
        }
    }

    /// 执行 `deny` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn deny(blockers: u64) -> Self {
        Self {
            allowed: false,
            status: ELM_MGR_STATUS_PERMISSION,
            blockers,
        }
    }
}

/// 执行 `check_current_cell` 定义的模型或协议操作；返回值反映校验后的结果。
pub fn check_current_cell(action: u32) -> ElmPolicyCheck {
    let Some(context) = current_context() else {
        return ElmPolicyCheck::allow();
    };
    if context.allowed_actions & action != 0 {
        ElmPolicyCheck::allow()
    } else {
        ElmPolicyCheck::deny(ELM_POLICY_BLOCK_CAPABILITY_DENIED)
    }
}

/// 执行 `current_cell_allows` 定义的模型或协议操作；返回值反映校验后的结果。
pub fn current_cell_allows(action: u32) -> bool {
    check_current_cell(action).allowed
}

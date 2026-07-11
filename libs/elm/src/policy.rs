//! ELM 开发侧策略检查抽象。
//!
//! 子系统 crate 可以调用这里的函数检查当前 ELM 是否具备某类运行时能力。
//! 本模块不持有 `ElmCore`，只读取 `context` 中的当前执行快照，避免反向依赖
//! `kernel` 或在 ELM 原生调用期间重入核心锁。

use crate::context::current_context;
use crate::ids::{ElmId, Generation};
use crate::mgr::{
    ELM_MGR_STATUS_OK, ELM_MGR_STATUS_PERMISSION, ELM_POLICY_BLOCK_CAPABILITY_DENIED,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmPolicyCheck {
    pub allowed: bool,
    pub status: i32,
    pub blockers: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmPrincipalKind {
    Kernel = 1,
    UserAdmin = 2,
    ElmCell = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmPrincipal {
    pub kind: ElmPrincipalKind,
    pub actor_id: u64,
    pub credential_id: u64,
    pub generation: Generation,
}

impl ElmPrincipal {
    pub const fn kernel() -> Self {
        Self {
            kind: ElmPrincipalKind::Kernel,
            actor_id: 0,
            credential_id: 0,
            generation: Generation(0),
        }
    }

    pub const fn user_admin(task_id: u64, credential_id: u64) -> Self {
        Self {
            kind: ElmPrincipalKind::UserAdmin,
            actor_id: task_id,
            credential_id,
            generation: Generation(0),
        }
    }

    pub const fn elm_cell(cell_id: ElmId, generation: Generation) -> Self {
        Self {
            kind: ElmPrincipalKind::ElmCell,
            actor_id: cell_id.0,
            credential_id: 0,
            generation,
        }
    }

    pub fn current_elm_cell() -> Option<Self> {
        current_context().map(|context| Self::elm_cell(context.cell_id, context.generation))
    }
}

impl ElmPolicyCheck {
    pub const fn allow() -> Self {
        Self {
            allowed: true,
            status: ELM_MGR_STATUS_OK,
            blockers: 0,
        }
    }

    pub const fn deny(blockers: u64) -> Self {
        Self {
            allowed: false,
            status: ELM_MGR_STATUS_PERMISSION,
            blockers,
        }
    }
}

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

pub fn current_cell_allows(action: u32) -> bool {
    check_current_cell(action).allowed
}

//! 内核单元生命周期状态机。

use crate::error::{ElmError, ElmResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElmState {
    Discovered,
    Verified,
    Loaded,
    Linked,
    Ready,
    Active,
    Quiescing,
    Paused,
    Detached,
    Retired,
    Faulted,
    Quarantined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmTransition {
    pub from: ElmState,
    pub to: ElmState,
}

impl ElmState {
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
                | (Self::Quiescing, Self::Paused)
                | (Self::Paused, Self::Active)
                | (Self::Paused, Self::Detached)
                | (Self::Quiescing, Self::Detached)
                | (Self::Detached, Self::Retired)
                | (Self::Loaded, Self::Faulted)
                | (Self::Active, Self::Faulted)
                | (Self::Quiescing, Self::Faulted)
                | (Self::Paused, Self::Faulted)
                | (Self::Faulted, Self::Quarantined)
                | (Self::Quarantined, Self::Detached)
        )
    }

    pub fn transition_to(self, to: ElmState) -> ElmResult<ElmTransition> {
        if self.can_transition_to(to) {
            Ok(ElmTransition { from: self, to })
        } else {
            Err(ElmError::InvalidTransition)
        }
    }
}

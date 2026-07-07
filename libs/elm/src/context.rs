//! ELM 生命周期上下文。

use crate::ids::{ElmId, Generation};
use crate::state::ElmState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElmLifecyclePhase {
    Initialize,
    Finalize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmContext {
    cell_id: ElmId,
    parent_id: Option<ElmId>,
    generation: Generation,
    state: ElmState,
    phase: ElmLifecyclePhase,
    flags: u32,
}

impl ElmContext {
    pub const fn new(
        cell_id: ElmId,
        parent_id: Option<ElmId>,
        generation: Generation,
        state: ElmState,
        phase: ElmLifecyclePhase,
        flags: u32,
    ) -> Self {
        Self {
            cell_id,
            parent_id,
            generation,
            state,
            phase,
            flags,
        }
    }

    pub const fn cell_id(&self) -> ElmId {
        self.cell_id
    }

    pub const fn parent_id(&self) -> Option<ElmId> {
        self.parent_id
    }

    pub const fn generation(&self) -> Generation {
        self.generation
    }

    pub const fn state(&self) -> ElmState {
        self.state
    }

    pub const fn phase(&self) -> ElmLifecyclePhase {
        self.phase
    }

    pub const fn flags(&self) -> u32 {
        self.flags
    }

    pub fn set_state(&mut self, state: ElmState) {
        self.state = state;
    }
}

//! ELM 生命周期上下文。

use crate::ids::{ElmId, Generation};
use crate::state::ElmState;

pub const ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION: u16 = 1;
pub const ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElmLifecyclePhase {
    Initialize,
    Finalize,
    Quiesce,
    Pause,
    Resume,
    MigrateExport,
    MigrateImport,
    MigrateAbort,
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

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNativeHookContextV1 {
    pub abi_version: u16,
    pub phase: u16,
    pub flags: u32,
    pub cell_id: u64,
    pub parent_id: u64,
    pub generation: u64,
    pub state: u32,
    pub reserved: u32,
}

impl ElmNativeHookContextV1 {
    pub const fn from_context(context: &ElmContext) -> Self {
        Self {
            abi_version: ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION,
            phase: match context.phase() {
                ElmLifecyclePhase::Initialize => 1,
                ElmLifecyclePhase::Finalize => 2,
                ElmLifecyclePhase::Quiesce => 3,
                ElmLifecyclePhase::Pause => 4,
                ElmLifecyclePhase::Resume => 5,
                ElmLifecyclePhase::MigrateExport => 6,
                ElmLifecyclePhase::MigrateImport => 7,
                ElmLifecyclePhase::MigrateAbort => 8,
            },
            flags: context.flags(),
            cell_id: context.cell_id().0,
            parent_id: match context.parent_id() {
                Some(parent) => parent.0,
                None => 0,
            },
            generation: context.generation().0,
            state: context.state() as u32,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNativeMigrationContextV1 {
    pub abi_version: u16,
    pub phase: u16,
    pub flags: u32,
    pub cell_id: u64,
    pub old_generation: u64,
    pub new_generation: u64,
    pub buffer_ptr: u64,
    pub buffer_capacity: u64,
    pub buffer_len: u64,
    pub status: i32,
    pub reserved: u32,
}

impl ElmNativeMigrationContextV1 {
    pub const fn new(
        phase: ElmLifecyclePhase,
        cell_id: ElmId,
        old_generation: Generation,
        new_generation: Generation,
        buffer_ptr: u64,
        buffer_capacity: u64,
        buffer_len: u64,
    ) -> Self {
        Self {
            abi_version: ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION,
            phase: match phase {
                ElmLifecyclePhase::MigrateExport => 6,
                ElmLifecyclePhase::MigrateImport => 7,
                ElmLifecyclePhase::MigrateAbort => 8,
                _ => 0,
            },
            flags: 0,
            cell_id: cell_id.0,
            old_generation: old_generation.0,
            new_generation: new_generation.0,
            buffer_ptr,
            buffer_capacity,
            buffer_len,
            status: 0,
            reserved: 0,
        }
    }
}

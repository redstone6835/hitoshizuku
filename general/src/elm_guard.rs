//! ELM 原生调用保护域。
//!
//! 这里提供架构无关的运行中 ELM 调用状态。架构层后续可以在 trap 路径读取当前保护域，
//! 将命中的原生 ELM fault 重定向到恢复出口；在没有架构 trampoline 时，本模块仍负责
//! 取消意图、超时意图和嵌套保护域拒绝。

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub const ELM_GUARD_PHASE_NONE: u32 = 0;
pub const ELM_GUARD_PHASE_HOOK: u32 = 1;
pub const ELM_GUARD_PHASE_MIGRATION: u32 = 2;
pub const ELM_GUARD_PHASE_ENTRY: u32 = 3;
pub const ELM_GUARD_PHASE_PROVIDER_CALL: u32 = 4;
pub const ELM_GUARD_PHASE_PROVIDER_SNAPSHOT: u32 = 5;

pub const ELM_GUARD_ABORT_NONE: usize = 0;
pub const ELM_GUARD_ABORT_CANCEL: usize = 1;
pub const ELM_GUARD_ABORT_TIMEOUT: usize = 2;
pub const ELM_GUARD_ABORT_TRAP: usize = 3;
pub const ELM_GUARD_ABORT_PANIC: usize = 4;

static ACTIVE_CELL: AtomicU64 = AtomicU64::new(0);
static ACTIVE_PHASE: AtomicUsize = AtomicUsize::new(ELM_GUARD_PHASE_NONE as usize);
static ACTIVE_DEADLINE_NS: AtomicU64 = AtomicU64::new(0);
static ABORT_REASON: AtomicUsize = AtomicUsize::new(ELM_GUARD_ABORT_NONE);

pub struct ElmGuard {
    cell: u64,
}

impl ElmGuard {
    pub fn enter(cell: u64, phase: u32, deadline_ns: u64) -> Option<Self> {
        if cell == 0 || phase == ELM_GUARD_PHASE_NONE {
            return None;
        }
        if ACTIVE_CELL
            .compare_exchange(0, cell, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        ACTIVE_PHASE.store(phase as usize, Ordering::Release);
        ACTIVE_DEADLINE_NS.store(deadline_ns, Ordering::Release);
        ABORT_REASON.store(ELM_GUARD_ABORT_NONE, Ordering::Release);
        Some(Self { cell })
    }

    pub fn abort_reason(&self) -> usize {
        ABORT_REASON.load(Ordering::Acquire)
    }

    pub fn aborted(&self) -> bool {
        self.abort_reason() != ELM_GUARD_ABORT_NONE
    }
}

impl Drop for ElmGuard {
    fn drop(&mut self) {
        if ACTIVE_CELL.load(Ordering::Acquire) == self.cell {
            ACTIVE_DEADLINE_NS.store(0, Ordering::Release);
            ACTIVE_PHASE.store(ELM_GUARD_PHASE_NONE as usize, Ordering::Release);
            ABORT_REASON.store(ELM_GUARD_ABORT_NONE, Ordering::Release);
            ACTIVE_CELL.store(0, Ordering::Release);
        }
    }
}

pub fn active_cell() -> u64 {
    ACTIVE_CELL.load(Ordering::Acquire)
}

pub fn active_phase() -> u32 {
    ACTIVE_PHASE.load(Ordering::Acquire) as u32
}

pub fn active_deadline_ns() -> u64 {
    ACTIVE_DEADLINE_NS.load(Ordering::Acquire)
}

pub fn request_abort(cell: u64, reason: usize) -> bool {
    if cell == 0 || reason == ELM_GUARD_ABORT_NONE || ACTIVE_CELL.load(Ordering::Acquire) != cell {
        return false;
    }
    ABORT_REASON.store(reason, Ordering::Release);
    true
}

pub fn request_timeout_if_expired(now_ns: u64) -> bool {
    let cell = ACTIVE_CELL.load(Ordering::Acquire);
    let deadline = ACTIVE_DEADLINE_NS.load(Ordering::Acquire);
    if cell == 0 || deadline == 0 || now_ns < deadline {
        return false;
    }
    ABORT_REASON.store(ELM_GUARD_ABORT_TIMEOUT, Ordering::Release);
    true
}

pub fn request_trap_recovery(cell: u64) -> bool {
    request_abort(cell, ELM_GUARD_ABORT_TRAP)
}

pub fn request_panic_recovery(cell: u64) -> bool {
    request_abort(cell, ELM_GUARD_ABORT_PANIC)
}

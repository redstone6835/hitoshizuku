const STAT_RANDOM_GENERATING: u32 = 1 << 30;
const STAT_RANDOM_SEEDING: u32 = 1 << 31;

const ISTAT_RANDOM_READY: u32 = 1 << 0;
const ISTAT_SEED_DONE: u32 = 1 << 1;
const ISTAT_LFSR_LOCKUP: u32 = 1 << 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitState {
    Pending,
    Ready,
    Lockup,
}

pub(crate) fn is_idle(status: u32) -> bool {
    status & (STAT_RANDOM_GENERATING | STAT_RANDOM_SEEDING) == 0
}

fn wait_state(interrupt_status: u32, ready: u32) -> WaitState {
    if interrupt_status & ISTAT_LFSR_LOCKUP != 0 {
        WaitState::Lockup
    } else if interrupt_status & ready != 0 {
        WaitState::Ready
    } else {
        WaitState::Pending
    }
}

pub(crate) fn reseed_wait_state(interrupt_status: u32) -> WaitState {
    wait_state(interrupt_status, ISTAT_SEED_DONE)
}

pub(crate) fn random_wait_state(interrupt_status: u32) -> WaitState {
    wait_state(interrupt_status, ISTAT_RANDOM_READY)
}

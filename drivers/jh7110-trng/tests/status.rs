#[path = "../src/status.rs"]
mod status;

use status::{WaitState, is_idle, random_wait_state, reseed_wait_state};

#[test]
fn busy_status_is_not_idle() {
    assert!(is_idle(0));
    assert!(!is_idle(1 << 30));
    assert!(!is_idle(1 << 31));
    assert!(!is_idle((1 << 30) | (1 << 31)));
}

#[test]
fn reseed_wait_requires_seed_done_and_rejects_lockup() {
    assert_eq!(reseed_wait_state(0), WaitState::Pending);
    assert_eq!(reseed_wait_state(1 << 1), WaitState::Ready);
    assert_eq!(reseed_wait_state(1 << 0), WaitState::Pending);
    assert_eq!(reseed_wait_state((1 << 1) | (1 << 4)), WaitState::Lockup);
}

#[test]
fn random_wait_requires_random_ready_and_rejects_lockup() {
    assert_eq!(random_wait_state(0), WaitState::Pending);
    assert_eq!(random_wait_state(1 << 0), WaitState::Ready);
    assert_eq!(random_wait_state(1 << 1), WaitState::Pending);
    assert_eq!(random_wait_state((1 << 0) | (1 << 4)), WaitState::Lockup);
}

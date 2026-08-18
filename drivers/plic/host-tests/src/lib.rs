#![allow(dead_code)]

extern crate alloc;

#[path = "../../src/config.rs"]
mod config;

#[path = "../../src/dispatch.rs"]
mod dispatch;

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use core::sync::atomic::AtomicBool;

    use super::dispatch::{DispatchGate, drain_pending, mark_unhandled_once};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Event {
        Dispatch(u32),
        Complete(u32),
    }

    #[test]
    fn draining_dispatches_before_completing_each_claim() {
        let claims = RefCell::new(VecDeque::from([3, 1, 0]));
        let events = RefCell::new(Vec::new());

        let result = drain_pending(
            95,
            64,
            || claims.borrow_mut().pop_front().unwrap_or(0),
            |hwirq| {
                events.borrow_mut().push(Event::Dispatch(hwirq));
                hwirq == 3
            },
            |hwirq| events.borrow_mut().push(Event::Complete(hwirq)),
        );

        assert_eq!(
            events.into_inner(),
            [
                Event::Dispatch(3),
                Event::Complete(3),
                Event::Dispatch(1),
                Event::Complete(1),
            ]
        );
        assert_eq!(result.claimed, 2);
        assert_eq!(result.handled, 1);
        assert_eq!(result.unhandled, 1);
        assert_eq!(result.invalid, None);
        assert!(!result.exhausted);
    }

    #[test]
    fn draining_stops_at_budget_even_when_source_stays_pending() {
        let mut dispatched = 0;
        let mut completed = 0;

        let result = drain_pending(
            95,
            4,
            || 7,
            |_| {
                dispatched += 1;
                true
            },
            |_| completed += 1,
        );

        assert_eq!(dispatched, 4);
        assert_eq!(completed, 4);
        assert_eq!(result.claimed, 4);
        assert!(result.exhausted);
    }

    #[test]
    fn draining_completes_invalid_claim_and_stops() {
        let mut dispatched = 0;
        let mut completed = Vec::new();

        let result = drain_pending(
            95,
            64,
            || 96,
            |_| {
                dispatched += 1;
                true
            },
            |hwirq| completed.push(hwirq),
        );

        assert_eq!(dispatched, 0);
        assert_eq!(completed, [96]);
        assert_eq!(result.claimed, 1);
        assert_eq!(result.invalid, Some(96));
        assert!(!result.exhausted);
    }

    #[test]
    fn dispatch_gate_rejects_entries_after_close() {
        let gate = DispatchGate::new();
        let active = gate.try_enter().expect("open gate must admit dispatch");

        assert!(gate.close());
        assert!(!gate.close());
        assert!(gate.try_enter().is_none());
        drop(active);
        gate.wait_for_idle();
    }

    #[test]
    fn unhandled_source_is_reported_once_per_enable_lifecycle() {
        let reported = AtomicBool::new(false);

        assert!(mark_unhandled_once(&reported));
        assert!(!mark_unhandled_once(&reported));
        reported.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(mark_unhandled_once(&reported));
    }
}

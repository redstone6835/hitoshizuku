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

    use super::dispatch::{
        DispatchGate, drain_pending, mark_unhandled_once, select_target_context,
    };

    #[test]
    fn routing_selects_one_online_context_per_source() {
        let contexts = [0, 2, 5];
        let online = (1u64 << 0) | (1u64 << 5);

        let route = |hwirq| {
            select_target_context(contexts.len(), online, 0, hwirq, |index| contexts[index])
        };

        assert_eq!(route(1), Some(0));
        assert_eq!(route(2), Some(2));
        assert_eq!(route(3), Some(0));
        assert_eq!(route(4), Some(2));
    }

    #[test]
    fn routing_falls_back_when_no_context_cpu_is_online() {
        let contexts = [1, 4];

        assert_eq!(
            select_target_context(contexts.len(), 0, 1, 7, |index| contexts[index]),
            Some(1)
        );
        assert_eq!(
            select_target_context(contexts.len(), 0, 1, 0, |index| contexts[index]),
            None
        );
        assert_eq!(
            select_target_context(0, u64::MAX, 0, 1, |_| unreachable!()),
            None
        );
        assert_eq!(
            select_target_context(contexts.len(), u64::MAX, 2, 1, |index| contexts[index]),
            None
        );
    }

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

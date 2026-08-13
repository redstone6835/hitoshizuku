use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::{Access, AccessKind, Report, ReportWindow};

const REPORT_SLOTS: usize = 64;
const FIRST_SEQUENCE: u64 = 1;
const SLOT_STATE_BITS: u32 = 2;
const SLOT_STATE_MASK: u64 = (1 << SLOT_STATE_BITS) - 1;
const SLOT_COMPLETE: u64 = 1;
const SLOT_READING: u64 = 2;
const SLOT_WRITING: u64 = 3;

#[repr(C, align(64))]
struct ReportSlot {
    state: AtomicU64,
    first_address: AtomicUsize,
    first_meta: AtomicU64,
    first_task: AtomicU64,
    first_pc: AtomicUsize,
    first_timestamp: AtomicU64,
    second_address: AtomicUsize,
    second_meta: AtomicU64,
    second_task: AtomicU64,
    second_pc: AtomicUsize,
    second_timestamp: AtomicU64,
}

impl ReportSlot {
    const fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
            first_address: AtomicUsize::new(0),
            first_meta: AtomicU64::new(0),
            first_task: AtomicU64::new(0),
            first_pc: AtomicUsize::new(0),
            first_timestamp: AtomicU64::new(0),
            second_address: AtomicUsize::new(0),
            second_meta: AtomicU64::new(0),
            second_task: AtomicU64::new(0),
            second_pc: AtomicUsize::new(0),
            second_timestamp: AtomicU64::new(0),
        }
    }

    fn try_publish(&self, sequence: u64, first: Access, second: Access) -> bool {
        let observed = self.state.load(Ordering::Acquire);
        if matches!(slot_status(observed), SLOT_READING | SLOT_WRITING)
            || self
                .state
                .compare_exchange(
                    observed,
                    slot_state(sequence, SLOT_WRITING),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
        {
            return false;
        }
        self.first_address.store(first.address, Ordering::Relaxed);
        self.first_meta.store(encode_meta(first), Ordering::Relaxed);
        self.first_task.store(first.task, Ordering::Relaxed);
        self.first_pc.store(first.pc, Ordering::Relaxed);
        self.first_timestamp
            .store(first.timestamp, Ordering::Relaxed);
        self.second_address.store(second.address, Ordering::Relaxed);
        self.second_meta
            .store(encode_meta(second), Ordering::Relaxed);
        self.second_task.store(second.task, Ordering::Relaxed);
        self.second_pc.store(second.pc, Ordering::Relaxed);
        self.second_timestamp
            .store(second.timestamp, Ordering::Relaxed);
        self.state
            .store(slot_state(sequence, SLOT_COMPLETE), Ordering::Release);
        true
    }

    fn read(&self, sequence: u64) -> Option<Report> {
        let complete = slot_state(sequence, SLOT_COMPLETE);
        if self
            .state
            .compare_exchange(
                complete,
                slot_state(sequence, SLOT_READING),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return None;
        }
        let report = Report {
            sequence,
            first: decode_access(
                self.first_address.load(Ordering::Relaxed),
                self.first_meta.load(Ordering::Relaxed),
                self.first_task.load(Ordering::Relaxed),
                self.first_pc.load(Ordering::Relaxed),
                self.first_timestamp.load(Ordering::Relaxed),
            ),
            second: decode_access(
                self.second_address.load(Ordering::Relaxed),
                self.second_meta.load(Ordering::Relaxed),
                self.second_task.load(Ordering::Relaxed),
                self.second_pc.load(Ordering::Relaxed),
                self.second_timestamp.load(Ordering::Relaxed),
            ),
        };
        self.state.store(complete, Ordering::Release);
        Some(report)
    }
}

static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(FIRST_SEQUENCE);
static PUBLISHING: AtomicBool = AtomicBool::new(false);
static REPORTS: [ReportSlot; REPORT_SLOTS] = [const { ReportSlot::new() }; REPORT_SLOTS];

struct PublishGuard;

impl Drop for PublishGuard {
    fn drop(&mut self) {
        PUBLISHING.store(false, Ordering::Release);
    }
}

pub(crate) fn publish(first: Access, second: Access) -> Option<u64> {
    if PUBLISHING
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return None;
    }
    let _guard = PublishGuard;
    let sequence = NEXT_SEQUENCE.load(Ordering::Relaxed);
    if !REPORTS[sequence as usize & (REPORT_SLOTS - 1)].try_publish(sequence, first, second) {
        return None;
    }
    NEXT_SEQUENCE.store(sequence.wrapping_add(1), Ordering::Release);
    Some(sequence)
}

pub fn report_window() -> ReportWindow {
    let next_sequence = NEXT_SEQUENCE.load(Ordering::Acquire);
    let first_sequence = next_sequence
        .saturating_sub(REPORT_SLOTS as u64)
        .max(FIRST_SEQUENCE);
    ReportWindow {
        first_sequence,
        next_sequence,
        overwritten: first_sequence.saturating_sub(FIRST_SEQUENCE),
    }
}

pub fn report(sequence: u64) -> Option<Report> {
    let window = report_window();
    if sequence < window.first_sequence || sequence >= window.next_sequence {
        return None;
    }
    REPORTS[sequence as usize & (REPORT_SLOTS - 1)].read(sequence)
}

fn slot_state(sequence: u64, status: u64) -> u64 {
    sequence.wrapping_shl(SLOT_STATE_BITS) | status
}

fn slot_status(state: u64) -> u64 {
    state & SLOT_STATE_MASK
}

fn encode_meta(access: Access) -> u64 {
    (access.size.min(u16::MAX as usize) as u64)
        | (u64::from(access.kind as u8) << 16)
        | ((access.cpu.min(u16::MAX as usize) as u64) << 32)
}

fn decode_access(address: usize, meta: u64, task: u64, pc: usize, timestamp: u64) -> Access {
    Access {
        address,
        size: (meta & 0xffff) as usize,
        kind: AccessKind::from_raw(((meta >> 16) & 0xff) as u8),
        cpu: ((meta >> 32) & 0xffff) as usize,
        task,
        pc,
        timestamp,
    }
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    NEXT_SEQUENCE.store(FIRST_SEQUENCE, Ordering::SeqCst);
    PUBLISHING.store(false, Ordering::SeqCst);
    for slot in &REPORTS {
        slot.state.store(0, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(address: usize, kind: AccessKind) -> Access {
        Access {
            address,
            size: 8,
            kind,
            cpu: 2,
            task: 9,
            pc: 0x1234,
            timestamp: 77,
        }
    }

    #[test]
    fn report_ring_rejects_overwritten_sequences() {
        let _serial = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_for_test();
        for offset in 0..REPORT_SLOTS + 3 {
            let _ = publish(
                access(0x8000_0000_0000_1000 + offset, AccessKind::Write),
                access(0x8000_0000_0000_1000 + offset, AccessKind::Read),
            );
        }
        let window = report_window();
        assert_eq!(window.first_sequence, 4);
        assert_eq!(window.overwritten, 3);
        assert!(report(1).is_none());
        let latest = report(window.next_sequence - 1).unwrap();
        assert_eq!(latest.sequence, window.next_sequence - 1);
        assert_eq!(latest.first.kind, AccessKind::Write);
    }

    #[test]
    fn busy_publisher_drops_without_creating_sequence_gap() {
        let _serial = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_for_test();
        PUBLISHING.store(true, Ordering::Release);
        assert!(
            publish(
                access(0x8000_0000_0000_2000, AccessKind::Write),
                access(0x8000_0000_0000_2000, AccessKind::Read),
            )
            .is_none()
        );
        assert_eq!(report_window().next_sequence, FIRST_SEQUENCE);

        PUBLISHING.store(false, Ordering::Release);
        assert_eq!(
            publish(
                access(0x8000_0000_0000_2000, AccessKind::Write),
                access(0x8000_0000_0000_2000, AccessKind::Read),
            ),
            Some(FIRST_SEQUENCE)
        );
        assert!(report(FIRST_SEQUENCE).is_some());
    }

    #[test]
    fn active_reader_prevents_torn_ring_overwrite() {
        let _serial = crate::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_for_test();
        for offset in 0..REPORT_SLOTS {
            assert_eq!(
                publish(
                    access(0x8000_0000_0000_3000 + offset, AccessKind::Write),
                    access(0x8000_0000_0000_3000 + offset, AccessKind::Read),
                ),
                Some(FIRST_SEQUENCE + offset as u64)
            );
        }

        let first_slot = &REPORTS[FIRST_SEQUENCE as usize & (REPORT_SLOTS - 1)];
        let complete = slot_state(FIRST_SEQUENCE, SLOT_COMPLETE);
        assert_eq!(
            first_slot.state.compare_exchange(
                complete,
                slot_state(FIRST_SEQUENCE, SLOT_READING),
                Ordering::Acquire,
                Ordering::Relaxed,
            ),
            Ok(complete)
        );
        assert!(
            publish(
                access(0x8000_0000_0000_4000, AccessKind::Write),
                access(0x8000_0000_0000_4000, AccessKind::Read),
            )
            .is_none()
        );
        assert_eq!(
            report_window().next_sequence,
            FIRST_SEQUENCE + REPORT_SLOTS as u64
        );

        first_slot.state.store(complete, Ordering::Release);
        assert_eq!(
            publish(
                access(0x8000_0000_0000_4000, AccessKind::Write),
                access(0x8000_0000_0000_4000, AccessKind::Read),
            ),
            Some(FIRST_SEQUENCE + REPORT_SLOTS as u64)
        );
    }
}

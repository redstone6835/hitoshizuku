extern crate self as errno;
extern crate self as sched;

use core::sync::atomic::{AtomicI64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Errno {
    EINVAL,
}

pub mod sync {
    pub struct Spinlock<T>(std::sync::Mutex<T>);

    impl<T> Spinlock<T> {
        pub const fn new(value: T) -> Self {
            Self(std::sync::Mutex::new(value))
        }

        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
            self.0.lock().unwrap()
        }
    }
}

mod vdso {
    use super::{AtomicI64, Ordering};

    static REALTIME_OFFSET_NS: AtomicI64 = AtomicI64::new(0);

    pub fn monotonic_ns() -> u64 {
        0
    }

    pub(crate) fn adjust_realtime_offset(delta_ns: i64) {
        REALTIME_OFFSET_NS.fetch_add(delta_ns, Ordering::Relaxed);
    }

    pub fn realtime_offset_ns() -> i64 {
        REALTIME_OFFSET_NS.load(Ordering::Relaxed)
    }
}

#[path = "../../kernel/src/adjtimex.rs"]
mod adjtimex;

fn request(modes: u32, status: i32, offset: i64) -> adjtimex::TimexFields {
    adjtimex::TimexFields {
        modes,
        offset,
        freq: 0,
        maxerror: 0,
        esterror: 0,
        status,
        constant: 0,
        tick: 10_000,
        time_sec: 0,
        time_subsec: 0,
        precision: 0,
        tolerance: 0,
    }
}

#[test]
fn adj_status_preserves_kernel_bits_and_nano_mode() {
    let nano = adjtimex::do_adjtimex(request(adjtimex::ADJ_NANO, 0, 0)).unwrap();
    assert_ne!(nano.status & adjtimex::STA_NANO, 0);

    let requested = adjtimex::STA_PLL | adjtimex::STA_PPSSIGNAL | adjtimex::STA_CLOCKERR;
    let status = adjtimex::do_adjtimex(request(adjtimex::ADJ_STATUS, requested, 0)).unwrap();
    assert_ne!(status.status & adjtimex::STA_PLL, 0);
    assert_eq!(status.status & adjtimex::STA_PPSSIGNAL, 0);
    assert_eq!(status.status & adjtimex::STA_CLOCKERR, 0);
    assert_ne!(status.status & adjtimex::STA_NANO, 0);

    let before = vdso::realtime_offset_ns();
    adjtimex::do_adjtimex(request(adjtimex::ADJ_OFFSET, 0, 7)).unwrap();
    assert_eq!(vdso::realtime_offset_ns() - before, 7);
}

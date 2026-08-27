//! 时间命名空间：realtime/monotonic/boottime 偏移。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicI64, Ordering};

use crate::{Namespace, NsType, allocate_ns_inum};

/// 时间命名空间。
pub struct TimeNamespace {
    inum: u64,
    realtime_offset: AtomicI64,
    monotonic_offset: AtomicI64,
    boottime_offset: AtomicI64,
}

impl TimeNamespace {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inum: allocate_ns_inum(),
            realtime_offset: AtomicI64::new(0),
            monotonic_offset: AtomicI64::new(0),
            boottime_offset: AtomicI64::new(0),
        })
    }

    /// 相对根时钟的偏移（调用方将 `clock_gettime` 结果加上该值）。
    ///
    /// Linux 时间命名空间偏移覆盖 REALTIME/MONOTONIC/BOOTTIME 及其 COARSE
    /// 变体；TAI 由 REALTIME 派生，故叠加 realtime 偏移。MONOTONIC_RAW(4) 与
    /// CPU 时钟(2/3) 不偏移。
    pub fn offset(&self, clock_id: i32) -> i64 {
        match clock_id {
            // CLOCK_REALTIME(0) / CLOCK_REALTIME_COARSE(5) / CLOCK_TAI(11)
            0 | 5 | 11 => self.realtime_offset.load(Ordering::Acquire),
            // CLOCK_MONOTONIC(1) / CLOCK_MONOTONIC_COARSE(6)
            1 | 6 => self.monotonic_offset.load(Ordering::Acquire),
            // CLOCK_BOOTTIME(7)
            7 => self.boottime_offset.load(Ordering::Acquire),
            _ => 0,
        }
    }

    pub fn set_realtime_offset(&self, offset: i64) {
        self.realtime_offset.store(offset, Ordering::Release);
    }

    pub fn set_monotonic_offset(&self, offset: i64) {
        self.monotonic_offset.store(offset, Ordering::Release);
    }

    pub fn set_boottime_offset(&self, offset: i64) {
        self.boottime_offset.store(offset, Ordering::Release);
    }
}

impl Namespace for TimeNamespace {
    fn ns_type(&self) -> NsType {
        NsType::Time
    }

    fn inum(&self) -> u64 {
        self.inum
    }
}

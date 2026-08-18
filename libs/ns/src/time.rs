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
    pub fn offset(&self, clock_id: i32) -> i64 {
        match clock_id {
            0 => self.realtime_offset.load(Ordering::Acquire), // CLOCK_REALTIME
            1 => self.monotonic_offset.load(Ordering::Acquire), // CLOCK_MONOTONIC
            7 => self.boottime_offset.load(Ordering::Acquire), // CLOCK_BOOTTIME
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

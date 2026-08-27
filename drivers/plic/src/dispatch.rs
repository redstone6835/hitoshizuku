//! PLIC 路由、批处理与卸载门控的纯逻辑。

use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DrainResult {
    pub(crate) claimed: usize,
    pub(crate) handled: usize,
    pub(crate) unhandled: usize,
    pub(crate) invalid: Option<u32>,
    pub(crate) exhausted: bool,
}

/// 按“claim、设备处理、complete”的顺序有界排空当前 context。
pub(crate) fn drain_pending(
    ndev: u32,
    budget: usize,
    mut claim: impl FnMut() -> u32,
    mut dispatch: impl FnMut(u32) -> bool,
    mut complete: impl FnMut(u32),
) -> DrainResult {
    let mut result = DrainResult::default();
    for _ in 0..budget {
        let hwirq = claim();
        if hwirq == 0 {
            return result;
        }

        result.claimed += 1;
        if hwirq > ndev {
            complete(hwirq);
            result.invalid = Some(hwirq);
            return result;
        }

        if dispatch(hwirq) {
            result.handled += 1;
        } else {
            result.unhandled += 1;
        }
        complete(hwirq);
    }
    result.exhausted = budget != 0;
    result
}

/// 同一 source 在一次 enable 生命周期内只报告一次未处理状态。
pub(crate) fn mark_unhandled_once(reported: &AtomicBool) -> bool {
    !reported.swap(true, Ordering::Relaxed)
}

/// 跟踪在途 PLIC 分发，并为驱动卸载提供关闭屏障。
pub(crate) struct DispatchGate {
    closed: AtomicBool,
    active: AtomicUsize,
}

impl DispatchGate {
    pub(crate) const fn new() -> Self {
        Self {
            closed: AtomicBool::new(false),
            active: AtomicUsize::new(0),
        }
    }

    pub(crate) fn try_enter(&self) -> Option<DispatchGuard<'_>> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        if self.closed.load(Ordering::Acquire) {
            self.active.fetch_sub(1, Ordering::Release);
            return None;
        }
        Some(DispatchGuard { gate: self })
    }

    pub(crate) fn close(&self) -> bool {
        !self.closed.swap(true, Ordering::AcqRel)
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn wait_for_idle(&self) {
        while self.active.load(Ordering::Acquire) != 0 {
            spin_loop();
        }
    }
}

pub(crate) struct DispatchGuard<'a> {
    gate: &'a DispatchGate,
}

impl Drop for DispatchGuard<'_> {
    fn drop(&mut self) {
        self.gate.active.fetch_sub(1, Ordering::Release);
    }
}

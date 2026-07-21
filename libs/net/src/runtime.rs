//! 网络运行时的并发协调原语。

use core::sync::atomic::{AtomicU8, Ordering, fence};

const WORK_PENDING: u8 = 1 << 0;
const OWNER_SLEEPING: u8 = 1 << 1;

/// 协调单一 owner 与多个工作发布者的睡眠和唤醒。
///
/// 工作位与睡眠位必须位于同一个原子对象中。发布者先原子设置工作位，并从同一次
/// 修改返回值判断 owner 是否已经发布睡眠意图；owner 则先原子设置睡眠位，再检查
/// 工作位。两种操作因此具有统一的原子修改顺序，不会出现双方同时读取旧值而遗漏
/// 唤醒的 store-buffering 窗口。
#[derive(Debug)]
pub struct WorkSignal {
    state: AtomicU8,
}

impl WorkSignal {
    /// 创建无待处理工作且 owner 处于运行状态的信号。
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
        }
    }

    /// 发布一份工作，并返回调用方是否必须唤醒 owner。
    ///
    /// 即使工作位原本已经置位，只要 owner 已发布睡眠意图，本方法仍返回 `true`。
    /// 调用方不得再使用“工作位是否从零变一”来决定是否唤醒。
    #[must_use]
    #[inline]
    pub fn publish_work(&self) -> bool {
        self.state.fetch_or(WORK_PENDING, Ordering::AcqRel) & OWNER_SLEEPING != 0
    }

    /// 在一轮排空结束后清除工作位，并在清除后重新检查所有实际工作源。
    ///
    /// `work_visible` 必须在本方法内部执行，不能由调用方预先计算，否则发布者可能在
    /// 预计算与清除工作位之间插入工作，随后又被 owner 覆盖掉。
    #[inline]
    pub fn finish_drain<F>(&self, work_visible: F) -> bool
    where
        F: FnOnce() -> bool,
    {
        self.state.fetch_and(!WORK_PENDING, Ordering::AcqRel);
        fence(Ordering::SeqCst);
        if work_visible() {
            self.state.fetch_or(WORK_PENDING, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// 发布 owner 的睡眠意图，并返回当前是否仍可继续进入睡眠。
    ///
    /// 返回 `false` 表示准备睡眠前已经存在工作。调用方无论是否真正切换任务状态，
    /// 最终都必须调用 [`Self::end_sleep`] 撤销睡眠意图。
    #[must_use]
    #[inline]
    pub fn begin_sleep(&self) -> bool {
        let previous = self.state.fetch_or(OWNER_SLEEPING, Ordering::AcqRel);
        debug_assert_eq!(
            previous & OWNER_SLEEPING,
            0,
            "同一个工作信号不能嵌套发布睡眠意图"
        );
        previous & WORK_PENDING == 0
    }

    /// 检查准备睡眠期间是否有发布者提交了工作。
    #[must_use]
    #[inline]
    pub fn sleep_invalidated(&self) -> bool {
        self.state.load(Ordering::Acquire) & WORK_PENDING != 0
    }

    /// 撤销 owner 的睡眠意图。
    #[inline]
    pub fn end_sleep(&self) {
        let previous = self.state.fetch_and(!OWNER_SLEEPING, Ordering::Release);
        debug_assert_ne!(
            previous & OWNER_SLEEPING,
            0,
            "工作信号不存在可撤销的睡眠意图"
        );
    }
}

impl Default for WorkSignal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::WorkSignal;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    #[test]
    fn 先发布的工作阻止_owner_睡眠() {
        let signal = WorkSignal::new();

        assert!(!signal.publish_work());
        assert!(!signal.begin_sleep());
        assert!(signal.sleep_invalidated());
        signal.end_sleep();
    }

    #[test]
    fn 睡眠发布后每次工作发布都要求唤醒() {
        let signal = WorkSignal::new();

        assert!(signal.begin_sleep());
        assert!(signal.publish_work());
        assert!(signal.publish_work());
        assert!(signal.sleep_invalidated());
        signal.end_sleep();
    }

    #[test]
    fn 排空必须在清除工作位后复查实际工作源() {
        let signal = WorkSignal::new();

        assert!(!signal.publish_work());
        assert!(signal.finish_drain(|| true));
        assert!(!signal.begin_sleep());
        signal.end_sleep();
    }

    #[test]
    fn 完全排空后允许重新进入睡眠() {
        let signal = WorkSignal::new();

        assert!(!signal.publish_work());
        assert!(!signal.finish_drain(|| false));
        assert!(signal.begin_sleep());
        assert!(!signal.sleep_invalidated());
        signal.end_sleep();
    }

    #[test]
    fn 并发发布与睡眠准备不会遗漏工作() {
        const ROUNDS: usize = 10_000;

        let signal = Arc::new(WorkSignal::new());
        let start = Arc::new(Barrier::new(2));
        let published = Arc::new(Barrier::new(2));
        let reset = Arc::new(Barrier::new(2));
        let wake_required = Arc::new(AtomicBool::new(false));

        thread::scope(|scope| {
            scope.spawn({
                let signal = Arc::clone(&signal);
                let start = Arc::clone(&start);
                let published = Arc::clone(&published);
                let reset = Arc::clone(&reset);
                let wake_required = Arc::clone(&wake_required);
                move || {
                    for _ in 0..ROUNDS {
                        start.wait();
                        wake_required.store(signal.publish_work(), Ordering::Release);
                        published.wait();
                        reset.wait();
                    }
                }
            });

            for _ in 0..ROUNDS {
                assert!(!signal.finish_drain(|| false));
                wake_required.store(false, Ordering::Relaxed);
                start.wait();
                let can_sleep = signal.begin_sleep();
                published.wait();

                assert!(signal.sleep_invalidated());
                if can_sleep {
                    assert!(wake_required.load(Ordering::Acquire));
                }

                signal.end_sleep();
                reset.wait();
            }
        });
    }
}

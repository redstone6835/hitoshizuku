//! TTY 输入泵的 timer tick 桥接。
//!
//! 字符设备驱动只负责报告底层 FIFO 可读；终端控制字符属于 devtmpfs/TTY 兼
//! 容层的行规程。主路径由 timer/IRQ hook 推进；低优先级内核线程只作为兜底，
//! 避免某些虚拟串口后端没有可靠产生外部中断时控制字符长期滞留。

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static POLLER_STARTED: AtomicBool = AtomicBool::new(false);
static LAST_HOOK_NS: AtomicU64 = AtomicU64::new(u64::MAX);
static POLL_REQUESTED: AtomicBool = AtomicBool::new(false);
const TTY_POLLER_FALLBACK_INTERVAL_NS: u64 = 10_000_000;

/// Timer/IRQ 钩子：通知 TTY kthread 推进一次行规程输入处理。
pub fn tick_tty_poll(now_ns: u64) {
    if LAST_HOOK_NS.swap(now_ns, Ordering::AcqRel) == now_ns {
        return;
    }
    POLL_REQUESTED.store(true, Ordering::Release);
}

unsafe extern "C" fn tty_poll_worker(_arg: usize) -> ! {
    loop {
        wait_for_request_or_timeout();
        let _ = POLL_REQUESTED.swap(false, Ordering::AcqRel);
        general::vfs::devtmpfs::poll_tty_input();
    }
}

fn wait_for_request_or_timeout() {
    if POLL_REQUESTED.load(Ordering::Acquire) {
        return;
    }

    let task = sched::current_task_direct();
    let now = sched::now_ns_direct();
    let deadline = now.saturating_add(TTY_POLLER_FALLBACK_INTERVAL_NS);

    // TTY 行规程会触碰 VFS/TTY 锁，必须在进程上下文运行。hook 只做原子置位，
    // 不在 IRQ 上下文拿任何锁；kthread 最多 10ms 醒一次，兼顾 Ctrl-C 延迟和
    // 后台负载。若置位发生在睡前窗口，下面的二次检查会直接返回。
    if !task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping)
        && !task.cas_state(sched::TaskState::Runnable, sched::TaskState::Sleeping)
    {
        let _ = sched::operation::sched_yield();
        return;
    }
    let deadline_armed = sched::register_sleep_deadline(&task, deadline);
    if POLL_REQUESTED.load(Ordering::Acquire) || !deadline_armed {
        restore_current_task_after_sleep(&task);
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        } else {
            let _ = sched::operation::sched_yield();
        }
        return;
    }

    sched::schedule_once(now);
    sched::cancel_sleep_deadline(&task);
    restore_current_task_after_sleep(&task);
}

fn restore_current_task_after_sleep(task: &alloc::sync::Arc<sched::Task>) {
    if !task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running) {
        let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Running);
    }
}

/// 注册 TTY 输入泵 tick hook。
pub fn register() {
    hal::user::register_tty_poll_hook(tick_tty_poll);
    if POLLER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = sched::kthread_spawn(
        tty_poll_worker,
        0,
        sched::SchedParams {
            nice: 19,
            slice_ns: 0,
        },
    );
}

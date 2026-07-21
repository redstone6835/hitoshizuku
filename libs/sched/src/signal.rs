//! POSIX 信号：类型、集合、动作、per-task 与 per-tg 状态。
//!
//! 本实现覆盖 Linux 常用 64 个信号（标准 1..=31 + 实时 32..=64）。SIGKILL/SIGSTOP
//! 不可被 handler / blocked / ignored —— 投递路径直接走默认动作。

use alloc::sync::Weak;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::ids::Uid;
use crate::pid::PidT;
use crate::sync::Spinlock;

/// 支持的最大信号数（含 0 号无效位，实际可用 1..=NSIG-1）。
pub const NSIG: usize = 65;

/// 标准信号号码（对齐 Linux）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalNumber(u8);

impl SignalNumber {
    /// 从原始 `i32` 构造；越界或非法返回 `None`。
    pub const fn from_raw(n: i32) -> Option<Self> {
        if n >= 1 && (n as usize) < NSIG {
            Some(Self(n as u8))
        } else {
            None
        }
    }
    pub const fn raw(self) -> u8 {
        self.0
    }
    pub const fn as_usize(self) -> usize {
        self.0 as usize
    }
    pub const fn bit(self) -> u64 {
        1u64 << ((self.0 - 1) as u64)
    }
}

macro_rules! sig_const {
    ($($name:ident = $val:expr),* $(,)?) => {
        impl SignalNumber {
            $(pub const $name: Self = Self($val);)*
        }
    };
}

sig_const!(
    SIGHUP = 1,
    SIGINT = 2,
    SIGQUIT = 3,
    SIGILL = 4,
    SIGTRAP = 5,
    SIGABRT = 6,
    SIGBUS = 7,
    SIGFPE = 8,
    SIGKILL = 9,
    SIGUSR1 = 10,
    SIGSEGV = 11,
    SIGUSR2 = 12,
    SIGPIPE = 13,
    SIGALRM = 14,
    SIGTERM = 15,
    SIGCHLD = 17,
    SIGCONT = 18,
    SIGSTOP = 19,
    SIGTSTP = 20,
    SIGTTIN = 21,
    SIGTTOU = 22,
    SIGURG = 23,
    SIGWINCH = 28,
);

/// 64 位信号位集。Linux/POSIX sigset 编码用 bit(signo - 1) 表示信号 signo。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SigSet(pub u64);

impl SigSet {
    pub const EMPTY: Self = Self(0);
    pub const fn has(self, sig: SignalNumber) -> bool {
        (self.0 & sig.bit()) != 0
    }
    pub const fn with(self, sig: SignalNumber) -> Self {
        Self(self.0 | sig.bit())
    }
    pub const fn without(self, sig: SignalNumber) -> Self {
        Self(self.0 & !sig.bit())
    }
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
    pub const fn from_raw(bits: u64) -> Self {
        Self(bits)
    }

    /// SIGKILL 与 SIGSTOP 不可屏蔽——从集合里剥掉它们。
    pub const fn sanitized(self) -> Self {
        Self(self.0 & !(SignalNumber::SIGKILL.bit() | SignalNumber::SIGSTOP.bit()))
    }
}

/// 用户态 sigaction handler 地址。具体 sigframe / sigreturn 由上层注册的
/// ProcessImageOps 负责，sched 只保存动作语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigHandler {
    /// 按 [`default_action`] 决定。
    Default,
    /// 静默丢弃。
    Ignore,
    /// 用户态处理函数地址。
    Handler(usize),
}

impl Default for SigHandler {
    fn default() -> Self {
        Self::Default
    }
}

/// `sigprocmask` 的 how 参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigProcMaskHow {
    Block,
    Unblock,
    SetMask,
}

/// sigaction flags 的最小子集。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SigActionFlags(pub u32);

impl SigActionFlags {
    pub const SA_ONSTACK: u32 = 0x08000000;
    pub const SA_NODEFER: u32 = 0x40000000;
    pub const SA_RESETHAND: u32 = 0x80000000;
    pub const SA_RESTART: u32 = 0x10000000;
    pub const SA_SIGINFO: u32 = 0x00000004;

    pub const fn raw(self) -> u32 {
        self.0
    }
    pub const fn has(self, flag: u32) -> bool {
        (self.0 & flag) != 0
    }
}

/// sigaction 表项。
#[derive(Debug, Clone, Copy)]
pub struct SigAction {
    pub handler: SigHandler,
    pub mask: SigSet,
    pub flags: SigActionFlags,
    pub restorer: usize,
}

impl SigAction {
    pub const fn default_new() -> Self {
        Self {
            handler: SigHandler::Default,
            mask: SigSet::EMPTY,
            flags: SigActionFlags(0),
            restorer: 0,
        }
    }
}

impl Default for SigAction {
    fn default() -> Self {
        Self::default_new()
    }
}

/// siginfo 的内核表示。
///
/// `raw` 用于保留 `rt_sigqueueinfo`/`rt_tgsigqueueinfo` 这类用户态排队信号的
/// 完整 Linux ABI payload；内核自生成信号只填 typed 字段并保持 `raw = None`。
#[derive(Debug, Clone, Copy)]
pub struct SigInfo {
    pub sig: SignalNumber,
    pub code: i32,
    pub sender_pid: PidT,
    pub sender_uid: Uid,
    pub raw: Option<[u8; 128]>,
}

/// 默认动作分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultAction {
    /// 结束进程。
    Term,
    /// 结束进程并打印 core（占位等同 Term）。
    Core,
    /// 停止进程。
    Stop,
    /// 恢复停止的进程。
    Cont,
    /// 静默忽略。
    Ign,
}

/// 按信号号返回默认动作。
pub const fn default_action(sig: SignalNumber) -> DefaultAction {
    match sig.raw() {
        // Term
        1 | 2 | 10 | 12 | 13 | 14 | 15 => DefaultAction::Term,
        // Core
        3 | 4 | 6 | 7 | 8 | 11 | 31 => DefaultAction::Core,
        // Stop
        19 | 20 | 21 | 22 => DefaultAction::Stop,
        // Cont
        18 => DefaultAction::Cont,
        // Ign
        17 | 23 | 28 | 29 => DefaultAction::Ign,
        // 实时信号默认 Term
        _ => DefaultAction::Term,
    }
}

// ── per-task 信号状态 ────────────────────────────────────────────────────────

/// 订阅某个 per-task 或 thread-group 信号状态的定向观察者。
pub trait SignalObserver: Send + Sync {
    fn signal_state_changed(&self);
}

struct SignalSubscription {
    id: u64,
    observer: Weak<dyn SignalObserver>,
}

struct SignalObservers {
    has_entries: AtomicBool,
    next_id: AtomicU64,
    entries: Spinlock<Vec<SignalSubscription>>,
}

impl SignalObservers {
    const fn new() -> Self {
        Self {
            has_entries: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            entries: Spinlock::new(Vec::new()),
        }
    }

    fn subscribe(&self, observer: Weak<dyn SignalObserver>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        assert!(id != 0, "signal observer id 已耗尽");
        self.entries
            .lock()
            .push(SignalSubscription { id, observer });
        self.has_entries.store(true, Ordering::Release);
    }

    fn notify(&self) {
        if !self.has_entries.load(Ordering::Acquire) {
            return;
        }
        {
            let mut entries = self.entries.lock();
            entries.retain(|entry| entry.observer.strong_count() != 0);
            if entries.is_empty() {
                self.has_entries.store(false, Ordering::Release);
                return;
            }
        }
        let mut after = 0u64;
        loop {
            let next = {
                let entries = self.entries.lock();
                let index = entries.partition_point(|entry| entry.id <= after);
                entries
                    .get(index)
                    .map(|entry| (entry.id, entry.observer.clone()))
            };
            let Some((id, observer)) = next else {
                break;
            };
            after = id;
            if let Some(observer) = observer.upgrade() {
                observer.signal_state_changed();
            }
        }
    }
}

/// 每个 Task 自带的信号上下文。
pub struct SignalState {
    pending_bits: AtomicU64,
    pending_infos: Spinlock<Vec<SigInfo>>,
    blocked: AtomicU64,
    saved_blocked: AtomicU64,
    sigtimedwait_mask: AtomicU64,
    observers: SignalObservers,
}

impl SignalState {
    pub const fn new() -> Self {
        Self {
            pending_bits: AtomicU64::new(0),
            pending_infos: Spinlock::new(Vec::new()),
            blocked: AtomicU64::new(0),
            saved_blocked: AtomicU64::new(0),
            sigtimedwait_mask: AtomicU64::new(0),
            observers: SignalObservers::new(),
        }
    }

    /// 订阅当前任务的 pending 信号变化。
    pub fn subscribe(&self, observer: Weak<dyn SignalObserver>) {
        self.observers.subscribe(observer);
    }

    /// 把一条信号投到本 task 的 pending。
    pub fn deliver(&self, info: SigInfo) {
        self.pending_bits.fetch_or(info.sig.bit(), Ordering::AcqRel);
        self.pending_infos.lock().push(info);
        self.observers.notify();
    }

    /// 取出一条当前未被 block 的信号；无匹配返回 None。
    pub fn dequeue_one(&self) -> Option<SigInfo> {
        let blocked = self.blocked.load(Ordering::Acquire);
        let mut queue = self.pending_infos.lock();
        let idx = queue.iter().position(|i| (blocked & i.sig.bit()) == 0)?;
        let info = queue.swap_remove(idx);
        // 若该信号已经没有其它实例，则清掉位图。
        let still_has = queue.iter().any(|i| i.sig == info.sig);
        if !still_has {
            self.pending_bits
                .fetch_and(!info.sig.bit(), Ordering::AcqRel);
        }
        drop(queue);
        self.observers.notify();
        Some(info)
    }

    /// sigtimedwait 用：从 per-task pending 里取出一条属于 `these` 集合的信号。
    /// sigtimedwait 显式等待调用方给定集合，不再受当前 blocked mask 过滤。
    pub fn dequeue_one_in(&self, these: u64) -> Option<SigInfo> {
        let mut queue = self.pending_infos.lock();
        let idx = queue.iter().position(|i| (these & i.sig.bit()) != 0)?;
        let info = queue.swap_remove(idx);
        let still_has = queue.iter().any(|i| i.sig == info.sig);
        if !still_has {
            self.pending_bits
                .fetch_and(!info.sig.bit(), Ordering::AcqRel);
        }
        drop(queue);
        self.observers.notify();
        Some(info)
    }

    /// 是否存在属于 `these` 的 pending 信号；不消费队列，不受 blocked mask 过滤。
    pub fn has_pending_in(&self, these: u64) -> bool {
        (self.pending_bits.load(Ordering::Acquire) & these) != 0
    }

    /// 是否有 pending 信号（不限 these 集合）。
    pub fn has_any_pending(&self) -> bool {
        self.pending_bits.load(Ordering::Acquire) != 0
    }

    /// 是否存在至少一条可投递信号（未屏蔽）。
    pub fn has_deliverable(&self) -> bool {
        let pending = self.pending_bits.load(Ordering::Acquire);
        let blocked = self.blocked.load(Ordering::Acquire);
        (pending & !blocked) != 0
    }

    /// 当前 pending 位图快照。
    pub fn pending_snapshot(&self) -> SigSet {
        SigSet(self.pending_bits.load(Ordering::Acquire))
    }

    /// blocked 位图快照。
    pub fn blocked_snapshot(&self) -> SigSet {
        SigSet(self.blocked.load(Ordering::Acquire))
    }

    /// 记录当前线程正在 sigtimedwait 显式等待的集合，供共享信号投递路径唤醒。
    pub fn begin_sigtimedwait(&self, set: SigSet) {
        self.sigtimedwait_mask.store(set.0, Ordering::Release);
    }

    pub fn end_sigtimedwait(&self) {
        self.sigtimedwait_mask.store(0, Ordering::Release);
    }

    pub fn sigtimedwait_wants(&self, sig: SignalNumber) -> bool {
        (self.sigtimedwait_mask.load(Ordering::Acquire) & sig.bit()) != 0
    }

    /// sigprocmask 修改。SIGKILL/SIGSTOP 自动剥离。
    pub fn block(&self, set: SigSet, how: SigProcMaskHow) -> SigSet {
        let set = set.sanitized();
        let prev = self.blocked.load(Ordering::Acquire);
        let next = match how {
            SigProcMaskHow::Block => prev | set.0,
            SigProcMaskHow::Unblock => prev & !set.0,
            SigProcMaskHow::SetMask => set.0,
        };
        self.blocked.store(next, Ordering::Release);
        SigSet(prev)
    }

    /// sigsuspend 用：保存当前 mask，临时换一个。
    pub fn save_blocked(&self, new_mask: SigSet) {
        let prev = self.blocked.load(Ordering::Acquire);
        self.saved_blocked.store(prev, Ordering::Release);
        self.blocked
            .store(new_mask.sanitized().0, Ordering::Release);
    }

    /// sigreturn 用：恢复保存的 mask。
    pub fn restore_blocked(&self) {
        let saved = self.saved_blocked.load(Ordering::Acquire);
        self.blocked.store(saved, Ordering::Release);
    }
}

impl Default for SignalState {
    fn default() -> Self {
        Self::new()
    }
}

// ── per-thread-group 信号状态 ────────────────────────────────────────────────

/// ThreadGroup 共享的信号表：sigaction + 进程级 pending。
pub struct SharedSignal {
    actions: Spinlock<[SigAction; NSIG]>,
    shared_pending_bits: AtomicU64,
    shared_pending_infos: Spinlock<Vec<SigInfo>>,
    observers: SignalObservers,
}

impl SharedSignal {
    pub fn new() -> Self {
        Self {
            actions: Spinlock::new([SigAction::default_new(); NSIG]),
            shared_pending_bits: AtomicU64::new(0),
            shared_pending_infos: Spinlock::new(Vec::new()),
            observers: SignalObservers::new(),
        }
    }

    /// 深拷一份（不 CLONE_SIGHAND 时）；pending 不复制。
    pub fn fork_copy(&self) -> Self {
        let actions_copy = *self.actions.lock();
        Self {
            actions: Spinlock::new(actions_copy),
            shared_pending_bits: AtomicU64::new(0),
            shared_pending_infos: Spinlock::new(Vec::new()),
            observers: SignalObservers::new(),
        }
    }

    /// 为 `CLONE_CLEAR_SIGHAND` 深拷信号表，并把父进程中已捕获的信号恢复
    /// 为默认 disposition。`SIG_IGN` 按 Linux 语义继续保持忽略。
    pub fn fork_copy_clearing_handlers(&self) -> Self {
        let copied = self.fork_copy();
        copied.reset_caught_handlers();
        copied
    }

    /// 订阅当前线程组共享 pending 信号变化。
    pub fn subscribe(&self, observer: Weak<dyn SignalObserver>) {
        self.observers.subscribe(observer);
    }

    pub fn get_action(&self, sig: SignalNumber) -> SigAction {
        self.actions.lock()[sig.as_usize()]
    }

    /// 写 sigaction；SIGKILL/SIGSTOP 不允许改——调用方负责拒绝。
    pub fn set_action(&self, sig: SignalNumber, new: SigAction) -> SigAction {
        let mut guard = self.actions.lock();
        let old = guard[sig.as_usize()];
        guard[sig.as_usize()] = new;
        old
    }

    /// execve 时按 POSIX 重置信号处理：所有 caught 信号恢复为 SIG_DFL，
    /// SIG_IGN 保持（除 SIGCHLD 特殊情况）。SIGKILL/SIGSTOP 不可改，跳过。
    pub fn reset_handlers_for_exec(&self) {
        self.reset_caught_handlers();
    }

    fn reset_caught_handlers(&self) {
        let mut guard = self.actions.lock();
        for sig_idx in 0..guard.len() {
            let sig = SignalNumber::from_raw(sig_idx as i32);
            let action = guard[sig_idx];
            match action.handler {
                SigHandler::Handler(_) => {
                    guard[sig_idx] = SigAction {
                        handler: SigHandler::Default,
                        flags: SigActionFlags(0),
                        mask: SigSet(0),
                        restorer: 0,
                    };
                }
                SigHandler::Ignore => {
                    // TODO: SIG_IGN 跨 exec 保持（SIGCHLD 有特殊语义但这里先保持）
                }
                SigHandler::Default => {
                    // 已经 SIG_DFL，不变
                }
            }
            let _ = sig;
        }
    }

    /// 投一条信号到 tg 的共享 pending。
    pub fn deliver(&self, info: SigInfo) {
        self.shared_pending_bits
            .fetch_or(info.sig.bit(), Ordering::AcqRel);
        self.shared_pending_infos.lock().push(info);
        self.observers.notify();
    }

    /// 取出一条与 per-task `blocked` 不冲突的信号。
    pub fn dequeue_one(&self, blocked: u64) -> Option<SigInfo> {
        let mut queue = self.shared_pending_infos.lock();
        let idx = queue.iter().position(|i| (blocked & i.sig.bit()) == 0)?;
        let info = queue.swap_remove(idx);
        let still_has = queue.iter().any(|i| i.sig == info.sig);
        if !still_has {
            self.shared_pending_bits
                .fetch_and(!info.sig.bit(), Ordering::AcqRel);
        }
        drop(queue);
        self.observers.notify();
        Some(info)
    }

    /// sigtimedwait 用：从 tg 共享 pending 里取出一条属于 `these` 集合的信号。
    /// 不受调用线程当前 blocked mask 过滤。
    ///
    /// `these` 的含义是"调用方想要消费的信号集"——通常在
    /// `rt_sigtimedwait(uthese, ...)` 中由用户态直接传入。
    pub fn dequeue_one_in(&self, these: u64) -> Option<SigInfo> {
        let mut queue = self.shared_pending_infos.lock();
        let idx = queue.iter().position(|i| (these & i.sig.bit()) != 0)?;
        let info = queue.swap_remove(idx);
        let still_has = queue.iter().any(|i| i.sig == info.sig);
        if !still_has {
            self.shared_pending_bits
                .fetch_and(!info.sig.bit(), Ordering::AcqRel);
        }
        drop(queue);
        self.observers.notify();
        Some(info)
    }

    /// 是否存在属于 `these` 的共享 pending 信号；不消费队列。
    pub fn has_pending_in(&self, these: u64) -> bool {
        (self.shared_pending_bits.load(Ordering::Acquire) & these) != 0
    }

    pub fn pending_snapshot(&self) -> SigSet {
        SigSet(self.shared_pending_bits.load(Ordering::Acquire))
    }

    pub fn pending_len_hint(&self) -> usize {
        self.shared_pending_infos.lock().len()
    }
}

impl Default for SharedSignal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod observer_tests {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::AtomicU64;

    struct Observer(AtomicU64);

    impl SignalObserver for Observer {
        fn signal_state_changed(&self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn signal_state_notifies_only_its_subscribers_on_enqueue_and_dequeue() {
        let state = SignalState::new();
        let observer = Arc::new(Observer(AtomicU64::new(0)));
        let subscriber: Arc<dyn SignalObserver> = observer.clone();
        state.subscribe(Arc::downgrade(&subscriber));
        state.deliver(SigInfo {
            sig: SignalNumber::SIGUSR1,
            code: 0,
            sender_pid: 1,
            sender_uid: Uid(0),
            raw: None,
        });
        assert_eq!(observer.0.load(Ordering::Acquire), 1);
        assert!(state.dequeue_one_in(SignalNumber::SIGUSR1.bit()).is_some());
        assert_eq!(observer.0.load(Ordering::Acquire), 2);
    }
}

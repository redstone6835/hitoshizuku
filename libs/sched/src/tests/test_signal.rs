//! POSIX 信号集与信号号码测试。
//!
//! 验证 SigSet 的位操作（has/with/without/union/intersection）与
//! SignalNumber 的合法性检查和位转换。信号 N 使用 bit(N - 1)，与用户态
//! sigset 编码保持一致。

extern crate std;

use crate::ids::Uid;
use crate::signal::{
    SharedSignal, SigAction, SigActionFlags, SigHandler, SigInfo, SigProcMaskHow, SigSet,
    SignalNumber, SignalState,
};
use ktest::ktest;

/// 空集不包含任何信号。
#[ktest]
fn sigset_empty() {
    assert!(!SigSet::EMPTY.has(SignalNumber::SIGKILL));
    assert!(!SigSet::EMPTY.has(SignalNumber::SIGTERM));
}

/// CLONE_CLEAR_SIGHAND 只重置已捕获的 handler，SIG_IGN 在子进程中保持。
#[ktest]
fn clear_sighand_copy_resets_handlers_but_keeps_ignored_signals() {
    let parent = SharedSignal::new();
    let caught = SigAction {
        handler: SigHandler::Handler(0x1234),
        mask: SigSet::EMPTY.with(SignalNumber::SIGTERM),
        flags: SigActionFlags(SigActionFlags::SA_RESTART),
        restorer: 0x5678,
    };
    let ignored = SigAction {
        handler: SigHandler::Ignore,
        mask: SigSet::EMPTY,
        flags: SigActionFlags(0),
        restorer: 0,
    };
    parent.set_action(SignalNumber::SIGUSR1, caught);
    parent.set_action(SignalNumber::SIGUSR2, ignored);

    let child = parent.fork_copy_clearing_handlers();
    let child_caught = child.get_action(SignalNumber::SIGUSR1);
    let child_ignored = child.get_action(SignalNumber::SIGUSR2);

    assert!(matches!(child_caught.handler, SigHandler::Default));
    assert_eq!(child_caught.mask, SigSet::EMPTY);
    assert_eq!(child_caught.flags, SigActionFlags(0));
    assert_eq!(child_caught.restorer, 0);
    assert!(matches!(child_ignored.handler, SigHandler::Ignore));
    assert!(matches!(
        parent.get_action(SignalNumber::SIGUSR1).handler,
        SigHandler::Handler(0x1234)
    ));
}

/// with 添加信号后 has 返回 true。
#[ktest]
fn sigset_with_has() {
    let s = SigSet::EMPTY.with(SignalNumber::SIGKILL);
    assert!(s.has(SignalNumber::SIGKILL));
    assert!(!s.has(SignalNumber::SIGTERM));
}

/// without 移除信号后 has 返回 false。
#[ktest]
fn sigset_without() {
    let s = SigSet::EMPTY
        .with(SignalNumber::SIGKILL)
        .without(SignalNumber::SIGKILL);
    assert!(!s.has(SignalNumber::SIGKILL));
}

/// union 合并两个集合，保留双方的信号。
#[ktest]
fn sigset_union() {
    let a = SigSet::EMPTY.with(SignalNumber::SIGKILL);
    let b = SigSet::EMPTY.with(SignalNumber::SIGTERM);
    let u = a.union(b);
    assert!(u.has(SignalNumber::SIGKILL));
    assert!(u.has(SignalNumber::SIGTERM));
}

/// intersection 仅保留双方共有的信号。
#[ktest]
fn sigset_intersection() {
    let a = SigSet::EMPTY
        .with(SignalNumber::SIGKILL)
        .with(SignalNumber::SIGTERM);
    let b = SigSet::EMPTY.with(SignalNumber::SIGKILL);
    let i = a.intersection(b);
    assert!(i.has(SignalNumber::SIGKILL));
    assert!(!i.has(SignalNumber::SIGTERM));
}

/// from_raw 构造后 raw() 往返一致。
#[ktest]
fn sigset_raw_roundtrip() {
    let s = SigSet::from_raw(0xdead_beef);
    assert_eq!(s.raw(), 0xdead_beef);
}

/// sigsuspend 的旧 mask 只交给首个 handler frame，不能被嵌套投递重复消费。
#[ktest]
fn sigsuspend_saved_mask_is_consumed_once() {
    let state = SignalState::new();
    let original = SigSet::EMPTY.with(SignalNumber::SIGUSR1);
    let temporary = SigSet::EMPTY.with(SignalNumber::SIGUSR2);
    state.block(original, SigProcMaskHow::SetMask);

    state.save_blocked(temporary);

    assert_eq!(state.blocked_snapshot(), temporary);
    assert_eq!(state.take_sigsuspend_saved_blocked(), Some(original));
    assert_eq!(state.take_sigsuspend_saved_blocked(), None);
}

/// 未建立 handler frame 时显式恢复会同时清除待消费标记。
#[ktest]
fn sigsuspend_explicit_restore_clears_saved_mask() {
    let state = SignalState::new();
    let original = SigSet::EMPTY.with(SignalNumber::SIGUSR1);
    state.block(original, SigProcMaskHow::SetMask);
    state.save_blocked(SigSet::EMPTY);

    state.restore_blocked();

    assert_eq!(state.blocked_snapshot(), original);
    assert_eq!(state.take_sigsuspend_saved_blocked(), None);
}

/// 信号编号 1..=64 为合法，构造成功。
#[ktest]
fn signal_number_from_raw_valid() {
    assert!(SignalNumber::from_raw(1).is_some());
    assert!(SignalNumber::from_raw(31).is_some());
    assert!(SignalNumber::from_raw(64).is_some());
}

/// 0、65、-1 均为非法信号编号，返回 None。
#[ktest]
fn signal_number_from_raw_invalid() {
    assert!(SignalNumber::from_raw(0).is_none());
    assert!(SignalNumber::from_raw(65).is_none());
    assert!(SignalNumber::from_raw(-1).is_none());
}

/// 信号 N 的位掩码为 1 << (N - 1)。
#[ktest]
fn signal_number_bit() {
    assert_eq!(SignalNumber::SIGKILL.bit(), 1 << 8);
    assert_eq!(SignalNumber::SIGHUP.bit(), 1);
}

/// as_usize 返回信号编号的数值。
#[ktest]
fn signal_number_as_usize() {
    assert_eq!(SignalNumber::SIGKILL.as_usize(), 9);
}

/// CLONE_CLEAR_SIGHAND 的信号表副本清除用户处理函数，但保留 SIG_IGN。
#[ktest]
fn clear_sighand_copy_resets_handlers_and_keeps_ignored_actions() {
    let source = SharedSignal::new();
    source.set_action(
        SignalNumber::SIGUSR1,
        SigAction {
            handler: SigHandler::Handler(0x1234),
            mask: SigSet::EMPTY.with(SignalNumber::SIGTERM),
            flags: SigActionFlags(SigActionFlags::SA_RESTART),
            restorer: 0x5678,
        },
    );
    source.set_action(
        SignalNumber::SIGUSR2,
        SigAction {
            handler: SigHandler::Ignore,
            ..SigAction::default_new()
        },
    );

    let copied = source.fork_copy_clearing_handlers();

    assert_eq!(
        copied.get_action(SignalNumber::SIGUSR1).handler,
        SigHandler::Default
    );
    assert_eq!(
        copied.get_action(SignalNumber::SIGUSR2).handler,
        SigHandler::Ignore
    );
    assert_eq!(
        source.get_action(SignalNumber::SIGUSR1).handler,
        SigHandler::Handler(0x1234)
    );
}

/// CLONE_SIGHAND 共享处理表，但不能共享线程组 pending 队列。
#[ktest]
fn clone_sighand_shares_actions_without_sharing_pending() {
    let parent = SharedSignal::new();
    let child = parent.clone_sighand();
    let ignored = SigAction {
        handler: SigHandler::Ignore,
        flags: SigActionFlags(0),
        mask: SigSet::EMPTY,
        restorer: 0,
    };

    child.set_action(SignalNumber::SIGUSR2, ignored);
    assert_eq!(
        parent.get_action(SignalNumber::SIGUSR2).handler,
        SigHandler::Ignore
    );

    parent.deliver(SigInfo {
        sig: SignalNumber::SIGUSR2,
        code: 0,
        sender_pid: 1,
        sender_uid: Uid(0),
        raw: None,
    });
    assert!(parent.pending_snapshot().has(SignalNumber::SIGUSR2));
    assert!(!child.pending_snapshot().has(SignalNumber::SIGUSR2));
}

//! POSIX wait4/waitid 状态编码测试。
//!
//! 验证 WaitStatus 的构造/解码（exit/signal/stop/continued）与 WaitId 的
//! pid 参数解析。wstatus 编码与 Linux 位布局对齐。

extern crate std;

use crate::signal::SignalNumber;
use crate::wait_flags::{WaitId, WaitStatus};
use ktest::ktest;

/// from_exit 构造的 wstatus 满足 wifexited，退出码正确。
#[ktest]
fn exit_status() {
    let w = WaitStatus::from_exit(42);
    assert!(w.wifexited());
    assert_eq!(w.wexitstatus(), 42);
    assert!(!w.wifsignaled());
    assert!(!w.wifstopped());
}

/// from_signal 构造的 wstatus 满足 wifsignaled，终止信号正确。
#[ktest]
fn signal_status() {
    let w = WaitStatus::from_signal(SignalNumber::SIGKILL);
    assert!(w.wifsignaled());
    assert_eq!(w.wtermsig(), 9);
    assert!(!w.wifexited());
    assert!(!w.wcoredump());
}

/// from_signal_core 构造的 wstatus 同时满足 wifsignaled 和 wcoredump。
#[ktest]
fn signal_core() {
    let w = WaitStatus::from_signal_core(SignalNumber::SIGQUIT);
    assert!(w.wifsignaled());
    assert!(w.wcoredump());
}

/// from_stop 构造的 wstatus 满足 wifstopped，停止信号正确。
#[ktest]
fn stop_status() {
    let w = WaitStatus::from_stop(SignalNumber::SIGSTOP);
    assert!(w.wifstopped());
    assert_eq!(w.wstopsig(), 19);
    assert!(!w.wifexited());
}

/// continued() 构造的 wstatus 满足 wifcontinued。
#[ktest]
fn continued_status() {
    let w = WaitStatus::continued();
    assert!(w.wifcontinued());
}

/// pid > 0 时 from_wait4_pid 返回 Pid(pid)。
#[ktest]
fn waitid_from_pid_positive() {
    assert_eq!(WaitId::from_wait4_pid(42), WaitId::Pid(42));
}

/// pid == -1 时 from_wait4_pid 返回 All（任意子进程）。
#[ktest]
fn waitid_from_pid_all() {
    assert_eq!(WaitId::from_wait4_pid(-1), WaitId::All);
}

/// pid == 0 时 from_wait4_pid 返回 SameGroup（同进程组）。
#[ktest]
fn waitid_from_pid_same_group() {
    assert_eq!(WaitId::from_wait4_pid(0), WaitId::SameGroup);
}

/// pid < -1 时 from_wait4_pid 返回 Pgid(-pid)。
#[ktest]
fn waitid_from_pid_pgid() {
    assert_eq!(WaitId::from_wait4_pid(-5), WaitId::Pgid(5));
}

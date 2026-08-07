//! clone(2) 标志位测试。
//!
//! 验证 CloneFlags 的位检测、退出信号提取以及 fork/vfork 默认标志。
//! 标志位数值与 Linux UAPI 严格对齐。

extern crate std;

use crate::clone_flags::{CloneArgs, CloneFlags};
use errno::Errno;
use ktest::ktest;

/// CLONE_THREAD 位被正确检测。
#[ktest]
fn has_thread_flag() {
    let f = CloneFlags::from_raw(CloneFlags::CLONE_THREAD);
    assert!(f.has(CloneFlags::CLONE_THREAD));
    assert!(!f.has(CloneFlags::CLONE_VM));
}

/// CLONE_VM 位被正确检测。
#[ktest]
fn has_vm_flag() {
    let f = CloneFlags::from_raw(CloneFlags::CLONE_VM);
    assert!(f.has(CloneFlags::CLONE_VM));
}

/// CLONE_CLEAR_SIGHAND 位位于传统 32 位标志范围之外，解析时不得被截断。
#[ktest]
fn has_clear_sighand_flag() {
    let f = CloneFlags::from_raw(CloneFlags::CLONE_CLEAR_SIGHAND);
    assert!(f.has(CloneFlags::CLONE_CLEAR_SIGHAND));
    assert_eq!(CloneFlags::CLONE_CLEAR_SIGHAND, 1u64 << 32);
}

/// fork 默认标志仅包含 SIGCHLD (17)，不含 CLONE_VM/CLONE_THREAD。
#[ktest]
fn fork_default_flags() {
    let f = CloneFlags::fork_default();
    assert_eq!(f.exit_signal(), 17);
    assert!(!f.has(CloneFlags::CLONE_VM));
    assert!(!f.has(CloneFlags::CLONE_THREAD));
}

/// vfork 默认标志包含 CLONE_VFORK | CLONE_VM | SIGCHLD。
#[ktest]
fn vfork_default_flags() {
    let f = CloneFlags::vfork_default();
    assert!(f.has(CloneFlags::CLONE_VFORK));
    assert!(f.has(CloneFlags::CLONE_VM));
    assert_eq!(f.exit_signal(), 17);
}

/// CLONE_CLEAR_SIGHAND 使用 clone3 的高 32 位标志位，不能被截断。
#[ktest]
fn clear_sighand_flag_matches_linux_uapi() {
    let flags = CloneFlags::from_raw(CloneFlags::CLONE_CLEAR_SIGHAND);
    assert!(flags.has(CloneFlags::CLONE_CLEAR_SIGHAND));
    assert_eq!(flags.raw(), 0x1_00000000);
}

/// exit_signal 从低 8 位（CSIGNAL 掩码）提取退出信号编号。
#[ktest]
fn exit_signal_extract() {
    let f = CloneFlags::from_raw(0x0000_0009);
    assert_eq!(f.exit_signal(), 9);
}

/// CLONE_CLEAR_SIGHAND 使用 clone3 的 64 位 flag 位，不得被低 32 位截断。
#[ktest]
fn clear_sighand_flag_uses_linux_uapi_bit() {
    let flags = CloneFlags::from_raw(CloneFlags::CLONE_CLEAR_SIGHAND);
    assert_eq!(CloneFlags::CLONE_CLEAR_SIGHAND, 1u64 << 32);
    assert!(flags.has(CloneFlags::CLONE_CLEAR_SIGHAND));
}

/// CLONE_CLEAR_SIGHAND 可以单独使用，但不能与 CLONE_SIGHAND 同时指定。
#[ktest]
fn clear_sighand_conflicts_with_shared_sighand() {
    let mut args = CloneArgs::fork_default();
    args.flags =
        CloneFlags::from_raw(CloneFlags::CLONE_CLEAR_SIGHAND | CloneFlags::fork_default().raw());
    assert_eq!(crate::operation::validate_clone_args(args), Ok(()));

    args.flags = CloneFlags::from_raw(
        CloneFlags::CLONE_CLEAR_SIGHAND
            | CloneFlags::CLONE_SIGHAND
            | CloneFlags::CLONE_VM
            | CloneFlags::fork_default().raw(),
    );
    assert_eq!(
        crate::operation::validate_clone_args(args),
        Err(Errno::EINVAL)
    );
}

/// 当前 pidfd 只代表稳定的进程身份，不能把线程 TID 静默提升为进程 pidfd。
#[ktest]
fn process_pidfd_rejects_clone_thread() {
    let mut args = CloneArgs::fork_default();
    args.flags = CloneFlags::from_raw(
        CloneFlags::CLONE_PIDFD
            | CloneFlags::CLONE_THREAD
            | CloneFlags::CLONE_SIGHAND
            | CloneFlags::CLONE_VM,
    );
    args.pidfd = 0x1000;
    args.exit_signal = 0;

    assert_eq!(
        crate::operation::validate_clone_args(args),
        Err(Errno::EINVAL)
    );
}

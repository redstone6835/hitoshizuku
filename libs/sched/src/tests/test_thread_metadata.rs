//! 线程元数据测试。

extern crate alloc;
extern crate std;

use alloc::sync::{Arc, Weak};
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use errno::Errno;
use ktest::ktest;

use crate::runqueue::Runqueue;
use crate::{
    ArchContextOps, CpuMask, ExecPhase, ExecutionActionClaim, ExecutionScopeKind, NR_CPUS,
    NativeExternalControl, PidNamespace, ProcessGroup, ProcessPersonalityState, RobustListState,
    RseqEvent, RseqRegistration, SchedAttr, SchedClass, SchedParams, SchedPolicy, Session,
    SigAction, SigActionFlags, SigHandler, SigInfo, SigProcMaskHow, SigSet, SignalNumber,
    TASK_COMM_LEN, TASKEXT_VFS_FDTABLE, TASKEXT_VM_SPACE, Task, TaskState, TaskUsage, ThreadGroup,
    Uid, UserAbiKind, UserContextRef, WaitId, supported_cpu_mask,
};

const TEST_EXECUTION_ACTION: u64 = 1;

#[ktest]
fn vm_space_extension_can_be_borrowed_without_arc_clone() {
    let task = make_task();
    let payload = Arc::new(42usize);
    let erased: Arc<dyn core::any::Any + Send + Sync> = payload;

    task.ext_install(TASKEXT_VM_SPACE, erased);
    assert_eq!(
        task.ext_with(TASKEXT_VM_SPACE, |value| {
            value.downcast_ref::<usize>().copied()
        }),
        Some(Some(42))
    );
    assert!(task.ext_remove(TASKEXT_VM_SPACE).is_some());
    assert!(task.ext_with(TASKEXT_VM_SPACE, |_| ()).is_none());
}

#[ktest]
fn existing_extension_can_be_replaced_without_changing_its_slot() {
    const COLD_KEY: u64 = 0x7fff_0001;
    let task = make_task();
    task.ext_install(TASKEXT_VM_SPACE, Arc::new(10usize));
    task.ext_install(COLD_KEY, Arc::new(20usize));

    let old_hot = task
        .ext_replace(TASKEXT_VM_SPACE, Arc::new(11usize))
        .expect("hot 扩展槽应已存在");
    let old_cold = task
        .ext_replace(COLD_KEY, Arc::new(21usize))
        .expect("cold 扩展槽应已存在");

    assert_eq!(old_hot.downcast_ref::<usize>(), Some(&10));
    assert_eq!(old_cold.downcast_ref::<usize>(), Some(&20));
    assert_eq!(
        task.ext_with(TASKEXT_VM_SPACE, |value| value
            .downcast_ref::<usize>()
            .copied()),
        Some(Some(11))
    );
    assert_eq!(
        task.ext_with(COLD_KEY, |value| value.downcast_ref::<usize>().copied()),
        Some(Some(21))
    );
}

#[ktest]
fn replacing_missing_extension_returns_payload_without_installing_it() {
    const MISSING_KEY: u64 = 0x7fff_0002;
    let task = make_task();
    let replacement: Arc<dyn core::any::Any + Send + Sync> = Arc::new(30usize);

    let returned = match task.ext_replace(MISSING_KEY, replacement) {
        Ok(_) => panic!("缺失扩展槽不得在 replace 中隐式分配"),
        Err(payload) => payload,
    };

    assert_eq!(returned.downcast_ref::<usize>(), Some(&30));
    assert!(task.ext_lookup(MISSING_KEY).is_none());
}

#[ktest]
fn extension_identity_check_rejects_replaced_payload() {
    const KEY: u64 = 0x7fff_0003;
    let task = make_task();
    let observed: Arc<dyn core::any::Any + Send + Sync> = Arc::new(40usize);
    task.ext_install(KEY, Arc::clone(&observed));

    assert!(task.ext_is_current(KEY, &observed));
    task.ext_replace(KEY, Arc::new(41usize))
        .expect("待重验扩展槽应已存在");
    assert!(!task.ext_is_current(KEY, &observed));
}

#[ktest]
fn taking_robust_list_clears_registration_before_cleanup() {
    let task = make_task();
    task.set_robust_list(0x1234, 24);

    let taken = task.take_robust_list();

    assert_eq!(
        taken,
        RobustListState {
            head: 0x1234,
            len: 24
        }
    );
    assert_eq!(task.robust_list(), RobustListState::default());
}

#[ktest]
fn task_execution_scope_allows_each_action_once_and_resets_on_exit() {
    let task = make_task();

    assert_eq!(
        task.claim_execution_action(TEST_EXECUTION_ACTION),
        ExecutionActionClaim::OutsideScope
    );
    assert!(task.begin_execution_scope(ExecutionScopeKind::Syscall));
    assert_eq!(
        task.execution_scope_kind(),
        Some(ExecutionScopeKind::Syscall)
    );
    assert!(!task.begin_execution_scope(ExecutionScopeKind::NetworkWorker));
    assert!(!task.execution_action_claimed(TEST_EXECUTION_ACTION));
    assert_eq!(
        task.claim_execution_action(TEST_EXECUTION_ACTION),
        ExecutionActionClaim::Claimed(ExecutionScopeKind::Syscall)
    );
    assert!(task.execution_action_claimed(TEST_EXECUTION_ACTION));
    assert_eq!(
        task.claim_execution_action(TEST_EXECUTION_ACTION),
        ExecutionActionClaim::AlreadyClaimed(ExecutionScopeKind::Syscall)
    );
    assert_eq!(
        task.end_execution_scope(ExecutionScopeKind::Syscall),
        TEST_EXECUTION_ACTION
    );
    assert_eq!(task.execution_scope_kind(), None);
    assert!(!task.execution_action_claimed(TEST_EXECUTION_ACTION));

    assert!(task.begin_execution_scope(ExecutionScopeKind::NetworkWorker));
    assert_eq!(
        task.claim_execution_action(TEST_EXECUTION_ACTION),
        ExecutionActionClaim::Claimed(ExecutionScopeKind::NetworkWorker)
    );
    assert_eq!(
        task.end_execution_scope(ExecutionScopeKind::NetworkWorker),
        TEST_EXECUTION_ACTION
    );
}

unsafe fn init_test_context(
    _ctx: NonNull<u8>,
    _stack_top: usize,
    _entry: crate::KernelEntry,
    _arg: usize,
) {
}

unsafe extern "C" fn switch_test_context(
    _prev: NonNull<u8>,
    _next: NonNull<u8>,
    prev_on_cpu: NonNull<core::sync::atomic::AtomicUsize>,
) {
    unsafe {
        prev_on_cpu
            .as_ref()
            .store(0, core::sync::atomic::Ordering::Release)
    };
}

static TEST_ARCH_CONTEXT_OPS: ArchContextOps = ArchContextOps {
    context_size: 16,
    context_align: 16,
    init_kernel_context: init_test_context,
    switch_context: switch_test_context,
};

pub(super) fn make_task() -> alloc::sync::Arc<Task> {
    crate::arch_hooks::register(&TEST_ARCH_CONTEXT_OPS);
    let session = Session::new();
    let pg = ProcessGroup::new(&session);
    session.register_group(&pg);
    let tg = ThreadGroup::new();
    let task = Task::new(SchedParams::default_fair(), Weak::new(), tg, pg);
    task.adopt_current_context();
    task
}

fn make_task_in_group(group: Arc<ThreadGroup>) -> Arc<Task> {
    crate::arch_hooks::register(&TEST_ARCH_CONTEXT_OPS);
    let session = Session::new();
    let pg = ProcessGroup::new(&session);
    session.register_group(&pg);
    let task = Task::new(SchedParams::default_fair(), Weak::new(), group, pg);
    task.adopt_current_context();
    task
}

#[ktest]
fn new_thread_group_is_tomori_without_native_payload() {
    let group = ThreadGroup::new();

    assert_eq!(group.user_abi_kind(), UserAbiKind::TomoriLinux);
    assert_eq!(group.exec_phase(), ExecPhase::Running);
    assert_eq!(group.exec_generation(), 0);
    assert!(group.native_personality_payload().is_none());
}

#[ktest]
fn pidfd_wait_target_keeps_thread_group_identity_after_leader_replacement() {
    let group = ThreadGroup::new();
    let old_leader = make_task_in_group(Arc::clone(&group));
    let executor = make_task_in_group(Arc::clone(&group));
    group.set_leader(&old_leader);
    let target = WaitId::Pidfd(Arc::clone(&group));

    group.set_leader(&executor);

    assert_eq!(target, WaitId::Pidfd(executor.thread_group()));
}

#[ktest]
fn native_children_are_owned_by_the_thread_group_not_the_spawning_task() {
    let owner = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&owner));
    let worker = make_task_in_group(Arc::clone(&owner));
    owner.set_leader(&leader);
    owner.add_member(&leader);
    owner.add_member(&worker);

    let child = make_task();
    let child_group = child.thread_group();
    child_group.set_leader(&child);
    child_group.add_member(&child);
    child.set_state(TaskState::Zombie);
    assert!(child_group.mark_terminated_if_all_members_terminal());

    assert!(
        owner
            .try_add_native_child(Arc::clone(&child))
            .expect("Native child 登记不应因资源不足失败")
    );
    assert!(leader.snapshot_children().is_empty());
    assert!(worker.snapshot_children().is_empty());

    // leader 可以先退出；只要线程组尚未整体终止，Native child 仍属于 owner。
    leader.set_state(TaskState::Dead);
    assert!(!owner.is_terminated());
    assert!(
        owner
            .snapshot_native_children()
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, &child))
    );

    let reaped = owner
        .reap_native_child(|candidate| Arc::ptr_eq(candidate, &child))
        .expect("owner 线程组应能回收 Native child");
    assert!(Arc::ptr_eq(&reaped, &child));
}

#[ktest]
fn running_leader_inspection_serializes_with_identity_replacement() {
    let group = ThreadGroup::new();
    let old_leader = make_task_in_group(Arc::clone(&group));
    let executor = make_task_in_group(Arc::clone(&group));
    old_leader.ext_install(TASKEXT_VFS_FDTABLE, Arc::new(0x5a_u8));
    group.set_leader(&old_leader);

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let inspected_group = Arc::clone(&group);
    let inspector = std::thread::spawn(move || {
        inspected_group.with_running_leader(|leader| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            leader.ext_lookup(TASKEXT_VFS_FDTABLE)
        })
    });
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("资源读取应进入稳定 leader 临界区");

    let (replaced_tx, replaced_rx) = std::sync::mpsc::channel();
    let replacement_group = Arc::clone(&group);
    let replacement = Arc::clone(&executor);
    let replacer = std::thread::spawn(move || {
        replacement_group.set_leader(&replacement);
        replaced_tx.send(()).unwrap();
    });
    assert!(
        replaced_rx
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err(),
        "leader 身份替换不得穿过受保护的资源读取"
    );

    release_tx.send(()).unwrap();
    let payload = inspector
        .join()
        .unwrap()
        .expect("Running 线程组应存在 leader")
        .expect("旧 leader 的 fdtable 扩展应可读取");
    assert_eq!(payload.downcast_ref::<u8>(), Some(&0x5a));
    replaced_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("资源读取结束后身份替换应继续");
    replacer.join().unwrap();
    assert!(
        group
            .leader()
            .is_some_and(|leader| Arc::ptr_eq(&leader, &executor))
    );
}

#[ktest]
fn personality_publication_updates_existing_and_late_thread_caches() {
    let group = ThreadGroup::new();
    let first = make_task_in_group(Arc::clone(&group));
    let second = make_task_in_group(Arc::clone(&group));
    group.add_member(&first);
    group.add_member(&second);
    assert_eq!(first.user_abi_kind(), UserAbiKind::TomoriLinux);
    assert_eq!(second.user_abi_kind(), UserAbiKind::TomoriLinux);

    let payload: Arc<dyn core::any::Any + Send + Sync> = Arc::new(0x5a5a_u32);
    {
        let mut exec = group.lock_exec();
        exec.set_phase(ExecPhase::Transitioning);
        exec.install_personality(ProcessPersonalityState::MygoNative(Arc::clone(&payload)));
        assert_eq!(exec.advance_generation(), 1);
        exec.set_phase(ExecPhase::Running);
    }

    assert_eq!(first.user_abi_kind(), UserAbiKind::MygoNative);
    assert_eq!(second.user_abi_kind(), UserAbiKind::MygoNative);
    assert_eq!(group.exec_generation(), 1);
    let installed = group
        .native_personality_payload()
        .expect("Native personality 应持有进程级 payload");
    assert!(Arc::ptr_eq(&installed, &payload));

    let late = make_task_in_group(Arc::clone(&group));
    group.add_member(&late);
    assert_eq!(late.user_abi_kind(), UserAbiKind::MygoNative);
}

#[ktest]
fn native_external_control_consumes_ignored_signal_without_user_frame() {
    let task = make_native_control_task();
    task.shared_signal().set_action(
        SignalNumber::SIGUSR1,
        SigAction {
            handler: SigHandler::Ignore,
            mask: SigSet::EMPTY,
            flags: SigActionFlags(0),
            restorer: 0,
        },
    );
    task.signal.deliver(SigInfo {
        sig: SignalNumber::SIGUSR1,
        code: 0,
        sender_pid: 1,
        sender_uid: Uid::ROOT,
        raw: None,
    });

    assert_eq!(
        crate::operation::consume_native_external_control_for_task(&task),
        NativeExternalControl::Continue
    );
    assert!(!task.signal.has_any_pending());
    assert_eq!(task.state(), TaskState::Running);
}

#[ktest]
fn native_external_control_applies_default_terminate_at_safe_boundary() {
    let task = make_native_control_task();
    task.signal.deliver(SigInfo {
        sig: SignalNumber::SIGTERM,
        code: 0,
        sender_pid: 1,
        sender_uid: Uid::ROOT,
        raw: None,
    });

    assert_eq!(
        crate::operation::consume_native_external_control_for_task(&task),
        NativeExternalControl::Terminate
    );
    assert_eq!(task.state(), TaskState::Zombie);
}

#[ktest]
fn generic_signal_delivery_cannot_build_a_linux_frame_for_native() {
    let task = make_native_control_task();
    task.shared_signal().set_action(
        SignalNumber::SIGTERM,
        SigAction {
            handler: SigHandler::Handler(0x1234),
            mask: SigSet::EMPTY,
            flags: SigActionFlags(0),
            restorer: 0,
        },
    );
    task.signal.deliver(SigInfo {
        sig: SignalNumber::SIGTERM,
        code: 0,
        sender_pid: 1,
        sender_uid: Uid::ROOT,
        raw: None,
    });

    assert!(
        crate::operation::deliver_pending_signals_for_task(&task, UserContextRef::new(0x4000),)
            .is_none()
    );
    assert_eq!(task.state(), TaskState::Zombie);
}

#[ktest]
fn native_external_control_reports_stop_and_continue_as_reschedule() {
    let task = make_native_control_task();
    task.signal.deliver(SigInfo {
        sig: SignalNumber::SIGSTOP,
        code: 0,
        sender_pid: 1,
        sender_uid: Uid::ROOT,
        raw: None,
    });
    assert_eq!(
        crate::operation::consume_native_external_control_for_task(&task),
        NativeExternalControl::Reschedule
    );
    assert_eq!(task.state(), TaskState::Stopped);

    let continued = SigInfo {
        sig: SignalNumber::SIGCONT,
        code: 0,
        sender_pid: 1,
        sender_uid: Uid::ROOT,
        raw: None,
    };
    task.shared_signal().deliver(continued);
    crate::signal_wakeup(&task, &continued);
    assert_eq!(
        crate::operation::consume_native_external_control_for_task(&task),
        NativeExternalControl::Continue
    );
    assert_eq!(task.state(), TaskState::Runnable);
    assert_eq!(task.shared_signal().pending_len_hint(), 0);
}

#[ktest]
fn native_external_control_completes_published_group_exit() {
    let task = make_native_control_task();
    assert_eq!(task.thread_group().request_group_exit(73), 73);
    task.publish_group_exit_wakeup();

    assert_eq!(
        crate::operation::consume_native_external_control_for_task(&task),
        NativeExternalControl::Terminate
    );
    assert_eq!(task.state(), TaskState::Zombie);
}

fn make_native_control_task() -> Arc<Task> {
    let task = make_task();
    let group = task.thread_group();
    group.set_leader(&task);
    group.add_member(&task);
    task.set_state(TaskState::Running);
    let payload: Arc<dyn core::any::Any + Send + Sync> = Arc::new(());
    let mut exec = group.lock_exec();
    exec.set_phase(ExecPhase::Transitioning);
    exec.install_personality(ProcessPersonalityState::MygoNative(payload));
    exec.set_phase(ExecPhase::Running);
    drop(exec);
    task
}

#[ktest]
fn session_leader_follows_thread_group_identity_after_exec_adoption() {
    let session = Session::new();
    let process_group = ProcessGroup::new(&session);
    session.register_group(&process_group);
    let group = ThreadGroup::new();
    let old_leader = make_task_in_group(Arc::clone(&group));
    let executor = make_task_in_group(Arc::clone(&group));

    group.set_leader(&old_leader);
    session.set_leader(&old_leader);
    group.add_member(&old_leader);
    group.add_member(&executor);

    group.set_leader(&executor);

    assert!(
        session
            .leader()
            .is_some_and(|leader| Arc::ptr_eq(&leader, &executor))
    );
}

#[ktest]
fn child_registration_waits_for_process_identity_transaction() {
    let parent = make_task();
    let child = make_task();
    let identity = crate::pid::lock_process_identity();
    let (registered_tx, registered_rx) = std::sync::mpsc::channel();
    let parent_for_thread = Arc::clone(&parent);
    let child_for_thread = Arc::clone(&child);
    let registration = std::thread::spawn(move || {
        parent_for_thread.add_child(child_for_thread);
        registered_tx.send(()).unwrap();
    });

    assert!(
        registered_rx
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err(),
        "父侧 child 登记不得绕过进程身份事务"
    );
    drop(identity);
    registered_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("身份事务结束后 child 登记应继续");
    registration.join().unwrap();
    assert!(parent.has_child(&child));
}

#[ktest]
fn pid_registry_exposes_fallible_task_snapshot_for_pre_ponr_work() {
    let namespace = PidNamespace::new_root();
    let task = make_task();
    let pid = namespace.registry().allocate(&task).expect("测试任务 pid");
    task.register_pid(Arc::clone(&namespace), pid);

    let snapshot = namespace
        .registry()
        .try_snapshot_tasks()
        .expect("任务快照应传播可失败分配结果");

    assert!(
        snapshot
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, &task))
    );
}

#[ktest]
fn transitioning_keeps_signal_producers_live_and_pauses_consumers() {
    let group = ThreadGroup::new();
    let task = make_task_in_group(Arc::clone(&group));
    group.add_member(&task);
    task.signal.deliver(SigInfo {
        sig: SignalNumber::SIGUSR1,
        code: 0,
        sender_pid: 1,
        sender_uid: Uid(0),
        raw: None,
    });
    group.lock_exec().set_phase(ExecPhase::Transitioning);

    task.shared_signal().deliver(SigInfo {
        sig: SignalNumber::SIGUSR2,
        code: 0,
        sender_pid: 2,
        sender_uid: Uid(0),
        raw: None,
    });
    assert!(
        task.dequeue_pending_signal_in(SigSet::from_raw(
            SignalNumber::SIGUSR1.bit() | SignalNumber::SIGUSR2.bit()
        ))
        .is_none(),
        "Transitioning 期间 consumer 不得出队"
    );
    assert!(task.signal.has_pending_in(SignalNumber::SIGUSR1.bit()));
    assert!(
        task.shared_signal()
            .has_pending_in(SignalNumber::SIGUSR2.bit())
    );

    group.lock_exec().set_phase(ExecPhase::Running);
    let first = task
        .dequeue_pending_signal_in(SigSet::from_raw(
            SignalNumber::SIGUSR1.bit() | SignalNumber::SIGUSR2.bit(),
        ))
        .expect("恢复 Running 后应消费 per-task pending");
    let second = task
        .dequeue_pending_signal_in(SigSet::from_raw(
            SignalNumber::SIGUSR1.bit() | SignalNumber::SIGUSR2.bit(),
        ))
        .expect("恢复 Running 后应消费 shared pending");
    assert_eq!(first.sig, SignalNumber::SIGUSR1);
    assert_eq!(second.sig, SignalNumber::SIGUSR2);
    assert!(
        task.dequeue_pending_signal_in(SigSet::from_raw(
            SignalNumber::SIGUSR1.bit() | SignalNumber::SIGUSR2.bit()
        ))
        .is_none(),
        "两条 pending 各消费一次后队列应为空"
    );
}

#[ktest]
fn transitioning_publication_waits_for_active_signal_consumer() {
    let group = ThreadGroup::new();
    let consumer = group
        .lock_signal_consumer()
        .expect("Running 阶段应允许 consumer 进入");
    let (attempting_tx, attempting_rx) = std::sync::mpsc::channel();
    let (published_tx, published_rx) = std::sync::mpsc::channel();
    let publisher_group = Arc::clone(&group);
    let publisher = std::thread::spawn(move || {
        attempting_tx.send(()).expect("应通知发布线程已启动");
        publisher_group
            .lock_exec()
            .set_phase(ExecPhase::Transitioning);
        published_tx.send(()).expect("应通知 Transitioning 已发布");
    });

    attempting_rx.recv().expect("发布线程应启动");
    assert!(
        published_rx
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err(),
        "活动 consumer 退出前不得发布 Transitioning"
    );
    drop(consumer);
    published_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("consumer 退出后应完成发布");
    publisher.join().expect("发布线程不得 panic");
    assert_eq!(group.exec_phase(), ExecPhase::Transitioning);
}

#[ktest]
fn transitioning_waits_for_complete_signal_consumption() {
    let group = ThreadGroup::new();
    let task = make_task_in_group(Arc::clone(&group));
    group.add_member(&task);
    task.signal.deliver(SigInfo {
        sig: SignalNumber::SIGUSR1,
        code: 0,
        sender_pid: 1,
        sender_uid: Uid(0),
        raw: None,
    });
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let consumer_task = Arc::clone(&task);
    let consumer = std::thread::spawn(move || {
        consumer_task
            .consume_pending_signal(|info| {
                assert_eq!(info.sig, SignalNumber::SIGUSR1);
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .expect("Running 阶段应消费 pending signal");
    });
    entered_rx.recv().expect("consumer 应进入完整消费区间");

    let (published_tx, published_rx) = std::sync::mpsc::channel();
    let publisher_group = Arc::clone(&group);
    let publisher = std::thread::spawn(move || {
        publisher_group
            .lock_exec()
            .set_phase(ExecPhase::Transitioning);
        published_tx.send(()).unwrap();
    });
    assert!(
        published_rx
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err(),
        "signal action 完成前不得发布 Transitioning"
    );
    release_tx.send(()).unwrap();
    published_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("完整消费结束后应完成 Transitioning 发布");
    consumer.join().unwrap();
    publisher.join().unwrap();
}

#[ktest]
fn clone_guard_rejects_transitioning_and_serializes_publication() {
    let group = ThreadGroup::new();
    group.lock_exec().set_phase(ExecPhase::Transitioning);
    assert!(group.lock_for_clone().is_none());
    group.lock_exec().set_phase(ExecPhase::Running);

    let clone_guard = group.lock_for_clone().expect("Running 阶段应允许 clone");
    let (published_tx, published_rx) = std::sync::mpsc::channel();
    let publisher_group = Arc::clone(&group);
    let publisher = std::thread::spawn(move || {
        publisher_group
            .lock_exec()
            .set_phase(ExecPhase::Transitioning);
        published_tx.send(()).unwrap();
    });
    assert!(
        published_rx
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err(),
        "clone 登记完成前 exec 不得发布 Transitioning"
    );
    drop(clone_guard);
    published_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("clone guard 释放后 exec 应继续");
    publisher.join().unwrap();
}

#[ktest]
fn task_comm_is_nul_padded_and_truncated() {
    let task = make_task();
    assert_eq!(&task.comm()[..5], b"mygo\0");

    task.set_comm(b"abcdefghijklmnopqr");
    let comm = task.comm();
    assert_eq!(&comm[..TASK_COMM_LEN - 1], b"abcdefghijklmno");
    assert_eq!(comm[TASK_COMM_LEN - 1], 0);
}

#[ktest]
fn thread_group_accounting_waits_for_last_member_and_aggregates_usage() {
    let group = ThreadGroup::new();
    let first = make_task();
    let second = make_task();
    group.add_member(&first);
    group.add_member(&second);

    assert!(!group.account_member_exit(TaskUsage {
        user_ns: 10,
        minflt: 2,
        ..TaskUsage::default()
    }));
    assert!(group.account_member_exit(TaskUsage {
        system_ns: 20,
        majflt: 3,
        ..TaskUsage::default()
    }));
    assert_eq!(
        group.exited_usage_snapshot(),
        TaskUsage {
            user_ns: 10,
            system_ns: 20,
            minflt: 2,
            majflt: 3,
            ..TaskUsage::default()
        }
    );
    assert!(group.try_claim_acct_record());
    assert!(!group.try_claim_acct_record());
}

#[ktest]
fn shared_signal_marks_all_members_and_unblock_rearms_delivery() {
    let group = ThreadGroup::new();
    let first = make_task_in_group(Arc::clone(&group));
    let second = make_task_in_group(Arc::clone(&group));
    group.set_leader(&first);
    group.add_member(&first);
    group.add_member(&second);

    let blocked = SigSet::EMPTY.with(SignalNumber::SIGUSR1);
    first.signal.block(blocked, SigProcMaskHow::SetMask);
    second.signal.block(blocked, SigProcMaskHow::SetMask);
    assert!(!first.refresh_user_return_work_hint());
    assert!(!second.refresh_user_return_work_hint());

    crate::scheduler::deliver_shared_signal_to_group(
        &group,
        SigInfo {
            sig: SignalNumber::SIGUSR1,
            code: 0,
            sender_pid: 1,
            sender_uid: Uid::ROOT,
            raw: None,
        },
    );
    assert!(first.user_return_work_hint_relaxed());
    assert!(second.user_return_work_hint_relaxed());
    assert!(!first.refresh_user_return_work_hint());
    assert!(!second.refresh_user_return_work_hint());

    first.signal.block(SigSet::EMPTY, SigProcMaskHow::SetMask);
    assert!(first.user_return_work_hint_relaxed());
    assert!(first.refresh_user_return_work_hint());
}

#[ktest]
fn member_joining_after_shared_signal_inherits_return_work_hint() {
    let group = ThreadGroup::new();
    crate::scheduler::deliver_shared_signal_to_group(
        &group,
        SigInfo {
            sig: SignalNumber::SIGUSR1,
            code: 0,
            sender_pid: 1,
            sender_uid: Uid::ROOT,
            raw: None,
        },
    );

    let late = make_task_in_group(Arc::clone(&group));
    late.signal.block(
        SigSet::EMPTY.with(SignalNumber::SIGUSR1),
        SigProcMaskHow::SetMask,
    );
    assert!(!late.refresh_user_return_work_hint());
    assert!(!late.user_return_work_hint_relaxed());
    group.add_member(&late);
    assert!(!late.has_deliverable_signal());
    assert!(late.user_return_work_hint_relaxed());

    late.signal.block(SigSet::EMPTY, SigProcMaskHow::SetMask);
    assert!(late.has_deliverable_signal());
    assert!(late.refresh_user_return_work_hint());
}

#[ktest]
fn thread_group_leader_becomes_waitable_only_after_last_member() {
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let worker = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    assert!(group.try_add_member(&leader));
    assert!(group.try_add_member(&worker));

    leader.set_state(TaskState::Zombie);
    assert!(!group.mark_terminated_if_all_members_terminal());
    assert!(!leader.is_waitable_zombie());

    let pidfd_waiter = make_task();
    let wait_entry = leader
        .exit_waiters
        .prepare_to_wait(&pidfd_waiter, TaskState::Sleeping);
    worker.set_state(TaskState::Dead);
    assert!(group.mark_terminated_if_all_members_terminal());

    assert!(group.is_terminated());
    assert!(leader.is_waitable_zombie());
    assert_eq!(pidfd_waiter.state(), TaskState::Runnable);
    leader.exit_waiters.finish_wait(&wait_entry);

    let late = make_task_in_group(Arc::clone(&group));
    assert!(!group.try_add_member(&late));
}

#[ktest]
fn process_exit_waiters_are_woken_by_group_termination() {
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let worker = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&worker);

    leader.set_state(TaskState::Zombie);
    worker.set_state(TaskState::Dead);
    let waiter = make_task();
    let entry = group
        .process_exit_waiters()
        .prepare_to_wait(&waiter, TaskState::Sleeping);

    assert!(group.mark_terminated_if_all_members_terminal());
    assert_eq!(waiter.state(), TaskState::Runnable);
    group.process_exit_waiters().finish_wait(&entry);
}

#[ktest]
fn reap_delays_thread_group_leader_until_group_termination() {
    let parent = make_task();
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let worker = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&worker);
    parent.add_child(Arc::clone(&leader));

    leader.set_state(TaskState::Zombie);
    assert!(!group.mark_terminated_if_all_members_terminal());
    assert!(parent.reap_matching(|_| true).is_none());

    worker.set_state(TaskState::Dead);
    assert!(group.mark_terminated_if_all_members_terminal());
    let reaped = parent
        .reap_matching(|task| Arc::ptr_eq(task, &leader))
        .expect("leader must become reapable after the final member exits");
    assert!(Arc::ptr_eq(&reaped, &leader));
    assert_eq!(leader.state(), TaskState::Dead);
}

#[ktest]
fn leader_parent_notification_is_deferred_until_worker_exit() {
    let parent = make_task();
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let worker = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&worker);
    leader.reparent_to(&parent);
    worker.reparent_to(&parent);
    parent.add_child(Arc::clone(&leader));

    crate::spawn::exit_task(&leader, crate::ExitCode(19));
    assert_eq!(leader.state(), TaskState::Zombie);
    assert!(!leader.is_waitable_zombie());
    assert_eq!(parent.shared_signal().pending_len_hint(), 0);
    assert!(parent.reap_matching(|_| true).is_none());

    worker.set_exit_signal(0);
    crate::spawn::exit_task(&worker, crate::ExitCode(19));
    assert_eq!(worker.state(), TaskState::Dead);
    assert!(leader.is_waitable_zombie());
    assert_eq!(parent.shared_signal().pending_len_hint(), 1);
}

#[ktest]
fn late_exit_group_status_overrides_leader_raw_exit_for_parent_wait() {
    let parent = make_task();
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let worker = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&worker);
    parent.add_child(Arc::clone(&leader));

    leader.mark_exited(crate::ExitCode(7));
    assert_eq!(group.request_group_exit(42), 42);
    worker.set_state(TaskState::Dead);
    assert!(group.mark_terminated_if_all_members_terminal());

    let reaped = parent
        .reap_matching(|task| Arc::ptr_eq(task, &leader))
        .expect("parent must reap the terminated leader");
    assert_eq!(reaped.exit_code(), Some(crate::ExitCode(42)));
    let status = reaped.exit_wait_status().expect("leader wait status");
    assert!(status.wifexited());
    assert_eq!(status.wexitstatus(), 42);
}

#[ktest]
fn late_sigkill_status_overrides_leader_raw_exit_for_parent_wait() {
    let parent = make_task();
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let worker = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&worker);
    parent.add_child(Arc::clone(&leader));

    leader.mark_exited(crate::ExitCode(7));
    let _ = group.request_group_signal(SignalNumber::SIGKILL, false);
    worker.set_state(TaskState::Dead);
    assert!(group.mark_terminated_if_all_members_terminal());

    let reaped = parent
        .reap_matching(|task| Arc::ptr_eq(task, &leader))
        .expect("parent must reap the SIGKILL-terminated leader");
    assert_eq!(reaped.exit_code(), Some(crate::ExitCode(9)));
    let status = reaped.exit_wait_status().expect("leader wait status");
    assert!(status.wifsignaled());
    assert_eq!(status.wtermsig(), SignalNumber::SIGKILL.raw() as i32);
    assert!(!status.wcoredump());
}

#[ktest]
fn default_sigterm_terminates_the_complete_thread_group() {
    let parent = make_task();
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let worker = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&worker);
    parent.add_child(Arc::clone(&leader));
    leader.set_state(TaskState::Running);
    worker.set_state(TaskState::Running);

    crate::operation::apply_default_action_for_task(
        &leader,
        crate::SigInfo {
            sig: SignalNumber::SIGTERM,
            code: 0,
            sender_pid: 1,
            sender_uid: crate::Uid::ROOT,
            raw: None,
        },
    );

    assert_eq!(leader.state(), TaskState::Zombie);
    assert!(worker.group_exit_boundary_pending());
    assert!(parent.reap_matching(|_| true).is_none());

    assert!(crate::operation::complete_group_exit_if_requested(&worker));
    let reaped = parent
        .reap_matching(|task| Arc::ptr_eq(task, &leader))
        .expect("parent must reap the SIGTERM-terminated leader");
    let status = reaped.exit_wait_status().expect("leader wait status");
    assert!(status.wifsignaled());
    assert_eq!(status.wtermsig(), SignalNumber::SIGTERM.raw() as i32);
    assert!(!status.wcoredump());
}

#[ktest]
fn default_sigsegv_terminates_the_complete_thread_group_with_core_status() {
    let parent = make_task();
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let worker = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&worker);
    parent.add_child(Arc::clone(&leader));
    leader.set_state(TaskState::Running);
    worker.set_state(TaskState::Running);

    crate::operation::apply_default_action_for_task(
        &leader,
        crate::SigInfo {
            sig: SignalNumber::SIGSEGV,
            code: 0,
            sender_pid: 1,
            sender_uid: crate::Uid::ROOT,
            raw: None,
        },
    );

    assert_eq!(leader.state(), TaskState::Zombie);
    assert!(worker.group_exit_boundary_pending());
    assert!(parent.reap_matching(|_| true).is_none());

    assert!(crate::operation::complete_group_exit_if_requested(&worker));
    let reaped = parent
        .reap_matching(|task| Arc::ptr_eq(task, &leader))
        .expect("parent must reap the SIGSEGV-terminated leader");
    let status = reaped.exit_wait_status().expect("leader wait status");
    assert!(status.wifsignaled());
    assert_eq!(status.wtermsig(), SignalNumber::SIGSEGV.raw() as i32);
    assert!(status.wcoredump());
}

#[ktest]
fn group_exit_sleep_commit_is_cancelled_after_precheck_window() {
    let group = ThreadGroup::new();
    let task = make_task_in_group(Arc::clone(&group));
    group.set_leader(&task);
    group.add_member(&task);
    task.set_state(TaskState::Running);

    assert!(!task.group_exit_pending());
    assert_eq!(group.request_group_exit(33), 33);
    task.publish_group_exit_wakeup();
    assert!(!task.cas_state(TaskState::Running, TaskState::Sleeping));
    assert_eq!(task.state(), TaskState::Running);

    let queue = crate::WaitQueue::new();
    let entry = queue.prepare_to_wait(&task, TaskState::Sleeping);
    assert_eq!(task.state(), TaskState::Running);
    queue.finish_wait(&entry);
    assert_eq!(task.state(), TaskState::Running);

    // 反向时序：睡眠已提交后才发布请求，调度入口必须
    // 撤销它，覆盖未通过 WaitQueue 的直接睡眠路径。
    let late_group = ThreadGroup::new();
    let late_task = make_task_in_group(Arc::clone(&late_group));
    late_group.set_leader(&late_task);
    late_group.add_member(&late_task);
    late_task.set_state(TaskState::Running);
    assert!(late_task.cas_state(TaskState::Running, TaskState::Sleeping));
    assert_eq!(late_group.request_group_exit(34), 34);
    late_task.publish_group_exit_wakeup();
    assert!(late_task.abort_group_exit_sleep());
    assert_eq!(late_task.state(), TaskState::Running);
}

#[ktest]
fn fatal_group_resume_does_not_publish_continued_event() {
    let group = ThreadGroup::new();
    let task = make_task_in_group(Arc::clone(&group));
    group.set_leader(&task);
    group.add_member(&task);
    task.set_state(TaskState::Running);
    assert!(task.mark_stopped(SignalNumber::SIGSTOP));
    assert_eq!(task.state(), TaskState::Stopped);

    let _ = group.request_group_signal(SignalNumber::SIGKILL, false);
    task.publish_group_exit_wakeup();
    assert!(task.resume_for_fatal_exit());
    assert_eq!(task.state(), TaskState::Runnable);
    assert!(task.wait_continued_status(false).is_none());
    assert!(task.wait_stopped_status(false).is_none());
}

#[ktest]
fn exec_sibling_exit_is_distinct_from_group_exit_and_blocks_sleep() {
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let worker = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&worker);
    leader.set_state(TaskState::Running);
    worker.set_state(TaskState::Running);
    worker.set_exit_signal(0);

    crate::operation::request_exec_sibling_exit(&worker, false);

    assert!(worker.exec_sibling_exit_boundary_pending());
    assert!(group.group_exit_status().is_none());
    assert!(!worker.cas_state(TaskState::Running, TaskState::Sleeping));
    assert!(crate::operation::complete_exec_sibling_exit_if_requested(
        &worker
    ));
    assert_eq!(worker.state(), TaskState::Dead);
    assert!(group.has_only_member(&leader));
}

#[ktest]
fn exec_sibling_exit_interrupts_blocking_syscall() {
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let worker = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&worker);
    worker.set_state(TaskState::Running);

    assert!(!crate::operation::has_interrupting_signal(&worker));
    crate::operation::request_exec_sibling_exit(&worker, false);

    assert!(crate::operation::has_interrupting_signal(&worker));
}

#[ktest]
fn exec_sibling_exit_keeps_user_return_work_armed() {
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let worker = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&worker);

    crate::operation::request_exec_sibling_exit(&worker, false);

    assert!(worker.user_return_work_hint_acquire());
    assert!(worker.signal.take_user_return_work());
    assert!(worker.refresh_user_return_work_hint());
    assert!(worker.user_return_work_hint_acquire());
}

#[ktest]
fn exec_preserved_zombie_leader_waits_for_identity_adoption() {
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let executor = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&executor);
    leader.set_state(TaskState::Zombie);
    leader.cleanup_exit_extensions();

    crate::operation::request_exec_sibling_exit(&leader, true);

    assert_eq!(leader.state(), TaskState::Zombie);
    assert!(crate::operation::complete_exec_sibling_exit_if_requested(
        &leader
    ));
    assert_eq!(leader.state(), TaskState::Zombie);
    assert!(leader.exec_sibling_exit_boundary_pending());
    assert!(
        group
            .snapshot()
            .iter()
            .any(|member| Arc::ptr_eq(member, &leader))
    );
}

#[ktest]
fn process_identity_transaction_serializes_reparent_without_blocking_pid_lookup() {
    let parent = make_task();
    let new_parent = make_task();
    let child = make_task();
    parent.add_child(Arc::clone(&child));
    child.reparent_to(&parent);

    let namespace = PidNamespace::new_root();
    let pid = namespace.registry().allocate(&child).expect("child pid");
    child.register_pid(Arc::clone(&namespace), pid);

    let identity = crate::pid::lock_process_identity();
    let (lookup_tx, lookup_rx) = std::sync::mpsc::channel();
    let lookup_namespace = Arc::clone(&namespace);
    let lookup = std::thread::spawn(move || {
        let owner = lookup_namespace
            .registry()
            .lookup(pid)
            .and_then(|owner| owner.upgrade());
        lookup_tx.send(owner).unwrap();
    });
    let (reparent_tx, reparent_rx) = std::sync::mpsc::channel();
    let exiting_parent = Arc::clone(&parent);
    let reaper = Arc::clone(&new_parent);
    let reparent = std::thread::spawn(move || {
        exiting_parent.reparent_children_to(&reaper);
        reparent_tx.send(()).unwrap();
    });

    let owner = lookup_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("PID 查询应由注册表自身锁完成");
    assert!(owner.is_some_and(|owner| Arc::ptr_eq(&owner, &child)));
    assert!(
        reparent_rx
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err(),
        "身份事务提交前父退出不得改写亲缘关系"
    );

    drop(identity);
    reparent_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("事务结束后 reparent 应继续");
    lookup.join().unwrap();
    reparent.join().unwrap();
    assert!(
        child
            .parent()
            .is_some_and(|task| Arc::ptr_eq(&task, &new_parent))
    );
}

#[ktest]
fn readonly_task_pid_query_does_not_wait_for_identity_transaction() {
    let namespace = PidNamespace::new_root();
    let task = make_task();
    let pid = namespace
        .registry()
        .allocate(&task)
        .expect("应分配测试 PID");
    task.register_pid(Arc::clone(&namespace), pid);
    let identity = crate::pid::lock_process_identity();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let queried = Arc::clone(&task);
    let query = std::thread::spawn(move || {
        result_tx.send(queried.pid_root()).unwrap();
    });

    let result = result_rx.recv_timeout(std::time::Duration::from_millis(100));
    drop(identity);
    query.join().unwrap();

    assert_eq!(result, Ok(Some(pid)), "只读 PID 查询不应进入全局身份事务");
}

#[ktest]
fn readonly_pid_registry_queries_do_not_wait_for_identity_transaction() {
    let namespace = PidNamespace::new_root();
    let task = make_task();
    let pid = namespace
        .registry()
        .allocate(&task)
        .expect("应分配测试 PID");
    task.register_pid(Arc::clone(&namespace), pid);
    let identity = crate::pid::lock_process_identity();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let queried_namespace = Arc::clone(&namespace);
    let query = std::thread::spawn(move || {
        let owner = queried_namespace
            .registry()
            .lookup(pid)
            .and_then(|owner| owner.upgrade());
        let snapshot_contains_owner = queried_namespace
            .registry()
            .try_snapshot_tasks()
            .unwrap()
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, &task));
        result_tx
            .send((
                owner.is_some_and(|owner| Arc::ptr_eq(&owner, &task)),
                snapshot_contains_owner,
            ))
            .unwrap();
    });

    let result = result_rx.recv_timeout(std::time::Duration::from_millis(100));
    drop(identity);
    query.join().unwrap();

    assert_eq!(
        result,
        Ok((true, true)),
        "PID 注册表只读查询不应进入全局身份事务"
    );
}

#[ktest]
fn unrelated_pid_registry_mutation_does_not_wait_for_identity_transaction() {
    let namespace = PidNamespace::new_root();
    let task = make_task();
    let identity = crate::pid::lock_process_identity();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let mutated_namespace = Arc::clone(&namespace);
    let mutation = std::thread::spawn(move || {
        let pid = mutated_namespace
            .registry()
            .allocate(&task)
            .expect("应分配测试 PID");
        mutated_namespace.registry().release(pid);
        result_tx.send(pid).unwrap();
    });

    let result = result_rx.recv_timeout(std::time::Duration::from_millis(100));
    drop(identity);
    mutation.join().unwrap();

    assert!(result.is_ok(), "无关 PID 分配与释放不应进入全局身份事务");
}

#[ktest]
fn exec_leader_adoption_joins_process_identity_transaction() {
    let task = make_task();
    let identity = crate::pid::lock_process_identity();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let worker = Arc::clone(&task);
    let adoption = std::thread::spawn(move || {
        result_tx.send(crate::operation::adopt_exec_leader_identity(
            &worker, &worker,
        ))
    });

    assert!(
        result_rx
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err(),
        "exec 身份接管必须进入统一身份事务"
    );
    drop(identity);
    assert_eq!(
        result_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("身份事务结束后接管检查应继续"),
        Err(Errno::EINVAL)
    );
    adoption.join().unwrap().unwrap();
}

#[ktest]
fn failed_exec_leader_adoption_keeps_the_old_identity_waitable() {
    let parent = make_task();
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let executor = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&executor);
    parent.add_child(Arc::clone(&leader));
    leader.reparent_to(&parent);
    leader.set_state(TaskState::Zombie);
    leader.cleanup_exit_extensions();

    crate::operation::prepare_exec_leader_identity(&executor, &leader)
        .expect("身份迁移应在退出旧 leader 前完成资源预留");
    crate::operation::request_exec_sibling_exit(&leader, true);
    assert!(crate::operation::complete_exec_sibling_exit_if_requested(
        &leader
    ));
    assert!(crate::operation::adopt_exec_leader_identity(&executor, &leader).is_err());

    assert_eq!(leader.state(), TaskState::Zombie);
    assert!(
        group
            .snapshot()
            .iter()
            .any(|member| Arc::ptr_eq(member, &leader))
    );
    assert!(
        group
            .leader()
            .is_some_and(|current| Arc::ptr_eq(&current, &leader))
    );
    assert!(parent.has_child(&leader));

    let _ = group.request_group_exit(127);
    crate::spawn::exit_task(&executor, crate::ExitCode(127));
    assert!(leader.is_waitable_zombie());
    let reaped = parent
        .reap_matching(|child| Arc::ptr_eq(child, &leader))
        .expect("失败的身份接管必须留下可回收的进程身份");
    assert!(Arc::ptr_eq(&reaped, &leader));
}

#[ktest]
fn zombie_leader_is_not_finished_while_it_still_owns_a_cpu_context() {
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let executor = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&executor);
    leader.set_state(TaskState::Zombie);
    assert!(leader.try_claim_cpu(0));

    crate::operation::request_exec_sibling_exit(&leader, true);
    assert!(crate::operation::complete_exec_sibling_exit_if_requested(
        &leader
    ));
    assert_eq!(leader.state(), TaskState::Zombie);

    // Safety: 测试持有 leader 的强引用，并只模拟上下文切换汇编完成 release store。
    unsafe {
        leader.on_cpu_slot().as_ref().store(0, Ordering::Release);
    }
    leader.cleanup_exit_extensions();
    assert!(crate::operation::complete_exec_sibling_exit_if_requested(
        &leader
    ));
    assert_eq!(leader.state(), TaskState::Zombie);
}

#[ktest]
fn nonleader_exec_adopts_stopped_leader_identity() {
    crate::arch_hooks::register(&TEST_ARCH_CONTEXT_OPS);
    let parent = make_task();
    let session = Session::new();
    let process_group = ProcessGroup::new(&session);
    session.register_group(&process_group);
    let group = ThreadGroup::new();
    let leader = Task::new(
        SchedParams::default_fair(),
        Arc::downgrade(&parent),
        Arc::clone(&group),
        Arc::clone(&process_group),
    );
    let executor = Task::new(
        SchedParams::default_fair(),
        Arc::downgrade(&leader),
        Arc::clone(&group),
        Arc::clone(&process_group),
    );
    leader.adopt_current_context();
    executor.adopt_current_context();
    group.add_member(&leader);
    group.add_member(&executor);
    process_group.add_member(&leader);
    process_group.add_member(&executor);
    parent.add_child(Arc::clone(&leader));

    let namespace = PidNamespace::new_root();
    let leader_pid = namespace.registry().allocate(&leader).expect("leader pid");
    let executor_pid = namespace
        .registry()
        .allocate(&executor)
        .expect("executor tid");
    leader.register_pid(Arc::clone(&namespace), leader_pid);
    executor.register_pid(Arc::clone(&namespace), executor_pid);
    leader.set_tgid_cache(leader_pid);
    executor.set_tgid_cache(leader_pid);
    group.set_leader(&leader);
    leader.set_state(TaskState::Running);
    executor.set_state(TaskState::Running);
    executor.set_exit_signal(0);

    let child_group = ThreadGroup::new();
    let child = Task::new(
        SchedParams::default_fair(),
        Arc::downgrade(&leader),
        child_group,
        Arc::clone(&process_group),
    );
    leader.add_child(Arc::clone(&child));

    crate::operation::prepare_exec_leader_identity(&executor, &leader)
        .expect("身份迁移应在退出旧 leader 前完成资源预留");
    crate::operation::request_exec_sibling_exit(&leader, true);
    assert!(crate::operation::complete_exec_sibling_exit_if_requested(
        &leader
    ));
    assert_eq!(leader.state(), TaskState::Zombie);
    assert_eq!(leader.pid_root(), Some(leader_pid));
    assert!(Arc::ptr_eq(&parent.snapshot_children()[0], &leader));

    crate::operation::adopt_exec_leader_identity(&executor, &leader)
        .expect("执行线程应接管 leader 身份");

    assert_eq!(executor.pid_root(), Some(leader_pid));
    assert_eq!(leader.pid_root(), None);
    assert!(
        namespace
            .registry()
            .lookup(leader_pid)
            .and_then(|task| task.upgrade())
            .is_some_and(|task| Arc::ptr_eq(&task, &executor))
    );
    assert!(namespace.registry().lookup(executor_pid).is_none());
    assert!(
        group
            .leader()
            .is_some_and(|task| Arc::ptr_eq(&task, &executor))
    );
    assert!(
        executor
            .parent()
            .is_some_and(|task| Arc::ptr_eq(&task, &parent))
    );
    assert!(Arc::ptr_eq(&parent.snapshot_children()[0], &executor));
    assert!(
        child
            .parent()
            .is_some_and(|task| Arc::ptr_eq(&task, &executor))
    );
    assert!(
        executor
            .snapshot_children()
            .iter()
            .any(|task| Arc::ptr_eq(task, &child))
    );
    assert_eq!(executor.exit_signal(), SignalNumber::SIGCHLD.raw() as i32);
    assert_eq!(leader.exit_signal(), 0);
}

#[ktest]
fn robust_list_and_rseq_state_roundtrip() {
    let task = make_task();
    task.set_robust_list(0x1000, 24);
    assert_eq!(
        task.robust_list(),
        RobustListState {
            head: 0x1000,
            len: 24,
        }
    );

    let rseq = RseqRegistration {
        ptr: 0x2000,
        len: 32,
        signature: 0x5305_5305,
        registered: true,
    };
    assert_eq!(task.rseq_registration_if_registered(), None);
    task.set_rseq_registration(rseq);
    assert!(task.rseq_registered());
    assert_eq!(task.rseq_registration(), rseq);
    assert_eq!(task.rseq_registration_if_registered(), Some(rseq));
    assert_eq!(task.pending_rseq_work(), None);
    task.mark_rseq_event(RseqEvent::Preempt);
    let (pending_registration, pending_events) = task
        .pending_rseq_work()
        .expect("已注册且存在事件时必须返回 rseq 工作");
    assert_eq!(pending_registration, rseq);
    assert!(pending_events.contains(RseqEvent::Preempt));
    assert!(task.rseq_events().contains(RseqEvent::Preempt));
    task.publish_rseq_cpu(0);
    task.publish_rseq_cpu(1);
    assert!(task.rseq_events().contains(RseqEvent::Migrate));
    task.clear_rseq_registration();
    assert!(!task.rseq_registered());
    assert_eq!(task.rseq_registration(), RseqRegistration::default());
    assert_eq!(task.rseq_registration_if_registered(), None);
    assert!(task.rseq_events().is_empty());
}

#[ktest]
fn pi_donation_tracks_nested_sources_and_restores_base_attr() {
    let task = make_task();
    task.set_sched_attr(SchedAttr::fair(8, 0));

    let fair = task.pi_add_donation(1, SchedAttr::fair(-5, 0));
    assert_eq!(fair.policy, SchedPolicy::Fair);
    assert_eq!(fair.nice, -5);

    let rt = task.pi_add_donation(2, SchedAttr::rt_fifo(40));
    assert_eq!(rt.policy, SchedPolicy::RtFifo);
    assert_eq!(rt.priority, 40);

    let still_rt = task.pi_remove_donation(1);
    assert_eq!(still_rt.policy, SchedPolicy::RtFifo);
    assert_eq!(still_rt.priority, 40);

    let restored = task.pi_remove_donation(2);
    assert_eq!(restored.policy, SchedPolicy::Fair);
    assert_eq!(restored.nice, 8);
}

#[ktest]
fn pi_donation_preserves_base_update_until_last_waiter_leaves() {
    let task = make_task();
    task.set_sched_attr(SchedAttr::fair(10, 0));
    let boosted = task.pi_add_donation(9, SchedAttr::rt_round_robin(30, 1_000_000));
    task.sched.set_sched_attr(boosted);

    task.set_sched_attr(SchedAttr::fair(3, 0));
    assert_eq!(task.sched.policy(), SchedPolicy::RtFifo);
    assert_eq!(task.sched.rt_priority(), 30);
    assert_eq!(task.pi_base_attr().nice, 3);

    let restored = task.pi_remove_donation(9);
    assert_eq!(restored.policy, SchedPolicy::Fair);
    assert_eq!(restored.nice, 3);
}

#[ktest]
fn deferred_pi_update_reads_latest_effective_attr() {
    let task = make_task();
    task.set_sched_attr(SchedAttr::fair(7, 0));
    let _ = task.pi_add_donation(11, SchedAttr::rt_fifo(50));
    task.request_deferred_pi_update();

    let _ = task.pi_remove_donation(11);

    let effective = task
        .take_deferred_pi_effective_attr()
        .expect("延迟 PI 更新应保留待处理标记");
    assert_eq!(effective.policy, SchedPolicy::Fair);
    assert_eq!(effective.nice, 7);
    assert!(task.take_deferred_pi_effective_attr().is_none());
}

#[ktest]
fn timer_slack_defaults_resets_and_inherits() {
    let parent = make_task();
    let child = make_task();

    assert_eq!(parent.timer_slack_ns(), crate::DEFAULT_TIMER_SLACK_NS);
    parent.set_timer_slack_ns(125_000);
    child.inherit_timer_slack_from(&parent);
    assert_eq!(child.timer_slack_ns(), 125_000);

    child.set_timer_slack_ns(0);
    assert_eq!(child.timer_slack_ns(), crate::DEFAULT_TIMER_SLACK_NS);
}

#[ktest]
fn supported_cpu_mask_matches_configured_capacity() {
    let expected = if NR_CPUS >= 64 {
        u64::MAX
    } else {
        (1u64 << NR_CPUS) - 1
    };
    assert_eq!(supported_cpu_mask(), expected);

    let task = make_task();
    task.set_cpu_affinity(0);
    assert_eq!(task.cpu_affinity(), 1);
    task.set_cpu_affinity(u64::MAX);
    assert_eq!(task.cpu_affinity(), supported_cpu_mask());
}

#[ktest]
fn runqueue_pick_respects_cpu_affinity_mask() {
    let task0 = make_task();
    let task1 = make_task();
    task0.set_cpu_affinity(CpuMask::single_raw(0).bits());
    task1.set_cpu_affinity(CpuMask::single_raw(1).bits());

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&task0), 1);
    rq.enqueue(alloc::sync::Arc::clone(&task1), 1);

    let picked = rq
        .pick_next_on(2, CpuMask::single_raw(1).bits())
        .expect("cpu1 should find an allowed task");
    assert!(alloc::sync::Arc::ptr_eq(&picked, &task1));

    assert!(rq.dequeue(&task0, 3));
    assert!(rq.dequeue(&task1, 3));
}

#[ktest]
fn runqueue_exact_pick_selects_requested_fair_task() {
    let first = make_task();
    let target = make_task();
    first.sched.store_vruntime(10);
    target.sched.store_vruntime(1_000);

    let rq = Runqueue::new();
    assert!(rq.enqueue(alloc::sync::Arc::clone(&first), 1));
    assert!(rq.enqueue(alloc::sync::Arc::clone(&target), 1));

    let picked = rq
        .pick_target_on(&target, 2, CpuMask::single_raw(0).bits())
        .expect("精确目标仍满足普通公平类约束");
    assert!(alloc::sync::Arc::ptr_eq(&picked, &target));
    assert!(rq.dequeue(&target, 3));
    assert!(rq.dequeue(&first, 3));
}

#[ktest]
fn runqueue_exact_pick_yields_to_higher_class() {
    let target = make_task();
    let realtime = make_task();
    realtime.sched.set_sched_attr(SchedAttr::rt_fifo(20));

    let rq = Runqueue::new();
    assert!(rq.enqueue(alloc::sync::Arc::clone(&target), 1));
    assert!(rq.enqueue(alloc::sync::Arc::clone(&realtime), 1));

    assert!(
        rq.pick_target_on(&target, 2, CpuMask::single_raw(0).bits())
            .is_none()
    );
    let picked = rq.pick_next(3).expect("实时任务应保持 class precedence");
    assert!(alloc::sync::Arc::ptr_eq(&picked, &realtime));
    assert!(rq.dequeue(&realtime, 4));
    assert!(rq.dequeue(&target, 4));
}

#[ktest]
fn runqueue_exact_pick_rejects_owned_non_current_task() {
    let current = make_task();
    let target = make_task();
    let rq = Runqueue::new();
    rq.set_current(alloc::sync::Arc::clone(&current));
    assert!(rq.enqueue(alloc::sync::Arc::clone(&target), 1));
    assert!(target.try_claim_cpu(0));

    assert!(
        rq.pick_target_on(&target, 2, CpuMask::single_raw(0).bits())
            .is_none()
    );
    assert!(rq.is_current(&current));

    unsafe {
        target
            .on_cpu_slot()
            .as_ref()
            .store(0, core::sync::atomic::Ordering::Release);
    }
    let picked = rq
        .pick_target_on(&target, 3, CpuMask::single_raw(0).bits())
        .expect("上下文释放后精确目标应可被选中");
    assert!(alloc::sync::Arc::ptr_eq(&picked, &target));
}

#[ktest]
fn runqueue_reports_task_waiting_for_context_release() {
    let task = make_task();
    task.set_cpu_affinity(CpuMask::single_raw(1).bits());
    assert!(task.try_claim_cpu(0));

    let rq = Runqueue::new();
    assert!(rq.enqueue(alloc::sync::Arc::clone(&task), 1));
    assert!(rq.has_ownership_blocked(CpuMask::single_raw(1).bits()));
    assert!(!rq.has_ownership_blocked(CpuMask::single_raw(0).bits()));

    unsafe {
        task.on_cpu_slot()
            .as_ref()
            .store(0, core::sync::atomic::Ordering::Release);
    }
    assert!(!rq.has_ownership_blocked(CpuMask::single_raw(1).bits()));
    assert!(rq.dequeue(&task, 2));
}

#[ktest]
fn runqueue_ignores_local_context_release_window() {
    let task = make_task();
    task.set_cpu_affinity(CpuMask::single_raw(0).bits());
    assert!(task.try_claim_cpu(0));

    let rq = Runqueue::new();
    assert!(rq.enqueue(alloc::sync::Arc::clone(&task), 1));
    assert!(!rq.has_ownership_blocked(CpuMask::single_raw(0).bits()));

    unsafe {
        task.on_cpu_slot()
            .as_ref()
            .store(0, core::sync::atomic::Ordering::Release);
    }
    assert!(rq.dequeue(&task, 2));
}

#[ktest]
fn runqueue_does_not_pick_task_before_context_release() {
    let task = make_task();
    task.set_cpu_affinity(CpuMask::single_raw(0).bits());
    assert!(task.try_claim_cpu(0));

    let rq = Runqueue::new();
    assert!(rq.enqueue(alloc::sync::Arc::clone(&task), 1));
    assert!(rq.pick_next_on(2, CpuMask::single_raw(0).bits()).is_none());

    unsafe {
        task.on_cpu_slot()
            .as_ref()
            .store(0, core::sync::atomic::Ordering::Release);
    }
    let picked = rq
        .pick_next_on(3, CpuMask::single_raw(0).bits())
        .expect("任务释放上下文后应可被选中");
    assert!(alloc::sync::Arc::ptr_eq(&picked, &task));
}

#[ktest]
fn runqueue_migratable_load_excludes_idle_class() {
    let fair = make_task();
    let idle = make_task();
    idle.sched.set_sched_attr(SchedAttr::idle());

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&fair), 1);
    rq.enqueue(alloc::sync::Arc::clone(&idle), 1);

    assert_eq!(rq.nr_running(), 2);
    assert_eq!(rq.migratable_load(), 1);

    assert!(rq.dequeue(&fair, 2));
    assert!(rq.dequeue(&idle, 2));
}

#[ktest]
fn runqueue_migratable_load_filters_cpu_affinity() {
    let cpu0 = make_task();
    let cpu1 = make_task();
    cpu0.set_cpu_affinity(CpuMask::single_raw(0).bits());
    cpu1.set_cpu_affinity(CpuMask::single_raw(1).bits());

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&cpu0), 1);
    rq.enqueue(alloc::sync::Arc::clone(&cpu1), 1);

    assert_eq!(rq.migratable_load(), 2);
    assert_eq!(rq.migratable_load_for(CpuMask::single_raw(0).bits()), 1);
    assert_eq!(rq.migratable_load_for(CpuMask::single_raw(1).bits()), 1);
    assert_eq!(rq.migratable_load_for(CpuMask::single_raw(2).bits()), 0);

    assert!(rq.dequeue(&cpu0, 2));
    assert!(rq.dequeue(&cpu1, 2));
}

#[ktest]
fn runqueue_reports_migratable_load_by_sched_class() {
    let fair = make_task();
    let realtime = make_task();
    realtime.sched.set_sched_attr(SchedAttr::rt_fifo(20));
    let deadline = make_task();
    deadline
        .sched
        .set_sched_attr(SchedAttr::deadline(1_000_000, 4_000_000, 4_000_000));

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&fair), 1);
    rq.enqueue(alloc::sync::Arc::clone(&realtime), 1);
    rq.enqueue(alloc::sync::Arc::clone(&deadline), 1);

    let load = rq.migratable_class_load();
    assert_eq!(load.fair, 1);
    assert_eq!(load.realtime, 1);
    assert_eq!(load.deadline, 1);
    assert_eq!(load.deadline_utilization, 256);
    assert_eq!(load.fair_weight, 1024);
    assert_eq!(load.total(), 3);

    assert!(rq.dequeue(&fair, 2));
    assert!(rq.dequeue(&realtime, 2));
    assert!(rq.dequeue(&deadline, 2));
}

#[ktest]
fn runqueue_takes_migratable_task_from_requested_class() {
    let fair = make_task();
    let realtime = make_task();
    realtime.sched.set_sched_attr(SchedAttr::rt_fifo(20));

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&fair), 1);
    rq.enqueue(alloc::sync::Arc::clone(&realtime), 1);

    let pulled = rq
        .take_migratable_from_class(SchedClass::Realtime, CpuMask::single_raw(0).bits(), 2)
        .expect("realtime task should be migratable");
    assert!(alloc::sync::Arc::ptr_eq(&pulled, &realtime));
    assert_eq!(rq.migratable_class_load().fair, 1);
    assert_eq!(rq.migratable_class_load().realtime, 0);

    assert!(rq.dequeue(&fair, 3));
}

#[ktest]
fn runqueue_class_load_includes_current_task() {
    let fair = make_task();
    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&fair), 1);
    let current = rq.pick_next(2).expect("current task");

    assert_eq!(rq.migratable_class_load().fair, 0);
    assert_eq!(rq.class_load().fair, 1);

    assert!(rq.dequeue(&current, 3));
}

/// CPU 使用时间只能累计任务实际作为 current 运行的区间。
#[ktest]
fn runqueue_accounts_current_cpu_runtime() {
    let task = make_task();
    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&task), 100);
    let current = rq.pick_next(100).expect("current task");

    rq.tick(150);
    assert_eq!(current.usage_snapshot(150).user_ns, 50);

    assert!(rq.dequeue(&current, 200));
    assert_eq!(current.usage_snapshot(300).user_ns, 100);
    rq.tick(400);
    assert_eq!(current.usage_snapshot(400).user_ns, 100);
}

#[ktest]
fn runqueue_rt_bandwidth_throttles_fifo_and_replenishes_next_period() {
    let realtime = make_task();
    realtime.sched.set_sched_attr(SchedAttr::rt_fifo(40));
    let fair = make_task();
    let rq = Runqueue::new_with_rt_bandwidth(100, 80);

    assert!(rq.enqueue(alloc::sync::Arc::clone(&realtime), 0));
    assert!(rq.enqueue(alloc::sync::Arc::clone(&fair), 0));
    let first = rq.pick_next(1).expect("RT task should run first");
    assert!(alloc::sync::Arc::ptr_eq(&first, &realtime));

    assert!(!rq.tick(80));
    assert!(rq.tick(81), "RT budget exhaustion must request reschedule");
    let throttled_pick = rq
        .pick_next(81)
        .expect("fair fallback while RT is throttled");
    assert!(alloc::sync::Arc::ptr_eq(&throttled_pick, &fair));

    assert!(rq.tick(100), "new RT period must request reschedule");
    let replenished = rq.pick_next(100).expect("RT task after replenishment");
    assert!(alloc::sync::Arc::ptr_eq(&replenished, &realtime));

    assert!(rq.dequeue(&realtime, 101));
    assert!(rq.dequeue(&fair, 101));
}

#[ktest]
fn runqueue_rt_bandwidth_charges_round_robin_runtime() {
    let realtime = make_task();
    realtime
        .sched
        .set_sched_attr(SchedAttr::rt_round_robin(30, 1_000));
    let fair = make_task();
    let rq = Runqueue::new_with_rt_bandwidth(100, 40);

    assert!(rq.enqueue(alloc::sync::Arc::clone(&realtime), 0));
    assert!(rq.enqueue(alloc::sync::Arc::clone(&fair), 0));
    assert!(alloc::sync::Arc::ptr_eq(
        &rq.pick_next(1).expect("RR task"),
        &realtime
    ));
    assert!(rq.tick(41));
    assert!(alloc::sync::Arc::ptr_eq(
        &rq.pick_next(41).expect("fair fallback"),
        &fair
    ));

    assert!(rq.dequeue(&realtime, 42));
    assert!(rq.dequeue(&fair, 42));
}

#[ktest]
fn runqueue_zero_rt_runtime_stays_throttled_across_periods() {
    let realtime = make_task();
    realtime.sched.set_sched_attr(SchedAttr::rt_fifo(40));
    let fair = make_task();
    let rq = Runqueue::new_with_rt_bandwidth(100, 80);

    assert!(rq.enqueue(alloc::sync::Arc::clone(&realtime), 0));
    assert!(rq.enqueue(alloc::sync::Arc::clone(&fair), 0));
    rq.set_rt_bandwidth(100, 0, 0);

    let first = rq.pick_next(1).expect("fair task with zero RT runtime");
    assert!(alloc::sync::Arc::ptr_eq(&first, &fair));
    assert!(!rq.tick(100));
    let next_period = rq.pick_next(100).expect("fair task in next period");
    assert!(alloc::sync::Arc::ptr_eq(&next_period, &fair));

    assert!(rq.dequeue(&realtime, 101));
    assert!(rq.dequeue(&fair, 101));
}

#[ktest]
fn runqueue_pi_boosted_owner_bypasses_rt_throttle() {
    let realtime = make_task();
    realtime.sched.set_sched_attr(SchedAttr::rt_fifo(40));
    let fair = make_task();
    let owner = make_task();
    let effective = owner.pi_add_donation(7, SchedAttr::rt_fifo(60));
    owner.sched.set_sched_attr(effective);
    let rq = Runqueue::new_with_rt_bandwidth(100, 20);

    assert!(rq.enqueue(alloc::sync::Arc::clone(&realtime), 0));
    assert!(rq.enqueue(alloc::sync::Arc::clone(&fair), 0));
    assert!(alloc::sync::Arc::ptr_eq(
        &rq.pick_next(1).expect("RT task"),
        &realtime
    ));
    assert!(rq.tick(21));
    assert!(alloc::sync::Arc::ptr_eq(
        &rq.pick_next(21).expect("fair fallback"),
        &fair
    ));

    assert!(rq.enqueue(alloc::sync::Arc::clone(&owner), 22));
    let boosted = rq.pick_next(22).expect("PI owner must bypass throttle");
    assert!(alloc::sync::Arc::ptr_eq(&boosted, &owner));

    let restored = owner.pi_remove_donation(7);
    assert!(rq.update_sched_attr_raw(&owner, restored, 23));
    let after_unlock = rq.pick_next(24).expect("fair task after PI unlock");
    assert_eq!(after_unlock.sched.policy(), SchedPolicy::Fair);
    assert!(!alloc::sync::Arc::ptr_eq(&after_unlock, &realtime));

    assert!(rq.dequeue(&realtime, 25));
    assert!(rq.dequeue(&fair, 25));
    assert!(rq.dequeue(&owner, 25));
}

#[ktest]
fn runqueue_drain_queued_keeps_current_and_idle_tasks() {
    let current = make_task();
    let queued = make_task();
    let idle = make_task();
    idle.sched.set_sched_attr(SchedAttr::idle());

    let rq = Runqueue::new();
    rq.set_current(alloc::sync::Arc::clone(&current));
    rq.enqueue(alloc::sync::Arc::clone(&queued), 1);
    rq.enqueue(alloc::sync::Arc::clone(&idle), 1);

    let drained = rq.drain_queued(2);

    assert_eq!(drained.len(), 1);
    assert!(alloc::sync::Arc::ptr_eq(&drained[0], &queued));
    assert!(rq.is_current(&current));
    assert_eq!(rq.nr_running(), 2);
    assert!(rq.dequeue(&current, 3));
    assert!(rq.dequeue(&idle, 3));
}

#[ktest]
fn runqueue_kernel_idle_current_never_enters_idle_tree() {
    let idle = make_task();
    idle.mark_idle_task();
    idle.sched.set_sched_attr(SchedAttr::idle());

    let rq = Runqueue::new();
    rq.set_current(alloc::sync::Arc::clone(&idle));

    for now in 1..=32 {
        let current = rq.pick_next(now).expect("kernel idle task");
        assert!(alloc::sync::Arc::ptr_eq(&current, &idle));
        assert!(rq.snapshot_runnable().is_empty());
        assert_eq!(rq.nr_running(), 1);
    }

    assert!(rq.dequeue(&idle, 33));
}

#[ktest]
fn runqueue_wake_current_sleep_transition_does_not_duplicate_task() {
    let task = make_task();
    let rq = Runqueue::new();
    rq.set_current(alloc::sync::Arc::clone(&task));
    task.set_state(TaskState::Sleeping);

    assert!(!rq.enqueue(alloc::sync::Arc::clone(&task), 1));
    assert!(rq.is_current(&task));
    assert_eq!(task.state(), TaskState::Running);
    // 唤醒把仍是 current 的任务拉回 Running 之后，它依然归属本 rq，因此
    // on_rq 必须保持为 QUEUED。若这里清成 NONE，紧随其后的一次远端唤醒就能
    // 通过 `enqueue` 的 on_rq 门禁，把同一个任务重复挂进第二个 rq。
    assert!(task.sched.on_rq());
    assert_eq!(task.sched.on_rq_state(), crate::eevdf::TASK_ON_RQ_QUEUED);
    assert!(!task.sched.is_migrating());
    assert_eq!(rq.nr_running(), 1);

    let current = rq.pick_next(2).expect("current task");
    assert!(alloc::sync::Arc::ptr_eq(&current, &task));
    assert_eq!(rq.nr_running(), 1);
    assert!(rq.dequeue(&current, 3));
}

#[ktest]
fn runqueue_take_migratable_respects_cpu_affinity() {
    let cpu0 = make_task();
    let cpu1 = make_task();
    cpu0.set_cpu_affinity(CpuMask::single_raw(0).bits());
    cpu1.set_cpu_affinity(CpuMask::single_raw(1).bits());

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&cpu0), 1);
    rq.enqueue(alloc::sync::Arc::clone(&cpu1), 1);

    let pulled = rq
        .take_migratable(CpuMask::single_raw(1).bits(), 2)
        .expect("cpu1 should pull an affinity-compatible task");
    assert!(alloc::sync::Arc::ptr_eq(&pulled, &cpu1));
    // 摘出来的任务处于"已离开源 rq、尚未挂上目标 rq"的中间态：on_rq 记为
    // MIGRATING 而不是 NONE，这样并发的唤醒者会等迁移落地而不是抢先入队。
    assert!(pulled.sched.is_migrating());
    assert_eq!(
        pulled.sched.on_rq_state(),
        crate::eevdf::TASK_ON_RQ_MIGRATING
    );
    assert!(pulled.sched.on_rq());
    assert_eq!(rq.migratable_load(), 1);
    assert!(
        rq.take_migratable(CpuMask::single_raw(2).bits(), 3)
            .is_none()
    );

    assert!(rq.dequeue(&cpu0, 4));
}

#[ktest]
fn runqueue_take_migratable_preserves_fair_lag() {
    let early = make_task();
    let late = make_task();
    early.sched.store_vruntime(100);
    late.sched.store_vruntime(300);

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&early), 1);
    rq.enqueue(alloc::sync::Arc::clone(&late), 1);

    let pulled = rq
        .take_migratable(CpuMask::single_raw(0).bits(), 2)
        .expect("fair task should be migratable");
    assert!(alloc::sync::Arc::ptr_eq(&pulled, &late));
    assert_eq!(pulled.sched.lag(), -200);

    assert!(rq.dequeue(&early, 3));
}

#[ktest]
fn runqueue_update_nice_keeps_policy_and_slice() {
    let task = make_task();
    task.sched
        .set_sched_attr(SchedAttr::rt_round_robin(20, 50_000_000));

    let rq = Runqueue::new();
    rq.enqueue(alloc::sync::Arc::clone(&task), 1);
    rq.update_nice(&task, 10, 2);

    let attr = task.sched.sched_attr();
    assert_eq!(attr.policy, SchedPolicy::RtRoundRobin);
    assert_eq!(attr.priority, 20);
    assert_eq!(attr.slice_ns, 50_000_000);
    assert_eq!(attr.nice, 10);

    assert!(rq.dequeue(&task, 3));
}

#[ktest]
fn runqueue_update_wrong_queue_does_not_mutate_entity() {
    let task = make_task();
    task.sched.set_on_rq(true);
    task.set_current_cpu(1);

    let rq = Runqueue::new();
    assert!(!rq.update_nice(&task, -10, 1));
    assert_eq!(task.sched.nice(), 0);
    assert!(task.sched.on_rq());
}

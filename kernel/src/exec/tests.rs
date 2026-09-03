use alloc::sync::{Arc, Weak};
use core::cell::Cell;

use errno::Errno;
use ktest::ktest;
use native_abi::{ExecPhase, UserAbiKind};
use sched::{
    NativeExternalControl, ProcessGroup, ProcessPersonalityState, SchedParams, Session, SigAction,
    SigActionFlags, SigHandler, SigInfo, SigProcMaskHow, SigSet, SignalNumber, TASKEXT_EXEC_ACCESS,
    TASKEXT_EXEC_ARGS, TASKEXT_EXEC_ENVP, TASKEXT_EXEC_PATH, TASKEXT_VFS_FDTABLE, Task,
    ThreadGroup, Uid,
};
use vfs::fdtable::FdTable;

use super::{
    ExecSnapshot, INSTALL_STEPS, abort_exec_transition_before_ponr, drive_install_steps,
    finish_exec_commit, plan_exec_thread_transition, release_before_diverging,
    reset_signal_state_for_exec, revalidate_before_ponr,
};

struct DropProbe<'a>(&'a Cell<usize>);

impl Drop for DropProbe<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

fn make_task() -> Arc<Task> {
    let session = Session::new();
    let process_group = ProcessGroup::new(&session);
    session.register_group(&process_group);
    Task::new(
        SchedParams::default_fair(),
        Weak::new(),
        ThreadGroup::new(),
        process_group,
    )
}

fn make_task_in_group(group: Arc<ThreadGroup>) -> Arc<Task> {
    let session = Session::new();
    let process_group = ProcessGroup::new(&session);
    session.register_group(&process_group);
    Task::new(
        SchedParams::default_fair(),
        Weak::new(),
        group,
        process_group,
    )
}

#[ktest]
fn setid_root_exec_keeps_capabilities_when_identity_is_unchanged() {
    let task = make_task();
    // Alpine's mount helper is setuid-root. Executing it from PID 1 (already
    // root) must not turn a no-op UID transition into a capability drop.
    assert!(super::compute_exec_credentials(&task, Some((0, 0, 0o4755))).is_none());
}

#[ktest]
fn revalidate_failure_keeps_old_exec_state_running() {
    let group = ThreadGroup::new();
    let mut guard = group.lock_exec();

    let result = revalidate_before_ponr(&mut guard, 0, || Err(Errno::EAGAIN));

    assert_eq!(result, Err(Errno::EAGAIN));
    assert_eq!(guard.phase(), ExecPhase::Running);
    assert_eq!(guard.generation(), 0);
}

#[ktest]
fn revalidate_rejects_changed_exec_generation_without_transitioning() {
    let group = ThreadGroup::new();
    let mut guard = group.lock_exec();
    guard.advance_generation();

    let result = revalidate_before_ponr(&mut guard, 0, || Ok(()));

    assert_eq!(result, Err(Errno::EAGAIN));
    assert_eq!(guard.phase(), ExecPhase::Running);
    assert_eq!(guard.generation(), 1);
}

#[ktest]
fn single_thread_revalidation_failure_can_restore_running_before_ponr() {
    let group = ThreadGroup::new();
    let mut guard = group.lock_exec();
    guard.set_phase(ExecPhase::Transitioning);

    let result = abort_exec_transition_before_ponr(&mut guard, Errno::EAGAIN);

    assert_eq!(result, Err(Errno::EAGAIN));
    assert_eq!(guard.phase(), ExecPhase::Running);
    assert_eq!(guard.generation(), 0);
}

#[ktest]
fn revalidate_rejects_replaced_executable_access_lease() {
    let task = make_task();
    for key in [
        TASKEXT_EXEC_PATH,
        TASKEXT_EXEC_ARGS,
        TASKEXT_EXEC_ENVP,
        TASKEXT_EXEC_ACCESS,
    ] {
        task.ext_install(key, Arc::new(1usize));
    }
    let snapshot = ExecSnapshot {
        exec_generation: 0,
        source_abi: UserAbiKind::TomoriLinux,
        vm: None,
        fdtable: None,
        vfs_context: None,
        vfs_generation: 0,
        exec_access: task
            .ext_lookup(TASKEXT_EXEC_ACCESS)
            .expect("exec access 扩展槽应已存在"),
        shared_signal: task.shared_signal(),
        signal_generation: task.shared_signal().actions_generation(),
    };

    task.ext_replace(TASKEXT_EXEC_ACCESS, Arc::new(2usize))
        .expect("exec access 扩展槽应已存在");

    assert_eq!(snapshot.revalidate(&task), Err(Errno::EAGAIN));
}

#[ktest]
fn every_post_ponr_install_failure_enters_terminating() {
    for fault in INSTALL_STEPS {
        let group = ThreadGroup::new();
        let mut guard = group.lock_exec();
        guard.set_phase(ExecPhase::Transitioning);

        let result = drive_install_steps(&mut guard, |_, step| {
            if step == fault {
                Err(Errno::EIO)
            } else {
                Ok(())
            }
        });

        assert_eq!(result, Err(Errno::EIO));
        assert_eq!(guard.phase(), ExecPhase::Terminating);
        assert_eq!(guard.generation(), 0);
    }
}

#[ktest]
fn complete_install_stays_transitioning_until_handoffs_finish() {
    let group = ThreadGroup::new();
    let mut guard = group.lock_exec();
    guard.set_phase(ExecPhase::Transitioning);

    drive_install_steps(&mut guard, |_, _| Ok::<(), Errno>(())).expect("完整安装序列应成功");

    assert_eq!(guard.phase(), ExecPhase::Transitioning);
    assert_eq!(guard.generation(), 0);
}

#[ktest]
fn failed_handoff_enters_terminating_without_advancing_generation() {
    let group = ThreadGroup::new();
    let mut guard = group.lock_exec();
    guard.set_phase(ExecPhase::Transitioning);

    let result = finish_exec_commit(&mut guard, false);

    assert_eq!(result, Err(Errno::ENOMEM));
    assert_eq!(guard.phase(), ExecPhase::Terminating);
    assert_eq!(guard.generation(), 0);
}

#[ktest]
fn successful_handoff_publishes_new_running_generation() {
    let group = ThreadGroup::new();
    let mut guard = group.lock_exec();
    guard.set_phase(ExecPhase::Transitioning);
    drive_install_steps(&mut guard, |_, _| Ok::<(), Errno>(())).expect("完整安装序列应成功");

    finish_exec_commit(&mut guard, true).expect("handoff 成功后应发布新映像");

    assert_eq!(guard.phase(), ExecPhase::Running);
    assert_eq!(guard.generation(), 1);
}

#[ktest]
fn termination_releases_owned_resources_before_diverging_action() {
    let dropped = Cell::new(0);
    let resources = [
        DropProbe(&dropped),
        DropProbe(&dropped),
        DropProbe(&dropped),
    ];

    let observed = release_before_diverging(resources, || dropped.get());

    assert_eq!(observed, 3);
    assert_eq!(dropped.get(), 3);
}

#[ktest]
fn fdtable_owner_detection_ignores_temporary_arc_references() {
    let task = make_task();
    let table = Arc::new(FdTable::new_default());
    task.ext_install(TASKEXT_VFS_FDTABLE, table.clone());
    let _temporary = Arc::clone(&table);

    assert!(!crate::syscalls::fdtable_has_other_live_owner_in(
        &task,
        &table,
        [&task],
    ));

    let other = make_task();
    other.ext_install(TASKEXT_VFS_FDTABLE, table.clone());
    assert!(crate::syscalls::fdtable_has_other_live_owner_in(
        &task,
        &table,
        [&task, &other],
    ));
}

#[ktest]
fn cross_personality_exec_rejects_a_multithreaded_process() {
    let group = ThreadGroup::new();
    let executor = make_task_in_group(Arc::clone(&group));
    let sibling = make_task_in_group(Arc::clone(&group));
    group.set_leader(&executor);
    group.add_member(&executor);
    group.add_member(&sibling);
    let guard = group.lock_exec();

    let result = plan_exec_thread_transition(
        &guard,
        &executor,
        UserAbiKind::TomoriLinux,
        UserAbiKind::MygoNative,
    );

    assert_eq!(result.err(), Some(Errno::EBUSY));
}

#[ktest]
fn native_exec_makes_blocked_pending_signal_consumable() {
    let task = make_task();
    let group = task.thread_group();
    group.set_leader(&task);
    group.add_member(&task);
    task.shared_signal().set_action(
        SignalNumber::SIGUSR1,
        SigAction {
            handler: SigHandler::Ignore,
            mask: SigSet::EMPTY,
            flags: SigActionFlags(0),
            restorer: 0,
        },
    );
    task.signal.block(
        SigSet::EMPTY.with(SignalNumber::SIGUSR1),
        SigProcMaskHow::SetMask,
    );
    task.signal.deliver(SigInfo {
        sig: SignalNumber::SIGUSR1,
        code: 0,
        sender_pid: 1,
        sender_uid: Uid::ROOT,
        raw: None,
    });

    let payload: Arc<dyn core::any::Any + Send + Sync> = Arc::new(());
    let mut guard = group.lock_exec();
    guard.set_phase(ExecPhase::Transitioning);
    guard.install_personality(ProcessPersonalityState::MygoNative(payload));
    reset_signal_state_for_exec(&task, UserAbiKind::MygoNative);
    assert!(task.signal.has_any_pending());
    guard.set_phase(ExecPhase::Running);
    drop(guard);

    assert_eq!(
        sched::operation::consume_native_external_control_for_task(&task),
        NativeExternalControl::Continue
    );
    assert!(!task.signal.has_any_pending());
}

#[ktest]
fn tomori_exec_keeps_the_thread_signal_mask() {
    let task = make_task();
    let blocked = SigSet::EMPTY.with(SignalNumber::SIGUSR1);
    task.signal.block(blocked, SigProcMaskHow::SetMask);

    reset_signal_state_for_exec(&task, UserAbiKind::TomoriLinux);

    assert_eq!(task.signal.blocked_snapshot(), blocked);
}

#[ktest]
fn exec_planning_rejects_a_group_exit_already_in_progress() {
    let group = ThreadGroup::new();
    let executor = make_task_in_group(Arc::clone(&group));
    group.set_leader(&executor);
    group.add_member(&executor);
    group.request_group_exit(9);
    let guard = group.lock_exec();

    let result = plan_exec_thread_transition(
        &guard,
        &executor,
        UserAbiKind::TomoriLinux,
        UserAbiKind::TomoriLinux,
    );

    assert_eq!(result.err(), Some(Errno::EBUSY));
}

#[ktest]
fn tomori_nonleader_exec_plans_leader_identity_adoption() {
    let group = ThreadGroup::new();
    let leader = make_task_in_group(Arc::clone(&group));
    let executor = make_task_in_group(Arc::clone(&group));
    group.set_leader(&leader);
    group.add_member(&leader);
    group.add_member(&executor);
    let guard = group.lock_exec();

    let transition = plan_exec_thread_transition(
        &guard,
        &executor,
        UserAbiKind::TomoriLinux,
        UserAbiKind::TomoriLinux,
    )
    .expect("Tomori exec 应规划 de-thread");

    assert_eq!(transition.siblings.len(), 1);
    assert!(Arc::ptr_eq(&transition.siblings[0], &leader));
    assert!(
        transition
            .replaced_leader
            .as_ref()
            .is_some_and(|task| Arc::ptr_eq(task, &leader))
    );
}

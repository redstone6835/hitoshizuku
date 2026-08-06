use alloc::sync::{Arc, Weak};

use errno::Errno;
use ktest::ktest;
use native_abi::{ExecPhase, UserAbiKind};
use sched::{
    ProcessGroup, SchedParams, Session, Task, ThreadGroup, TASKEXT_EXEC_ACCESS, TASKEXT_EXEC_ARGS,
    TASKEXT_EXEC_ENVP, TASKEXT_EXEC_PATH,
};

use super::{drive_install_steps, revalidate_before_ponr, ExecSnapshot, INSTALL_STEPS};

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
fn complete_install_publishes_new_running_generation() {
    let group = ThreadGroup::new();
    let mut guard = group.lock_exec();
    guard.set_phase(ExecPhase::Transitioning);

    drive_install_steps(&mut guard, |_, _| Ok::<(), Errno>(())).expect("完整安装序列应成功");

    assert_eq!(guard.phase(), ExecPhase::Running);
    assert_eq!(guard.generation(), 1);
}

//! 调度器状态所有权与 per-CPU 状态隔离测试。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::std::sync::Barrier;
use super::std::thread;
use ktest::ktest;

use super::test_thread_metadata::make_task;
use crate::ids::Uid;
use crate::scheduler::{
    cpu_ready_for_activation, dequeue_for_state_change_on, enqueue_task_on_scheduler,
    offline_cpu_with_scheduler, record_deferred_timer_tick, refresh_task_placement,
    requeue_balance_task_on, take_deferred_timer_tick, task_runqueue_cpu_on,
};
use crate::{
    BorrowedCurrentTask, CpuId, CpuMask, HandoffReason, HandoffTarget, PlacementState,
    RunqueueClassLoad, SCHED_CAPACITY_SCALE, SchedAttr, SchedClass, SchedDomain, SchedTopology,
    Scheduler, SigInfo, SigProcMaskHow, SigSet, SignalNumber, TaskState,
};

#[ktest]
fn borrowed_current_does_not_change_strong_count() {
    let task = make_task();
    let raw_owner = Arc::into_raw(Arc::clone(&task));
    let before = Arc::strong_count(&task);
    // Safety: raw_owner 是模拟 current_raw 槽持有的有效 Arc 裸指针。
    let borrowed = unsafe { BorrowedCurrentTask::from_current_raw(raw_owner) };

    assert!(!core::mem::needs_drop::<BorrowedCurrentTask>());
    assert_eq!(Arc::strong_count(&task), before);
    let promoted = borrowed.to_arc();
    assert_eq!(Arc::strong_count(&task), before + 1);
    drop(promoted);
    drop(borrowed);
    assert_eq!(Arc::strong_count(&task), before);

    // Safety: raw_owner 由上面的 Arc::into_raw 产生，尚未被消费。
    unsafe { drop(Arc::from_raw(raw_owner)) };
}

#[ktest]
fn borrowed_current_survives_raw_slot_replacement_with_prev_owner() {
    let task = make_task();
    let raw_owner = Arc::into_raw(Arc::clone(&task));
    // Safety: raw_owner 是模拟 current_raw 槽持有的有效 Arc 裸指针。
    let borrowed = unsafe { BorrowedCurrentTask::from_current_raw(raw_owner) };
    let suspended_stack_prev = Arc::clone(&task);

    // 模拟调度器 publish next 时释放旧 current_raw；暂停栈上的 prev Arc 继续托底。
    unsafe { drop(Arc::from_raw(raw_owner)) };
    drop(task);
    assert!(Arc::ptr_eq(borrowed.as_arc(), &suspended_stack_prev));
    let promoted = borrowed.to_arc();
    assert!(Arc::ptr_eq(&promoted, &suspended_stack_prev));
}

#[ktest]
fn blocked_signal_hint_is_cleared_and_rearmed_on_unblock() {
    let task = make_task();
    let blocked = SigSet::EMPTY.with(SignalNumber::SIGUSR1);
    task.signal.block(blocked, SigProcMaskHow::SetMask);
    task.signal.deliver(SigInfo {
        sig: SignalNumber::SIGUSR1,
        code: 0,
        sender_pid: 1,
        sender_uid: Uid::ROOT,
        raw: None,
    });

    assert!(!task.has_deliverable_signal());
    assert!(task.user_return_work_hint_relaxed());
    assert!(!task.refresh_user_return_work_hint());
    assert!(!task.user_return_work_hint_relaxed());

    task.signal.block(SigSet::EMPTY, SigProcMaskHow::SetMask);
    assert!(task.has_deliverable_signal());
    assert!(task.user_return_work_hint_relaxed());
    assert!(task.refresh_user_return_work_hint());
}

#[ktest]
fn task_work_hint_preserves_publish_after_clear() {
    let task = make_task();
    task.mark_user_return_work();
    assert!(task.signal.take_user_return_work());

    let producer = Arc::clone(&task);
    let barrier = Arc::new(Barrier::new(2));
    let producer_barrier = Arc::clone(&barrier);
    let handle = thread::spawn(move || {
        producer_barrier.wait();
        producer.mark_user_return_work();
    });
    barrier.wait();
    handle.join().expect("producer thread");

    assert!(task.user_return_work_hint_acquire());
}

#[ktest]
fn cpu_work_hint_preserves_publish_after_clear() {
    let scheduler = Arc::new(Scheduler::new());
    let cpu = scheduler.cpu(0).expect("boot cpu state");
    cpu.request_resched();
    assert!(cpu.take_user_return_work());
    assert!(cpu.take_resched());

    let producer_scheduler = Arc::clone(&scheduler);
    let barrier = Arc::new(Barrier::new(2));
    let producer_barrier = Arc::clone(&barrier);
    let handle = thread::spawn(move || {
        producer_barrier.wait();
        producer_scheduler
            .cpu(0)
            .expect("boot cpu state")
            .request_resched();
    });
    barrier.wait();
    handle.join().expect("producer thread");

    assert!(cpu.user_return_work_pending_acquire());
    assert!(cpu.needs_resched());
}

#[ktest]
fn scheduler_state_bootstraps_root_and_boot_cpu() {
    let core = Scheduler::new();

    assert_eq!(core.online_set(), CpuMask::BOOT);
    assert_eq!(core.active_set(), CpuMask::BOOT);
    assert_eq!(core.topology(), SchedTopology::bootstrap());
    assert!(core.cpu(CpuId::boot().get()).is_some());
    assert!(core.cpu(crate::scheduler::NR_CPUS).is_none());
}

#[ktest]
fn scheduler_state_keeps_cpu_intents_isolated() {
    let core = Scheduler::new();
    let cpu0 = core.cpu(0).expect("cpu0 state");
    let cpu1 = core.cpu(1).expect("cpu1 state");

    cpu1.request_resched();
    assert!(cpu1.claim_resched_notification());
    assert!(!cpu1.claim_resched_notification());
    cpu1.request_balance();
    cpu1.request_post_syscall_handoff(1);
    cpu1.request_post_syscall_handoff(1);

    assert!(!cpu0.needs_resched());
    assert!(!cpu0.user_return_work_pending_relaxed());
    assert!(!cpu0.take_balance());
    assert!(!cpu0.has_post_syscall_handoff());
    assert!(cpu1.needs_resched());
    assert!(cpu1.user_return_work_pending_relaxed());
    assert!(cpu1.take_balance());
    let (rounds, target) = cpu1.take_post_syscall_handoff();
    assert_eq!(rounds, 1);
    assert!(target.is_none());
    assert!(cpu1.take_resched());
    assert!(cpu1.claim_resched_notification());
    assert!(!cpu1.needs_resched());
    assert!(cpu1.take_user_return_work());
    assert!(!cpu1.user_return_work_authoritative());
}

#[ktest]
fn scheduler_state_merges_the_same_exact_handoff_target() {
    let core = Scheduler::new();
    let cpu1 = core.cpu(1).expect("cpu1 state");
    let task = make_task();
    let target = HandoffTarget::new(task, 1, HandoffReason::SocketRead, true);

    cpu1.request_targeted_handoff(target.clone());
    cpu1.request_targeted_handoff(target);

    let (rounds, pending) = cpu1.take_post_syscall_handoff();
    let pending = pending.expect("精确请求应保留");
    assert_eq!(rounds, 0);
    assert_eq!(pending.preferred_cpu(), 1);
    assert_eq!(pending.request_generation, 1);
    assert!(!cpu1.has_post_syscall_handoff());
}

#[ktest]
fn scheduler_state_keeps_socket_consumer_continuation_identity() {
    let core = Scheduler::new();
    let cpu1 = core.cpu(1).expect("cpu1 state");
    let task = make_task();
    let target = HandoffTarget::new(
        Arc::clone(&task),
        1,
        HandoffReason::SocketReadContinuation,
        false,
    );

    cpu1.request_targeted_handoff(target);

    let (rounds, pending) = cpu1.take_post_syscall_handoff();
    let pending = pending.expect("消费者交接目标应保留");
    assert_eq!(rounds, 0);
    assert!(Arc::ptr_eq(&pending.task, &task));
    assert_eq!(pending.reason, HandoffReason::SocketReadContinuation);
    assert!(!pending.woke_from_sleep);
}

#[ktest]
fn reschedule_notification_can_be_rearmed_after_interrupt_ack() {
    let scheduler = Scheduler::new();
    let cpu = scheduler.cpu(0).expect("boot cpu state");

    cpu.request_resched();
    assert!(cpu.claim_resched_notification());
    cpu.clear_resched_notification();
    assert!(cpu.claim_resched_notification());
}

#[ktest]
fn deferred_timer_tick_coalesces_latest_timestamp_and_clears_on_take() {
    let slot = AtomicU64::new(0);

    record_deferred_timer_tick(&slot, 30);
    record_deferred_timer_tick(&slot, 20);
    record_deferred_timer_tick(&slot, 40);

    assert_eq!(take_deferred_timer_tick(&slot), 40);
    assert_eq!(take_deferred_timer_tick(&slot), 0);
}

#[ktest]
fn scheduler_state_installs_topology_and_online_cpu_together() {
    let core = Scheduler::new();
    let old_generation = core.topology_snapshot().generation;
    assert_eq!(core.topology_generation(), old_generation);
    let local = SchedDomain::new(
        1,
        CpuMask::single_raw(0).union(CpuMask::single_raw(1)),
        1,
        Some(0),
    )
    .expect("local domain");
    let topology =
        SchedTopology::from_domains(&[SchedDomain::root(), local]).expect("valid topology");

    core.install_topology(topology);
    core.register_cpu(CpuId::new(1).expect("cpu1"));

    assert_eq!(core.topology(), topology);
    assert!(core.topology_snapshot().generation > old_generation);
    assert_eq!(
        core.topology_generation(),
        core.topology_snapshot().generation
    );
    assert!(core.online_set().contains(CpuId::boot()));
    assert!(core.online_set().contains(CpuId::new(1).unwrap()));
    assert!(core.active_set().contains(CpuId::new(1).unwrap()));
}

#[ktest]
fn scheduler_state_deactivates_cpu_before_removing_online_state() {
    let scheduler = two_cpu_scheduler();
    let cpu1 = CpuId::new(1).expect("cpu1");

    assert!(scheduler.deactivate_cpu(cpu1));
    assert!(scheduler.online_set().contains(cpu1));
    assert!(!scheduler.active_set().contains(cpu1));
    assert!(scheduler.activate_cpu(cpu1));
    assert!(scheduler.active_set().contains(cpu1));
    assert!(scheduler.unregister_cpu(cpu1));
    assert!(!scheduler.online_set().contains(cpu1));
    assert!(!scheduler.active_set().contains(cpu1));
}

#[ktest]
fn scheduler_state_publishes_online_before_active() {
    let scheduler = Scheduler::new();
    let cpu1 = CpuId::new(1).expect("cpu1");

    assert!(scheduler.mark_cpu_online(cpu1));
    assert!(scheduler.online_set().contains(cpu1));
    assert!(!scheduler.active_set().contains(cpu1));
    assert!(scheduler.activate_cpu(cpu1));
    assert!(scheduler.active_set().contains(cpu1));
    assert!(!scheduler.mark_cpu_offline(cpu1));
    assert!(scheduler.deactivate_cpu(cpu1));
    assert!(scheduler.mark_cpu_offline(cpu1));
}

#[ktest]
fn unbound_task_placement_does_not_use_compatibility_cpu_mirror() {
    let scheduler = two_cpu_scheduler();
    let busy = make_task();
    bind_to_cpu(&scheduler, &busy, 0);
    scheduler
        .cpu_or_boot(0)
        .runqueue()
        .set_current(Arc::clone(&busy));

    let task = make_task();
    task.set_cpu_affinity(CpuMask::single_raw(0).union(CpuMask::single_raw(1)).bits());

    assert_eq!(task.placement().state, PlacementState::Unbound);
    assert_eq!(task.current_cpu(), 0);

    let cpu_id = enqueue_task_on_scheduler(&scheduler, Arc::clone(&task), 1, false, true);

    assert_eq!(cpu_id, 1);
    assert_eq!(task.placement().cpu, CpuId::new(1));
    assert!(scheduler.cpu_or_boot(1).runqueue().dequeue_queued(&task, 2));
    let _ = scheduler.cpu_or_boot(0).runqueue().dequeue(&busy, 2);
}

#[ktest]
fn cpu_activation_requires_current_and_idle() {
    let scheduler = Scheduler::new();
    let cpu1 = CpuId::new(1).expect("cpu1");
    let current = make_task();
    let idle = make_task();
    idle.mark_idle_task();
    idle.sched.set_sched_attr(SchedAttr::idle());
    scheduler.mark_cpu_online(cpu1);

    assert!(!cpu_ready_for_activation(&scheduler, 1));
    scheduler
        .cpu_or_boot(1)
        .runqueue()
        .set_current(Arc::clone(&current));
    scheduler
        .cpu_or_boot(1)
        .publish_current(Arc::clone(&current));
    assert!(!cpu_ready_for_activation(&scheduler, 1));
    assert!(scheduler.cpu_or_boot(1).install_idle(idle).is_ok());
    assert!(cpu_ready_for_activation(&scheduler, 1));

    let _ = scheduler.cpu_or_boot(1).clear_current();
    assert!(scheduler.cpu_or_boot(1).runqueue().dequeue(&current, 1));
    let _ = scheduler.cpu_or_boot(1).clear_idle();
}

#[ktest]
fn cpu_deactivation_waits_for_enqueue_in_progress() {
    let scheduler = Arc::new(two_cpu_scheduler());
    let cpu1 = CpuId::new(1).expect("cpu1");
    let enqueue_guard = scheduler.cpu_or_boot(1).begin_enqueue();
    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));

    let worker_scheduler = Arc::clone(&scheduler);
    let worker_started = Arc::clone(&started);
    let worker_finished = Arc::clone(&finished);
    let worker = thread::spawn(move || {
        worker_started.store(true, Ordering::Release);
        assert!(worker_scheduler.deactivate_cpu(cpu1));
        worker_finished.store(true, Ordering::Release);
    });

    while !started.load(Ordering::Acquire) {
        thread::yield_now();
    }
    while scheduler.active_set().contains(cpu1) {
        thread::yield_now();
    }
    assert!(!finished.load(Ordering::Acquire));
    drop(enqueue_guard);
    worker.join().expect("deactivate worker");
    assert!(finished.load(Ordering::Acquire));
}

#[ktest]
fn scheduler_state_aggregates_load_for_nested_domains() {
    let scheduler = Scheduler::new();
    let cluster = SchedDomain::new(
        1,
        CpuMask::single_raw(0).union(CpuMask::single_raw(1)),
        1,
        Some(0),
    )
    .expect("cluster domain");
    let cpu0 = SchedDomain::new(2, CpuMask::single_raw(0), 2, Some(1)).expect("cpu0 domain");
    let topology =
        SchedTopology::from_domains(&[SchedDomain::root(), cluster, cpu0]).expect("valid topology");
    scheduler.install_topology(topology);
    scheduler.register_cpu(CpuId::new(1).expect("cpu1"));

    let mut cpu_loads = [RunqueueClassLoad::default(); crate::NR_CPUS];
    cpu_loads[0].fair = 2;
    cpu_loads[0].fair_weight = 2 * SCHED_CAPACITY_SCALE;
    cpu_loads[1].realtime = 1;
    let snapshot = scheduler.topology_snapshot();
    scheduler.update_domain_stats(snapshot, &cpu_loads);

    let root = scheduler.domain_stats(0).expect("root stats");
    let cluster = scheduler.domain_stats(1).expect("cluster stats");
    let leaf = scheduler.domain_stats(2).expect("leaf stats");
    assert_eq!(root.load, cluster.load);
    assert_eq!(cluster.load.fair, 2);
    assert_eq!(cluster.load.realtime, 1);
    assert_eq!(cluster.capacity, 2 * SCHED_CAPACITY_SCALE);
    assert_eq!(leaf.load.fair, 2);
    assert_eq!(leaf.capacity, SCHED_CAPACITY_SCALE);
    assert_eq!(cluster.utilization(SchedClass::Fair), SCHED_CAPACITY_SCALE);
}

#[ktest]
fn cpu_offline_drains_queued_tasks_to_active_cpu() {
    let scheduler = two_cpu_scheduler();
    let task = make_task();
    task.set_cpu_affinity(CpuMask::single_raw(0).union(CpuMask::single_raw(1)).bits());
    bind_to_cpu(&scheduler, &task, 1);
    assert!(
        scheduler
            .cpu_or_boot(1)
            .runqueue()
            .enqueue(Arc::clone(&task), 1)
    );

    offline_cpu_with_scheduler(&scheduler, 1, 2).expect("offline cpu1");

    let cpu1 = CpuId::new(1).expect("cpu1");
    assert!(!scheduler.online_set().contains(cpu1));
    assert!(!scheduler.active_set().contains(cpu1));
    assert_eq!(task.placement().cpu, CpuId::new(0));
    assert_eq!(task.placement().state, PlacementState::Bound);
    assert!(scheduler.cpu_or_boot(0).runqueue().dequeue_queued(&task, 3));
    assert_eq!(scheduler.cpu_or_boot(1).runqueue().nr_running(), 0);
}

#[ktest]
fn cpu_offline_moves_sleeping_deadline_reservation() {
    let scheduler = two_cpu_scheduler();
    let task = make_task();
    task.set_state(TaskState::Sleeping);
    task.set_cpu_affinity(CpuMask::single_raw(0).union(CpuMask::single_raw(1)).bits());
    bind_to_cpu(&scheduler, &task, 1);
    let attr = SchedAttr::deadline(500, 500, 1_000).normalized();
    scheduler
        .deadline_admission()
        .update_attr(
            &task,
            CpuId::new(1).expect("cpu1"),
            attr,
            SCHED_CAPACITY_SCALE,
            || {
                task.sched.set_sched_attr(attr);
                true
            },
        )
        .expect("睡眠 Deadline 任务应准入");

    offline_cpu_with_scheduler(&scheduler, 1, 2).expect("offline cpu1");

    assert_eq!(task.state(), TaskState::Sleeping);
    assert_eq!(task.placement().cpu, CpuId::new(0));
    assert_eq!(
        scheduler.deadline_admission().reservation_of(&task),
        Some((CpuId::new(0).expect("cpu0"), 512))
    );
    assert_eq!(
        enqueue_task_on_scheduler(&scheduler, Arc::clone(&task), 3, false, true),
        0
    );
    assert!(scheduler.cpu_or_boot(0).runqueue().dequeue_queued(&task, 4));
    scheduler.deadline_admission().release(&task);
}

#[ktest]
fn cpu_offline_restores_source_when_affinity_has_no_target() {
    let scheduler = two_cpu_scheduler();
    let task = make_task();
    task.set_cpu_affinity(CpuMask::single_raw(1).bits());
    bind_to_cpu(&scheduler, &task, 1);
    let source = task.placement();
    assert!(
        scheduler
            .cpu_or_boot(1)
            .runqueue()
            .enqueue(Arc::clone(&task), 1)
    );

    assert_eq!(
        offline_cpu_with_scheduler(&scheduler, 1, 2),
        Err(errno::Errno::EBUSY)
    );

    let cpu1 = CpuId::new(1).expect("cpu1");
    assert!(scheduler.online_set().contains(cpu1));
    assert!(scheduler.active_set().contains(cpu1));
    assert_eq!(task.placement(), source);
    assert!(scheduler.cpu_or_boot(1).runqueue().dequeue_queued(&task, 3));
}

#[ktest]
fn cpu_offline_rejects_non_idle_current_and_boot_cpu() {
    let scheduler = two_cpu_scheduler();
    let task = make_task();
    bind_to_cpu(&scheduler, &task, 1);
    let cpu1 = scheduler.cpu_or_boot(1);
    cpu1.runqueue().set_current(Arc::clone(&task));
    cpu1.publish_current(Arc::clone(&task));

    assert_eq!(
        offline_cpu_with_scheduler(&scheduler, 1, 2),
        Err(errno::Errno::EBUSY)
    );
    assert_eq!(
        offline_cpu_with_scheduler(&scheduler, 0, 2),
        Err(errno::Errno::EBUSY)
    );

    let _ = cpu1.clear_current();
    assert!(cpu1.runqueue().dequeue(&task, 3));
}

#[ktest]
fn cpu_offline_removes_idle_current_and_cpu_requests() {
    let scheduler = two_cpu_scheduler();
    let idle = make_task();
    idle.mark_idle_task();
    idle.sched.set_sched_attr(SchedAttr::idle());
    idle.set_cpu_affinity(CpuMask::single_raw(1).bits());
    bind_to_cpu(&scheduler, &idle, 1);
    let cpu1 = scheduler.cpu_or_boot(1);
    assert!(cpu1.install_idle(Arc::clone(&idle)).is_ok());
    cpu1.runqueue().set_current(Arc::clone(&idle));
    cpu1.publish_current(Arc::clone(&idle));
    cpu1.request_resched();
    cpu1.request_balance();

    offline_cpu_with_scheduler(&scheduler, 1, 2).expect("offline idle cpu");

    assert!(cpu1.current().is_none());
    assert!(cpu1.idle().is_none());
    assert!(cpu1.runqueue().current().is_none());
    assert!(!cpu1.needs_resched());
    assert!(!cpu1.take_balance());
    assert_eq!(idle.state(), TaskState::Stopped);
    assert_eq!(idle.placement().state, PlacementState::Unbound);
}

#[ktest]
fn cpu_offline_cancels_inactive_startup_cpu() {
    let scheduler = Scheduler::new();
    let cpu1_id = CpuId::new(1).expect("cpu1");
    let idle = make_task();
    idle.mark_idle_task();
    idle.sched.set_sched_attr(SchedAttr::idle());
    idle.set_cpu_affinity(CpuMask::single_raw(1).bits());
    bind_to_cpu(&scheduler, &idle, 1);

    assert!(scheduler.mark_cpu_online(cpu1_id));
    assert!(
        scheduler
            .cpu_or_boot(1)
            .install_idle(Arc::clone(&idle))
            .is_ok()
    );
    assert!(!scheduler.active_set().contains(cpu1_id));

    offline_cpu_with_scheduler(&scheduler, 1, 2).expect("cancel inactive CPU startup");

    assert!(!scheduler.online_set().contains(cpu1_id));
    assert!(scheduler.cpu_or_boot(1).idle().is_none());
    assert_eq!(idle.state(), TaskState::Stopped);
    assert_eq!(idle.placement().state, PlacementState::Unbound);
}

#[ktest]
fn topology_refresh_updates_runnable_and_sleeping_placements() {
    let scheduler = two_cpu_scheduler();
    let old_generation = scheduler.topology_snapshot().generation;
    let runnable = make_task();
    let sleeping = make_task();
    runnable.set_state(TaskState::Runnable);
    sleeping.set_state(TaskState::Sleeping);
    runnable.bind_placement(CpuId::boot(), 0, old_generation);
    sleeping.bind_placement(CpuId::boot(), 0, old_generation);

    scheduler.install_topology(SchedTopology::with_cpu_domains());
    let new_generation = scheduler.topology_snapshot().generation;
    assert!(refresh_task_placement(&scheduler, &runnable));
    assert!(refresh_task_placement(&scheduler, &sleeping));

    for task in [&runnable, &sleeping] {
        assert_eq!(task.placement().domain_id, 1);
        assert_eq!(task.placement().topology_generation, new_generation);
        assert_eq!(task.placement().state, PlacementState::Bound);
    }
}

#[ktest]
fn runqueue_owner_refreshes_topology_and_current_cpu_mirror() {
    let scheduler = two_cpu_scheduler();
    let task = make_task();
    task.set_cpu_affinity(CpuMask::single_raw(0).union(CpuMask::single_raw(1)).bits());
    bind_to_cpu(&scheduler, &task, 1);
    task.set_current_cpu(0);

    scheduler.install_topology(SchedTopology::bootstrap());
    let generation = scheduler.topology_snapshot().generation;

    assert_eq!(task_runqueue_cpu_on(&scheduler, &task), CpuId::new(1));
    assert_eq!(task.current_cpu(), 1);
    assert_eq!(task.placement().domain_id, 0);
    assert_eq!(task.placement().topology_generation, generation);
}

#[ktest]
fn state_change_uses_placement_when_current_cpu_mirror_is_stale() {
    let scheduler = two_cpu_scheduler();
    let task = make_task();
    task.set_cpu_affinity(CpuMask::single_raw(0).union(CpuMask::single_raw(1)).bits());
    bind_to_cpu(&scheduler, &task, 1);
    assert!(
        scheduler
            .cpu_or_boot(1)
            .runqueue()
            .enqueue(Arc::clone(&task), 1)
    );
    task.set_current_cpu(0);

    assert!(dequeue_for_state_change_on(&scheduler, &task, 0, 2));
    assert_eq!(task.current_cpu(), 1);
    assert!(!task.sched.on_rq());
    assert_eq!(scheduler.cpu_or_boot(0).runqueue().nr_running(), 0);
    assert_eq!(scheduler.cpu_or_boot(1).runqueue().nr_running(), 0);
}

#[ktest]
fn task_enqueue_falls_back_when_previous_cpu_is_inactive() {
    let scheduler = two_cpu_scheduler();
    let task = make_task();
    task.set_state(TaskState::Sleeping);
    task.set_cpu_affinity(CpuMask::single_raw(0).union(CpuMask::single_raw(1)).bits());
    bind_to_cpu(&scheduler, &task, 1);
    assert!(scheduler.deactivate_cpu(CpuId::new(1).expect("cpu1")));

    let cpu_id = enqueue_task_on_scheduler(&scheduler, Arc::clone(&task), 1, false, true);

    assert_eq!(cpu_id, 0);
    assert_eq!(task.placement().cpu, CpuId::new(0));
    assert!(scheduler.cpu_or_boot(0).runqueue().dequeue_queued(&task, 2));
}

#[ktest]
fn sleeping_task_enqueue_keeps_active_current_cpu() {
    let scheduler = two_cpu_scheduler();
    let task = make_task();
    task.set_state(TaskState::Sleeping);
    task.set_cpu_affinity(CpuMask::single_raw(0).union(CpuMask::single_raw(1)).bits());
    bind_to_cpu(&scheduler, &task, 1);

    let cpu_id = enqueue_task_on_scheduler(&scheduler, Arc::clone(&task), 1, false, true);

    assert_eq!(cpu_id, 1);
    assert_eq!(task.placement().cpu, CpuId::new(1));
    assert!(scheduler.cpu_or_boot(1).runqueue().dequeue_queued(&task, 2));
}

#[ktest]
fn balance_requeue_avoids_inactive_source_cpu() {
    let scheduler = two_cpu_scheduler();
    let task = make_task();
    task.set_state(TaskState::Runnable);
    task.set_cpu_affinity(CpuMask::single_raw(0).union(CpuMask::single_raw(1)).bits());
    bind_to_cpu(&scheduler, &task, 1);
    assert!(scheduler.deactivate_cpu(CpuId::new(1).expect("cpu1")));

    let cpu_id = requeue_balance_task_on(&scheduler, Arc::clone(&task), 1, 1);

    assert_eq!(cpu_id, 0);
    assert_eq!(task.placement().cpu, CpuId::new(0));
    assert!(scheduler.cpu_or_boot(0).runqueue().dequeue_queued(&task, 2));
}

#[ktest]
fn concurrent_wake_enqueues_task_once() {
    let scheduler = Arc::new(two_cpu_scheduler());
    let task = make_task();
    task.set_state(TaskState::Sleeping);
    task.set_cpu_affinity(CpuMask::single_raw(0).union(CpuMask::single_raw(1)).bits());
    bind_to_cpu(&scheduler, &task, 1);
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();

    for now_ns in [1, 2] {
        let worker_scheduler = Arc::clone(&scheduler);
        let worker_task = Arc::clone(&task);
        let worker_barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            worker_barrier.wait();
            enqueue_task_on_scheduler(&worker_scheduler, worker_task, now_ns, false, true)
        }));
    }
    barrier.wait();
    for worker in workers {
        let _ = worker.join().expect("wake worker");
    }

    let placement = task.placement();
    let mut queued = 0usize;
    for cpu_id in 0..=1 {
        if scheduler
            .cpu_or_boot(cpu_id)
            .runqueue()
            .dequeue_queued(&task, 3)
        {
            assert_eq!(placement.cpu, CpuId::new(cpu_id));
            queued += 1;
        }
    }
    assert_eq!(queued, 1);
}

fn two_cpu_scheduler() -> Scheduler {
    let scheduler = Scheduler::new();
    scheduler.install_topology(SchedTopology::with_cpu_domains());
    scheduler.register_cpu(CpuId::new(1).expect("cpu1"));
    scheduler
}

fn bind_to_cpu(scheduler: &Scheduler, task: &crate::Task, cpu_id: usize) {
    let cpu = CpuId::new(cpu_id).expect("cpu");
    let snapshot = scheduler.topology_snapshot();
    let domain = snapshot.topology.domain_for_cpu(cpu).expect("cpu domain");
    task.bind_placement(cpu, domain.id(), snapshot.generation);
}

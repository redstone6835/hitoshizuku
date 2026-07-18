//! 等待队列登记、唤醒和结束等待测试。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use ktest::ktest;

use super::test_thread_metadata::make_task;
use crate::{TaskState, WaitQueue};

#[ktest]
fn prepare_to_wait_registers_before_returning_sleeping_task() {
    let queue = WaitQueue::new();
    let task = make_task();

    let entry = queue.prepare_to_wait(&task, TaskState::Sleeping);

    assert_eq!(queue.len_hint(), 1);
    assert_eq!(task.state(), TaskState::Sleeping);
    queue.finish_wait(&entry);
    assert_eq!(task.state(), TaskState::Runnable);
    assert_eq!(queue.len_hint(), 0);
}

#[ktest]
fn wake_all_marks_entry_before_running_callback() {
    let callbacks = AtomicUsize::new(0);
    let queue = WaitQueue::new();
    let task = make_task();
    let entry = queue.prepare_to_wait(&task, TaskState::Sleeping);

    queue.wake_all_with(|_| {
        callbacks.fetch_add(1, Ordering::AcqRel);
    });
    queue.finish_wait(&entry);

    assert_eq!(callbacks.load(Ordering::Acquire), 1);
    assert_eq!(task.state(), TaskState::Runnable);
    assert_eq!(queue.len_hint(), 0);
}

#[ktest]
fn wake_one_wakes_only_one_wait_queue_entry() {
    let queue = WaitQueue::new();
    let first = make_task();
    let second = make_task();
    let first_entry = queue.prepare_to_wait(&first, TaskState::Sleeping);
    let second_entry = queue.prepare_to_wait(&second, TaskState::Sleeping);

    let woken = queue.wake_one_with(|_| {}).expect("one waiter");

    assert_eq!(queue.len_hint(), 1);
    assert!(Arc::ptr_eq(&woken, &first) || Arc::ptr_eq(&woken, &second));
    queue.finish_wait(&first_entry);
    queue.finish_wait(&second_entry);
    assert_eq!(first.state(), TaskState::Runnable);
    assert_eq!(second.state(), TaskState::Runnable);
}

#[ktest]
fn finish_wait_is_idempotent() {
    let queue = WaitQueue::new();
    let task = make_task();
    let entry = queue.prepare_to_wait(&task, TaskState::Sleeping);

    queue.finish_wait(&entry);
    queue.finish_wait(&entry);

    assert_eq!(queue.len_hint(), 0);
    assert_eq!(task.state(), TaskState::Runnable);
}

#[ktest]
fn repeated_wake_does_not_run_callback_twice() {
    let callbacks = AtomicUsize::new(0);
    let queue = WaitQueue::new();
    let task = make_task();
    let entry = queue.prepare_to_wait(&task, TaskState::Sleeping);

    queue.wake_all_with(|_| {
        callbacks.fetch_add(1, Ordering::AcqRel);
    });
    queue.wake_all_with(|_| {
        callbacks.fetch_add(1, Ordering::AcqRel);
    });
    queue.finish_wait(&entry);

    assert_eq!(callbacks.load(Ordering::Acquire), 1);
}

#[ktest]
fn wake_n_skips_stale_entries_and_wakes_requested_count() {
    let callbacks = AtomicUsize::new(0);
    let queue = WaitQueue::new();
    let stale = make_task();
    queue.enqueue(&stale);
    let first = make_task();
    let second = make_task();
    let third = make_task();
    let first_entry = queue.enqueue(&first);
    let second_entry = queue.enqueue(&second);
    let third_entry = queue.enqueue(&third);
    first.set_state(TaskState::Sleeping);
    second.set_state(TaskState::Sleeping);
    third.set_state(TaskState::Sleeping);
    drop(stale);

    let woken = queue.wake_n(2, |_| {
        callbacks.fetch_add(1, Ordering::AcqRel);
    });

    assert_eq!(woken, 2);
    assert_eq!(callbacks.load(Ordering::Acquire), 2);
    assert_eq!(queue.len_hint(), 1);
    queue.finish_wait(&first_entry);
    queue.finish_wait(&second_entry);
    queue.finish_wait(&third_entry);
}

#[ktest]
fn wake_one_keeps_registration_for_non_sleeping_task() {
    let queue = WaitQueue::new();
    let task = make_task();
    queue.enqueue(&task);

    assert!(queue.wake_one(|_| {}).is_none());
    assert_eq!(queue.len_hint(), 1);

    queue.remove(&task);
    assert_eq!(queue.len_hint(), 0);
}

//! Native 线程对象、显式栈/TLS 映射与协作终止。

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, Ordering};

use general::syscall::NativeCallOutcome;
use mm::VmFlags;
use native_abi::wire::{ThreadCreateRequest, ThreadInfo, ThreadResult};
use native_abi::{NativeHandle, ObjectInterface, Rights, status, wire};
use sched::{SchedParams, Task, TaskState, ThreadGroup};

use super::component::ComponentObject;
use super::dispatch::native_return;
use super::memory::{
    InternalMemoryMapping, MemoryObject, map_internal_rw, release_internal_mapping,
};
use super::{
    KernelNativeObject, NativeProcessState, copy_user_value, copy_user_value_out, task_vm,
};

pub(crate) const TASKEXT_NATIVE_THREAD: sched::TaskExtKey = 0x0004_0000;

struct ThreadRuntime {
    stack: InternalMemoryMapping,
    tls: Option<InternalMemoryMapping>,
    tls_registered: bool,
}

struct ThreadShared {
    task: sched::sync::Spinlock<Option<Arc<Task>>>,
    result: sched::sync::Spinlock<Option<ThreadResult>>,
    runtime: sched::sync::Spinlock<Option<ThreadRuntime>>,
    state: Weak<NativeProcessState>,
    identity: u64,
    tls_base: u64,
    join_in_progress: AtomicBool,
    component: Option<Weak<ComponentObject>>,
}

/// Thread handle 引用的稳定对象。任务退出后仍保留结果，但不再保活 Task。
pub(crate) struct ThreadObject {
    shared: Arc<ThreadShared>,
    owner: Weak<ThreadGroup>,
}

impl ThreadObject {
    fn task_for(&self, caller: &Task) -> Result<Option<Arc<Task>>, u32> {
        let Some(owner) = self.owner.upgrade() else {
            return Err(status::THREAD_INVALID);
        };
        if !Arc::ptr_eq(&owner, &caller.thread_group()) {
            return Err(status::THREAD_INVALID);
        }
        Ok(self.shared.task.lock().as_ref().map(Arc::clone))
    }

    fn result(&self) -> Option<ThreadResult> {
        *self.shared.result.lock()
    }
}

pub(super) fn thread_create(
    task: &Arc<Task>,
    state: &Arc<NativeProcessState>,
    object: &KernelNativeObject,
    user: u64,
    caller_component: u64,
) -> NativeCallOutcome {
    if !matches!(object, KernelNativeObject::SelfProcess) {
        return native_return(status::HANDLE_WRONG_INTERFACE, 0, 0);
    }
    let request = match copy_user_value::<ThreadCreateRequest>(task, user) {
        Ok(request) => request,
        Err(error) => return native_return(error, 0, 0),
    };
    if request.flags != 0
        || request.entry == 0
        || request.entry & 3 != 0
        || request.stack_memory == 0
        || request.stack_offset % native_abi::PAGE_SIZE != 0
        || request.stack_size == 0
        || request.stack_size % native_abi::PAGE_SIZE != 0
        || (request.tls_memory == 0 && request.tls_offset != 0)
        || (request.tls_memory != 0 && request.tls_offset % native_abi::PAGE_SIZE != 0)
    {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }

    let vm = match task_vm(task) {
        Ok(vm) => vm,
        Err(error) => return native_return(error, 0, 0),
    };
    let Ok(entry) = usize::try_from(request.entry) else {
        return native_return(status::THREAD_INVALID, 0, 0);
    };
    if vm
        .contains_user_range_with_flags(
            entry..entry.saturating_add(1),
            VmFlags::USER | VmFlags::EXEC,
        )
        .is_err()
    {
        return native_return(status::THREAD_INVALID, 0, 0);
    }

    let (stack_object, tls_object) = match lookup_thread_memory(state, &request) {
        Ok(objects) => objects,
        Err(error) => return native_return(error, 0, 0),
    };
    let _component_prepare = match state.components.begin_thread_prepare() {
        Ok(prepare) => prepare,
        Err(error) => return native_return(error, 0, 0),
    };
    let component = match state.components.resolve_component_marker(caller_component) {
        Ok(component) => component,
        Err(error) => return native_return(error, 0, 0),
    };
    if request
        .stack_offset
        .checked_add(request.stack_size)
        .is_none_or(|end| end > stack_object.size())
    {
        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
    }
    let tls_length = match &tls_object {
        Some(object) => match object.size().checked_sub(request.tls_offset) {
            Some(length) if length != 0 => length,
            _ => return native_return(status::MEMORY_INVALID_RANGE, 0, 0),
        },
        None => 0,
    };

    let stack = match map_internal_rw(
        state,
        &vm,
        &stack_object,
        request.stack_offset,
        request.stack_size,
    ) {
        Ok(mapping) => mapping,
        Err(error) => return native_return(error, 0, 0),
    };
    let tls = match tls_object {
        Some(object) => {
            match map_internal_rw(state, &vm, &object, request.tls_offset, tls_length) {
                Ok(mapping) => Some(mapping),
                Err(error) => {
                    release_internal_mapping(state, stack);
                    return native_return(error, 0, 0);
                }
            }
        }
        None => None,
    };
    let stack_top = stack.range.end;
    let tls_base = tls.as_ref().map_or(0, |mapping| mapping.range.start);
    let argument = match usize::try_from(request.argument) {
        Ok(argument) => argument,
        Err(_) => {
            if let Some(tls) = tls {
                release_internal_mapping(state, tls);
            }
            release_internal_mapping(state, stack);
            return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
        }
    };

    let child = match sched::spawn_native_thread(task, SchedParams::default_fair()) {
        Ok(child) => child,
        Err(_) => {
            if let Some(tls) = tls {
                release_internal_mapping(state, tls);
            }
            release_internal_mapping(state, stack);
            return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
        }
    };
    if crate::sched::prepare_native_thread(
        &child,
        Arc::clone(&vm),
        entry,
        stack_top,
        argument,
        tls_base,
    )
    .is_err()
    {
        sched::abort_new_task(&child);
        if let Some(tls) = tls {
            release_internal_mapping(state, tls);
        }
        release_internal_mapping(state, stack);
        return native_return(status::THREAD_INVALID, 0, 0);
    }

    let tls_registered = match state
        .components
        .install_thread_tls(tls.as_ref().map(|mapping| mapping.range.clone()))
    {
        Ok(registered) => registered,
        Err(error) => {
            sched::abort_new_task(&child);
            if let Some(tls) = tls {
                release_internal_mapping(state, tls);
            }
            release_internal_mapping(state, stack);
            return native_return(error, 0, 0);
        }
    };

    let identity = child.pid_root_cached().unwrap_or(0) as u64;
    let shared = Arc::new(ThreadShared {
        task: sched::sync::Spinlock::new(Some(Arc::clone(&child))),
        result: sched::sync::Spinlock::new(None),
        runtime: sched::sync::Spinlock::new(Some(ThreadRuntime {
            stack,
            tls,
            tls_registered,
        })),
        state: Arc::downgrade(state),
        identity,
        tls_base: tls_base as u64,
        join_in_progress: AtomicBool::new(false),
        component: component.as_ref().map(Arc::downgrade),
    });
    if let Some(component) = &component {
        if let Err(error) = component.register_thread(&child) {
            abort_prepared_thread(state, &child);
            return native_return(error, 0, 0);
        }
    }
    child.ext_install(TASKEXT_NATIVE_THREAD, shared.clone());
    let thread = Arc::new(ThreadObject {
        shared,
        owner: Arc::downgrade(&task.thread_group()),
    });
    let handle = match state.handles.lock().insert(
        KernelNativeObject::Thread(thread),
        ObjectInterface::Thread,
        Rights::INSPECT | Rights::WAIT | Rights::TERMINATE | Rights::DUPLICATE,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            abort_prepared_thread(state, &child);
            return native_return(error, 0, 0);
        }
    };
    if sched::activate_task(&child).is_err() {
        let _ = state.handles.lock().close(handle);
        abort_prepared_thread(state, &child);
        return native_return(status::THREAD_INVALID, 0, 0);
    }
    native_return(status::OK, handle.raw(), 0)
}

pub(super) fn thread_join(
    caller: &Arc<Task>,
    thread: &ThreadObject,
    user: u64,
    deadline_ns: u64,
) -> NativeCallOutcome {
    let target = match thread.task_for(caller) {
        Ok(Some(target)) => target,
        Ok(None) => {
            let Some(result) = thread.result() else {
                return native_return(status::THREAD_INVALID, 0, 0);
            };
            return write_result(caller, user, &result);
        }
        Err(error) => return native_return(error, 0, 0),
    };
    if Arc::ptr_eq(caller, &target) {
        return native_return(status::THREAD_SELF, 0, 0);
    }
    if thread
        .shared
        .join_in_progress
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return native_return(status::THREAD_WOULD_BLOCK, 0, 0);
    }
    let outcome = wait_for_thread(caller, &target, deadline_ns);
    thread
        .shared
        .join_in_progress
        .store(false, Ordering::Release);
    match outcome {
        Ok(()) => {
            let result = thread
                .result()
                .unwrap_or_else(|| thread_result_from_task(&target));
            write_result(caller, user, &result)
        }
        Err(ThreadWaitError::ExternalControl) => NativeCallOutcome::RetryExternalControl,
        Err(ThreadWaitError::Timeout) => native_return(status::THREAD_TIMEOUT, 0, 0),
    }
}

pub(super) fn thread_terminate(
    caller: &Arc<Task>,
    thread: &ThreadObject,
    code: u64,
) -> NativeCallOutcome {
    if code > u32::MAX as u64 {
        return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
    }
    let target = match thread.task_for(caller) {
        Ok(Some(target)) => target,
        Ok(None) => return native_return(status::THREAD_ALREADY_EXITED, 0, 0),
        Err(error) => return native_return(error, 0, 0),
    };
    if matches!(target.state(), TaskState::Zombie | TaskState::Dead) {
        return native_return(status::THREAD_ALREADY_EXITED, 0, 0);
    }
    let code = code as u32 as i32;
    if Arc::ptr_eq(caller, &target) {
        return NativeCallOutcome::ExitThread(code);
    }
    let _ = sched::native_thread_exit_wakeup(&target, code);
    native_return(status::OK, 0, 0)
}

pub(super) fn thread_query(
    caller: &Arc<Task>,
    thread: &ThreadObject,
    user: u64,
) -> NativeCallOutcome {
    let info = match thread.task_for(caller) {
        Ok(Some(task)) => thread_info_from_task(thread, &task),
        Ok(None) => {
            let Some(result) = thread.result() else {
                return native_return(status::THREAD_INVALID, 0, 0);
            };
            ThreadInfo {
                state: result.state,
                flags: result.flags,
                identity: thread.shared.identity,
                cpu_time_ns: 0,
                exit_code: result.exit_code,
                fault_kind: result.fault_kind,
                tls_base: thread.shared.tls_base,
                reserved: 0,
            }
        }
        Err(error) => return native_return(error, 0, 0),
    };
    match copy_user_value_out(caller, user, &info) {
        Ok(()) => native_return(status::OK, 0, 0),
        Err(error) => native_return(error, 0, 0),
    }
}

/// 任务退出扩展清理阶段固化结果、撤销栈/TLS 映射并断开 Task 强引用。
pub(crate) fn record_task_exit(task: &Arc<Task>) {
    let Some(shared) = task
        .ext_lookup(TASKEXT_NATIVE_THREAD)
        .and_then(|payload| payload.downcast::<ThreadShared>().ok())
    else {
        return;
    };
    if shared.result.lock().is_none() {
        *shared.result.lock() = Some(thread_result_from_task(task));
    }
    if let Some(runtime) = shared.runtime.lock().take()
        && let Some(state) = shared.state.upgrade()
    {
        if let Some(tls) = runtime.tls {
            if runtime.tls_registered {
                state.components.retire_thread_tls(&state, tls);
            } else {
                release_internal_mapping(&state, tls);
            }
        }
        release_internal_mapping(&state, runtime.stack);
    }
    shared.task.lock().take();
    if let Some(component) = shared.component.as_ref().and_then(Weak::upgrade) {
        component.unregister_thread(task);
        component.wake_drain_waiters();
    }
}

pub(crate) fn component_for_task(task: &Arc<Task>) -> Option<Arc<ComponentObject>> {
    task.ext_lookup(TASKEXT_NATIVE_THREAD)
        .and_then(|payload| payload.downcast::<ThreadShared>().ok())
        .and_then(|shared| shared.component.as_ref().and_then(Weak::upgrade))
}

fn lookup_thread_memory(
    state: &NativeProcessState,
    request: &ThreadCreateRequest,
) -> Result<(Arc<MemoryObject>, Option<Arc<MemoryObject>>), u32> {
    let handles = state.handles.lock();
    let stack = handles.lookup(
        NativeHandle::from_raw(request.stack_memory),
        Some(ObjectInterface::MemoryObject),
        Rights::MAP | Rights::READ | Rights::WRITE,
    )?;
    let KernelNativeObject::MemoryObject(stack) = stack.object else {
        return Err(status::HANDLE_WRONG_INTERFACE);
    };
    let tls = if request.tls_memory == 0 {
        None
    } else {
        let entry = handles.lookup(
            NativeHandle::from_raw(request.tls_memory),
            Some(ObjectInterface::MemoryObject),
            Rights::MAP | Rights::READ | Rights::WRITE,
        )?;
        let KernelNativeObject::MemoryObject(object) = entry.object else {
            return Err(status::HANDLE_WRONG_INTERFACE);
        };
        Some(Arc::clone(object))
    };
    Ok((Arc::clone(stack), tls))
}

fn abort_prepared_thread(state: &NativeProcessState, child: &Arc<Task>) {
    let shared = child
        .ext_remove(TASKEXT_NATIVE_THREAD)
        .and_then(|payload| payload.downcast::<ThreadShared>().ok());
    sched::abort_new_task(child);
    if let Some(shared) = shared {
        if let Some(component) = shared.component.as_ref().and_then(Weak::upgrade) {
            component.unregister_thread(child);
        }
        if let Some(runtime) = shared.runtime.lock().take() {
            if runtime.tls_registered
                && let Some(tls) = runtime.tls.as_ref()
            {
                state.components.unregister_thread_tls(&tls.range);
            }
            if let Some(tls) = runtime.tls {
                release_internal_mapping(state, tls);
            }
            release_internal_mapping(state, runtime.stack);
        }
        shared.task.lock().take();
    }
}

fn thread_result_from_task(task: &Task) -> ThreadResult {
    let group = task.thread_group();
    let fault = group.native_fault();
    let terminal = matches!(task.state(), TaskState::Zombie | TaskState::Dead);
    ThreadResult {
        state: if terminal && fault.is_some() {
            wire::THREAD_STATE_FAULTED
        } else if terminal {
            wire::THREAD_STATE_EXITED
        } else {
            wire::THREAD_STATE_RUNNING
        },
        flags: 0,
        exit_code: task.exit_code().map_or(0, |code| code.0 as u32),
        fault_kind: fault.map_or(0, |fault| fault.kind),
        detail0: fault.map_or(0, |fault| fault.exception_code),
        detail1: fault.map_or(0, |fault| fault.address),
    }
}

fn thread_info_from_task(thread: &ThreadObject, task: &Task) -> ThreadInfo {
    let result = thread_result_from_task(task);
    ThreadInfo {
        state: result.state,
        flags: result.flags,
        identity: thread.shared.identity,
        cpu_time_ns: task.cpu_runtime_ns(sched::now_ns_public()),
        exit_code: result.exit_code,
        fault_kind: result.fault_kind,
        tls_base: thread.shared.tls_base,
        reserved: 0,
    }
}

fn write_result(task: &Task, user: u64, result: &ThreadResult) -> NativeCallOutcome {
    match copy_user_value_out(task, user, result) {
        Ok(()) => native_return(status::OK, 0, 0),
        Err(error) => native_return(error, 0, 0),
    }
}

enum ThreadWaitError {
    Timeout,
    ExternalControl,
}

fn wait_for_thread(
    caller: &Arc<Task>,
    target: &Arc<Task>,
    deadline_ns: u64,
) -> Result<(), ThreadWaitError> {
    while !matches!(target.state(), TaskState::Zombie | TaskState::Dead) {
        if super::operations::has_native_external_control(caller) {
            return Err(ThreadWaitError::ExternalControl);
        }
        if deadline_ns != 0 && sched::now_ns_public() >= deadline_ns {
            return Err(ThreadWaitError::Timeout);
        }
        let entry = target
            .exit_waiters
            .prepare_to_wait(caller, TaskState::Sleeping);
        let deadline_armed =
            deadline_ns != 0 && sched::register_sleep_deadline(caller, deadline_ns);
        if deadline_ns != 0 && !deadline_armed {
            target.exit_waiters.finish_wait(&entry);
            super::operations::restore_native_task_after_wait(caller);
            return Err(ThreadWaitError::Timeout);
        }
        if matches!(target.state(), TaskState::Zombie | TaskState::Dead) {
            if deadline_armed {
                sched::cancel_sleep_deadline(caller);
            }
            target.exit_waiters.finish_wait(&entry);
            super::operations::restore_native_task_after_wait(caller);
            break;
        }
        sched::schedule_once(sched::now_ns_public());
        if deadline_armed {
            sched::cancel_sleep_deadline(caller);
        }
        target.exit_waiters.finish_wait(&entry);
        super::operations::restore_native_task_after_wait(caller);
    }
    Ok(())
}

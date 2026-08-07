//! MyGO Native operation 的内核对象执行路径。

use alloc::sync::Arc;

use general::mm::VmSpace;
use general::syscall::{NativeCallFrame, NativeCallOutcome};
use general::vfs::error::VfsError;
use general::vfs::file::{File, PollEvents};
use native_abi::{NativeHandle, ObjectInterface, OperationId, Rights};

use super::dispatch::native_return;
use super::{KernelNativeObject, NativeProcessState};

pub(super) struct PinnedNativeHandle {
    pub(super) object: KernelNativeObject,
    pub(super) interface: ObjectInterface,
    pub(super) rights: Rights,
}

pub(super) fn execute_native_operation(
    task: &Arc<sched::Task>,
    state: &NativeProcessState,
    operation: OperationId,
    handle: NativeHandle,
    pinned: PinnedNativeHandle,
    call: NativeCallFrame,
) -> NativeCallOutcome {
    match operation {
        OperationId::ProcessExit => {
            if !matches!(pinned.object, KernelNativeObject::SelfProcess)
                || call.args[0] > u32::MAX as u64
            {
                return native_return(native_abi::status::CORE_INVALID_ARGUMENT, 0, 0);
            }
            NativeCallOutcome::ExitGroup(call.args[0] as u32 as i32)
        }
        OperationId::HandleClose => {
            let result = state.handles.lock().close(handle);
            match result {
                Ok(object) => {
                    drop(object);
                    native_return(native_abi::status::OK, 0, 0)
                }
                Err(status) => native_return(status, 0, 0),
            }
        }
        OperationId::HandleDuplicate => insert_pinned_handle(state, pinned),
        OperationId::HandleRestrict => {
            let requested = Rights::from_bits(call.args[0]);
            if !requested.is_subset_of(pinned.rights) {
                return native_return(native_abi::status::SECURITY_RIGHTS_DENIED, 0, 0);
            }
            insert_native_handle(state, pinned.object, pinned.interface, requested)
        }
        OperationId::StreamWrite => {
            let KernelNativeObject::Stream(file) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            stream_write(task, &file, call.args[0], call.args[1])
        }
        OperationId::ClockRead => {
            if !matches!(pinned.object, KernelNativeObject::MonotonicClock) {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            }
            native_return(native_abi::status::OK, hal::time::monotonic_ns(), 0)
        }
        OperationId::StreamRead | OperationId::VmMapAnon | OperationId::VmUnmap => {
            native_return(native_abi::status::ABI_UNSUPPORTED_OPERATION, 0, 0)
        }
    }
}

fn insert_pinned_handle(
    state: &NativeProcessState,
    pinned: PinnedNativeHandle,
) -> NativeCallOutcome {
    insert_native_handle(state, pinned.object, pinned.interface, pinned.rights)
}

fn insert_native_handle(
    state: &NativeProcessState,
    object: KernelNativeObject,
    interface: ObjectInterface,
    rights: Rights,
) -> NativeCallOutcome {
    match state.handles.lock().insert(object, interface, rights) {
        Ok(handle) => native_return(native_abi::status::OK, handle.raw(), 0),
        Err(status) => native_return(status, 0, 0),
    }
}

fn stream_write(task: &Arc<sched::Task>, file: &File, user: u64, len: u64) -> NativeCallOutcome {
    let Ok(user) = usize::try_from(user) else {
        return native_return(native_abi::status::IO_FAULT, 0, 0);
    };
    let Ok(len) = usize::try_from(len) else {
        return native_return(native_abi::status::CORE_OUT_OF_RANGE, 0, 0);
    };
    if len == 0 {
        return native_return(native_abi::status::OK, 0, 0);
    }
    if user.checked_add(len).is_none() {
        return native_return(native_abi::status::IO_FAULT, 0, 0);
    }
    let Some(vm) = task
        .ext_lookup(sched::TASKEXT_VM_SPACE)
        .and_then(|payload| payload.downcast::<VmSpace>().ok())
    else {
        return native_return(native_abi::status::IO_FAULT, 0, 0);
    };

    let mut written = 0usize;
    while written < len {
        let address = match user.checked_add(written) {
            Some(address) => address,
            None => return stream_write_fault(written),
        };
        let result =
            unsafe { vm.with_user_read_slice(address, len - written, |buffer| file.write(buffer)) };
        let count = match result {
            Ok(Ok(count)) => count,
            Ok(Err(VfsError::WouldBlock)) if written != 0 => {
                return native_return(native_abi::status::OK, written as u64, 0);
            }
            Ok(Err(VfsError::WouldBlock)) if file.flags().nonblock => {
                return native_return(native_abi::status::IO_WOULD_BLOCK, 0, 0);
            }
            Ok(Err(VfsError::WouldBlock)) => {
                match wait_for_stream_writable(task, file) {
                    Ok(()) => {}
                    Err(StreamWaitError::ExternalControl) => {
                        return NativeCallOutcome::RetryExternalControl;
                    }
                    Err(StreamWaitError::Unavailable) => {
                        return native_return(native_abi::status::IO_WOULD_BLOCK, 0, 0);
                    }
                }
                continue;
            }
            Ok(Err(error)) => return map_stream_write_error(error, written),
            Err(_) => return stream_write_fault(written),
        };
        if count == 0 {
            return map_stream_write_error(VfsError::Io, written);
        }
        written = match written.checked_add(count) {
            Some(written) => written,
            None => return native_return(native_abi::status::CORE_OUT_OF_RANGE, 0, 0),
        };
    }
    native_return(native_abi::status::OK, written as u64, 0)
}

fn stream_write_fault(written: usize) -> NativeCallOutcome {
    if written == 0 {
        native_return(native_abi::status::IO_FAULT, 0, 0)
    } else {
        native_return(native_abi::status::OK, written as u64, 0)
    }
}

pub(super) fn map_stream_write_error(error: VfsError, written: usize) -> NativeCallOutcome {
    if written != 0 {
        return native_return(native_abi::status::OK, written as u64, 0);
    }
    let status = match error {
        VfsError::WouldBlock => native_abi::status::IO_WOULD_BLOCK,
        VfsError::BrokenPipe | VfsError::ConnectionReset => native_abi::status::IO_CLOSED,
        _ => native_abi::status::IO_ERROR,
    };
    native_return(status, 0, 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamWaitError {
    ExternalControl,
    Unavailable,
}

fn wait_for_stream_writable(task: &Arc<sched::Task>, file: &File) -> Result<(), StreamWaitError> {
    const IO_RECHECK_NS: u64 = 10_000_000;

    if !file.poll(PollEvents::POLLOUT).is_empty() {
        return Ok(());
    }
    if has_native_external_control(task) {
        return Err(StreamWaitError::ExternalControl);
    }
    let sleeping = task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping)
        || task.cas_state(sched::TaskState::Runnable, sched::TaskState::Sleeping);
    if !sleeping {
        return if has_native_external_control(task) {
            Err(StreamWaitError::ExternalControl)
        } else {
            Err(StreamWaitError::Unavailable)
        };
    }
    let registered = file.poll_add_waiter(task, PollEvents::POLLOUT);
    let deadline_armed =
        sched::register_sleep_deadline(task, sched::now_ns_public().saturating_add(IO_RECHECK_NS));
    if !file.poll(PollEvents::POLLOUT).is_empty() {
        finish_stream_wait(task, file, registered, deadline_armed);
        return Ok(());
    }
    if has_native_external_control(task) {
        finish_stream_wait(task, file, registered, deadline_armed);
        return Err(StreamWaitError::ExternalControl);
    }

    if registered || deadline_armed {
        sched::schedule_once(sched::now_ns_public());
        finish_stream_wait(task, file, registered, deadline_armed);
    } else {
        restore_native_task_after_wait(task);
        sched::operation::sched_yield().map_err(|_| StreamWaitError::Unavailable)?;
    }

    if has_native_external_control(task) {
        Err(StreamWaitError::ExternalControl)
    } else {
        Ok(())
    }
}

fn finish_stream_wait(
    task: &Arc<sched::Task>,
    file: &File,
    registered: bool,
    deadline_armed: bool,
) {
    if registered {
        file.poll_remove_waiter(task);
    }
    if deadline_armed {
        sched::cancel_sleep_deadline(task);
    }
    restore_native_task_after_wait(task);
}

fn has_native_external_control(task: &Arc<sched::Task>) -> bool {
    task.group_exit_pending()
        || task.signal.has_any_pending()
        || task.shared_signal_pending_bits_quick() != 0
        || matches!(
            task.state(),
            sched::TaskState::Stopped
                | sched::TaskState::Continued
                | sched::TaskState::Zombie
                | sched::TaskState::Dead
        )
}

fn restore_native_task_after_wait(task: &Arc<sched::Task>) {
    if !task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running) {
        let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Running);
    }
}

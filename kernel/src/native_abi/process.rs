//! Native 进程对象、派生事务与一次性回收语义。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU8, Ordering};

use general::mm::{copy_from_user, copy_to_user};
use general::syscall::NativeCallOutcome;
use native_abi::wire::{
    HandleTransfer, ProcessArrayRef, ProcessResult, ProcessStringRef, SpawnRequest,
};
use native_abi::{NativeHandle, ObjectInterface, Rights, status, wire};
use sched::{SchedParams, Task, TaskState, ThreadGroup, UserContextRef};

use super::dispatch::native_return;
use super::operations::insert_native_handle;
use super::{KernelNativeObject, NativeProcessState, PreparedNativeCapability};

const WAIT_AVAILABLE: u8 = 0;
const WAIT_RESERVED: u8 = 1;
const WAIT_REAPED: u8 = 2;
const MAX_PROCESS_STRINGS: u32 = 4096;
const MAX_PROCESS_STRING_BYTES: usize = 128 * 1024;

/// 指向稳定线程组身份的 Native Process capability。
pub(crate) struct ProcessObject {
    group: Arc<ThreadGroup>,
    owner: Weak<ThreadGroup>,
    wait_state: AtomicU8,
}

impl ProcessObject {
    pub(super) fn new(group: Arc<ThreadGroup>, owner: &Arc<ThreadGroup>) -> Arc<Self> {
        Arc::new(Self {
            group,
            owner: Arc::downgrade(owner),
            wait_state: AtomicU8::new(WAIT_AVAILABLE),
        })
    }

    pub(crate) fn group(&self) -> &Arc<ThreadGroup> {
        &self.group
    }

    pub(crate) fn result(&self) -> ProcessResult {
        process_result(
            &self.group,
            self.wait_state.load(Ordering::Acquire) == WAIT_REAPED,
        )
    }
}

pub(super) fn image_create(
    state: &NativeProcessState,
    user: u64,
    length: u64,
) -> NativeCallOutcome {
    let image = match super::ExecutableImage::copy_from_user(user, length) {
        Ok(image) => image,
        Err(error) => return native_return(error, 0, 0),
    };
    insert_native_handle(
        state,
        KernelNativeObject::ExecutableImage(image),
        ObjectInterface::ExecutableImage,
        Rights::EXECUTE | Rights::DUPLICATE,
    )
}

pub(super) fn process_spawn(
    task: &Arc<Task>,
    state: &NativeProcessState,
    request_user: u64,
    request_size: u64,
) -> NativeCallOutcome {
    let request = match read_spawn_request(request_user, request_size) {
        Ok(request) => request,
        Err(error) => return native_return(error, 0, 0),
    };
    let (image, argv, env, transferred) = match prepare_spawn_inputs(state, &request) {
        Ok(inputs) => inputs,
        Err(error) => return native_return(error, 0, 0),
    };
    if let Err(error) = validate_move_transfers(state, &transferred) {
        return native_return(error, 0, 0);
    }

    let loaded = match crate::soyo::load_executable_image(&image) {
        Ok(loaded) => loaded,
        Err(error) => return native_return(map_image_prepare_error(error), 0, 0),
    };
    let prepared = match crate::soyo::prepare_soyo_runtime_with_capabilities(
        loaded,
        &argv,
        &env,
        None,
        &transferred,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return native_return(map_image_prepare_error(error), 0, 0),
    };

    let parent_group = task.thread_group();
    let parent_exec = parent_group.lock_exec();
    if parent_exec.phase() != native_abi::ExecPhase::Running
        || parent_group.user_abi_kind() != native_abi::UserAbiKind::MygoNative
        || parent_group.group_exit_status().is_some()
    {
        return native_return(status::PROCESS_INVALID_STATE, 0, 0);
    }
    let child = match sched::spawn_native_child(task, SchedParams::default_fair()) {
        Ok(child) => child,
        Err(_) => return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0),
    };
    if crate::sched::prepare_native_child(&child, prepared).is_err() {
        sched::abort_new_task(&child);
        return native_return(status::PROCESS_INVALID_STATE, 0, 0);
    }

    let process = ProcessObject::new(child.thread_group(), &task.thread_group());
    let process_handle = match state.handles.lock().insert(
        KernelNativeObject::Process(process),
        ObjectInterface::Process,
        Rights::INSPECT | Rights::WAIT | Rights::TERMINATE | Rights::OBSERVE | Rights::DUPLICATE,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            sched::abort_new_task(&child);
            return native_return(error, 0, 0);
        }
    };
    if let Err(error) = commit_move_transfers(state, &transferred) {
        let _ = state.handles.lock().close(process_handle);
        sched::abort_new_task(&child);
        return native_return(error, 0, 0);
    }
    if sched::activate_task(&child).is_err() {
        // move transfer 已经提交，不能伪装成可回滚失败。保留 Process handle，
        // 把未启动 child 发布成可 query/wait 的终止进程。
        let _ = child.thread_group().request_group_exit(127);
        sched::exit_task(&child, sched::ExitCode(127));
    }
    drop(parent_exec);
    drop(parent_group);
    native_return(status::OK, process_handle.raw(), 0)
}

fn map_image_prepare_error(error: errno::Errno) -> u32 {
    match error {
        errno::Errno::ENOMEM => status::CORE_RESOURCE_EXHAUSTED,
        errno::Errno::ENOEXEC | errno::Errno::E2BIG => status::IMAGE_INVALID,
        _ => status::PROCESS_INVALID_STATE,
    }
}

pub(super) fn process_replace(
    task: &Arc<Task>,
    state: &NativeProcessState,
    request_user: u64,
    request_size: u64,
    user_context: UserContextRef,
) -> NativeCallOutcome {
    let request = match read_spawn_request(request_user, request_size) {
        Ok(request) => request,
        Err(error) => return native_return(error, 0, 0),
    };
    let (image, argv, env, transferred) = match prepare_spawn_inputs(state, &request) {
        Ok(inputs) => inputs,
        Err(error) => return native_return(error, 0, 0),
    };
    if let Err(error) = validate_move_transfers(state, &transferred) {
        return native_return(error, 0, 0);
    }
    let loaded = match crate::soyo::load_executable_image(&image) {
        Ok(loaded) => loaded,
        Err(_) => return native_return(status::IMAGE_INVALID, 0, 0),
    };
    let prepared = match crate::soyo::prepare_soyo_runtime_with_capabilities(
        loaded,
        &argv,
        &env,
        None,
        &transferred,
    ) {
        Ok(prepared) => prepared,
        Err(_) => return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0),
    };
    match crate::exec::commit_native_replace(task, prepared, user_context) {
        Ok(()) => {
            // Native -> Native replace 已经完成 frame/VM 提交；此时 move source
            // 不再有可失败路径，代际仍由同一 handle 表原子校验。
            if let Err(error) = commit_move_transfers(state, &transferred) {
                log::error!("[native][replace] committed move transfer lost: {error:#x}");
                return NativeCallOutcome::ExitGroup(127);
            }
            NativeCallOutcome::FrameFinalized
        }
        Err(_) => native_return(status::PROCESS_INVALID_STATE, 0, 0),
    }
}

pub(super) fn process_query(process: &ProcessObject, user: u64) -> NativeCallOutcome {
    write_query_result(user, &process.result())
}

pub(super) fn process_query_self(task: &Arc<Task>, user: u64) -> NativeCallOutcome {
    write_query_result(user, &process_result(&task.thread_group(), false))
}

fn write_query_result(user: u64, result: &ProcessResult) -> NativeCallOutcome {
    match write_process_result(user, result) {
        Ok(()) => native_return(status::OK, 0, 0),
        Err(error) => native_return(error, 0, 0),
    }
}

pub(super) fn process_wait(
    task: &Arc<Task>,
    process: &ProcessObject,
    user: u64,
    deadline_ns: u64,
) -> NativeCallOutcome {
    let owner = match process.owner.upgrade() {
        Some(owner) if Arc::ptr_eq(&owner, &task.thread_group()) => owner,
        _ => return native_return(status::PROCESS_NOT_CHILD, 0, 0),
    };
    if process.wait_state.load(Ordering::Acquire) == WAIT_REAPED {
        return native_return(status::PROCESS_ALREADY_REAPED, 0, 0);
    }
    if !process.group.is_terminated() && deadline_ns != 0 && sched::now_ns_public() >= deadline_ns {
        return native_return(status::PROCESS_WOULD_BLOCK, 0, 0);
    }
    if process
        .wait_state
        .compare_exchange(
            WAIT_AVAILABLE,
            WAIT_RESERVED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return native_return(status::PROCESS_WAIT_IN_PROGRESS, 0, 0);
    }

    let outcome = wait_until_terminated(task, &process.group, deadline_ns);
    if let Err(error) = outcome {
        process.wait_state.store(WAIT_AVAILABLE, Ordering::Release);
        return match error {
            WaitFailure::ExternalControl => NativeCallOutcome::RetryExternalControl,
            WaitFailure::Timeout => native_return(status::PROCESS_WOULD_BLOCK, 0, 0),
        };
    }

    let target = Arc::clone(&process.group);
    let result = process.result();
    if let Err(error) = write_process_result(user, &result) {
        process.wait_state.store(WAIT_AVAILABLE, Ordering::Release);
        return native_return(error, 0, 0);
    }
    if sched::reap_native_child(&owner, |child| Arc::ptr_eq(&child.thread_group(), &target))
        .is_none()
    {
        process.wait_state.store(WAIT_AVAILABLE, Ordering::Release);
        return native_return(status::PROCESS_NOT_CHILD, 0, 0);
    }
    process.wait_state.store(WAIT_REAPED, Ordering::Release);
    native_return(status::OK, 0, 0)
}

pub(super) fn process_terminate(process: &ProcessObject, code: u64) -> NativeCallOutcome {
    process_terminate_group(&process.group, code)
}

pub(super) fn process_terminate_self(task: &Arc<Task>, code: u64) -> NativeCallOutcome {
    process_terminate_group(&task.thread_group(), code)
}

fn process_terminate_group(group: &ThreadGroup, code: u64) -> NativeCallOutcome {
    if code > u32::MAX as u64 {
        return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
    }
    if group.is_terminated() {
        return native_return(status::OK, 0, 0);
    }
    let _ = group.request_group_exit(code as u32 as i32);
    for member in group.snapshot() {
        if !matches!(member.state(), TaskState::Zombie | TaskState::Dead) {
            sched::group_exit_wakeup(&member);
        }
    }
    native_return(status::OK, 0, 0)
}

pub(super) fn process_result(group: &ThreadGroup, reaped: bool) -> ProcessResult {
    let status = group.group_exit_status();
    let fault = group.native_fault();
    let state = if reaped {
        wire::PROCESS_STATE_REAPED
    } else if fault.is_some() {
        wire::PROCESS_STATE_FAULTED
    } else if group.is_terminated() {
        wire::PROCESS_STATE_EXITED
    } else if status.is_some() {
        wire::PROCESS_STATE_TERMINATING
    } else {
        wire::PROCESS_STATE_RUNNING
    };
    ProcessResult {
        state,
        exit_code: status.map_or(0, |status| status.exit_code() as u32),
        fault_kind: fault.map_or(0, |fault| fault.kind),
        detail0: fault.map_or(0, |fault| fault.exception_code),
        detail1: fault.map_or(0, |fault| fault.address),
        ..ProcessResult::default()
    }
}

type SpawnInputs = (
    Arc<super::ExecutableImage>,
    Vec<Vec<u8>>,
    Vec<Vec<u8>>,
    Vec<PreparedNativeCapability>,
);

fn prepare_spawn_inputs(
    state: &NativeProcessState,
    request: &SpawnRequest,
) -> Result<SpawnInputs, u32> {
    if request.resource_policy != 0 {
        return Err(status::CORE_INVALID_ARGUMENT);
    }
    let image = {
        let handles = state.handles.lock();
        let entry = handles.lookup(
            NativeHandle::from_raw(request.image),
            Some(ObjectInterface::ExecutableImage),
            Rights::EXECUTE,
        )?;
        let KernelNativeObject::ExecutableImage(image) = entry.object else {
            return Err(status::HANDLE_WRONG_INTERFACE);
        };
        Arc::clone(image)
    };
    let argv = read_string_array(request.argv)?;
    let env = read_string_array(request.env)?;
    let transferred = read_transfers(state, request.transfers)?;
    if transferred.iter().any(|transfer| {
        !image
            .metadata
            .capabilities
            .iter()
            .any(|capability| capability.requirement_id == transfer.requirement_id as u32)
    }) {
        // 未在目标映像声明的 transfer 不会进入 child capability table，不能静默消耗
        // 调用者的 move source。
        return Err(status::CORE_INVALID_ARGUMENT);
    }
    Ok((image, argv, env, transferred))
}

fn read_spawn_request(user: u64, size: u64) -> Result<SpawnRequest, u32> {
    if size != wire::SPAWN_REQUEST_SIZE as u64 {
        return Err(status::CORE_INVALID_ARGUMENT);
    }
    let request: SpawnRequest = copy_user_value(user)?;
    if request.argv.reserved != 0 || request.env.reserved != 0 || request.transfers.reserved != 0 {
        return Err(status::CORE_INVALID_ARGUMENT);
    }
    Ok(request)
}

fn read_string_array(array: ProcessArrayRef) -> Result<Vec<Vec<u8>>, u32> {
    if array.count > MAX_PROCESS_STRINGS || (array.count != 0 && array.ptr == 0) {
        return Err(status::CORE_OUT_OF_RANGE);
    }
    let mut strings = Vec::new();
    strings
        .try_reserve_exact(array.count as usize)
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    let mut total = 0usize;
    for index in 0..array.count {
        let address = array
            .ptr
            .checked_add(u64::from(index) * size_of::<ProcessStringRef>() as u64)
            .ok_or(status::CORE_OUT_OF_RANGE)?;
        let item: ProcessStringRef = copy_user_value(address)?;
        let length = usize::try_from(item.len).map_err(|_| status::CORE_OUT_OF_RANGE)?;
        total = total.checked_add(length).ok_or(status::CORE_OUT_OF_RANGE)?;
        if total > MAX_PROCESS_STRING_BYTES || (length != 0 && item.ptr == 0) {
            return Err(status::CORE_OUT_OF_RANGE);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        bytes.resize(length, 0);
        if length != 0 {
            let user = usize::try_from(item.ptr).map_err(|_| status::STREAM_FAULT)?;
            copy_from_user(user, &mut bytes).map_err(|_| status::STREAM_FAULT)?;
        }
        strings.push(bytes);
    }
    Ok(strings)
}

fn read_transfers(
    state: &NativeProcessState,
    array: ProcessArrayRef,
) -> Result<Vec<PreparedNativeCapability>, u32> {
    if array.count > soyo::registry::MAX_CAPABILITIES || (array.count != 0 && array.ptr == 0) {
        return Err(status::CORE_OUT_OF_RANGE);
    }
    let mut transfers = Vec::new();
    transfers
        .try_reserve_exact(array.count as usize)
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    for index in 0..array.count {
        let address = array
            .ptr
            .checked_add(u64::from(index) * size_of::<HandleTransfer>() as u64)
            .ok_or(status::CORE_OUT_OF_RANGE)?;
        let transfer: HandleTransfer = copy_user_value(address)?;
        if transfer.reserved != 0 || transfer.flags & !wire::HANDLE_TRANSFER_MOVE != 0 {
            return Err(status::CORE_INVALID_ARGUMENT);
        }
        let requirement = native_abi::requirement_by_id(transfer.requirement_id)
            .ok_or(status::CORE_INVALID_ARGUMENT)?;
        let requirement_id = requirement.id;
        if matches!(
            requirement_id,
            native_abi::RequirementId::SelfProcess | native_abi::RequirementId::CurrentAddressSpace
        ) {
            return Err(status::CORE_INVALID_ARGUMENT);
        }
        if transfers
            .iter()
            .any(|entry: &PreparedNativeCapability| entry.requirement_id == requirement_id)
        {
            return Err(status::CORE_INVALID_ARGUMENT);
        }
        let source_handle = NativeHandle::from_raw(transfer.source_handle);
        if transfer.flags == wire::HANDLE_TRANSFER_MOVE
            && transfers
                .iter()
                .any(|entry: &PreparedNativeCapability| entry.source_handle == Some(source_handle))
        {
            return Err(status::CORE_INVALID_ARGUMENT);
        }
        let requested = Rights::from_bits(transfer.requested_rights);
        let entry = {
            let handles = state.handles.lock();
            let entry = handles.lookup(source_handle, Some(requirement.interface), requested)?;
            PreparedNativeCapability {
                requirement_id,
                object: entry.object.clone(),
                interface: entry.interface,
                rights: requested,
                source_handle: (transfer.flags == wire::HANDLE_TRANSFER_MOVE)
                    .then_some(NativeHandle::from_raw(transfer.source_handle)),
            }
        };
        transfers.push(entry);
    }
    Ok(transfers)
}

fn validate_move_transfers(
    state: &NativeProcessState,
    transfers: &[PreparedNativeCapability],
) -> Result<(), u32> {
    let handles = state.handles.lock();
    for transfer in transfers {
        let Some(source) = transfer.source_handle else {
            continue;
        };
        handles.lookup(source, Some(transfer.interface), transfer.rights)?;
    }
    Ok(())
}

fn commit_move_transfers(
    state: &NativeProcessState,
    transfers: &[PreparedNativeCapability],
) -> Result<(), u32> {
    let mut handles = state.handles.lock();
    // Validate every source while holding the table lock so a move is all-or-none.
    for transfer in transfers {
        let Some(source) = transfer.source_handle else {
            continue;
        };
        handles.lookup(source, Some(transfer.interface), transfer.rights)?;
    }
    for transfer in transfers {
        if let Some(source) = transfer.source_handle {
            let _ = handles.close(source)?;
        }
    }
    Ok(())
}

fn copy_user_value<T: Copy + Default>(user: u64) -> Result<T, u32> {
    let user = usize::try_from(user).map_err(|_| status::STREAM_FAULT)?;
    if user == 0 {
        return Err(status::STREAM_FAULT);
    }
    let mut value = T::default();
    let bytes = unsafe {
        core::slice::from_raw_parts_mut((&mut value as *mut T).cast::<u8>(), size_of::<T>())
    };
    copy_from_user(user, bytes).map_err(|_| status::STREAM_FAULT)?;
    Ok(value)
}

fn write_process_result(user: u64, result: &ProcessResult) -> Result<(), u32> {
    let user = usize::try_from(user).map_err(|_| status::STREAM_FAULT)?;
    if user == 0 {
        return Err(status::STREAM_FAULT);
    }
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (result as *const ProcessResult).cast::<u8>(),
            size_of::<ProcessResult>(),
        )
    };
    copy_to_user(user, bytes).map_err(|_| status::STREAM_FAULT)
}

enum WaitFailure {
    Timeout,
    ExternalControl,
}

fn wait_until_terminated(
    task: &Arc<Task>,
    group: &ThreadGroup,
    deadline_ns: u64,
) -> Result<(), WaitFailure> {
    while !group.is_terminated() {
        if super::operations::has_native_external_control(task) {
            return Err(WaitFailure::ExternalControl);
        }
        if deadline_ns != 0 && sched::now_ns_public() >= deadline_ns {
            return Err(WaitFailure::Timeout);
        }
        let entry = group
            .process_exit_waiters()
            .prepare_to_wait(task, TaskState::Sleeping);
        let deadline_armed = deadline_ns != 0 && sched::register_sleep_deadline(task, deadline_ns);
        if deadline_ns != 0 && !deadline_armed {
            group.process_exit_waiters().finish_wait(&entry);
            super::operations::restore_native_task_after_wait(task);
            return Err(WaitFailure::Timeout);
        }
        if group.is_terminated() {
            if deadline_armed {
                sched::cancel_sleep_deadline(task);
            }
            group.process_exit_waiters().finish_wait(&entry);
            super::operations::restore_native_task_after_wait(task);
            break;
        }
        if super::operations::has_native_external_control(task) {
            if deadline_armed {
                sched::cancel_sleep_deadline(task);
            }
            group.process_exit_waiters().finish_wait(&entry);
            super::operations::restore_native_task_after_wait(task);
            return Err(WaitFailure::ExternalControl);
        }
        sched::schedule_once(sched::now_ns_public());
        if deadline_armed {
            sched::cancel_sleep_deadline(task);
        }
        group.process_exit_waiters().finish_wait(&entry);
        super::operations::restore_native_task_after_wait(task);
    }
    Ok(())
}

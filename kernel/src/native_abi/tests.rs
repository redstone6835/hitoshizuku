use alloc::sync::{Arc, Weak};
use alloc::vec;

use general::TrapFramePtr;
use general::mm::VmSpace;
use general::syscall::{NativeCallFrame, NativeCallOutcome, NativeCallReturn};
use ktest::ktest;
use mm::VmFlags;
use native_abi::{
    BoundCallSlot, ExecPhase, NativeBindingPlan, NativeHandle, NativeHandleTable, ObjectInterface,
    OperationId, Rights, operation, status,
};
use sched::{ProcessGroup, ProcessPersonalityState, SchedParams, Session, Task, ThreadGroup};

use super::operations::map_stream_write_error;
use super::{KernelNativeObject, NativeProcessState, dispatch_native_call};

#[ktest]
fn native_frame_reads_the_frozen_register_contract() {
    let mut frame = arch_trap_frame();
    frame.a7 = 7;
    frame.a6 = 0x0000_0002_0000_0003;
    frame.a0 = 10;
    frame.a1 = 11;
    frame.a2 = 12;
    frame.a3 = 13;
    frame.a4 = 14;
    frame.a5 = 0xfeed;

    let call = (general::syscall::frame_ops()
        .expect("架构必须已注册 syscall frame ops")
        .native_call)(TrapFramePtr::new(&mut frame as *mut _ as usize));

    assert_eq!(call.slot, 7);
    assert_eq!(call.object_handle, 0x0000_0002_0000_0003);
    assert_eq!(call.args, [10, 11, 12, 13, 14]);
    assert_eq!(call.reserved_arg, 0xfeed);
}

#[ktest]
fn native_failure_return_clears_values_and_advances_pc() {
    let mut frame = arch_trap_frame();
    set_frame_pc(&mut frame, 0x4000);
    let tf = TrapFramePtr::new(&mut frame as *mut _ as usize);
    let ops = general::syscall::frame_ops().expect("架构必须已注册 syscall frame ops");

    (ops.set_native_ret)(
        tf,
        NativeCallReturn {
            status: status::HANDLE_STALE,
            value0: 0xaaaa,
            value1: 0xbbbb,
        },
    );
    (ops.advance_pc)(tf);

    assert_eq!(frame.a0, status::HANDLE_STALE as usize);
    assert_eq!(frame.a1, 0);
    assert_eq!(frame.a2, 0);
    assert_eq!(frame_pc(&frame), 0x4004);
}

#[ktest]
fn personality_selects_native_dispatch_without_linux_syscall_table() {
    let task = make_task(true);
    let mut frame = arch_trap_frame();
    frame.a7 = usize::MAX;
    set_frame_pc(&mut frame, 0x5000);

    general::syscall::dispatch_for_task(TrapFramePtr::new(&mut frame as *mut _ as usize), task);

    assert_eq!(frame.a0, status::ABI_BAD_SLOT as usize);
    assert_eq!(frame.a1, 0);
    assert_eq!(frame.a2, 0);
    assert_eq!(frame_pc(&frame), 0x5004);
}

#[ktest]
fn native_return_boundary_consumes_ignored_external_signal() {
    let task = make_task(true);
    task.shared_signal().set_action(
        sched::SignalNumber::SIGUSR1,
        sched::SigAction {
            handler: sched::SigHandler::Ignore,
            mask: sched::SigSet::EMPTY,
            flags: sched::SigActionFlags(0),
            restorer: 0,
        },
    );
    task.signal.deliver(sched::SigInfo {
        sig: sched::SignalNumber::SIGUSR1,
        code: 0,
        sender_pid: 1,
        sender_uid: sched::Uid::ROOT,
        raw: None,
    });
    let mut frame = arch_trap_frame();
    frame.a7 = usize::MAX;
    set_frame_pc(&mut frame, 0x5800);

    general::syscall::dispatch_for_task(
        TrapFramePtr::new(&mut frame as *mut _ as usize),
        Arc::clone(&task),
    );

    assert!(!task.signal.has_any_pending());
    assert_eq!(frame.a0, status::ABI_BAD_SLOT as usize);
    assert_eq!(frame_pc(&frame), 0x5804);
}

#[ktest]
fn tomori_personality_stays_on_the_linux_syscall_table() {
    let task = make_task(false);
    let mut frame = arch_trap_frame();
    frame.a7 = usize::MAX;
    set_frame_pc(&mut frame, 0x6000);

    general::syscall::dispatch_for_task(TrapFramePtr::new(&mut frame as *mut _ as usize), task);

    assert_eq!(
        frame.a0,
        (-(errno::Errno::ENOSYS.as_i32() as isize)) as usize
    );
    assert_eq!(frame_pc(&frame), 0x6004);
}

#[ktest]
fn native_dispatch_rejects_reserved_register_before_handle_lookup() {
    let task = make_native_task(
        alloc::vec![bound_slot(0, OperationId::ClockRead)],
        empty_handles(),
    )
    .0;

    let result = invoke_native(
        &task,
        NativeCallFrame {
            slot: 0,
            object_handle: 0,
            args: [0; 5],
            reserved_arg: 1,
        },
    );

    assert_eq!(result.status, status::CORE_INVALID_ARGUMENT);
}

#[ktest]
fn native_dispatch_rejects_unused_argument_before_handle_lookup() {
    let task = make_native_task(
        alloc::vec![bound_slot(0, OperationId::ClockRead)],
        empty_handles(),
    )
    .0;

    let result = invoke_native(
        &task,
        NativeCallFrame {
            slot: 0,
            object_handle: 0,
            args: [1, 0, 0, 0, 0],
            reserved_arg: 0,
        },
    );

    assert_eq!(result.status, status::CORE_INVALID_ARGUMENT);
}

#[ktest]
fn native_dispatch_reports_wrong_interface_before_rights() {
    let mut handles = empty_handles();
    let handle = handles
        .insert(
            KernelNativeObject::MonotonicClock,
            ObjectInterface::Clock,
            Rights::TERMINATE_SELF,
        )
        .expect("测试 handle 应分配成功");
    let task = make_native_task(
        alloc::vec![bound_slot(0, OperationId::ProcessExit)],
        handles,
    )
    .0;

    let result = invoke_native(&task, native_call(0, handle));

    assert_eq!(result.status, status::HANDLE_WRONG_INTERFACE);
}

#[ktest]
fn native_dispatch_reports_rights_denial() {
    let mut handles = empty_handles();
    let handle = handles
        .insert(
            KernelNativeObject::SelfProcess,
            ObjectInterface::Process,
            Rights::NONE,
        )
        .expect("测试 handle 应分配成功");
    let task = make_native_task(
        alloc::vec![bound_slot(0, OperationId::ProcessExit)],
        handles,
    )
    .0;

    let result = invoke_native(&task, native_call(0, handle));

    assert_eq!(result.status, status::SECURITY_RIGHTS_DENIED);
}

#[ktest]
fn native_dispatch_reports_stale_handle() {
    let mut handles = empty_handles();
    let handle = handles
        .insert(
            KernelNativeObject::MonotonicClock,
            ObjectInterface::Clock,
            Rights::READ,
        )
        .expect("测试 handle 应分配成功");
    handles.close(handle).expect("测试 handle 应可关闭");
    let task = make_native_task(alloc::vec![bound_slot(0, OperationId::ClockRead)], handles).0;

    let result = invoke_native(&task, native_call(0, handle));

    assert_eq!(result.status, status::HANDLE_STALE);
}

#[ktest]
fn native_dispatch_returns_optional_unbound_before_argument_validation() {
    let task = make_native_task(
        alloc::vec![BoundCallSlot {
            slot: 0,
            operation: None,
            interface: None,
            required_rights: Rights::NONE,
        }],
        empty_handles(),
    )
    .0;

    let result = invoke_native(
        &task,
        NativeCallFrame {
            slot: 0,
            object_handle: 0,
            args: [1; 5],
            reserved_arg: 1,
        },
    );

    assert_eq!(result.status, status::ABI_UNSUPPORTED_OPERATION);
}

#[ktest]
fn process_exit_preserves_the_full_u32_code() {
    let mut handles = empty_handles();
    let handle = handles
        .insert(
            KernelNativeObject::SelfProcess,
            ObjectInterface::Process,
            Rights::TERMINATE_SELF,
        )
        .expect("self process handle 应分配成功");
    let task = make_native_task(
        alloc::vec![bound_slot(0, OperationId::ProcessExit)],
        handles,
    )
    .0;
    let mut call = native_call(0, handle);
    call.args[0] = 0xfedc_ba98;

    assert!(matches!(
        dispatch_native_call(&task, call),
        NativeCallOutcome::ExitGroup(code) if code == 0xfedc_ba98_u32 as i32
    ));
}

#[ktest]
fn process_exit_rejects_bits_above_u32() {
    let mut handles = empty_handles();
    let handle = handles
        .insert(
            KernelNativeObject::SelfProcess,
            ObjectInterface::Process,
            Rights::TERMINATE_SELF,
        )
        .expect("self process handle 应分配成功");
    let task = make_native_task(
        alloc::vec![bound_slot(0, OperationId::ProcessExit)],
        handles,
    )
    .0;
    let mut call = native_call(0, handle);
    call.args[0] = 1_u64 << 32;

    assert_eq!(
        invoke_native(&task, call).status,
        status::CORE_INVALID_ARGUMENT
    );
}

#[ktest]
fn handle_duplicate_returns_an_independent_handle() {
    let mut handles = empty_handles();
    let rights = Rights::READ | Rights::DUPLICATE;
    let source = handles
        .insert(
            KernelNativeObject::MonotonicClock,
            ObjectInterface::Clock,
            rights,
        )
        .expect("源 handle 应分配成功");
    let (task, state) = make_native_task(
        alloc::vec![bound_slot(0, OperationId::HandleDuplicate)],
        handles,
    );

    let result = invoke_native(&task, native_call(0, source));
    let duplicate = NativeHandle::from_raw(result.value0);

    assert_eq!(result.status, status::OK);
    assert_ne!(duplicate, source);
    assert!(
        state
            .handles
            .lock()
            .lookup(duplicate, Some(ObjectInterface::Clock), rights)
            .is_ok()
    );
}

#[ktest]
fn handle_restrict_creates_a_rights_subset_without_changing_source() {
    let mut handles = empty_handles();
    let source_rights = Rights::READ | Rights::DUPLICATE;
    let source = handles
        .insert(
            KernelNativeObject::MonotonicClock,
            ObjectInterface::Clock,
            source_rights,
        )
        .expect("源 handle 应分配成功");
    let (task, state) = make_native_task(
        alloc::vec![bound_slot(0, OperationId::HandleRestrict)],
        handles,
    );
    let mut call = native_call(0, source);
    call.args[0] = Rights::READ.bits();

    let result = invoke_native(&task, call);
    let restricted = NativeHandle::from_raw(result.value0);
    let handles = state.handles.lock();

    assert_eq!(result.status, status::OK);
    assert_eq!(
        handles
            .lookup(restricted, Some(ObjectInterface::Clock), Rights::READ)
            .expect("降权 handle 应可查找")
            .rights,
        Rights::READ
    );
    assert_eq!(
        handles
            .lookup(source, Some(ObjectInterface::Clock), source_rights)
            .expect("源 handle 不应改变")
            .rights,
        source_rights
    );
}

#[ktest]
fn handle_restrict_rejects_added_rights() {
    let mut handles = empty_handles();
    let source = handles
        .insert(
            KernelNativeObject::MonotonicClock,
            ObjectInterface::Clock,
            Rights::READ | Rights::DUPLICATE,
        )
        .expect("源 handle 应分配成功");
    let task = make_native_task(
        alloc::vec![bound_slot(0, OperationId::HandleRestrict)],
        handles,
    )
    .0;
    let mut call = native_call(0, source);
    call.args[0] = Rights::READ.bits() | Rights::WRITE.bits();

    assert_eq!(
        invoke_native(&task, call).status,
        status::SECURITY_RIGHTS_DENIED
    );
}

#[ktest]
fn handle_close_makes_the_old_value_stale() {
    let mut handles = empty_handles();
    let source = handles
        .insert(
            KernelNativeObject::MonotonicClock,
            ObjectInterface::Clock,
            Rights::READ,
        )
        .expect("源 handle 应分配成功");
    let (task, state) = make_native_task(
        alloc::vec![bound_slot(0, OperationId::HandleClose)],
        handles,
    );

    let result = invoke_native(&task, native_call(0, source));

    assert_eq!(result.status, status::OK);
    assert_eq!(
        state
            .handles
            .lock()
            .lookup(source, None, Rights::NONE)
            .err(),
        Some(status::HANDLE_STALE)
    );
}

#[ktest]
fn clock_read_returns_a_monotonic_value_and_zero_second_result() {
    let mut handles = empty_handles();
    let clock = handles
        .insert(
            KernelNativeObject::MonotonicClock,
            ObjectInterface::Clock,
            Rights::READ,
        )
        .expect("clock handle 应分配成功");
    let task = make_native_task(alloc::vec![bound_slot(0, OperationId::ClockRead)], handles).0;

    let first = invoke_native(&task, native_call(0, clock));
    let second = invoke_native(&task, native_call(0, clock));

    assert_eq!(first.status, status::OK);
    assert_eq!(first.value1, 0);
    assert!(second.value0 >= first.value0);
}

#[ktest]
fn stream_write_zero_length_does_not_touch_the_pointer() {
    let (task, _state, _read, stream) = make_stream_task(Rights::WRITE, true);
    let mut call = native_call(0, stream);
    call.args[0] = u64::MAX;

    let result = invoke_native(&task, call);

    assert_eq!(result.status, status::OK);
    assert_eq!(result.value0, 0);
}

#[ktest]
fn stream_write_reports_user_buffer_fault_before_progress() {
    let (task, _state, _read, stream) = make_stream_task(Rights::WRITE, true);
    let mut call = native_call(0, stream);
    call.args[0] = 0x1000_0000;
    call.args[1] = 4;

    assert_eq!(invoke_native(&task, call).status, status::IO_FAULT);
}

#[ktest]
fn stream_write_returns_partial_progress() {
    let (task, _state, _read, stream) = make_stream_task(Rights::WRITE, true);
    let write_file = stream_file(&task, stream);
    write_file
        .write(&vec![0; 15 * 4096])
        .expect("测试 pipe 预填充应成功");
    let user = install_user_bytes(&task, &vec![0x5a; 2 * 4096]);
    let mut call = native_call(0, stream);
    call.args[0] = user as u64;
    call.args[1] = (2 * 4096) as u64;

    let result = invoke_native(&task, call);

    assert_eq!(result.status, status::OK);
    assert_eq!(result.value0, 4096);
}

#[ktest]
fn stream_write_maps_zero_progress_would_block() {
    let (task, _state, _read, stream) = make_stream_task(Rights::WRITE, true);
    let write_file = stream_file(&task, stream);
    write_file
        .write(&vec![0; 16 * 4096])
        .expect("测试 pipe 预填充应成功");
    let user = install_user_bytes(&task, &[0x5a; 16]);
    let mut call = native_call(0, stream);
    call.args[0] = user as u64;
    call.args[1] = 16;

    assert_eq!(invoke_native(&task, call).status, status::IO_WOULD_BLOCK);
}

#[ktest]
fn stream_write_maps_closed_peer_without_reusing_user_fault() {
    let (task, _state, read, stream) = make_stream_task(Rights::WRITE, true);
    drop(read);
    let user = install_user_bytes(&task, &[0x5a; 16]);
    let mut call = native_call(0, stream);
    call.args[0] = user as u64;
    call.args[1] = 16;

    assert_eq!(invoke_native(&task, call).status, status::IO_CLOSED);
}

#[ktest]
fn stream_write_maps_general_io_error_without_reusing_user_fault() {
    assert_native_return(
        map_stream_write_error(general::vfs::error::VfsError::Io, 0),
        status::IO_ERROR,
        0,
    );
    assert_native_return(
        map_stream_write_error(general::vfs::error::VfsError::Io, 7),
        status::OK,
        7,
    );
}

#[ktest]
fn stream_write_unwinds_when_group_exit_rejects_sleep() {
    let (task, _state, _read, stream) = make_stream_task(Rights::WRITE, false);
    assert!(task.cas_state(sched::TaskState::New, sched::TaskState::Running));
    let write_file = stream_file(&task, stream);
    write_file
        .write(&vec![0; 16 * 4096])
        .expect("测试 pipe 预填充应成功");
    let user = install_user_bytes(&task, &[0x5a; 16]);
    let mut call = native_call(0, stream);
    call.args[0] = user as u64;
    call.args[1] = 16;
    assert_eq!(task.thread_group().request_group_exit(37), 37);
    sched::group_exit_wakeup(&task);

    assert!(matches!(
        dispatch_native_call(&task, call),
        NativeCallOutcome::RetryExternalControl
    ));
    assert_eq!(task.state(), sched::TaskState::Running);
}

fn assert_native_return(outcome: NativeCallOutcome, status: u32, value0: u64) {
    let NativeCallOutcome::Return(result) = outcome else {
        panic!("测试映射必须返回 Native status");
    };
    assert_eq!(result.status, status);
    assert_eq!(result.value0, value0);
    assert_eq!(result.value1, 0);
}

fn make_task(native: bool) -> Arc<Task> {
    let task = make_plain_task();
    if native {
        install_native_state(
            &task,
            NativeBindingPlan {
                call_slots: alloc::vec::Vec::new(),
            },
            empty_handles(),
        );
    }
    task
}

fn make_plain_task() -> Arc<Task> {
    let session = Session::new();
    let process_group = ProcessGroup::new(&session);
    session.register_group(&process_group);
    let group = ThreadGroup::new();
    let task = Task::new(
        SchedParams::default_fair(),
        Weak::new(),
        Arc::clone(&group),
        process_group,
    );
    group.set_leader(&task);
    group.add_member(&task);
    task
}

fn make_native_task(
    call_slots: alloc::vec::Vec<BoundCallSlot>,
    handles: NativeHandleTable<KernelNativeObject>,
) -> (Arc<Task>, Arc<NativeProcessState>) {
    let task = make_plain_task();
    let state = install_native_state(&task, NativeBindingPlan { call_slots }, handles);
    (task, state)
}

fn install_native_state(
    task: &Arc<Task>,
    binding: NativeBindingPlan,
    handles: NativeHandleTable<KernelNativeObject>,
) -> Arc<NativeProcessState> {
    let state = Arc::new(NativeProcessState {
        binding,
        handles: sched::sync::Spinlock::new(handles),
        build_id: [0; 32],
        content_hash: [0; 32],
        image_base: 0,
    });
    let payload: Arc<dyn core::any::Any + Send + Sync> = state.clone();
    let group = task.thread_group();
    let mut exec = group.lock_exec();
    exec.set_phase(ExecPhase::Transitioning);
    exec.install_personality(ProcessPersonalityState::MygoNative(payload));
    exec.set_phase(ExecPhase::Running);
    state
}

fn empty_handles() -> NativeHandleTable<KernelNativeObject> {
    NativeHandleTable::new().expect("测试 Native handle table 应创建成功")
}

fn bound_slot(slot: u32, id: OperationId) -> BoundCallSlot {
    let spec = operation(id).expect("测试 operation 必须已注册");
    BoundCallSlot {
        slot,
        operation: Some(id),
        interface: spec.interface,
        required_rights: spec.required_rights,
    }
}

fn native_call(slot: u64, handle: NativeHandle) -> NativeCallFrame {
    NativeCallFrame {
        slot,
        object_handle: handle.raw(),
        args: [0; 5],
        reserved_arg: 0,
    }
}

fn invoke_native(task: &Arc<Task>, call: NativeCallFrame) -> NativeCallReturn {
    match dispatch_native_call(task, call) {
        NativeCallOutcome::Return(result) => result,
        NativeCallOutcome::ExitGroup(_) => panic!("测试调用不应退出线程组"),
        NativeCallOutcome::RetryExternalControl => panic!("测试调用不应等待外部控制"),
    }
}

fn make_stream_task(
    rights: Rights,
    nonblock: bool,
) -> (
    Arc<Task>,
    Arc<NativeProcessState>,
    Arc<general::vfs::file::File>,
    NativeHandle,
) {
    let (read, write) =
        general::vfs::pipe::new_pipe(Arc::new(general::vfs::Credentials::root()), nonblock)
            .expect("测试 pipe 应创建成功");
    let mut handles = empty_handles();
    let handle = handles
        .insert(
            KernelNativeObject::Stream(write),
            ObjectInterface::Stream,
            rights,
        )
        .expect("stream handle 应分配成功");
    let (task, state) = make_native_task(
        alloc::vec![bound_slot(0, OperationId::StreamWrite)],
        handles,
    );
    (task, state, read, handle)
}

fn stream_file(task: &Arc<Task>, handle: NativeHandle) -> Arc<general::vfs::file::File> {
    let payload = task
        .thread_group()
        .native_personality_payload()
        .expect("测试 task 应有 Native personality");
    let state = payload
        .downcast::<NativeProcessState>()
        .expect("Native payload 类型应正确");
    let object = state
        .handles
        .lock()
        .lookup(handle, Some(ObjectInterface::Stream), Rights::WRITE)
        .expect("stream handle 应可查找")
        .object
        .clone();
    let KernelNativeObject::Stream(file) = object else {
        panic!("stream handle 应固定 Stream 对象");
    };
    file
}

fn install_user_bytes(task: &Arc<Task>, bytes: &[u8]) -> usize {
    const USER: usize = 0x1000_0000;
    let page_size = general::mm::page_size();
    let mapped_len = bytes.len().div_ceil(page_size) * page_size;
    let vm = Arc::new(VmSpace::new());
    vm.map_anon(
        USER..USER + mapped_len,
        VmFlags::EMPTY
            .with(VmFlags::READ)
            .with(VmFlags::WRITE)
            .with(VmFlags::USER),
    )
    .expect("测试用户缓冲应映射成功");
    let mut copied = 0;
    while copied < bytes.len() {
        let count = unsafe {
            vm.with_user_write_slice(USER + copied, bytes.len() - copied, |window| {
                window.copy_from_slice(&bytes[copied..copied + window.len()]);
                window.len()
            })
        }
        .expect("测试用户缓冲应可写入");
        copied += count;
    }
    let payload: Arc<dyn core::any::Any + Send + Sync> = vm;
    task.ext_install(sched::TASKEXT_VM_SPACE, payload);
    USER
}

#[cfg(target_arch = "riscv64")]
fn arch_trap_frame() -> arch::riscv64::trap_frame::TrapFrame {
    arch::riscv64::trap_frame::TrapFrame::default()
}

#[cfg(target_arch = "loongarch64")]
fn arch_trap_frame() -> arch::loongarch64::TrapFrame {
    arch::loongarch64::TrapFrame::default()
}

#[cfg(target_arch = "riscv64")]
fn set_frame_pc(frame: &mut arch::riscv64::trap_frame::TrapFrame, pc: usize) {
    frame.sepc = pc;
}

#[cfg(target_arch = "loongarch64")]
fn set_frame_pc(frame: &mut arch::loongarch64::TrapFrame, pc: usize) {
    frame.pc = pc;
}

#[cfg(target_arch = "riscv64")]
fn frame_pc(frame: &arch::riscv64::trap_frame::TrapFrame) -> usize {
    frame.sepc
}

#[cfg(target_arch = "loongarch64")]
fn frame_pc(frame: &arch::loongarch64::TrapFrame) -> usize {
    frame.pc
}

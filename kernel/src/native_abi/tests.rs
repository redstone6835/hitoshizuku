use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;

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
use soyo::ImageSegment;
use soyo::registry::{SegmentKind, SegmentPermissions};

use super::dispatch::dispatch_native_call;
use super::operations::{map_stream_read_error, map_stream_write_error, stream_read_progress};
use super::{KernelNativeObject, NativeProcessState};

#[ktest]
fn component_final_permissions_protect_only_unmapped_gaps() {
    const BASE: usize = 0x4000_0000;
    const PAGE: u64 = 4096;

    let segments = [
        ImageSegment {
            kind: SegmentKind::Code,
            permissions: (SegmentPermissions::READ | SegmentPermissions::EXECUTE).bits(),
            virtual_offset: 0,
            file_offset: PAGE,
            file_size: 32,
            memory_size: 32,
            alignment: PAGE,
        },
        ImageSegment {
            kind: SegmentKind::Data,
            permissions: (SegmentPermissions::READ | SegmentPermissions::WRITE).bits(),
            virtual_offset: PAGE * 3,
            file_offset: PAGE * 2,
            file_size: 16,
            memory_size: 16,
            alignment: PAGE,
        },
    ];

    let plan =
        super::component::component_protection_plan(&(BASE..BASE + PAGE as usize * 4), &segments)
            .expect("合法组件段应生成最终权限计划");

    assert_eq!(plan.len(), 3);
    assert_eq!(plan[0].0, BASE..BASE + PAGE as usize);
    assert!(plan[0].1.has(VmFlags::READ));
    assert!(plan[0].1.has(VmFlags::EXEC));
    assert_eq!(plan[1].0, BASE + PAGE as usize..BASE + PAGE as usize * 3);
    assert_eq!(plan[1].1.permissions(), VmFlags::EMPTY);
    assert_eq!(
        plan[2].0,
        BASE + PAGE as usize * 3..BASE + PAGE as usize * 4
    );
    assert!(plan[2].1.has(VmFlags::READ));
    assert!(plan[2].1.has(VmFlags::WRITE));
}

#[ktest]
fn native_fixed_record_copy_supports_cross_page_user_ranges() {
    const BASE: usize = 0x1200_0000;

    let task = make_plain_task();
    let vm = Arc::new(VmSpace::new());
    let page_size = general::mm::page_size();
    vm.map_anon(
        BASE..BASE + page_size * 2,
        VmFlags::EMPTY
            .with(VmFlags::READ)
            .with(VmFlags::WRITE)
            .with(VmFlags::USER),
    )
    .expect("测试跨页用户记录应映射成功");
    task.ext_install(sched::TASKEXT_VM_SPACE, vm.clone());

    let expected = native_abi::wire::ThreadCreateRequest {
        entry: 0x1122_3344_5566_7788,
        stack_memory: 0x8877_6655_4433_2211,
        stack_offset: 0x1000,
        stack_size: 0x2000,
        tls_memory: 0x0102_0304_0506_0708,
        tls_offset: 0x3000,
        argument: 0xaabb_ccdd_eeff_0011,
        flags: 0,
    };
    let address = BASE + page_size - core::mem::size_of_val(&expected) / 2;
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&expected as *const native_abi::wire::ThreadCreateRequest).cast::<u8>(),
            core::mem::size_of_val(&expected),
        )
    };
    let mut copied = 0usize;
    while copied < bytes.len() {
        let count = unsafe {
            vm.with_user_write_slice(address + copied, bytes.len() - copied, |window| {
                window.copy_from_slice(&bytes[copied..copied + window.len()]);
                window.len()
            })
        }
        .expect("测试跨页用户记录应写入成功");
        copied += count;
    }

    let actual =
        super::copy_user_value::<native_abi::wire::ThreadCreateRequest>(&task, address as u64)
            .expect("Native 固定记录应支持跨页读取");
    assert_eq!(actual, expected);

    let replacement = native_abi::wire::ThreadCreateRequest {
        argument: 0x55aa_aa55_1234_5678,
        ..expected
    };
    super::copy_user_value_out(&task, address as u64, &replacement)
        .expect("Native 固定记录应支持跨页写回");
    let actual =
        super::copy_user_value::<native_abi::wire::ThreadCreateRequest>(&task, address as u64)
            .expect("跨页写回结果应可重新读取");
    assert_eq!(actual, replacement);
}

#[ktest]
fn native_frame_reads_the_frozen_register_contract() {
    let mut frame = arch_trap_frame();
    configure_native_call_frame(&mut frame);

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

    assert_eq!(
        native_return_values(&frame),
        (status::HANDLE_STALE as usize, 0, 0)
    );
    assert_eq!(frame_pc(&frame), native_next_pc(0x4000));
}

#[ktest]
fn personality_selects_native_dispatch_without_linux_syscall_table() {
    let task = make_task(true);
    let mut frame = arch_trap_frame();
    set_native_invalid_slot(&mut frame);
    set_frame_pc(&mut frame, 0x5000);

    general::syscall::dispatch_for_task(TrapFramePtr::new(&mut frame as *mut _ as usize), task);

    assert_eq!(
        native_return_values(&frame),
        (status::ABI_BAD_SLOT as usize, 0, 0)
    );
    assert_eq!(frame_pc(&frame), native_next_pc(0x5000));
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
    set_native_invalid_slot(&mut frame);
    set_frame_pc(&mut frame, 0x5800);

    general::syscall::dispatch_for_task(
        TrapFramePtr::new(&mut frame as *mut _ as usize),
        Arc::clone(&task),
    );

    assert!(!task.signal.has_any_pending());
    assert_eq!(
        native_return_values(&frame).0,
        status::ABI_BAD_SLOT as usize
    );
    assert_eq!(frame_pc(&frame), native_next_pc(0x5800));
}

#[ktest]
fn tomori_personality_stays_on_the_linux_syscall_table() {
    let task = make_task(false);
    let mut frame = arch_trap_frame();
    set_native_invalid_slot(&mut frame);
    set_frame_pc(&mut frame, 0x6000);

    general::syscall::dispatch_for_task(TrapFramePtr::new(&mut frame as *mut _ as usize), task);

    assert_eq!(
        native_return_values(&frame).0,
        (-(errno::Errno::ENOSYS.as_i32() as isize)) as usize
    );
    assert_eq!(frame_pc(&frame), native_next_pc(0x6000));
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
            Rights::EXIT,
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
            Rights::EXIT,
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
            Rights::EXIT,
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
fn memory_allocate_returns_owned_aligned_memory_and_frees_it() {
    let (task, vm, address_space) = make_address_space_task();
    let page_size = native_abi::PAGE_SIZE;
    let alignment = 4 * 1024 * 1024;
    let mut map = native_call(0, address_space);
    map.args = [page_size * 2, alignment, 0, 0, 0];

    let mapped = invoke_native(&task, map);
    assert_eq!(mapped.status, status::OK);
    assert_eq!(mapped.value0 % alignment, 0);
    let start = mapped.value0 as usize;
    vm.contains_user_range(start..start + mapped.value1 as usize)
        .expect("Native 匿名映射应完整存在");

    let mut unmap = native_call(1, address_space);
    unmap.args[0] = mapped.value0;
    unmap.args[1] = mapped.value1;
    assert_eq!(invoke_native(&task, unmap).status, status::OK);
    assert!(
        vm.contains_user_range(start..start + mapped.value1 as usize)
            .is_err()
    );
}

#[ktest]
fn vm_map_anon_any_aligned_publishes_the_mapping_before_return() {
    let vm = Arc::new(VmSpace::new());
    let page_size = general::mm::page_size();
    let range = vm
        .map_anon_any_aligned(
            page_size * 2,
            2 * 1024 * 1024,
            VmFlags::EMPTY.with(VmFlags::READ).with(VmFlags::USER),
        )
        .expect("对齐匿名映射应成功");

    assert_eq!(range.start % (2 * 1024 * 1024), 0);
    vm.contains_user_range(range)
        .expect("映射返回时必须已经发布到 VMA 集合");
}

#[ktest]
fn memory_allocate_rejects_invalid_contract() {
    let (task, _vm, address_space) = make_address_space_task();
    let mut map = native_call(0, address_space);
    map.args = [0, native_abi::PAGE_SIZE, 0, 0, 0];
    assert_eq!(
        invoke_native(&task, map).status,
        status::MEMORY_INVALID_RANGE
    );
    map.args = [native_abi::PAGE_SIZE, native_abi::PAGE_SIZE / 2, 0, 0, 0];
    assert_eq!(
        invoke_native(&task, map).status,
        status::MEMORY_INVALID_ALIGNMENT
    );
    map.args = [u64::MAX, native_abi::PAGE_SIZE, 0, 0, 0];
    assert_eq!(
        invoke_native(&task, map).status,
        status::MEMORY_INVALID_RANGE
    );
}

#[ktest]
fn memory_free_rejects_ranges_not_owned_by_native_allocation() {
    let (task, _vm, address_space) = make_address_space_task();
    let layout = general::mm::user_vm_layout().expect("架构必须注册用户 VM 布局");
    let mut unmap = native_call(1, address_space);
    unmap.args[0] = layout.user_mmap_base as u64;
    unmap.args[1] = native_abi::PAGE_SIZE;

    assert_eq!(invoke_native(&task, unmap).status, status::MEMORY_NOT_OWNED);
}

#[ktest]
fn memory_free_preserves_runtime_owned_ranges() {
    let (task, vm, address_space) = make_address_space_task();
    let page_size = native_abi::PAGE_SIZE;
    let mut map = native_call(0, address_space);
    map.args = [page_size, page_size, 0, 0, 0];
    assert_eq!(invoke_native(&task, map).status, status::OK);

    let mut unmap = native_call(1, address_space);
    unmap.args[0] = page_size;
    unmap.args[1] = page_size;
    assert_eq!(invoke_native(&task, unmap).status, status::MEMORY_NOT_OWNED);
    vm.contains_user_range(page_size as usize..(page_size * 2) as usize)
        .expect("StartInfo 保护区不应被取消映射");
}

#[ktest]
fn memory_object_create_map_query_and_unmap_form_a_closed_loop() {
    let (task, state) = make_native_task(
        alloc::vec![
            bound_slot(0, OperationId::MemoryCreate),
            bound_slot(1, OperationId::MemoryMap),
            bound_slot(2, OperationId::MemoryQuery),
            bound_slot(3, OperationId::MemoryUnmap),
            bound_slot(4, OperationId::MemoryRevoke),
            bound_slot(5, OperationId::MemoryStatistics),
        ],
        empty_handles(),
    );
    let vm = task_vm(&task);
    let (process, address_space) = {
        let mut handles = state.handles.lock();
        let process = handles
            .insert(
                KernelNativeObject::SelfProcess,
                ObjectInterface::Process,
                Rights::CREATE,
            )
            .expect("测试 Process handle 应分配成功");
        let address_space = handles
            .insert(
                KernelNativeObject::AddressSpace(Arc::clone(&vm)),
                ObjectInterface::AddressSpace,
                Rights::ALLOCATE | Rights::FREE,
            )
            .expect("测试 AddressSpace handle 应分配成功");
        (process, address_space)
    };

    let request_address = 0x1000_0000;
    let query_address = request_address + general::mm::page_size();
    let map_address = query_address + general::mm::page_size();
    let statistics_address = map_address + general::mm::page_size();
    install_user_value(
        &task,
        request_address,
        &native_abi::wire::MemoryCreateRequest {
            size: native_abi::PAGE_SIZE * 2,
            alignment: native_abi::PAGE_SIZE,
            flags: native_abi::wire::MEMORY_FLAG_SHARED,
            kind: native_abi::wire::MEMORY_KIND_ANONYMOUS,
            source_handle: 0,
            source_offset: 0,
            reserved: [0; 3],
        },
    );
    install_user_value(
        &task,
        query_address,
        &native_abi::wire::MemoryInfo::default(),
    );
    install_user_value(
        &task,
        map_address,
        &native_abi::wire::MemoryMapRequest {
            address_space: address_space.raw(),
            offset: native_abi::PAGE_SIZE,
            length: native_abi::PAGE_SIZE,
            alignment: native_abi::PAGE_SIZE,
            address_hint: 0,
            permissions: native_abi::wire::MEMORY_PERMISSION_READ
                | native_abi::wire::MEMORY_PERMISSION_WRITE,
            flags: 0,
            reserved: [0; 2],
        },
    );
    install_user_value(
        &task,
        statistics_address,
        &native_abi::wire::MemoryStatistics::default(),
    );

    let mut create = native_call(0, process);
    create.args[0] = request_address as u64;
    let created = invoke_native(&task, create);
    assert_eq!(created.status, status::OK);
    let memory = NativeHandle::from_raw(created.value0);
    let map_only = {
        let mut handles = state.handles.lock();
        let object = handles
            .lookup(memory, Some(ObjectInterface::MemoryObject), Rights::MAP)
            .expect("测试 MemoryObject 应存在")
            .object
            .clone();
        handles
            .insert(object, ObjectInterface::MemoryObject, Rights::MAP)
            .expect("测试受限 MemoryObject handle 应分配成功")
    };

    let mut query = native_call(2, memory);
    query.args[0] = query_address as u64;
    assert_eq!(invoke_native(&task, query).status, status::OK);
    let info: native_abi::wire::MemoryInfo = read_user_value(&task, query_address);
    assert_eq!(info.size, native_abi::PAGE_SIZE * 2);
    assert_eq!(info.kind, native_abi::wire::MEMORY_KIND_ANONYMOUS);
    assert_eq!(info.mapping_count, 0);
    assert_eq!(info.state, native_abi::wire::MEMORY_STATE_ACTIVE);

    let mut map = native_call(1, memory);
    map.args[0] = map_address as u64;
    let mut denied_map = map;
    denied_map.object_handle = map_only.raw();
    assert_eq!(
        invoke_native(&task, denied_map).status,
        status::SECURITY_RIGHTS_DENIED
    );
    let mapped = invoke_native(&task, map);
    assert_eq!(mapped.status, status::OK);
    assert_eq!(mapped.value1, native_abi::PAGE_SIZE);
    assert_ne!(mapped.value0, 0);

    let mut statistics = native_call(5, memory);
    statistics.args[0] = statistics_address as u64;
    assert_eq!(invoke_native(&task, statistics).status, status::OK);
    let snapshot: native_abi::wire::MemoryStatistics = read_user_value(&task, statistics_address);
    assert_eq!(snapshot.mapped_pages, 1);
    assert_eq!(snapshot.resident_mappings, 0);
    assert_eq!(snapshot.materialized_pages, 0);
    assert_eq!(snapshot.shared_resident_mappings, 0);

    unsafe {
        vm.with_user_write_slice(mapped.value0 as usize, 1, |window| window[0] = 0x5a)
            .expect("第一次访问应按需物化 MemoryObject 页");
    }
    assert_eq!(invoke_native(&task, statistics).status, status::OK);
    let snapshot: native_abi::wire::MemoryStatistics = read_user_value(&task, statistics_address);
    assert_eq!(snapshot.mapped_pages, 1);
    assert_eq!(snapshot.resident_mappings, 1);
    assert_eq!(snapshot.materialized_pages, 1);
    assert_eq!(snapshot.shared_resident_mappings, 0);

    assert_eq!(invoke_native(&task, query).status, status::OK);
    let info: native_abi::wire::MemoryInfo = read_user_value(&task, query_address);
    assert_eq!(info.mapping_count, 1);

    let mut unmap = native_call(3, address_space);
    unmap.args[0] = mapped.value0;
    unmap.args[1] = mapped.value1;
    assert_eq!(invoke_native(&task, unmap).status, status::OK);
    assert_eq!(invoke_native(&task, query).status, status::OK);
    let info: native_abi::wire::MemoryInfo = read_user_value(&task, query_address);
    assert_eq!(info.mapping_count, 0);

    let remapped = invoke_native(&task, map);
    assert_eq!(remapped.status, status::OK);
    let remapped_range = remapped.value0 as usize..(remapped.value0 + remapped.value1) as usize;
    let revoke = native_call(4, memory);
    let revoked = invoke_native(&task, revoke);
    assert_eq!(revoked.status, status::OK);
    assert_eq!(revoked.value0, 1);
    assert!(vm.contains_user_range(remapped_range).is_err());

    assert_eq!(invoke_native(&task, query).status, status::OK);
    let info: native_abi::wire::MemoryInfo = read_user_value(&task, query_address);
    assert_eq!(info.mapping_count, 0);
    assert_eq!(info.state, native_abi::wire::MEMORY_STATE_REVOKED);
    assert_eq!(info.generation, 2);
    assert_eq!(invoke_native(&task, revoke).status, status::MEMORY_REVOKED);
    assert_eq!(invoke_native(&task, map).status, status::MEMORY_REVOKED);
}

#[ktest]
fn memory_revoke_removes_mappings_from_every_native_process() {
    let (owner_task, owner_state) = make_native_task(
        alloc::vec![
            bound_slot(0, OperationId::MemoryCreate),
            bound_slot(1, OperationId::MemoryMap),
            bound_slot(2, OperationId::MemoryRevoke),
        ],
        empty_handles(),
    );
    let (peer_task, peer_state) = make_native_task(
        alloc::vec![bound_slot(0, OperationId::MemoryMap)],
        empty_handles(),
    );
    let owner_vm = task_vm(&owner_task);
    let peer_vm = task_vm(&peer_task);
    let (process, owner_address_space) = {
        let mut handles = owner_state.handles.lock();
        let process = handles
            .insert(
                KernelNativeObject::SelfProcess,
                ObjectInterface::Process,
                Rights::CREATE,
            )
            .expect("测试 Process handle 应分配成功");
        let address_space = handles
            .insert(
                KernelNativeObject::AddressSpace(Arc::clone(&owner_vm)),
                ObjectInterface::AddressSpace,
                Rights::ALLOCATE,
            )
            .expect("测试 owner AddressSpace handle 应分配成功");
        (process, address_space)
    };
    let peer_address_space = peer_state
        .handles
        .lock()
        .insert(
            KernelNativeObject::AddressSpace(Arc::clone(&peer_vm)),
            ObjectInterface::AddressSpace,
            Rights::ALLOCATE,
        )
        .expect("测试 peer AddressSpace handle 应分配成功");

    const CREATE: usize = 0x1000_0000;
    const OWNER_MAP: usize = 0x1000_1000;
    const PEER_MAP: usize = 0x1000_0000;
    install_user_value(
        &owner_task,
        CREATE,
        &native_abi::wire::MemoryCreateRequest {
            size: native_abi::PAGE_SIZE,
            alignment: native_abi::PAGE_SIZE,
            flags: native_abi::wire::MEMORY_FLAG_SHARED,
            kind: native_abi::wire::MEMORY_KIND_ANONYMOUS,
            source_handle: 0,
            source_offset: 0,
            reserved: [0; 3],
        },
    );
    let mut create = native_call(0, process);
    create.args[0] = CREATE as u64;
    let created = invoke_native(&owner_task, create);
    assert_eq!(created.status, status::OK);
    let owner_memory = NativeHandle::from_raw(created.value0);
    let shared_object = owner_state
        .handles
        .lock()
        .lookup(
            owner_memory,
            Some(ObjectInterface::MemoryObject),
            Rights::MAP | Rights::READ | Rights::WRITE,
        )
        .expect("测试 MemoryObject 应存在")
        .object
        .clone();
    let peer_memory = peer_state
        .handles
        .lock()
        .insert(
            shared_object,
            ObjectInterface::MemoryObject,
            Rights::MAP | Rights::READ | Rights::WRITE,
        )
        .expect("测试 peer MemoryObject handle 应分配成功");

    let map_request = |address_space: NativeHandle| native_abi::wire::MemoryMapRequest {
        address_space: address_space.raw(),
        offset: 0,
        length: native_abi::PAGE_SIZE,
        alignment: native_abi::PAGE_SIZE,
        address_hint: 0,
        permissions: native_abi::wire::MEMORY_PERMISSION_READ
            | native_abi::wire::MEMORY_PERMISSION_WRITE,
        flags: 0,
        reserved: [0; 2],
    };
    install_user_value(&owner_task, OWNER_MAP, &map_request(owner_address_space));
    install_user_value(&peer_task, PEER_MAP, &map_request(peer_address_space));
    let mut owner_map = native_call(1, owner_memory);
    owner_map.args[0] = OWNER_MAP as u64;
    let owner_mapped = invoke_native(&owner_task, owner_map);
    assert_eq!(owner_mapped.status, status::OK);
    let mut peer_map = native_call(0, peer_memory);
    peer_map.args[0] = PEER_MAP as u64;
    let peer_mapped = invoke_native(&peer_task, peer_map);
    assert_eq!(peer_mapped.status, status::OK);

    let revoked = invoke_native(&owner_task, native_call(2, owner_memory));
    assert_eq!(revoked.status, status::OK);
    assert_eq!(revoked.value0, 2);
    assert!(
        owner_vm
            .contains_user_range(
                owner_mapped.value0 as usize..(owner_mapped.value0 + owner_mapped.value1) as usize,
            )
            .is_err()
    );
    assert!(
        peer_vm
            .contains_user_range(
                peer_mapped.value0 as usize..(peer_mapped.value0 + peer_mapped.value1) as usize,
            )
            .is_err()
    );
}

#[ktest]
fn memory_mapping_keeps_object_alive_after_handle_close() {
    let (task, state) = make_native_task(
        alloc::vec![
            bound_slot(0, OperationId::MemoryCreate),
            bound_slot(1, OperationId::MemoryMap),
            bound_slot(2, OperationId::MemoryUnmap),
        ],
        empty_handles(),
    );
    let vm = task_vm(&task);
    let (process, address_space) = {
        let mut handles = state.handles.lock();
        let process = handles
            .insert(
                KernelNativeObject::SelfProcess,
                ObjectInterface::Process,
                Rights::CREATE,
            )
            .expect("测试 Process handle 应分配成功");
        let address_space = handles
            .insert(
                KernelNativeObject::AddressSpace(Arc::clone(&vm)),
                ObjectInterface::AddressSpace,
                Rights::ALLOCATE | Rights::FREE,
            )
            .expect("测试 AddressSpace handle 应分配成功");
        (process, address_space)
    };

    const CREATE: usize = 0x1000_0000;
    const MAP: usize = 0x1000_1000;
    install_user_value(
        &task,
        CREATE,
        &native_abi::wire::MemoryCreateRequest {
            size: native_abi::PAGE_SIZE,
            alignment: native_abi::PAGE_SIZE,
            flags: native_abi::wire::MEMORY_FLAG_SHARED,
            kind: native_abi::wire::MEMORY_KIND_ANONYMOUS,
            source_handle: 0,
            source_offset: 0,
            reserved: [0; 3],
        },
    );
    install_user_value(
        &task,
        MAP,
        &native_abi::wire::MemoryMapRequest {
            address_space: address_space.raw(),
            offset: 0,
            length: native_abi::PAGE_SIZE,
            alignment: native_abi::PAGE_SIZE,
            address_hint: 0,
            permissions: native_abi::wire::MEMORY_PERMISSION_READ
                | native_abi::wire::MEMORY_PERMISSION_WRITE,
            flags: 0,
            reserved: [0; 2],
        },
    );

    let mut create = native_call(0, process);
    create.args[0] = CREATE as u64;
    let created = invoke_native(&task, create);
    assert_eq!(created.status, status::OK);
    let memory = NativeHandle::from_raw(created.value0);
    let mut map = native_call(1, memory);
    map.args[0] = MAP as u64;
    let mapped = invoke_native(&task, map);
    assert_eq!(mapped.status, status::OK);

    let closed = state
        .handles
        .lock()
        .close(memory)
        .expect("MemoryObject handle 应可关闭");
    drop(closed);

    let mut unmap = native_call(2, address_space);
    unmap.args[0] = mapped.value0;
    unmap.args[1] = mapped.value1;
    assert_eq!(invoke_native(&task, unmap).status, status::OK);
    assert!(
        vm.contains_user_range(mapped.value0 as usize..(mapped.value0 + mapped.value1) as usize,)
            .is_err()
    );
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
fn channel_receive_fault_restores_the_message() {
    let (task, state) = make_native_task(
        alloc::vec![
            bound_slot(0, OperationId::ChannelCreate),
            bound_slot(1, OperationId::ChannelSend),
            bound_slot(2, OperationId::ChannelReceive),
        ],
        empty_handles(),
    );
    let process = state
        .handles
        .lock()
        .insert(
            KernelNativeObject::SelfProcess,
            ObjectInterface::Process,
            Rights::CREATE,
        )
        .expect("测试 Process handle 应分配成功");
    let created = {
        let mut call = native_call(0, process);
        call.args[0] = 1;
        invoke_native(&task, call)
    };
    assert_eq!(created.status, status::OK);
    let sender = NativeHandle::from_raw(created.value0);
    let receiver = NativeHandle::from_raw(created.value1);

    const BASE: usize = 0x1000_0000;
    const SEND_DATA: usize = BASE;
    const SEND_MESSAGE: usize = BASE + 0x1000;
    const RECEIVE_DATA: usize = BASE + 0x2000;
    const RECEIVE_MESSAGE: usize = BASE + 0x3000;
    install_user_value(&task, SEND_DATA, &0x4433_2211u32);
    install_user_value(
        &task,
        SEND_MESSAGE,
        &native_abi::wire::ChannelMessage {
            data_ptr: SEND_DATA as u64,
            data_size: 4,
            data_capacity: 4,
            handles_ptr: 0,
            handle_count: 0,
            handle_capacity: 0,
            flags: 0,
            reserved: [0; 3],
        },
    );
    let mut send = native_call(1, sender);
    send.args[0] = SEND_MESSAGE as u64;
    assert_eq!(invoke_native(&task, send).status, status::OK);

    install_user_value(&task, RECEIVE_DATA, &0u32);
    install_user_value(
        &task,
        RECEIVE_MESSAGE,
        &native_abi::wire::ChannelMessage {
            data_ptr: u64::MAX,
            data_capacity: 4,
            ..native_abi::wire::ChannelMessage::default()
        },
    );
    let mut receive = native_call(2, receiver);
    receive.args[0] = RECEIVE_MESSAGE as u64;
    assert_eq!(invoke_native(&task, receive).status, status::STREAM_FAULT);

    write_user_value(
        &task,
        RECEIVE_MESSAGE,
        &native_abi::wire::ChannelMessage {
            data_ptr: RECEIVE_DATA as u64,
            data_capacity: 4,
            ..native_abi::wire::ChannelMessage::default()
        },
    );
    let received = invoke_native(&task, receive);
    assert_eq!(received.status, status::OK);
    assert_eq!(received.value0, 4);
    assert_eq!(read_user_value::<u32>(&task, RECEIVE_DATA), 0x4433_2211);
}

#[ktest]
fn stream_write_reports_user_buffer_fault_before_progress() {
    let (task, _state, _read, stream) = make_stream_task(Rights::WRITE, true);
    let mut call = native_call(0, stream);
    call.args[0] = 0x1000_0000;
    call.args[1] = 4;

    assert_eq!(invoke_native(&task, call).status, status::STREAM_FAULT);
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

    assert_eq!(
        invoke_native(&task, call).status,
        status::STREAM_WOULD_BLOCK
    );
}

#[ktest]
fn stream_write_maps_closed_peer_without_reusing_user_fault() {
    let (task, _state, read, stream) = make_stream_task(Rights::WRITE, true);
    drop(read);
    let user = install_user_bytes(&task, &[0x5a; 16]);
    let mut call = native_call(0, stream);
    call.args[0] = user as u64;
    call.args[1] = 16;

    assert_eq!(invoke_native(&task, call).status, status::STREAM_CLOSED);
}

#[ktest]
fn stream_write_maps_general_io_error_without_reusing_user_fault() {
    assert_native_return(
        map_stream_write_error(general::vfs::error::VfsError::Io, 0),
        status::STREAM_ERROR,
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

#[ktest]
fn stream_read_zero_length_does_not_touch_the_pointer() {
    let (task, _state, _write, stream) = make_read_stream_task(Rights::READ, true);
    let mut call = native_call(0, stream);
    call.args[0] = usize::MAX as u64;
    assert_native_return(dispatch_native_call(&task, call), status::OK, 0);
}

#[ktest]
fn stream_read_copies_available_bytes_and_reports_eof() {
    let (task, _state, write, stream) = make_read_stream_task(Rights::READ, true);
    write.write(b"hello").expect("测试 pipe 写入应成功");
    let user = install_user_bytes(&task, &[0; 8]);
    let mut call = native_call(0, stream);
    call.args[0] = user as u64;
    call.args[1] = 8;

    assert_native_return(dispatch_native_call(&task, call), status::OK, 5);
    assert_eq!(read_user_bytes(&task, user, 5), b"hello");

    drop(write);
    let user = install_user_bytes(&task, &[0; 1]);
    let mut call = native_call(0, stream);
    call.args[0] = user as u64;
    call.args[1] = 1;
    assert_native_return(dispatch_native_call(&task, call), status::STREAM_END, 0);
}

#[ktest]
fn stream_read_returns_after_first_positive_progress() {
    assert_native_return(stream_read_progress(0, 5), status::OK, 5);
}

#[ktest]
fn blocking_stream_read_returns_a_short_read_while_writer_remains_open() {
    let (task, _state, write, stream) = make_read_stream_task(Rights::READ, false);
    write.write(b"hello").expect("测试 pipe 写入应成功");
    let user = install_user_bytes(&task, &[0; 8]);
    let mut call = native_call(0, stream);
    call.args[0] = user as u64;
    call.args[1] = 8;

    assert_native_return(dispatch_native_call(&task, call), status::OK, 5);
    assert_eq!(read_user_bytes(&task, user, 5), b"hello");
}

#[ktest]
fn stream_read_maps_nonblocking_empty_pipe_and_user_fault() {
    let (task, _state, _write, stream) = make_read_stream_task(Rights::READ, true);
    let user = install_user_bytes(&task, &[0; 4]);
    let mut call = native_call(0, stream);
    call.args[0] = user as u64;
    call.args[1] = 4;
    assert_native_return(
        dispatch_native_call(&task, call),
        status::STREAM_WOULD_BLOCK,
        0,
    );

    let (task, _state, _write, stream) = make_read_stream_task(Rights::READ, true);
    let mut call = native_call(0, stream);
    call.args[0] = 0;
    call.args[1] = 4;
    assert_native_return(dispatch_native_call(&task, call), status::STREAM_FAULT, 0);
}

#[ktest]
fn stream_read_unwinds_when_group_exit_rejects_sleep() {
    let (task, _state, _write, stream) = make_read_stream_task(Rights::READ, false);
    assert!(task.cas_state(sched::TaskState::New, sched::TaskState::Running));
    let user = install_user_bytes(&task, &[0; 8]);
    let mut call = native_call(0, stream);
    call.args[0] = user as u64;
    call.args[1] = 8;
    assert_eq!(task.thread_group().request_group_exit(37), 37);
    sched::group_exit_wakeup(&task);

    assert!(matches!(
        dispatch_native_call(&task, call),
        NativeCallOutcome::RetryExternalControl
    ));
}

#[ktest]
fn stream_read_maps_general_errors_without_reusing_user_fault() {
    assert_native_return(
        map_stream_read_error(general::vfs::error::VfsError::Io, 0),
        status::STREAM_ERROR,
        0,
    );
    assert_native_return(
        map_stream_read_error(general::vfs::error::VfsError::Io, 7),
        status::OK,
        7,
    );
}

#[ktest]
fn file_open_access_ignores_non_io_rights() {
    assert_eq!(
        super::fs::file_access_mode(Rights::READ | Rights::MAP),
        vfs::file::AccessMode::ReadOnly
    );
    assert_eq!(
        super::fs::file_access_mode(Rights::READ | Rights::WRITE | Rights::MAP),
        vfs::file::AccessMode::ReadWrite
    );
}

#[ktest]
fn file_io_rejects_unknown_flags() {
    assert_eq!(super::fs::validate_file_io_flags(0), Ok(()));
    assert_eq!(
        super::fs::validate_file_io_flags(1),
        Err(status::CORE_INVALID_ARGUMENT)
    );
}

#[ktest]
fn ring_batch_validation_does_not_consume_any_submission_on_failure() {
    let (task, _state, ring, clock, shared) = make_ring_task(4);
    write_ring_submission(&task, shared, 0, ring_clock_submission(clock, 11));
    let mut invalid = ring_clock_submission(clock, 12);
    invalid.slot = u64::MAX;
    write_ring_submission(&task, shared, 1, invalid);
    write_ring_index(
        &task,
        shared,
        native_abi::wire::ring_shared_state::SQ_TAIL,
        2,
    );

    let mut kick = native_call(1, ring);
    kick.args[0] = 2;
    assert_eq!(
        invoke_native(&task, kick).status,
        status::RING_INVALID_DESCRIPTOR
    );
    assert_eq!(
        read_ring_index(&task, shared, native_abi::wire::ring_shared_state::SQ_HEAD),
        0
    );

    let mut cancel = native_call(2, ring);
    cancel.args[0] = 11;
    assert_eq!(invoke_native(&task, cancel).status, status::RING_NOT_FOUND);
}

#[ktest]
fn ring_rejects_direct_only_operations_before_consuming_the_batch() {
    let (task, state, ring, _clock, shared) = make_ring_task(4);
    let process = state
        .handles
        .lock()
        .insert(
            KernelNativeObject::SelfProcess,
            ObjectInterface::Process,
            Rights::EXIT,
        )
        .expect("测试 Process handle 应分配成功");
    write_ring_submission(
        &task,
        shared,
        0,
        native_abi::wire::SubmissionDescriptor {
            slot: 9,
            handle: process.raw(),
            arg0: 0,
            arg1: 0,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            user_data: 15,
        },
    );
    write_ring_index(
        &task,
        shared,
        native_abi::wire::ring_shared_state::SQ_TAIL,
        1,
    );
    let mut kick = native_call(1, ring);
    kick.args[0] = 1;
    assert_eq!(invoke_native(&task, kick).status, status::RING_UNSUPPORTED);
    assert_eq!(
        read_ring_index(&task, shared, native_abi::wire::ring_shared_state::SQ_HEAD),
        0
    );
}

#[ktest]
fn ring_rejects_nonzero_unused_inline_arguments() {
    let (task, _state, ring, clock, shared) = make_ring_task(4);
    let mut submission = ring_clock_submission(clock, 17);
    submission.arg0 = 1;
    write_ring_submission(&task, shared, 0, submission);
    write_ring_index(
        &task,
        shared,
        native_abi::wire::ring_shared_state::SQ_TAIL,
        1,
    );
    let mut kick = native_call(1, ring);
    kick.args[0] = 1;
    assert_eq!(
        invoke_native(&task, kick).status,
        status::RING_INVALID_DESCRIPTOR
    );
    assert_eq!(
        read_ring_index(&task, shared, native_abi::wire::ring_shared_state::SQ_HEAD),
        0
    );
}

#[ktest]
fn ring_cq_backpressure_keeps_the_submission_queue_untouched() {
    let (task, _state, ring, clock, shared) = make_ring_task(4);
    write_ring_submission(&task, shared, 0, ring_clock_submission(clock, 21));
    write_ring_index(
        &task,
        shared,
        native_abi::wire::ring_shared_state::SQ_TAIL,
        1,
    );
    write_ring_index(
        &task,
        shared,
        native_abi::wire::ring_shared_state::CQ_TAIL,
        4,
    );

    let mut kick = native_call(1, ring);
    kick.args[0] = 1;
    assert_eq!(invoke_native(&task, kick).status, status::RING_FULL);
    assert_eq!(
        read_ring_index(&task, shared, native_abi::wire::ring_shared_state::SQ_HEAD),
        0
    );
}

#[ktest]
fn ring_wait_rejects_an_unreachable_minimum() {
    let (task, _state, ring, _clock, _shared) = make_ring_task(4);
    let mut wait = native_call(8, ring);
    wait.args[0] = 5;
    assert_eq!(
        invoke_native(&task, wait).status,
        status::CORE_INVALID_ARGUMENT
    );
}

#[ktest]
fn ring_poll_readiness_tracks_shared_cq_consumption() {
    let (task, state, ring, _clock, shared) = make_ring_task(4);
    let object = state
        .handles
        .lock()
        .lookup(ring, Some(ObjectInterface::SubmissionRing), Rights::OBSERVE)
        .expect("测试 Ring handle 应可查找")
        .object
        .clone();
    let KernelNativeObject::SubmissionRing(object) = object else {
        panic!("测试对象必须是 SubmissionRing");
    };

    write_ring_index(
        &task,
        shared,
        native_abi::wire::ring_shared_state::CQ_TAIL,
        1,
    );
    assert!(
        object
            .poll_source()
            .snapshot()
            .0
            .has(vfs::file::PollEvents::POLLIN)
    );

    write_ring_index(
        &task,
        shared,
        native_abi::wire::ring_shared_state::CQ_HEAD,
        1,
    );
    assert!(object.poll_source().snapshot().0.is_empty());
}

#[ktest]
fn ring_cancel_publishes_one_completion_and_reserves_user_data_until_consumed() {
    let (task, state, ring, clock, shared) = make_ring_task(4);
    let object = state
        .handles
        .lock()
        .lookup(ring, Some(ObjectInterface::SubmissionRing), Rights::CANCEL)
        .expect("测试 Ring handle 应可查找")
        .object
        .clone();
    let KernelNativeObject::SubmissionRing(object) = object else {
        panic!("测试对象必须是 SubmissionRing");
    };
    super::ring::pause_worker_for_test(&object);
    write_ring_submission(&task, shared, 0, ring_clock_submission(clock, 31));
    write_ring_index(
        &task,
        shared,
        native_abi::wire::ring_shared_state::SQ_TAIL,
        1,
    );
    let mut kick = native_call(1, ring);
    kick.args[0] = 1;
    assert_eq!(invoke_native(&task, kick).status, status::OK);

    let mut cancel = native_call(2, ring);
    cancel.args[0] = 31;
    assert_eq!(invoke_native(&task, cancel).status, status::OK);
    assert_eq!(invoke_native(&task, cancel).status, status::RING_NOT_FOUND);
    assert_eq!(
        read_ring_index(&task, shared, native_abi::wire::ring_shared_state::CQ_TAIL),
        1
    );
    let completion = read_ring_completion(&task, shared, 0);
    assert_eq!(completion.user_data, 31);
    assert_eq!(completion.status, status::RING_CANCELLED);
    assert_eq!(completion.reserved, 0);

    write_ring_submission(&task, shared, 1, ring_clock_submission(clock, 31));
    write_ring_index(
        &task,
        shared,
        native_abi::wire::ring_shared_state::SQ_TAIL,
        2,
    );
    assert_eq!(
        invoke_native(&task, kick).status,
        status::RING_INVALID_DESCRIPTOR
    );
    assert_eq!(
        read_ring_index(&task, shared, native_abi::wire::ring_shared_state::SQ_HEAD),
        1
    );
}

#[ktest]
fn ring_rejects_a_registration_after_its_memory_generation_changes() {
    let (task, state, ring, _clock, shared) = make_ring_task(4);
    const REQUEST: usize = 0x1000_0000;
    install_user_value(
        &task,
        REQUEST,
        &native_abi::wire::MemoryCreateRequest {
            size: native_abi::PAGE_SIZE,
            alignment: native_abi::PAGE_SIZE,
            flags: native_abi::wire::MEMORY_FLAG_SHARED,
            kind: native_abi::wire::MEMORY_KIND_ANONYMOUS,
            source_handle: 0,
            source_offset: 0,
            reserved: [0; 3],
        },
    );
    let process = state
        .handles
        .lock()
        .insert(
            KernelNativeObject::SelfProcess,
            ObjectInterface::Process,
            Rights::CREATE,
        )
        .expect("测试 Process handle 应分配成功");
    let mut create = native_call(4, process);
    create.args[0] = REQUEST as u64;
    let memory = NativeHandle::from_raw(invoke_native(&task, create).value0);

    let mut register = native_call(3, ring);
    register.args = [memory.raw(), 0, native_abi::PAGE_SIZE, 0, 0];
    let token = invoke_native(&task, register).value0;
    assert_ne!(token, 0);
    assert_eq!(
        invoke_native(&task, native_call(5, memory)).status,
        status::OK
    );

    let (read, _write) =
        general::vfs::pipe::new_pipe(Arc::new(general::vfs::Credentials::root()), true)
            .expect("测试 pipe 应创建成功");
    let stream = state
        .handles
        .lock()
        .insert(
            KernelNativeObject::Stream(read),
            ObjectInterface::Stream,
            Rights::READ,
        )
        .expect("测试 Stream handle 应分配成功");
    write_ring_submission(
        &task,
        shared,
        0,
        native_abi::wire::SubmissionDescriptor {
            slot: 6,
            handle: stream.raw(),
            arg0: token,
            arg1: 0,
            arg2: 1,
            arg3: 0,
            arg4: 0,
            user_data: 41,
        },
    );
    write_ring_index(
        &task,
        shared,
        native_abi::wire::ring_shared_state::SQ_TAIL,
        1,
    );
    let mut kick = native_call(1, ring);
    kick.args[0] = 1;
    assert_eq!(
        invoke_native(&task, kick).status,
        status::RING_INVALID_DESCRIPTOR
    );
    assert_eq!(
        read_ring_index(&task, shared, native_abi::wire::ring_shared_state::SQ_HEAD),
        0
    );
}

#[ktest]
fn process_result_preserves_full_exit_code_and_fault_details_after_reap() {
    let group = ThreadGroup::new();
    group.record_native_fault(sched::group::NativeFaultInfo {
        kind: native_abi::wire::PROCESS_FAULT_MEMORY,
        exception_code: 13,
        address: 0xfeed_cafe,
    });
    assert_eq!(
        group.request_group_exit(0x8765_4321u32 as i32),
        0x8765_4321u32 as i32
    );

    let faulted = super::process::process_result(&group, false);
    assert_eq!(faulted.state, native_abi::wire::PROCESS_STATE_FAULTED);
    assert_eq!(faulted.exit_code, 0x8765_4321);
    assert_eq!(faulted.fault_kind, native_abi::wire::PROCESS_FAULT_MEMORY);
    assert_eq!(faulted.detail0, 13);
    assert_eq!(faulted.detail1, 0xfeed_cafe);

    let reaped = super::process::process_result(&group, true);
    assert_eq!(reaped.state, native_abi::wire::PROCESS_STATE_REAPED);
    assert_eq!(reaped.exit_code, faulted.exit_code);
    assert_eq!(reaped.fault_kind, faulted.fault_kind);
    assert_eq!(reaped.detail0, faulted.detail0);
    assert_eq!(reaped.detail1, faulted.detail1);
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
    let vm = Arc::new(VmSpace::new());
    let handles = Arc::new(sched::sync::Spinlock::new(handles));
    let components =
        super::component::ComponentManager::new(Arc::clone(&vm), &binding, Arc::clone(&handles))
            .expect("测试 component manager 应创建成功");
    let state = Arc::new(NativeProcessState {
        binding,
        handles,
        build_id: [0; 32],
        content_hash: [0; 32],
        image_base: 0,
        components,
        vfs_context: None,
        runtime_ranges: sched::sync::Spinlock::new(None),
        allocations: sched::sync::Spinlock::new(alloc::vec::Vec::new()),
        memory_owner_id: super::next_memory_owner_id(),
        mapped_memory_objects: Arc::new(super::memory::MemoryMappingRegistry::new()),
    });
    task.ext_install(sched::TASKEXT_VM_SPACE, vm);
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
        NativeCallOutcome::ExitThread(_) => panic!("测试调用不应退出线程"),
        NativeCallOutcome::FrameFinalized => panic!("测试调用不应替换用户 frame"),
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

fn make_read_stream_task(
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
            KernelNativeObject::Stream(read),
            ObjectInterface::Stream,
            rights,
        )
        .expect("stream handle 应分配成功");
    let (task, state) =
        make_native_task(alloc::vec![bound_slot(0, OperationId::StreamRead)], handles);
    (task, state, write, handle)
}

fn make_address_space_task() -> (Arc<Task>, Arc<VmSpace>, NativeHandle) {
    let vm = Arc::new(VmSpace::new());
    let mut handles = empty_handles();
    let handle = handles
        .insert(
            KernelNativeObject::AddressSpace(Arc::clone(&vm)),
            ObjectInterface::AddressSpace,
            Rights::ALLOCATE | Rights::FREE,
        )
        .expect("AddressSpace handle 应分配成功");
    let (task, state) = make_native_task(
        alloc::vec![
            bound_slot(0, OperationId::MemoryAllocate),
            bound_slot(1, OperationId::MemoryFree),
        ],
        handles,
    );
    let layout = general::mm::user_vm_layout().expect("架构必须注册用户 VM 布局");
    let page_size = native_abi::PAGE_SIZE as usize;
    vm.map_anon(
        page_size..page_size * 2,
        VmFlags::EMPTY.with(VmFlags::READ).with(VmFlags::USER),
    )
    .expect("测试 StartInfo 保护区应真实映射");
    state.install_runtime_ranges(
        layout.default_stack_top - page_size..layout.default_stack_top,
        page_size..page_size * 2,
        None,
    );
    (task, vm, handle)
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

fn read_user_bytes(task: &Arc<Task>, address: usize, len: usize) -> Vec<u8> {
    let vm = task
        .ext_lookup(sched::TASKEXT_VM_SPACE)
        .and_then(|payload| payload.downcast::<VmSpace>().ok())
        .expect("测试 task 应有 VM");
    unsafe {
        vm.with_user_read_slice(address, len, |bytes| bytes.to_vec())
            .expect("测试用户缓冲应可读取")
    }
}

fn task_vm(task: &Arc<Task>) -> Arc<VmSpace> {
    task.ext_lookup(sched::TASKEXT_VM_SPACE)
        .and_then(|payload| payload.downcast::<VmSpace>().ok())
        .expect("测试 task 应有 VM")
}

fn install_user_value<T: Copy>(task: &Arc<Task>, address: usize, value: &T) {
    let vm = task_vm(task);
    let page_size = general::mm::page_size();
    let end = address
        .checked_add(page_size)
        .expect("测试用户映射不得溢出");
    vm.map_anon(
        address..end,
        VmFlags::EMPTY
            .with(VmFlags::READ)
            .with(VmFlags::WRITE)
            .with(VmFlags::USER),
    )
    .expect("测试用户值应映射成功");
    let bytes = unsafe {
        core::slice::from_raw_parts(value as *const T as *const u8, core::mem::size_of::<T>())
    };
    unsafe {
        vm.with_user_write_slice(address, bytes.len(), |window| {
            window.copy_from_slice(bytes);
        })
        .expect("测试用户值应写入成功");
    }
}

fn read_user_value<T: Copy>(task: &Arc<Task>, address: usize) -> T {
    let vm = task_vm(task);
    let mut value = core::mem::MaybeUninit::<T>::uninit();
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(value.as_mut_ptr() as *mut u8, core::mem::size_of::<T>())
    };
    unsafe {
        vm.with_user_read_slice(address, bytes.len(), |window| {
            bytes.copy_from_slice(window);
        })
        .expect("测试用户值应读取成功");
        value.assume_init()
    }
}

fn write_user_value<T: Copy>(task: &Arc<Task>, address: usize, value: &T) {
    let vm = task_vm(task);
    let bytes = unsafe {
        core::slice::from_raw_parts(value as *const T as *const u8, core::mem::size_of::<T>())
    };
    unsafe {
        vm.with_user_write_slice(address, bytes.len(), |window| {
            window.copy_from_slice(bytes);
        })
        .expect("测试用户值应写入既有映射");
    }
}

fn make_ring_task(
    entries: u32,
) -> (
    Arc<Task>,
    Arc<NativeProcessState>,
    NativeHandle,
    NativeHandle,
    usize,
) {
    let (task, state) = make_native_task(
        alloc::vec![
            bound_slot(0, OperationId::RingCreate),
            bound_slot(1, OperationId::RingKick),
            bound_slot(2, OperationId::RingCancel),
            bound_slot(3, OperationId::RingRegister),
            bound_slot(4, OperationId::MemoryCreate),
            bound_slot(5, OperationId::MemoryRevoke),
            bound_slot(6, OperationId::StreamRead),
            bound_slot(7, OperationId::ClockRead),
            bound_slot(8, OperationId::RingWait),
            bound_slot(9, OperationId::ProcessExit),
        ],
        empty_handles(),
    );
    let (process, clock) = {
        let mut handles = state.handles.lock();
        let process = handles
            .insert(
                KernelNativeObject::SelfProcess,
                ObjectInterface::Process,
                Rights::CREATE,
            )
            .expect("测试 Process handle 应分配成功");
        let clock = handles
            .insert(
                KernelNativeObject::MonotonicClock,
                ObjectInterface::Clock,
                Rights::READ,
            )
            .expect("测试 Clock handle 应分配成功");
        (process, clock)
    };
    let mut create = native_call(0, process);
    create.args[0] = u64::from(entries);
    let created = invoke_native(&task, create);
    assert_eq!(created.status, status::OK);
    assert_ne!(created.value1, 0);
    (
        task,
        state,
        NativeHandle::from_raw(created.value0),
        clock,
        created.value1 as usize,
    )
}

fn ring_clock_submission(
    clock: NativeHandle,
    user_data: u64,
) -> native_abi::wire::SubmissionDescriptor {
    native_abi::wire::SubmissionDescriptor {
        slot: 7,
        handle: clock.raw(),
        arg0: 0,
        arg1: 0,
        arg2: 0,
        arg3: 0,
        arg4: 0,
        user_data,
    }
}

fn write_ring_submission(
    task: &Arc<Task>,
    shared: usize,
    position: u32,
    submission: native_abi::wire::SubmissionDescriptor,
) {
    let header: native_abi::wire::RingSharedState = read_user_value(task, shared);
    let index = position & header.mask;
    let address = shared
        + header.sq_offset as usize
        + index as usize * core::mem::size_of::<native_abi::wire::SubmissionDescriptor>();
    write_user_value(task, address, &submission);
}

fn read_ring_completion(
    task: &Arc<Task>,
    shared: usize,
    position: u32,
) -> native_abi::wire::CompletionRecord {
    let header: native_abi::wire::RingSharedState = read_user_value(task, shared);
    let index = position & header.mask;
    let address = shared
        + header.cq_offset as usize
        + index as usize * core::mem::size_of::<native_abi::wire::CompletionRecord>();
    read_user_value(task, address)
}

fn write_ring_index(task: &Arc<Task>, shared: usize, offset: usize, value: u32) {
    task_vm(task)
        .store_user_u32_nofault(shared + offset, value)
        .expect("测试 Ring index 应可写入");
}

fn read_ring_index(task: &Arc<Task>, shared: usize, offset: usize) -> u32 {
    task_vm(task)
        .read_user_u32_nofault(shared + offset)
        .expect("测试 Ring index 应可读取")
}

#[cfg(target_arch = "riscv64")]
fn arch_trap_frame() -> arch::riscv64::trap_frame::TrapFrame {
    arch::riscv64::trap_frame::TrapFrame::default()
}

#[cfg(target_arch = "loongarch64")]
fn arch_trap_frame() -> arch::loongarch64::TrapFrame {
    arch::loongarch64::TrapFrame::default()
}

#[cfg(target_arch = "x86_64")]
fn arch_trap_frame() -> arch::x86_64::trap_frame::TrapFrame {
    arch::x86_64::trap_frame::TrapFrame::default()
}

#[cfg(target_arch = "riscv64")]
fn set_frame_pc(frame: &mut arch::riscv64::trap_frame::TrapFrame, pc: usize) {
    frame.sepc = pc;
}

#[cfg(target_arch = "loongarch64")]
fn set_frame_pc(frame: &mut arch::loongarch64::TrapFrame, pc: usize) {
    frame.pc = pc;
}

#[cfg(target_arch = "x86_64")]
fn set_frame_pc(frame: &mut arch::x86_64::trap_frame::TrapFrame, pc: usize) {
    frame.rip = pc;
}

#[cfg(target_arch = "riscv64")]
fn frame_pc(frame: &arch::riscv64::trap_frame::TrapFrame) -> usize {
    frame.sepc
}

#[cfg(target_arch = "loongarch64")]
fn frame_pc(frame: &arch::loongarch64::TrapFrame) -> usize {
    frame.pc
}

#[cfg(target_arch = "x86_64")]
fn frame_pc(frame: &arch::x86_64::trap_frame::TrapFrame) -> usize {
    frame.rip
}

#[cfg(target_arch = "riscv64")]
fn configure_native_call_frame(frame: &mut arch::riscv64::trap_frame::TrapFrame) {
    frame.a7 = 7;
    frame.a6 = 0x0000_0002_0000_0003;
    frame.a0 = 10;
    frame.a1 = 11;
    frame.a2 = 12;
    frame.a3 = 13;
    frame.a4 = 14;
    frame.a5 = 0xfeed;
}

#[cfg(target_arch = "loongarch64")]
fn configure_native_call_frame(frame: &mut arch::loongarch64::TrapFrame) {
    frame.a7 = 7;
    frame.a6 = 0x0000_0002_0000_0003;
    frame.a0 = 10;
    frame.a1 = 11;
    frame.a2 = 12;
    frame.a3 = 13;
    frame.a4 = 14;
    frame.a5 = 0xfeed;
}

#[cfg(target_arch = "x86_64")]
fn configure_native_call_frame(frame: &mut arch::x86_64::trap_frame::TrapFrame) {
    frame.rax = 7;
    frame.rdi = 0x0000_0002_0000_0003;
    frame.rsi = 10;
    frame.rdx = 11;
    frame.r10 = 12;
    frame.r8 = 13;
    frame.r9 = 14;
    frame.rbx = 0xfeed;
}

#[cfg(target_arch = "riscv64")]
fn set_native_invalid_slot(frame: &mut arch::riscv64::trap_frame::TrapFrame) {
    frame.a7 = usize::MAX;
}

#[cfg(target_arch = "loongarch64")]
fn set_native_invalid_slot(frame: &mut arch::loongarch64::TrapFrame) {
    frame.a7 = usize::MAX;
}

#[cfg(target_arch = "x86_64")]
fn set_native_invalid_slot(frame: &mut arch::x86_64::trap_frame::TrapFrame) {
    frame.rax = usize::MAX;
}

#[cfg(target_arch = "riscv64")]
fn native_return_values(frame: &arch::riscv64::trap_frame::TrapFrame) -> (usize, usize, usize) {
    (frame.a0, frame.a1, frame.a2)
}

#[cfg(target_arch = "loongarch64")]
fn native_return_values(frame: &arch::loongarch64::TrapFrame) -> (usize, usize, usize) {
    (frame.a0, frame.a1, frame.a2)
}

#[cfg(target_arch = "x86_64")]
fn native_return_values(frame: &arch::x86_64::trap_frame::TrapFrame) -> (usize, usize, usize) {
    (frame.rax, frame.rdx, frame.r10)
}

#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
const fn native_next_pc(pc: usize) -> usize {
    pc + 4
}

#[cfg(target_arch = "x86_64")]
const fn native_next_pc(pc: usize) -> usize {
    pc + arch::x86_64::trap_frame::TrapFrame::SYSCALL_INSN_LEN
}

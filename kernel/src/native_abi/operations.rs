//! MyGO Native operation 的内核对象执行路径。

use alloc::sync::Arc;
use core::ops::Range;

use errno::Errno;
use general::mm::VmSpace;
use general::syscall::{NativeCallFrame, NativeCallOutcome};
use general::vfs::error::VfsError;
use general::vfs::file::{File, PollEvents};
use mm::VmFlags;
use native_abi::{NativeHandle, ObjectInterface, OperationId, Rights};
use sched::UserContextRef;

use super::dispatch::native_return;
use super::{KernelNativeObject, NativeProcessState};

pub(super) struct PinnedNativeHandle {
    pub(super) object: KernelNativeObject,
    pub(super) interface: ObjectInterface,
    pub(super) rights: Rights,
}

pub(super) fn execute_native_operation(
    task: &Arc<sched::Task>,
    state: &Arc<NativeProcessState>,
    operation: OperationId,
    handle: NativeHandle,
    pinned: PinnedNativeHandle,
    call: NativeCallFrame,
    user_context: UserContextRef,
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
        OperationId::ThreadExit => {
            if !matches!(pinned.object, KernelNativeObject::SelfProcess)
                || call.args[0] > u32::MAX as u64
            {
                return native_return(native_abi::status::CORE_INVALID_ARGUMENT, 0, 0);
            }
            NativeCallOutcome::ExitThread(call.args[0] as u32 as i32)
        }
        OperationId::ThreadYield => {
            if !matches!(pinned.object, KernelNativeObject::SelfProcess) {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            }
            match sched::operation::sched_yield() {
                Ok(()) => native_return(native_abi::status::OK, 0, 0),
                Err(_) => native_return(native_abi::status::THREAD_INVALID, 0, 0),
            }
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
        OperationId::StreamRead => {
            let KernelNativeObject::Stream(file) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            stream_read(task, &file, call.args[0], call.args[1])
        }
        OperationId::ClockRead => {
            if !matches!(pinned.object, KernelNativeObject::MonotonicClock) {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            }
            native_return(native_abi::status::OK, hal::time::monotonic_ns(), 0)
        }
        OperationId::MemoryAllocate => {
            let KernelNativeObject::AddressSpace(vm) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            memory_allocate(state, &vm, call.args[0], call.args[1])
        }
        OperationId::MemoryFree => {
            let KernelNativeObject::AddressSpace(vm) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            memory_free(state, &vm, call.args[0], call.args[1])
        }
        OperationId::ImageCreate => {
            if !matches!(pinned.object, KernelNativeObject::SelfProcess) {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            }
            super::process::image_create(state, call.args[0], call.args[1])
        }
        OperationId::ImageQuery => {
            let KernelNativeObject::Image(image) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::image::image_query(&image, call.args[0])
        }
        OperationId::ProcessSpawn => {
            if !matches!(pinned.object, KernelNativeObject::SelfProcess) {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            }
            super::process::process_spawn(task, state, call.args[0], call.args[1])
        }
        OperationId::ProcessReplace => {
            if !matches!(pinned.object, KernelNativeObject::SelfProcess) {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            }
            super::process::process_replace(task, state, call.args[0], call.args[1], user_context)
        }
        OperationId::ProcessQuery => match pinned.object {
            KernelNativeObject::Process(process) => {
                super::process::process_query(&process, call.args[0])
            }
            KernelNativeObject::SelfProcess => {
                super::process::process_query_self(task, call.args[0])
            }
            _ => native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0),
        },
        OperationId::ProcessWait => match pinned.object {
            KernelNativeObject::Process(process) => {
                super::process::process_wait(task, &process, call.args[0], call.args[1])
            }
            _ => native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0),
        },
        OperationId::ProcessTerminate => match pinned.object {
            KernelNativeObject::Process(process) => {
                super::process::process_terminate(&process, call.args[0])
            }
            KernelNativeObject::SelfProcess => {
                super::process::process_terminate_self(task, call.args[0])
            }
            _ => native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0),
        },
        OperationId::EventCreate => super::event::event_create(state, &pinned.object, call.args[0]),
        OperationId::EventBind => super::event::event_bind(
            state,
            &pinned.object,
            call.args[0],
            call.args[1],
            call.args[2],
        ),
        OperationId::EventTimer => {
            super::event::event_timer(&pinned.object, call.args[0], call.args[1], call.args[2])
        }
        OperationId::EventCancel => super::event::event_cancel(&pinned.object, call.args[0]),
        OperationId::EventWait => super::event::event_wait(
            task,
            &pinned.object,
            call.args[0],
            call.args[1],
            call.args[2],
        ),
        OperationId::ComponentLoad => {
            if !matches!(pinned.object, KernelNativeObject::SelfProcess) {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            }
            super::component::component_load(state, call.args[0], call.args[1])
        }
        OperationId::ComponentActivate => {
            let KernelNativeObject::ComponentTransaction(transaction) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::component::component_activate(
                state,
                &transaction,
                handle,
                call.args[0],
                call.args[1],
            )
        }
        OperationId::ComponentQuery => {
            let KernelNativeObject::Component(component) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::component::component_query(&component, call.args[0])
        }
        OperationId::ComponentInterface => {
            let KernelNativeObject::Component(component) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::component::component_interface(state, &component, handle, call.args[0])
        }
        OperationId::ComponentUnload => {
            let KernelNativeObject::Component(component) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::component::component_unload(
                task,
                state,
                &component,
                handle,
                call.args[0],
                call.args[1],
                call.args[2],
            )
        }
        OperationId::ComponentFinish => {
            let KernelNativeObject::ComponentTransaction(transaction) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::component::component_finish(
                state,
                &transaction,
                handle,
                call.args[0],
                call.args[1],
            )
        }
        OperationId::ComponentWake => {
            let KernelNativeObject::Component(component) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::component::component_wake(&component, call.args[0])
        }
        OperationId::MemoryCreate => {
            super::memory::memory_create(task, state, &pinned.object, call.args[0])
        }
        OperationId::MemoryMap => {
            let KernelNativeObject::MemoryObject(object) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::memory::memory_map(task, state, &object, pinned.rights, call.args[0])
        }
        OperationId::MemoryUnmap => {
            let KernelNativeObject::AddressSpace(vm) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::memory::memory_unmap(state, &vm, call.args[0], call.args[1])
        }
        OperationId::MemoryQuery => {
            let KernelNativeObject::MemoryObject(object) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::memory::memory_query(task, &object, call.args[0])
        }
        OperationId::MemoryRevoke => {
            let KernelNativeObject::MemoryObject(object) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::memory::memory_revoke(task, &object)
        }
        OperationId::MemoryStatistics => {
            let KernelNativeObject::MemoryObject(object) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::memory::memory_statistics(task, state, &object, call.args[0])
        }
        OperationId::ThreadCreate => {
            super::thread::thread_create(task, state, &pinned.object, call.args[0], call.args[1])
        }
        OperationId::ThreadJoin => {
            let KernelNativeObject::Thread(thread) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::thread::thread_join(task, &thread, call.args[0], call.args[1])
        }
        OperationId::ThreadTerminate => {
            let KernelNativeObject::Thread(thread) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::thread::thread_terminate(task, &thread, call.args[0])
        }
        OperationId::ThreadQuery => {
            let KernelNativeObject::Thread(thread) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::thread::thread_query(task, &thread, call.args[0])
        }
        OperationId::DirectoryOpen => {
            let KernelNativeObject::Directory(directory) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::fs::directory_open(task, state, &directory, call.args[0])
        }
        OperationId::DirectoryCreate => {
            let KernelNativeObject::Directory(directory) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::fs::directory_create(task, state, &directory, call.args[0])
        }
        OperationId::DirectoryRemove => {
            let KernelNativeObject::Directory(directory) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::fs::directory_remove(task, &directory, call.args[0], call.args[1])
        }
        OperationId::DirectoryQuery => {
            let KernelNativeObject::Directory(directory) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::fs::directory_query(task, &directory, call.args[0])
        }
        OperationId::FileRead => {
            let KernelNativeObject::File(file) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::fs::file_read(
                task,
                &file,
                call.args[0],
                call.args[1],
                call.args[2],
                call.args[3],
            )
        }
        OperationId::FileWrite => {
            let KernelNativeObject::File(file) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::fs::file_write(
                task,
                &file,
                call.args[0],
                call.args[1],
                call.args[2],
                call.args[3],
            )
        }
        OperationId::FileResize => {
            let KernelNativeObject::File(file) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::fs::file_resize(&file, call.args[0])
        }
        OperationId::FileQuery => {
            let KernelNativeObject::File(file) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::fs::file_query(task, &file, call.args[0])
        }
        OperationId::FileMap => {
            let KernelNativeObject::File(file) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::fs::file_map(
                task,
                state,
                &file,
                pinned.rights,
                call.args[0],
                call.args[1],
                call.args[2],
            )
        }
        OperationId::ChannelCreate => {
            super::channel::channel_create(state, &pinned.object, call.args[0])
        }
        OperationId::ChannelSend => {
            let KernelNativeObject::Channel(channel) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::channel::channel_send(task, state, &channel, call.args[0])
        }
        OperationId::ChannelReceive => {
            let KernelNativeObject::Channel(channel) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::channel::channel_receive(task, state, &channel, call.args[0], call.args[1])
        }
        OperationId::RingCreate => {
            super::ring::ring_create(task, state, &pinned.object, call.args[0])
        }
        OperationId::RingRegister => {
            let KernelNativeObject::SubmissionRing(ring) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::ring::ring_register(state, &ring, call.args[0], call.args[1], call.args[2])
        }
        OperationId::RingUnregister => {
            let KernelNativeObject::SubmissionRing(ring) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::ring::ring_unregister(&ring, call.args[0])
        }
        OperationId::RingKick => {
            let KernelNativeObject::SubmissionRing(ring) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::ring::ring_kick(state, &ring, call.args[0])
        }
        OperationId::RingCancel => {
            let KernelNativeObject::SubmissionRing(ring) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::ring::ring_cancel(&ring, call.args[0])
        }
        OperationId::RingWait => {
            let KernelNativeObject::SubmissionRing(ring) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::ring::ring_wait(task, &ring, call.args[0], call.args[1])
        }
        OperationId::RingQuery => {
            let KernelNativeObject::SubmissionRing(ring) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::ring::ring_query(task, &ring, call.args[0])
        }
        OperationId::SocketCreate => {
            super::socket::socket_create(task, state, &pinned.object, call.args[0])
        }
        OperationId::SocketBind => {
            let KernelNativeObject::Socket(socket) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::socket::socket_bind(task, &socket, call.args[0])
        }
        OperationId::SocketConnect => {
            let KernelNativeObject::Socket(socket) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::socket::socket_connect(task, &socket, call.args[0])
        }
        OperationId::SocketListen => {
            let KernelNativeObject::Socket(socket) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::socket::socket_listen(&socket, call.args[0])
        }
        OperationId::SocketAccept => {
            let KernelNativeObject::Socket(socket) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::socket::socket_accept(task, state, &socket, call.args[0])
        }
        OperationId::SocketSend => {
            let KernelNativeObject::Socket(socket) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::socket::socket_send(
                task,
                state,
                &socket,
                call.args[0],
                call.args[1],
                call.args[2],
                call.args[3],
                call.args[4],
            )
        }
        OperationId::SocketReceive => {
            let KernelNativeObject::Socket(socket) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::socket::socket_receive(
                task,
                state,
                &socket,
                call.args[0],
                call.args[1],
                call.args[2],
                call.args[3],
                call.args[4],
            )
        }
        OperationId::SocketShutdown => {
            let KernelNativeObject::Socket(socket) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::socket::socket_shutdown(&socket, call.args[0])
        }
        OperationId::SocketQuery => {
            let KernelNativeObject::Socket(socket) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::socket::socket_query(task, &socket, call.args[0])
        }
        OperationId::DeviceInvoke => {
            let KernelNativeObject::DeviceFunction(device) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::device::device_invoke(task, state, &device, call.args[0])
        }
        OperationId::DeviceQuery => {
            let KernelNativeObject::DeviceFunction(device) = pinned.object else {
                return native_return(native_abi::status::HANDLE_WRONG_INTERFACE, 0, 0);
            };
            super::device::device_query(task, &device, call.args[0])
        }
    }
}

fn memory_allocate(
    state: &NativeProcessState,
    vm: &VmSpace,
    length: u64,
    alignment: u64,
) -> NativeCallOutcome {
    let page_size = native_abi::PAGE_SIZE;
    if length == 0 {
        return native_return(native_abi::status::MEMORY_INVALID_RANGE, 0, 0);
    }
    if alignment < page_size || !alignment.is_power_of_two() {
        return native_return(native_abi::status::MEMORY_INVALID_ALIGNMENT, 0, 0);
    }
    let Ok(length) = usize::try_from(length) else {
        return native_return(native_abi::status::MEMORY_INVALID_RANGE, 0, 0);
    };
    let Ok(alignment) = usize::try_from(alignment) else {
        return native_return(native_abi::status::MEMORY_INVALID_ALIGNMENT, 0, 0);
    };
    let Some(actual_length) = length
        .checked_add(page_size as usize - 1)
        .map(|rounded| rounded / page_size as usize * page_size as usize)
    else {
        return native_return(native_abi::status::MEMORY_INVALID_RANGE, 0, 0);
    };
    let vm_flags = VmFlags::from_bits(VmFlags::USER | VmFlags::READ | VmFlags::WRITE);
    let range = match vm.map_anon_any_aligned(actual_length, alignment, vm_flags) {
        Ok(range) => range,
        Err(_) => return native_return(native_abi::status::CORE_RESOURCE_EXHAUSTED, 0, 0),
    };
    if state.record_allocation(range.clone()).is_err() {
        let _ = vm.unmap_existing(range);
        return native_return(native_abi::status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    native_return(
        native_abi::status::OK,
        range.start as u64,
        range.len() as u64,
    )
}

fn memory_free(
    state: &NativeProcessState,
    vm: &VmSpace,
    address: u64,
    length: u64,
) -> NativeCallOutcome {
    let page_size = native_abi::PAGE_SIZE;
    if address % page_size != 0 || length == 0 || length % page_size != 0 {
        return native_return(native_abi::status::MEMORY_INVALID_RANGE, 0, 0);
    }
    let Ok(length) = usize::try_from(length) else {
        return native_return(native_abi::status::MEMORY_INVALID_RANGE, 0, 0);
    };
    let Ok(range) = checked_native_user_range(address, length) else {
        return native_return(native_abi::status::MEMORY_INVALID_RANGE, 0, 0);
    };
    if state.overlaps_runtime_range(&range) {
        return native_return(native_abi::status::MEMORY_NOT_OWNED, 0, 0);
    }
    if !state.owns_allocation(&range) {
        return native_return(native_abi::status::MEMORY_NOT_OWNED, 0, 0);
    }
    match vm.unmap_existing(range.clone()) {
        Ok(()) => {
            let _ = state.remove_allocation(&range);
            native_return(native_abi::status::OK, 0, 0)
        }
        Err(Errno::ENOMEM | Errno::EINVAL) => {
            native_return(native_abi::status::MEMORY_INVALID_RANGE, 0, 0)
        }
        Err(_) => native_return(native_abi::status::CORE_RESOURCE_EXHAUSTED, 0, 0),
    }
}

fn checked_native_user_range(address: u64, length: usize) -> Result<Range<usize>, ()> {
    let address = usize::try_from(address).map_err(|_| ())?;
    let end = address.checked_add(length).ok_or(())?;
    let layout = general::mm::user_vm_layout().ok_or(())?;
    if address < native_abi::PAGE_SIZE as usize || end > layout.user_mmap_limit {
        return Err(());
    }
    Ok(address..end)
}

fn insert_pinned_handle(
    state: &NativeProcessState,
    pinned: PinnedNativeHandle,
) -> NativeCallOutcome {
    insert_native_handle(state, pinned.object, pinned.interface, pinned.rights)
}

pub(super) fn insert_native_handle(
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
        return native_return(native_abi::status::STREAM_FAULT, 0, 0);
    };
    let Ok(len) = usize::try_from(len) else {
        return native_return(native_abi::status::CORE_OUT_OF_RANGE, 0, 0);
    };
    if len == 0 {
        return native_return(native_abi::status::OK, 0, 0);
    }
    if user.checked_add(len).is_none() {
        return native_return(native_abi::status::STREAM_FAULT, 0, 0);
    }
    let Some(vm) = task
        .ext_lookup(sched::TASKEXT_VM_SPACE)
        .and_then(|payload| payload.downcast::<VmSpace>().ok())
    else {
        return native_return(native_abi::status::STREAM_FAULT, 0, 0);
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
                return native_return(native_abi::status::STREAM_WOULD_BLOCK, 0, 0);
            }
            Ok(Err(VfsError::WouldBlock)) => {
                match wait_for_stream_writable(task, file) {
                    Ok(()) => {}
                    Err(StreamWaitError::ExternalControl) => {
                        return NativeCallOutcome::RetryExternalControl;
                    }
                    Err(StreamWaitError::Unavailable) => {
                        return native_return(native_abi::status::STREAM_WOULD_BLOCK, 0, 0);
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
        native_return(native_abi::status::STREAM_FAULT, 0, 0)
    } else {
        native_return(native_abi::status::OK, written as u64, 0)
    }
}

fn stream_read(task: &Arc<sched::Task>, file: &File, user: u64, len: u64) -> NativeCallOutcome {
    let Ok(user) = usize::try_from(user) else {
        return native_return(native_abi::status::STREAM_FAULT, 0, 0);
    };
    let Ok(len) = usize::try_from(len) else {
        return native_return(native_abi::status::CORE_OUT_OF_RANGE, 0, 0);
    };
    if len == 0 {
        return native_return(native_abi::status::OK, 0, 0);
    }
    if user.checked_add(len).is_none() {
        return native_return(native_abi::status::STREAM_FAULT, 0, 0);
    }
    let Some(vm) = task
        .ext_lookup(sched::TASKEXT_VM_SPACE)
        .and_then(|payload| payload.downcast::<VmSpace>().ok())
    else {
        return native_return(native_abi::status::STREAM_FAULT, 0, 0);
    };

    let read = 0usize;
    while read < len {
        let address = match user.checked_add(read) {
            Some(address) => address,
            None => return stream_read_fault(read),
        };
        let result =
            unsafe { vm.with_user_write_slice(address, len - read, |buffer| file.read(buffer)) };
        let count = match result {
            Ok(Ok(count)) => count,
            Ok(Err(VfsError::WouldBlock)) if read != 0 => {
                return native_return(native_abi::status::OK, read as u64, 0);
            }
            Ok(Err(VfsError::WouldBlock)) if file.flags().nonblock => {
                return native_return(native_abi::status::STREAM_WOULD_BLOCK, 0, 0);
            }
            Ok(Err(VfsError::WouldBlock)) => {
                match wait_for_stream_event(task, file, PollEvents::POLLIN) {
                    Ok(()) => {}
                    Err(StreamWaitError::ExternalControl) => {
                        return NativeCallOutcome::RetryExternalControl;
                    }
                    Err(StreamWaitError::Unavailable) => {
                        return native_return(native_abi::status::STREAM_WOULD_BLOCK, 0, 0);
                    }
                }
                continue;
            }
            Ok(Err(error)) => return map_stream_read_error(error, read),
            Err(_) => return stream_read_fault(read),
        };
        if count == 0 {
            let status = if read == 0 {
                native_abi::status::STREAM_END
            } else {
                native_abi::status::OK
            };
            return native_return(status, read as u64, 0);
        }
        return stream_read_progress(read, count);
    }
    native_return(native_abi::status::OK, read as u64, 0)
}

pub(super) fn stream_read_memory(
    file: &File,
    memory: &Arc<super::memory::MemoryObject>,
    offset: u64,
    length: u64,
) -> NativeCallOutcome {
    let Ok(length) = usize::try_from(length) else {
        return native_return(native_abi::status::CORE_OUT_OF_RANGE, 0, 0);
    };
    if length == 0 {
        return native_return(native_abi::status::OK, 0, 0);
    }
    let mut buffer = alloc::vec::Vec::new();
    if buffer.try_reserve_exact(length).is_err() {
        return native_return(native_abi::status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    buffer.resize(length, 0);
    stream_read_memory_buffered(file, memory, offset, &mut buffer)
}

pub(super) fn stream_read_memory_buffered(
    file: &File,
    memory: &Arc<super::memory::MemoryObject>,
    offset: u64,
    buffer: &mut [u8],
) -> NativeCallOutcome {
    let access = match memory.begin_access() {
        Ok(access) => access,
        Err(error) => return native_return(error, 0, 0),
    };
    if let Err(error) = memory.validate_transfer(offset, buffer.len()) {
        return native_return(error, 0, 0);
    }
    let count = match file.read(buffer) {
        Ok(count) => count,
        Err(error) => return map_stream_read_error(error, 0),
    };
    if count > buffer.len() {
        return native_return(native_abi::status::STREAM_ERROR, 0, 0);
    }
    if count == 0 {
        return native_return(native_abi::status::STREAM_END, 0, 0);
    }
    if let Err(error) = access.write_from(offset, &buffer[..count]) {
        return native_return(error, 0, 0);
    }
    native_return(native_abi::status::OK, count as u64, 0)
}

pub(super) fn stream_write_memory(
    file: &File,
    memory: &Arc<super::memory::MemoryObject>,
    offset: u64,
    length: u64,
) -> NativeCallOutcome {
    let Ok(length) = usize::try_from(length) else {
        return native_return(native_abi::status::CORE_OUT_OF_RANGE, 0, 0);
    };
    if length == 0 {
        return native_return(native_abi::status::OK, 0, 0);
    }
    let mut buffer = alloc::vec::Vec::new();
    if buffer.try_reserve_exact(length).is_err() {
        return native_return(native_abi::status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    buffer.resize(length, 0);
    stream_write_memory_buffered(file, memory, offset, &mut buffer)
}

pub(super) fn stream_write_memory_buffered(
    file: &File,
    memory: &Arc<super::memory::MemoryObject>,
    offset: u64,
    buffer: &mut [u8],
) -> NativeCallOutcome {
    if let Err(error) = memory.read_into(offset, buffer) {
        return native_return(error, 0, 0);
    }
    match file.write(&buffer) {
        Ok(count) if count <= buffer.len() => {
            native_return(native_abi::status::OK, count as u64, 0)
        }
        Ok(_) => native_return(native_abi::status::STREAM_ERROR, 0, 0),
        Err(error) => map_stream_write_error(error, 0),
    }
}

pub(super) fn stream_read_progress(read: usize, count: usize) -> NativeCallOutcome {
    match read.checked_add(count) {
        Some(total) => native_return(native_abi::status::OK, total as u64, 0),
        None => native_return(native_abi::status::CORE_OUT_OF_RANGE, 0, 0),
    }
}

fn stream_read_fault(read: usize) -> NativeCallOutcome {
    if read == 0 {
        native_return(native_abi::status::STREAM_FAULT, 0, 0)
    } else {
        native_return(native_abi::status::OK, read as u64, 0)
    }
}

pub(super) fn map_stream_read_error(error: VfsError, read: usize) -> NativeCallOutcome {
    if read != 0 {
        return native_return(native_abi::status::OK, read as u64, 0);
    }
    let status = match error {
        VfsError::WouldBlock => native_abi::status::STREAM_WOULD_BLOCK,
        VfsError::BrokenPipe | VfsError::ConnectionReset => native_abi::status::STREAM_CLOSED,
        _ => native_abi::status::STREAM_ERROR,
    };
    native_return(status, 0, 0)
}

pub(super) fn map_stream_write_error(error: VfsError, written: usize) -> NativeCallOutcome {
    if written != 0 {
        return native_return(native_abi::status::OK, written as u64, 0);
    }
    let status = match error {
        VfsError::WouldBlock => native_abi::status::STREAM_WOULD_BLOCK,
        VfsError::BrokenPipe | VfsError::ConnectionReset => native_abi::status::STREAM_CLOSED,
        _ => native_abi::status::STREAM_ERROR,
    };
    native_return(status, 0, 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamWaitError {
    ExternalControl,
    Unavailable,
}

fn wait_for_stream_writable(task: &Arc<sched::Task>, file: &File) -> Result<(), StreamWaitError> {
    wait_for_stream_event(task, file, PollEvents::POLLOUT)
}

fn wait_for_stream_event(
    task: &Arc<sched::Task>,
    file: &File,
    event: PollEvents,
) -> Result<(), StreamWaitError> {
    const IO_RECHECK_NS: u64 = 10_000_000;

    if !file.poll(event).is_empty() {
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
    let registered = file.poll_add_waiter(task, event);
    let deadline_armed =
        sched::register_sleep_deadline(task, sched::now_ns_public().saturating_add(IO_RECHECK_NS));
    if !file.poll(event).is_empty() {
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

pub(super) fn has_native_external_control(task: &Arc<sched::Task>) -> bool {
    task.group_exit_pending()
        || task.native_thread_exit_boundary_pending().is_some()
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

pub(super) fn restore_native_task_after_wait(task: &Arc<sched::Task>) {
    if !task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running) {
        let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Running);
    }
}

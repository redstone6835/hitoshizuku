//! MyGO Native Call Slot 分发与调用参数校验。

use alloc::sync::Arc;

use general::syscall::{NativeCallFrame, NativeCallOutcome, NativeCallReturn};
use native_abi::{NativeHandle, OperationId};

use super::NativeProcessState;
use super::operations::{PinnedNativeHandle, execute_native_operation};

pub(super) fn dispatch_native_call(
    task: &Arc<sched::Task>,
    call: NativeCallFrame,
) -> NativeCallOutcome {
    let Some(payload) = task.thread_group().native_personality_payload() else {
        return native_return(native_abi::status::ABI_UNSUPPORTED_OPERATION, 0, 0);
    };
    let Ok(state) = payload.downcast::<NativeProcessState>() else {
        return native_return(native_abi::status::ABI_UNSUPPORTED_OPERATION, 0, 0);
    };
    let Ok(slot) = usize::try_from(call.slot) else {
        return native_return(native_abi::status::ABI_BAD_SLOT, 0, 0);
    };
    let Some(binding) = state.binding.call_slots.get(slot) else {
        return native_return(native_abi::status::ABI_BAD_SLOT, 0, 0);
    };
    if binding.slot as usize != slot {
        return native_return(native_abi::status::ABI_BAD_SLOT, 0, 0);
    }
    let Some(operation) = binding.operation else {
        return native_return(native_abi::status::ABI_UNSUPPORTED_OPERATION, 0, 0);
    };
    if !valid_call_arguments(operation, &call) {
        return native_return(native_abi::status::CORE_INVALID_ARGUMENT, 0, 0);
    }

    let handle = NativeHandle::from_raw(call.object_handle);
    let pinned = {
        let handles = state.handles.lock();
        let entry = match handles.lookup(handle, binding.interface, binding.required_rights) {
            Ok(entry) => entry,
            Err(status) => return native_return(status, 0, 0),
        };
        PinnedNativeHandle {
            object: entry.object.clone(),
            interface: entry.interface,
            rights: entry.rights,
        }
    };
    execute_native_operation(task, &state, operation, handle, pinned, call)
}

fn valid_call_arguments(operation: OperationId, call: &NativeCallFrame) -> bool {
    if call.reserved_arg != 0 {
        return false;
    }
    let unused = match operation {
        OperationId::ProcessExit | OperationId::HandleRestrict => &call.args[1..],
        OperationId::HandleClose | OperationId::HandleDuplicate | OperationId::ClockRead => {
            &call.args[..]
        }
        OperationId::StreamRead | OperationId::StreamWrite => &call.args[2..],
        OperationId::MemoryAllocate => &call.args[2..],
        OperationId::MemoryFree => &call.args[2..],
    };
    unused.iter().all(|argument| *argument == 0)
}

pub(super) fn native_return(status: u32, value0: u64, value1: u64) -> NativeCallOutcome {
    NativeCallOutcome::Return(NativeCallReturn {
        status,
        value0,
        value1,
    })
}

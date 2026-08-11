//! Native DeviceFunction：显式授予的设备 capability 调用面。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::Ordering as CmpOrdering;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use general::dev::function::{DeviceFunction, DeviceFunctionInvokeError};
use general::syscall::NativeCallOutcome;
use native_abi::wire::{DeviceInfo, DeviceRequest, MemoryRegion};
use native_abi::{NativeHandle, ObjectInterface, Rights, status};
use sched::Task;
use sha2::{Digest, Sha256};

use super::dispatch::native_return;
use super::{KernelNativeObject, NativeProcessState, copy_user_value, copy_user_value_out};

const MAX_DEVICE_TRANSFER: usize = 1024 * 1024;

pub(crate) struct DeviceFunctionObject {
    function: Arc<dyn DeviceFunction>,
    generation: AtomicU64,
    revoked: AtomicBool,
}

impl DeviceFunctionObject {
    pub(crate) fn new(function: Arc<dyn DeviceFunction>) -> Arc<Self> {
        Arc::new(Self {
            function,
            generation: AtomicU64::new(1),
            revoked: AtomicBool::new(false),
        })
    }

    fn mark_revoked(&self) {
        if !self.revoked.swap(true, Ordering::AcqRel) {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn is_revoked(&self) -> bool {
        if self.function.is_gone() {
            self.mark_revoked();
        }
        self.revoked.load(Ordering::Acquire)
    }

    pub(crate) fn dma_context(&self) -> Option<general::dev::dma::DmaContext> {
        if self.is_revoked() {
            return None;
        }
        self.function.dma_context()
    }
}

pub(crate) fn capability(function: Arc<dyn DeviceFunction>) -> KernelNativeObject {
    KernelNativeObject::DeviceFunction(DeviceFunctionObject::new(function))
}

pub(crate) fn bootstrap_capability(
    vfs_context: Option<&Arc<general::vfs::VfsContext>>,
) -> Option<KernelNativeObject> {
    use vfs::cred::Capability;

    let context = vfs_context?;
    if !context.cred().has_cap(Capability::SysAdmin) {
        return None;
    }
    let functions = general::dev::enumerate::DEVICES.functions.try_list()?;
    select_bootstrap_function(functions).map(capability)
}

fn select_bootstrap_function(
    functions: Vec<Arc<dyn DeviceFunction>>,
) -> Option<Arc<dyn DeviceFunction>> {
    functions
        .into_iter()
        .filter(|function| !function.is_gone() && function.operation_contract().is_some())
        .min_by(
            |left, right| match (left.dma_context().is_some(), right.dma_context().is_some()) {
                (true, false) => CmpOrdering::Less,
                (false, true) => CmpOrdering::Greater,
                _ => (left.class_id().raw_id(), left.dev_name())
                    .cmp(&(right.class_id().raw_id(), right.dev_name())),
            },
        )
}

pub(super) fn device_invoke(
    task: &Arc<Task>,
    state: &NativeProcessState,
    device: &DeviceFunctionObject,
    user: u64,
) -> NativeCallOutcome {
    if device.is_revoked() {
        return native_return(status::DEVICE_GONE, 0, 0);
    }
    let request = match copy_user_value::<DeviceRequest>(task, user) {
        Ok(request) => request,
        Err(error) => return native_return(error, 0, 0),
    };
    if request.flags != 0
        || request.reserved != [0; 2]
        || request.deadline_ns != 0 && sched::now_ns_public() >= request.deadline_ns
    {
        return native_return(status::DEVICE_INVALID_REQUEST, 0, 0);
    }
    let input = match copy_region_in(state, &request.input) {
        Ok(input) => input,
        Err(error) => return native_return(error, 0, 0),
    };
    let (output_object, output_offset, output_length) =
        match resolve_region(state, &request.output, Rights::WRITE, true) {
            Ok(region) => region,
            Err(error) => return native_return(error, 0, 0),
        };
    let mut output = Vec::new();
    if output.try_reserve_exact(output_length).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    output.resize(output_length, 0);
    let output_access = match output_object.as_deref() {
        Some(object) => {
            let access = match object.begin_access() {
                Ok(access) => access,
                Err(error) => return native_return(error, 0, 0),
            };
            if let Err(error) = object.validate_transfer(output_offset, output_length) {
                return native_return(error, 0, 0);
            }
            Some(access)
        }
        None => None,
    };
    let written = match device.function.invoke(request.opcode, &input, &mut output) {
        Ok(written) if written <= output.len() => written,
        Ok(_) => return native_return(status::DEVICE_FAULT, 0, 0),
        Err(error) => {
            if error == DeviceFunctionInvokeError::Gone {
                device.mark_revoked();
            }
            return map_device_error(error);
        }
    };
    if written != 0
        && let Some(output_access) = output_access.as_ref()
        && let Err(error) = output_access.write_from(output_offset, &output[..written])
    {
        return native_return(error, 0, 0);
    }
    device.generation.fetch_add(1, Ordering::AcqRel);
    native_return(status::OK, written as u64, 0)
}

pub(super) fn device_invoke_memory_buffered(
    device: &DeviceFunctionObject,
    opcode: u32,
    input: Option<(&super::memory::MemoryObject, u64, usize)>,
    output: Option<(&super::memory::MemoryObject, u64, usize)>,
    buffer: &mut [u8],
) -> NativeCallOutcome {
    if device.is_revoked() {
        return native_return(status::DEVICE_GONE, 0, 0);
    }
    let input_length = input.map_or(0, |(_, _, length)| length);
    let output_length = output.map_or(0, |(_, _, length)| length);
    if input_length
        .checked_add(output_length)
        .is_none_or(|total| total != buffer.len())
    {
        return native_return(status::DEVICE_INVALID_REQUEST, 0, 0);
    }
    let (input_buffer, output_buffer) = buffer.split_at_mut(input_length);
    if let Some((memory, offset, _)) = input
        && let Err(error) = memory.read_into(offset, input_buffer)
    {
        return native_return(error, 0, 0);
    }
    let output_access = match output {
        Some((memory, offset, length)) => {
            let access = match memory.begin_access() {
                Ok(access) => access,
                Err(error) => return native_return(error, 0, 0),
            };
            if let Err(error) = memory.validate_transfer(offset, length) {
                return native_return(error, 0, 0);
            }
            Some((access, offset))
        }
        None => None,
    };
    let written = match device.function.invoke(opcode, input_buffer, output_buffer) {
        Ok(written) if written <= output_buffer.len() => written,
        Ok(_) => return native_return(status::DEVICE_FAULT, 0, 0),
        Err(error) => {
            if error == DeviceFunctionInvokeError::Gone {
                device.mark_revoked();
            }
            return map_device_error(error);
        }
    };
    if let Some((access, offset)) = output_access.as_ref()
        && let Err(error) = access.write_from(*offset, &output_buffer[..written])
    {
        return native_return(error, 0, 0);
    }
    device.generation.fetch_add(1, Ordering::AcqRel);
    native_return(status::OK, written as u64, 0)
}

pub(super) fn device_query(
    task: &Arc<Task>,
    device: &DeviceFunctionObject,
    user: u64,
) -> NativeCallOutcome {
    let contract_hash: [u8; 32] = device
        .function
        .operation_contract()
        .map(|contract| Sha256::digest(contract.as_bytes()).into())
        .unwrap_or([0; 32]);
    let name_hash: [u8; 32] = Sha256::digest(device.function.dev_name().as_bytes()).into();
    let info = DeviceInfo {
        class_id: device.function.class_id().raw_id(),
        generation: device.generation.load(Ordering::Acquire),
        state: if device.is_revoked() { 2 } else { 1 },
        flags: 0,
        contract_hash,
        name_hash,
        reserved: 0,
    };
    match copy_user_value_out(task, user, &info) {
        Ok(()) => native_return(status::OK, 0, 0),
        Err(error) => native_return(error, 0, 0),
    }
}

fn copy_region_in(state: &NativeProcessState, region: &MemoryRegion) -> Result<Vec<u8>, u32> {
    let (object, offset, length) = resolve_region(state, region, Rights::READ, false)?;
    let mut input = Vec::new();
    input
        .try_reserve_exact(length)
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    input.resize(length, 0);
    if length == 0 {
        return Ok(input);
    }
    object
        .ok_or(status::MEMORY_INVALID_RANGE)?
        .read_into(offset, &mut input)?;
    Ok(input)
}

fn resolve_region(
    state: &NativeProcessState,
    region: &MemoryRegion,
    rights: Rights,
    allow_empty: bool,
) -> Result<(Option<Arc<super::memory::MemoryObject>>, u64, usize), u32> {
    if region.length == 0 {
        if allow_empty && region.memory == 0 && region.offset == 0 && region.generation == 0 {
            return Ok((None, 0, 0));
        }
        return Err(status::MEMORY_INVALID_RANGE);
    }
    let length = usize::try_from(region.length).map_err(|_| status::MEMORY_INVALID_RANGE)?;
    if length > MAX_DEVICE_TRANSFER {
        return Err(status::CORE_OUT_OF_RANGE);
    }
    let handle = NativeHandle::from_raw(region.memory);
    let object = {
        let handles = state.handles.lock();
        let entry = handles.lookup(handle, Some(ObjectInterface::MemoryObject), rights)?;
        let KernelNativeObject::MemoryObject(object) = entry.object else {
            return Err(status::HANDLE_WRONG_INTERFACE);
        };
        if object.generation() != region.generation {
            return Err(status::RING_TOKEN_STALE);
        }
        Arc::clone(object)
    };
    if region
        .offset
        .checked_add(region.length)
        .is_none_or(|end| end > object.size())
    {
        return Err(status::MEMORY_INVALID_RANGE);
    }
    Ok((Some(object), region.offset, length))
}

fn map_device_error(error: DeviceFunctionInvokeError) -> NativeCallOutcome {
    let status = match error {
        DeviceFunctionInvokeError::Invalid => status::DEVICE_INVALID_REQUEST,
        DeviceFunctionInvokeError::Gone => status::DEVICE_GONE,
        DeviceFunctionInvokeError::Busy => status::DEVICE_BUSY,
        DeviceFunctionInvokeError::Unsupported => status::DEVICE_UNSUPPORTED,
        DeviceFunctionInvokeError::Fault => status::DEVICE_FAULT,
        DeviceFunctionInvokeError::NoMemory => status::CORE_RESOURCE_EXHAUSTED,
    };
    native_return(status, 0, 0)
}

#[cfg(feature = "soyo-tests")]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec;
    use core::any::Any;
    use core::sync::atomic::{AtomicBool, Ordering};

    use general::dev::dma::DmaContext;
    use general::dev::function::{DeviceClassId, DeviceFunction};
    use ktest::ktest;

    struct TestFunction {
        name: &'static str,
        dma: bool,
        gone: AtomicBool,
    }

    impl DeviceFunction for TestFunction {
        fn class_id(&self) -> DeviceClassId {
            DeviceClassId::new("test")
        }

        fn dev_name(&self) -> &str {
            self.name
        }

        fn operation_contract(&self) -> Option<&str> {
            Some("mygo.device.test@1")
        }

        fn dma_context(&self) -> Option<DmaContext> {
            self.dma.then(DmaContext::default_coherent)
        }

        fn is_gone(&self) -> bool {
            self.gone.load(Ordering::Acquire)
        }

        fn mark_gone(&self) {
            self.gone.store(true, Ordering::Release);
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[ktest]
    fn bootstrap_capability_prefers_a_dma_provider() {
        let control: Arc<dyn DeviceFunction> = Arc::new(TestFunction {
            name: "control",
            dma: false,
            gone: AtomicBool::new(false),
        });
        let dma: Arc<dyn DeviceFunction> = Arc::new(TestFunction {
            name: "dma",
            dma: true,
            gone: AtomicBool::new(false),
        });
        let selected = super::select_bootstrap_function(vec![control, Arc::clone(&dma)])
            .expect("至少一个设备应可授予");
        assert!(Arc::ptr_eq(&selected, &dma));
    }

    #[ktest]
    fn old_capability_observes_device_removal() {
        let function = Arc::new(TestFunction {
            name: "removed",
            dma: true,
            gone: AtomicBool::new(false),
        });
        let erased: Arc<dyn DeviceFunction> = function.clone();
        let capability = super::DeviceFunctionObject::new(erased);

        assert!(capability.dma_context().is_some());
        function.mark_gone();

        assert!(capability.dma_context().is_none());
        assert_native_return(
            super::device_invoke_memory_buffered(&capability, 1, None, None, &mut []),
            native_abi::status::DEVICE_GONE,
        );
    }

    fn assert_native_return(outcome: general::syscall::NativeCallOutcome, expected: u32) {
        let general::syscall::NativeCallOutcome::Return(result) = outcome else {
            panic!("设备测试必须直接返回 Native status");
        };
        assert_eq!(result.status, expected);
        assert_eq!((result.value0, result.value1), (0, 0));
    }
}

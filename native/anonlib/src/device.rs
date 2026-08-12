//! 显式授予的 DeviceFunction 与 DMA MemoryObject。

use super::memory::{MemoryCreate, MemoryObject, MemoryRegion};
use super::{BorrowedHandle, Process, Status, abi, mrt_call};

pub enum DeviceFunctionObject {}

/// 启动策略显式授予的 DeviceFunction capability。
pub struct DeviceFunction {
    handle: BorrowedHandle<'static, DeviceFunctionObject>,
}

impl DeviceFunction {
    /// 获取可选的启动 DeviceFunction；内核不会提供全局设备枚举。
    pub fn initial() -> Option<Self> {
        let raw = unsafe { super::mrt_initial_handle(abi::MYGO_REQUIREMENT_device_function) };
        Some(Self {
            handle: BorrowedHandle::from_raw(raw)?,
        })
    }

    pub(crate) fn raw(&self) -> u64 {
        self.handle.raw()
    }

    /// 创建受此设备 DMA 约束管理的 MemoryObject。
    pub fn create_dma_memory(
        &self,
        process: &Process,
        size: u64,
        alignment: u64,
        device_reads: bool,
        device_writes: bool,
    ) -> Result<MemoryObject, Status> {
        let mut flags = 0;
        if device_reads {
            flags |= abi::MYGO_MEMORY_FLAG_DEVICE_READ;
        }
        if device_writes {
            flags |= abi::MYGO_MEMORY_FLAG_DEVICE_WRITE;
        }
        process.create_memory(MemoryCreate::dma(size, alignment, self.raw(), flags))
    }

    /// 同步调用设备 operation；异步批量调用使用 Submission::device_invoke。
    pub fn invoke(
        &self,
        opcode: u32,
        input: Option<&MemoryRegion<'_>>,
        output: Option<&MemoryRegion<'_>>,
        deadline_ns: u64,
    ) -> Result<usize, Status> {
        if !abi::MYGO_HAS_device_invoke {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let request = abi::MygoDeviceRequest {
            opcode,
            flags: 0,
            input: input.map_or_else(abi::MygoMemoryRegion::default, |region| region.raw),
            output: output.map_or_else(abi::MygoMemoryRegion::default, |region| region.raw),
            deadline_ns,
            reserved: [0; 2],
        };
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_device_invoke,
                self.raw(),
                &request as *const _ as usize as u64,
                0,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        usize::try_from(result.value0).map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))
    }

    pub fn query(&self) -> Result<abi::MygoDeviceInfo, Status> {
        if !abi::MYGO_HAS_device_query {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let mut info = abi::MygoDeviceInfo::default();
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_device_query,
                self.raw(),
                &mut info as *mut _ as usize as u64,
                0,
                0,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(info)
        } else {
            Err(Status(result.status))
        }
    }
}

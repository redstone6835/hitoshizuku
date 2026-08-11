#![no_std]

//! MyGO Native 的最小 Rust 安全对象接口。

mod component;
mod channel;
mod device;
mod fs;
mod memory;
mod memory_intrinsics;
mod ring;
mod socket;
mod thread;

pub use component::{Component, ComponentCall, Interface};
pub use channel::{Channel, ChannelMessage, ChannelTransfer, ReceivedHandle};
pub use device::DeviceFunction;
pub use fs::{Directory, DirectoryRights, File, FileRights};
pub use memory::{
    AddressSpace, MappedRegion, MemoryCreate, MemoryObject, MemoryPermissions, MemoryRegion,
};
pub use ring::{Completion, Registration, Ring, Submission};
pub use socket::{NetworkAddress, Socket, SocketConfig};
pub use thread::{Thread, ThreadCreate};

use core::marker::PhantomData;
use core::num::NonZeroU64;

#[allow(dead_code)]
mod abi {
    include!(env!("MYGO_PROGRAM_RS"));
}

unsafe extern "C" {
    pub(crate) fn mrt_call(
        slot: u64,
        object_handle: u64,
        arg0: u64,
        arg1: u64,
        arg2: u64,
        arg3: u64,
        arg4: u64,
    ) -> abi::MygoNativeResult;
    fn mrt_initial_handle(requirement_id: u32) -> u64;
    pub(crate) fn mrt_current_component() -> u64;
    fn mrt_terminate(status: u32) -> !;
    fn mrt_abort() -> !;
}

fn close_handle(raw: u64) {
    if abi::MYGO_HAS_handle_close {
        let _ = unsafe { mrt_call(abi::MYGO_SLOT_handle_close, raw, 0, 0, 0, 0, 0) };
    }
}

/// MyGO Native operation 返回的原始状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Status(u32);

impl Status {
    /// 判断 operation 是否成功，不要求调用者依赖生成 binding 的数值常量。
    pub const fn is_ok(self) -> bool {
        self.0 == abi::MYGO_STATUS_ok
    }

    /// 返回 Wire ABI 中未经转换的状态值。
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// 由启动环境借给当前映像的 capability handle。
pub struct BorrowedHandle<'a, T> {
    raw: NonZeroU64,
    marker: PhantomData<&'a T>,
}

impl<T> Copy for BorrowedHandle<'_, T> {}

impl<T> Clone for BorrowedHandle<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, T> BorrowedHandle<'a, T> {
    fn from_raw(raw: u64) -> Option<Self> {
        Some(Self {
            raw: NonZeroU64::new(raw)?,
            marker: PhantomData,
        })
    }

    fn raw(self) -> u64 {
        self.raw.get()
    }
}

/// Stream 对象的类型标记。
pub enum StreamObject {}

/// Process 对象的类型标记。
pub enum ProcessObject {}

/// ExecutableImage 对象的类型标记。
pub enum ImageObject {}

/// EventPort 对象的类型标记。
pub enum EventPortObject {}

/// 具备程序 manifest 所声明权限的 Stream capability。
pub struct Stream<'a> {
    handle: BorrowedHandle<'a, StreamObject>,
}

impl Stream<'_> {
    /// 将完整字节切片写入 Stream。
    pub fn write(&self, bytes: &[u8]) -> Result<usize, Status> {
        if !abi::MYGO_HAS_stream_write {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let length =
            u64::try_from(bytes.len()).map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_stream_write,
                self.handle.raw(),
                bytes.as_ptr() as usize as u64,
                length,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        let written = usize::try_from(result.value0)
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        if written > bytes.len() {
            return Err(Status(abi::MYGO_STATUS_core_out_of_range));
        }
        Ok(written)
    }
}

/// 获取启动环境授予的 stdout Stream。
pub fn stdout() -> Option<Stream<'static>> {
    let raw = unsafe { mrt_initial_handle(abi::MYGO_REQUIREMENT_stdout) };
    Some(Stream {
        handle: BorrowedHandle::from_raw(raw)?,
    })
}

pub(crate) struct OwnedHandle<T> {
    raw: NonZeroU64,
    marker: PhantomData<T>,
}

impl<T> OwnedHandle<T> {
    pub(crate) fn new(raw: u64) -> Option<Self> {
        Some(Self {
            raw: NonZeroU64::new(raw)?,
            marker: PhantomData,
        })
    }

    pub(crate) fn raw(&self) -> u64 {
        self.raw.get()
    }

    pub(crate) fn into_raw(self) -> u64 {
        let handle = core::mem::ManuallyDrop::new(self);
        handle.raw.get()
    }
}

impl<T> Drop for OwnedHandle<T> {
    fn drop(&mut self) {
        close_handle(self.raw());
    }
}

/// 当前线程组的借用 Process capability。
pub struct Process {
    handle: BorrowedHandle<'static, ProcessObject>,
}

impl Process {
    /// 获取启动环境授予当前映像的 self process capability。
    pub fn current() -> Option<Self> {
        let raw = unsafe { mrt_initial_handle(abi::MYGO_REQUIREMENT_self_process) };
        Some(Self {
            handle: BorrowedHandle::from_raw(raw)?,
        })
    }

    pub(crate) fn raw(&self) -> u64 {
        self.handle.raw()
    }

    /// 由已经验证的映像创建一个 Native 子进程。
    pub fn spawn(&self, request: &SpawnRequest<'_>) -> Result<ChildProcess, Status> {
        if !abi::MYGO_HAS_process_spawn {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_process_spawn,
                self.raw(),
                &request.raw as *const _ as usize as u64,
                core::mem::size_of::<abi::MygoSpawnRequest>() as u64,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        OwnedHandle::new(result.value0)
            .map(|handle| ChildProcess { handle })
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))
    }
}

/// 已复制并通过内核校验的不可变 SOYO 映像。
pub struct Image {
    handle: OwnedHandle<ImageObject>,
}

impl Image {
    /// 将用户地址中的 SOYO 字节复制为可复用映像对象。
    pub fn create(process: &Process, bytes: &[u8]) -> Result<Self, Status> {
        if !abi::MYGO_HAS_image_create {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let length =
            u64::try_from(bytes.len()).map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_image_create,
                process.raw(),
                bytes.as_ptr() as usize as u64,
                length,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        OwnedHandle::new(result.value0)
            .map(|handle| Self { handle })
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))
    }

    pub(crate) fn raw(&self) -> u64 {
        self.handle.raw()
    }

    /// 按上层 Channel 协议把收到的 owned handle 解释为 Image。
    ///
    /// 若发送方违反协议，后续 Image operation 仍由内核返回 wrong-interface。
    pub fn from_received(handle: ReceivedHandle) -> Self {
        Self {
            handle: OwnedHandle::new(handle.into_raw()).expect("received handle 必须非零"),
        }
    }
}

/// 向 child 显式转移一个 stdout capability 的描述。
#[repr(transparent)]
pub struct HandleTransfer {
    raw: abi::MygoHandleTransfer,
}

impl HandleTransfer {
    pub(crate) const fn raw(&self) -> &abi::MygoHandleTransfer {
        &self.raw
    }

    /// 复制 Stream 的 write 权限给 child 的 stdout requirement。
    pub fn stdout(stream: &Stream<'_>) -> Self {
        Self {
            raw: abi::MygoHandleTransfer {
                requirement_id: abi::MYGO_REQUIREMENT_stdout,
                reserved: 0,
                source_handle: stream.handle.raw(),
                requested_rights: abi::MYGO_RIGHT_write,
                flags: 0,
            },
        }
    }

    /// 复制 Channel endpoint，满足 child 的通用服务通道 requirement。
    pub fn service_channel(channel: &Channel) -> Self {
        Self {
            raw: abi::MygoHandleTransfer {
                requirement_id: abi::MYGO_REQUIREMENT_service_channel,
                reserved: 0,
                source_handle: channel.raw(),
                requested_rights: abi::MYGO_RIGHT_send
                    | abi::MYGO_RIGHT_receive
                    | abi::MYGO_RIGHT_duplicate
                    | abi::MYGO_RIGHT_observe,
                flags: 0,
            },
        }
    }

    /// 复制 Directory 视图，满足 child 的 root directory requirement。
    pub fn root_directory(directory: &Directory) -> Self {
        Self {
            raw: abi::MygoHandleTransfer {
                requirement_id: abi::MYGO_REQUIREMENT_root_directory,
                reserved: 0,
                source_handle: directory.raw(),
                requested_rights: abi::MYGO_RIGHT_open | abi::MYGO_RIGHT_inspect,
                flags: 0,
            },
        }
    }
}

/// process.spawn 的固定请求视图。数组为空时表示不传递参数或 capability。
pub struct SpawnRequest<'a> {
    raw: abi::MygoSpawnRequest,
    transfers: PhantomData<&'a [HandleTransfer]>,
}

impl<'a> SpawnRequest<'a> {
    /// 创建不带 argv/env/transfer 的最小请求。
    pub fn new(image: &'a Image) -> Self {
        Self {
            raw: abi::MygoSpawnRequest {
                image: image.raw(),
                argv: abi::MygoProcessArrayRef {
                    ptr: 0,
                    count: 0,
                    reserved: 0,
                },
                env: abi::MygoProcessArrayRef {
                    ptr: 0,
                    count: 0,
                    reserved: 0,
                },
                transfers: abi::MygoProcessArrayRef {
                    ptr: 0,
                    count: 0,
                    reserved: 0,
                },
                resource_policy: 0,
            },
            transfers: PhantomData,
        }
    }

    /// 添加显式 capability transfer；rights 由每个 transfer 单独声明。
    pub fn with_transfers(mut self, transfers: &'a [HandleTransfer]) -> Self {
        self.raw.transfers = abi::MygoProcessArrayRef {
            ptr: transfers
                .first()
                .map(|transfer| &transfer.raw as *const _ as usize as u64)
                .unwrap_or(0),
            count: transfers.len() as u32,
            reserved: 0,
        };
        self.transfers = PhantomData;
        self
    }
}

/// Native child Process capability，析构时关闭父侧引用。
pub struct ChildProcess {
    handle: OwnedHandle<ProcessObject>,
}

impl ChildProcess {
    fn raw(&self) -> u64 {
        self.handle.raw()
    }

    /// 判断完成记录是否来自该 child 的 terminal event。
    pub fn event_matches(&self, record: &EventRecord) -> bool {
        record.source_handle == self.raw()
    }

    /// 等待 child 终止并读取完整 Native ProcessResult。
    pub fn wait(&self, deadline_ns: u64) -> Result<ProcessResult, Status> {
        if !abi::MYGO_HAS_process_wait {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let mut result = abi::MygoProcessResult::default();
        let call = unsafe {
            mrt_call(
                abi::MYGO_SLOT_process_wait,
                self.raw(),
                &mut result as *mut _ as usize as u64,
                deadline_ns,
                0,
                0,
                0,
            )
        };
        if call.status != abi::MYGO_STATUS_ok {
            return Err(Status(call.status));
        }
        Ok(result)
    }
}

/// EventPort 的完成队列对象。
pub struct EventPort {
    handle: OwnedHandle<EventPortObject>,
}

impl EventPort {
    /// 创建固定容量的 EventPort。
    pub fn create(process: &Process, capacity: u32) -> Result<Self, Status> {
        if !abi::MYGO_HAS_event_create {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_event_create,
                process.raw(),
                u64::from(capacity),
                0,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        OwnedHandle::new(result.value0)
            .map(|handle| Self { handle })
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))
    }

    /// 绑定 child 退出事件并返回 EventPort 内 token。
    pub fn bind_process_exit(&self, child: &ChildProcess, user_data: u64) -> Result<u64, Status> {
        if !abi::MYGO_HAS_event_bind {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_event_bind,
                self.handle.raw(),
                child.raw(),
                u64::from(abi::MYGO_EVENT_KIND_PROCESS_EXITED),
                user_data,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        Ok(result.value0)
    }

    /// 摘取最多 records 长度的完成记录；无记录时按 deadline 阻塞。
    pub fn wait(&self, records: &mut [EventRecord], deadline_ns: u64) -> Result<usize, Status> {
        if !abi::MYGO_HAS_event_wait {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let capacity =
            u32::try_from(records.len()).map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_event_wait,
                self.handle.raw(),
                records.as_mut_ptr() as usize as u64,
                u64::from(capacity),
                deadline_ns,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        let count = usize::try_from(result.value0)
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        if count > records.len() {
            return Err(Status(abi::MYGO_STATUS_core_out_of_range));
        }
        Ok(count)
    }
}

/// 由 SOYO binding 生成的 Native 进程结果。
pub type ProcessResult = abi::MygoProcessResult;

/// 由 SOYO binding 生成的 EventPort 完成记录。
pub type EventRecord = abi::MygoEventRecord;

/// 通过当前进程 capability 正常终止进程。
pub fn exit(status: u32) -> ! {
    unsafe { mrt_terminate(status) }
}

/// 以确定性异常路径终止当前映像。
pub fn abort() -> ! {
    unsafe { mrt_abort() }
}

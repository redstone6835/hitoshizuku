//! 保留消息边界的 Channel 对象。

use core::marker::PhantomData;

use super::{BorrowedHandle, Image, OwnedHandle, Process, Status, abi, mrt_call};

pub enum ChannelObject {}

enum ChannelHandle {
    Borrowed(BorrowedHandle<'static, ChannelObject>),
    Owned(OwnedHandle<ChannelObject>),
}

impl ChannelHandle {
    fn raw(&self) -> u64 {
        match self {
            Self::Borrowed(handle) => handle.raw(),
            Self::Owned(handle) => handle.raw(),
        }
    }
}

/// Native Channel capability。启动服务端点是借用对象，运行时创建端点是 owned 对象。
pub struct Channel {
    handle: ChannelHandle,
}

pub enum ReceivedObject {}

/// Channel 接收后由当前进程负责关闭的 capability。
pub struct ReceivedHandle {
    handle: OwnedHandle<ReceivedObject>,
    rights: u64,
}

impl ReceivedHandle {
    pub fn rights(&self) -> u64 {
        self.rights
    }

    pub(crate) fn into_raw(self) -> u64 {
        self.handle.into_raw()
    }
}

/// 一次原子 Channel handle copy；生命周期保证发送期间源 Image 仍然存活。
#[repr(transparent)]
pub struct ChannelTransfer<'a> {
    raw: abi::MygoChannelHandleTransfer,
    source: PhantomData<&'a Image>,
}

impl<'a> ChannelTransfer<'a> {
    /// 以最小 `load` 权限复制 Image，供组件仓库返回装载输入。
    pub fn copy_image(image: &'a Image) -> Self {
        Self {
            raw: abi::MygoChannelHandleTransfer {
                source_handle: image.raw(),
                requested_rights: abi::MYGO_RIGHT_load,
                flags: 0,
                reserved: 0,
            },
            source: PhantomData,
        }
    }
}

/// 一次 receive 实际写入的数据与 handle 数量。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelMessage {
    pub bytes: usize,
    pub handles: usize,
}

impl Process {
    /// 创建一对互联 Channel endpoint。
    pub fn create_channel(&self, capacity: u32) -> Result<(Channel, Channel), Status> {
        if !abi::MYGO_HAS_channel_create {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_channel_create,
                self.raw(),
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
        let left = OwnedHandle::new(result.value0)
            .map(|handle| Channel {
                handle: ChannelHandle::Owned(handle),
            })
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))?;
        let right = OwnedHandle::new(result.value1)
            .map(|handle| Channel {
                handle: ChannelHandle::Owned(handle),
            })
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))?;
        Ok((left, right))
    }
}

impl Channel {
    /// 获取启动环境显式 transfer 的服务 Channel；内核不会自动创建该对象。
    pub fn service() -> Option<Self> {
        let raw = unsafe { super::mrt_initial_handle(abi::MYGO_REQUIREMENT_service_channel) };
        Some(Self {
            handle: ChannelHandle::Borrowed(BorrowedHandle::from_raw(raw)?),
        })
    }

    pub(crate) fn raw(&self) -> u64 {
        self.handle.raw()
    }

    /// 原子发送一条无 handle transfer 的消息。
    pub fn send(&self, bytes: &[u8]) -> Result<(), Status> {
        self.send_with_handles(bytes, &[])
    }

    /// 原子发送消息和 capability copies；任一 transfer 失败时消息不会入队。
    pub fn send_with_handles(
        &self,
        bytes: &[u8],
        handles: &[ChannelTransfer<'_>],
    ) -> Result<(), Status> {
        if !abi::MYGO_HAS_channel_send {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let size = u32::try_from(bytes.len())
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        let handle_count = u32::try_from(handles.len())
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        if handle_count > abi::MYGO_MAX_CHANNEL_MESSAGE_HANDLES {
            return Err(Status(abi::MYGO_STATUS_core_out_of_range));
        }
        let message = abi::MygoChannelMessage {
            data_ptr: bytes.as_ptr() as usize as u64,
            data_size: size,
            data_capacity: size,
            handles_ptr: handles
                .first()
                .map(|handle| &handle.raw as *const _ as usize as u64)
                .unwrap_or(0),
            handle_count,
            handle_capacity: handle_count,
            flags: 0,
            reserved: [0; 3],
        };
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_channel_send,
                self.raw(),
                &message as *const _ as usize as u64,
                0,
                0,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(())
        } else {
            Err(Status(result.status))
        }
    }

    /// 接收一条消息；缓冲不足时内核不会截断或消费消息。
    pub fn receive(
        &self,
        bytes: &mut [u8],
        received_handles: &mut [Option<ReceivedHandle>],
        deadline_ns: u64,
    ) -> Result<ChannelMessage, Status> {
        if !abi::MYGO_HAS_channel_receive {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let data_capacity = u32::try_from(bytes.len())
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        let handle_capacity = u32::try_from(received_handles.len())
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        if handle_capacity > abi::MYGO_MAX_CHANNEL_MESSAGE_HANDLES {
            return Err(Status(abi::MYGO_STATUS_core_out_of_range));
        }
        for handle in received_handles.iter_mut() {
            *handle = None;
        }
        let mut raw_handles = [abi::MygoChannelHandleTransfer {
            source_handle: 0,
            requested_rights: 0,
            flags: 0,
            reserved: 0,
        }; abi::MYGO_MAX_CHANNEL_MESSAGE_HANDLES as usize];
        let mut message = abi::MygoChannelMessage {
            data_ptr: bytes.as_mut_ptr() as usize as u64,
            data_size: 0,
            data_capacity,
            handles_ptr: raw_handles.as_mut_ptr() as usize as u64,
            handle_count: 0,
            handle_capacity,
            flags: 0,
            reserved: [0; 3],
        };
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_channel_receive,
                self.raw(),
                &mut message as *mut _ as usize as u64,
                deadline_ns,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        let data_size = usize::try_from(result.value0)
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        let handle_count = usize::try_from(result.value1)
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        if data_size > bytes.len() || handle_count > received_handles.len() {
            return Err(Status(abi::MYGO_STATUS_core_out_of_range));
        }
        for (target, transfer) in received_handles.iter_mut().zip(raw_handles).take(handle_count) {
            if transfer.flags != 0 || transfer.reserved != 0 {
                return Err(Status(abi::MYGO_STATUS_core_out_of_range));
            }
            let Some(handle) = OwnedHandle::new(transfer.source_handle) else {
                return Err(Status(abi::MYGO_STATUS_core_out_of_range));
            };
            *target = Some(ReceivedHandle {
                handle,
                rights: transfer.requested_rights,
            });
        }
        Ok(ChannelMessage {
            bytes: data_size,
            handles: handle_count,
        })
    }
}

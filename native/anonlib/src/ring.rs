//! SubmissionRing 批量提交、注册内存与唯一完成记录。

use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::channel::Channel;
use super::device::DeviceFunction;
use super::fs::File;
use super::memory::MemoryObject;
use super::socket::Socket;
use super::{OwnedHandle, Process, Status, abi, mrt_call};

pub enum RingObject {}

/// Native SubmissionRing capability。
pub struct Ring {
    handle: OwnedHandle<RingObject>,
    shared: NonNull<abi::MygoRingSharedState>,
    entries: u32,
    mask: u32,
    sq_offset: usize,
    cq_offset: usize,
    submit_lock: AtomicBool,
    completion_lock: AtomicBool,
}

struct RingLock<'a> {
    lock: &'a AtomicBool,
}

impl Drop for RingLock<'_> {
    fn drop(&mut self) {
        self.lock.store(false, Ordering::Release);
    }
}

/// Ring 内固定的 MemoryObject 区间。
pub struct Registration<'a> {
    ring: u64,
    token: u64,
    marker: PhantomData<&'a MemoryObject>,
}

impl Registration<'_> {
    pub const fn token(&self) -> u64 {
        self.token
    }
}

/// 固定 64 字节 submission descriptor 的类型化构造结果。
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Submission {
    raw: abi::MygoSubmissionDescriptor,
}

/// 固定 32 字节 completion record。
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Completion {
    raw: abi::MygoCompletionRecord,
}

impl Completion {
    pub const fn user_data(&self) -> u64 {
        self.raw.user_data
    }

    pub const fn status(&self) -> Status {
        Status(self.raw.status)
    }

    pub const fn values(&self) -> (u64, u64) {
        (self.raw.value0, self.raw.value1)
    }
}

impl Process {
    /// 创建固定容量的 SubmissionRing。
    pub fn create_ring(&self, capacity: u32) -> Result<Ring, Status> {
        if !abi::MYGO_HAS_ring_create {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_ring_create,
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
        let handle = OwnedHandle::new(result.value0)
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))?;
        let shared = NonNull::new(result.value1 as usize as *mut abi::MygoRingSharedState)
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))?;
        let header = unsafe { shared.as_ptr().read() };
        if header.magic != abi::MYGO_RING_SHARED_MAGIC
            || header.version != abi::MYGO_RING_SHARED_VERSION
            || header.flags != 0
            || header.entries != capacity
            || header.mask != capacity.wrapping_sub(1)
            || header.sq_head != 0
            || header.sq_tail != 0
            || header.cq_head != 0
            || header.cq_tail != 0
            || header.sq_offset < core::mem::size_of::<abi::MygoRingSharedState>() as u64
            || header.cq_offset
                < header
                    .sq_offset
                    .saturating_add(u64::from(capacity) * core::mem::size_of::<Submission>() as u64)
            || header.generation == 0
            || header.reserved != 0
        {
            return Err(Status(abi::MYGO_STATUS_ring_invalid_descriptor));
        }
        Ok(Ring {
            handle,
            shared,
            entries: header.entries,
            mask: header.mask,
            sq_offset: usize::try_from(header.sq_offset)
                .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?,
            cq_offset: usize::try_from(header.cq_offset)
                .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?,
            submit_lock: AtomicBool::new(false),
            completion_lock: AtomicBool::new(false),
        })
    }
}

impl Ring {
    pub(crate) fn raw(&self) -> u64 {
        self.handle.raw()
    }

    fn lock<'a>(&'a self, lock: &'a AtomicBool) -> RingLock<'a> {
        while lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        RingLock { lock }
    }

    fn header(&self) -> *mut abi::MygoRingSharedState {
        self.shared.as_ptr()
    }

    unsafe fn atomic(&self, field: *const u32) -> &AtomicU32 {
        unsafe { &*field.cast::<AtomicU32>() }
    }

    fn entries(&self) -> u32 {
        self.entries
    }

    fn submission_address(&self, position: u32) -> *mut abi::MygoSubmissionDescriptor {
        let index = usize::try_from(position & self.mask).expect("Ring index 必须适配 usize");
        unsafe {
            (self
                .header()
                .cast::<u8>()
                .add(self.sq_offset))
                .cast::<abi::MygoSubmissionDescriptor>()
                .add(index)
        }
    }

    fn completion_address(&self, position: u32) -> *const abi::MygoCompletionRecord {
        let index = usize::try_from(position & self.mask).expect("Ring index 必须适配 usize");
        unsafe {
            (self
                .header()
                .cast::<u8>()
                .add(self.cq_offset))
                .cast::<abi::MygoCompletionRecord>()
                .add(index)
        }
    }

    /// 固定 MemoryObject 区间并返回 generation token。
    pub fn register<'a>(
        &self,
        memory: &'a MemoryObject,
        offset: u64,
        length: u64,
    ) -> Result<Registration<'a>, Status> {
        if !abi::MYGO_HAS_ring_register {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_ring_register,
                self.raw(),
                memory.raw(),
                offset,
                length,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(Registration {
                ring: self.raw(),
                token: result.value0,
                marker: PhantomData,
            })
        } else {
            Err(Status(result.status))
        }
    }

    /// 显式解除注册；使用中的 registration 会返回 ring.busy。
    pub fn unregister(&self, registration: Registration<'_>) -> Result<(), Status> {
        if registration.ring != self.raw() {
            return Err(Status(abi::MYGO_STATUS_ring_token_stale));
        }
        if !abi::MYGO_HAS_ring_unregister {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_ring_unregister,
                self.raw(),
                registration.token,
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

    /// 原子验证并提交一批 descriptor。
    pub fn kick(&self, submissions: &[Submission]) -> Result<usize, Status> {
        if !abi::MYGO_HAS_ring_kick {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let count = u32::try_from(submissions.len())
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        if count == 0 || count > abi::MYGO_MAX_RING_BATCH {
            return Err(Status(abi::MYGO_STATUS_core_invalid_argument));
        }
        let _lock = self.lock(&self.submit_lock);
        let header = self.header();
        let sq_head = unsafe { self.atomic(core::ptr::addr_of!((*header).sq_head)) }
            .load(Ordering::Acquire);
        let sq_tail = unsafe { self.atomic(core::ptr::addr_of!((*header).sq_tail)) }
            .load(Ordering::Relaxed);
        let queued = sq_tail.wrapping_sub(sq_head);
        if queued > self.entries() || count > self.entries() - queued {
            return Err(Status(abi::MYGO_STATUS_ring_full));
        }
        for (index, submission) in submissions.iter().enumerate() {
            let position = sq_tail.wrapping_add(index as u32);
            unsafe { self.submission_address(position).write(submission.raw) };
        }
        unsafe { self.atomic(core::ptr::addr_of!((*header).sq_tail)) }
            .store(sq_tail.wrapping_add(count), Ordering::Release);
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_ring_kick,
                self.raw(),
                u64::from(count),
                0,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            unsafe { self.atomic(core::ptr::addr_of!((*header).sq_tail)) }
                .store(sq_tail, Ordering::Release);
            return Err(Status(result.status));
        }
        if result.value0 != u64::from(count) {
            return Err(Status(abi::MYGO_STATUS_core_out_of_range));
        }
        Ok(count as usize)
    }

    /// 取消由 user_data 标识的 queued/running 请求。
    pub fn cancel(&self, user_data: u64) -> Result<(), Status> {
        if !abi::MYGO_HAS_ring_cancel {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_ring_cancel,
                self.raw(),
                user_data,
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

    /// 等待至少 `minimum` 项完成后摘取记录；每个 user_data 最多产生一个最终 completion。
    pub fn wait(
        &self,
        completions: &mut [Completion],
        minimum: usize,
        deadline_ns: u64,
    ) -> Result<usize, Status> {
        if !abi::MYGO_HAS_ring_wait {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        if minimum == 0
            || minimum > completions.len()
            || minimum > self.entries() as usize
            || minimum > abi::MYGO_MAX_RING_BATCH as usize
        {
            return Err(Status(abi::MYGO_STATUS_core_invalid_argument));
        }
        if self.available_completions()? < minimum {
            let result = unsafe {
                mrt_call(
                    abi::MYGO_SLOT_ring_wait,
                    self.raw(),
                    minimum as u64,
                    deadline_ns,
                    0,
                    0,
                    0,
                )
            };
            if result.status != abi::MYGO_STATUS_ok {
                return Err(Status(result.status));
            }
            if result.value0 < minimum as u64 || result.value0 > u64::from(self.entries()) {
                return Err(Status(abi::MYGO_STATUS_ring_invalid_descriptor));
            }
        }
        let count = self.drain(completions)?;
        if count < minimum {
            return Err(Status(abi::MYGO_STATUS_ring_invalid_descriptor));
        }
        Ok(count)
    }

    fn available_completions(&self) -> Result<usize, Status> {
        let header = self.header();
        let cq_head = unsafe { self.atomic(core::ptr::addr_of!((*header).cq_head)) }
            .load(Ordering::Relaxed);
        let cq_tail = unsafe { self.atomic(core::ptr::addr_of!((*header).cq_tail)) }
            .load(Ordering::Acquire);
        let queued = cq_tail.wrapping_sub(cq_head);
        if queued > self.entries() {
            return Err(Status(abi::MYGO_STATUS_ring_invalid_descriptor));
        }
        Ok(queued as usize)
    }

    /// 不陷入内核，直接从共享 CQ 读取已经发布的完成记录。
    pub fn drain(&self, completions: &mut [Completion]) -> Result<usize, Status> {
        let _lock = self.lock(&self.completion_lock);
        let header = self.header();
        let cq_head = unsafe { self.atomic(core::ptr::addr_of!((*header).cq_head)) }
            .load(Ordering::Relaxed);
        let cq_tail = unsafe { self.atomic(core::ptr::addr_of!((*header).cq_tail)) }
            .load(Ordering::Acquire);
        let queued = cq_tail.wrapping_sub(cq_head);
        if queued > self.entries() {
            return Err(Status(abi::MYGO_STATUS_ring_invalid_descriptor));
        }
        let count = completions.len().min(queued as usize);
        for (index, completion) in completions[..count].iter_mut().enumerate() {
            let raw = unsafe { self.completion_address(cq_head.wrapping_add(index as u32)).read() };
            if raw.reserved != 0 || raw.user_data == 0 {
                return Err(Status(abi::MYGO_STATUS_ring_invalid_descriptor));
            }
            completion.raw = raw;
        }
        unsafe { self.atomic(core::ptr::addr_of!((*header).cq_head)) }
            .store(cq_head.wrapping_add(count as u32), Ordering::Release);
        Ok(count)
    }

    pub fn query(&self) -> Result<abi::MygoRingInfo, Status> {
        if !abi::MYGO_HAS_ring_query {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let mut info = abi::MygoRingInfo::default();
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_ring_query,
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

impl Submission {
    fn memory_io(
        slot: u64,
        handle: u64,
        registration: &Registration<'_>,
        offset: u64,
        length: u64,
        operation_offset: u64,
        user_data: u64,
    ) -> Self {
        Self {
            raw: abi::MygoSubmissionDescriptor {
                slot,
                handle,
                arg0: registration.token,
                arg1: offset,
                arg2: length,
                arg3: operation_offset,
                arg4: 0,
                user_data,
            },
        }
    }

    pub fn file_read(
        file: &File,
        registration: &Registration<'_>,
        memory_offset: u64,
        length: u64,
        file_offset: u64,
        user_data: u64,
    ) -> Self {
        Self::memory_io(
            abi::MYGO_SLOT_file_read,
            file.raw(),
            registration,
            memory_offset,
            length,
            file_offset,
            user_data,
        )
    }

    pub fn file_write(
        file: &File,
        registration: &Registration<'_>,
        memory_offset: u64,
        length: u64,
        file_offset: u64,
        user_data: u64,
    ) -> Self {
        Self::memory_io(
            abi::MYGO_SLOT_file_write,
            file.raw(),
            registration,
            memory_offset,
            length,
            file_offset,
            user_data,
        )
    }

    pub fn channel_send(
        channel: &Channel,
        registration: &Registration<'_>,
        offset: u64,
        length: u64,
        user_data: u64,
    ) -> Self {
        Self::memory_io(
            abi::MYGO_SLOT_channel_send,
            channel.raw(),
            registration,
            offset,
            length,
            0,
            user_data,
        )
    }

    pub fn channel_receive(
        channel: &Channel,
        registration: &Registration<'_>,
        offset: u64,
        length: u64,
        user_data: u64,
    ) -> Self {
        Self::memory_io(
            abi::MYGO_SLOT_channel_receive,
            channel.raw(),
            registration,
            offset,
            length,
            0,
            user_data,
        )
    }

    pub fn socket_send(
        socket: &Socket,
        registration: &Registration<'_>,
        offset: u64,
        length: u64,
        address: Option<&Registration<'_>>,
        deadline_ns: u64,
        user_data: u64,
    ) -> Self {
        Self {
            raw: abi::MygoSubmissionDescriptor {
                slot: abi::MYGO_SLOT_socket_send,
                handle: socket.raw(),
                arg0: registration.token,
                arg1: offset,
                arg2: length,
                arg3: address.map_or(0, |registration| registration.token),
                arg4: deadline_ns,
                user_data,
            },
        }
    }

    pub fn socket_receive(
        socket: &Socket,
        registration: &Registration<'_>,
        offset: u64,
        length: u64,
        address: Option<&Registration<'_>>,
        deadline_ns: u64,
        user_data: u64,
    ) -> Self {
        let mut submission = Self::socket_send(
            socket,
            registration,
            offset,
            length,
            address,
            deadline_ns,
            user_data,
        );
        submission.raw.slot = abi::MYGO_SLOT_socket_receive;
        submission
    }

    pub fn device_invoke(
        device: &DeviceFunction,
        opcode: u32,
        input: Option<(&Registration<'_>, u64)>,
        output: Option<(&Registration<'_>, u64)>,
        user_data: u64,
    ) -> Self {
        Self {
            raw: abi::MygoSubmissionDescriptor {
                slot: abi::MYGO_SLOT_device_invoke,
                handle: device.raw(),
                arg0: u64::from(opcode),
                arg1: input.map_or(0, |(registration, _)| registration.token),
                arg2: output.map_or(0, |(registration, _)| registration.token),
                arg3: input.map_or(0, |(_, length)| length),
                arg4: output.map_or(0, |(_, length)| length),
                user_data,
            },
        }
    }
}

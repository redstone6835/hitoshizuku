//! 显式线程栈、TLS 与退出结果。

use core::marker::PhantomData;

use super::memory::MemoryObject;
use super::{OwnedHandle, Process, Status, abi, mrt_call};

pub enum ThreadObject {}

/// thread.create 的固定请求。
pub struct ThreadCreate<'a> {
    raw: abi::MygoThreadCreateRequest,
    marker: PhantomData<&'a MemoryObject>,
}

impl<'a> ThreadCreate<'a> {
    /// 使用一个 MemoryObject 区间作为独占线程栈。
    pub fn new(
        entry: unsafe extern "C" fn(u64) -> !,
        stack: &'a MemoryObject,
        stack_offset: u64,
        stack_size: u64,
        argument: u64,
    ) -> Self {
        Self {
            raw: abi::MygoThreadCreateRequest {
                entry: entry as usize as u64,
                stack_memory: stack.raw(),
                stack_offset,
                stack_size,
                tls_memory: 0,
                tls_offset: 0,
                argument,
                flags: 0,
            },
            marker: PhantomData,
        }
    }

    /// 为线程附加一个显式 TLS MemoryObject。
    pub fn with_tls(mut self, tls: &'a MemoryObject, tls_offset: u64) -> Self {
        self.raw.tls_memory = tls.raw();
        self.raw.tls_offset = tls_offset;
        self
    }
}

/// Native Thread capability；析构只关闭引用，不终止线程。
pub struct Thread {
    handle: OwnedHandle<ThreadObject>,
}

impl Process {
    /// 创建线程；栈、TLS 和入口均由请求显式给出。
    pub fn create_thread(&self, request: ThreadCreate<'_>) -> Result<Thread, Status> {
        if !abi::MYGO_HAS_thread_create {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_thread_create,
                self.raw(),
                &request.raw as *const _ as usize as u64,
                super::mrt_current_component(),
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        OwnedHandle::new(result.value0)
            .map(|handle| Thread { handle })
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))
    }
}

impl Thread {
    pub(crate) fn raw(&self) -> u64 {
        self.handle.raw()
    }

    /// 等待线程终止并读取完整结果。
    pub fn join(&self, deadline_ns: u64) -> Result<abi::MygoThreadResult, Status> {
        if !abi::MYGO_HAS_thread_join {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let mut thread_result = abi::MygoThreadResult::default();
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_thread_join,
                self.raw(),
                &mut thread_result as *mut _ as usize as u64,
                deadline_ns,
                0,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(thread_result)
        } else {
            Err(Status(result.status))
        }
    }

    /// 协作终止目标线程；不会因 handle 析构而隐式调用。
    pub fn terminate(&self, exit_code: u32) -> Result<(), Status> {
        if !abi::MYGO_HAS_thread_terminate {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_thread_terminate,
                self.raw(),
                u64::from(exit_code),
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

    /// 查询线程 identity、CPU 时间、TLS 和终止状态。
    pub fn query(&self) -> Result<abi::MygoThreadInfo, Status> {
        if !abi::MYGO_HAS_thread_query {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let mut info = abi::MygoThreadInfo::default();
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_thread_query,
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

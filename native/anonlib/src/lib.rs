#![no_std]

//! MyGO Native 的最小 Rust 安全对象接口。

use core::marker::PhantomData;
use core::num::NonZeroU64;

#[allow(dead_code)]
mod abi {
    include!(env!("MYGO_PROGRAM_RS"));
}

unsafe extern "C" {
    fn mrt_call(
        slot: u64,
        object_handle: u64,
        arg0: u64,
        arg1: u64,
        arg2: u64,
        arg3: u64,
        arg4: u64,
    ) -> abi::MygoNativeResult;
    fn mrt_initial_handle(requirement_id: u32) -> u64;
    fn mrt_terminate(status: u32) -> !;
    fn mrt_abort() -> !;
}

/// MyGO Native operation 返回的原始状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Status(u32);

impl Status {
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

/// 具备程序 manifest 所声明权限的 Stream capability。
pub struct Stream<'a> {
    handle: BorrowedHandle<'a, StreamObject>,
}

impl Stream<'_> {
    /// 将完整字节切片写入 Stream。
    pub fn write(&self, bytes: &[u8]) -> Result<usize, Status> {
        let length =
            u64::try_from(bytes.len()).map_err(|_| Status(abi::MYGO_STATUS_CORE_OUT_OF_RANGE))?;
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_STREAM_WRITE,
                self.handle.raw(),
                bytes.as_ptr() as usize as u64,
                length,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_OK {
            return Err(Status(result.status));
        }
        let written = usize::try_from(result.value0)
            .map_err(|_| Status(abi::MYGO_STATUS_CORE_OUT_OF_RANGE))?;
        if written > bytes.len() {
            return Err(Status(abi::MYGO_STATUS_CORE_OUT_OF_RANGE));
        }
        Ok(written)
    }
}

/// 获取启动环境授予的 stdout Stream。
pub fn stdout() -> Option<Stream<'static>> {
    let raw = unsafe { mrt_initial_handle(abi::MYGO_REQUIREMENT_STDOUT) };
    Some(Stream {
        handle: BorrowedHandle::from_raw(raw)?,
    })
}

/// 通过当前进程 capability 正常终止进程。
pub fn exit(status: u32) -> ! {
    unsafe { mrt_terminate(status) }
}

/// 以确定性异常路径终止当前映像。
pub fn abort() -> ! {
    unsafe { mrt_abort() }
}

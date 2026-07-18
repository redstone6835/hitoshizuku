//! 字符设备类型、驱动 trait 与全局字符设备列表。
//!
//! SMP 多核并发安全，无未定义行为。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicU8, Ordering};

pub use super::control::{CharControlRequest, CharControlResponse, ControlError, DriverControl};

// ─────────────────────────── I/O 错误 ────────────────────────────────────
/// 字符设备 I/O 错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharIoError {
    /// 硬件级错误（帧、奇偶校验、溢出等）。
    HardwareError,
    /// 设备不可用或已断开。
    Unavailable,
    /// 阻塞 I/O 被信号打断。
    Interrupted,
    /// 自旋等待超时（防止硬件故障导致死锁）。
    Timeout,
}

// ─────────────────────────── 数据 I/O trait ──────────────────────────────

/// 字符设备数据 I/O 接口。
///
/// 所有方法均通过 `&self` 调用——实现方在内部自行处理并发保护。
///
/// | 方法 | 阻塞？ | 语义 |
/// |---|---|---|
/// | [`write`](Self::write) | 否 | 尽量写入，返回实际接受的字节数（可为 0） |
/// | [`read`](Self::read) | 否 | 尽量读取，返回实际可用的字节数（可为 0） |
/// | [`flush`](Self::flush) | 是 | 阻塞直到内部缓冲全部排空 |
/// | [`write_all`](Self::write_all) | 是 | 阻塞直到 `buf` 全部写入 |
pub trait CharDriver: Send + Sync {
    /// 非阻塞写：将 `buf` 中尽可能多的字节送入设备。
    ///
    /// 返回实际被设备接受的字节数。`Ok(0)` 表示设备当前无法接受数据
    /// （如 FIFO 满），不代表错误。
    ///
    /// 实现可自由选择底层策略（FIFO 批量填充、DMA 提交等），
    /// 调用方只关心"写了多少"。
    fn write(&self, buf: &[u8]) -> Result<usize, CharIoError>;

    /// 非阻塞读：从设备读取可用字节填入 `buf`。
    ///
    /// 返回实际读取的字节数。`Ok(0)` 表示当前无可用数据。
    fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError>;

    /// 查询设备底层当前是否有可读数据。
    ///
    /// 这是给 `poll(2)`/`select(2)` 的非破坏性快照接口；TTY 行规程仍由
    /// devtmpfs 负责，驱动只暴露硬件接收 FIFO/软件接收队列是否非空。
    fn poll_read(&self) -> bool {
        false
    }

    /// 登记一个等待底层字符设备就绪的任务。
    ///
    /// 设备层只接收“读/写方向”这类通用 I/O 意图，不感知 `poll(2)` 的
    /// POSIX 位图。没有中断或内部等待队列的驱动保持默认返回 `false`，
    /// 上层会退化为定期重查 readiness。
    fn poll_add_waiter(
        &self,
        _task: &Arc<sched::Task>,
        _want_read: bool,
        _want_write: bool,
    ) -> bool {
        false
    }

    /// 移除此前通过 [`CharDriver::poll_add_waiter`] 登记的等待者。
    fn poll_remove_waiter(&self, _task: &Arc<sched::Task>) {}

    /// 阻塞等待设备内部写缓冲全部排空。
    ///
    /// 默认实现为空操作（无内部缓冲的设备）。
    fn flush(&self) -> Result<(), CharIoError> {
        Ok(())
    }

    /// 非阻塞轮询发送路径。
    ///
    /// 对带软件 TX 缓冲的驱动，此方法应尽可能把待发数据继续推到底层硬件；
    /// 对无内部发送缓冲的设备，默认实现为空操作。
    fn poll_write(&self) {}

    /// 执行字符设备类 typed control。
    ///
    /// 默认只把通用 drain 请求接到底层 [`flush`](Self::flush)，其它请求返回
    /// `Unsupported`。丢弃输入/输出队列需要驱动明确知道自己的缓冲结构，
    /// 不能由类层假定完成。
    fn control(&self, req: CharControlRequest) -> Result<CharControlResponse, ControlError> {
        match req {
            CharControlRequest::DrainTx => {
                self.flush().map_err(map_char_control_error)?;
                Ok(CharControlResponse::Done)
            }
            _ => Err(ControlError::Unsupported),
        }
    }

    /// 返回 `self` 的 `&dyn Any` 引用，用于向下转型到具体驱动类型。
    ///
    /// 类型安全控制路径应优先使用 [`CharDevice::control`]；`as_any` 只作为
    /// 少数调试或内部恢复具体驱动类型的逃生口。
    ///
    /// ```rust,ignore
    /// let _ = dev.control(CharControlRequest::DrainTx)?;
    /// ```
    ///
    /// 实现者只需写 `fn as_any(&self) -> &dyn Any { self }`。
    fn as_any(&self) -> &dyn Any;

    /// 阻塞写入整个缓冲区（带超时逃生，防止硬件故障导致死锁）。
    ///
    /// 驱动可覆盖此方法以获得更优实现（如等待 FIFO 空后一次性填满）。
    fn write_all(&self, buf: &[u8]) -> Result<(), CharIoError> {
        let mut remaining = buf;
        let mut retries = 0usize;
        while !remaining.is_empty() {
            let n = self.write(remaining)?;
            remaining = &remaining[n..];
            if n == 0 {
                retries += 1;
                if retries > 10_000_000 {
                    return Err(CharIoError::Timeout);
                }
                core::hint::spin_loop();
            } else {
                retries = 0;
            }
        }
        Ok(())
    }

    /// 设备是否具备 TTY/终端语义。覆盖返回 `true` 的设备会被 devtmpfs
    /// 创建为可交互终端节点（支持 termios/ioctl/job control 等）。
    fn is_tty(&self) -> bool {
        false
    }

    /// 设备是否可作为内核控制台输出。
    fn is_console(&self) -> bool {
        false
    }
}

impl<T: CharDriver + ?Sized> CharDriver for &'static T {
    fn write(&self, buf: &[u8]) -> Result<usize, CharIoError> {
        (**self).write(buf)
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
        (**self).read(buf)
    }

    fn poll_read(&self) -> bool {
        (**self).poll_read()
    }

    fn poll_add_waiter(&self, task: &Arc<sched::Task>, want_read: bool, want_write: bool) -> bool {
        (**self).poll_add_waiter(task, want_read, want_write)
    }

    fn poll_remove_waiter(&self, task: &Arc<sched::Task>) {
        (**self).poll_remove_waiter(task)
    }

    fn flush(&self) -> Result<(), CharIoError> {
        (**self).flush()
    }

    fn poll_write(&self) {
        (**self).poll_write();
    }

    fn control(&self, req: CharControlRequest) -> Result<CharControlResponse, ControlError> {
        (**self).control(req)
    }

    fn as_any(&self) -> &dyn Any {
        (**self).as_any()
    }

    fn write_all(&self, buf: &[u8]) -> Result<(), CharIoError> {
        (**self).write_all(buf)
    }

    fn is_tty(&self) -> bool {
        (**self).is_tty()
    }

    fn is_console(&self) -> bool {
        (**self).is_console()
    }
}

/// [`CharDevice::new`] 的驱动输入转换 helper。
#[doc(hidden)]
pub trait IntoCharDriverArc {
    fn into_char_driver_arc(self) -> Arc<dyn CharDriver>;
}

impl IntoCharDriverArc for Arc<dyn CharDriver> {
    fn into_char_driver_arc(self) -> Arc<dyn CharDriver> {
        self
    }
}

impl<T> IntoCharDriverArc for Arc<T>
where
    T: CharDriver + 'static,
{
    fn into_char_driver_arc(self) -> Arc<dyn CharDriver> {
        self
    }
}

impl<T> IntoCharDriverArc for &'static T
where
    T: CharDriver + ?Sized + 'static,
{
    fn into_char_driver_arc(self) -> Arc<dyn CharDriver> {
        Arc::new(self)
    }
}

fn map_char_control_error(err: CharIoError) -> ControlError {
    match err {
        CharIoError::HardwareError => ControlError::Io,
        CharIoError::Unavailable => ControlError::NoDevice,
        CharIoError::Interrupted => ControlError::Busy,
        CharIoError::Timeout => ControlError::Busy,
    }
}

// ─────────────────────────── 字符设备条目 ─────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CharDeviceState {
    Active = 0,
    Gone = 1,
}

struct CharDeviceInner {
    fw_name: Box<str>,
    driver: Arc<dyn CharDriver>,
    state: AtomicU8,
}

/// 一个已注册的字符设备句柄。
///
/// 句柄可克隆，但所有克隆共享同一个 active/gone 状态。设备注销后，旧的 `/dev`
/// inode、打开文件和注册表快照都会通过该状态返回 `Unavailable`，而不是继续访问
/// 已下线设备。
#[derive(Clone)]
pub struct CharDevice {
    inner: Arc<CharDeviceInner>,
}

struct NullCharDriver;
struct ZeroCharDriver;

impl CharDriver for NullCharDriver {
    fn write(&self, buf: &[u8]) -> Result<usize, CharIoError> {
        Ok(buf.len())
    }

    fn read(&self, _buf: &mut [u8]) -> Result<usize, CharIoError> {
        Ok(0)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl CharDriver for ZeroCharDriver {
    fn write(&self, buf: &[u8]) -> Result<usize, CharIoError> {
        Ok(buf.len())
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn poll_read(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[kernel_symbols::export]
impl CharDevice {
    #[inline]
    pub fn new<N, D>(fw_name: N, driver: D) -> Self
    where
        N: Into<Box<str>>,
        D: IntoCharDriverArc,
    {
        Self {
            inner: Arc::new(CharDeviceInner {
                fw_name: fw_name.into(),
                driver: driver.into_char_driver_arc(),
                state: AtomicU8::new(CharDeviceState::Active as u8),
            }),
        }
    }

    /// 在常驻内核侧构造字符设备对象，供动态 ELM 传入自身驱动 trait object。
    #[kernel_symbols::export(
        name = "general.dev.char.CharDevice.from_arc",
        contract = "kernel.general.char-device@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
        retained_args = 2u64
    )]
    pub fn from_arc(fw_name: Box<str>, driver: Arc<dyn CharDriver>) -> Self {
        Self {
            inner: Arc::new(CharDeviceInner {
                fw_name,
                driver,
                state: AtomicU8::new(CharDeviceState::Active as u8),
            }),
        }
    }

    pub fn null() -> Self {
        Self::new("null", Arc::new(NullCharDriver))
    }

    pub fn zero() -> Self {
        Self::new("zero", Arc::new(ZeroCharDriver))
    }

    /// 是否具备 TTY 语义（终端）。委托至底层驱动。
    #[inline]
    pub fn is_tty(&self) -> bool {
        self.inner.driver.is_tty()
    }

    /// 是否可作为内核控制台输出。委托至底层驱动。
    #[inline]
    pub fn is_console(&self) -> bool {
        self.inner.driver.is_console()
    }

    #[inline]
    pub fn fw_name(&self) -> &str {
        self.inner.fw_name.as_ref()
    }

    #[inline]
    pub fn state(&self) -> CharDeviceState {
        match self.inner.state.load(Ordering::Acquire) {
            0 => CharDeviceState::Active,
            _ => CharDeviceState::Gone,
        }
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.state() == CharDeviceState::Active
    }

    #[inline]
    pub fn mark_gone(&self) {
        self.inner
            .state
            .store(CharDeviceState::Gone as u8, Ordering::Release);
    }

    /// 非阻塞写（委托至驱动）。
    #[inline]
    pub fn write(&self, buf: &[u8]) -> Result<usize, CharIoError> {
        if !self.is_active() {
            return Err(CharIoError::Unavailable);
        }
        self.inner.driver.write(buf)
    }

    /// 非阻塞读（委托至驱动）。
    #[inline]
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
        if !self.is_active() {
            return Err(CharIoError::Unavailable);
        }
        self.inner.driver.read(buf)
    }

    /// 查询底层设备当前是否有可读字节（委托至驱动）。
    #[inline]
    pub fn poll_read(&self) -> bool {
        self.is_active() && self.inner.driver.poll_read()
    }

    /// 将任务挂到字符设备自己的 I/O 等待源上。
    #[inline]
    pub fn poll_add_waiter(
        &self,
        task: &Arc<sched::Task>,
        want_read: bool,
        want_write: bool,
    ) -> bool {
        self.is_active()
            && self
                .inner
                .driver
                .poll_add_waiter(task, want_read, want_write)
    }

    /// 从字符设备自己的 I/O 等待源移除任务。
    #[inline]
    pub fn poll_remove_waiter(&self, task: &Arc<sched::Task>) {
        if self.is_active() {
            self.inner.driver.poll_remove_waiter(task);
        }
    }

    /// 阻塞排空（委托至驱动）。
    #[inline]
    pub fn flush(&self) -> Result<(), CharIoError> {
        if !self.is_active() {
            return Err(CharIoError::Unavailable);
        }
        self.inner.driver.flush()
    }

    /// 阻塞写入全部字节（委托至驱动）。
    #[inline]
    pub fn write_all(&self, buf: &[u8]) -> Result<(), CharIoError> {
        if !self.is_active() {
            return Err(CharIoError::Unavailable);
        }
        self.inner.driver.write_all(buf)
    }

    /// 非阻塞轮询驱动内部发送路径（若有）。
    #[inline]
    pub fn poll_write(&self) {
        if self.is_active() {
            self.inner.driver.poll_write();
        }
    }

    /// 执行字符设备类 typed control。
    #[inline]
    pub fn control(&self, req: CharControlRequest) -> Result<CharControlResponse, ControlError> {
        if !self.is_active() {
            return Err(ControlError::NoDevice);
        }
        self.inner.driver.control(req)
    }

    /// 尝试将驱动向下转型为具体类型 `T`。
    ///
    /// 成功时返回 `Some(&T)`，类型不匹配时返回 `None`。
    /// 通用 ioctl/VFS 路径不应使用此方法做设备类型分派；它们应调用
    /// [`CharDevice::control`]。本方法仅保留给少数确实需要具体驱动类型的
    /// 内核内部路径。
    ///
    /// ```rust,ignore
    /// assert!(dev.downcast_driver::<MyDebugDriver>().is_none());
    /// ```
    #[inline]
    pub fn downcast_driver<T: 'static>(&self) -> Option<&T> {
        if !self.is_active() {
            return None;
        }
        self.inner.driver.as_any().downcast_ref::<T>()
    }
}

impl core::fmt::Debug for CharDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CharDev")
            .field("fw_name", &self.fw_name())
            .field("state", &self.state())
            .finish()
    }
}

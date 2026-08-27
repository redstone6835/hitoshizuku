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
    /// 无剩余空间(如 /dev/full 的写)。
    NoSpace,
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
    /// 窗口大小变化通知(pts 对端同步等)。
    fn winsize_changed(&self, _winsize: crate::vfs::user_api::tty::UserWinSize) {}

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
        CharIoError::NoSpace => ControlError::Invalid,
        CharIoError::HardwareError => ControlError::Io,
        CharIoError::Unavailable => ControlError::NoDevice,
        CharIoError::Interrupted => ControlError::Busy,
        CharIoError::Timeout => ControlError::Busy,
    }
}

/// 常驻字符设备对象持有的动态 ELM 驱动代理。
///
/// `CharDriver` 的 trait vtable 和 drop glue 可能位于可卸载镜像中。代理只缓存
/// 不会变化的设备类型元数据；所有会进入动态实现的操作都先恢复创建对象时捕获的
/// 完整 ELM 上下文。无法恢复时不再触碰动态 vtable，并把对应 generation 标记为失败。
struct ElmCharDriverProxy {
    context: elm_model::ElmCurrentContext,
    is_tty: bool,
    is_console: bool,
    driver: Option<Arc<dyn CharDriver>>,
}

impl ElmCharDriverProxy {
    fn wrap(
        driver: Arc<dyn CharDriver>,
        context: elm_model::ElmCurrentContext,
    ) -> Arc<dyn CharDriver> {
        // `from_arc` 在动态调用边界内执行，因此这里仍可安全读取一次稳定元数据。
        // 后续查询只读代理缓存，避免为类型判断进入可卸载代码。
        let is_tty = driver.is_tty();
        let is_console = driver.is_console();
        Arc::new(Self {
            context,
            is_tty,
            is_console,
            driver: Some(driver),
        })
    }

    fn driver(&self) -> &dyn CharDriver {
        self.driver
            .as_deref()
            .expect("ELM char driver proxy used after drop")
    }

    fn enter(&self, operation: &'static str) -> Option<elm_model::ElmCurrentContextGuard> {
        let guard = super::pnp::enter_elm_snapshot(self.context);
        if guard.is_none() {
            log::error!(
                "[char] cannot enter ELM context for driver operation {}: cell={} generation={}",
                operation,
                self.context.cell_id.0,
                self.context.generation.0
            );
            super::elm_lifecycle::mark_context_failed(self.context);
        }
        guard
    }
}

impl CharDriver for ElmCharDriverProxy {
    fn write(&self, buf: &[u8]) -> Result<usize, CharIoError> {
        let Some(_guard) = self.enter("write") else {
            return Err(CharIoError::Unavailable);
        };
        self.driver().write(buf)
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
        let Some(_guard) = self.enter("read") else {
            return Err(CharIoError::Unavailable);
        };
        self.driver().read(buf)
    }

    fn poll_read(&self) -> bool {
        let Some(_guard) = self.enter("poll_read") else {
            return false;
        };
        self.driver().poll_read()
    }

    fn poll_add_waiter(&self, task: &Arc<sched::Task>, want_read: bool, want_write: bool) -> bool {
        let Some(_guard) = self.enter("poll_add_waiter") else {
            return false;
        };
        self.driver().poll_add_waiter(task, want_read, want_write)
    }

    fn poll_remove_waiter(&self, task: &Arc<sched::Task>) {
        let Some(_guard) = self.enter("poll_remove_waiter") else {
            return;
        };
        self.driver().poll_remove_waiter(task);
    }

    fn flush(&self) -> Result<(), CharIoError> {
        let Some(_guard) = self.enter("flush") else {
            return Err(CharIoError::Unavailable);
        };
        self.driver().flush()
    }

    fn poll_write(&self) {
        let Some(_guard) = self.enter("poll_write") else {
            return;
        };
        self.driver().poll_write();
    }

    fn winsize_changed(&self, winsize: crate::vfs::user_api::tty::UserWinSize) {
        let Some(_guard) = self.enter("winsize_changed") else {
            return;
        };
        self.driver().winsize_changed(winsize);
    }

    fn control(&self, req: CharControlRequest) -> Result<CharControlResponse, ControlError> {
        let Some(_guard) = self.enter("control") else {
            return Err(ControlError::NoDevice);
        };
        self.driver().control(req)
    }

    fn as_any(&self) -> &dyn Any {
        // 不允许动态实现的 `Any` vtable 引用逃出上下文 guard。
        self
    }

    fn write_all(&self, buf: &[u8]) -> Result<(), CharIoError> {
        let Some(_guard) = self.enter("write_all") else {
            return Err(CharIoError::Unavailable);
        };
        self.driver().write_all(buf)
    }

    fn is_tty(&self) -> bool {
        self.is_tty
    }

    fn is_console(&self) -> bool {
        self.is_console
    }
}

impl Drop for ElmCharDriverProxy {
    fn drop(&mut self) {
        let Some(driver) = self.driver.take() else {
            return;
        };
        let Some(_guard) = self.enter("drop") else {
            // 不能恢复精确 generation 时不得执行动态 drop glue。泄漏最后一个 Arc
            // 并让生命周期失败，优先避免跳进已经卸载或正在替换的镜像。
            core::mem::forget(driver);
            return;
        };
        drop(driver);
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
        let driver = match elm_model::current_context() {
            Some(context) => ElmCharDriverProxy::wrap(driver, context),
            None => driver,
        };
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
    /// 通知驱动窗口大小变化(pts 对端同步等)。
    pub fn winsize_changed(&self, winsize: crate::vfs::user_api::tty::UserWinSize) {
        self.inner.driver.winsize_changed(winsize);
    }

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

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::any::Any;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::vfs::sync::Spinlock;

    static TEST_ELM_CONTEXT_STATE: Spinlock<()> = Spinlock::new(());

    struct ContextRecordingCharDriver {
        expected: elm_model::ElmCurrentContext,
        calls: Arc<AtomicUsize>,
        metadata_calls: Arc<AtomicUsize>,
        any_calls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl ContextRecordingCharDriver {
        fn record(&self, counter: &AtomicUsize) {
            assert_eq!(elm_model::current_context(), Some(self.expected));
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl CharDriver for ContextRecordingCharDriver {
        fn write(&self, buf: &[u8]) -> Result<usize, CharIoError> {
            self.record(&self.calls);
            Ok(buf.len())
        }

        fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
            self.record(&self.calls);
            buf.fill(0x5a);
            Ok(buf.len())
        }

        fn poll_read(&self) -> bool {
            self.record(&self.calls);
            true
        }

        fn flush(&self) -> Result<(), CharIoError> {
            self.record(&self.calls);
            Ok(())
        }

        fn poll_write(&self) {
            self.record(&self.calls);
        }

        fn winsize_changed(&self, _winsize: crate::vfs::user_api::tty::UserWinSize) {
            self.record(&self.calls);
        }

        fn control(&self, _req: CharControlRequest) -> Result<CharControlResponse, ControlError> {
            self.record(&self.calls);
            Ok(CharControlResponse::Done)
        }

        fn as_any(&self) -> &dyn Any {
            self.record(&self.any_calls);
            self
        }

        fn write_all(&self, _buf: &[u8]) -> Result<(), CharIoError> {
            self.record(&self.calls);
            Ok(())
        }

        fn is_tty(&self) -> bool {
            self.record(&self.metadata_calls);
            true
        }

        fn is_console(&self) -> bool {
            self.record(&self.metadata_calls);
            true
        }
    }

    impl Drop for ContextRecordingCharDriver {
        fn drop(&mut self) {
            assert_eq!(elm_model::current_context(), Some(self.expected));
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct ResidentCharDriver;

    impl CharDriver for ResidentCharDriver {
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

    fn test_elm_context(
        cell: u64,
        generation: u64,
        phase: elm_model::ElmLifecyclePhase,
        flags: u32,
        allowed_actions: u32,
    ) -> elm_model::ElmContext {
        elm_model::ElmContext::new(
            elm_model::ElmId(cell),
            Some(elm_model::ElmId(cell + 1000)),
            elm_model::Generation(generation),
            elm_model::ElmState::Active,
            phase,
            flags,
        )
        .with_kind(elm_model::ElmKind::Driver)
        .with_allowed_actions(allowed_actions)
    }

    #[test]
    fn elm_proxy_restores_owner_context_without_wrapping_resident_driver() {
        let _context_state = TEST_ELM_CONTEXT_STATE.lock();
        assert!(elm_model::current_context().is_none());

        let owner = test_elm_context(
            0xc401,
            7,
            elm_model::ElmLifecyclePhase::Initialize,
            0x55aa,
            0x12d,
        );
        let owner_snapshot = elm_model::ElmCurrentContext::from_context(&owner);
        let calls = Arc::new(AtomicUsize::new(0));
        let metadata_calls = Arc::new(AtomicUsize::new(0));
        let any_calls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let dynamic_device = {
            let _owner_guard = elm_model::enter_current_context(&owner).unwrap();
            CharDevice::from_arc(
                "elm-context-char".into(),
                Arc::new(ContextRecordingCharDriver {
                    expected: owner_snapshot,
                    calls: Arc::clone(&calls),
                    metadata_calls: Arc::clone(&metadata_calls),
                    any_calls: Arc::clone(&any_calls),
                    drops: Arc::clone(&drops),
                }),
            )
        };
        assert_eq!(metadata_calls.load(Ordering::Relaxed), 2);
        assert!(elm_model::current_context().is_none());

        let outer = test_elm_context(
            0xc402,
            11,
            elm_model::ElmLifecyclePhase::Resume,
            0xa55a,
            0x3,
        );
        let outer_snapshot = elm_model::ElmCurrentContext::from_context(&outer);
        {
            let _outer_guard = elm_model::enter_current_context(&outer).unwrap();
            assert_eq!(dynamic_device.write(b"abc"), Ok(3));
            let mut read = [0_u8; 2];
            assert_eq!(dynamic_device.read(&mut read), Ok(2));
            assert_eq!(read, [0x5a; 2]);
            assert!(dynamic_device.poll_read());
            assert_eq!(dynamic_device.flush(), Ok(()));
            assert_eq!(dynamic_device.write_all(b"def"), Ok(()));
            dynamic_device.poll_write();
            dynamic_device
                .winsize_changed(crate::vfs::user_api::tty::UserWinSize::default_console());
            assert_eq!(
                dynamic_device.control(CharControlRequest::DrainTx),
                Ok(CharControlResponse::Done)
            );
            assert!(dynamic_device.is_tty());
            assert!(dynamic_device.is_console());
            assert!(
                dynamic_device
                    .downcast_driver::<ContextRecordingCharDriver>()
                    .is_none()
            );
            assert_eq!(elm_model::current_context(), Some(outer_snapshot));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 8);
        assert_eq!(metadata_calls.load(Ordering::Relaxed), 2);
        assert_eq!(any_calls.load(Ordering::Relaxed), 0);
        drop(dynamic_device);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(elm_model::current_context().is_none());

        let resident: Arc<dyn CharDriver> = Arc::new(ResidentCharDriver);
        let resident_device = CharDevice::from_arc("resident-char".into(), resident);
        assert!(
            resident_device
                .downcast_driver::<ResidentCharDriver>()
                .is_some()
        );
    }
}

//! 进程镜像、用户上下文和信号帧的外部能力注入点。
//!
//! `sched` 拥有 exec/fork/sigreturn 的状态机，但不直接依赖 ELF loader、用户
//! 地址空间或 trap-frame 布局。上层内核注册这些 ops 后，`sched::operation`
//! 即可完成语义调度；未注册时返回 `ENOSYS`，不会留下半初始化任务。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicPtr, Ordering};

use errno::Errno;

use crate::clone_flags::CloneArgs;
use crate::signal::{SigAction, SigInfo};
use crate::task::Task;

/// 架构 trap frame / 用户上下文的不透明引用。`sched` 不解释它，只转交给
/// kernel/hal 注册的 ops；0 表示调用点没有可用的用户上下文。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct UserContextRef(usize);

impl UserContextRef {
    pub const NONE: Self = Self(0);

    pub const fn new(raw: usize) -> Self {
        Self(raw)
    }

    pub const fn as_usize(self) -> usize {
        self.0
    }

    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// execve 的用户 ABI 参数。指针值由上层 ops 按当前地址空间解释。
#[derive(Debug, Clone, Copy)]
pub struct ExecRequest {
    pub path_user: usize,
    pub argv_user: usize,
    pub envp_user: usize,
}

impl ExecRequest {
    pub const fn new(path_user: usize, argv_user: usize, envp_user: usize) -> Self {
        Self {
            path_user,
            argv_user,
            envp_user,
        }
    }
}

/// 用户执行路径相关 ops。
pub struct ProcessImageOps {
    /// 用新镜像替换 `task` 的用户地址空间和返回上下文。
    pub execve:
        fn(task: &Arc<Task>, request: ExecRequest, user_ctx: UserContextRef) -> Result<(), Errno>,
    /// 为 fork/clone 出来的 child 安装首次返回用户态所需的上下文。
    pub clone_user_context: fn(
        parent: &Arc<Task>,
        child: &Arc<Task>,
        args: CloneArgs,
        user_ctx: UserContextRef,
    ) -> Result<(), Errno>,
    /// 从当前 signal frame 恢复用户态上下文。
    pub sigreturn: fn(task: &Arc<Task>, user_ctx: UserContextRef) -> Result<(), Errno>,
    /// 为用户 handler 构造 signal frame。
    pub setup_signal_frame: fn(
        task: &Arc<Task>,
        info: SigInfo,
        action: SigAction,
        user_ctx: UserContextRef,
    ) -> Result<(), Errno>,
}

unsafe impl Sync for ProcessImageOps {}
unsafe impl Send for ProcessImageOps {}

static PROCESS_IMAGE_OPS: AtomicPtr<ProcessImageOps> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_process_image_ops(ops: &'static ProcessImageOps) {
    let ptr = ops as *const _ as *mut _;
    match PROCESS_IMAGE_OPS.compare_exchange(
        core::ptr::null_mut(),
        ptr,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(prev) if prev == ptr => {}
        Err(_) => panic!("[sched] ProcessImageOps already registered"),
    }
}

pub fn process_image_ops() -> Option<&'static ProcessImageOps> {
    let ptr = PROCESS_IMAGE_OPS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // Safety: register_process_image_ops only stores 'static ops.
        Some(unsafe { &*(ptr as *const ProcessImageOps) })
    }
}

//! 架构无关的系统调用分发。
//!
//! 本模块负责两件事：
//!
//! 1. **arch 注入契约 [`SyscallFrameOps`]**：从 trap frame 取号、读参、写返回、
//!    推进 PC 的 4 个回调。这是 arch 唯一对 syscall 子系统的耦合点。
//! 2. **表驱动分发**：[`SYSCALL_TABLE`] 是 `[Option<SyscallFn>; SYSCALL_TABLE_LEN]`，
//!    [`register_syscall`] 由 kernel 启动期填表，[`dispatch`] 在 trap 进来时
//!    构造 [`SyscallContext`] 并调用对应条目。
//!
//! ## 调用契约
//!
//! 表里每个条目都是 [`SyscallFn`] = `fn(&mut SyscallContext) -> Result<usize, Errno>`：
//! - 返回 `Ok(v)` → 写回 `v as isize`；
//! - 返回 `Err(e)` → 写回 `-(e.as_i32() as isize)`；
//! - dispatch 末尾默认写返回值并 `advance_pc` 推 PC；`execve/sigreturn` 这类
//!   已经重写完整 trap frame 的 syscall 可在 context 上关闭默认收尾。
//!
//! [`SyscallContext`] 把 trap frame、当前 task、syscall 号、参数四件事打包，
//! 让 syscall fn 既能读用户态寄存器（execve / sigreturn 这种要改 PC 的特例
//! 走 ctx.tf 直接改），也能 `ctx.task.ext_lookup` 拿 fdtable / vmspace。

use alloc::sync::Arc;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use errno::Errno;

use crate::TrapFramePtr;

// ── 1. arch 注入契约 ─────────────────────────────────────────────────────────

/// arch 提供的 trap-frame 字段访问契约。
#[repr(C)]
pub struct SyscallFrameOps {
    /// 取 syscall 号，具体寄存器由架构 ABI 决定。
    pub sys_nr: fn(TrapFramePtr) -> usize,
    /// 取六个 syscall 参数。Linux 通用。
    pub sys_args: fn(TrapFramePtr) -> [usize; 6],
    /// 写返回值（通常进 a0 / rax）。负数即 -errno。
    pub set_sys_ret: fn(TrapFramePtr, isize),
    /// 把 PC 跨过 syscall 指令本身，步长由架构 ABI 决定。
    pub advance_pc: fn(TrapFramePtr),
}

unsafe impl Sync for SyscallFrameOps {}
unsafe impl Send for SyscallFrameOps {}

static FRAME_OPS: AtomicPtr<SyscallFrameOps> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_frame_ops(ops: &'static SyscallFrameOps) {
    FRAME_OPS.store(ops as *const _ as *mut _, Ordering::Release);
}

pub fn frame_ops() -> Option<&'static SyscallFrameOps> {
    let ptr = FRAME_OPS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // Safety: 仅由 register_frame_ops 写入 'static 指针；Acquire/Release 配对。
        Some(unsafe { &*(ptr as *const SyscallFrameOps) })
    }
}

pub fn frame_ops_registered() -> bool {
    frame_ops().is_some()
}

// ── 2. SyscallContext + 表 ───────────────────────────────────────────────────

/// 单次系统调用的上下文。
///
/// `tf` 让需要直接改 PC / 寄存器的特殊 syscall（execve / sigreturn）能拿到
/// 完整 trap frame；普通 syscall 只用 `args` + `task` 即可。
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum TaskOwnership {
    Owned,
    Borrowed,
    Released,
}

pub struct SyscallContext<'a> {
    pub nr: usize,
    pub args: [usize; 6],
    pub tf: TrapFramePtr,
    task: ManuallyDrop<Arc<sched::Task>>,
    task_ownership: TaskOwnership,
    frame_finalized: bool,
    restart_disabled: bool,
    execution_scope_active: bool,
    _phantom: core::marker::PhantomData<(&'a (), *mut ())>,
}

impl<'a> SyscallContext<'a> {
    fn new(nr: usize, args: [usize; 6], tf: TrapFramePtr, task: Arc<sched::Task>) -> Self {
        Self {
            nr,
            args,
            tf,
            task: ManuallyDrop::new(task),
            task_ownership: TaskOwnership::Owned,
            frame_finalized: false,
            restart_disabled: false,
            execution_scope_active: false,
            _phantom: core::marker::PhantomData,
        }
    }

    /// RISC-V syscall 热路径借用调度器 current 槽，不修改 `Arc` 强引用计数。
    #[inline(always)]
    fn new_borrowed(
        nr: usize,
        args: [usize; 6],
        tf: TrapFramePtr,
        task: &'a Arc<sched::Task>,
    ) -> Self {
        Self {
            nr,
            args,
            tf,
            // Safety: task 指向 current 槽托底的 Arc allocation。该 Arc 视图由
            // ManuallyDrop 包装，borrowed context 的结束路径绝不会减少强引用。
            task: ManuallyDrop::new(unsafe { Arc::from_raw(Arc::as_ptr(task)) }),
            task_ownership: TaskOwnership::Borrowed,
            frame_finalized: false,
            restart_disabled: false,
            execution_scope_active: false,
            _phantom: core::marker::PhantomData,
        }
    }

    /// 在当前 syscall 第一次进入网络协议栈前建立有界执行作用域。
    ///
    /// 普通 syscall 不消费网络栈调用预算，因此不应为它们修改任务上的原子状态。
    /// 网络 syscall 可以经过多个通用文件 I/O 分支，本方法保持幂等，由首次确认
    /// `NetSocketFileOps` 的分支负责调用，最终仍由 context 的 RAII 边界统一结束。
    pub fn ensure_network_execution_scope(&mut self) {
        if self.execution_scope_active {
            return;
        }
        assert!(
            self.task()
                .begin_execution_scope(sched::ExecutionScopeKind::Syscall),
            "同一任务不能嵌套进入 syscall 执行作用域"
        );
        self.execution_scope_active = true;
    }

    #[inline(always)]
    fn finish_execution_scope(&mut self) {
        if !self.execution_scope_active {
            return;
        }
        if self.task_ownership != TaskOwnership::Released {
            let _ = self
                .task()
                .end_execution_scope(sched::ExecutionScopeKind::Syscall);
        }
        self.execution_scope_active = false;
    }

    #[inline(always)]
    pub fn task(&self) -> &Arc<sched::Task> {
        debug_assert!(
            self.task_ownership != TaskOwnership::Released,
            "[syscall] task already released"
        );
        &self.task
    }

    /// 在不会再返回或访问本 context 的退出路径提前释放拥有型 task 引用。
    ///
    /// # Safety
    /// 调用后不得再调用 [`SyscallContext::task`]，并且控制流必须立即进入不返回的
    /// 任务退出或最终调度路径。
    pub unsafe fn release_task_ref(&mut self) {
        self.finish_execution_scope();
        let ownership = core::mem::replace(&mut self.task_ownership, TaskOwnership::Released);
        if ownership == TaskOwnership::Owned {
            // Safety: Owned 只由 new 构造且尚未释放，此处恰好消费一次强引用。
            unsafe { ManuallyDrop::drop(&mut self.task) };
        }
    }

    /// 结束由 `new_borrowed` 构造的同步 syscall context，不触碰 Arc 强引用计数。
    #[inline(always)]
    fn finish_borrowed(&mut self) {
        self.finish_execution_scope();
        debug_assert!(self.task_ownership != TaskOwnership::Owned);
        self.task_ownership = TaskOwnership::Released;
    }

    /// 标记当前 syscall 已经完整重写 trap frame。dispatch 不再写 syscall
    /// 返回值、不推进 PC，也不在本次返回前投递 signal frame。
    pub fn finalize_frame(&mut self) {
        self.frame_finalized = true;
    }

    pub fn frame_finalized(&self) -> bool {
        self.frame_finalized
    }

    /// 禁止 `SA_RESTART` 自动重新执行本次系统调用。
    ///
    /// Linux 对 `nanosleep`、`clock_nanosleep` 等调用明确要求把 `EINTR` 暴露给
    /// 用户态，即使信号动作带有 `SA_RESTART`。这类 syscall 必须在执行前设置
    /// 本标志，避免通用分发器错误恢复到原 syscall 指令。
    pub fn disable_restart(&mut self) {
        self.restart_disabled = true;
    }

    /// 查询当前系统调用是否禁止 `SA_RESTART` 自动重启。
    pub fn restart_disabled(&self) -> bool {
        self.restart_disabled
    }
}

impl Drop for SyscallContext<'_> {
    fn drop(&mut self) {
        self.finish_execution_scope();
        if self.task_ownership == TaskOwnership::Owned {
            // Safety: Owned context 的强引用尚未由 release_task_ref 消费。
            unsafe { ManuallyDrop::drop(&mut self.task) };
            self.task_ownership = TaskOwnership::Released;
        }
    }
}

/// 标准 syscall 函数签名。
pub type SyscallFn = fn(&mut SyscallContext<'_>) -> Result<usize, Errno>;

/// 表大小：覆盖 Linux asm-generic 全部 syscall 号（最大约 450）。
pub const SYSCALL_TABLE_LEN: usize = 512;

static SYSCALL_TABLE: [AtomicUsize; SYSCALL_TABLE_LEN] =
    [const { AtomicUsize::new(0) }; SYSCALL_TABLE_LEN];

#[cfg(feature = "trace-task-lifecycle")]
fn trace_signal_boundary(nr: usize) -> bool {
    // LoongArch 与 RISC-V 均采用 asm-generic 的 kill 系统调用号。
    nr == 129
}

/// 在启动期注册一个 syscall 号 → fn 的映射。重复注册会 panic（防止表条目被
/// 静默覆盖）。
pub fn register_syscall(nr: usize, f: SyscallFn) {
    assert!(
        nr < SYSCALL_TABLE_LEN,
        "[syscall] nr {} out of range (max {})",
        nr,
        SYSCALL_TABLE_LEN - 1
    );
    let old =
        SYSCALL_TABLE[nr].compare_exchange(0, f as usize, Ordering::AcqRel, Ordering::Acquire);
    assert!(old.is_ok(), "[syscall] nr {} already registered", nr);
}

/// 启动期已注册的 syscall 数量；smoketest / debug 用。
pub fn registered_count() -> usize {
    SYSCALL_TABLE
        .iter()
        .filter(|e| e.load(Ordering::Acquire) != 0)
        .count()
}

/// syscall 实现已经返回，此时深层调用栈中的 VmSpace/File 等临时 Arc 均已析构；
/// 在这个边界消费 exit_group 请求，避免远程废弃另一个线程的 Rust 栈。
#[inline]
fn complete_group_exit_at_boundary(ctx: &mut SyscallContext<'_>) {
    if !sched::operation::complete_group_exit_if_requested(ctx.task()) {
        return;
    }
    // Safety: group-exit 完成后立即最终调度；本 context 不会再被访问。
    unsafe { ctx.release_task_ref() };
    sched::schedule_once(0);
    panic!("[syscall] group-exit task scheduled back unexpectedly");
}

/// `EINTR + SA_RESTART` 的低频信号帧构造路径。
#[cold]
#[inline(never)]
fn try_restart_syscall_signal(ctx: &mut SyscallContext<'_>, tf: TrapFramePtr) -> bool {
    let Some((info, action)) = sched::operation::consume_restartable_signal() else {
        return false;
    };
    if sched::operation::setup_user_signal_frame_for_task(
        ctx.task(),
        info,
        action,
        sched::UserContextRef::new(tf.as_usize()),
    )
    .is_err()
    {
        return false;
    }
    #[cfg(feature = "performance-profile")]
    let _handoff_profile =
        profiling::scope(profiling::Event::SyscallHandoff).trace_args(ctx.nr as u64, 0);
    sched::run_post_syscall_handoff(sched::now_ns_direct());
    true
}

// ── 3. 主分发 ────────────────────────────────────────────────────────────────

/// arch 的 ECODE_SYS 分支调用。把 trap frame 翻成 [`SyscallContext`] 后查表
/// 调用；写返回 + 推 PC 在末尾统一执行。
pub fn dispatch(tf: TrapFramePtr) {
    let Some(ops) = frame_ops() else {
        return;
    };
    let nr = (ops.sys_nr)(tf);
    let args = (ops.sys_args)(tf);

    // 取 current task；sched::init 之前不应触发用户 syscall，但安全起见做防御。
    if !sched::is_ready_direct() {
        log::debug!("[syscall] dispatch before sched ready, nr={}", nr);
        (ops.set_sys_ret)(tf, -(Errno::ENOSYS.as_i32_direct() as isize));
        (ops.advance_pc)(tf);
        return;
    }
    #[cfg(feature = "performance-profile")]
    let _span = profiling::enter_span();
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::SyscallDispatch).trace_args(nr as u64, 0);
    #[cfg(feature = "performance-profile")]
    let mut syscall_profile = profiling::syscall_scope(nr);

    let task = sched::current_task_direct();
    let mut ctx = SyscallContext::new(nr, args, tf, task);

    // syscall 表只在启动期注册；热路径无锁读取函数指针，避免 lmbench
    // simple syscall 每次都争用全局自旋锁。
    let entry = if nr < SYSCALL_TABLE_LEN {
        // 表在用户任务启动前完成注册且之后只读，等价于 Linux 的静态 syscall 表。
        let ptr = SYSCALL_TABLE[nr].load(Ordering::Relaxed);
        if ptr == 0 {
            None
        } else {
            // Safety: register_syscall 只写入 SyscallFn 函数指针，且条目一旦设置不再修改。
            Some(unsafe { core::mem::transmute::<usize, SyscallFn>(ptr) })
        }
    } else {
        None
    };

    #[cfg(feature = "performance-profile")]
    let invoke_profile = profiling::scope(profiling::Event::SyscallInvoke).trace_args(nr as u64, 0);
    let ret: isize = match entry {
        Some(f) => match f(&mut ctx) {
            Ok(v) => v as isize,
            Err(e) => -(e.as_i32_direct() as isize),
        },
        None => -(Errno::ENOSYS.as_i32_direct() as isize),
    };
    #[cfg(feature = "trace-task-lifecycle")]
    if trace_signal_boundary(nr) {
        log::info!(
            "[syscall][signal-boundary] invoke-done pid={:?} nr={} ret={}",
            ctx.task().pid_root(),
            nr,
            ret,
        );
    }
    #[cfg(feature = "performance-profile")]
    syscall_profile.set_result(ret);
    #[cfg(feature = "performance-profile")]
    drop(invoke_profile);

    complete_group_exit_at_boundary(&mut ctx);

    let frame_finalized = ctx.frame_finalized();
    if !frame_finalized {
        #[cfg(feature = "performance-profile")]
        let finalize_profile =
            profiling::scope(profiling::Event::SyscallFinalize).trace_args(nr as u64, 0);
        if ret == -(Errno::EINTR.as_i32_direct() as isize) && !ctx.restart_disabled() {
            if let Some((info, action)) = sched::operation::consume_restartable_signal() {
                let delivered = sched::operation::setup_user_signal_frame_for_task(
                    ctx.task(),
                    info,
                    action,
                    sched::UserContextRef::new(tf.as_usize()),
                )
                .is_ok();
                if delivered {
                    #[cfg(feature = "performance-profile")]
                    drop(finalize_profile);
                    #[cfg(feature = "performance-profile")]
                    let _handoff_profile =
                        profiling::scope(profiling::Event::SyscallHandoff).trace_args(nr as u64, 0);
                    sched::run_post_syscall_handoff(sched::now_ns_direct());
                    return;
                }
            }
        }

        (ops.set_sys_ret)(tf, ret);
        (ops.advance_pc)(tf);

        #[cfg(feature = "trace-task-lifecycle")]
        if trace_signal_boundary(nr) {
            log::info!(
                "[syscall][signal-boundary] frame-done pid={:?} nr={}",
                ctx.task().pid_root(),
                nr,
            );
        }

        let task = ctx.task();
        if task.signal.has_any_pending() || task.shared_signal_pending_bits_quick() != 0 {
            let _ = sched::operation::deliver_pending_signals_for_task(
                &task,
                sched::UserContextRef::new(tf.as_usize()),
            );
        }
        #[cfg(feature = "trace-task-lifecycle")]
        if trace_signal_boundary(nr) {
            log::info!(
                "[syscall][signal-boundary] signals-done pid={:?} nr={} state={:?}",
                task.pid_root(),
                nr,
                task.state(),
            );
        }

        match task.state() {
            sched::TaskState::Zombie | sched::TaskState::Dead => {
                // Safety: terminal task 随即最终调度，本 context 不会再被访问。
                unsafe { ctx.release_task_ref() };
                sched::schedule_once(0);
                panic!("[syscall] terminal task scheduled back unexpectedly");
            }
            sched::TaskState::Stopped | sched::TaskState::Continued => {
                sched::schedule_once(0);
            }
            _ => {}
        }
        #[cfg(feature = "performance-profile")]
        drop(finalize_profile);
        #[cfg(feature = "performance-profile")]
        let _handoff_profile =
            profiling::scope(profiling::Event::SyscallHandoff).trace_args(nr as u64, 0);
        sched::run_post_syscall_handoff_lazy();
        #[cfg(feature = "trace-task-lifecycle")]
        if trace_signal_boundary(nr) {
            log::info!(
                "[syscall][signal-boundary] handoff-done pid={:?} nr={}",
                ctx.task().pid_root(),
                nr,
            );
        }
    }
    // syscall 是 libcbench/lmbench 的最热路径，默认不能格式化并写入每次调用。
    // 需要单步追踪时临时打开下面的编译期开关即可。
    #[cfg(feature = "trace-syscall")]
    log::debug!("[syscall] nr={} args={:?} -> {}", nr, args, ret);
}

/// RISC-V 快速 syscall 调用后 trap frame 的所有权结果。
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FastDispatchOutcome {
    /// 分发器已经写回返回值并推进 PC，可继续检查返回工作。
    FrameAdvanced,
    /// syscall 或信号重启逻辑整体重写了用户上下文，必须完整恢复。
    FrameRewritten,
}

impl FastDispatchOutcome {
    #[inline(always)]
    pub const fn requires_full_restore(self) -> bool {
        matches!(self, Self::FrameRewritten)
    }
}

/// RISC-V arch 快速 syscall 调用入口。
///
/// 本函数只负责查表、调用和基本 frame 写回。调用方必须随后执行架构返回工作
/// 预检；signal/group-exit/rseq/resched/handoff 的消费统一位于 arch 冷路径。
#[inline]
pub fn dispatch_fast_with_frame<F>(
    tf: TrapFramePtr,
    nr: usize,
    args: [usize; 6],
    task: &Arc<sched::Task>,
    mut finish: F,
) -> FastDispatchOutcome
where
    F: FnMut(TrapFramePtr, isize),
{
    #[cfg(feature = "performance-profile")]
    let _span = profiling::enter_span();
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::SyscallDispatch).trace_args(nr as u64, 0);
    #[cfg(feature = "performance-profile")]
    let mut syscall_profile = profiling::syscall_scope(nr);

    let entry = if nr < SYSCALL_TABLE_LEN {
        // 注册在首个用户任务运行前完成，热路径只读，不需要每次建立 Acquire 栅栏。
        let ptr = SYSCALL_TABLE[nr].load(Ordering::Relaxed);
        if ptr == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, SyscallFn>(ptr) })
        }
    } else {
        None
    };

    let mut ctx = SyscallContext::new_borrowed(nr, args, tf, task);

    #[cfg(feature = "performance-profile")]
    let invoke_profile = profiling::scope(profiling::Event::SyscallInvoke).trace_args(nr as u64, 0);
    let ret: isize = match entry {
        Some(f) => match f(&mut ctx) {
            Ok(v) => v as isize,
            Err(e) => -(e.as_i32_direct() as isize),
        },
        None => -(Errno::ENOSYS.as_i32_direct() as isize),
    };
    #[cfg(feature = "trace-task-lifecycle")]
    if trace_signal_boundary(nr) {
        log::info!(
            "[syscall][signal-boundary] invoke-done pid={:?} nr={} ret={}",
            ctx.task().pid_root(),
            nr,
            ret,
        );
    }
    #[cfg(feature = "performance-profile")]
    syscall_profile.set_result(ret);
    #[cfg(feature = "performance-profile")]
    drop(invoke_profile);

    let frame_finalized = ctx.frame_finalized();
    if !frame_finalized {
        #[cfg(feature = "performance-profile")]
        let finalize_profile =
            profiling::scope(profiling::Event::SyscallFinalize).trace_args(nr as u64, 0);
        if ret == -(Errno::EINTR.as_i32_internal() as isize)
            && !ctx.restart_disabled()
            && try_restart_syscall_signal(&mut ctx, tf)
        {
            ctx.finish_borrowed();
            core::mem::forget(ctx);
            return FastDispatchOutcome::FrameRewritten;
        }

        finish(tf, ret);

        #[cfg(feature = "trace-task-lifecycle")]
        if trace_signal_boundary(nr) {
            log::info!(
                "[syscall][signal-boundary] frame-done pid={:?} nr={}",
                ctx.task().pid_root(),
                nr,
            );
        }
    }
    ctx.finish_borrowed();
    core::mem::forget(ctx);
    if frame_finalized {
        FastDispatchOutcome::FrameRewritten
    } else {
        FastDispatchOutcome::FrameAdvanced
    }
}

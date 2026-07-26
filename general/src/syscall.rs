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
pub struct SyscallContext<'a> {
    pub nr: usize,
    pub args: [usize; 6],
    pub tf: TrapFramePtr,
    task: Option<Arc<sched::Task>>,
    frame_finalized: bool,
    restart_disabled: bool,
    _phantom: core::marker::PhantomData<&'a ()>,
}

impl SyscallContext<'_> {
    pub fn task(&self) -> &Arc<sched::Task> {
        self.task.as_ref().expect("[syscall] task already released")
    }

    pub fn release_task_ref(&mut self) {
        self.task.take();
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

/// 标准 syscall 函数签名。
pub type SyscallFn = fn(&mut SyscallContext<'_>) -> Result<usize, Errno>;

/// 表大小：覆盖 Linux asm-generic 全部 syscall 号（最大约 450）。
pub const SYSCALL_TABLE_LEN: usize = 512;

static SYSCALL_TABLE: [AtomicUsize; SYSCALL_TABLE_LEN] =
    [const { AtomicUsize::new(0) }; SYSCALL_TABLE_LEN];

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
    if !sched::is_ready() {
        log::debug!("[syscall] dispatch before sched ready, nr={}", nr);
        (ops.set_sys_ret)(tf, -(Errno::ENOSYS.as_i32() as isize));
        (ops.advance_pc)(tf);
        return;
    }
    #[cfg(feature = "performance-profile")]
    let _span = profiling::enter_span();
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::SyscallDispatch).trace_args(nr as u64, 0);

    let task = sched::current_task();
    let mut ctx = SyscallContext {
        nr,
        args,
        tf,
        task: Some(task),
        frame_finalized: false,
        restart_disabled: false,
        _phantom: core::marker::PhantomData,
    };

    // syscall 表只在启动期注册；热路径无锁读取函数指针，避免 lmbench
    // simple syscall 每次都争用全局自旋锁。
    let entry = if nr < SYSCALL_TABLE_LEN {
        let ptr = SYSCALL_TABLE[nr].load(Ordering::Acquire);
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
            Err(e) => -(e.as_i32() as isize),
        },
        None => -(Errno::ENOSYS.as_i32() as isize),
    };
    #[cfg(feature = "performance-profile")]
    drop(invoke_profile);

    let frame_finalized = ctx.frame_finalized();
    if !frame_finalized {
        #[cfg(feature = "performance-profile")]
        let finalize_profile =
            profiling::scope(profiling::Event::SyscallFinalize).trace_args(nr as u64, 0);
        if ret == -(Errno::EINTR.as_i32() as isize) && !ctx.restart_disabled() {
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
                    sched::run_post_syscall_handoff(sched::now_ns_public());
                    return;
                }
            }
        }

        (ops.set_sys_ret)(tf, ret);
        (ops.advance_pc)(tf);

        let task = ctx.task();
        if task.signal.has_any_pending() || task.shared_signal_pending_bits_quick() != 0 {
            let _ = sched::operation::deliver_pending_signals_for_task(
                &task,
                sched::UserContextRef::new(tf.as_usize()),
            );
        }

        match task.state() {
            sched::TaskState::Zombie | sched::TaskState::Dead => {
                ctx.release_task_ref();
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
    }
    // syscall 是 libcbench/lmbench 的最热路径，默认不能格式化并写入每次调用。
    // 需要单步追踪时临时打开下面的编译期开关即可。
    #[cfg(feature = "trace-syscall")]
    log::debug!("[syscall] nr={} args={:?} -> {}", nr, args, ret);
}

/// arch 快速 syscall 路径用。调用方已经从 trap frame 取出 syscall 号和参数，
/// 因而这里跳过 frame_ops 的 sys_nr/sys_args 间接调用，但保持普通 dispatch
/// 的任务引用与信号语义。
#[inline]
pub fn dispatch_fast(tf: TrapFramePtr, nr: usize, args: [usize; 6]) {
    let Some(ops) = frame_ops() else { return };
    dispatch_fast_with_frame(tf, nr, args, |tf, ret| {
        (ops.set_sys_ret)(tf, ret);
        (ops.advance_pc)(tf);
    });
}

/// arch 快速 syscall 路径用。调用方直接提供 trap frame 写回逻辑，避免热路径
/// 每次通过 `frame_ops()` 全局表做原子加载和间接调用。
#[inline]
pub fn dispatch_fast_with_frame<F>(tf: TrapFramePtr, nr: usize, args: [usize; 6], mut finish: F)
where
    F: FnMut(TrapFramePtr, isize),
{
    let task = sched::current_task_fast();
    #[cfg(feature = "performance-profile")]
    let _span = profiling::enter_span();
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::SyscallDispatch).trace_args(nr as u64, 0);

    let entry = if nr < SYSCALL_TABLE_LEN {
        let ptr = SYSCALL_TABLE[nr].load(Ordering::Acquire);
        if ptr == 0 {
            None
        } else {
            Some(unsafe { core::mem::transmute::<usize, SyscallFn>(ptr) })
        }
    } else {
        None
    };

    let mut ctx = SyscallContext {
        nr,
        args,
        tf,
        task: Some(task),
        frame_finalized: false,
        restart_disabled: false,
        _phantom: core::marker::PhantomData,
    };

    #[cfg(feature = "performance-profile")]
    let invoke_profile = profiling::scope(profiling::Event::SyscallInvoke).trace_args(nr as u64, 0);
    let ret: isize = match entry {
        Some(f) => match f(&mut ctx) {
            Ok(v) => v as isize,
            Err(e) => -(e.as_i32() as isize),
        },
        None => -(Errno::ENOSYS.as_i32() as isize),
    };
    #[cfg(feature = "performance-profile")]
    drop(invoke_profile);

    let frame_finalized = ctx.frame_finalized();
    if !frame_finalized {
        #[cfg(feature = "performance-profile")]
        let finalize_profile =
            profiling::scope(profiling::Event::SyscallFinalize).trace_args(nr as u64, 0);
        if ret == -(Errno::EINTR.as_i32() as isize) && !ctx.restart_disabled() {
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
                    sched::run_post_syscall_handoff(sched::now_ns_public());
                    return;
                }
            }
        }

        finish(tf, ret);

        let task = ctx.task();
        if task.signal.has_any_pending() || task.shared_signal_pending_bits_quick() != 0 {
            let _ = sched::operation::deliver_pending_signals_for_task(
                task,
                sched::UserContextRef::new(tf.as_usize()),
            );
        }

        match task.state() {
            sched::TaskState::Zombie | sched::TaskState::Dead => {
                ctx.release_task_ref();
                sched::schedule_once(0);
                panic!("[syscall] terminal task scheduled back");
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
    }
}

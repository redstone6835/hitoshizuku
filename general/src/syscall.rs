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
use core::sync::atomic::{AtomicPtr, Ordering};

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
    pub task: Arc<sched::Task>,
    frame_finalized: bool,
    _phantom: core::marker::PhantomData<&'a ()>,
}

impl SyscallContext<'_> {
    /// 标记当前 syscall 已经完整重写 trap frame。dispatch 不再写 syscall
    /// 返回值、不推进 PC，也不在本次返回前投递 signal frame。
    pub fn finalize_frame(&mut self) {
        self.frame_finalized = true;
    }

    pub fn frame_finalized(&self) -> bool {
        self.frame_finalized
    }
}

/// 标准 syscall 函数签名。
pub type SyscallFn = fn(&mut SyscallContext<'_>) -> Result<usize, Errno>;

/// 表大小：覆盖 Linux asm-generic 全部 syscall 号（最大约 450）。
pub const SYSCALL_TABLE_LEN: usize = 512;

static SYSCALL_TABLE: spin::Mutex<[Option<SyscallFn>; SYSCALL_TABLE_LEN]> =
    spin::Mutex::new([None; SYSCALL_TABLE_LEN]);

/// 在启动期注册一个 syscall 号 → fn 的映射。重复注册会 panic（防止表条目被
/// 静默覆盖）。
pub fn register_syscall(nr: usize, f: SyscallFn) {
    assert!(
        nr < SYSCALL_TABLE_LEN,
        "[syscall] nr {} out of range (max {})",
        nr,
        SYSCALL_TABLE_LEN - 1
    );
    let mut table = SYSCALL_TABLE.lock();
    assert!(
        table[nr].is_none(),
        "[syscall] nr {} already registered",
        nr
    );
    table[nr] = Some(f);
}

/// 启动期已注册的 syscall 数量；smoketest / debug 用。
pub fn registered_count() -> usize {
    SYSCALL_TABLE.lock().iter().filter(|e| e.is_some()).count()
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

    let task = sched::current_task();
    let mut ctx = SyscallContext {
        nr,
        args,
        tf,
        task,
        frame_finalized: false,
        _phantom: core::marker::PhantomData,
    };

    //   提前从锁中取出条目，释放锁后再执行 syscall，
    //   避免整个 syscall 执行期间持有全局表锁。
    let entry = if nr < SYSCALL_TABLE_LEN {
        SYSCALL_TABLE.lock()[nr]
    } else {
        None
    };

    let ret: isize = match entry {
        Some(f) => match f(&mut ctx) {
            Ok(v) => v as isize,
            Err(e) => -(e.as_i32() as isize),
        },
        None => -(Errno::ENOSYS.as_i32() as isize),
    };

    let frame_finalized = ctx.frame_finalized();
    if !frame_finalized {
        (ops.set_sys_ret)(tf, ret);
        (ops.advance_pc)(tf);
        let _ = sched::operation::deliver_pending_signals_with_context(sched::UserContextRef::new(
            tf.as_usize(),
        ));
        if matches!(
            ctx.task.state(),
            sched::TaskState::Stopped
                | sched::TaskState::Continued
                | sched::TaskState::Zombie
                | sched::TaskState::Dead
        ) {
            sched::schedule_once(0);
        }
    }
    log::debug!("[syscall] nr={} args={:?} -> {}", nr, args, ret);
}

//! RISC-V64 异常/中断分发处理。
//!
//! 本模块实现异常入口的 Rust 端逻辑，由汇编入口
//! `__riscv_exception_entry` 保存完整 TrapFrame 后调用。
//!
//! 参数约定（与汇编端一致）：
//! - `tf_ptr`  : TrapFrame 指针（内核栈上，已由汇编填好）
//! - `_user_sp`: 用户栈指针（当前未使用）
//!
//! 返回值：
//! - 非零 → 汇编端恢复该 TrapFrame 指针并执行 `sret`；
//! - 零   → 汇编端进入死循环宕机。

use crate::trap::Riscv64MessageInterruptOps;
use crate::*;
use general::{Exception, Interrupt};

/// 将 `TrapFrame` 指针转换为可变引用。
///
/// # Safety
/// `ptr` 必须是汇编入口写入的有效、对齐且完整的 TrapFrame 指针。
#[inline]
unsafe fn trap_frame_mut<'a>(ptr: usize) -> &'a mut TrapFrame {
    // 安全性：调用方保证 ptr 来自 arch 入口写入的 TrapFrame 指针，生命周期
    //         在 trap 返回前一直有效。
    unsafe { &mut *(ptr as *mut TrapFrame) }
}

/// 在从用户态 trap 返回前投递异步信号。
///
/// syscall 返回路径已经在 `general::syscall::dispatch` 中带 trap-frame context
/// 投递一次；这里补齐 timer/外设中断和可恢复异常路径。
fn deliver_user_signals_before_return(tf_ptr: usize, from_user: bool) {
    if !from_user || !sched::is_ready() {
        return;
    }
    let task = sched::current_task();
    if !task.signal.has_any_pending() && task.shared_signal_pending_bits_quick() == 0 {
        return;
    }
    let _ = sched::operation::deliver_pending_signals_for_task(
        &task,
        sched::UserContextRef::new(tf_ptr),
    );
    match task.state() {
        sched::TaskState::Zombie | sched::TaskState::Dead => {
            sched::schedule_once(kernel_timestamp_ns());
            panic!("[trap][signal] terminal task scheduled back unexpectedly");
        }
        sched::TaskState::Stopped | sched::TaskState::Continued => {
            sched::schedule_once(kernel_timestamp_ns());
        }
        _ => {}
    }
}

fn signal_for_user_exception(code: usize) -> sched::SignalNumber {
    match code {
        EXC_ILLEGAL_INST => sched::SignalNumber::SIGILL,
        EXC_BREAKPOINT => sched::SignalNumber::SIGTRAP,
        EXC_INST_MISALIGNED | EXC_LOAD_MISALIGNED | EXC_STORE_MISALIGNED | EXC_INST_ACCESS
        | EXC_LOAD_ACCESS | EXC_STORE_ACCESS => sched::SignalNumber::SIGBUS,
        _ => sched::SignalNumber::SIGSEGV,
    }
}


fn terminate_user_exception(
    tf: &TrapFrame,
    code: usize,
    sig: sched::SignalNumber,
    tf_ptr: usize,
    from_user: bool,
) -> usize {
    let (pid, comm) = if sched::is_ready() {
        let task = sched::current_task();
        (task.pid_root(), task.comm())
    } else {
        (None, [0; sched::TASK_COMM_LEN])
    };

    log::warning!(
        "[trap][exception] user exception pid={:?} comm={:?} code={:#x} sepc={:#x} stval={:#x} sig={}",
        pid,
        comm,
        code,
        tf.sepc,
        tf.tval,
        sig.raw()
    );

    if sched::is_ready() {
        let me = sched::current_task();
        let pid = me.pid_root().unwrap_or(0);
        let _ = sched::operation::tkill(pid, Some(sig));
        drop(me);
        deliver_user_signals_before_return(tf_ptr, from_user);
    }

    tf_ptr
}

/// 解码中断：从 SCAUSE 寄存器提取中断类型。
fn decode_interrupt(cause: usize) -> Interrupt {
    // RISC-V 把待处理中断类型压在 SCAUSE 的高比特位。与 LoongArch 不同，RISC-V
    // 的中断类型通过单独的 exception code 区分。这里做的就是把硬件编码翻译成上层
    // 更容易消费的枚举。
    let code = cause & !SCAUSE_INTERRUPT;
    match code {
        IRQ_S_SOFT => Interrupt::Ipi,
        IRQ_S_TIMER => Interrupt::Timer,
        IRQ_S_EXT => Interrupt::Hardware(0),
        _ => Interrupt::Other(code),
    }
}

/// 解码异常代码为 Exception 类型。
fn decode_exception(code: usize) -> Exception {
    match code {
        EXC_INST_PAGE_FAULT => Exception::InstructionPageFault,
        EXC_LOAD_PAGE_FAULT => Exception::LoadPageFault,
        EXC_STORE_PAGE_FAULT => Exception::StorePageFault,
        EXC_ILLEGAL_INST => Exception::IllegalInstruction,
        EXC_BREAKPOINT => Exception::Breakpoint,
        EXC_INST_MISALIGNED => Exception::AddressError,
        EXC_LOAD_MISALIGNED => Exception::AddressError,
        EXC_STORE_MISALIGNED => Exception::AddressError,
        EXC_LOAD_ACCESS => Exception::AddressError,
        EXC_STORE_ACCESS => Exception::AddressError,
        EXC_INST_ACCESS => Exception::AddressError,
        _ => Exception::Other(code),
    }
}

/// RISC-V64 统一异常入口（Rust 端）。
///
/// # Safety
/// 本函数由汇编入口调用，必须遵循参数约定传递有效的 TrapFrame 指针。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn riscv64_handle_exception(tf_ptr: usize, _user_sp: usize) -> usize {
    // 汇编入口已经完成最危险的硬件现场保存；Rust 端从这里开始处理"解释现场并决定命运"。
    // 返回非零表示异常可恢复，汇编端将按该 TrapFrame 恢复寄存器并执行 `sret`；
    // 返回零表示当前策略认定无法恢复，汇编端进入停机路径。
    let tf = unsafe { trap_frame_mut(tf_ptr) };
    let cause = tf.cause;
    let is_interrupt = (cause & SCAUSE_INTERRUPT) != 0;
    let code = cause & !SCAUSE_INTERRUPT;

    let from_user = (tf.status & SSTATUS_SPP) == 0;

    if is_interrupt {
        // 对中断而言，最关键的信息是 SCAUSE 位域。与同步异常不同，中断通常不需要 tval，
        // 且多数情况下 PC 只用于诊断，不决定恢复逻辑。
        let intr = decode_interrupt(cause);
        // 清除定时器中断标志
        if code == IRQ_S_TIMER {
            // RISC-V 的定时器中断需要软件显式清除，否则在 `sret` 后会立即再次陷入，
            // 形成"看似无法返回"的中断风暴。
            super::super::time::rearm_periodic_timer();
            let now_ns = kernel_timestamp_ns();
            let _ = general::elm_guard::request_timeout_if_expired(now_ns);
            sched::on_timer_tick(now_ns);
            super::super::vdso::run_timer_tick_hook(now_ns);
            super::super::vdso::run_net_poll_hook(now_ns);
            super::super::vdso::run_tty_poll_hook(now_ns);
            deliver_user_signals_before_return(tf_ptr, from_user);
            if from_user {
                sched::preempt_if_needed(now_ns);
            }
            return tf_ptr;
        }
        // 非 timer 中断：SupervisorExternal (PLIC), IPI 等
        log::debug!(
            "[trap][interrupt] {:?} sepc={:#x} cpu={}",
            intr,
            tf.sepc,
            Riscv64MessageInterruptOps::current_cpu_id()
        );
        let now_ns = kernel_timestamp_ns();
        let _ = general::dev::irq::dispatch_interrupt(intr);
        super::super::vdso::run_tty_poll_hook(now_ns);
        deliver_user_signals_before_return(tf_ptr, from_user);
        if from_user {
            sched::preempt_if_needed(now_ns);
        }
        tf_ptr
    } else if code == EXC_ECALL_U || code == EXC_ECALL_S {
        general::syscall::dispatch(general::TrapFramePtr::new(tf_ptr));
        if sched::needs_resched_current() {
            sched::preempt_if_needed(kernel_timestamp_ns());
        }
        tf_ptr
    } else if code == EXC_INST_PAGE_FAULT
        || code == EXC_LOAD_PAGE_FAULT
        || code == EXC_STORE_PAGE_FAULT
        || code == EXC_LOAD_ACCESS
        || code == EXC_STORE_ACCESS
    {
        // 缺页族 → 统一走 general::mm::dispatch_page_fault。
        // 分派结果：
        //   Fixed                      → 重试指令，返回 tf_ptr；
        //   Segv                       → 本轮先 log 并投 SIGSEGV；
        //   Kernel(NotInitialized)     → 启动早期缺页，按旧路径 halt；
        //   Kernel(UncaughtKernelAccess) → 真内核 bug，halt。
        use general::mm::FaultOutcome;
        let tf_ptr_gen = general::TrapFramePtr::new(tf_ptr);
        match general::mm::dispatch_page_fault(tf_ptr_gen) {
            FaultOutcome::Fixed => {
                deliver_user_signals_before_return(tf_ptr, from_user);
                tf_ptr
            }
            FaultOutcome::Segv => {
                let (pid, comm) = if sched::is_ready() {
                    let task = sched::current_task();
                    (task.pid_root(), task.comm())
                } else {
                    (None, [0; 16])
                };
                log::warning!(
                    "[trap][mem] user SIGSEGV pid={:?} comm={:?} sepc={:#x} tval={:#x} code={:#x}",
                    pid,
                    comm,
                    tf.sepc,
                    tf.tval,
                    code
                );

                if sched::is_ready() {
                    let me = sched::current_task();
                    let pid = me.pid_root().unwrap_or(0);
                    let _ = sched::operation::tkill(pid, Some(sched::SignalNumber::SIGSEGV));
                    drop(me);
                    deliver_user_signals_before_return(tf_ptr, from_user);
                }
                tf_ptr
            }
            FaultOutcome::Kernel(reason) => {
                let (pid, kind, state, comm) = if sched::is_ready() {
                    let task = sched::current_task();
                    (task.pid_root(), task.kind(), task.state(), task.comm())
                } else {
                    (
                        None,
                        sched::TaskKind::KernelThread,
                        sched::TaskState::Running,
                        [0u8; sched::TASK_COMM_LEN],
                    )
                };
                log::error!(
                    "[trap][mm] FATAL kernel fault ({:?}) sepc={:#x} ra={:#x} sp={:#x} tval={:#x} status={:#x} code={:#x} pid={:?} kind={:?} state={:?} comm={:?} — halting",
                    reason,
                    tf.sepc,
                    tf.ra,
                    tf.sp,
                    tf.tval,
                    tf.status,
                    code,
                    pid,
                    kind,
                    state,
                    comm
                );
                0
            }
        }
    } else if code == EXC_ILLEGAL_INST
        && from_user
        && {
            let vec = crate::riscv64::vector::enable_user_vector_if_needed(tf);
            let fpu = enable_user_fpu_if_needed(tf);
            vec || fpu
        }
    {
        // 用户首次触碰浮点状态时按需打开 FS，保持 sepc 不变让原指令重试。
        tf_ptr
    } else if code == EXC_BREAKPOINT {
        // ebreak 指令可能是 4 字节标准格式或 2 字节压缩格式（c.ebreak）。
        // 通过读取指令低 2 位判断宽度：低 2 位为 0x3 表示 4 字节指令。
        let insn_lo = unsafe { core::ptr::read(tf.sepc as *const u16) };
        let step = if insn_lo & 0x3 == 0x3 { 4usize } else { 2usize };
        tf.sepc = tf.sepc.wrapping_add(step);
        tf_ptr
    } else {
        if from_user {
            return terminate_user_exception(
                tf,
                code,
                signal_for_user_exception(code),
                tf_ptr,
                from_user,
            );
        }

        let exc = decode_exception(code);
        log::error!(
            "[trap][exception] UNHANDLED kernel exception {:?} code={:#x} sepc={:#x} stval={:#x}",
            exc,
            code,
            tf.sepc,
            tf.tval
        );
        0
    }
}

// ── syscall 快速路径 ─────────────────────────────────────────────────────────

/// syscall 快速路径 handler。与 `riscv64_handle_exception` 相同签名，
/// 但只处理 ecall（入口未保存 FPU）。
///
/// 由汇编快速路径在确认 scause=8 后直接调用。
/// 只有 FS=Off 时才允许完全跳过 FPU；FS!=Off 说明用户态可能依赖浮点现场，
/// 需要保存后走完整恢复路径，避免 signal/resched/full restore 读到旧状态。
#[unsafe(no_mangle)]
pub extern "C" fn riscv64_fast_syscall_dispatch(tf_ptr: usize, _user_sp: usize) -> usize {
    let (fpu_dirty, vector_active, nr, args) = {
        let tf = unsafe { trap_frame_mut(tf_ptr) };

        let fpu_dirty = (tf.status & SSTATUS_FS_MASK) == SSTATUS_FS_DIRTY;
        if fpu_dirty {
            unsafe { save_fpu_to_frame(tf) };
        }
        let vector_active = (tf.status & SSTATUS_VS_MASK) != 0;
        if vector_active {
            crate::riscv64::vector::save_current_if_active(tf);
        }

        (
            fpu_dirty,
            vector_active,
            tf.a7,
            [tf.a0, tf.a1, tf.a2, tf.a3, tf.a4, tf.a5],
        )
    };
    general::syscall::dispatch_fast_with_frame(
        general::TrapFramePtr::new(tf_ptr),
        nr,
        args,
        |tf, ret| {
            let frame = unsafe { trap_frame_mut(tf.as_usize()) };
            frame.a0 = ret as usize;
            frame.sepc = frame.sepc.wrapping_add(4);
        },
    );

    if sched::needs_resched_current() {
        sched::preempt_if_needed(kernel_timestamp_ns());
        return tf_ptr | 1;
    }
    if fpu_dirty || vector_active {
        return tf_ptr | 1;
    }
    tf_ptr
}

fn enable_user_fpu_if_needed(tf: &mut TrapFrame) -> bool {
    if !looks_like_fpu_instruction(tf.sepc) {
        return false;
    }

    let new_fs = SSTATUS_FS_DIRTY;
    unsafe {
        core::arch::asm!(
            "csrr t0, sstatus",
            "or t0, t0, {fs}",
            "csrw sstatus, t0",
            fs = in(reg) new_fs,
            out("t0") _,
        );
    }
    tf.status = (tf.status & !SSTATUS_FS_MASK) | new_fs;
    true
}

fn looks_like_fpu_instruction(pc: usize) -> bool {
    let mut lo = [0u8; 2];
    if general::mm::copy_from_user(pc, &mut lo).is_err() {
        return false;
    }
    let half = u16::from_le_bytes(lo);
    if half & 0x3 != 0x3 {
        return matches!(
            half & 0xe003,
            0x2000 | 0x6000 | 0xa000 | 0xe000 | 0x2002 | 0x6002 | 0xa002 | 0xe002
        );
    }

    let mut raw = [0u8; 4];
    raw[..2].copy_from_slice(&lo);
    if general::mm::copy_from_user(pc.wrapping_add(2), &mut raw[2..]).is_err() {
        return false;
    }
    let insn = u32::from_le_bytes(raw);
    let opcode = insn & 0x7f;
    matches!(
        opcode,
        0x07 | 0x27 | 0x43 | 0x47 | 0x4b | 0x4f | 0x53 | 0x73
    ) && (opcode != 0x73 || matches!((insn >> 20) & 0xfff, 0x001..=0x003))
}

/// 将当前 CPU 的 FPU 寄存器保存到 trap frame。
///
/// # Safety
///
/// 必须在 FPU 可访问时调用（sstatus.FS != Off）。
#[inline]
unsafe fn save_fpu_to_frame(tf: &mut TrapFrame) {
    let base = tf.f.as_mut_ptr() as usize;
    let fcsr: usize;
    unsafe {
        core::arch::asm!(
            ".option push",
            ".option arch, +d",
            "frcsr {fcsr}",
            "fsd f0,  0*8({base})",  "fsd f1,  1*8({base})",
            "fsd f2,  2*8({base})",  "fsd f3,  3*8({base})",
            "fsd f4,  4*8({base})",  "fsd f5,  5*8({base})",
            "fsd f6,  6*8({base})",  "fsd f7,  7*8({base})",
            "fsd f8,  8*8({base})",  "fsd f9,  9*8({base})",
            "fsd f10, 10*8({base})", "fsd f11, 11*8({base})",
            "fsd f12, 12*8({base})", "fsd f13, 13*8({base})",
            "fsd f14, 14*8({base})", "fsd f15, 15*8({base})",
            "fsd f16, 16*8({base})", "fsd f17, 17*8({base})",
            "fsd f18, 18*8({base})", "fsd f19, 19*8({base})",
            "fsd f20, 20*8({base})", "fsd f21, 21*8({base})",
            "fsd f22, 22*8({base})", "fsd f23, 23*8({base})",
            "fsd f24, 24*8({base})", "fsd f25, 25*8({base})",
            "fsd f26, 26*8({base})", "fsd f27, 27*8({base})",
            "fsd f28, 28*8({base})", "fsd f29, 29*8({base})",
            "fsd f30, 30*8({base})", "fsd f31, 31*8({base})",
            ".option pop",
            base = in(reg) base,
            fcsr = out(reg) fcsr,
            options(nostack)
        );
    }
    tf.fcsr = fcsr as u32;
}

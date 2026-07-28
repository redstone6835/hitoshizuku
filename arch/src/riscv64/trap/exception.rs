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

use crate::riscv64::{time, vdso};
use crate::trap::Riscv64MessageInterruptOps;
use crate::*;
use general::{Exception, Interrupt};

/// 将 `TrapFrame` 指针转换为短生命周期共享引用。
///
/// # Safety
/// `ptr` 必须是汇编入口写入的有效、对齐且完整的 TrapFrame 指针。
#[inline]
unsafe fn trap_frame_ref<'a>(ptr: usize) -> &'a TrapFrame {
    unsafe { &*(ptr as *const TrapFrame) }
}

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

fn prepare_user_state_before_return(tf_ptr: usize, from_user: bool) {
    if !from_user || !sched::is_ready() {
        return;
    }
    let task = sched::current_task();
    let _ =
        sched::operation::prepare_user_return_for_task(&task, sched::UserContextRef::new(tf_ptr));
    match task.state() {
        sched::TaskState::Zombie | sched::TaskState::Dead => {
            drop(task);
            sched::schedule_once(kernel_timestamp_ns());
            panic!("[trap][signal] terminal task scheduled back unexpectedly");
        }
        sched::TaskState::Stopped | sched::TaskState::Continued => {
            drop(task);
            sched::schedule_once(kernel_timestamp_ns());
        }
        _ => {}
    }
}

/// 在从用户态 trap 返回前投递异步信号。
///
/// syscall 返回路径已经在 `general::syscall::dispatch` 中带 trap-frame context
/// 投递一次；这里补齐 timer/外设中断和可恢复异常路径。
fn deliver_user_signals_before_return(tf_ptr: usize, from_user: bool) {
    prepare_user_state_before_return(tf_ptr, from_user);
    if !from_user || !sched::is_ready() {
        return;
    }
    let task = sched::current_task();
    if task.signal.has_any_pending() || task.shared_signal_pending_bits_quick() != 0 {
        let _ = sched::operation::deliver_pending_signals_for_task(
            &task,
            sched::UserContextRef::new(tf_ptr),
        );
    }
    match task.state() {
        sched::TaskState::Zombie | sched::TaskState::Dead => {
            drop(task);
            sched::schedule_once(kernel_timestamp_ns());
            panic!("[trap][signal] terminal task scheduled back unexpectedly");
        }
        sched::TaskState::Stopped | sched::TaskState::Continued => {
            drop(task);
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

// Linux RISC-V64 syscall ABI 中会整体替换当前用户上下文的调用。
const SYS_RT_SIGRETURN: usize = 139;
const SYS_EXECVE: usize = 221;
const SYS_EXECVEAT: usize = 281;

#[inline]
fn rewrites_user_frame(nr: usize) -> bool {
    matches!(nr, SYS_RT_SIGRETURN | SYS_EXECVE | SYS_EXECVEAT)
}

/// 对即将返回 U-mode 的架构上下文做最后一道防御性净化。
///
/// `trusted_satp` 仅用于 rt_sigreturn：该 syscall 的用户帧不能改变地址空间根。
fn sanitize_user_return_frame(tf_ptr: usize, trusted_satp: Option<usize>) {
    let trusted_kstack_top = crate::riscv64::specific::current_kernel_stack_top();
    let tf = unsafe { trap_frame_mut(tf_ptr) };
    tf.status = (tf.status & SSTATUS_USER_RESTORE_MASK) | SSTATUS_USER_RETURN_BASE;
    // FS=Off 时 FPU 区不会被恢复；保持它的任务初始化副本不动，避免普通 syscall
    // 为无浮点任务写入扩展上下文所在的额外 cache line。
    if tf.status & SSTATUS_FS_MASK != 0 {
        tf.fcsr &= 0xff;
    }
    tf.kstack_top = if trusted_kstack_top != 0 {
        trusted_kstack_top
    } else {
        // 用户 trap 入口在栈顶预留一帧给最终返回窗口中的嵌套 S-mode fault。
        tf_ptr.saturating_add(FRAME_SIZE * 2)
    };
    if let Some(satp) = trusted_satp {
        tf.satp = satp;
        if tf.status & SSTATUS_FS_MASK == 0 {
            tf.f.fill(0);
            tf.fcsr = 0;
            tf._pad = 0;
        }
    }
}

#[inline]
fn finish_trap_return(tf_ptr: usize, from_user: bool) -> usize {
    if from_user {
        prepare_user_state_before_return(tf_ptr, true);
        sanitize_user_return_frame(tf_ptr, None);
    }
    tf_ptr
}

fn recover_elm_trap(
    tf_ptr: usize,
    recovery: general::elm_guard::ElmTrapRecovery,
    event: &str,
) -> usize {
    let (sepc, tval) = {
        let tf = unsafe { trap_frame_ref(tf_ptr) };
        (tf.sepc, tf.tval)
    };
    log::warning!(
        "[trap][elm] {} cell={} phase={} reason={} sepc={:#x} tval={:#x} return_pc={:#x} return_sp={:#x}",
        event,
        recovery.cell,
        recovery.phase,
        recovery.reason,
        sepc,
        tval,
        recovery.return_pc,
        recovery.return_sp
    );
    let tf = unsafe { trap_frame_mut(tf_ptr) };
    tf.sepc = recovery.return_pc;
    tf.sp = recovery.return_sp;
    tf.a0 = recovery.return_value;
    tf_ptr
}

fn try_recover_elm_kernel_fault(tf_ptr: usize, code: usize, event: &str) -> Option<usize> {
    let (sepc, tval) = {
        let tf = unsafe { trap_frame_ref(tf_ptr) };
        (tf.sepc, tf.tval)
    };
    general::elm_guard::try_recover_kernel_fault(sepc, tval, code)
        .map(|recovery| recover_elm_trap(tf_ptr, recovery, event))
}

/// kernel-origin trap 的临时 frame 在 FS=Off 时调用：清除未初始化的 FPU 区域。
/// user-origin frame 在任务创建或可信 sigreturn 时已经初始化，不在热路径重复清零。
#[unsafe(no_mangle)]
pub extern "C" fn riscv64_zero_inactive_fpu_frame(tf_ptr: usize) {
    let tf = unsafe { trap_frame_mut(tf_ptr) };
    tf.f.fill(0);
    tf.fcsr = 0;
    tf._pad = 0;
}

fn terminate_user_exception(
    code: usize,
    sig: sched::SignalNumber,
    tf_ptr: usize,
    from_user: bool,
) -> usize {
    let (sepc, tval) = {
        let tf = unsafe { trap_frame_ref(tf_ptr) };
        (tf.sepc, tf.tval)
    };
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
        sepc,
        tval,
        sig.raw()
    );

    if sched::is_ready() {
        let me = sched::current_task();
        let pid = me.pid_root().unwrap_or(0);
        let _ = sched::operation::tkill(pid, Some(sig));
        drop(me);
        deliver_user_signals_before_return(tf_ptr, from_user);
    }

    finish_trap_return(tf_ptr, from_user)
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

fn handle_interrupt(tf_ptr: usize, cause: usize, code: usize, from_user: bool) -> usize {
    let interrupt = decode_interrupt(cause);

    if code == IRQ_S_TIMER {
        #[cfg(feature = "performance-profile")]
        {
            let pc = unsafe { trap_frame_ref(tf_ptr) }.sepc;
            profiling::sample_pc(pc, from_user);
        }
        // Timer 必须先重装 compare，否则 sret 后会立刻再次陷入。
        let now_ticks = time::rearm_periodic_timer();
        general::dev::irq::record_timer_interrupt();
        let now_ns = time::stable_counter_to_ns(now_ticks);
        let _ = general::elm_guard::request_timeout_if_expired(now_ns);
        if !from_user {
            let sepc = unsafe { trap_frame_ref(tf_ptr) }.sepc;
            if let Some(recovery) = general::elm_guard::try_recover_requested_abort(sepc) {
                return recover_elm_trap(tf_ptr, recovery, "forced native exit");
            }
        }
        vdso::run_timer_tick_hook(now_ns);
        // timer 可打断持有 runqueue、topology 等普通自旋锁的内核路径。
        // top-half 只记录待处理时间；syscall 返回或下一次主动调度会在无锁
        // 边界补做调度工作，避免同一 CPU 重入锁后永久自旋。
        if !from_user {
            sched::defer_timer_tick(now_ns);
            if general::elm_guard::active_cell() == 0 {
                vdso::run_net_poll_hook(now_ns);
                vdso::run_tty_poll_hook(now_ns);
            }
            return finish_trap_return(tf_ptr, false);
        }
        sched::on_timer_tick(now_ns);
        vdso::run_net_poll_hook(now_ns);
        vdso::run_tty_poll_hook(now_ns);
        deliver_user_signals_before_return(tf_ptr, from_user);
        if from_user {
            sched::preempt_if_needed(now_ns);
        }
        return finish_trap_return(tf_ptr, from_user);
    }

    if code == IRQ_S_SOFT {
        unsafe { Riscv64MessageInterruptOps::ack_ipi() };
        crate::riscv64::smp::handle_ipi();
    }

    let sepc = unsafe { trap_frame_ref(tf_ptr) }.sepc;
    log::debug!(
        "[trap][interrupt] {:?} sepc={:#x} cpu={}",
        interrupt,
        sepc,
        Riscv64MessageInterruptOps::current_cpu_id()
    );
    let now_ns = kernel_timestamp_ns();
    let _ = general::dev::irq::dispatch_interrupt(interrupt);
    if !from_user {
        let sepc = unsafe { trap_frame_ref(tf_ptr) }.sepc;
        if let Some(recovery) = general::elm_guard::try_recover_requested_abort(sepc) {
            return recover_elm_trap(tf_ptr, recovery, "interrupt abort");
        }
    }
    vdso::run_tty_poll_hook(now_ns);
    deliver_user_signals_before_return(tf_ptr, from_user);
    if from_user {
        sched::preempt_if_needed(now_ns);
    }
    finish_trap_return(tf_ptr, from_user)
}

fn handle_user_syscall(tf_ptr: usize) -> usize {
    let (nr, original_satp) = {
        let tf = unsafe { trap_frame_ref(tf_ptr) };
        (tf.a7, tf.satp)
    };
    general::syscall::dispatch(general::TrapFramePtr::new(tf_ptr));
    if sched::needs_resched_current() {
        sched::preempt_if_needed(kernel_timestamp_ns());
    }
    prepare_user_state_before_return(tf_ptr, true);
    sanitize_user_return_frame(tf_ptr, (nr == SYS_RT_SIGRETURN).then_some(original_satp));
    tf_ptr
}

fn handle_page_fault(tf_ptr: usize, code: usize, from_user: bool) -> usize {
    use general::mm::FaultOutcome;

    match general::mm::dispatch_page_fault(general::TrapFramePtr::new(tf_ptr)) {
        FaultOutcome::Fixed => {
            deliver_user_signals_before_return(tf_ptr, from_user);
            finish_trap_return(tf_ptr, from_user)
        }
        FaultOutcome::Segv => {
            if sched::is_ready() {
                let task = sched::current_task();
                let pid = task.pid_root().unwrap_or(0);
                let _ = sched::operation::tkill(pid, Some(sched::SignalNumber::SIGSEGV));
                drop(task);
                deliver_user_signals_before_return(tf_ptr, from_user);
            }
            finish_trap_return(tf_ptr, from_user)
        }
        FaultOutcome::OutOfMemory => {
            if sched::is_ready() {
                let task = sched::current_task();
                let pid = task.pid_root().unwrap_or(0);
                let comm = task.comm();
                let buddy = allocator::KERNEL_ALLOCATOR.buddy_stats();
                let alloc = allocator::KERNEL_ALLOCATOR.layer_stats();
                let vm = general::mm::vm_space_diag();
                log::warning!(
                    "[trap][mm][oom] pid={} comm={:?} free_pages={} allocated_pages={} reserved_pages={} alloc_failures={} physical_records={} small_records={} large_records={} kheap_cached_pages={} vm_live={} vm_created={} vm_dropped={} private_file_pressure_reclaims={}",
                    pid,
                    comm,
                    buddy.free_pages,
                    buddy.allocated_pages,
                    buddy.reserved_pages,
                    buddy.alloc_failures,
                    alloc.registry.live_physical,
                    alloc.registry.live_small,
                    alloc.registry.live_large,
                    alloc.kheap.cached_pages,
                    vm.live,
                    vm.created,
                    vm.dropped,
                    vm.private_file_pressure_reclaims,
                );
                let _ = sched::operation::tkill(pid, Some(sched::SignalNumber::SIGKILL));
                drop(task);
                deliver_user_signals_before_return(tf_ptr, from_user);
            }
            finish_trap_return(tf_ptr, from_user)
        }
        FaultOutcome::Kernel(reason) => {
            if !from_user
                && let Some(recovered) =
                    try_recover_elm_kernel_fault(tf_ptr, code, "native page fault")
            {
                return recovered;
            }
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
            let tf = unsafe { trap_frame_ref(tf_ptr) };
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
}

fn handle_access_fault(tf_ptr: usize, code: usize, from_user: bool) -> usize {
    if from_user {
        return terminate_user_exception(code, sched::SignalNumber::SIGBUS, tf_ptr, true);
    }
    if general::mm::fault::try_kernel_fixup(general::TrapFramePtr::new(tf_ptr)) {
        return tf_ptr;
    }
    if let Some(recovered) = try_recover_elm_kernel_fault(tf_ptr, code, "native access fault") {
        return recovered;
    }

    let tf = unsafe { trap_frame_ref(tf_ptr) };
    log::error!(
        "[trap][access] UNHANDLED kernel access fault code={:#x} sepc={:#x} stval={:#x}",
        code,
        tf.sepc,
        tf.tval
    );
    0
}

fn handle_user_illegal_instruction(tf_ptr: usize, code: usize) -> usize {
    let enabled = {
        let tf = unsafe { trap_frame_mut(tf_ptr) };
        let vector_enabled = crate::riscv64::vector::enable_user_vector_if_needed(tf);
        let fpu_enabled = enable_user_fpu_if_needed(tf);
        vector_enabled || fpu_enabled
    };

    if enabled {
        // 保持 sepc 不变，让刚刚启用的扩展指令重新执行。
        finish_trap_return(tf_ptr, true)
    } else {
        terminate_user_exception(code, signal_for_user_exception(code), tf_ptr, true)
    }
}

fn handle_breakpoint(tf_ptr: usize, code: usize, from_user: bool) -> usize {
    if from_user {
        return terminate_user_exception(code, sched::SignalNumber::SIGTRAP, tf_ptr, true);
    }
    if let Some(recovered) = try_recover_elm_kernel_fault(tf_ptr, code, "native breakpoint") {
        return recovered;
    }

    let tf = unsafe { trap_frame_ref(tf_ptr) };
    log::error!(
        "[trap][breakpoint] kernel breakpoint sepc={:#x} stval={:#x}",
        tf.sepc,
        tf.tval
    );
    0
}

fn handle_unhandled_exception(tf_ptr: usize, code: usize, from_user: bool) -> usize {
    if from_user {
        return terminate_user_exception(code, signal_for_user_exception(code), tf_ptr, true);
    }
    if let Some(recovered) = try_recover_elm_kernel_fault(tf_ptr, code, "native exception") {
        return recovered;
    }

    let exception = decode_exception(code);
    let tf = unsafe { trap_frame_ref(tf_ptr) };
    log::error!(
        "[trap][exception] UNHANDLED kernel exception {:?} code={:#x} sepc={:#x} stval={:#x}",
        exception,
        code,
        tf.sepc,
        tf.tval
    );
    0
}

/// RISC-V64 统一异常入口（Rust 端）。
///
/// # Safety
/// 本函数由汇编入口调用，必须遵循参数约定传递有效的 TrapFrame 指针。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn riscv64_handle_exception(tf_ptr: usize, _user_sp: usize) -> usize {
    // 只有实际中断原生 ELM 时，trap 内部分配才需要排除在该单元账本之外。普通用户
    // syscall 可能在分派阶段启动 ELM，不能让后创建的执行上下文继承暂停状态。
    let _accounting_suspension = general::elm_guard::native_execution_active()
        .then(allocator::suspend_implicit_allocation_accounting)
        .flatten();
    // 汇编入口已经完成最危险的硬件现场保存；Rust 端从这里开始处理"解释现场并决定命运"。
    // 返回非零表示异常可恢复，汇编端将按该 TrapFrame 恢复寄存器并执行 `sret`；
    // 返回零表示当前策略认定无法恢复，汇编端进入停机路径。
    let (cause, saved_status) = {
        let tf = unsafe { trap_frame_ref(tf_ptr) };
        (tf.cause, tf.status)
    };
    let is_interrupt = (cause & SCAUSE_INTERRUPT) != 0;
    let code = cause & !SCAUSE_INTERRUPT;
    let from_user = (saved_status & SSTATUS_SPP) == 0;

    if is_interrupt {
        return handle_interrupt(tf_ptr, cause, code, from_user);
    }

    if !from_user
        && matches!(code, EXC_ECALL_U | EXC_ECALL_S)
        && let Some(recovered) = try_recover_elm_kernel_fault(tf_ptr, code, "native ecall")
    {
        return recovered;
    }

    if code == EXC_ECALL_U && from_user {
        return handle_user_syscall(tf_ptr);
    }

    if matches!(
        code,
        EXC_INST_PAGE_FAULT | EXC_LOAD_PAGE_FAULT | EXC_STORE_PAGE_FAULT
    ) {
        return handle_page_fault(tf_ptr, code, from_user);
    }

    if matches!(code, EXC_INST_ACCESS | EXC_LOAD_ACCESS | EXC_STORE_ACCESS) {
        return handle_access_fault(tf_ptr, code, from_user);
    }

    if code == EXC_ILLEGAL_INST && from_user {
        return handle_user_illegal_instruction(tf_ptr, code);
    }

    if code == EXC_BREAKPOINT {
        return handle_breakpoint(tf_ptr, code, from_user);
    }

    handle_unhandled_exception(tf_ptr, code, from_user)
}

// ── syscall 快速路径 ─────────────────────────────────────────────────────────

/// syscall 快速路径 handler。与 `riscv64_handle_exception` 相同签名，
/// 但只处理 ecall（入口未保存 FPU）。
///
/// 由汇编快速路径在确认 scause=8 后直接调用。
/// FS=Off 可完全跳过 FPU；FS=Clean 已有可信的 TrapFrame 副本，也可跳过入口保存，
/// 返回桩通过 per-hart context-switch sequence 判断 live FPR 是否仍属于当前任务；
/// 发生过调度时才重载。FS=Initial/Dirty 或任意 VS active 状态仍走完整恢复，避免
/// signal/resched/frame rewrite 读取不完整状态。
#[unsafe(no_mangle)]
pub extern "C" fn riscv64_fast_syscall_dispatch(tf_ptr: usize, _user_sp: usize) -> usize {
    let (nr, args, original_satp, original_sepc, original_sp, original_switch_sequence) = {
        let tf = unsafe { trap_frame_ref(tf_ptr) };
        (
            tf.a7,
            [tf.a0, tf.a1, tf.a2, tf.a3, tf.a4, tf.a5],
            tf.satp,
            tf.sepc,
            tf.sp,
            tf.tval,
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

    let mut require_full_restore = rewrites_user_frame(nr);
    if sched::needs_resched_current() {
        sched::preempt_if_needed(kernel_timestamp_ns());
        require_full_restore = true;
    }
    prepare_user_state_before_return(tf_ptr, true);

    let frame = unsafe { trap_frame_ref(tf_ptr) };
    // signal delivery、exec/sigreturn 或其它上下文重写都会改变 PC/SP。最小返回不会
    // 恢复 s0-s10，因此只有仍保持普通 syscall 收尾形态时才可使用。
    if frame.sepc != original_sepc.wrapping_add(4) || frame.sp != original_sp {
        require_full_restore = true;
    }
    let fs = frame.status & SSTATUS_FS_MASK;
    let vs = frame.status & SSTATUS_VS_MASK;
    let switched =
        crate::riscv64::specific::current_context_switch_sequence() != original_switch_sequence;
    #[cfg(feature = "performance-profile")]
    if switched {
        profiling::observe(profiling::Metric::SyscallReturnAfterSwitch, 1);
    }
    let unsupported_extension_state = vs != 0 || !matches!(fs, 0 | SSTATUS_FS_CLEAN);
    if require_full_restore || unsupported_extension_state {
        #[cfg(feature = "performance-profile")]
        {
            profiling::observe(profiling::Metric::SyscallReturnFull, 1);
            if fs != 0 {
                profiling::observe(profiling::Metric::SyscallReturnFpuRestore, 1);
            }
            if vs != 0 {
                profiling::observe(profiling::Metric::SyscallReturnVectorRestore, 1);
            }
        }
        // signal/exec/sigreturn、调度或扩展状态恢复会进入完整 resume；此时必须
        // 重建可信 kstack/satp 并净化用户可修改的 frame。普通 fast return 的
        // status 已由汇编掩码，且入口写入的 kstack/satp 未被改动，无需重复写。
        sanitize_user_return_frame(tf_ptr, (nr == SYS_RT_SIGRETURN).then_some(original_satp));
        return tf_ptr | 1;
    }
    #[cfg(feature = "performance-profile")]
    {
        profiling::observe(profiling::Metric::SyscallReturnFast, 1);
        if switched && fs != 0 {
            profiling::observe(profiling::Metric::SyscallReturnFpuRestore, 1);
        }
    }
    tf_ptr
}

fn enable_user_fpu_if_needed(tf: &mut TrapFrame) -> bool {
    if tf.status & SSTATUS_FS_MASK != 0 || !looks_like_fpu_instruction(tf.sepc) {
        return false;
    }

    tf.f.fill(0);
    tf.fcsr = 0;
    tf._pad = 0;
    // trap entry 已把当前 S-mode 的硬件 FS 临时置为 Dirty，Rust handler 可安全
    // 执行；这里只发布将要恢复给用户的任务状态，避免首次 FPU fault 重复写 CSR。
    tf.status = (tf.status & !SSTATUS_FS_MASK) | SSTATUS_FS_DIRTY;
    true
}

fn looks_like_fpu_instruction(pc: usize) -> bool {
    let mut lo = [0u8; 2];
    if crate::riscv64::mm::user_copy::copy_instruction_from_user(pc, &mut lo).is_err() {
        return false;
    }
    let half = u16::from_le_bytes(lo);
    if half & 0x3 != 0x3 {
        return matches!(half & 0xe003, 0x2000 | 0xa000 | 0x2002 | 0xa002);
    }

    let mut raw = [0u8; 4];
    raw[..2].copy_from_slice(&lo);
    if crate::riscv64::mm::user_copy::copy_instruction_from_user(pc.wrapping_add(2), &mut raw[2..])
        .is_err()
    {
        return false;
    }
    let insn = u32::from_le_bytes(raw);
    let opcode = insn & 0x7f;
    match opcode {
        // LOAD-FP/STORE-FP：RV64 标量 F/D 只接受 FLW/FLD/FSW/FSD width。
        0x07 | 0x27 => matches!((insn >> 12) & 0x7, 0b010 | 0b011),
        0x43 | 0x47 | 0x4b | 0x4f | 0x53 => true,
        0x73 => {
            let funct3 = (insn >> 12) & 0x7;
            let csr = (insn >> 20) & 0xfff;
            funct3 != 0 && matches!(csr, 0x001..=0x003)
        }
        _ => false,
    }
}

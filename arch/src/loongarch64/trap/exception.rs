//! LoongArch64 异常/中断分发处理。
//!
//! 本模块实现异常入口的 Rust 端逻辑，由汇编入口
//! `__loongarch_exception_entry` 保存完整 TrapFrame 后调用。
//!
//! 参数约定（与汇编端一致）：
//! - `arg0`    : PC（CSR_ERA 快照）
//! - `arg1`    : ESTAT（CSR_ESTAT 快照）
//! - `arg2`    : BADV（CSR_BADV 快照）
//! - `arg3`    : SP（用户栈指针）
//! - `arg4`    : TrapFrame 指针（内核栈上，已由汇编填好）
//!
//! 返回值：
//! - 非零 → 汇编端恢复该 TrapFrame 指针并 ertn；
//! - 零   → 汇编端进入死循环宕机。

use crate::*;
use general::{Exception, Interrupt, TrapType};

// LoongArch64 ESTAT 字段偏移/掩码
const ESTAT_ECODE_SHIFT: usize = 16;
const ESTAT_ECODE_MASK: usize = 0x3f;
const ESTAT_ESUBCODE_SHIFT: usize = 22;
const ESTAT_ESUBCODE_MASK: usize = 0x1ff;

// IS 位定义（ESTAT[12:0]）
const IS_SWI_MASK: usize = 0x0003; // bits 1:0  – 软件中断
const IS_HWI_MASK: usize = 0x00fc; // bits 7:2  – 硬件中断 HWI0-5
const IS_TIMER_BIT: usize = 1 << 11; // bit 11    – 定时器中断
const IS_IPI_BIT: usize = 1 << 12; // bit 12    – 核间中断

/// 将 `TrapFrame` 指针转换为可变引用。
///
/// # Safety
/// `ptr` 必须是汇编入口写入的有效、对齐且完整的 TrapFrame 指针。
#[inline]
unsafe fn trap_frame_mut<'a>(ptr: usize) -> &'a mut TrapFrame {
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
            sched::schedule_once(super::super::specific::kernel_timestamp_ns());
            panic!("[trap][signal] terminal task scheduled back unexpectedly");
        }
        sched::TaskState::Stopped | sched::TaskState::Continued => {
            drop(task);
            sched::schedule_once(super::super::specific::kernel_timestamp_ns());
        }
        _ => {}
    }
}

/// 在从用户态 trap 返回前投递异步信号。
///
/// syscall 返回路径已经在 `general::syscall::dispatch` 中带 trap-frame context
/// 投递一次；这里补齐 timer/外设中断和可恢复异常路径。这样忙循环的用户线程
/// 即使不再进入 syscall，也能在下一次时钟中断返回前进入用户信号 handler。
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
            sched::schedule_once(super::super::specific::kernel_timestamp_ns());
            panic!("[trap][signal] terminal task scheduled back unexpectedly");
        }
        sched::TaskState::Stopped | sched::TaskState::Continued => {
            drop(task);
            sched::schedule_once(super::super::specific::kernel_timestamp_ns());
        }
        _ => {}
    }
}

/// 解码中断：从 IS 字段选出最高优先级中断类型。
fn decode_interrupt(is: usize) -> Interrupt {
    // LoongArch 把待处理中断线压在 ESTAT.IS 位域里，但 Rust 侧更关心语义化后的
    // “定时器 / IPI / 某条 HWI”。这里做的就是把硬件位图翻译成上层更容易消费的枚举。
    // 当前策略按优先级顺序挑选一个代表项，而不是返回位图全集。
    if is & IS_IPI_BIT != 0 {
        Interrupt::Ipi
    } else if is & IS_TIMER_BIT != 0 {
        Interrupt::Timer
    } else if is & IS_HWI_MASK != 0 {
        // 找到最低设置位的硬件中断编号（HWI0 对应 bit 2）
        let hwi_bit = (is & IS_HWI_MASK).trailing_zeros() as usize;
        Interrupt::Hardware(hwi_bit.saturating_sub(2))
    } else if is & IS_SWI_MASK != 0 {
        Interrupt::Other(is & IS_SWI_MASK)
    } else {
        Interrupt::Other(is)
    }
}

/// 解码 ECODE 为 Exception 类型。
fn decode_exception(ecode: usize, _esubcode: usize) -> Exception {
    // 这里保留的是“异常类别翻译表”语义：把 ESTAT.ECODE 这种硬件编码转成通用异常枚举，
    // 方便后续打印、统计和更高层策略判断。esubcode 目前尚未细分使用，因此先保留参数位。
    match ecode {
        ECODE_PIL | ECODE_PIS | ECODE_PIF => Exception::LoadPageFault,
        ECODE_PME => Exception::PageModified,
        ECODE_PNR => Exception::PageNoRead,
        ECODE_PNX => Exception::PageNoExecute,
        ECODE_PPI => Exception::PagePrivilegeIllegal,
        ECODE_ADE => Exception::AddressError,
        ECODE_ALE => Exception::AddressAlignmentError,
        ECODE_BCE => Exception::BoundsCheck,
        ECODE_BRK => Exception::Breakpoint,
        ECODE_INE => Exception::IllegalInstruction,
        ECODE_IPE => Exception::InstructionPrivilege,
        ECODE_FPD => Exception::FloatingPointDisabled,
        ECODE_SXD => Exception::VectorExtDisabled,
        ECODE_ASXD => Exception::AdvancedVectorExtDisabled,
        ECODE_FPE => Exception::FloatingPointException,
        other => Exception::Other(other),
    }
}

/// LoongArch64 统一异常入口（Rust 端）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn loongarch64_handle_exception(
    arg0: usize, // $r4 = PC (CSR_ERA snapshot)
    arg1: usize, // $r5 = ESTAT
    arg2: usize, // $r6 = BADV
    arg3: usize, // $r7 = SP
    arg4: usize, // $r8 = TrapFrame ptr
) -> usize {
    let from_user = unsafe { trap_frame_mut(arg4) }.status & CSR_PRMD_PPLV_MASK != 0;
    let result = unsafe { loongarch64_handle_exception_inner(arg0, arg1, arg2, arg3, arg4) };
    if result != 0 {
        prepare_user_state_before_return(result, from_user);
    }
    result
}

unsafe fn loongarch64_handle_exception_inner(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
) -> usize {
    // 只有实际中断原生 ELM 时，trap 内部分配才需要排除在该单元账本之外。普通用户
    // syscall 可能在分派阶段启动 ELM，不能让后创建的执行上下文继承暂停状态。
    let _accounting_suspension = general::elm_guard::native_execution_active()
        .then(allocator::suspend_implicit_allocation_accounting)
        .flatten();
    // 即使 trap 发生前本地临时屏蔽了 IPI，也在进入 Rust 分发的安全边界
    // 处理其它 CPU 发布的 TLB/I-cache 刷新请求。
    super::super::smp::handle_shootdown_requests();
    // 汇编入口已经完成最危险的硬件现场保存；Rust 端从这里开始处理“解释现场并决定命运”。
    // 返回非零表示异常可恢复，汇编端将按该 TrapFrame 恢复寄存器并执行 `ertn`；
    // 返回零表示当前策略认定无法恢复，汇编端进入停机路径。
    let estat = arg1;
    let is = estat & ESTAT_IS_MASK;
    let ecode = (estat >> ESTAT_ECODE_SHIFT) & ESTAT_ECODE_MASK;
    let esubcode = (estat >> ESTAT_ESUBCODE_SHIFT) & ESTAT_ESUBCODE_MASK;

    let tf = unsafe { trap_frame_mut(arg4) };
    let from_user = (tf.status & CSR_PRMD_PPLV_MASK) != 0;

    if ecode == ECODE_INT {
        // 对中断而言，最关键的信息是 IS 位域。与同步异常不同，中断通常不需要 BADV，
        // 且多数情况下 PC 只用于诊断，不决定恢复逻辑。
        let intr = decode_interrupt(is);
        log::debug!(
            "[trap] interrupt {:?} pc={:#x} cpu={}",
            intr,
            arg0,
            LoongArch64MessageInterruptOps::current_cpu_id()
        );
        if is & IS_IPI_BIT != 0 {
            super::super::smp::handle_ipi();
            let now_ns = super::super::specific::kernel_timestamp_ns();
            deliver_user_signals_before_return(arg4, from_user);
            // IPI 在 idle 任务的内核态也可能是唯一一次 resched 通知。若只在
            // from_user 时抢占，IPI 恰好落在 idle_entry 的 schedule_once 与
            // 下一条 `idle 0` 之间时会被清掉，随后 CPU 进入 WFI，而已经入队的
            // 任务再也没有事件把它唤醒。只有 idle_relax 已发布“安全等待”标记
            // 时才在此处调度；标记未发布意味着 IPI 可能打断 schedule_once，
            // 不能在中断上下文重入 runqueue。
            let preempt_idle = if !from_user && super::super::sched_ctx::idle_waiting() {
                // current_raw 是无锁的 per-CPU 发布槽；此处避免再次取得
                // CpuSchedState.current 锁，也避免其它 kernel task 误触发。
                // 确认 idle 后原子消费标记，防止它跨本次上下文切换残留。
                sched::current_task_ref().is_idle_task()
                    && super::super::sched_ctx::take_idle_waiting()
            } else {
                false
            };
            if from_user || preempt_idle {
                sched::preempt_if_needed(now_ns);
            }
            return arg4;
        }
        // 清除定时器中断标志（写 CSR_TICLR bit 0）
        if is & IS_TIMER_BIT != 0 {
            #[cfg(feature = "performance-profile")]
            profiling::sample_pc(arg0, from_user);
            // LoongArch 的定时器中断通常需要软件显式写 TICLR 清 pending，否则在 `ertn`
            // 后会立即再次陷入，形成“看似无法返回”的中断风暴。
            unsafe {
                core::arch::asm!(
                    "csrwr {val}, {csr}",
                    val = in(reg) 1usize,
                    csr = const CSR_TICLR,
                    options(nostack, preserves_flags)
                );
            }
            general::dev::irq::record_timer_interrupt();
            // TCFG 使用 one-shot 模式；先恢复常规 tick 作为兜底。调度器处理完
            // 到期等待后会按新的最早 deadline 再次缩短本次计时。
            super::super::loader::rearm_local_timer(None);
            // 通知调度器推进虚拟时间；若时间片用完会置 NEED_RESCHED，下方
            // 返回前的 preempt_if_needed 会真正切换。
            let now_ns = super::super::specific::kernel_timestamp_ns();
            let _ = general::elm_guard::request_timeout_if_expired(now_ns);
            if !from_user
                && let Some(recovery) = general::elm_guard::try_recover_requested_abort(tf.pc)
            {
                log::warning!(
                    "[trap][elm] forced native exit cell={} phase={} reason={} return_pc={:#x} return_sp={:#x}",
                    recovery.cell,
                    recovery.phase,
                    recovery.reason,
                    recovery.return_pc,
                    recovery.return_sp
                );
                tf.pc = recovery.return_pc;
                tf.sp = recovery.return_sp;
                tf.a0 = recovery.return_value;
                return arg4;
            }
            let boot_cpu = LoongArch64MessageInterruptOps::current_cpu_id() == 0;
            if boot_cpu {
                super::super::vdso::run_timer_tick_hook(now_ns);
            }
            // timer 可能打断持有 runqueue、topology 等普通自旋锁的内核路径。
            // 内核态 top-half 只记录待处理时间，随后由 syscall 返回或主动调度的
            // 无锁边界补做调度工作，避免本 CPU 重入锁后永久自旋。
            if !from_user {
                sched::defer_timer_tick(now_ns);
                if boot_cpu && general::elm_guard::active_cell() == 0 {
                    super::super::vdso::run_net_poll_hook(now_ns);
                    super::super::vdso::run_tty_poll_hook(now_ns);
                }
                return arg4;
            }

            let deadline_fired = sched::on_timer_tick(now_ns);
            deliver_user_signals_before_return(arg4, from_user);

            // deadline 到期时先让被唤醒任务运行，避免网络和 TTY 周期轮询叠加到
            // 短超时延迟。内核态中断已经在上方返回，此处是安全的用户态返回边界。
            let urgent_preempt = deadline_fired && sched::is_ready();
            if urgent_preempt {
                sched::preempt_if_needed(now_ns);
            }
            if boot_cpu {
                // 网络协议栈 poll：每 ~10ms 推一帧即可覆盖常见用例；
                // 调频若需要更细的节流，kernel 应在 hook 内部按 now_ns 自
                // 行 throttle。默认每次 tick 都调——smoltcp 的零分配 poll
                // 路径在 RX 队列空时本身极快（一次 mutex + 几次状态查询）。
                super::super::vdso::run_net_poll_hook(now_ns);
                // TTY 输入泵：即使前台任务没有调用 read()，也要及时处理
                // VINTR/VQUIT/VSUSP 这类控制字符并投递给前台进程组。
                super::super::vdso::run_tty_poll_hook(now_ns);
            }
            // 中断可能打断内核临界区。抢占只在返回用户态前消费，内核态返回
            // 继续执行被打断路径，避免在未知锁/栈状态下切走当前任务。
            if !urgent_preempt {
                sched::preempt_if_needed(now_ns);
            }
            return arg4;
        }
        let now_ns = super::super::specific::kernel_timestamp_ns();
        let _ = general::dev::irq::dispatch_interrupt(intr);
        if !from_user && let Some(recovery) = general::elm_guard::try_recover_requested_abort(tf.pc)
        {
            tf.pc = recovery.return_pc;
            tf.sp = recovery.return_sp;
            tf.a0 = recovery.return_value;
            return arg4;
        }
        // 串口输入在 UART 外部中断到来时最可靠：此时硬件 FIFO 已经可读，
        // 需要马上拉进 TTY 行规程，避免没有 reader 的前台任务错过 Ctrl-C。
        super::super::vdso::run_tty_poll_hook(now_ns);
        // trap 返回前的抢占检查：只有在进入过 sched::init 之后才生效，否则
        // 启动早期的中断会在尚无 current 时 panic。
        deliver_user_signals_before_return(arg4, from_user);
        // 与 timer 分支一致，只在返回用户态前处理抢占请求。
        if from_user {
            sched::preempt_if_needed(now_ns);
        }
        arg4
    } else if ecode == ECODE_SYS {
        if !from_user
            && let Some(recovery) = general::elm_guard::try_recover_kernel_fault(arg0, arg2, ecode)
        {
            log::warning!(
                "[trap][elm] recovered native syscall cell={} phase={} pc={:#x} badv={:#x} ecode={} return_pc={:#x} return_sp={:#x}",
                recovery.cell,
                recovery.phase,
                arg0,
                arg2,
                ecode,
                recovery.return_pc,
                recovery.return_sp
            );
            tf.pc = recovery.return_pc;
            tf.sp = recovery.return_sp;
            tf.a0 = recovery.return_value;
            return arg4;
        }
        // syscall 通过注入的 SyscallFrameOps 读 a7/a0-a5、写返回值、推 PC。
        // general::syscall::dispatch 本轮全部返 ENOSYS；ELF loader 那轮再逐条加 arm。
        // log::debug!("[trap] syscall id={} pc={:#x} from_user={}", tf.a7, arg0, from_user);
        general::syscall::dispatch(general::TrapFramePtr::new(arg4));
        // syscall 内部可能唤醒了其它任务或新建了子进程，并通过
        // request_resched() 标记当前 CPU 需要重调度。系统调用返回用户态前
        // 立即消费该标记，避免当前任务在同一时间片里连续启动 client，
        // 而刚 fork/唤醒的 server 只能等下一次 timer tick。
        if sched::needs_resched_current() {
            sched::preempt_if_needed(super::super::specific::kernel_timestamp_ns());
        }
        arg4
    } else if from_user && matches!(ecode, ECODE_FPD | ECODE_SXD) {
        let enable = match ecode {
            ECODE_FPD => EUEN_FPE,
            ECODE_SXD => {
                // SXE 关闭时入口没有可保存的向量状态。用已有 FPR 低 64 位初始化
                // 每个 LSX 寄存器，并清零高 64 位，再让返回路径装入确定的状态。
                let scalar_state_saved = tf.euen & FPU_SAVED != 0;
                for index in 0..tf.lsx.len() {
                    tf.lsx[index] = [if scalar_state_saved { tf.f[index] } else { 0 }, 0];
                }
                tf.euen |= LSX_SAVED;
                EUEN_SXE
            }
            _ => 0,
        };
        tf.euen |= enable;
        log::debug!(
            "[trap] enabled user extension euen_bits={:#x} pc={:#x}",
            enable,
            arg0
        );
        arg4
    } else if matches!(
        ecode,
        ECODE_PIL | ECODE_PIS | ECODE_PIF | ECODE_PME | ECODE_PNR | ECODE_PNX | ECODE_PPI
    ) {
        // 新内核堆映射不在任意调用方锁内同步等待远端 CPU。若本核缓存了映射发布
        // 前的无效 translation，先按页表映射代次做一次受限的本地收敛。权限异常
        // 和同一代次重复故障不会进入该路径，仍按真正的内核错误处理。
        if !from_user
            && matches!(ecode, ECODE_PIL | ECODE_PIS | ECODE_PIF)
            && super::super::heap_vm::recover_stale_kernel_heap_translation(
                arg2,
                ecode == ECODE_PIS,
                ecode == ECODE_PIF,
            )
        {
            return arg4;
        }
        // 缺页族 → 统一走 general::mm::dispatch_page_fault。
        // 分派结果：
        //   Fixed                      → 重试指令，返回 arg4；
        //   Segv                       → 本轮先 log 并 halt；sched 加 SIGSEGV 投递接口后改成投信号；
        //   Kernel(NotInitialized)     → 启动早期缺页，按旧路径 halt；
        //   Kernel(UncaughtKernelAccess) → 真内核 bug，halt。
        use general::mm::{FaultOutcome, KernelFaultReason};
        let tf_ptr = general::TrapFramePtr::new(arg4);
        match general::mm::dispatch_page_fault(tf_ptr) {
            FaultOutcome::Fixed => {
                deliver_user_signals_before_return(arg4, from_user);
                arg4
            }
            FaultOutcome::Segv => {
                // 同步 page fault 必须在返回用户态前立刻投递 SIGSEGV。若只把信号
                // 入队再返回，lmbench lat_sig prot 这类“handler 返回后重试同一条
                // fault 指令”的测试会在同一 PC 上反复 fault，永远等不到 syscall/
                // timer 边界来安装用户 signal frame。
                if sched::is_ready() {
                    let me = sched::current_task();
                    let pid = me.pid_root().unwrap_or(0);
                    let _ = sched::operation::tkill(pid, Some(sched::SignalNumber::SIGSEGV));
                    drop(me);
                    deliver_user_signals_before_return(arg4, from_user);
                }
                arg4
            }
            FaultOutcome::Kernel(reason) => {
                if !from_user
                    && let Some(recovery) =
                        general::elm_guard::try_recover_kernel_fault(arg0, arg2, ecode)
                {
                    log::warning!(
                        "[trap][elm] recovered native fault cell={} phase={} pc={:#x} badv={:#x} ecode={} return_pc={:#x} return_sp={:#x}",
                        recovery.cell,
                        recovery.phase,
                        arg0,
                        arg2,
                        ecode,
                        recovery.return_pc,
                        recovery.return_sp
                    );
                    tf.pc = recovery.return_pc;
                    tf.sp = recovery.return_sp;
                    tf.a0 = recovery.return_value;
                    return arg4;
                }
                log::debug!(
                    "[trap][mm] kernel fault ({:?}) pc={:#x} badv={:#x} ecode={}",
                    reason,
                    arg0,
                    arg2,
                    ecode
                );
                let _ = KernelFaultReason::NotInitialized;
                0
            }
        }
    } else {
        // 非中断、非 syscall 的路径通常代表真正的同步故障，例如页故障、地址错、非法指令。
        // 当前内核尚未实现可恢复异常处理，因此除了断点外，一律记录现场后宣告不可恢复。
        let exc = decode_exception(ecode, esubcode);
        log::debug!(
            "[trap] exception {:?} pc={:#x} sp={:#x} bad_addr={:#x} \
             ecode={} esubcode={} estat={:#x} from_user={}",
            TrapType::Exception(exc),
            arg0,
            arg3,
            arg2,
            ecode,
            esubcode,
            estat,
            from_user
        );
        log::debug!(
            "[trap] regs ra={:#x} a0={:#x} a1={:#x} a2={:#x} a3={:#x} \
             a4={:#x} a5={:#x} a6={:#x} a7={:#x} t0={:#x} t1={:#x}",
            tf.ra,
            tf.a0,
            tf.a1,
            tf.a2,
            tf.a3,
            tf.a4,
            tf.a5,
            tf.a6,
            tf.a7,
            tf.t0,
            tf.t1
        );

        if !from_user
            && let Some(recovery) = general::elm_guard::try_recover_kernel_fault(arg0, arg2, ecode)
        {
            log::warning!(
                "[trap][elm] recovered native exception cell={} phase={} pc={:#x} badv={:#x} ecode={} return_pc={:#x} return_sp={:#x}",
                recovery.cell,
                recovery.phase,
                arg0,
                arg2,
                ecode,
                recovery.return_pc,
                recovery.return_sp
            );
            tf.pc = recovery.return_pc;
            tf.sp = recovery.return_sp;
            tf.a0 = recovery.return_value;
            return arg4;
        }

        if matches!(exc, Exception::Breakpoint) {
            // 断点异常的硬件语义更接近“调试陷入”而不是致命错误；最小可恢复策略就是跳过
            // 当前断点指令，让执行流继续向前。
            tf.pc = tf.pc.wrapping_add(4);
            return arg4;
        }

        // 其余异常目前无法恢复：返回 0 让汇编端宕机
        0
    }
}

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
        // 清除定时器中断标志（写 CSR_TICLR bit 0）
        if is & IS_TIMER_BIT != 0 {
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
            // 通知调度器推进虚拟时间；若时间片用完会置 NEED_RESCHED，下方
            // 返回前的 preempt_if_needed 会真正切换。
            let now_ns = super::super::specific::kernel_timestamp_ns();
            sched::on_timer_tick(now_ns);
            super::super::vdso::run_timer_tick_hook(now_ns);
            sched::preempt_if_needed(now_ns);
            return arg4;
        }
        // trap 返回前的抢占检查：只有在进入过 sched::init 之后才生效，否则
        // 启动早期的中断会在尚无 current 时 panic。
        sched::preempt_if_needed(super::super::specific::kernel_timestamp_ns());
        arg4
    } else if ecode == ECODE_SYS {
        // syscall 通过注入的 SyscallFrameOps 读 a7/a0-a5、写返回值、推 PC。
        // general::syscall::dispatch 本轮全部返 ENOSYS；ELF loader 那轮再逐条加 arm。
        // log::debug!("[trap] syscall id={} pc={:#x} from_user={}", tf.a7, arg0, from_user);
        general::syscall::dispatch(general::TrapFramePtr::new(arg4));
        arg4
    } else if from_user && matches!(ecode, ECODE_FPD | ECODE_SXD | ECODE_ASXD) {
        let enable = match ecode {
            ECODE_FPD => EUEN_FPE,
            ECODE_SXD => EUEN_SXE,
            ECODE_ASXD => EUEN_SXE | EUEN_ASXE,
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
        ECODE_PIL | ECODE_PIS | ECODE_PIF | ECODE_PME | ECODE_PNR | ECODE_PNX
    ) {
        // 缺页族 → 统一走 general::mm::dispatch_page_fault。
        // 分派结果：
        //   Fixed                      → 重试指令，返回 arg4；
        //   Segv                       → 本轮先 log 并 halt；sched 加 SIGSEGV 投递接口后改成投信号；
        //   Kernel(NotInitialized)     → 启动早期缺页，按旧路径 halt；
        //   Kernel(UncaughtKernelAccess) → 真内核 bug，halt。
        use general::mm::{FaultOutcome, KernelFaultReason};
        let tf_ptr = general::TrapFramePtr::new(arg4);
        match general::mm::dispatch_page_fault(tf_ptr) {
            FaultOutcome::Fixed => arg4,
            FaultOutcome::Segv => {
                log::info!(
                    "[trap][mm] user SIGSEGV pc={:#x} badv={:#x} ecode={}",
                    arg0,
                    arg2,
                    ecode
                );
                // 投 SIGSEGV 给当前线程；下一次调度边界 deliver_pending_signals
                // 拿到默认 Term 动作即触发 exit_task。本轮 hello 跑通不应触发；
                // 兜底替代旧的 halt 行为。
                if sched::is_ready() {
                    let me = sched::current_task();
                    let pid = me.pid_root().unwrap_or(0);
                    let _ = sched::operation::tkill(pid, Some(sched::SignalNumber::SIGSEGV));
                    sched::schedule_once(super::super::specific::kernel_timestamp_ns());
                }
                arg4
            }
            FaultOutcome::Kernel(reason) => {
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

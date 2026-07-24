//! LoongArch64 内核上下文切换实现。
//!
//! 这里只负责"内核线程之间的寄存器级切换"；trap frame（被 [`task`](super::task)
//! 模块管理）是另一层抽象：trap 入口把用户态寄存器保存到用户栈上的 TrapFrame，
//! 而内核态的调度切换保存的是内核栈上的被调用保存寄存器。两者职责不重叠。
//!
//! ## 布局
//!
//! 每个 Task 持有一段 128 字节的 `KernelContext` 缓冲，按 16 字节对齐。字段顺序
//! 与下方 `KCTX_*_OFFSET` 常量一致：
//!
//! ```text
//!   +0x00  ra      首次被切入时要 `ret` 到的地址（通常是 trampoline）
//!   +0x08  sp      线程自己的内核栈指针
//!   +0x10  s0      \
//!   +0x18  s1       |
//!   +0x20  s2       |
//!   +0x28  s3       |  LoongArch64 callee-saved 寄存器
//!   +0x30  s4       |  对应 $r23..$r31
//!   +0x38  s5       |
//!   +0x40  s6       |
//!   +0x48  s7       |
//!   +0x50  s8       |
//!   +0x58  s9      /
//!   +0x60  (pad)   填充到 0x80 以保 16 对齐
//! ```
//!
//! 注意 LoongArch64 ABI 里：
//!
//! - `$r1`  = `ra`
//! - `$r3`  = `sp`
//! - `$r22` = `fp` (也叫 `s9`)
//! - `$r23..$r31` = `s0..s8`
//! - `$r2`  = `tp`（线程指针），跨切换基本不变，此处不保存
//!
//! 我们把 fp(`$r22`) 命名为 s9、`$r23..$r31` 命名为 s0..s8，共 10 个 callee-saved
//! 寄存器，占 0x10..0x60 这 80 字节。
//!
//! ## 新线程启动
//!
//! [`init_kernel_context`] 把：
//! - `ra`  设为 [`__kthread_trampoline`] 的地址；
//! - `sp`  设为新分配内核栈栈顶；
//! - `s0`  设为用户传入的 `entry`；
//! - `s1`  设为用户传入的 `arg`。
//!
//! 首次 [`switch_context`] 完成时硬件会 `jirl $zero, $ra, 0` 回到 trampoline，
//! trampoline 再把 `s1 → $a0`、`s0 → $t0`，`jirl $zero, $t0, 0` 跳入用户入口。

use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

use general::TaskOps;
use sched::arch_hooks::{
    ArchContextOps, ArchDeadlineTimerOps, ArchIdleOps, ArchTimeOps, ArchTrapOps, KernelEntry,
};

use super::specific::{CSR_TCFG, kernel_timestamp_ns};
use super::task::LoongArch64TaskOps;
use super::trap::{LoongArch64InterruptOps, LoongArch64MessageInterruptOps};

pub(crate) const KCTX_SIZE: usize = 128;
pub(crate) const KCTX_ALIGN: usize = 16;

const RA_OFF: usize = 0x00;
const SP_OFF: usize = 0x08;
const S0_OFF: usize = 0x10;
const S1_OFF: usize = 0x18;
const S2_OFF: usize = 0x20;
const S3_OFF: usize = 0x28;
const S4_OFF: usize = 0x30;
const S5_OFF: usize = 0x38;
const S6_OFF: usize = 0x40;
const S7_OFF: usize = 0x48;
const S8_OFF: usize = 0x50;
const S9_OFF: usize = 0x58;

/// 初始化一个新内核线程的上下文。
///
/// 见模块文档中的布局说明。`stack_top` 必须按 16 字节对齐，由调用方保证。
///
/// # Safety
///
/// - `ctx` 必须指向 `KCTX_SIZE` 字节、按 `KCTX_ALIGN` 对齐的独占缓冲；
/// - `stack_top` 指向的栈必须在 Task 活跃期间不被回收。
unsafe fn init_kernel_context(ctx: NonNull<u8>, stack_top: usize, entry: KernelEntry, arg: usize) {
    let base = ctx.as_ptr();
    // Safety: 调用方保证 base 指向 KCTX_SIZE 字节的独占缓冲。
    unsafe {
        core::ptr::write_bytes(base, 0, KCTX_SIZE);
        let w = |off: usize, v: usize| {
            (base.add(off) as *mut usize).write(v);
        };
        // 首次恢复时的 `ra` 指向 trampoline。
        w(RA_OFF, __kthread_trampoline as *const () as usize);
        // 栈顶——LoongArch64 栈向下生长，`sp` 置为逻辑栈顶本身。
        w(SP_OFF, stack_top);
        // `s0`/`s1` 承载 entry/arg，trampoline 再派发到 `$a0`/`$t0`。
        w(S0_OFF, entry as *const () as usize);
        w(S1_OFF, arg);
    }
}

/// 保存当前内核寄存器到 `prev`，从 `next` 恢复后跳到恢复点。
///
/// 调用方必须持有调度锁并关中断；因为本函数会在 prev/next 之间透明穿越，
/// 一旦中断介入触发重入可能把同一个 ctx 写两遍。
///
/// # Safety
///
/// `prev` 和 `next` 必须指向两个**独立**的、已初始化过的 `KernelContext`
/// 缓冲（`next` 可以是从未跑过的新线程——那种情况下 `ra` 指向 trampoline）。
#[unsafe(naked)]
unsafe extern "C" fn switch_context(
    _prev: NonNull<u8>,
    _next: NonNull<u8>,
    _prev_on_cpu: NonNull<core::sync::atomic::AtomicUsize>,
) {
    // LoongArch64 传参：$a0 = prev, $a1 = next, $a2 = prev_on_cpu
    // 我们把 ra/sp/s0..s9 全部保存/恢复。fp ($r22) 在我们的命名里是 s9。
    core::arch::naked_asm!(
        // ── 保存 prev ────────────────────────────────────────────────
        "st.d  $r1,  $a0, {ra_off}",     // ra
        "st.d  $r3,  $a0, {sp_off}",     // sp
        "st.d  $r22, $a0, {s9_off}",     // fp / s9
        "st.d  $r23, $a0, {s0_off}",
        "st.d  $r24, $a0, {s1_off}",
        "st.d  $r25, $a0, {s2_off}",
        "st.d  $r26, $a0, {s3_off}",
        "st.d  $r27, $a0, {s4_off}",
        "st.d  $r28, $a0, {s5_off}",
        "st.d  $r29, $a0, {s6_off}",
        "st.d  $r30, $a0, {s7_off}",
        "st.d  $r31, $a0, {s8_off}",

        // 只有保存完整上下文后，远端 CPU 才能认领并恢复 prev。
        "dbar 0",
        "st.d  $zero, $a2, 0",

        // ── 恢复 next ────────────────────────────────────────────────
        "ld.d  $r1,  $a1, {ra_off}",
        "ld.d  $r3,  $a1, {sp_off}",
        "ld.d  $r22, $a1, {s9_off}",
        "ld.d  $r23, $a1, {s0_off}",
        "ld.d  $r24, $a1, {s1_off}",
        "ld.d  $r25, $a1, {s2_off}",
        "ld.d  $r26, $a1, {s3_off}",
        "ld.d  $r27, $a1, {s4_off}",
        "ld.d  $r28, $a1, {s5_off}",
        "ld.d  $r29, $a1, {s6_off}",
        "ld.d  $r30, $a1, {s7_off}",
        "ld.d  $r31, $a1, {s8_off}",

        // 返回到 next 的 ra。对于新线程，ra 是 trampoline；对于旧线程，
        // 是它上一次被切出时 "switch_context 的下一条指令" 的返回地址。
        "jirl  $zero, $r1, 0",

        ra_off = const RA_OFF,
        sp_off = const SP_OFF,
        s0_off = const S0_OFF,
        s1_off = const S1_OFF,
        s2_off = const S2_OFF,
        s3_off = const S3_OFF,
        s4_off = const S4_OFF,
        s5_off = const S5_OFF,
        s6_off = const S6_OFF,
        s7_off = const S7_OFF,
        s8_off = const S8_OFF,
        s9_off = const S9_OFF,
    );
}

/// 新内核线程的起跳点。
///
/// 首次 `switch_context(..., next=new)` 完成时，寄存器布局为：
/// - `$r1`(ra) = 本函数地址
/// - `$r23`(s0) = entry
/// - `$r24`(s1) = arg
///
/// trampoline 把 arg 搬到 `$a0`、跳到 entry。entry 声明为 `fn(usize) -> !`，
/// 理论上永不返回；真若返回则构成严重 bug，用 `break 0` 触发 trap 定位。
#[unsafe(naked)]
unsafe extern "C" fn __kthread_trampoline() {
    core::arch::naked_asm!(
        "move  $a0, $r24",   // arg  → $a0
        "move  $t0, $r23",   // entry → $t0
        "jirl  $ra, $t0, 0", // 调用 entry(arg)；按约定不返回
        "break 0",           // safeguard：entry 若返回立刻 trap
    );
}

// ── 注入 ──────────────────────────────────────────────────────────────────────

/// 内核上下文切换契约的静态实例。
static ARCH_CONTEXT_OPS: ArchContextOps = ArchContextOps {
    context_size: KCTX_SIZE,
    context_align: KCTX_ALIGN,
    init_kernel_context,
    switch_context,
};

/// 时间戳 + 当前 CPU id 契约。`now_ns` 接 rdtime 扫频后的单调纳秒源；
/// `current_cpu_id` 读 CSR_CPUID 的 coreid 位域。
static ARCH_TIME_OPS: ArchTimeOps = ArchTimeOps {
    now_ns: kernel_timestamp_ns,
    current_cpu_id: arch_current_cpu_id,
};

/// 将调度器发布的绝对软件截止时间投影到当前 CPU 的 TCFG one-shot 定时器。
static ARCH_DEADLINE_TIMER_OPS: ArchDeadlineTimerOps = ArchDeadlineTimerOps {
    reprogram: super::loader::rearm_local_timer,
};

fn arch_current_cpu_id() -> usize {
    LoongArch64MessageInterruptOps::current_cpu_id()
}

/// 切换后由 sched 调用，把 CSR_KS0 指向 next 的内核栈顶。
static ARCH_TRAP_OPS: ArchTrapOps = ArchTrapOps {
    set_kernel_trap_stack: set_kernel_trap_stack_raw,
};

static ARCH_IDLE_OPS: ArchIdleOps = ArchIdleOps {
    idle_relax: loongarch64_idle_relax,
};

fn loongarch64_idle_relax() {
    unsafe {
        let timer_config: usize;
        core::arch::asm!(
            "csrrd {value}, {csr}",
            value = out(reg) timer_config,
            csr = const CSR_TCFG,
            options(nomem, nostack, preserves_flags)
        );
        // one-shot 计时器可能在内核态处理多个短 deadline 时耗尽，而 pending
        // 已被上一轮中断清除。此时直接 idle 将永远没有事件唤醒本 CPU；idle
        // 是进入硬件等待前的公共安全边界，必须保证常规调度 tick 仍然启用。
        if timer_config & 1 == 0 {
            super::loader::rearm_local_timer(None);
        }
        // idle 任务运行在内核态，普通 trap/系统调用返回路径不会替它恢复
        // PRMD.PIE。进入 idle 等待窗口前必须临时打开 CRMD.IE，否则 timer
        // interrupt 不能唤醒 timed sleepers，阻塞 read/select 会永久睡眠。
        LoongArch64InterruptOps::enable_interrupts();
        if sched::needs_resched(arch_current_cpu_id()) {
            LoongArch64InterruptOps::disable_interrupts();
            return;
        }
        core::arch::asm!("idle 0", options(nomem, nostack, preserves_flags));
        LoongArch64InterruptOps::disable_interrupts();
        // QEMU LoongArch 可能在 IPI 唤醒 idle 后先续执行下一条指令，
        // 而不是立即进入 trap。此时 IPI 仍在 ESTAT 中 pending，必须在
        // 关中断后主动消费，否则 shootdown 确认会永久少一代。
        super::smp::handle_ipi();
    }
}

/// 把 [`LoongArch64TaskOps::set_kernel_trap_stack`] 拉成裸 `unsafe fn` 指针，
/// 对接 [`ArchTrapOps`] 的契约。
///
/// # Safety
/// `stack_top` 必须落在当前正要运行的任务所持有的内核栈上下文中，且按 16
/// 字节 ABI 对齐。调用方（sched::schedule_once）保证这两点。
unsafe fn set_kernel_trap_stack_raw(stack_top: usize) {
    <LoongArch64TaskOps as TaskOps>::set_kernel_trap_stack(stack_top);
}

static REGISTERED: AtomicBool = AtomicBool::new(false);

/// 把本架构的三套 ops 装入 `libs/sched`，并把 mm / syscall 侧的 ops 注入 general。
///
/// 幂等：重复调用仅第一次生效；启动路径（acpi / dtb 两条）都可以无忌调用。
pub fn register() {
    if REGISTERED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        sched::arch_hooks::register(&ARCH_CONTEXT_OPS);
        sched::arch_hooks::register_time(&ARCH_TIME_OPS);
        sched::arch_hooks::register_deadline_timer(&ARCH_DEADLINE_TIMER_OPS);
        sched::arch_hooks::register_trap(&ARCH_TRAP_OPS);
        sched::arch_hooks::register_idle(&ARCH_IDLE_OPS);
        sched::arch_hooks::register_cpu_control(&super::smp::CPU_CONTROL_OPS);
        // UserPgdOps / UserAccessOps / FaultDecodeOps
        super::mm::register();
        // SyscallFrameOps
        super::syscall::register();
    }
}

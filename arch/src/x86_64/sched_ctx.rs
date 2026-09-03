//! x86_64 内核上下文切换与调度器 hook。

use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

use general::TaskOps;
use sched::arch_hooks::{
    ArchContextOps, ArchDeadlineTimerOps, ArchIdleOps, ArchLocalInterruptOps, ArchTimeOps,
    ArchTrapOps, KernelEntry,
};

use super::task::X86_64TaskOps;

/// XSAVE requires a 64-byte aligned base in 64-bit mode.  Keeping every
/// scheduler context at that alignment also preserves the legacy FXSAVE
/// requirement when the CPU falls back to the baseline path.
pub(crate) const KCTX_ALIGN: usize = super::fpu::XSAVE_ALIGNMENT;
/// r15..r12、rbx、rbp、rsp、返回地址共 8 个 64 位槽。
const GP_CONTEXT_SIZE: usize = 8 * 8;
/// Keep the complete bounded XSAVE image out of the GP slots so the scheduler
/// can preserve x87/SSE, AVX, and AVX-512 kernel state without making the
/// generic scheduler know about an ISA-specific layout.  CPUs without XSAVE
/// use only the first 512 bytes and retain the same allocation contract.
pub(crate) const FPU_CONTEXT_OFFSET: usize = GP_CONTEXT_SIZE;
pub(crate) const KCTX_SIZE: usize = FPU_CONTEXT_OFFSET + super::fpu::MAX_XSAVE_SIZE;

const _: () = {
    assert!(FPU_CONTEXT_OFFSET % super::fpu::XSAVE_ALIGNMENT == 0);
    assert!(KCTX_SIZE % KCTX_ALIGN == 0);
};

#[cfg(target_os = "none")]
const RBX_OFFSET: usize = 0x00;
#[cfg(target_os = "none")]
const RBP_OFFSET: usize = 0x08;
const R12_OFFSET: usize = 0x10;
const R13_OFFSET: usize = 0x18;
#[cfg(target_os = "none")]
const R14_OFFSET: usize = 0x20;
#[cfg(target_os = "none")]
const R15_OFFSET: usize = 0x28;
const RSP_OFFSET: usize = 0x30;
const RIP_OFFSET: usize = 0x38;

unsafe fn init_kernel_context(ctx: NonNull<u8>, stack_top: usize, entry: KernelEntry, arg: usize) {
    // Safety: sched 契约保证 ctx 至少 KCTX_SIZE 字节且按 KCTX_ALIGN 对齐。
    unsafe {
        core::ptr::write_bytes(ctx.as_ptr(), 0, KCTX_SIZE);
        let base = ctx.as_ptr();
        (base.add(RIP_OFFSET) as *mut usize)
            .write(__x86_64_kthread_trampoline as *const () as usize);
        (base.add(RSP_OFFSET) as *mut usize).write(stack_top & !0xf);
        (base.add(R12_OFFSET) as *mut usize).write(entry as usize);
        (base.add(R13_OFFSET) as *mut usize).write(arg);

        // A freshly-created context may be restored before it has ever been
        // saved by `switch_context_raw`; seed a valid architectural FXSAVE
        // image instead of relying on zeroed reserved fields.  The standard
        // XSAVE header advertises only the legacy components; all enabled
        // extended components therefore restore their architectural init
        // state until the task first uses them.
        let fx = base.add(FPU_CONTEXT_OFFSET);
        (fx as *mut u16).write(0x037f); // x87 control word
        (fx.add(24) as *mut u32).write(0x1f80); // MXCSR reset value
        (fx.add(28) as *mut u32).write(0x0000_ffbf); // conservative MXCSR mask
        (fx.add(super::fpu::FXSAVE_AREA_SIZE) as *mut u64).write(super::fpu::XFEATURE_BASE); // XSTATE_BV
    }
}

#[cfg(target_os = "none")]
#[unsafe(naked)]
unsafe extern "C" fn switch_context_raw(
    _prev: NonNull<u8>,
    _next: NonNull<u8>,
    _prev_on_cpu: NonNull<core::sync::atomic::AtomicUsize>,
) {
    core::arch::naked_asm!(
        // SysV ABI: rdi=prev, rsi=next, rdx=prev_on_cpu.
        // XSAVE/XRSTOR use EDX:EAX as the feature mask.  Preserve the owner
        // slot pointer in a caller-saved register before issuing XSAVE.
        "mov r8, rdx",
        "mov [rdi + {rbx}], rbx",
        "mov [rdi + {rbp}], rbp",
        "mov [rdi + {r12}], r12",
        "mov [rdi + {r13}], r13",
        "mov [rdi + {r14}], r14",
        "mov [rdi + {r15}], r15",
        "mov [rdi + {rsp}], rsp",
        "lea rax, [rip + 14f]",
        "mov [rdi + {rip}], rax",
        // Save the current kernel's xstate before loading the next context.
        // The runtime flag is set only after CR4.OSXSAVE/XCR0 initialization;
        // CPUs without XSAVE stay on the FXSAVE-compatible branch.
        "cmp byte ptr [rip + {xsave_enabled}], 0",
        "je 10f",
        // XSTATE's EDX:EAX mask must be a subset of the current CPU's XCR0.
        // Asking XSAVE for every architectural bit is #GP when the policy
        // deliberately leaves an implemented component disabled.
        "xor ecx, ecx",
        "xgetbv",
        "xsave64 [rdi + {fpu}]",
        "jmp 11f",
        "10:",
        "fxsave64 [rdi + {fpu}]",
        "11:",
        // x86 stores are release ordered with respect to subsequent owner reads.
        "mov qword ptr [r8], 0",
        "mov rbx, [rsi + {rbx}]",
        "mov rbp, [rsi + {rbp}]",
        "mov r12, [rsi + {r12}]",
        "mov r13, [rsi + {r13}]",
        "mov r14, [rsi + {r14}]",
        "mov r15, [rsi + {r15}]",
        "mov rsp, [rsi + {rsp}]",
        "cmp byte ptr [rip + {xsave_enabled}], 0",
        "je 12f",
        "xor ecx, ecx",
        "xgetbv",
        "xrstor64 [rsi + {fpu}]",
        "jmp 13f",
        "12:",
        "fxrstor64 [rsi + {fpu}]",
        "13:",
        "jmp [rsi + {rip}]",
        "14:",
        // The saved RSP still points at switch_context_raw's caller return
        // address.  Returning here completes the original call after the
        // callee-saved registers have been restored.
        "ret",
        rbx = const RBX_OFFSET,
        rbp = const RBP_OFFSET,
        r12 = const R12_OFFSET,
        r13 = const R13_OFFSET,
        r14 = const R14_OFFSET,
        r15 = const R15_OFFSET,
        rsp = const RSP_OFFSET,
        rip = const RIP_OFFSET,
        fpu = const FPU_CONTEXT_OFFSET,
        xsave_enabled = sym super::fpu::XSAVE_ENABLED,
    );
}

#[cfg(not(target_os = "none"))]
unsafe extern "C" fn switch_context_raw(
    _prev: NonNull<u8>,
    _next: NonNull<u8>,
    _prev_on_cpu: NonNull<core::sync::atomic::AtomicUsize>,
) {
    panic!("x86_64 context switch is unavailable on a hosted target");
}

#[cfg(target_os = "none")]
#[unsafe(naked)]
unsafe extern "C" fn __x86_64_kthread_trampoline() -> ! {
    // The restored stack is 16-byte aligned. `call` supplies the synthetic
    // return address required by the SysV ABI, so the entry observes RSP%16=8.
    // Kernel entries are `-> !`; UD2 catches an accidental return.
    core::arch::naked_asm!("cld", "mov rdi, r13", "call r12", "ud2");
}

#[cfg(not(target_os = "none"))]
unsafe extern "C" fn __x86_64_kthread_trampoline() -> ! {
    panic!("x86_64 kthread trampoline is unavailable on a hosted target");
}

#[cfg(target_os = "none")]
fn save_and_disable_local_interrupts() -> usize {
    super::interrupt::save_and_disable()
}

#[cfg(not(target_os = "none"))]
fn save_and_disable_local_interrupts() -> usize {
    0
}

#[cfg(target_os = "none")]
fn restore_local_interrupts(state: usize) {
    super::interrupt::restore(state)
}

#[cfg(not(target_os = "none"))]
fn restore_local_interrupts(_state: usize) {}

unsafe fn set_kernel_trap_stack_raw(stack_top: usize) {
    <X86_64TaskOps as TaskOps>::set_kernel_trap_stack(stack_top);
}

unsafe fn set_current_task_raw(task_ptr: usize, cpu_work_ptr: usize) {
    unsafe { super::specific::set_current_task_ptr_with_work(task_ptr, cpu_work_ptr) };
}

fn current_task_ptr_raw() -> usize {
    super::specific::current_task_ptr()
}

fn now_ns() -> u64 {
    super::stable_counter_to_ns(super::stable_counter_raw())
}

fn current_cpu_id() -> usize {
    super::smp::current_cpu_id()
}

#[cfg(target_os = "none")]
#[inline]
fn bootstrap_stack_pointer() -> usize {
    let rsp: usize;
    // Read only the active bootstrap stack.  Interrupts remain disabled while
    // descriptor and IDT state are replaced; task publication later installs
    // the real per-task rsp0 through `set_kernel_trap_stack`.
    unsafe {
        core::arch::asm!(
            "mov {rsp}, rsp",
            rsp = out(reg) rsp,
            options(nostack, nomem, preserves_flags)
        );
    }
    rsp
}

#[cfg(not(target_os = "none"))]
#[inline]
fn bootstrap_stack_pointer() -> usize {
    0
}

#[inline]
fn reprogram_deadline(deadline_ns: Option<u64>) {
    // The scheduler publishes an absolute nanosecond deadline.  Keep the
    // conversion and hardware mode selection inside x86::time so this glue
    // remains a pure architecture-hook adapter.
    super::time::rearm_local_timer(deadline_ns);
}

fn idle_relax() {
    #[cfg(target_os = "none")]
    unsafe {
        // Match Linux's native_pause(): retain the compiler memory clobber so
        // polling loads/stores cannot be hoisted out of the wait iteration.
        core::arch::asm!("pause", options(nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    core::hint::spin_loop();
}

static ARCH_CONTEXT_OPS: ArchContextOps = ArchContextOps {
    context_size: KCTX_SIZE,
    context_align: KCTX_ALIGN,
    init_kernel_context,
    switch_context: switch_context_raw,
};

static ARCH_LOCAL_INTERRUPT_OPS: ArchLocalInterruptOps = ArchLocalInterruptOps {
    save_and_disable: save_and_disable_local_interrupts,
    restore: restore_local_interrupts,
};

static ARCH_TIME_OPS: ArchTimeOps = ArchTimeOps {
    now_ns,
    current_cpu_id,
};

static ARCH_DEADLINE_TIMER_OPS: ArchDeadlineTimerOps = ArchDeadlineTimerOps {
    reprogram: reprogram_deadline,
};

static ARCH_TRAP_OPS: ArchTrapOps = ArchTrapOps {
    set_kernel_trap_stack: set_kernel_trap_stack_raw,
    set_current_task: set_current_task_raw,
    current_task_ptr: current_task_ptr_raw,
};

static ARCH_IDLE_OPS: ArchIdleOps = ArchIdleOps { idle_relax };
static REGISTERED: AtomicBool = AtomicBool::new(false);

/// 将 x86_64 的调度/陷阱/syscall hook 注入通用层。
pub fn register() {
    if REGISTERED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        super::smp::initialize_boot_cpu();
        super::fpu::init();
        super::fpu::configure_best_user_policy()
            .expect("x86 BSP must select a bounded xstate policy before user tasks");
        super::time::init();
        super::mm::register();
        let stack_top = super::task::current_kernel_stack_top();
        let stack_top = if stack_top == 0 {
            bootstrap_stack_pointer()
        } else {
            stack_top
        };
        // Install the complete flat GDT, deny-all I/O bitmap TSS, and (on the
        // bare target) reload CS/SS/TR before any IDT gate or SYSCALL MSR can
        // reference the Linux-compatible selectors.
        let irq_state = save_and_disable_local_interrupts();
        unsafe { super::descriptor::initialize_current_cpu(stack_top) };
        restore_local_interrupts(irq_state);
        // Hosted builds return an explicit `Hosted` error; bare builds install
        // the IDT/MSR contract once the scheduler glue is registered.
        let _ = unsafe { super::trap::install_exception_entry() };
        sched::arch_hooks::register(&ARCH_CONTEXT_OPS);
        sched::arch_hooks::register_local_interrupt(&ARCH_LOCAL_INTERRUPT_OPS);
        sched::arch_hooks::register_time(&ARCH_TIME_OPS);
        sched::arch_hooks::register_deadline_timer(&ARCH_DEADLINE_TIMER_OPS);
        sched::arch_hooks::register_trap(&ARCH_TRAP_OPS);
        sched::arch_hooks::register_idle(&ARCH_IDLE_OPS);
        sched::arch_hooks::register_cpu_control(&super::smp::CPU_CONTROL_OPS);
        // ACPI may have published the LAPIC before scheduler registration.  At
        // this point the IDT and deadline callback are both visible, so retry
        // the local timer setup and arm its bounded regular tick without a
        // window in which an interrupt could lose the reprogram callback.
        super::time::initialize_local_timer();
        super::syscall::register();
    }
}

/// 对外使用的本地中断保存/恢复入口。
pub fn save_and_disable() -> usize {
    save_and_disable_local_interrupts()
}

pub fn restore_interrupts(state: usize) {
    restore_local_interrupts(state)
}

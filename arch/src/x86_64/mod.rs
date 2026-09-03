//! x86_64 架构支持总入口。
//!
//! 本模块集中导出 x86_64 的 ABI、陷阱帧、调度器胶水和浮点扩展状态。具体硬件
//! 语义（CPUID、XCR0、FXSAVE/XSAVE、段寄存器）仍留在本目录；上层只通过
//! `general`/`hal` 的不透明接口访问它们。

pub mod abi;
pub mod apic;
#[cfg(target_os = "none")]
pub mod boot;
pub mod boot_protocol;
pub mod descriptor;
pub mod early_console;
pub mod efi_stub;
pub mod fpu;
pub mod interrupt;
pub mod io;
pub mod loader;
pub mod mm;
pub mod paging;
mod ptrace;
pub mod random_source;
pub mod sched_ctx;
pub mod smp;
mod specific;
pub mod syscall;
mod task;
pub mod time;
pub mod trap;
pub mod trap_frame;
pub mod vdso;
pub mod xstate;

pub use ptrace::{
    BREAKPOINT_INSN as USER_BREAKPOINT_INSN, LINUX_FPREGSET_SIZE, LINUX_SIGNAL_XSTATE_MAX_SIZE,
    encode_linux_signal_xstate as encode_user_linux_signal_xstate,
    linux_signal_xstate_encoded_size as user_linux_signal_xstate_size, linux_xstate_size,
    read_linux_fpregs as read_user_linux_fpregs, read_linux_xstate as read_user_linux_xstate,
    restore_linux_signal_xstate as restore_user_linux_signal_xstate,
    store_task_frame as store_x86_ptrace_task_frame, task_frame as ptrace_task_frame,
    write_linux_fpregs as write_user_linux_fpregs, write_linux_xstate as write_user_linux_xstate,
};
pub use sched_ctx::register as register_sched_ctx;
pub use specific::{
    current_cpu_id, current_task_ptr, device_io_barrier, dma_clean_range, dma_invalidate_range,
    phys_to_virt, virt_to_phys,
};
pub use task::X86_64TaskOps;
pub use time::{
    CalibrationSource, LocalTimerBackend, calibration_source, local_timer_backend, set_frequency,
    set_lapic_timer_frequency, stable_counter_raw_ordered,
};
pub use trap_frame::TrapFrame;
pub use xstate::{TASKEXT_X86_XSTATE, TASKEXT_X86_XSTATE_SIGNAL_STACK, UserXState};

pub use fpu::{
    CpuFeatures, XStatePolicy, XStatePolicyError, configure_best_user_policy,
    configure_user_policy, supported_user_mask,
};

/// x86 固件应通过 ACPI `_CRS` 描述 PCI 窗口，不提供猜测范围。
pub const fn default_pci_mmio_window() -> Option<core::ops::Range<u64>> {
    None
}

/// x86 CPU 缓存与常规 PCI DMA 保持硬件一致性。
pub const fn acpi_pci_dma_coherent_default() -> bool {
    true
}

/// IOMMU 启用前，x86 PCI 主机使用 CPU 物理地址作为 DMA 地址。
pub const fn acpi_pci_identity_dma_default() -> bool {
    true
}

/// 根据 CPUID 和实际 XCR0 策略发布用户可见能力。
pub fn user_hwcap() -> usize {
    specific::user_hwcap()
}

/// 早期控制台输出；正式 UART 驱动接管前使用 COM1 16550A 轮询路径。
pub fn e_write_bytes(bytes: &[u8]) {
    early_console::write_bytes(bytes);
}

pub fn raw_debug_byte(byte: u8) {
    early_console::write_byte(byte);
}

pub fn raw_debug_hex16(value: usize) {
    early_console::write_hex16(value);
}

/// x86_64 的数据缓存与指令缓存保持硬件一致性。
pub fn sync_icache() {
    <X86_64TaskOps as general::TaskOps>::sync_icache();
}

/// fork 时处理 x86 拥有的可变用户扩展状态。
pub fn clone_user_task_extension(
    key: sched::TaskExtKey,
    src: &alloc::sync::Arc<dyn core::any::Any + Send + Sync>,
) -> Option<alloc::sync::Arc<dyn core::any::Any + Send + Sync>> {
    if key == ptrace::TASKEXT_X86_PTRACE_FRAME_DIRTY {
        // A child never inherits an in-flight parent ptrace writeback.
        return Some(
            alloc::sync::Arc::new(false) as alloc::sync::Arc<dyn core::any::Any + Send + Sync>
        );
    }
    if key == sched::TASKEXT_PTRACE_FRAME {
        // A ptrace stop snapshot is mutable through ptrace writes; never share
        // the parent's Arc with a forked child, otherwise a debugger write would
        // race and alter both tasks' register views.
        return src.downcast_ref::<TrapFrame>().map(|frame| {
            alloc::sync::Arc::new(*frame) as alloc::sync::Arc<dyn core::any::Any + Send + Sync>
        });
    }
    xstate::clone_task_extension(key, src)
}

pub fn reset_user_task_state(task: &sched::Task) {
    // ptrace snapshots are execution-state, not an exec-inherited attribute.
    let _ = task.ext_remove(sched::TASKEXT_PTRACE_FRAME);
    let _ = task.ext_remove(ptrace::TASKEXT_X86_PTRACE_FRAME_DIRTY);
    xstate::clear_for_task(task);
}

pub fn push_user_signal_state(
    task: &alloc::sync::Arc<sched::Task>,
    context: usize,
) -> Result<(), ()> {
    xstate::push_signal_snapshot(task, context)
}

pub fn pop_user_signal_state(task: &alloc::sync::Arc<sched::Task>, context: usize) {
    xstate::pop_signal_snapshot(task, context);
}

/// Trap entry hook for an explicitly enabled extended xstate policy.
///
/// The entry assembly must call this before using any instruction that can
/// modify vector registers.  It is a no-op for the default FXSAVE-only policy.
pub fn save_user_xstate_from_trap(context: usize) -> Result<(), ()> {
    let Some(task) = sched::try_current_task_ref() else {
        return Err(());
    };
    xstate::save_from_trap_entry(task, context)
}

/// 保存并关闭当前 CPU 的可屏蔽中断。
pub fn save_and_disable_local_interrupts() -> usize {
    sched_ctx::save_and_disable()
}

/// 恢复此前保存的本地中断状态。
pub fn restore_local_interrupts(state: usize) {
    sched_ctx::restore_interrupts(state);
}

#[inline]
pub fn stable_counter_raw() -> u64 {
    time::stable_counter_raw()
}

#[inline]
pub fn stable_counter_hz() -> u64 {
    time::stable_counter_hz()
}

#[inline]
pub fn stable_counter_to_ns(counter: u64) -> u64 {
    time::stable_counter_to_ns(counter)
}

#[inline]
pub fn kernel_timestamp_ns() -> u64 {
    time::kernel_timestamp_ns()
}

pub use smp::{SecondaryCpuReport, start_secondary_cpus};

pub fn register_entropy_source() {
    random_source::register();
}

pub fn activate_kernel_page_table() {
    // The user-PGD backend owns the active-CPU bookkeeping and therefore must
    // be used even when the caller has no current user task.
    unsafe { mm::activate_kernel_for_arch() };
}

/// 用户线程入口意外返回时使用的最小 `exit(0)` 指令序列。
pub const fn user_exit_stub_code() -> &'static [u8] {
    // mov eax, __NR_exit; xor edi, edi; syscall
    &[0xb8, 60, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05]
}

pub const fn linux_epoll_event_data_offset() -> usize {
    8
}

/// x86_64 动态链接器不需要架构兼容补丁。
pub fn patch_interpreter_image(_interp: &str, _bytes: &mut [u8]) {}

/// 设置 TSS.rsp0 的后端钩子。启动链路完成 GDT/TSS 后可替换此函数体。
#[inline]
#[cfg(target_os = "none")]
pub(crate) fn set_tss_rsp0(stack_top: usize) {
    descriptor::set_kernel_stack(stack_top);
}

/// 在进入用户态前发布 FS/GS 基址。
///
/// Linux 风格的 `swapgs` 入口把用户 GS 保存在 `MSR_KERNEL_GS_BASE`，因此
/// 这里只写该镜像，实际 GS 切换仍由 trap/syscall 入口桩负责。FS_BASE 可直接
/// 写入；两者均只在 CPL3 返回帧上更新，避免覆盖内核的 per-CPU GS。
#[cfg(target_os = "none")]
pub(crate) unsafe fn restore_user_segment_bases(frame: *const trap_frame::TrapFrame) {
    // Safety: caller owns a valid trap frame for the pending return.
    let (cs, fs, gs) = unsafe { ((*frame).cs, (*frame).fs_base, (*frame).gs_base) };
    const USER_SPACE_TOP: usize = 0x0000_8000_0000_0000;
    if cs != descriptor::USER_CS as usize
        || fs >= USER_SPACE_TOP
        || gs >= USER_SPACE_TOP
        || !paging::is_canonical(fs as u64, false)
        || !paging::is_canonical(gs as u64, false)
    {
        return;
    }
    unsafe {
        write_msr(0xc000_0100, fs); // MSR_FS_BASE
        write_msr(0xc000_0102, gs); // MSR_KERNEL_GS_BASE
    }
}

#[cfg(target_os = "none")]
#[inline]
pub(crate) unsafe fn write_msr(msr: u32, value: usize) {
    // Safety: the caller runs at CPL0 and supplies an architecturally valid
    // model-specific register.  FS_BASE/KERNEL_GS_BASE are valid in long mode.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            // WRMSR changes the architectural execution context.  Keep the
            // implicit memory clobber so compiler memory operations cannot be
            // moved across publication of FS/KERNEL_GS_BASE.
            options(nostack)
        );
    }
}

#[cfg(target_os = "none")]
#[inline]
pub(crate) unsafe fn read_msr(msr: u32) -> usize {
    let low: u32;
    let high: u32;
    // RDMSR is a privileged read; retain the memory clobber so ordinary
    // accesses cannot be moved across an architectural state read.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nostack)
        );
    }
    ((high as usize) << 32) | low as usize
}

/// 裸机返回陷阱帧的 iretq 桩。
#[cfg(target_os = "none")]
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn resume_to_trap_frame_raw(_frame: usize) -> ! {
    core::arch::naked_asm!(
        "mov r11, rdi",
        // A same-CPL return consumes only RIP/CS/RFLAGS.  A user return also
        // consumes RSP/SS; mark the path on the temporary kernel stack so the
        // common register restore can finish with `ret` or `iretq`.
        "test byte ptr [r11 + {cs}], 3",
        "jz 2f",
        "push qword ptr [r11 + {ss}]",
        "push qword ptr [r11 + {rsp}]",
        "push qword ptr [r11 + {rflags}]",
        "push qword ptr [r11 + {cs}]",
        "push qword ptr [r11 + {rip}]",
        "push 1",
        "jmp 3f",
        "2:",
        // For a CPL0 return, make the requested frame stack the post-iret
        // stack.  The synthetic RIP is popped by `ret` after common restore.
        "mov r10, [r11 + {rsp}]",
        "mov rsp, r10",
        "push qword ptr [r11 + {rip}]",
        "push qword ptr [r11 + {rflags}]",
        "popfq",
        "push 0",
        "3:",
        // The task-return hook restores any owned AVX-family image before this
        // final legacy restore.  The frame itself intentionally contains only
        // the fixed FXSAVE prefix, so this path remains ABI-stable.
        "fxrstor64 [r11 + {fxsave}]",
        "mov r15, [r11 + {r15}]",
        "mov r14, [r11 + {r14}]",
        "mov r13, [r11 + {r13}]",
        "mov r12, [r11 + {r12}]",
        "mov r10, [r11 + {r10}]",
        "mov r9,  [r11 + {r9}]",
        "mov r8,  [r11 + {r8}]",
        "mov rdi, [r11 + {rdi}]",
        "mov rsi, [r11 + {rsi}]",
        "mov rbp, [r11 + {rbp}]",
        "mov rbx, [r11 + {rbx}]",
        "mov rdx, [r11 + {rdx}]",
        "mov rcx, [r11 + {rcx}]",
        "mov rax, [r11 + {rax}]",
        "mov r11, [r11 + {r11}]",
        "cmp qword ptr [rsp], 1",
        "jne 4f",
        // The scheduler resumes from the kernel GS context.  A user return
        // must exchange it back with the task's saved user image before
        // `iretq`; otherwise `%gs` would remain the per-CPU kernel base.
        "swapgs",
        "add rsp, 8",
        "iretq",
        "4:",
        "add rsp, 8",
        "ret",
        ss = const trap_frame::SS_OFFSET,
        rsp = const trap_frame::RSP_OFFSET,
        rflags = const trap_frame::RFLAGS_OFFSET,
        cs = const trap_frame::CS_OFFSET,
        rip = const trap_frame::RIP_OFFSET,
        fxsave = const trap_frame::FXSAVE_OFFSET,
        r15 = const trap_frame::R15_OFFSET,
        r14 = const trap_frame::R14_OFFSET,
        r13 = const trap_frame::R13_OFFSET,
        r12 = const trap_frame::R12_OFFSET,
        r10 = const trap_frame::R10_OFFSET,
        r9 = const trap_frame::R9_OFFSET,
        r8 = const trap_frame::R8_OFFSET,
        rdi = const trap_frame::RDI_OFFSET,
        rsi = const trap_frame::RSI_OFFSET,
        rbp = const trap_frame::RBP_OFFSET,
        rbx = const trap_frame::RBX_OFFSET,
        rdx = const trap_frame::RDX_OFFSET,
        rcx = const trap_frame::RCX_OFFSET,
        rax = const trap_frame::RAX_OFFSET,
        r11 = const trap_frame::R11_OFFSET,
    );
}

pub(crate) unsafe extern "C" fn user_entry() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

pub(crate) unsafe extern "C" fn demo_user_entry() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

pub(crate) unsafe extern "C" fn idle_task_entry() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// ELM 原生调用门的 hosted 实现；裸机启动链路可替换为独立栈切换汇编。
pub unsafe fn call_elm_native(entry: usize, context: *mut u8, _stack_top: usize) -> i32 {
    if entry == 0 {
        return -1;
    }
    let function: unsafe extern "C" fn(*mut u8) -> i32 = unsafe { core::mem::transmute(entry) };
    unsafe { function(context) }
}

pub unsafe fn call_elm_native_current_stack(entry: usize, context: *mut u8) -> i32 {
    unsafe { call_elm_native(entry, context, 0) }
}

pub fn elm_native_recovery_address() -> usize {
    elm_native_recovery as *const () as usize
}

extern "C" fn elm_native_recovery() {}

pub unsafe fn resume_elm_panic(_return_pc: usize, _return_sp: usize, _return_value: usize) -> ! {
    panic!("x86_64 ELM panic recovery is unavailable before the trap backend");
}

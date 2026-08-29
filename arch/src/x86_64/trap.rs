//! x86_64 异常、中断和 `SYSCALL` 入口。
//!
//! The entry code in this module is deliberately small and explicit.  IDT
//! stubs put a vector marker on the stack, the common assembler builds the
//! stable [`TrapFrame`] layout, and the Rust handler delegates only to generic
//! dispatchers that already have an architecture-neutral contract.  A path
//! which cannot prove that it owns a valid frame returns zero and the assembly
//! path halts with interrupts disabled; it never resumes an unrecognised frame.

use core::mem::offset_of;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use general::TrapFramePtr;

use super::descriptor;
use super::fpu;
use super::interrupt;
use super::paging;
use super::ptrace;
use super::task;
use super::trap_frame::{self, TrapFrame};

/// Synthetic vector used by the one external-interrupt gate.  The actual LAPIC
/// vector is read from the in-service bitmap before dispatching.
pub const EXTERNAL_VECTOR_MARKER: usize = 0x100;
/// Synthetic vector used by the `SYSCALL` entry.  It cannot collide with an IDT
/// vector (which is limited to one byte).
pub const SYSCALL_VECTOR_MARKER: usize = 0x101;
/// Conventional LAPIC timer vector used by the x86 scheduler hook.
pub const TIMER_VECTOR: u8 = super::apic::TIMER_VECTOR;
/// Reserved fixed IPI vector.  Other vectors are dispatched through the IRQ
/// domain after their LAPIC in-service bit has been resolved.
pub const IPI_VECTOR: u8 = super::apic::IPI_VECTOR;
pub const RESCHEDULE_VECTOR: u8 = super::apic::RESCHEDULE_VECTOR;
pub const ERROR_VECTOR: u8 = super::apic::ERROR_VECTOR;
pub const SPURIOUS_VECTOR: u8 = super::apic::SPURIOUS_VECTOR;

const MSR_EFER: u32 = 0xc000_0080;
const MSR_STAR: u32 = 0xc000_0081;
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_FMASK: u32 = 0xc000_0084;
const MSR_FS_BASE: u32 = 0xc000_0100;
const MSR_GS_BASE: u32 = 0xc000_0101;
const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;
const EFER_SCE: usize = 1;
const EFER_NXE: usize = 1 << 11;
const RFLAGS_TRAP: usize = 1 << 8;
const RFLAGS_DIRECTION: usize = 1 << 10;
const RFLAGS_INTERRUPT_ENABLE: usize = 1 << 9;
const RFLAGS_ALIGNMENT_CHECK: usize = 1 << 18;
const RFLAGS_IOPL: usize = 0b11 << 12;
const RFLAGS_NESTED_TASK: usize = 1 << 14;

const GP_SAVE_SIZE: usize = 15 * core::mem::size_of::<usize>();
// Keep the copied GPR area rounded up so the frame starts at a stable
// 16-byte boundary.  The final eight bytes are padding, not a register.
const GP_SAVE_STORAGE_SIZE: usize = 128;
const RETURN_AREA_SIZE: usize = 48;
const FRAME_OFFSET: usize = RETURN_AREA_SIZE + GP_SAVE_STORAGE_SIZE;
const COMMON_ENTRY_RESERVE: usize = FRAME_OFFSET + trap_frame::FRAME_SIZE;

const _: () = {
    assert!(GP_SAVE_STORAGE_SIZE >= GP_SAVE_SIZE);
    assert!(GP_SAVE_STORAGE_SIZE % 16 == 0);
    assert!(FRAME_OFFSET % 16 == 0);
    assert!(COMMON_ENTRY_RESERVE % 16 == 0);
    assert!((FRAME_OFFSET + trap_frame::FXSAVE_OFFSET) % 16 == 0);
};

/// Per-CPU state needed before a SYSCALL entry can use a Rust stack.
///
/// Only BSP startup is currently exposed by the x86 scheduler, but keeping the
/// state in a cache-line-aligned object makes the assembly contract extensible
/// to a real per-CPU array without changing its offsets.
#[repr(C, align(64))]
struct EntryState {
    kernel_stack_top: AtomicUsize,
    user_rsp: AtomicUsize,
}

static ENTRY_STATE: EntryState = EntryState {
    kernel_stack_top: AtomicUsize::new(0),
    user_rsp: AtomicUsize::new(0),
};

const ENTRY_KERNEL_STACK_OFFSET: usize = offset_of!(EntryState, kernel_stack_top);
const ENTRY_USER_RSP_OFFSET: usize = offset_of!(EntryState, user_rsp);

#[cfg(target_os = "none")]
#[used]
static mut IDT: interrupt::Idt = interrupt::Idt::new();

static INSTALLED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapInstallError {
    /// A second installation attempted to replace a live IDT/MSR contract.
    AlreadyInstalled,
    /// The host build has no privileged IDT/MSR state.  Host tests use the
    /// software installation bit and never claim hardware execution.
    Hosted,
}

#[cfg(target_os = "none")]
type EntryFn = unsafe extern "C" fn() -> !;

/// Update the stack selected by the SYSCALL entry.  The scheduler invokes the
/// generic trap-stack hook whenever it publishes a new current task.
pub fn set_kernel_stack_top(stack_top: usize) {
    super::smp::set_kernel_stack_top(stack_top);
    ENTRY_STATE
        .kernel_stack_top
        .store(stack_top, Ordering::Release);
}

/// Return whether the x86 trap/MSR contract has been installed.
#[inline]
pub fn is_installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

/// Install all IDT gates and the long-mode syscall MSRs for the current CPU.
///
/// On a hosted target this only records the software state and returns
/// [`TrapInstallError::Hosted`]; callers must not interpret that as hardware
/// readiness.  The bare target performs the complete privileged sequence.
pub unsafe fn install_exception_entry() -> Result<(), TrapInstallError> {
    if INSTALLED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        #[cfg(target_os = "none")]
        unsafe {
            // IDTR and syscall MSRs are per-CPU even when the IDT image is
            // shared by all processors.
            install_bare_idt_and_syscall_msrs()?;
        }
        return Err(TrapInstallError::AlreadyInstalled);
    }

    #[cfg(target_os = "none")]
    {
        let irq_state = interrupt::save_and_disable();
        let result = unsafe { install_bare_idt_and_syscall_msrs() };
        interrupt::restore(irq_state);
        if result.is_err() {
            INSTALLED.store(false, Ordering::Release);
        }
        result
    }
    #[cfg(not(target_os = "none"))]
    {
        INSTALLED.store(false, Ordering::Release);
        Err(TrapInstallError::Hosted)
    }
}

#[cfg(target_os = "none")]
unsafe fn install_bare_idt_and_syscall_msrs() -> Result<(), TrapInstallError> {
    // The boot GDT/TSS owner is responsible for making descriptor::KERNEL_CS a
    // valid long-mode code selector before this hook is called.
    let idt = core::ptr::addr_of_mut!(IDT);
    for vector in 0..interrupt::IDT_ENTRIES {
        let handler = if vector < 32 {
            EXCEPTION_STUBS[vector]
        } else if vector == SPURIOUS_VECTOR as usize {
            // LAPIC spurious interrupts do not set an ISR bit.  They need a
            // real vector marker; resolving the shared external marker from
            // the ISR bitmap would otherwise fail closed and halt the CPU.
            __x86_spurious_stub
        } else {
            __x86_external_stub
        };
        let attributes = interrupt::PRESENT | interrupt::INTERRUPT_GATE;
        let ist = match vector as u8 {
            interrupt::DOUBLE_FAULT => descriptor::DOUBLE_FAULT_IST,
            interrupt::NMI => descriptor::NMI_IST,
            interrupt::MACHINE_CHECK => descriptor::MACHINE_CHECK_IST,
            _ => 0,
        };
        unsafe {
            (*idt).set_handler(
                vector as u8,
                handler as usize,
                descriptor::KERNEL_CS,
                attributes,
                ist,
            );
        }
    }
    // int3 is intentionally callable from CPL3, matching Linux's user debug
    // ABI.  Other exception gates retain DPL0.
    unsafe {
        (*idt).set_handler(
            interrupt::BREAKPOINT,
            EXCEPTION_STUBS[interrupt::BREAKPOINT as usize] as usize,
            descriptor::KERNEL_CS,
            interrupt::PRESENT | interrupt::TRAP_GATE | (3 << 5),
            0,
        );
        (*idt).load();
    }

    // Establish a kernel GS anchor for the entry stub.  User GS is kept in the
    // architectural KERNEL_GS_BASE slot and is exchanged by SWAPGS on entry/
    // return.  The initial user value is zero until a task publishes TLS.
    unsafe {
        super::write_msr(MSR_GS_BASE, super::smp::current_local_address());
        super::write_msr(MSR_KERNEL_GS_BASE, 0);
        install_syscall_msrs();
    }
    Ok(())
}

#[cfg(target_os = "none")]
unsafe fn install_syscall_msrs() {
    // NXE is part of the paging contract: user and heap mappings use the NX
    // PTE bit.  The Multiboot/AP entry paths validate support before reaching
    // this point; retain it while enabling SYSCALL on every CPU.
    let efer = unsafe { super::read_msr(MSR_EFER) } | EFER_SCE | EFER_NXE;
    unsafe { super::write_msr(MSR_EFER, efer) };

    // STAR uses the selector bases expected by SYSRET: the user base is the
    // user code selector minus 16.  The actual GDT is installed by the boot
    // path and must contain these selectors before user mode is entered.
    let star = ((u64::from(descriptor::KERNEL_CS)) << 32)
        | ((u64::from(descriptor::USER_CS.saturating_sub(16))) << 48);
    unsafe {
        super::write_msr(MSR_STAR, star as usize);
        super::write_msr(MSR_LSTAR, __x86_syscall_entry as *const () as usize);
        super::write_msr(
            MSR_FMASK,
            RFLAGS_TRAP
                | RFLAGS_DIRECTION
                | RFLAGS_INTERRUPT_ENABLE
                | RFLAGS_IOPL
                | RFLAGS_ALIGNMENT_CHECK
                | RFLAGS_NESTED_TASK,
        );
    }
}

// Error-code exceptions defined by Intel SDM.  The vector stub itself pushes
// only the vector; the CPU-provided error word is already below it.
const fn has_hardware_error(vector: usize) -> bool {
    matches!(vector, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21 | 29 | 30)
}

#[cfg(target_os = "none")]
macro_rules! exception_stub_table {
    ($($vector:literal),+ $(,)?) => {
        [$(exception_stub::<$vector>),+]
    };
}

#[cfg(target_os = "none")]
#[used]
static EXCEPTION_STUBS: [EntryFn; 32] = exception_stub_table!(
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 30, 31,
);

#[cfg(target_os = "none")]
#[unsafe(naked)]
#[unsafe(link_section = ".text.trap.entry")]
unsafe extern "C" fn exception_stub<const VECTOR: u8>() -> ! {
    core::arch::naked_asm!(
        "push {vector}",
        "jmp {common}",
        vector = const VECTOR,
        common = sym __x86_common_entry,
    );
}

#[cfg(target_os = "none")]
#[unsafe(naked)]
#[unsafe(link_section = ".text.trap.entry")]
unsafe extern "C" fn __x86_external_stub() -> ! {
    core::arch::naked_asm!(
        "push {vector}",
        "jmp {common}",
        vector = const EXTERNAL_VECTOR_MARKER,
        common = sym __x86_common_entry,
    );
}

#[cfg(target_os = "none")]
#[unsafe(naked)]
#[unsafe(link_section = ".text.trap.entry")]
unsafe extern "C" fn __x86_spurious_stub() -> ! {
    core::arch::naked_asm!(
        "push {vector}",
        "jmp {common}",
        vector = const SPURIOUS_VECTOR,
        common = sym __x86_common_entry,
    );
}

/// Common entry for IDT and SYSCALL paths.
///
/// At entry the stack is `[vector, rip, cs, rflags, rsp, ss]` for a synthetic
/// or no-error event, and `[vector, error, rip, cs, rflags, rsp, ss]` for an
/// error-code event.  The SYSCALL stub builds the same synthetic shape before
/// jumping here.  Fifteen GPR pushes are copied into the fixed TrapFrame, so
/// all scratch registers used by this routine are restored before iretq.
#[cfg(target_os = "none")]
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.trap.entry")]
unsafe extern "C" fn __x86_common_entry() -> ! {
    core::arch::naked_asm!(
        // Preserve every GPR before using scratch registers.  The resulting
        // memory order is r15..rax, exactly the TrapFrame prefix.
        "push rax",
        "push rcx",
        "push rdx",
        "push rbx",
        "push rbp",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov r10, rsp",
        // r10 is caller-saved under SysV; keep the raw stack pointer in r13
        // while the early xstate hook runs.
        "mov r13, r10",
        // A user IDT entry has not executed SWAPGS yet.  A nested NMI can also
        // interrupt the short transition window after CPL changed but before
        // the outer entry exchanged GS.  Inspect the architectural GS base,
        // not CS, and normalize it before any Rust/per-CPU access.  SYSCALL has
        // already swapped GS and therefore takes the ready branch.
        "mov ecx, {msr_gs_base}",
        "rdmsr",
        "shl rdx, 32",
        "or rax, rdx",
        "lea rcx, [rip + {cpu_locals}]",
        "cmp rax, rcx",
        "jb 20f",
        "lea rdx, [rcx + {cpu_locals_size}]",
        "cmp rax, rdx",
        "jae 20f",
        "sub rax, rcx",
        "test rax, {cpu_local_mask}",
        "jz 21f",
        "20:",
        "swapgs",
        // Fail closed if KERNEL_GS_BASE did not contain a published CpuLocal.
        "mov ecx, {msr_gs_base}",
        "rdmsr",
        "shl rdx, 32",
        "or rax, rdx",
        "lea rcx, [rip + {cpu_locals}]",
        "cmp rax, rcx",
        "jb 99f",
        "lea rdx, [rcx + {cpu_locals_size}]",
        "cmp rax, rdx",
        "jae 99f",
        "sub rax, rcx",
        "test rax, {cpu_local_mask}",
        "jnz 99f",
        "21:",
        // Align the TrapFrame for FXSAVE even if firmware delivered an
        // unexpectedly 8-byte-aligned interrupt stack.
        "and rsp, -16",
        "sub rsp, {reserve}",
        "lea r12, [rsp + {frame_offset}]",
        // Copy through a private scratch area.  The pushed GPR words and the
        // destination frame are separated from the raw source by the full
        // reserved area.  The source may begin at either A or A+8 after the
        // alignment step, so the scratch must end before A.
        "lea rdi, [rsp + {return_area}]",
        "mov rsi, r10",
        "mov ecx, 15",
        "cld",
        "rep movsq",
        "lea rsi, [rsp + {return_area}]",
        "mov rdi, r12",
        "mov ecx, 15",
        "rep movsq",
        // Determine the interrupted privilege level from the raw hardware
        // frame.  Error-code exceptions have one extra word before RIP/CS.
        "mov r11, [r13 + {gp_size}]",
        "xor r9d, r9d",
        "cmp r11, 8",
        "je 22f",
        "cmp r11, 10",
        "je 22f",
        "cmp r11, 11",
        "je 22f",
        "cmp r11, 12",
        "je 22f",
        "cmp r11, 13",
        "je 22f",
        "cmp r11, 14",
        "je 22f",
        "cmp r11, 17",
        "je 22f",
        "cmp r11, 21",
        "je 22f",
        "cmp r11, 29",
        "je 22f",
        "cmp r11, 30",
        "jne 23f",
        "22:",
        "mov r9d, 8",
        "23:",
        "xor edi, edi",
        "test byte ptr [r13 + {gp_size} + 16 + r9], 3",
        "setnz dil",
        // If an extended policy is active, capture XCR0 before any dispatcher
        // call can use vector registers.  RAX is an ownership token: kernel
        // entries do not disturb an outer user snapshot held across an NMI.
        "call {capture_early}",
        "test rax, rax",
        "jz 99f",
        "mov r14, rax",
        "mov r10, r13",

        // Keep the original push-stack pointer in r10.  Alignment may have
        // moved rsp down by eight bytes, so deriving this address from the
        // aligned frame would be incorrect.
        // The vector follows the saved GPR prefix.  Error-code exceptions have
        // one additional word between vector and RIP.
        "mov r11, [r10 + {gp_size}]",
        "xor r9d, r9d",
        "cmp r11, 8",
        "je 1f",
        "cmp r11, 10",
        "je 1f",
        "cmp r11, 11",
        "je 1f",
        "cmp r11, 12",
        "je 1f",
        "cmp r11, 13",
        "je 1f",
        "cmp r11, 14",
        "je 1f",
        "cmp r11, 17",
        "je 1f",
        "cmp r11, 21",
        "je 1f",
        "cmp r11, 29",
        "je 1f",
        "cmp r11, 30",
        "je 1f",
        "jmp 2f",
        "1:",
        "mov r9d, 8",
        "2:",
        "mov [r12 + {vector}], r11",
        // These software-owned fields are outside the copied GPR prefix.
        // Clear them before Rust observes the frame so stale stack bytes can
        // never be interpreted as a segment base or kernel stack pointer.
        "xor eax, eax",
        "mov [r12 + {fs_base}], rax",
        "mov [r12 + {gs_base}], rax",
        "mov [r12 + {kernel_stack_top}], rax",
        "lea r8, [r10 + {gp_size} + 8]",
        "add r8, r9",
        "test r9, r9",
        "jz 3f",
        "mov rax, [r8 - 8]",
        "mov [r12 + {error}], rax",
        "jmp 4f",
        "3:",
        "xor eax, eax",
        "mov [r12 + {error}], rax",
        "4:",
        "mov rax, [r8]",
        "mov [r12 + {rip}], rax",
        "mov rax, [r8 + 8]",
        "mov [r12 + {cs}], rax",
        "mov rax, [r8 + 16]",
        "mov [r12 + {rflags}], rax",
        // GS was normalized before capture_early.  Do not exchange it again:
        // doing so would return to the user GS base while Rust still runs.
        "5:",
        "mov rax, [r12 + {cs}]",
        "test al, 3",
        "jz 6f",
        "mov rax, [r8 + 24]",
        "mov [r12 + {rsp}], rax",
        "mov rax, [r8 + 32]",
        "mov [r12 + {ss}], rax",
        "jmp 7f",
        "6:",
        // Same-CPL events do not push RSP/SS.  Save the post-iret stack point
        // so the return path can reconstruct the original kernel stack.
        "lea rax, [r8 + 24]",
        "mov [r12 + {rsp}], rax",
        "mov rax, {kernel_ss}",
        "mov [r12 + {ss}], rax",
        "7:",
        // orig_rax is meaningful for SYSCALL only; all hardware events use a
        // sentinel so signal restart logic cannot mistake an interrupt frame.
        "cmp r11, {syscall_marker}",
        "jne 8f",
        "mov rax, [r12 + {rax}]",
        "mov [r12 + {orig_rax}], rax",
        "jmp 9f",
        "8:",
        "mov rax, -1",
        "mov [r12 + {orig_rax}], rax",
        "9:",
        "fxsave64 [r12 + {fxsave}]",
        "mov rdi, r12",
        "mov rsi, r14",
        "call {prepare}",
        // A failed owner allocation/capture must never continue with a stale
        // extended register image.  The prepare hook returns one on success;
        // zero takes the existing fail-closed halt path below.
        "test rax, rax",
        "jz 99f",
        "mov rdi, r12",
        "call {handler}",
        "test rax, rax",
        "jz 99f",
        "cmp rax, r12",
        "jne 99f",
        "mov rdi, r12",
        "call {prepare_return}",
        "test rax, rax",
        "jz 99f",

        // Build a fresh iret frame.  For a user return the reserved area below
        // TrapFrame is sufficient; for a same-CPL kernel return use the saved
        // original RSP so iretq leaves the interrupted stack untouched.
        "mov rax, [r12 + {cs}]",
        "test al, 3",
        "jz 10f",
        "mov rax, [r12 + {rip}]",
        "mov [rsp], rax",
        "mov rax, [r12 + {cs}]",
        "mov [rsp + 8], rax",
        "mov rax, [r12 + {rflags}]",
        "mov [rsp + 16], rax",
        "mov rax, [r12 + {rsp}]",
        "mov [rsp + 24], rax",
        "mov rax, [r12 + {ss}]",
        "mov [rsp + 32], rax",
        "jmp 11f",
        "10:",
        "mov r10, [r12 + {rsp}]",
        "test r10, r10",
        "jz 99f",
        "sub r10, 24",
        "mov rax, [r12 + {rip}]",
        "mov [r10], rax",
        "mov rax, [r12 + {cs}]",
        "mov [r10 + 8], rax",
        "mov rax, [r12 + {rflags}]",
        "mov [r10 + 16], rax",
        "mov rsp, r10",
        "11:",
        // Rust may use the kernel's FPU while dispatching.  Restore the
        // frame's legacy image immediately before any register/iret sequence;
        // an extended owner, when enabled, has already been restored by the
        // return hook and mirrors this legacy prefix.
        "fxrstor64 [r12 + {fxsave}]",
        // Restore the saved GPRs.  Keep r12 as the frame pointer until both
        // original r11 and r12 have been loaded.
        "mov r15, [r12 + {r15}]",
        "mov r14, [r12 + {r14}]",
        "mov r13, [r12 + {r13}]",
        "mov r10, [r12 + {r10}]",
        "mov r9,  [r12 + {r9}]",
        "mov r8,  [r12 + {r8}]",
        "mov rdi, [r12 + {rdi}]",
        "mov rsi, [r12 + {rsi}]",
        "mov rbp, [r12 + {rbp}]",
        "mov rbx, [r12 + {rbx}]",
        "mov rdx, [r12 + {rdx}]",
        "mov rcx, [r12 + {rcx}]",
        "mov rax, [r12 + {rax}]",
        "mov r11, [r12 + {r11}]",
        "mov r12, [r12 + {r12}]",
        "test byte ptr [rsp + 8], 3",
        "jz 12f",
        "swapgs",
        "12:",
        "iretq",
        "99:",
        "cli",
        "13:",
        "hlt",
        "jmp 13b",
        reserve = const COMMON_ENTRY_RESERVE,
        return_area = const RETURN_AREA_SIZE,
        frame_offset = const FRAME_OFFSET,
        gp_size = const GP_SAVE_SIZE,
        r15 = const trap_frame::R15_OFFSET,
        r14 = const trap_frame::R14_OFFSET,
        r13 = const trap_frame::R13_OFFSET,
        r12 = const trap_frame::R12_OFFSET,
        r11 = const trap_frame::R11_OFFSET,
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
        orig_rax = const trap_frame::ORIG_RAX_OFFSET,
        rip = const trap_frame::RIP_OFFSET,
        cs = const trap_frame::CS_OFFSET,
        rflags = const trap_frame::RFLAGS_OFFSET,
        rsp = const trap_frame::RSP_OFFSET,
        ss = const trap_frame::SS_OFFSET,
        error = const trap_frame::ERROR_CODE_OFFSET,
        vector = const trap_frame::VECTOR_OFFSET,
        fxsave = const trap_frame::FXSAVE_OFFSET,
        fs_base = const trap_frame::FS_BASE_OFFSET,
        gs_base = const trap_frame::GS_BASE_OFFSET,
        kernel_stack_top = const trap_frame::KERNEL_STACK_TOP_OFFSET,
        syscall_marker = const SYSCALL_VECTOR_MARKER,
        kernel_ss = const descriptor::KERNEL_SS as usize,
        msr_gs_base = const MSR_GS_BASE,
        cpu_locals = sym super::smp::CPU_LOCALS,
        cpu_locals_size = const super::smp::CPU_LOCALS_SIZE,
        cpu_local_mask = const (super::smp::CPU_LOCAL_STRIDE - 1),
        prepare = sym x86_64_trap_prepare,
        handler = sym x86_64_handle_trap,
        prepare_return = sym x86_64_prepare_trap_return,
        capture_early = sym super::xstate::capture_early,
    );
}

/// Long-mode SYSCALL entry.  `swapgs` makes [`ENTRY_STATE`] available, then a
/// synthetic hardware frame is built for the common entry above.  The common
/// path returns with `iretq` instead of SYSRET so non-canonical or signal-
/// rewritten frames cannot accidentally take SYSRET's stricter fast path.
#[cfg(target_os = "none")]
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.trap.entry")]
pub unsafe extern "C" fn __x86_syscall_entry() -> ! {
    core::arch::naked_asm!(
        "swapgs",
        "mov gs:[{user_rsp}], rsp",
        "mov rsp, gs:[{kernel_stack}]",
        "test rsp, rsp",
        "jz 1f",
        "and rsp, -16",
        // Synthetic hardware frame, pushed from the highest word down so the
        // common entry observes vector at the lowest address.
        "push {user_ss}",
        "push qword ptr gs:[{user_rsp}]",
        "push r11",
        "push {user_cs}",
        // SYSCALL places the address after the instruction in RCX.  Preserve
        // that architectural RCX value for ptrace while exposing the trapping
        // instruction through TrapFrame.rip to the generic syscall code.
        "push rcx",
        "sub qword ptr [rsp], {syscall_len}",
        "push {vector}",
        "jmp {common}",
        "1:",
        "cli",
        "2:",
        "hlt",
        "jmp 2b",
        user_rsp = const ENTRY_USER_RSP_OFFSET,
        kernel_stack = const ENTRY_KERNEL_STACK_OFFSET,
        user_ss = const descriptor::USER_SS as usize,
        user_cs = const descriptor::USER_CS as usize,
        syscall_len = const TrapFrame::SYSCALL_INSN_LEN,
        vector = const SYSCALL_VECTOR_MARKER,
        common = sym __x86_common_entry,
    );
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn x86_64_trap_prepare(frame: usize, capture_token: usize) -> usize {
    let Some(tf) = (frame != 0).then(|| unsafe { &mut *(frame as *mut TrapFrame) }) else {
        return 0;
    };
    // Hardware FXSAVE implementations may report extra MXCSR capability bits
    // in the metadata word.  Keep the frame's legacy control words within the
    // policy before any signal/ptrace path can observe or later restore it.
    if !fpu::sanitize_fxsave_area(&mut tf.fxsave) {
        return 0;
    }
    if tf.from_user() {
        // SYSCALL/IDT entry has already switched to the kernel GS anchor; the
        // user GS image is therefore visible through KERNEL_GS_BASE.
        tf.fs_base = unsafe { super::read_msr(MSR_FS_BASE) };
        tf.gs_base = unsafe { super::read_msr(MSR_KERNEL_GS_BASE) };
        tf.kernel_stack_top = task::current_kernel_stack_top();
        if super::save_user_xstate_from_trap(frame).is_err() {
            if capture_token == super::xstate::EARLY_CAPTURE_OWNED {
                super::xstate::discard_early();
            }
            return 0;
        }
        if let Some(task) = sched::try_current_task_ref()
            && task.is_ptrace_traced()
        {
            ptrace::publish_trap_frame(task, *tf);
        }
    } else if capture_token != super::xstate::EARLY_CAPTURE_KERNEL {
        // A kernel/NMI entry never owns the user scratch.  In particular, it
        // must not release an outer CPL3 snapshot while nested on that entry.
        return 0;
    }
    1
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn x86_64_prepare_trap_return(frame: usize) -> usize {
    let Some(tf) = (frame != 0).then(|| unsafe { &mut *(frame as *mut TrapFrame) }) else {
        return 0;
    };
    if !valid_return_frame(tf) {
        return 0;
    }
    if tf.from_user() {
        let Some(task) = sched::try_current_task_ref() else {
            return 0;
        };
        // A tracer changes the stop snapshot, never a live stack-resident
        // frame.  Merge it only when ptrace marked it dirty; otherwise a
        // syscall-entry snapshot would overwrite its own return value/PC.
        ptrace::finish_trap_frame(task, tf);
        if !valid_return_frame(tf) {
            return 0;
        }
        if super::xstate::restore_for_resume(task, frame).is_err() {
            return 0;
        }
        unsafe { super::restore_user_segment_bases(tf as *const TrapFrame) };
    }
    1
}

#[cfg(target_os = "none")]
fn valid_return_frame(tf: &TrapFrame) -> bool {
    if !paging::is_canonical(tf.rip as u64, false) || !paging::is_canonical(tf.rsp as u64, false) {
        return false;
    }
    if tf.rflags as u64 & trap_frame::RFLAGS_RESERVED == 0
        || tf.rflags as u64 & ((1 << 12) | (1 << 13) | (1 << 14) | (1 << 17)) != 0
    {
        return false;
    }
    // FXRSTOR faults on reserved MXCSR bits.  Validate the fixed legacy image
    // at the final return boundary as well as in ptrace/signal code, because a
    // generic signal builder can modify the TrapFrame directly.
    let mxcsr = u32::from_le_bytes(tf.fxsave[24..28].try_into().unwrap());
    let supplied_mask = u32::from_le_bytes(tf.fxsave[28..32].try_into().unwrap());
    let policy = fpu::policy();
    if !fpu::validate_mxcsr(mxcsr, policy.mxcsr_mask) || supplied_mask & !policy.mxcsr_mask != 0 {
        return false;
    }
    if tf.from_user() {
        const USER_SPACE_TOP: usize = 0x0000_8000_0000_0000;
        if tf.rip >= USER_SPACE_TOP
            || tf.rsp >= USER_SPACE_TOP
            || !paging::is_canonical(tf.fs_base as u64, false)
            || !paging::is_canonical(tf.gs_base as u64, false)
            || tf.fs_base >= USER_SPACE_TOP
            || tf.gs_base >= USER_SPACE_TOP
        {
            return false;
        }
        // Compare the complete selector words.  Checking only the low 16 bits
        // would let a forged high half pass `from_user()` and reach iretq.
        tf.cs == descriptor::USER_CS as usize && tf.ss == descriptor::USER_SS as usize
    } else {
        tf.cs == descriptor::KERNEL_CS as usize && tf.ss == descriptor::KERNEL_SS as usize
    }
}

/// Validate a scheduler-supplied frame before the non-returning iretq stub
/// dereferences it.  The pointer is still covered by the TaskOps lifetime
/// contract; this helper only rejects null/misaligned or architecturally
/// impossible state before touching the frame fields.
#[cfg(target_os = "none")]
pub(crate) fn validate_return_frame_ptr(frame: usize) -> bool {
    if frame == 0 || frame % trap_frame::FRAME_ALIGN != 0 {
        return false;
    }
    // Safety: callers pass a live TrapFrame allocation; the checks above keep
    // the ABI-alignment contract explicit before the raw return assembly.
    let tf = unsafe { &*(frame as *const TrapFrame) };
    valid_return_frame(tf)
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn x86_64_handle_trap(frame: usize) -> usize {
    let Some(tf) = (frame != 0).then(|| unsafe { &mut *(frame as *mut TrapFrame) }) else {
        return 0;
    };
    if frame % trap_frame::FRAME_ALIGN != 0 {
        return 0;
    }

    let mut vector = tf.vector;
    if vector == EXTERNAL_VECTOR_MARKER {
        let Some(actual) = super::apic::in_service_vector() else {
            return 0;
        };
        tf.vector = actual as usize;
        vector = actual as usize;
    }

    if vector == SYSCALL_VECTOR_MARKER {
        if !tf.from_user() {
            return 0;
        }
        general::syscall::dispatch(TrapFramePtr::new(frame));
        sched::run_post_syscall_handoff_lazy();
        sched::preempt_if_needed(sched::now_ns_direct());
        return frame;
    }

    if vector == interrupt::PAGE_FAULT as usize {
        return match general::mm::dispatch_page_fault(TrapFramePtr::new(frame)) {
            general::mm::FaultOutcome::Fixed => frame,
            outcome => {
                queue_user_exception(tf, outcome_signal(outcome));
                if tf.from_user() { frame } else { 0 }
            }
        };
    }

    if vector == TIMER_VECTOR as usize {
        // One-shot/deadline hardware has consumed its compare value.  Invalidate
        // the scheduler's programmed-deadline cache before servicing software
        // timers, then explicitly rearm after the tick so a quiet runqueue
        // still receives the next regular tick.
        sched::deadline_timer_fired();
        general::dev::irq::record_timer_interrupt();
        let now_ns = sched::now_ns_direct();
        let _ = sched::on_timer_tick(now_ns);
        sched::reprogram_current_deadline(None);
        super::apic::end_of_interrupt();
        sched::preempt_if_needed(sched::now_ns_direct());
        return frame;
    }
    if vector == IPI_VECTOR as usize {
        super::smp::handle_tlb_shootdown();
        sched::handle_membarrier_ipi();
        super::apic::end_of_interrupt();
        sched::preempt_if_needed(sched::now_ns_direct());
        return frame;
    }
    if vector == RESCHEDULE_VECTOR as usize {
        super::apic::end_of_interrupt();
        sched::preempt_if_needed(sched::now_ns_direct());
        return frame;
    }
    if vector == ERROR_VECTOR as usize {
        // LAPIC error interrupts are acknowledged even when no diagnostic
        // sink is installed; dropping EOI would wedge subsequent interrupts.
        super::apic::end_of_interrupt();
        return frame;
    }
    if vector == SPURIOUS_VECTOR as usize {
        // The spurious vector is intentionally not EOI'ed by the LAPIC.
        return frame;
    }

    if vector >= super::apic::FIRST_EXTERNAL_VECTOR as usize {
        if let Some(line) = super::apic::line_for_vector(vector as u8) {
            let _ = general::dev::irq::dispatch_irq_line(line);
            super::apic::end_of_interrupt();
            sched::preempt_if_needed(sched::now_ns_direct());
            return frame;
        }
        return 0;
    }

    if vector == interrupt::BREAKPOINT as usize && tf.from_user() {
        let pc = tf.rip.saturating_sub(1);
        let hook = USER_BREAK_HOOK.load(Ordering::Acquire);
        if hook != 0 {
            let hook_fn: fn(usize) -> bool = unsafe { core::mem::transmute(hook) };
            if hook_fn(pc) {
                return frame;
            }
        }
        queue_user_exception(tf, sched::SignalNumber::SIGTRAP);
        return frame;
    }

    // Kernel exceptions and user exceptions without a signal-capable task are
    // fatal.  Returning zero makes the assembly halt with IF=0 rather than
    // retrying the same fault indefinitely.
    if tf.from_user() {
        queue_user_exception(tf, signal_for_vector(vector));
        frame
    } else {
        0
    }
}

static USER_BREAK_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register the ptrace single-step breakpoint hook used by the kernel syscall
/// layer.  A second registration replaces the first only when it is the same
/// function, matching the scheduler's other one-time hook contracts.
#[cfg(target_os = "none")]
pub fn register_user_break_hook(hook: fn(usize) -> bool) {
    let value = hook as usize;
    let current = USER_BREAK_HOOK.load(Ordering::Acquire);
    if current == 0 {
        USER_BREAK_HOOK.store(value, Ordering::Release);
    } else if current != value {
        panic!("x86 user breakpoint hook already registered");
    }
}

fn signal_for_vector(vector: usize) -> sched::SignalNumber {
    const DIVIDE_ERROR: usize = interrupt::DIVIDE_ERROR as usize;
    const DEBUG: usize = interrupt::DEBUG as usize;
    const NMI: usize = interrupt::NMI as usize;
    const BREAKPOINT: usize = interrupt::BREAKPOINT as usize;
    const OVERFLOW: usize = interrupt::OVERFLOW as usize;
    const BOUND_RANGE: usize = interrupt::BOUND_RANGE as usize;
    const INVALID_OPCODE: usize = interrupt::INVALID_OPCODE as usize;
    const DEVICE_NOT_AVAILABLE: usize = interrupt::DEVICE_NOT_AVAILABLE as usize;
    const DOUBLE_FAULT: usize = interrupt::DOUBLE_FAULT as usize;
    const X87_FLOATING_POINT: usize = interrupt::X87_FLOATING_POINT as usize;
    const ALIGNMENT_CHECK: usize = interrupt::ALIGNMENT_CHECK as usize;
    const MACHINE_CHECK: usize = interrupt::MACHINE_CHECK as usize;
    const SIMD_FLOATING_POINT: usize = interrupt::SIMD_FLOATING_POINT as usize;
    const VIRTUALIZATION: usize = interrupt::VIRTUALIZATION as usize;
    const CONTROL_PROTECTION: usize = interrupt::CONTROL_PROTECTION as usize;
    const GENERAL_PROTECTION: usize = interrupt::GENERAL_PROTECTION as usize;
    const INVALID_TSS: usize = interrupt::INVALID_TSS as usize;
    const SEGMENT_NOT_PRESENT: usize = interrupt::SEGMENT_NOT_PRESENT as usize;
    const STACK_SEGMENT: usize = interrupt::STACK_SEGMENT as usize;
    const VMM_COMMUNICATION: usize = interrupt::VMM_COMMUNICATION as usize;
    const SECURITY_EXCEPTION: usize = interrupt::SECURITY_EXCEPTION as usize;
    match vector {
        DIVIDE_ERROR | X87_FLOATING_POINT | SIMD_FLOATING_POINT => sched::SignalNumber::SIGFPE,
        DEBUG | BREAKPOINT => sched::SignalNumber::SIGTRAP,
        INVALID_OPCODE => sched::SignalNumber::SIGILL,
        NMI | ALIGNMENT_CHECK | MACHINE_CHECK => sched::SignalNumber::SIGBUS,
        OVERFLOW | BOUND_RANGE | DEVICE_NOT_AVAILABLE | DOUBLE_FAULT | GENERAL_PROTECTION
        | INVALID_TSS | SEGMENT_NOT_PRESENT | STACK_SEGMENT | VIRTUALIZATION
        | CONTROL_PROTECTION | VMM_COMMUNICATION | SECURITY_EXCEPTION => {
            sched::SignalNumber::SIGSEGV
        }
        _ => sched::SignalNumber::SIGBUS,
    }
}

fn outcome_signal(outcome: general::mm::FaultOutcome) -> sched::SignalNumber {
    match outcome {
        general::mm::FaultOutcome::OutOfMemory => sched::SignalNumber::SIGKILL,
        _ => sched::SignalNumber::SIGSEGV,
    }
}

fn queue_user_exception(tf: &TrapFrame, signal: sched::SignalNumber) {
    if !tf.from_user() || !sched::is_ready() {
        return;
    }
    let task = sched::current_task();
    if let Some(pid) = task.pid_root() {
        let _ = sched::operation::tkill(pid, Some(signal));
        let _ = sched::operation::deliver_pending_signals_for_task(
            &task,
            sched::UserContextRef::new(tf as *const TrapFrame as usize),
        );
    }
}

#[cfg(not(target_os = "none"))]
pub fn register_user_break_hook(_hook: fn(usize) -> bool) {
    // Hosted tests do not execute IDT instructions.  Keep registration a
    // visible, deterministic no-op rather than exposing a false trap path.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_vectors_are_complete() {
        for vector in [8, 10, 11, 12, 13, 14, 17, 21, 29, 30] {
            assert!(has_hardware_error(vector));
        }
        assert!(!has_hardware_error(3));
        assert!(!has_hardware_error(32));
    }

    #[test]
    fn stack_marker_values_do_not_collide_with_idt() {
        assert!(EXTERNAL_VECTOR_MARKER > u8::MAX as usize);
        assert!(SYSCALL_VECTOR_MARKER > u8::MAX as usize);
    }

    #[test]
    fn common_entry_stack_layout_is_aligned_and_disjoint() {
        assert_eq!(GP_SAVE_SIZE, 15 * core::mem::size_of::<usize>());
        assert_eq!(GP_SAVE_STORAGE_SIZE, 128);
        assert_eq!(FRAME_OFFSET, RETURN_AREA_SIZE + GP_SAVE_STORAGE_SIZE);
        assert_eq!(COMMON_ENTRY_RESERVE, FRAME_OFFSET + trap_frame::FRAME_SIZE);
        assert_eq!(COMMON_ENTRY_RESERVE % 16, 0);
        assert_eq!((FRAME_OFFSET + trap_frame::FXSAVE_OFFSET) % 16, 0);

        // `and rsp, -16` leaves the raw push source at either A or A+8.
        // The scratch storage ends before A in both cases.
        let aligned_source = 0x20_000usize;
        let reserved_base = aligned_source - COMMON_ENTRY_RESERVE;
        let scratch_start = reserved_base + RETURN_AREA_SIZE;
        let scratch_end = scratch_start + GP_SAVE_STORAGE_SIZE;
        for source_offset in [0usize, 8] {
            let source_start = aligned_source + source_offset;
            let source_end = source_start + GP_SAVE_SIZE;
            assert!(scratch_end <= source_start || source_end <= scratch_start);
        }
    }

    #[test]
    fn stack_publication_is_observable() {
        set_kernel_stack_top(0x1234_5000);
        assert_eq!(
            ENTRY_STATE.kernel_stack_top.load(Ordering::Acquire),
            0x1234_5000
        );
    }

    #[test]
    fn linux_exception_signal_mapping_covers_arithmetic_debug_and_protection() {
        assert_eq!(
            signal_for_vector(interrupt::DIVIDE_ERROR as usize),
            sched::SignalNumber::SIGFPE
        );
        assert_eq!(
            signal_for_vector(interrupt::X87_FLOATING_POINT as usize),
            sched::SignalNumber::SIGFPE
        );
        assert_eq!(
            signal_for_vector(interrupt::DEBUG as usize),
            sched::SignalNumber::SIGTRAP
        );
        assert_eq!(
            signal_for_vector(interrupt::BREAKPOINT as usize),
            sched::SignalNumber::SIGTRAP
        );
        assert_eq!(
            signal_for_vector(interrupt::INVALID_OPCODE as usize),
            sched::SignalNumber::SIGILL
        );
        assert_eq!(
            signal_for_vector(interrupt::OVERFLOW as usize),
            sched::SignalNumber::SIGSEGV
        );
        assert_eq!(
            signal_for_vector(interrupt::GENERAL_PROTECTION as usize),
            sched::SignalNumber::SIGSEGV
        );
        assert_eq!(
            signal_for_vector(interrupt::ALIGNMENT_CHECK as usize),
            sched::SignalNumber::SIGBUS
        );
    }
}

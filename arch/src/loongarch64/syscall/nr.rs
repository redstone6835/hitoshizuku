//! LoongArch64 Linux syscall 编号表。

include!("../../linux_asm_generic_syscall_nr.rs");

pub const SYS_RISCV_FLUSH_ICACHE: Option<usize> = None;
pub const AUDIT_ARCH: u32 = 0xc000_0102;

pub static LINUX_SYSCALL_ABI: general::syscall::LinuxSyscallAbi =
    general::syscall::LinuxSyscallAbi {
        audit_arch: AUDIT_ARCH,
        strict_allow: [SYS_READ, SYS_WRITE, SYS_EXIT, SYS_RT_SIGRETURN],
        trace_signal_boundary: SYS_KILL,
    };

const _: () = {
    assert!(matches!(SYS_SYNC_FILE_RANGE, Some(84)));
    assert!(SYS_SYNC_FILE_RANGE2.is_none());
    assert!(SYS_CLOCK_GETTIME64.is_none());
    assert!(SYS_FUTEX_TIME64.is_none());
};

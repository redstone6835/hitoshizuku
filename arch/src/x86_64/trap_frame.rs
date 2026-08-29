//! x86_64 异常/系统调用帧。
//!
//! 这个布局是 arch 内部的稳定契约：入口汇编、`TaskOps`、syscall 分发器以及
//! HAL 的用户上下文包装都只通过这里定义的字段交换状态。浮点扩展状态不参与
//! 普通寄存器恢复；`fxsave` 保存 legacy FXSAVE 区，完整 XSAVE 状态由 `fpu`
//! 模块按需保存到任务扩展中。

use core::mem::offset_of;

/// `FXSAVE` legacy 区的固定大小。
pub const FXSAVE_SIZE: usize = 512;

/// x86_64 用户/内核代码段选择子（flat 64-bit GDT 的约定值）。
pub const USER_CS: u64 = 0x33;
pub const USER_SS: u64 = 0x2b;
pub const KERNEL_CS: u64 = 0x10;
pub const KERNEL_SS: u64 = 0x18;

/// `RFLAGS` 中始终应置位的保留位和用户态中断使能位。
pub const RFLAGS_RESERVED: u64 = 1 << 1;
pub const RFLAGS_INTERRUPT_ENABLE: u64 = 1 << 9;
pub const USER_RFLAGS: u64 = RFLAGS_RESERVED | RFLAGS_INTERRUPT_ENABLE;

/// 尚未由 syscall 入口填写 `orig_rax` 时使用的标记。
pub const ORIG_RAX_NONE: usize = usize::MAX;

/// x86_64 通用寄存器和返回控制状态。
///
/// 前半部分按照 Linux `pt_regs` 的常用顺序排列，便于将来直接接入入口桩；
/// `fxsave` 仅保存 legacy FPU/SSE 区，AVX 以上组件位于 `fpu::XState`。
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug)]
pub struct TrapFrame {
    pub r15: usize,
    pub r14: usize,
    pub r13: usize,
    pub r12: usize,
    pub r11: usize,
    pub r10: usize,
    pub r9: usize,
    pub r8: usize,
    pub rdi: usize,
    pub rsi: usize,
    pub rbp: usize,
    pub rbx: usize,
    pub rdx: usize,
    pub rcx: usize,
    pub rax: usize,
    pub orig_rax: usize,
    pub rip: usize,
    pub cs: usize,
    pub rflags: usize,
    pub rsp: usize,
    pub ss: usize,
    pub error_code: usize,
    pub vector: usize,
    /// 用户 FS base；x86 硬件异常帧不会自动压入，入口/上下文切换代码负责维护。
    pub fs_base: usize,
    /// 用户 GS base 的软件镜像（内核 GS 由交换桩维护）。
    pub gs_base: usize,
    /// 可信的内核栈顶；非零表示返回用户态，与其它架构保持一致的语义。
    pub kernel_stack_top: usize,
    /// legacy FXSAVE 区，包含 x87/MMX、XMM 和 MXCSR。
    pub fxsave: [u8; FXSAVE_SIZE],
}

impl Default for TrapFrame {
    fn default() -> Self {
        let fxsave = Self::initial_fxsave();
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rdi: 0,
            rsi: 0,
            rbp: 0,
            rbx: 0,
            rdx: 0,
            rcx: 0,
            rax: 0,
            orig_rax: ORIG_RAX_NONE,
            rip: 0,
            cs: 0,
            rflags: USER_RFLAGS as usize,
            rsp: 0,
            ss: 0,
            error_code: 0,
            vector: 0,
            fs_base: 0,
            gs_base: 0,
            kernel_stack_top: 0,
            fxsave,
        }
    }
}

impl TrapFrame {
    /// Legacy FXSAVE bytes for a freshly-created task.  The control word,
    /// MXCSR and MXCSR mask match the architectural/Linux reset convention.
    pub const fn initial_fxsave() -> [u8; FXSAVE_SIZE] {
        let mut fxsave = [0u8; FXSAVE_SIZE];
        fxsave[0] = 0x7f;
        fxsave[1] = 0x03;
        fxsave[24] = 0x80;
        fxsave[25] = 0x1f;
        fxsave[28] = 0xbf;
        fxsave[29] = 0xff;
        fxsave
    }

    /// Linux x86_64 syscall 指令的长度为 2 字节。
    pub const SYSCALL_INSN_LEN: usize = 2;

    /// Convert the architectural SYSCALL return RIP into the common syscall
    /// frame contract: `rip` names the syscall instruction itself and the
    /// generic dispatcher advances it exactly once when execution completes.
    #[inline]
    pub const fn syscall_instruction_rip(return_rip: usize) -> Option<usize> {
        return_rip.checked_sub(Self::SYSCALL_INSN_LEN)
    }

    #[inline]
    pub fn syscall_id(&self) -> usize {
        self.rax
    }

    #[inline]
    pub fn syscall_args(&self) -> [usize; 6] {
        [self.rdi, self.rsi, self.rdx, self.r10, self.r8, self.r9]
    }

    #[inline]
    pub fn set_syscall_return(&mut self, value: usize) {
        self.rax = value;
    }

    #[inline]
    pub fn skip_syscall_insn(&mut self) {
        self.rip = self.rip.wrapping_add(Self::SYSCALL_INSN_LEN);
    }

    #[inline]
    pub fn from_user(&self) -> bool {
        self.cs & 3 == 3
    }
}

// 汇编入口会依赖这些偏移；集中导出可避免手写数字与 Rust 布局失配。
pub const R15_OFFSET: usize = offset_of!(TrapFrame, r15);
pub const R14_OFFSET: usize = offset_of!(TrapFrame, r14);
pub const R13_OFFSET: usize = offset_of!(TrapFrame, r13);
pub const R12_OFFSET: usize = offset_of!(TrapFrame, r12);
pub const R11_OFFSET: usize = offset_of!(TrapFrame, r11);
pub const R10_OFFSET: usize = offset_of!(TrapFrame, r10);
pub const R9_OFFSET: usize = offset_of!(TrapFrame, r9);
pub const R8_OFFSET: usize = offset_of!(TrapFrame, r8);
pub const RDI_OFFSET: usize = offset_of!(TrapFrame, rdi);
pub const RSI_OFFSET: usize = offset_of!(TrapFrame, rsi);
pub const RBP_OFFSET: usize = offset_of!(TrapFrame, rbp);
pub const RBX_OFFSET: usize = offset_of!(TrapFrame, rbx);
pub const RDX_OFFSET: usize = offset_of!(TrapFrame, rdx);
pub const RCX_OFFSET: usize = offset_of!(TrapFrame, rcx);
pub const RAX_OFFSET: usize = offset_of!(TrapFrame, rax);
pub const ORIG_RAX_OFFSET: usize = offset_of!(TrapFrame, orig_rax);
pub const RIP_OFFSET: usize = offset_of!(TrapFrame, rip);
pub const CS_OFFSET: usize = offset_of!(TrapFrame, cs);
pub const RFLAGS_OFFSET: usize = offset_of!(TrapFrame, rflags);
pub const RSP_OFFSET: usize = offset_of!(TrapFrame, rsp);
pub const SS_OFFSET: usize = offset_of!(TrapFrame, ss);
pub const ERROR_CODE_OFFSET: usize = offset_of!(TrapFrame, error_code);
pub const VECTOR_OFFSET: usize = offset_of!(TrapFrame, vector);
pub const FS_BASE_OFFSET: usize = offset_of!(TrapFrame, fs_base);
pub const GS_BASE_OFFSET: usize = offset_of!(TrapFrame, gs_base);
pub const KERNEL_STACK_TOP_OFFSET: usize = offset_of!(TrapFrame, kernel_stack_top);
pub const FXSAVE_OFFSET: usize = offset_of!(TrapFrame, fxsave);

pub const FRAME_SIZE: usize = core::mem::size_of::<TrapFrame>();
pub const FRAME_ALIGN: usize = core::mem::align_of::<TrapFrame>();

const _: () = {
    assert!(FRAME_ALIGN >= 16);
    assert!(FRAME_SIZE % FRAME_ALIGN == 0);
    assert!(FXSAVE_OFFSET % 16 == 0);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fpu_state_is_architecturally_valid() {
        let frame = TrapFrame::default();
        assert_eq!(
            u16::from_le_bytes([frame.fxsave[0], frame.fxsave[1]]),
            0x037f
        );
        assert_eq!(
            u32::from_le_bytes([
                frame.fxsave[24],
                frame.fxsave[25],
                frame.fxsave[26],
                frame.fxsave[27]
            ]),
            0x1f80
        );
        assert_eq!(
            u16::from_le_bytes([frame.fxsave[28], frame.fxsave[29]]),
            0xffbf
        );
    }

    #[test]
    fn syscall_rip_uses_instruction_then_return_contract() {
        let instruction = 0x4000usize;
        let mut frame = TrapFrame::default();
        frame.rip = TrapFrame::syscall_instruction_rip(instruction + TrapFrame::SYSCALL_INSN_LEN)
            .expect("a SYSCALL return RIP follows the two-byte instruction");

        assert_eq!(frame.rip, instruction);
        frame.skip_syscall_insn();
        assert_eq!(frame.rip, instruction + TrapFrame::SYSCALL_INSN_LEN);
        assert_eq!(TrapFrame::syscall_instruction_rip(1), None);
    }
}

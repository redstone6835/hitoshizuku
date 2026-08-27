//! 用户态 trap-frame 的 HAL 封装。
//!
//! kernel/sched 只处理这个不透明类型；具体寄存器布局仍留在 arch。

use general::{TaskOps, TrapFramePtr};

#[cfg(target_arch = "loongarch64")]
#[derive(Clone, Copy)]
pub struct UserTrapFrame {
    inner: arch::TrapFrame,
}

#[cfg(target_arch = "riscv64")]
#[derive(Clone, Copy)]
pub struct UserTrapFrame {
    inner: arch::TrapFrame,
}

#[kernel_symbols::export]
impl UserTrapFrame {
    /// 复制 ptrace 停止点保存的原始架构 trap frame。
    pub fn from_ptrace_task(task: &sched::Task) -> Option<Self> {
        arch::ptrace_task_frame(task).map(|inner| Self { inner })
    }

    /// Linux `NT_FPREGSET` 在当前架构上的固定长度。
    pub const fn linux_fpregset_size() -> usize {
        arch::LINUX_FPREGSET_SIZE
    }

    /// 从 ptrace 停止点按 Linux `NT_FPREGSET` 布局编码浮点寄存器。
    pub fn read_linux_fpregs(task: &sched::Task) -> Option<alloc::vec::Vec<u8>> {
        arch::read_user_linux_fpregs(task)
    }

    /// 把 Linux `NT_FPREGSET` 字节写回 ptrace 停止点。
    pub fn write_linux_fpregs(task: &sched::Task, bytes: &[u8]) -> bool {
        arch::write_user_linux_fpregs(task, bytes)
    }

    /// 当前架构用于软件单步的用户断点指令。
    pub const fn breakpoint_insn() -> u32 {
        arch::USER_BREAKPOINT_INSN
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.from_context", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL)]
    pub fn from_context(raw: usize) -> Self {
        #[cfg(target_arch = "loongarch64")]
        {
            let tf = unsafe { &*(raw as *const arch::TrapFrame) };
            Self { inner: *tf }
        }

        #[cfg(target_arch = "riscv64")]
        {
            let tf = unsafe { &*(raw as *const arch::TrapFrame) };
            Self { inner: *tf }
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.apply_to_context", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn apply_to_context(&self, raw: usize) {
        #[cfg(target_arch = "loongarch64")]
        {
            let tf = unsafe { &mut *(raw as *mut arch::TrapFrame) };
            *tf = self.inner;
        }

        #[cfg(target_arch = "riscv64")]
        {
            let tf = unsafe { &mut *(raw as *mut arch::TrapFrame) };
            *tf = self.inner;
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.init_user", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL)]
    pub fn init_user(entry_pc: usize, user_sp: usize, arg0: usize) -> Self {
        #[cfg(target_arch = "loongarch64")]
        {
            let mut frame = arch::TrapFrame::default();
            let ptr = TrapFramePtr::new(&mut frame as *mut _ as usize);
            <arch::LoongArch64TaskOps as TaskOps>::init_user_trap_frame(
                ptr, entry_pc, user_sp, arg0,
            );
            Self { inner: frame }
        }

        #[cfg(target_arch = "riscv64")]
        {
            let mut frame = arch::TrapFrame::default();
            let ptr = TrapFramePtr::new(&mut frame as *mut _ as usize);
            <arch::Riscv64TaskOps as TaskOps>::init_user_trap_frame(ptr, entry_pc, user_sp, arg0);
            Self { inner: frame }
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.pc", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
    pub fn pc(&self) -> usize {
        #[cfg(target_arch = "loongarch64")]
        {
            self.inner.pc
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.sepc
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.sp", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
    pub fn sp(&self) -> usize {
        #[cfg(target_arch = "loongarch64")]
        {
            self.inner.sp
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.sp
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.set_pc", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn set_pc(&mut self, pc: usize) {
        #[cfg(target_arch = "loongarch64")]
        {
            self.inner.pc = pc;
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.sepc = pc;
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.set_sp", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn set_sp(&mut self, sp: usize) {
        #[cfg(target_arch = "loongarch64")]
        {
            self.inner.sp = sp;
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.sp = sp;
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.set_tls", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn set_tls(&mut self, tls: usize) {
        #[cfg(target_arch = "loongarch64")]
        {
            self.inner.tp = tls;
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.tp = tls;
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.set_ret", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn set_ret(&mut self, value: usize) {
        #[cfg(target_arch = "loongarch64")]
        {
            self.inner.a0 = value;
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.a0 = value;
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.ret", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
    pub fn ret(&self) -> usize {
        #[cfg(target_arch = "loongarch64")]
        {
            self.inner.a0
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.a0
        }
    }

    pub fn signal_interrupted_syscall_pc(&self) -> Option<usize> {
        let ptr = TrapFramePtr::new(&self.inner as *const arch::TrapFrame as usize);
        <arch::CurrentTaskOps as TaskOps>::signal_interrupted_syscall_pc(ptr)
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.set_args", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn set_args(&mut self, arg0: usize, arg1: usize, arg2: usize) {
        #[cfg(target_arch = "loongarch64")]
        {
            self.inner.a0 = arg0;
            self.inner.a1 = arg1;
            self.inner.a2 = arg2;
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.a0 = arg0;
            self.inner.a1 = arg1;
            self.inner.a2 = arg2;
        }
    }

    /// 设置 Native 启动入口专用的 bootstrap process handle。
    pub fn set_arg3(&mut self, value: usize) {
        #[cfg(target_arch = "loongarch64")]
        {
            self.inner.a3 = value;
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.a3 = value;
        }
    }

    pub fn set_ra(&mut self, ra: usize) {
        #[cfg(target_arch = "loongarch64")]
        {
            self.inner.ra = ra;
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.ra = ra;
        }
    }

    pub fn set_kernel_stack_top(&mut self, kstack_top: usize) {
        #[cfg(target_arch = "loongarch64")]
        {
            let _ = kstack_top;
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.kstack_top = kstack_top;
        }
    }

    pub fn set_current_address_space(&mut self) {
        #[cfg(target_arch = "loongarch64")]
        {}

        #[cfg(target_arch = "riscv64")]
        {
            let satp: usize;
            unsafe {
                core::arch::asm!(
                    "csrr {satp}, satp",
                    satp = out(reg) satp,
                    options(nomem, nostack, preserves_flags)
                );
            }
            self.inner.satp = satp;
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.advance_pc", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn advance_pc(&mut self) {
        #[cfg(target_arch = "loongarch64")]
        {
            self.inner.pc = self.inner.pc.wrapping_add(4);
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.sepc = self.inner.sepc.wrapping_add(4);
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.encoded_len", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
    pub fn encoded_len() -> usize {
        #[cfg(target_arch = "loongarch64")]
        {
            core::mem::size_of::<arch::TrapFrame>()
        }

        #[cfg(target_arch = "riscv64")]
        {
            core::mem::size_of::<arch::TrapFrame>()
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.write_bytes", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn write_bytes(&self, out: &mut [u8]) -> bool {
        let len = Self::encoded_len();
        if out.len() < len {
            return false;
        }
        #[cfg(target_arch = "loongarch64")]
        {
            let ptr = &self.inner as *const arch::TrapFrame as *const u8;
            let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
            out[..len].copy_from_slice(bytes);
            true
        }

        #[cfg(target_arch = "riscv64")]
        {
            let ptr = &self.inner as *const arch::TrapFrame as *const u8;
            let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
            out[..len].copy_from_slice(bytes);
            true
        }
    }

    pub fn write_linux_mcontext(&self, out: &mut [u8]) -> bool {
        #[cfg(target_arch = "loongarch64")]
        {
            const PC_OFF: usize = 0;
            const REGS_OFF: usize = 8;
            const FLAGS_OFF: usize = REGS_OFF + 32 * 8;
            if out.len() < FLAGS_OFF + 4 {
                return false;
            }
            out[..FLAGS_OFF + 4].fill(0);
            write_u64(out, PC_OFF, self.inner.pc as u64);
            let regs = [
                0usize,
                self.inner.ra,
                self.inner.tp,
                self.inner.sp,
                self.inner.a0,
                self.inner.a1,
                self.inner.a2,
                self.inner.a3,
                self.inner.a4,
                self.inner.a5,
                self.inner.a6,
                self.inner.a7,
                self.inner.t0,
                self.inner.t1,
                self.inner.t2,
                self.inner.t3,
                self.inner.t4,
                self.inner.t5,
                self.inner.t6,
                self.inner.t7,
                self.inner.t8,
                self.inner.rx,
                self.inner.s0,
                self.inner.s1,
                self.inner.s2,
                self.inner.s3,
                self.inner.s4,
                self.inner.s5,
                self.inner.s6,
                self.inner.s7,
                self.inner.s8,
                self.inner.s9,
            ];
            for (idx, reg) in regs.iter().enumerate() {
                write_u64(out, REGS_OFF + idx * 8, *reg as u64);
            }
            true
        }

        #[cfg(target_arch = "riscv64")]
        {
            // riscv 的 sigcontext.sc_regs 本身即 `struct user_regs_struct`
            // （pc + 31 个通用寄存器，无 x0），共 256 字节。这里严格按该布局：
            // pc@0、ra..t6 依次落在 REGS_OFF 起的 31 个槽位。
            const PC_OFF: usize = 0;
            const REGS_OFF: usize = 8;
            const MCONTEXT_LEN: usize = REGS_OFF + 31 * 8;
            if out.len() < MCONTEXT_LEN {
                return false;
            }
            out[..MCONTEXT_LEN].fill(0);
            write_u64(out, PC_OFF, self.inner.sepc as u64);
            let regs = [
                self.inner.ra,
                self.inner.sp,
                self.inner.gp,
                self.inner.tp,
                self.inner.t0,
                self.inner.t1,
                self.inner.t2,
                self.inner.s0,
                self.inner.s1,
                self.inner.a0,
                self.inner.a1,
                self.inner.a2,
                self.inner.a3,
                self.inner.a4,
                self.inner.a5,
                self.inner.a6,
                self.inner.a7,
                self.inner.s2,
                self.inner.s3,
                self.inner.s4,
                self.inner.s5,
                self.inner.s6,
                self.inner.s7,
                self.inner.s8,
                self.inner.s9,
                self.inner.s10,
                self.inner.s11,
                self.inner.t3,
                self.inner.t4,
                self.inner.t5,
                self.inner.t6,
            ];
            for (idx, reg) in regs.iter().enumerate() {
                write_u64(out, REGS_OFF + idx * 8, *reg as u64);
            }
            true
        }
    }

    pub fn apply_linux_mcontext(&mut self, input: &[u8]) -> bool {
        #[cfg(target_arch = "loongarch64")]
        {
            const PC_OFF: usize = 0;
            const REGS_OFF: usize = 8;
            const FLAGS_OFF: usize = REGS_OFF + 32 * 8;
            if input.len() < FLAGS_OFF + 4 {
                return false;
            }
            let reg = |idx: usize| -> usize { read_u64(input, REGS_OFF + idx * 8) as usize };
            self.inner.pc = read_u64(input, PC_OFF) as usize;
            self.inner.ra = reg(1);
            self.inner.tp = reg(2);
            self.inner.sp = reg(3);
            self.inner.a0 = reg(4);
            self.inner.a1 = reg(5);
            self.inner.a2 = reg(6);
            self.inner.a3 = reg(7);
            self.inner.a4 = reg(8);
            self.inner.a5 = reg(9);
            self.inner.a6 = reg(10);
            self.inner.a7 = reg(11);
            self.inner.t0 = reg(12);
            self.inner.t1 = reg(13);
            self.inner.t2 = reg(14);
            self.inner.t3 = reg(15);
            self.inner.t4 = reg(16);
            self.inner.t5 = reg(17);
            self.inner.t6 = reg(18);
            self.inner.t7 = reg(19);
            self.inner.t8 = reg(20);
            self.inner.rx = reg(21);
            self.inner.s0 = reg(22);
            self.inner.s1 = reg(23);
            self.inner.s2 = reg(24);
            self.inner.s3 = reg(25);
            self.inner.s4 = reg(26);
            self.inner.s5 = reg(27);
            self.inner.s6 = reg(28);
            self.inner.s7 = reg(29);
            self.inner.s8 = reg(30);
            self.inner.s9 = reg(31);
            true
        }

        #[cfg(target_arch = "riscv64")]
        {
            // 与 write_linux_mcontext 的 riscv 分支对称：pc@0 后紧跟 31 个
            // 通用寄存器（ra..t6），无 x0 槽位。
            const PC_OFF: usize = 0;
            const REGS_OFF: usize = 8;
            const MCONTEXT_LEN: usize = REGS_OFF + 31 * 8;
            if input.len() < MCONTEXT_LEN {
                return false;
            }
            let reg = |idx: usize| -> usize { read_u64(input, REGS_OFF + idx * 8) as usize };
            self.inner.sepc = read_u64(input, PC_OFF) as usize;
            self.inner.ra = reg(0);
            self.inner.sp = reg(1);
            self.inner.gp = reg(2);
            self.inner.tp = reg(3);
            self.inner.t0 = reg(4);
            self.inner.t1 = reg(5);
            self.inner.t2 = reg(6);
            self.inner.s0 = reg(7);
            self.inner.s1 = reg(8);
            self.inner.a0 = reg(9);
            self.inner.a1 = reg(10);
            self.inner.a2 = reg(11);
            self.inner.a3 = reg(12);
            self.inner.a4 = reg(13);
            self.inner.a5 = reg(14);
            self.inner.a6 = reg(15);
            self.inner.a7 = reg(16);
            self.inner.s2 = reg(17);
            self.inner.s3 = reg(18);
            self.inner.s4 = reg(19);
            self.inner.s5 = reg(20);
            self.inner.s6 = reg(21);
            self.inner.s7 = reg(22);
            self.inner.s8 = reg(23);
            self.inner.s9 = reg(24);
            self.inner.s10 = reg(25);
            self.inner.s11 = reg(26);
            self.inner.t3 = reg(27);
            self.inner.t4 = reg(28);
            self.inner.t5 = reg(29);
            self.inner.t6 = reg(30);
            true
        }
    }

    /// ptrace 原始寄存器组的大小（字节）。
    ///
    /// - riscv64：`struct user_regs_struct`（pc + 31 个通用寄存器）= 256；
    /// - loongarch64：`struct user_pt_regs`（regs[32] + orig_a0 + csr_era
    ///   + csr_badv + reserved[10]）= 360。
    ///
    /// 注意：这不是信号帧 mcontext/sigcontext 的大小——loongarch 的 sigcontext
    /// 是 268 字节（pc@0 + regs@8 + flags@264），两者不可混用。
    pub fn linux_user_regs_size() -> usize {
        #[cfg(target_arch = "loongarch64")]
        {
            32 * 8 + 8 + 8 + 8 + 10 * 8 // 360
        }
        #[cfg(target_arch = "riscv64")]
        {
            8 + 31 * 8 // 256
        }
    }

    /// 把 trap frame 编码为 ptrace 原始寄存器组
    /// （riscv64 `user_regs_struct` / loongarch64 `user_pt_regs`）。
    ///
    /// 与 `write_linux_mcontext` 的区别：后者编码信号帧的 sigcontext 布局；
    /// 本函数编码 `PTRACE_GETREGSET(NT_PRSTATUS)` / `GETREGS` / `PEEKUSR`
    /// 等 ptrace 路径所需的原始寄存器组。
    pub fn write_linux_user_regs(&self, out: &mut [u8]) -> bool {
        #[cfg(target_arch = "loongarch64")]
        {
            // `struct user_pt_regs`（loongarch64 Linux uapi）：
            //   regs[0..32] @ 0    —— r0..r31（r0 恒 0）
            //   orig_a0    @ 256   —— 进入 syscall 时的 a0（尽力而为）
            //   csr_era    @ 264   —— 返回地址 / pc
            //   csr_badv   @ 272   —— 出错地址（填 0）
            //   reserved   @ 280   —— 10 个 u64，全 0
            //
            // TrapFrame 字段名与 Linux ABI 命名错位：TrapFrame.s0 是 $r22（fp），
            // TrapFrame.s1..s9 是 $r23..$r31（Linux s0..s8），必须按 r0..r31
            // 的真实顺序映射。
            const REGS_OFF: usize = 0;
            const ORIG_A0_OFF: usize = 256;
            const CSR_ERA_OFF: usize = 264;
            const REGS_LEN: usize = 280 + 10 * 8; // 360
            if out.len() < REGS_LEN {
                return false;
            }
            out[..REGS_LEN].fill(0);
            let regs = [
                0usize,        // r0  = zero
                self.inner.ra, // r1  = ra
                self.inner.tp, // r2  = tp
                self.inner.sp, // r3  = sp
                self.inner.a0, // r4  = a0
                self.inner.a1, // r5  = a1
                self.inner.a2, // r6  = a2
                self.inner.a3, // r7  = a3
                self.inner.a4, // r8  = a4
                self.inner.a5, // r9  = a5
                self.inner.a6, // r10 = a6
                self.inner.a7, // r11 = a7
                self.inner.t0, // r12 = t0
                self.inner.t1, // r13 = t1
                self.inner.t2, // r14 = t2
                self.inner.t3, // r15 = t3
                self.inner.t4, // r16 = t4
                self.inner.t5, // r17 = t5
                self.inner.t6, // r18 = t6
                self.inner.t7, // r19 = t7
                self.inner.t8, // r20 = t8
                self.inner.rx, // r21 = u0（保留）
                self.inner.s0, // r22 = fp
                self.inner.s1, // r23 = s0
                self.inner.s2, // r24 = s1
                self.inner.s3, // r25 = s2
                self.inner.s4, // r26 = s3
                self.inner.s5, // r27 = s4
                self.inner.s6, // r28 = s5
                self.inner.s7, // r29 = s6
                self.inner.s8, // r30 = s7
                self.inner.s9, // r31 = s8
            ];
            for (idx, reg) in regs.iter().enumerate() {
                write_u64(out, REGS_OFF + idx * 8, *reg as u64);
            }
            write_u64(out, ORIG_A0_OFF, self.inner.a0 as u64);
            write_u64(out, CSR_ERA_OFF, self.inner.pc as u64);
            // csr_badv 与 reserved 保持 0。
            true
        }

        #[cfg(target_arch = "riscv64")]
        {
            // riscv 的 sigcontext.sc_regs 即 `struct user_regs_struct`，
            // 与修正后的 mcontext 布局一致，直接复用。
            self.write_linux_mcontext(out)
        }
    }

    /// 从 ptrace 原始寄存器组字节写回 trap frame（`write_linux_user_regs` 的反向）。
    pub fn apply_linux_user_regs(&mut self, input: &[u8]) -> bool {
        #[cfg(target_arch = "loongarch64")]
        {
            const REGS_OFF: usize = 0;
            const CSR_ERA_OFF: usize = 264;
            const REGS_LEN: usize = 280 + 10 * 8; // 360
            if input.len() < REGS_LEN {
                return false;
            }
            let reg = |idx: usize| -> usize { read_u64(input, REGS_OFF + idx * 8) as usize };
            // regs[0]（r0）恒 0，不回写；orig_a0 仅作记录，不覆盖 a0（regs[4]）。
            self.inner.ra = reg(1);
            self.inner.tp = reg(2);
            self.inner.sp = reg(3);
            self.inner.a0 = reg(4);
            self.inner.a1 = reg(5);
            self.inner.a2 = reg(6);
            self.inner.a3 = reg(7);
            self.inner.a4 = reg(8);
            self.inner.a5 = reg(9);
            self.inner.a6 = reg(10);
            self.inner.a7 = reg(11);
            self.inner.t0 = reg(12);
            self.inner.t1 = reg(13);
            self.inner.t2 = reg(14);
            self.inner.t3 = reg(15);
            self.inner.t4 = reg(16);
            self.inner.t5 = reg(17);
            self.inner.t6 = reg(18);
            self.inner.t7 = reg(19);
            self.inner.t8 = reg(20);
            self.inner.rx = reg(21);
            self.inner.s0 = reg(22);
            self.inner.s1 = reg(23);
            self.inner.s2 = reg(24);
            self.inner.s3 = reg(25);
            self.inner.s4 = reg(26);
            self.inner.s5 = reg(27);
            self.inner.s6 = reg(28);
            self.inner.s7 = reg(29);
            self.inner.s8 = reg(30);
            self.inner.s9 = reg(31);
            // csr_era 即 pc；csr_badv/reserved 忽略。
            self.inner.pc = read_u64(input, CSR_ERA_OFF) as usize;
            true
        }

        #[cfg(target_arch = "riscv64")]
        {
            self.apply_linux_mcontext(input)
        }
    }

    #[kernel_symbols::export(name = "hal.user_context.UserTrapFrame.read_bytes", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
    pub fn read_bytes(input: &[u8]) -> Option<Self> {
        let len = Self::encoded_len();
        if input.len() < len {
            return None;
        }
        #[cfg(target_arch = "loongarch64")]
        {
            let mut frame = arch::TrapFrame::default();
            let dst = &mut frame as *mut arch::TrapFrame as *mut u8;
            let dst = unsafe { core::slice::from_raw_parts_mut(dst, len) };
            dst.copy_from_slice(&input[..len]);
            Some(Self { inner: frame })
        }

        #[cfg(target_arch = "riscv64")]
        {
            let mut frame = arch::TrapFrame::default();
            let dst = &mut frame as *mut arch::TrapFrame as *mut u8;
            let dst = unsafe { core::slice::from_raw_parts_mut(dst, len) };
            dst.copy_from_slice(&input[..len]);
            Some(Self { inner: frame })
        }
    }

    /// # Safety
    ///
    /// `frame` must describe a valid user-mode context in the current address space.
    pub unsafe fn resume(&self) -> ! {
        #[cfg(target_arch = "loongarch64")]
        {
            let ptr = TrapFramePtr::new(&self.inner as *const arch::TrapFrame as usize);
            unsafe { <arch::LoongArch64TaskOps as TaskOps>::resume_to_trap_frame(ptr) }
        }

        #[cfg(target_arch = "riscv64")]
        {
            let ptr = TrapFramePtr::new(&self.inner as *const arch::TrapFrame as usize);
            unsafe { <arch::Riscv64TaskOps as TaskOps>::resume_to_trap_frame(ptr) }
        }
    }
}

fn write_u64(out: &mut [u8], off: usize, value: u64) {
    out[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u64(input: &[u8], off: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&input[off..off + 8]);
    u64::from_le_bytes(raw)
}

#[kernel_symbols::export(name = "hal.user_context.set_kernel_trap_stack", contract = "kernel.hal.user-context@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn set_kernel_trap_stack(stack_top: usize) {
    #[cfg(target_arch = "loongarch64")]
    {
        <arch::LoongArch64TaskOps as TaskOps>::set_kernel_trap_stack(stack_top);
    }

    #[cfg(target_arch = "riscv64")]
    {
        <arch::Riscv64TaskOps as TaskOps>::set_kernel_trap_stack(stack_top);
    }
}

unsafe impl Send for UserTrapFrame {}
unsafe impl Sync for UserTrapFrame {}

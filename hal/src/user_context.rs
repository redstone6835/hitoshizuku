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

#[cfg(target_arch = "x86_64")]
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

    /// 将 tracer 已验证的通用寄存器组写回架构的活动 ptrace stop 快照。
    ///
    /// x86 的真实陷阱帧驻留在被停止任务的内核栈上，不能由 tracer 直接
    /// 解引用；arch 会在恢复用户态前将这个快照合并回该实时帧。其它架构
    /// 没有这种写回后端时由 kernel 保留其首次用户返回的兼容回退。
    pub fn store_ptrace_task(&self, task: &sched::Task) -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            arch::store_x86_ptrace_task_frame(task, self.inner)
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (self, task);
            false
        }
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

    /// Encode the current context's Linux signal fpstate object.  The arch
    /// backend adds XSAVE software magic/trailer when extended state is live.
    pub fn encode_linux_signal_xstate(
        task: &sched::Task,
        context: usize,
    ) -> Option<alloc::vec::Vec<u8>> {
        #[cfg(target_arch = "x86_64")]
        {
            let frame = unsafe { &*(context as *const arch::TrapFrame) };
            return arch::encode_user_linux_signal_xstate(task, &frame.fxsave);
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (task, context);
            None
        }
    }

    /// Linux `NT_X86_XSTATE` 标准非压缩布局的当前长度；非 x86 返回 0。
    pub fn linux_xstate_size() -> usize {
        #[cfg(target_arch = "x86_64")]
        {
            arch::linux_xstate_size()
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            0
        }
    }

    pub fn linux_signal_xstate_max_size() -> usize {
        #[cfg(target_arch = "x86_64")]
        {
            arch::LINUX_SIGNAL_XSTATE_MAX_SIZE
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            0
        }
    }

    pub fn linux_signal_xstate_size_from_prefix(prefix: &[u8]) -> Option<usize> {
        #[cfg(target_arch = "x86_64")]
        {
            arch::user_linux_signal_xstate_size(prefix)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = prefix;
            None
        }
    }

    pub fn read_linux_xstate(task: &sched::Task) -> Option<alloc::vec::Vec<u8>> {
        #[cfg(target_arch = "x86_64")]
        {
            arch::read_user_linux_xstate(task)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = task;
            None
        }
    }

    pub fn write_linux_xstate(task: &sched::Task, bytes: &[u8]) -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            arch::write_user_linux_xstate(task, bytes)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (task, bytes);
            false
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub fn encode_linux_signal_xstate_from_fpregs(
        task: &sched::Task,
        fpregs: &[u8],
    ) -> Option<alloc::vec::Vec<u8>> {
        arch::encode_user_linux_signal_xstate(task, fpregs)
    }

    pub fn restore_linux_signal_xstate(
        task: &sched::Task,
        context: usize,
        bytes: Option<&[u8]>,
    ) -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            arch::restore_user_linux_signal_xstate(task, context, bytes)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (task, context, bytes);
            false
        }
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

        #[cfg(target_arch = "x86_64")]
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

        #[cfg(target_arch = "x86_64")]
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

        #[cfg(target_arch = "x86_64")]
        {
            let mut frame = arch::TrapFrame::default();
            let ptr = TrapFramePtr::new(&mut frame as *mut _ as usize);
            <arch::X86_64TaskOps as TaskOps>::init_user_trap_frame(ptr, entry_pc, user_sp, arg0);
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

        #[cfg(target_arch = "x86_64")]
        {
            self.inner.rip
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

        #[cfg(target_arch = "x86_64")]
        {
            self.inner.rsp
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

        #[cfg(target_arch = "x86_64")]
        {
            self.inner.rip = pc;
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

        #[cfg(target_arch = "x86_64")]
        {
            self.inner.rsp = sp;
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

        #[cfg(target_arch = "x86_64")]
        {
            self.inner.fs_base = tls;
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

        #[cfg(target_arch = "x86_64")]
        {
            self.inner.rax = value;
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

        #[cfg(target_arch = "x86_64")]
        {
            self.inner.rax
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

        #[cfg(target_arch = "x86_64")]
        {
            self.inner.rdi = arg0;
            self.inner.rsi = arg1;
            self.inner.rdx = arg2;
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

        #[cfg(target_arch = "x86_64")]
        {
            self.inner.rcx = value;
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

        #[cfg(target_arch = "x86_64")]
        {
            // x86-64 has no link-register slot.  Signal delivery places the
            // restorer in the user stack prefix through `hal::user`; writing
            // it into `orig_rax` would corrupt syscall restart/ptrace state.
            let _ = ra;
        }
    }

    /// 设置架构定义的信号返回入口。
    ///
    /// Link-register 架构将地址写入 trap frame；x86-64 的返回地址由
    /// `hal::user::encode_signal_return_prefix` 写入用户栈，因此这里是空操作。
    pub fn set_signal_return_address(&mut self, restorer: usize) {
        #[cfg(target_arch = "x86_64")]
        {
            let _ = restorer;
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            self.set_ra(restorer);
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

        #[cfg(target_arch = "x86_64")]
        {
            self.inner.kernel_stack_top = kstack_top;
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

        #[cfg(target_arch = "x86_64")]
        {
            self.inner.rip = self.inner.rip.wrapping_add(2);
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

        #[cfg(target_arch = "x86_64")]
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

        #[cfg(target_arch = "x86_64")]
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

        #[cfg(target_arch = "x86_64")]
        {
            // Linux x86_64 `struct sigcontext_64` is 256 bytes.  Keep the
            // exact UAPI offsets from asm/sigcontext.h; in particular the
            // selector quartet is packed at 144..151 and is followed by
            // eight-byte err/trapno/oldmask/cr2/fpstate fields.
            const LEN: usize = 256;
            if out.len() < LEN {
                return false;
            }
            out[..LEN].fill(0);
            let regs = [
                self.inner.r8,
                self.inner.r9,
                self.inner.r10,
                self.inner.r11,
                self.inner.r12,
                self.inner.r13,
                self.inner.r14,
                self.inner.r15,
                self.inner.rdi,
                self.inner.rsi,
                self.inner.rbp,
                self.inner.rbx,
                self.inner.rdx,
                self.inner.rax,
                self.inner.rcx,
                self.inner.rsp,
                self.inner.rip,
            ];
            for (index, value) in regs.into_iter().enumerate() {
                write_u64(out, index * 8, value as u64);
            }
            write_u64(out, 136, self.inner.rflags as u64);
            write_u16(out, 144, self.inner.cs as u16);
            write_u16(out, 146, 0); // gs selector is tracked by the arch frame
            write_u16(out, 148, 0); // fs selector; fs_base is carried separately
            // `sigcontext_64` has a reserved __pad0 word at 150; SS is not
            // part of the Linux x86_64 signal ABI and is restored from the
            // fixed user data selector by the kernel return path.
            write_u16(out, 150, 0);
            write_u64(out, 152, self.inner.error_code as u64);
            write_u64(out, 160, self.inner.vector as u64);
            // The generic trap frame has no signal-mask, CR2 or external
            // xstate pointer.  Zero is the Linux-compatible "not supplied"
            // value; the extended xstate is owned by the scheduler backend.
            write_u64(out, 168, 0); // oldmask
            write_u64(out, 176, 0); // cr2
            write_u64(out, 184, 0); // fpstate
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

        #[cfg(target_arch = "x86_64")]
        {
            const LEN: usize = 256;
            if input.len() < LEN {
                return false;
            }
            let reg = |index: usize| -> usize { read_u64(input, index * 8) as usize };
            // Decode into locals first.  A malformed signal frame must not
            // partially mutate the live trap frame before validation completes.
            let r8 = reg(0);
            let r9 = reg(1);
            let r10 = reg(2);
            let r11 = reg(3);
            let r12 = reg(4);
            let r13 = reg(5);
            let r14 = reg(6);
            let r15 = reg(7);
            let rdi = reg(8);
            let rsi = reg(9);
            let rbp = reg(10);
            let rbx = reg(11);
            let rdx = reg(12);
            let rax = reg(13);
            let rcx = reg(14);
            let rsp = reg(15);
            let rip = reg(16);
            let rflags = read_u64(input, 136);
            let cs = read_u16(input, 144);
            let gs = read_u16(input, 146);
            let fs = read_u16(input, 148);
            let pad0 = read_u16(input, 150);

            // Match the checks performed by the final x86 iret boundary:
            // low-half canonical user addresses, fixed user code/data
            // selectors, fixed RFLAGS bit, and no privilege-control flags.
            let forbidden_rflags = (1u64 << 12) | (1u64 << 13) | (1u64 << 14) | (1u64 << 17);
            if !x86_user_canonical(rip as u64)
                || !x86_user_canonical(rsp as u64)
                || rflags & 0x2 == 0
                || rflags & !0x0000_0000_003f_ffffu64 != 0
                || rflags & forbidden_rflags != 0
                || cs != arch::x86_64::descriptor::USER_CS
                || pad0 != 0
                || !x86_user_selector(gs)
                || !x86_user_selector(fs)
            {
                return false;
            }
            // The private tail is reserved by Linux's sigcontext ABI.  Do not
            // silently accept a future/foreign extension that this compact
            // implementation cannot restore.
            if input[192..LEN].iter().any(|byte| *byte != 0) {
                return false;
            }
            // This compact signal-frame implementation does not expose an
            // out-of-line xstate buffer.  Reject a non-zero pointer instead of
            // silently discarding user-controlled FPU state.
            if read_u64(input, 184) != 0 {
                return false;
            }

            self.inner.r8 = r8;
            self.inner.r9 = r9;
            self.inner.r10 = r10;
            self.inner.r11 = r11;
            self.inner.r12 = r12;
            self.inner.r13 = r13;
            self.inner.r14 = r14;
            self.inner.r15 = r15;
            self.inner.rdi = rdi;
            self.inner.rsi = rsi;
            self.inner.rbp = rbp;
            self.inner.rbx = rbx;
            self.inner.rdx = rdx;
            self.inner.rax = rax;
            self.inner.rcx = rcx;
            self.inner.rsp = rsp;
            self.inner.rip = rip;
            self.inner.rflags = rflags as usize;
            self.inner.cs = cs as usize;
            self.inner.ss = arch::x86_64::descriptor::USER_SS as usize;
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
        #[cfg(target_arch = "x86_64")]
        {
            // Linux x86_64 `struct user_regs_struct`: 27 u64 fields.
            27 * 8
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

        #[cfg(target_arch = "x86_64")]
        {
            const LEN: usize = 27 * 8;
            if out.len() < LEN {
                return false;
            }
            out[..LEN].fill(0);
            let regs = [
                self.inner.r15,
                self.inner.r14,
                self.inner.r13,
                self.inner.r12,
                self.inner.rbp,
                self.inner.rbx,
                self.inner.r11,
                self.inner.r10,
                self.inner.r9,
                self.inner.r8,
                self.inner.rax,
                self.inner.rcx,
                self.inner.rdx,
                self.inner.rsi,
                self.inner.rdi,
                self.inner.orig_rax,
                self.inner.rip,
                self.inner.cs,
                self.inner.rflags,
                self.inner.rsp,
                self.inner.ss,
                self.inner.fs_base,
                self.inner.gs_base,
                0, // ds
                0, // es
                0, // fs selector
                0, // gs selector
            ];
            for (index, value) in regs.into_iter().enumerate() {
                write_u64(out, index * 8, value as u64);
            }
            true
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

        #[cfg(target_arch = "x86_64")]
        {
            const LEN: usize = 27 * 8;
            if input.len() < LEN {
                return false;
            }
            let reg = |index: usize| -> usize { read_u64(input, index * 8) as usize };
            let r15 = reg(0);
            let r14 = reg(1);
            let r13 = reg(2);
            let r12 = reg(3);
            let rbp = reg(4);
            let rbx = reg(5);
            let r11 = reg(6);
            let r10 = reg(7);
            let r9 = reg(8);
            let r8 = reg(9);
            let rax = reg(10);
            let rcx = reg(11);
            let rdx = reg(12);
            let rsi = reg(13);
            let rdi = reg(14);
            let orig_rax = reg(15);
            let rip = reg(16);
            let cs = reg(17);
            let rflags = reg(18) as u64;
            let rsp = reg(19);
            let ss = reg(20);
            let fs_base = reg(21);
            let gs_base = reg(22);
            let ds = reg(23) as u16;
            let es = reg(24) as u16;
            let fs = reg(25) as u16;
            let gs = reg(26) as u16;
            if !x86_user_canonical(rip as u64)
                || !x86_user_canonical(rsp as u64)
                || rflags & 0x2 == 0
                || rflags & !(0x0000_0000_003f_ffffu64) != 0
                || cs != arch::x86_64::descriptor::USER_CS as usize
                || ss != arch::x86_64::descriptor::USER_SS as usize
                || !x86_user_selector(ds)
                || !x86_user_selector(es)
                || !x86_user_selector(fs)
                || !x86_user_selector(gs)
            {
                return false;
            }
            self.inner.r15 = r15;
            self.inner.r14 = r14;
            self.inner.r13 = r13;
            self.inner.r12 = r12;
            self.inner.rbp = rbp;
            self.inner.rbx = rbx;
            self.inner.r11 = r11;
            self.inner.r10 = r10;
            self.inner.r9 = r9;
            self.inner.r8 = r8;
            self.inner.rax = rax;
            self.inner.rcx = rcx;
            self.inner.rdx = rdx;
            self.inner.rsi = rsi;
            self.inner.rdi = rdi;
            self.inner.orig_rax = orig_rax;
            self.inner.rip = rip;
            self.inner.cs = cs;
            self.inner.rflags = rflags as usize;
            self.inner.rsp = rsp;
            self.inner.ss = ss;
            self.inner.fs_base = fs_base;
            self.inner.gs_base = gs_base;
            true
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

        #[cfg(target_arch = "x86_64")]
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

        #[cfg(target_arch = "x86_64")]
        {
            let ptr = TrapFramePtr::new(&self.inner as *const arch::TrapFrame as usize);
            unsafe { <arch::X86_64TaskOps as TaskOps>::resume_to_trap_frame(ptr) }
        }
    }
}

fn write_u64(out: &mut [u8], off: usize, value: u64) {
    out[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

fn write_u16(out: &mut [u8], off: usize, value: u16) {
    out[off..off + 2].copy_from_slice(&value.to_le_bytes());
}

fn read_u64(input: &[u8], off: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&input[off..off + 8]);
    u64::from_le_bytes(raw)
}

fn read_u16(input: &[u8], off: usize) -> u16 {
    let mut raw = [0u8; 2];
    raw.copy_from_slice(&input[off..off + 2]);
    u16::from_le_bytes(raw)
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn x86_user_canonical(value: u64) -> bool {
    // This kernel reserves the low canonical half for userspace.  Rejecting
    // high-half addresses here also prevents a signal frame from smuggling a
    // kernel pointer past the later iret validation.
    value <= 0x0000_7fff_ffff_ffff
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn x86_user_selector(value: u16) -> bool {
    value == 0 || (value & 0x4 == 0 && value & 0x3 == 0x3)
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

    #[cfg(target_arch = "x86_64")]
    {
        <arch::X86_64TaskOps as TaskOps>::set_kernel_trap_stack(stack_top);
    }
}

unsafe impl Send for UserTrapFrame {}
unsafe impl Sync for UserTrapFrame {}

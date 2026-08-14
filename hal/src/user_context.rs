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
            const PC_OFF: usize = 0;
            const REGS_OFF: usize = 8;
            const MCONTEXT_LEN: usize = REGS_OFF + 32 * 8;
            if out.len() < MCONTEXT_LEN {
                return false;
            }
            out[..MCONTEXT_LEN].fill(0);
            write_u64(out, PC_OFF, self.inner.sepc as u64);
            let regs = [
                0usize,
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
            const PC_OFF: usize = 0;
            const REGS_OFF: usize = 8;
            const MCONTEXT_LEN: usize = REGS_OFF + 32 * 8;
            if input.len() < MCONTEXT_LEN {
                return false;
            }
            let reg = |idx: usize| -> usize { read_u64(input, REGS_OFF + idx * 8) as usize };
            self.inner.sepc = read_u64(input, PC_OFF) as usize;
            self.inner.ra = reg(1);
            self.inner.sp = reg(2);
            self.inner.gp = reg(3);
            self.inner.tp = reg(4);
            self.inner.t0 = reg(5);
            self.inner.t1 = reg(6);
            self.inner.t2 = reg(7);
            self.inner.s0 = reg(8);
            self.inner.s1 = reg(9);
            self.inner.a0 = reg(10);
            self.inner.a1 = reg(11);
            self.inner.a2 = reg(12);
            self.inner.a3 = reg(13);
            self.inner.a4 = reg(14);
            self.inner.a5 = reg(15);
            self.inner.a6 = reg(16);
            self.inner.a7 = reg(17);
            self.inner.s2 = reg(18);
            self.inner.s3 = reg(19);
            self.inner.s4 = reg(20);
            self.inner.s5 = reg(21);
            self.inner.s6 = reg(22);
            self.inner.s7 = reg(23);
            self.inner.s8 = reg(24);
            self.inner.s9 = reg(25);
            self.inner.s10 = reg(26);
            self.inner.s11 = reg(27);
            self.inner.t3 = reg(28);
            self.inner.t4 = reg(29);
            self.inner.t5 = reg(30);
            self.inner.t6 = reg(31);
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

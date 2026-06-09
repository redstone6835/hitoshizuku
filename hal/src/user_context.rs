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

impl UserTrapFrame {
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
            <arch::Riscv64TaskOps as TaskOps>::init_user_trap_frame(
                ptr, entry_pc, user_sp, arg0,
            );
            Self { inner: frame }
        }
    }

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
        { let _ = kstack_top; }

        #[cfg(target_arch = "riscv64")]
        {
            self.inner.kstack_top = kstack_top;
        }
    }

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
            let ptr = TrapFramePtr::new(
                &self.inner as *const arch::TrapFrame as usize,
            );
            unsafe {
                <arch::LoongArch64TaskOps as TaskOps>::resume_to_trap_frame(ptr)
            }
        }

        #[cfg(target_arch = "riscv64")]
        {
            let ptr = TrapFramePtr::new(
                &self.inner as *const arch::TrapFrame as usize,
            );
            unsafe {
                <arch::Riscv64TaskOps as TaskOps>::resume_to_trap_frame(ptr)
            }
        }
    }
}

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

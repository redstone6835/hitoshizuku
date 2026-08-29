//! x86_64 `general::TaskOps` 实现。

use core::sync::atomic::{AtomicUsize, Ordering};

use general::{TaskOps, TrapFramePtr};

use super::trap_frame::{
    FRAME_ALIGN, FRAME_SIZE, KERNEL_CS, KERNEL_SS, ORIG_RAX_NONE, RFLAGS_RESERVED, TrapFrame,
    USER_CS, USER_RFLAGS, USER_SS,
};

/// 当前 CPU 的内核栈镜像。
///
/// 真正的入口桩可以把它映射到 per-CPU TSS/`rsp0`；在尚未安装 GDT/TSS 的
/// 早期阶段使用原子镜像仍能让 HAL 和测试拥有确定的行为。
static KERNEL_STACK_TOP: AtomicUsize = AtomicUsize::new(0);

/// x86_64 的任务寄存器操作。
pub struct X86_64TaskOps;

impl TaskOps for X86_64TaskOps {
    fn trap_frame_pc(trap_frame_ptr: TrapFramePtr) -> usize {
        unsafe { &*(trap_frame_ptr.as_usize() as *const TrapFrame) }.rip
    }

    fn trap_frame_sp(trap_frame_ptr: TrapFramePtr) -> usize {
        unsafe { &*(trap_frame_ptr.as_usize() as *const TrapFrame) }.rsp
    }

    fn trap_frame_status(trap_frame_ptr: TrapFramePtr) -> usize {
        unsafe { &*(trap_frame_ptr.as_usize() as *const TrapFrame) }.rflags
    }

    fn set_trap_frame_sp(trap_frame_ptr: TrapFramePtr, sp: usize) {
        unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) }.rsp = sp;
    }

    fn set_trap_frame_gp(trap_frame_ptr: TrapFramePtr, gp: usize) {
        // SysV x86_64 的 callee-saved GP 对应 RBX；没有独立的架构 GP 寄存器。
        unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) }.rbx = gp;
    }

    fn set_trap_frame_tp(trap_frame_ptr: TrapFramePtr, tp: usize) {
        // Linux x86_64 TLS 的用户可见线程指针由 FS base 承载。
        unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) }.fs_base = tp;
    }

    fn trap_frame_size() -> usize {
        FRAME_SIZE
    }

    fn trap_frame_align() -> usize {
        FRAME_ALIGN
    }

    fn set_kernel_trap_stack(stack_top: usize) {
        // TSS.rsp0 is packed at byte offset four and is therefore not an
        // naturally aligned 64-bit store.  Keep interrupts off while the
        // scheduler publishes it so a privilege transition cannot observe a
        // torn stack pointer.
        let irq_state = super::interrupt::save_and_disable();
        KERNEL_STACK_TOP.store(stack_top, Ordering::Release);
        super::trap::set_kernel_stack_top(stack_top);
        // TSS.rsp0 的写入由 x86 启动/TSS 模块接管；这里保留统一入口，避免
        // 在尚未完成 GDT 初始化时执行不可恢复的 segment 操作。
        #[cfg(target_os = "none")]
        super::set_tss_rsp0(stack_top);
        super::interrupt::restore(irq_state);
    }

    unsafe fn resume_to_trap_frame(trap_frame_ptr: TrapFramePtr) -> ! {
        #[cfg(target_os = "none")]
        {
            let frame = trap_frame_ptr.as_usize();
            if !super::trap::validate_return_frame_ptr(frame) {
                // A malformed frame must never reach the raw iretq sequence.
                // Halt with IF clear instead of attempting recovery on an
                // untrusted stack/control-state tuple.
                super::interrupt::disable();
                super::interrupt::halt();
            }
            // Safety: 调用方保证指针指向当前任务的有效帧；汇编只读取该帧，
            // 最终通过 iretq 恢复 CPL3/CPL0 返回状态，绝不返回 Rust 调用者。
            // FS/GS 基址通过 MSR 在进入裸汇编前发布；GS 的 swapgs 对由入口
            // 桩负责，避免在这里破坏 per-CPU 内核 GS。
            // Restore an explicitly owned AVX-family image before entering the
            // final iretq path.  A user frame without a published current task
            // is not a recoverable state: proceeding would leave the previous
            // task's extended registers live in the CPU.
            let from_user = unsafe { (*(frame as *const TrapFrame)).from_user() };
            if from_user {
                let Some(task) = sched::try_current_task_ref() else {
                    super::interrupt::disable();
                    super::interrupt::halt();
                };
                if super::xstate::restore_for_resume(task, frame).is_err() {
                    super::interrupt::disable();
                    super::interrupt::halt();
                }
            }
            unsafe { super::restore_user_segment_bases(frame as *const TrapFrame) };
            unsafe { super::resume_to_trap_frame_raw(frame) }
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = trap_frame_ptr;
            panic!("x86_64 resume_to_trap_frame is unavailable on a hosted target");
        }
    }

    fn init_kernel_trap_frame(trap_frame_ptr: TrapFramePtr, entry_pc: usize, kernel_sp: usize) {
        let tf = unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) };
        *tf = TrapFrame::default();
        tf.rip = entry_pc;
        tf.rsp = kernel_sp;
        tf.cs = KERNEL_CS as usize;
        tf.ss = KERNEL_SS as usize;
        tf.rflags = (RFLAGS_RESERVED | (1 << 9)) as usize;
        tf.kernel_stack_top = 0;
        tf.orig_rax = ORIG_RAX_NONE;
    }

    fn init_user_trap_frame(
        trap_frame_ptr: TrapFramePtr,
        entry_pc: usize,
        user_sp: usize,
        arg0: usize,
    ) {
        let tf = unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) };
        *tf = TrapFrame::default();
        tf.rip = entry_pc;
        tf.rsp = user_sp;
        tf.rdi = arg0;
        tf.cs = USER_CS as usize;
        tf.ss = USER_SS as usize;
        tf.rflags = USER_RFLAGS as usize;
        tf.kernel_stack_top = KERNEL_STACK_TOP.load(Ordering::Acquire);
        tf.orig_rax = ORIG_RAX_NONE;
    }

    fn set_user_trap_frame_args(
        trap_frame_ptr: TrapFramePtr,
        arg0: usize,
        arg1: usize,
        arg2: usize,
    ) {
        let tf = unsafe { &mut *(trap_frame_ptr.as_usize() as *mut TrapFrame) };
        tf.rdi = arg0;
        tf.rsi = arg1;
        tf.rdx = arg2;
    }

    fn signal_interrupted_syscall_pc(trap_frame_ptr: TrapFramePtr) -> Option<usize> {
        let tf = unsafe { &*(trap_frame_ptr.as_usize() as *const TrapFrame) };
        // x86 SYSCALL entry reconstructs the trapping instruction address in
        // the frame.  Signal setup for a restartable syscall runs before the
        // generic dispatcher advances this PC.
        (tf.orig_rax != ORIG_RAX_NONE).then_some(tf.rip)
    }

    fn init_user_entry() -> unsafe extern "C" fn() -> ! {
        super::user_entry
    }

    fn demo_user_entry() -> unsafe extern "C" fn() -> ! {
        super::demo_user_entry
    }

    fn idle_task_entry() -> unsafe extern "C" fn() -> ! {
        super::idle_task_entry
    }

    fn sync_icache() {
        // x86 指令缓存与数据缓存保持硬件一致性；序列化由修改代码的调用方
        // 负责（必要时使用 `cpuid`/`mfence`）。
        core::sync::atomic::compiler_fence(Ordering::SeqCst);
    }
}

/// 供启动代码查询当前已发布的 TSS 栈顶。
#[inline]
pub fn current_kernel_stack_top() -> usize {
    KERNEL_STACK_TOP.load(Ordering::Acquire)
}

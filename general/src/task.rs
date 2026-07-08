use crate::TrapFramePtr;

pub trait TaskOps {
    fn trap_frame_pc(trap_frame_ptr: TrapFramePtr) -> usize;

    fn trap_frame_sp(trap_frame_ptr: TrapFramePtr) -> usize;
    fn trap_frame_status(trap_frame_ptr: TrapFramePtr) -> usize;
    fn set_trap_frame_sp(trap_frame_ptr: TrapFramePtr, sp: usize);
    fn set_trap_frame_gp(trap_frame_ptr: TrapFramePtr, gp: usize);
    fn set_trap_frame_tp(trap_frame_ptr: TrapFramePtr, tp: usize);

    fn trap_frame_size() -> usize;

    fn trap_frame_align() -> usize;

    fn set_kernel_trap_stack(stack_top: usize);

    /// # Safety
    ///
    /// `trap_frame_ptr` must point to a valid trap frame for the current
    /// architecture. The frame must remain alive for the duration of the resume
    /// operation and contain register state that is safe to restore.
    unsafe fn resume_to_trap_frame(trap_frame_ptr: TrapFramePtr) -> !;

    fn init_kernel_trap_frame(trap_frame_ptr: TrapFramePtr, entry_pc: usize, kernel_sp: usize);

    fn init_user_trap_frame(
        trap_frame_ptr: TrapFramePtr,
        entry_pc: usize,
        user_sp: usize,
        arg0: usize,
    );
    fn set_user_trap_frame_args(
        trap_frame_ptr: TrapFramePtr,
        arg0: usize,
        arg1: usize,
        arg2: usize,
    );

    fn signal_interrupted_syscall_pc(trap_frame_ptr: TrapFramePtr) -> Option<usize>;

    fn init_user_entry() -> unsafe extern "C" fn() -> !;

    fn demo_user_entry() -> unsafe extern "C" fn() -> !;

    fn idle_task_entry() -> unsafe extern "C" fn() -> !;

    fn sync_icache();
}

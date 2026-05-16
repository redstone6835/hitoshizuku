//! LoongArch64 syscall 注入总入口。本子树唯一 pub 符号是 [`register`]。
mod frame_ops;

/// 由 `arch::loongarch64::sched_ctx::register` 启动期调用一次。
pub fn register() {
    general::syscall::register_frame_ops(&frame_ops::SYSCALL_FRAME_OPS);
}

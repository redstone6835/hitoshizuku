//! RISC-V64 的 syscall trap frame 适配 → [`general::syscall::SyscallFrameOps`]。
//!
//! 本文件**唯一**对外符号是 `static SYSCALL_FRAME_OPS`，由同级 `syscall::register`
//! 注入 general。general 的 `syscall::dispatch` 在 trap 进来时通过这张 vtable
//! 读 a7 / a0-a5、写返回值、推 PC——arch 零业务逻辑。

use general::TrapFramePtr;
use general::syscall::{NativeCallFrame, NativeCallReturn, SyscallFrameOps};

use crate::riscv64::specific::TrapFrame;

/// # Safety
/// `tf` 必须是 arch trap 入口写入的有效 TrapFrame 指针。
unsafe fn trap_frame<'a>(tf: TrapFramePtr) -> &'a TrapFrame {
    // 安全性：调用方约束；arch 侧保证该指针在返回用户态前一直有效。
    unsafe { &*(tf.as_usize() as *const TrapFrame) }
}

/// # Safety
/// 同上的可变版本。
unsafe fn trap_frame_mut<'a>(tf: TrapFramePtr) -> &'a mut TrapFrame {
    // 安全性：同 trap_frame；上层保证同一 trap 内不并发调用。
    unsafe { &mut *(tf.as_usize() as *mut TrapFrame) }
}

fn sys_nr(tf: TrapFramePtr) -> usize {
    // 安全性：契约。
    unsafe { trap_frame(tf).a7 }
}

fn sys_args(tf: TrapFramePtr) -> [usize; 6] {
    // 安全性：契约。
    let f = unsafe { trap_frame(tf) };
    [f.a0, f.a1, f.a2, f.a3, f.a4, f.a5]
}

fn set_sys_ret(tf: TrapFramePtr, ret: isize) {
    // 安全性：契约；同一 trap 不会并发进入。
    unsafe { trap_frame_mut(tf).a0 = ret as usize };
}

fn native_call(tf: TrapFramePtr) -> NativeCallFrame {
    // 安全性：契约。
    let f = unsafe { trap_frame(tf) };
    NativeCallFrame {
        slot: f.a7 as u64,
        object_handle: f.a6 as u64,
        args: [
            f.a0 as u64,
            f.a1 as u64,
            f.a2 as u64,
            f.a3 as u64,
            f.a4 as u64,
        ],
        reserved_arg: f.a5 as u64,
    }
}

fn set_native_ret(tf: TrapFramePtr, ret: NativeCallReturn) {
    let ret = ret.canonicalized();
    // 安全性：契约；同一 trap 不会并发进入。
    let f = unsafe { trap_frame_mut(tf) };
    f.a0 = ret.status as usize;
    f.a1 = ret.value0 as usize;
    f.a2 = ret.value1 as usize;
}

fn advance_pc(tf: TrapFramePtr) {
    // 安全性：契约。
    let f = unsafe { trap_frame_mut(tf) };
    f.sepc = f.sepc.wrapping_add(4);
}

pub(super) static SYSCALL_FRAME_OPS: SyscallFrameOps = SyscallFrameOps {
    sys_nr,
    sys_args,
    set_sys_ret,
    native_call,
    set_native_ret,
    advance_pc,
};

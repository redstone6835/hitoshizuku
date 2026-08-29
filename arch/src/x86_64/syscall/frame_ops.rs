//! x86_64 syscall trap frame 适配。

use general::TrapFramePtr;
use general::syscall::{NativeCallFrame, NativeCallReturn, SyscallFrameOps};

use super::super::trap_frame::TrapFrame;

unsafe fn frame<'a>(ptr: TrapFramePtr) -> &'a TrapFrame {
    // Safety: syscall 入口保证 ptr 指向当前任务仍存活的 TrapFrame。
    unsafe { &*(ptr.as_usize() as *const TrapFrame) }
}

unsafe fn frame_mut<'a>(ptr: TrapFramePtr) -> &'a mut TrapFrame {
    // Safety: 同一 syscall trap 内只有当前 CPU 写入该帧。
    unsafe { &mut *(ptr.as_usize() as *mut TrapFrame) }
}

fn sys_nr(ptr: TrapFramePtr) -> usize {
    unsafe { frame(ptr).rax }
}

fn sys_args(ptr: TrapFramePtr) -> [usize; 6] {
    let f = unsafe { frame(ptr) };
    [f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9]
}

fn set_sys_ret(ptr: TrapFramePtr, ret: isize) {
    unsafe { frame_mut(ptr).rax = ret as usize };
}

fn native_call(ptr: TrapFramePtr) -> NativeCallFrame {
    let f = unsafe { frame(ptr) };
    NativeCallFrame {
        slot: f.rax as u64,
        object_handle: f.rdi as u64,
        args: [
            f.rsi as u64,
            f.rdx as u64,
            f.r10 as u64,
            f.r8 as u64,
            f.r9 as u64,
        ],
        // SYSCALL overwrites R11 with user RFLAGS before the entry stub runs.
        // RBX is preserved by the instruction and is otherwise outside the
        // Linux syscall argument register set, so it carries the native ABI's
        // explicit reserved word.
        reserved_arg: f.rbx as u64,
    }
}

fn set_native_ret(ptr: TrapFramePtr, ret: NativeCallReturn) {
    let ret = ret.canonicalized();
    let f = unsafe { frame_mut(ptr) };
    f.rax = ret.status as usize;
    f.rdx = ret.value0 as usize;
    f.r10 = ret.value1 as usize;
}

fn advance_pc(ptr: TrapFramePtr) {
    unsafe { frame_mut(ptr).skip_syscall_insn() };
}

pub(super) static SYSCALL_FRAME_OPS: SyscallFrameOps = SyscallFrameOps {
    sys_nr,
    sys_args,
    set_sys_ret,
    native_call,
    set_native_ret,
    advance_pc,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_reserved_word_uses_a_syscall_preserved_register() {
        let mut frame = TrapFrame::default();
        frame.rbx = 0xfeed;
        frame.r11 = 0xdead;
        let call = native_call(TrapFramePtr::new(&mut frame as *mut TrapFrame as usize));

        assert_eq!(call.reserved_arg, 0xfeed);
    }
}

//! RISC-V64 Linux ptrace 寄存器 ABI。

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use sched::{TASKEXT_PTRACE_FRAME, Task};

use super::trap_frame::TrapFrame;

pub const LINUX_FPREGSET_SIZE: usize = 32 * 8 + 8;
pub const BREAKPOINT_INSN: u32 = 0x0010_0073;

pub fn task_frame(task: &Task) -> Option<TrapFrame> {
    task.ext_lookup(TASKEXT_PTRACE_FRAME)
        .and_then(|payload| payload.downcast::<TrapFrame>().ok())
        .map(|frame| *frame)
}

pub fn read_linux_fpregs(task: &Task) -> Option<Vec<u8>> {
    let frame = task
        .ext_lookup(TASKEXT_PTRACE_FRAME)?
        .downcast::<TrapFrame>()
        .ok()?;
    let mut out = vec![0u8; LINUX_FPREGSET_SIZE];
    for (index, reg) in frame.f.iter().enumerate() {
        out[index * 8..index * 8 + 8].copy_from_slice(&reg.to_le_bytes());
    }
    out[256..264].copy_from_slice(&(frame.fcsr as u64).to_le_bytes());
    Some(out)
}

pub fn write_linux_fpregs(task: &Task, bytes: &[u8]) -> bool {
    if bytes.len() < LINUX_FPREGSET_SIZE {
        return false;
    }
    let Some(frame) = task
        .ext_lookup(TASKEXT_PTRACE_FRAME)
        .and_then(|payload| payload.downcast::<TrapFrame>().ok())
    else {
        return false;
    };
    let mut new = *frame;
    for (index, reg) in new.f.iter_mut().enumerate() {
        *reg = u64::from_le_bytes(bytes[index * 8..index * 8 + 8].try_into().unwrap());
    }
    new.fcsr = u64::from_le_bytes(bytes[256..264].try_into().unwrap()) as u32;
    let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(new);
    task.ext_replace(TASKEXT_PTRACE_FRAME, erased).is_ok()
}

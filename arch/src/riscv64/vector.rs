//! RISC-V Vector 用户态上下文管理。
//!
//! 当前只支持用户态 V：内核普通代码不启用 `+v`，只有本模块的 save/restore
//! helper 使用向量指令。每个线程按需分配独立的 V 寄存器缓冲，未触碰 V 的
//! 线程保持 `VS=Off`，不影响 syscall 快路径。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use sched::{TASKEXT_RISCV_VECTOR_STATE, Task};
use spin::Mutex;

use crate::riscv64::specific::{
    SSTATUS_SPP, SSTATUS_VS_CLEAN, SSTATUS_VS_DIRTY, SSTATUS_VS_INITIAL, SSTATUS_VS_MASK, TrapFrame,
};

pub static HAS_VECTOR: AtomicBool = AtomicBool::new(false);
pub static VECTOR_VLENB: AtomicUsize = AtomicUsize::new(0);

const VECTOR_REGS: usize = 32;

/// 单线程用户态 V 上下文。
///
/// `regs` 的布局是 v0..v31 连续排列，每个寄存器占 `vlenb` 字节。
#[derive(Clone)]
pub struct UserVectorState {
    pub vlenb: usize,
    pub vl: usize,
    pub vtype: usize,
    pub vstart: usize,
    pub vcsr: usize,
    pub regs: Vec<u8>,
}

impl UserVectorState {
    fn new(vlenb: usize) -> Result<Self, ()> {
        let bytes = VECTOR_REGS.checked_mul(vlenb).ok_or(())?;
        let mut regs = Vec::new();
        regs.try_reserve_exact(bytes).map_err(|_| ())?;
        regs.resize(bytes, 0);
        Ok(Self {
            vlenb,
            vl: 0,
            vtype: 0,
            vstart: 0,
            vcsr: 0,
            regs,
        })
    }
}

pub type SharedUserVectorState = Arc<Mutex<UserVectorState>>;

pub fn has_user_vector() -> bool {
    HAS_VECTOR.load(Ordering::Acquire)
}

pub fn user_hwcap() -> usize {
    if has_user_vector() {
        // Linux RISC-V UAPI: COMPAT_HWCAP_ISA_V = 1 << ('v' - 'a').
        1usize << (b'v' - b'a')
    } else {
        0
    }
}

/// 根据所有可用 hart 的 ISA 交集探测并发布用户态 V 能力。
pub fn detect_vector_support(supported: bool) {
    if !supported {
        return;
    }

    let old = crate::read_csr!(sstatus);
    let probe_status = (old & !SSTATUS_VS_MASK) | SSTATUS_VS_INITIAL;
    crate::write_csr!(sstatus, probe_status);
    let vlenb = read_vlenb();
    crate::write_csr!(sstatus, old);

    if vlenb == 0 {
        log::warning!("[loader] ISA: V present but vlenb read as 0, disabling user V");
        return;
    }

    VECTOR_VLENB.store(vlenb, Ordering::Release);
    HAS_VECTOR.store(true, Ordering::Release);
    log::info!("[loader] ISA: V detected, vlenb={}", vlenb);
}

fn read_vlenb() -> usize {
    let value: usize;
    unsafe {
        core::arch::asm!(
            ".option push",
            ".option arch, +v",
            "csrr {value}, vlenb",
            ".option pop",
            value = out(reg) value,
            options(nomem, nostack)
        );
    }
    value
}

fn current_state() -> Option<SharedUserVectorState> {
    if !sched::is_ready() {
        return None;
    }
    let task = sched::current_task();
    task.ext_lookup(TASKEXT_RISCV_VECTOR_STATE)
        .and_then(|payload| payload.downcast::<Mutex<UserVectorState>>().ok())
}

fn current_state_or_alloc() -> Result<SharedUserVectorState, ()> {
    if !sched::is_ready() {
        return Err(());
    }
    if let Some(state) = current_state() {
        return Ok(state);
    }

    let vlenb = VECTOR_VLENB.load(Ordering::Acquire);
    if vlenb == 0 {
        return Err(());
    }
    let state = Arc::new(Mutex::new(UserVectorState::new(vlenb)?));
    sched::current_task().ext_install(TASKEXT_RISCV_VECTOR_STATE, state.clone());
    Ok(state)
}

/// 同步硬件 `sstatus.VS`，避免 TrapFrame 已净化但处理器仍允许用户访问旧 V 寄存器。
#[inline]
fn set_hardware_vs(vs: usize) {
    let status = crate::read_csr!(sstatus);
    crate::write_csr!(
        sstatus,
        (status & !SSTATUS_VS_MASK) | (vs & SSTATUS_VS_MASK)
    );
}

pub fn clone_ext_payload(src: &Arc<dyn Any + Send + Sync>) -> Arc<dyn Any + Send + Sync> {
    let state = Arc::clone(src)
        .downcast::<Mutex<UserVectorState>>()
        .expect("[riscv64][vector] vector state type mismatch");
    Arc::new(Mutex::new(state.lock().clone()))
}

pub fn clear_for_task(task: &Task) {
    let _ = task.ext_remove(TASKEXT_RISCV_VECTOR_STATE);
}

pub fn snapshot_current_for_signal(tf: &mut TrapFrame) -> Option<UserVectorState> {
    save_current_if_active(tf);
    current_state().map(|state| state.lock().clone())
}

pub fn restore_signal_snapshot(tf: &mut TrapFrame, snapshot: Option<UserVectorState>) {
    match snapshot {
        Some(state) => {
            let shared = Arc::new(Mutex::new(state));
            sched::current_task().ext_remove(TASKEXT_RISCV_VECTOR_STATE);
            sched::current_task().ext_install(TASKEXT_RISCV_VECTOR_STATE, shared);
            tf.status = (tf.status & !SSTATUS_VS_MASK) | SSTATUS_VS_CLEAN;
        }
        None => {
            sched::current_task().ext_remove(TASKEXT_RISCV_VECTOR_STATE);
            tf.status &= !SSTATUS_VS_MASK;
        }
    }
}

pub fn save_current_if_active(tf: &mut TrapFrame) {
    let saved_vs = tf.status & SSTATUS_VS_MASK;
    if saved_vs == 0 {
        return;
    }
    if let Some(state) = current_state() {
        // Clean/Initial 表示 task-local 副本仍是最新的；只有用户真正改写过
        // 向量寄存器（Dirty）时才搬运整组 v0-v31。
        if saved_vs == SSTATUS_VS_DIRTY {
            let mut guard = state.lock();
            unsafe { save_vector_state(&mut guard) };
        }
        tf.status = (tf.status & !SSTATUS_VS_MASK) | SSTATUS_VS_CLEAN;
        set_hardware_vs(SSTATUS_VS_CLEAN);
    } else {
        tf.status &= !SSTATUS_VS_MASK;
        set_hardware_vs(0);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn save_vector_from_trap_entry(tf_ptr: usize) {
    let tf = unsafe { &mut *(tf_ptr as *mut TrapFrame) };
    if (tf.status & SSTATUS_SPP) != 0 {
        return;
    }
    save_current_if_active(tf);
}

pub fn restore_current_if_active(tf: &mut TrapFrame) {
    if (tf.status & SSTATUS_VS_MASK) == 0 {
        return;
    }
    if let Some(state) = current_state() {
        let guard = state.lock();
        unsafe { restore_vector_state(&guard) };
        tf.status = (tf.status & !SSTATUS_VS_MASK) | SSTATUS_VS_CLEAN;
        // restore 指令会把硬件 VS 标成 Dirty；状态已经完整落盘，返回用户前
        // 改回 Clean，使下一次未使用 Vector 的 trap 可以跳过整组保存。
        set_hardware_vs(SSTATUS_VS_CLEAN);
    } else {
        tf.status &= !SSTATUS_VS_MASK;
        // 用户可通过当前 raw signal ABI 伪造 VS bits，但没有配套的 task-local
        // vector state 时绝不能带着旧硬件寄存器返回 U-mode。
        set_hardware_vs(0);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn restore_vector_from_resume(tf_ptr: usize) {
    let tf = unsafe { &mut *(tf_ptr as *mut TrapFrame) };
    if (tf.status & SSTATUS_SPP) != 0 {
        return;
    }
    restore_current_if_active(tf);
}

pub fn enable_user_vector_if_needed(tf: &mut TrapFrame) -> bool {
    if !has_user_vector() || (tf.status & SSTATUS_VS_MASK) != 0 {
        return false;
    }
    if !looks_like_vector_instruction(tf.sepc) {
        return false;
    }
    if current_state_or_alloc().is_err() {
        return false;
    }
    tf.status = (tf.status & !SSTATUS_VS_MASK) | SSTATUS_VS_INITIAL;
    true
}

fn looks_like_vector_instruction(pc: usize) -> bool {
    let mut lo = [0u8; 2];
    if crate::riscv64::mm::user_copy::copy_instruction_from_user(pc, &mut lo).is_err() {
        return false;
    }
    let half = u16::from_le_bytes(lo);
    if half & 0x3 != 0x3 {
        return false;
    }

    let mut raw = [0u8; 4];
    raw[..2].copy_from_slice(&lo);
    if crate::riscv64::mm::user_copy::copy_instruction_from_user(pc.wrapping_add(2), &mut raw[2..])
        .is_err()
    {
        return false;
    }
    let insn = u32::from_le_bytes(raw);
    let opcode = insn & 0x7f;
    if opcode == 0x57 {
        return true;
    }

    // LOAD-FP/STORE-FP opcode 同时承载 F/D 与 V load/store。Vector memory
    // 指令要求 width(funct3) 不为 0b010/0b011 的标量 FLW/FLD/FSW/FSD。
    if opcode == 0x07 || opcode == 0x27 {
        let width = (insn >> 12) & 0x7;
        return !matches!(width, 0b010 | 0b011);
    }

    false
}

unsafe fn save_vector_state(state: &mut UserVectorState) {
    let base = state.regs.as_mut_ptr() as usize;
    let stride = state.vlenb;
    let vl: usize;
    let vtype: usize;
    let vstart: usize;
    let vcsr: usize;
    unsafe {
        core::arch::asm!(
            ".option push",
            ".option arch, +v",
            "csrr {vl}, vl",
            "csrr {vtype}, vtype",
            "csrr {vstart}, vstart",
            "csrr {vcsr}, vcsr",
            "csrw vstart, x0",
            "mv t0, {base}",
            "mv t1, {stride}",
            "vs1r.v v0, (t0)",  "add t0, t0, t1",
            "vs1r.v v1, (t0)",  "add t0, t0, t1",
            "vs1r.v v2, (t0)",  "add t0, t0, t1",
            "vs1r.v v3, (t0)",  "add t0, t0, t1",
            "vs1r.v v4, (t0)",  "add t0, t0, t1",
            "vs1r.v v5, (t0)",  "add t0, t0, t1",
            "vs1r.v v6, (t0)",  "add t0, t0, t1",
            "vs1r.v v7, (t0)",  "add t0, t0, t1",
            "vs1r.v v8, (t0)",  "add t0, t0, t1",
            "vs1r.v v9, (t0)",  "add t0, t0, t1",
            "vs1r.v v10, (t0)", "add t0, t0, t1",
            "vs1r.v v11, (t0)", "add t0, t0, t1",
            "vs1r.v v12, (t0)", "add t0, t0, t1",
            "vs1r.v v13, (t0)", "add t0, t0, t1",
            "vs1r.v v14, (t0)", "add t0, t0, t1",
            "vs1r.v v15, (t0)", "add t0, t0, t1",
            "vs1r.v v16, (t0)", "add t0, t0, t1",
            "vs1r.v v17, (t0)", "add t0, t0, t1",
            "vs1r.v v18, (t0)", "add t0, t0, t1",
            "vs1r.v v19, (t0)", "add t0, t0, t1",
            "vs1r.v v20, (t0)", "add t0, t0, t1",
            "vs1r.v v21, (t0)", "add t0, t0, t1",
            "vs1r.v v22, (t0)", "add t0, t0, t1",
            "vs1r.v v23, (t0)", "add t0, t0, t1",
            "vs1r.v v24, (t0)", "add t0, t0, t1",
            "vs1r.v v25, (t0)", "add t0, t0, t1",
            "vs1r.v v26, (t0)", "add t0, t0, t1",
            "vs1r.v v27, (t0)", "add t0, t0, t1",
            "vs1r.v v28, (t0)", "add t0, t0, t1",
            "vs1r.v v29, (t0)", "add t0, t0, t1",
            "vs1r.v v30, (t0)", "add t0, t0, t1",
            "vs1r.v v31, (t0)",
            ".option pop",
            base = in(reg) base,
            stride = in(reg) stride,
            vl = out(reg) vl,
            vtype = out(reg) vtype,
            vstart = out(reg) vstart,
            vcsr = out(reg) vcsr,
            out("t0") _,
            out("t1") _,
            options(nostack)
        );
    }
    state.vl = vl;
    state.vtype = vtype;
    state.vstart = vstart;
    state.vcsr = vcsr;
}

unsafe fn restore_vector_state(state: &UserVectorState) {
    let base = state.regs.as_ptr() as usize;
    let stride = state.vlenb;
    unsafe {
        core::arch::asm!(
            ".option push",
            ".option arch, +v",
            "vsetvl x0, {vl}, {vtype}",
            "csrw vcsr, {vcsr}",
            "csrw vstart, x0",
            "mv t0, {base}",
            "mv t1, {stride}",
            "vl1re8.v v0, (t0)",  "add t0, t0, t1",
            "vl1re8.v v1, (t0)",  "add t0, t0, t1",
            "vl1re8.v v2, (t0)",  "add t0, t0, t1",
            "vl1re8.v v3, (t0)",  "add t0, t0, t1",
            "vl1re8.v v4, (t0)",  "add t0, t0, t1",
            "vl1re8.v v5, (t0)",  "add t0, t0, t1",
            "vl1re8.v v6, (t0)",  "add t0, t0, t1",
            "vl1re8.v v7, (t0)",  "add t0, t0, t1",
            "vl1re8.v v8, (t0)",  "add t0, t0, t1",
            "vl1re8.v v9, (t0)",  "add t0, t0, t1",
            "vl1re8.v v10, (t0)", "add t0, t0, t1",
            "vl1re8.v v11, (t0)", "add t0, t0, t1",
            "vl1re8.v v12, (t0)", "add t0, t0, t1",
            "vl1re8.v v13, (t0)", "add t0, t0, t1",
            "vl1re8.v v14, (t0)", "add t0, t0, t1",
            "vl1re8.v v15, (t0)", "add t0, t0, t1",
            "vl1re8.v v16, (t0)", "add t0, t0, t1",
            "vl1re8.v v17, (t0)", "add t0, t0, t1",
            "vl1re8.v v18, (t0)", "add t0, t0, t1",
            "vl1re8.v v19, (t0)", "add t0, t0, t1",
            "vl1re8.v v20, (t0)", "add t0, t0, t1",
            "vl1re8.v v21, (t0)", "add t0, t0, t1",
            "vl1re8.v v22, (t0)", "add t0, t0, t1",
            "vl1re8.v v23, (t0)", "add t0, t0, t1",
            "vl1re8.v v24, (t0)", "add t0, t0, t1",
            "vl1re8.v v25, (t0)", "add t0, t0, t1",
            "vl1re8.v v26, (t0)", "add t0, t0, t1",
            "vl1re8.v v27, (t0)", "add t0, t0, t1",
            "vl1re8.v v28, (t0)", "add t0, t0, t1",
            "vl1re8.v v29, (t0)", "add t0, t0, t1",
            "vl1re8.v v30, (t0)", "add t0, t0, t1",
            "vl1re8.v v31, (t0)",
            "csrw vstart, {vstart}",
            ".option pop",
            base = in(reg) base,
            stride = in(reg) stride,
            vl = in(reg) state.vl,
            vtype = in(reg) state.vtype,
            vstart = in(reg) state.vstart,
            vcsr = in(reg) state.vcsr,
            out("t0") _,
            out("t1") _,
            options(nostack)
        );
    }
}

//! x86_64 用户 xstate 所有权与信号快照。
//!
//! The scheduler context keeps the kernel's legacy FXSAVE image.  User mode
//! state is separate: an extended XSAVE image is installed lazily for a task
//! only after the boot code explicitly enables an extended XCR0 policy.  This
//! keeps the default (x87/SSE) path small while giving trap and signal code a
//! single owner for AVX-family state.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
#[cfg(any(target_os = "none", test))]
use core::cell::UnsafeCell;
#[cfg(any(target_os = "none", test))]
use core::sync::atomic::{AtomicBool, Ordering};

use sched::{Task, TaskExtKey};
use spin::Mutex;

use super::fpu;
use super::trap_frame::{FXSAVE_SIZE, TrapFrame};

/// Architecture-private task extension keys.  The range is reserved for x86
/// in the scheduler extension namespace; it intentionally does not require a
/// scheduler crate change for other architectures.
pub const TASKEXT_X86_XSTATE: TaskExtKey = 0x0001_0007;
pub const TASKEXT_X86_XSTATE_SIGNAL_STACK: TaskExtKey = 0x0001_0008;

const CHUNK_SIZE: usize = 64;
pub(crate) const EARLY_CAPTURE_NONE: usize = 0;
pub(crate) const EARLY_CAPTURE_KERNEL: usize = 1;
pub(crate) const EARLY_CAPTURE_OWNED: usize = 2;

#[cfg(any(target_os = "none", test))]
#[repr(C, align(64))]
struct EarlyXStateSlot {
    // XSAVE requires a 64-byte aligned destination.  This is first so the
    // enclosing slot's alignment directly applies to the image.
    image: UnsafeCell<[u8; fpu::MAX_XSAVE_SIZE]>,
    valid: AtomicBool,
    busy: AtomicBool,
}

// The image is only accessed by the CPU that acquired `busy`; the atomic
// release/acquire edges publish it to that CPU's Rust return path.
#[cfg(any(target_os = "none", test))]
unsafe impl Sync for EarlyXStateSlot {}

#[cfg(any(target_os = "none", test))]
impl EarlyXStateSlot {
    const fn new() -> Self {
        Self {
            image: UnsafeCell::new([0; fpu::MAX_XSAVE_SIZE]),
            valid: AtomicBool::new(false),
            busy: AtomicBool::new(false),
        }
    }
}

/// One aligned early-entry xstate image per logical CPU.  NMI/nested entries
/// on the same CPU fail closed while its slot is busy; simultaneous entries on
/// distinct CPUs never share mutable xstate storage.
#[cfg(target_os = "none")]
static EARLY_XSTATE_SLOTS: [EarlyXStateSlot; super::smp::MAX_CPUS] =
    [const { EarlyXStateSlot::new() }; super::smp::MAX_CPUS];

#[cfg(target_os = "none")]
#[inline]
fn current_early_slot() -> Option<&'static EarlyXStateSlot> {
    EARLY_XSTATE_SLOTS.get(super::smp::current_cpu_id())
}

/// Capture all components currently enabled in XCR0 before entering Rust.
///
/// The function is called from the naked trap entry after all GPRs have been
/// spilled.  It returns `false` if an extended policy was requested without a
/// usable XSAVE implementation; the assembly path then halts rather than
/// allowing one task's vector state to leak into another.
#[cfg(target_os = "none")]
#[inline(never)]
pub unsafe extern "C" fn capture_early(from_user: usize) -> usize {
    // Do not call `fpu::init()` here: this function runs from naked entry
    // before the interrupted user's vector state has been copied.  Scheduler
    // registration publishes the mask and XSAVE-ready flag before IDT setup;
    // an unset flag therefore means the safe legacy path is still active.
    let mask = fpu::enabled_mask_raw();
    if mask & !fpu::XFEATURE_BASE == 0 {
        return EARLY_CAPTURE_KERNEL;
    }
    if !fpu::xsave_enabled() {
        return EARLY_CAPTURE_NONE;
    }
    // The freestanding kernel is compiled without AVX/AVX-512.  Kernel-only
    // entries therefore need the fixed FXSAVE frame but must leave an outer
    // user snapshot untouched when an NMI nests on the entry path.
    if from_user == 0 {
        return EARLY_CAPTURE_KERNEL;
    }
    let Some(slot) = current_early_slot() else {
        return EARLY_CAPTURE_NONE;
    };
    if slot
        .busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // A second entry cannot safely share the BSP scratch area.  Returning
        // false makes the assembly halt with interrupts disabled.
        return EARLY_CAPTURE_NONE;
    }
    slot.valid.store(false, Ordering::Release);
    let area = slot.image.get() as *mut u8;
    // XSAVE is permitted to omit components in architectural init state.  A
    // reused scratch image must therefore be cleared with integer-only code
    // before the instruction, otherwise an NT_X86_XSTATE read could disclose
    // bytes left by the previous task on this CPU.
    let mut clear_dst = area;
    let mut clear_qwords = fpu::MAX_XSAVE_SIZE / core::mem::size_of::<u64>();
    unsafe {
        core::arch::asm!(
            "cld",
            "rep stosq",
            inout("rdi") clear_dst => _,
            inout("rcx") clear_qwords => _,
            in("rax") 0usize,
            options(nostack)
        );
    }
    // Pass the exact policy mask selected by boot.  XSAVE treats EDX:EAX as
    // the requested component bitmap; all-ones is not a portable substitute
    // because XCR0 may deliberately leave implementation-specific components
    // disabled (and requesting one can raise #GP on some processors).
    let mask_low = mask as u32;
    let mask_high = (mask >> 32) as u32;
    unsafe {
        core::arch::asm!(
            "xsave64 [{area}]",
            area = in(reg) area,
            in("eax") mask_low,
            in("edx") mask_high,
            options(nostack)
        );
    }
    slot.valid.store(true, Ordering::Release);
    EARLY_CAPTURE_OWNED
}

#[cfg(not(target_os = "none"))]
#[inline]
pub unsafe extern "C" fn capture_early(_from_user: usize) -> usize {
    EARLY_CAPTURE_KERNEL
}

/// Drop an early image that will not be consumed by a user-return path.
///
/// A nested entry cannot reach this helper: `capture_early` claims the single
/// BSP slot first and returns false to the assembly path when it is busy.  It
/// is therefore safe for the owning kernel-trap path to release both tokens.
pub(crate) fn discard_early() {
    #[cfg(target_os = "none")]
    {
        if let Some(slot) = current_early_slot() {
            slot.valid.store(false, Ordering::Release);
            slot.busy.store(false, Ordering::Release);
        }
    }
}

#[cfg(target_os = "none")]
fn copy_early_image(dst: &mut [u8]) -> bool {
    let Some(slot) = current_early_slot() else {
        return false;
    };
    if dst.len() > fpu::MAX_XSAVE_SIZE || !slot.valid.swap(false, Ordering::AcqRel) {
        slot.busy.store(false, Ordering::Release);
        return false;
    }
    let ptr = slot.image.get() as *const u8;
    // Safety: capture_early wrote a complete, bounded image to this aligned
    // static and ownership is serialized by the busy/valid flags.
    unsafe { core::ptr::copy_nonoverlapping(ptr, dst.as_mut_ptr(), dst.len()) };
    slot.busy.store(false, Ordering::Release);
    true
}

/// A chunk keeps the backing allocation 64-byte aligned, as required by
/// XSAVE/XRSTOR.  The final chunk may contain padding beyond `UserXState::len`.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct XStateChunk([u8; CHUNK_SIZE]);

/// Owned, standard-layout xstate image for one user task.
#[derive(Clone)]
pub struct UserXState {
    /// XCR0 mask used by XSAVE/XRSTOR for this image.
    pub mask: u64,
    /// Valid bytes in `chunks`; padding is never exposed to userspace.
    pub len: usize,
    chunks: Vec<XStateChunk>,
}

impl UserXState {
    /// Allocate an image for an already-enabled policy.
    pub fn new(mask: u64) -> Result<Self, ()> {
        let features = *fpu::init();
        let enabled = fpu::enabled_mask();
        if mask == 0
            || mask & !enabled != 0
            || mask & fpu::XFEATURE_BASE != fpu::XFEATURE_BASE
            || (mask & fpu::XFEATURE_AVX512 != 0
                && mask & fpu::XFEATURE_AVX512 != fpu::XFEATURE_AVX512)
            || (mask & fpu::XFEATURE_AVX512 != 0 && mask & fpu::XFEATURE_YMM == 0)
        {
            return Err(());
        }
        let len = features.size_for_mask(mask).max(FXSAVE_SIZE);
        if len > fpu::MAX_XSAVE_SIZE {
            return Err(());
        }
        let count = len.checked_add(CHUNK_SIZE - 1).ok_or(())? / CHUNK_SIZE;
        let mut chunks = Vec::new();
        chunks.try_reserve_exact(count).map_err(|_| ())?;
        chunks.resize(count, XStateChunk([0; CHUNK_SIZE]));
        let mut state = Self { mask, len, chunks };
        state
            .legacy_mut()
            .copy_from_slice(&TrapFrame::initial_fxsave());
        // XRSTOR initializes components whose XSTATE_BV bit is clear.  Mark
        // the legacy pair in use so a freshly-created extended task restores
        // the initialized FXSAVE values rather than silently discarding them.
        if state.len >= FXSAVE_SIZE + 8 {
            state.bytes_mut()[FXSAVE_SIZE..FXSAVE_SIZE + 8]
                .copy_from_slice(&fpu::XFEATURE_BASE.to_le_bytes());
        }
        Ok(state)
    }

    #[inline]
    fn bytes(&self) -> &[u8] {
        // Safety: chunks is a contiguous array of `XStateChunk`; `len` is
        // bounded by the allocation made in `new` and never changes.
        unsafe { core::slice::from_raw_parts(self.chunks.as_ptr() as *const u8, self.len) }
    }

    #[inline]
    fn bytes_mut(&mut self) -> &mut [u8] {
        // Safety: see `bytes`; this is the sole mutable view of the image.
        unsafe { core::slice::from_raw_parts_mut(self.chunks.as_mut_ptr() as *mut u8, self.len) }
    }

    #[inline]
    fn legacy(&self) -> &[u8] {
        &self.bytes()[..FXSAVE_SIZE]
    }

    #[inline]
    fn legacy_mut(&mut self) -> &mut [u8] {
        &mut self.bytes_mut()[..FXSAVE_SIZE]
    }

    #[inline]
    fn as_ptr(&self) -> *const u8 {
        self.chunks.as_ptr() as *const u8
    }

    #[inline]
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.chunks.as_mut_ptr() as *mut u8
    }

    fn copy_legacy_from_trap(&mut self, frame: &TrapFrame) {
        self.legacy_mut().copy_from_slice(&frame.fxsave);
    }

    fn copy_legacy_to_trap(&self, frame: &mut TrapFrame) {
        frame.fxsave.copy_from_slice(self.legacy());
    }

    #[inline]
    fn image(&self) -> &[u8] {
        self.bytes()
    }

    #[inline]
    fn replace_image(&mut self, image: &[u8]) -> bool {
        if image.len() != self.len {
            return false;
        }
        self.bytes_mut().copy_from_slice(image);
        true
    }

    /// Capture the processor image.  The caller must invoke this at the trap
    /// entry boundary before executing code that can modify vector registers.
    pub unsafe fn save_hardware(&mut self) {
        // Safety: the chunk allocation is 64-byte aligned and `mask` is a
        // subset of the enabled XCR0 policy established at boot.
        unsafe { fpu::save(self.as_mut_ptr(), self.mask) };
    }

    /// Restore the processor image immediately before returning to user mode.
    pub unsafe fn restore_hardware(&self) {
        // Safety: see `save_hardware`; the image was validated when installed.
        unsafe { fpu::restore(self.as_ptr(), self.mask) };
    }
}

pub type SharedUserXState = Arc<Mutex<UserXState>>;
type UserXStateSignalStack = Mutex<Vec<Option<UserXState>>>;

#[inline]
fn state_for_task(task: &Task) -> Option<SharedUserXState> {
    task.ext_lookup(TASKEXT_X86_XSTATE)
        .and_then(|payload| payload.downcast::<Mutex<UserXState>>().ok())
}

fn ensure_state(task: &Task) -> Result<Option<SharedUserXState>, ()> {
    let mask = fpu::enabled_mask();
    // The baseline FXSAVE image lives in TrapFrame; do not allocate a second
    // 512-byte owner unless an extended component was explicitly enabled.
    if mask & !fpu::XFEATURE_BASE == 0 {
        return Ok(None);
    }
    if let Some(state) = state_for_task(task) {
        return Ok(Some(state));
    }
    let state = Arc::new(Mutex::new(UserXState::new(mask)?));
    task.ext_install(TASKEXT_X86_XSTATE, state.clone());
    Ok(Some(state))
}

fn signal_stack(task: &Task) -> Result<Arc<UserXStateSignalStack>, ()> {
    if let Some(stack) = task
        .ext_lookup(TASKEXT_X86_XSTATE_SIGNAL_STACK)
        .and_then(|payload| payload.downcast::<UserXStateSignalStack>().ok())
    {
        return Ok(stack);
    }
    let stack = Arc::new(UserXStateSignalStack::new(Vec::new()));
    task.ext_install(TASKEXT_X86_XSTATE_SIGNAL_STACK, stack.clone());
    Ok(stack)
}

fn trap_frame(context: usize) -> Option<&'static TrapFrame> {
    if context == 0 || context % core::mem::align_of::<TrapFrame>() != 0 {
        return None;
    }
    // Safety: the HAL contract passes a live, correctly aligned TrapFrame.
    Some(unsafe { &*(context as *const TrapFrame) })
}

fn trap_frame_mut(context: usize) -> Option<&'static mut TrapFrame> {
    if context == 0 || context % core::mem::align_of::<TrapFrame>() != 0 {
        return None;
    }
    // Safety: the HAL contract gives exclusive access while a signal frame is
    // being assembled/restored.
    Some(unsafe { &mut *(context as *mut TrapFrame) })
}

/// Fork hook for x86-owned task extensions.
pub fn clone_task_extension(
    key: TaskExtKey,
    source: &Arc<dyn Any + Send + Sync>,
) -> Option<Arc<dyn Any + Send + Sync>> {
    match key {
        TASKEXT_X86_XSTATE => {
            let source = Arc::clone(source).downcast::<Mutex<UserXState>>().ok()?;
            Some(Arc::new(Mutex::new(source.lock().clone())))
        }
        TASKEXT_X86_XSTATE_SIGNAL_STACK => Some(Arc::new(UserXStateSignalStack::new(Vec::new()))),
        _ => None,
    }
}

/// Drop all x86-owned user state on exec/exit.
pub fn clear_for_task(task: &Task) {
    let _ = task.ext_remove(TASKEXT_X86_XSTATE);
    let _ = task.ext_remove(TASKEXT_X86_XSTATE_SIGNAL_STACK);
}

/// Save an extended image from the earliest trap-entry hook.
pub fn save_from_trap_entry(task: &Task, context: usize) -> Result<(), ()> {
    let Some(state) = ensure_state(task)? else {
        return Ok(());
    };
    let Some(_frame) = trap_frame(context) else {
        return Err(());
    };
    let mut state = state.lock();
    // The early entry hook captured the processor before any Rust prologue or
    // dispatcher could use vector registers.  Never fall back to a late XSAVE:
    // that would save kernel work instead of the interrupted user's state.
    #[cfg(target_os = "none")]
    {
        if !copy_early_image(state.bytes_mut()) {
            return Err(());
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        // Hosted code never enables an extended policy, but keep the function
        // deterministic for unit tests and synthetic callers.
        unsafe { state.save_hardware() };
    }
    // FXSAVE may report a wider implementation mask than the user ABI
    // exposes.  Canonicalize the legacy prefix before mirroring it into the
    // trap frame or retaining it in the task owner.
    if !fpu::sanitize_fxsave_area(state.legacy_mut()) {
        return Err(());
    }
    // The hardware image is authoritative for extended components.  Keep the
    // legacy half mirrored in the trap frame so ptrace/signal code sees one
    // consistent FXSAVE view.
    if let Some(frame) = trap_frame_mut(context) {
        state.copy_legacy_to_trap(frame);
    }
    Ok(())
}

/// Restore x86-owned state at the final user-return boundary.
pub fn restore_for_resume(task: &Task, context: usize) -> Result<(), ()> {
    // An extended XCR0 policy is global, so every user return must restore a
    // complete image.  Exec/exit cleanup may have removed the owner while the
    // task is still current; lazily install a fresh, zeroed image instead of
    // letting the previous task's YMM/ZMM state remain live in the CPU.
    let Some(state) = ensure_state(task)? else {
        return Ok(());
    };
    let Some(frame) = trap_frame_mut(context) else {
        return Err(());
    };
    let state = state.lock();
    state.copy_legacy_to_trap(frame);
    // This hook is deliberately explicit; callers must invoke it immediately
    // before the iret/sysret assembly so kernel code cannot modify restored
    // registers afterward.
    unsafe { state.restore_hardware() };
    Ok(())
}

/// Save a signal snapshot.  Legacy state is copied from the trap frame; an
/// extended owner, when present, is cloned without sharing mutable storage.
pub fn push_signal_snapshot(task: &Arc<Task>, context: usize) -> Result<(), ()> {
    let Some(frame) = trap_frame(context) else {
        return Err(());
    };
    let snapshot = if let Some(state) = ensure_state(task.as_ref())? {
        let mut guard = state.lock();
        guard.copy_legacy_from_trap(frame);
        Some(guard.clone())
    } else {
        None
    };
    let stack = signal_stack(task.as_ref())?;
    let mut stack = stack.lock();
    stack.try_reserve(1).map_err(|_| ())?;
    stack.push(snapshot);
    Ok(())
}

/// Restore and consume the oldest signal snapshot (nested signals are LIFO).
pub fn pop_signal_snapshot(task: &Arc<Task>, context: usize) {
    let Some(stack) = task
        .ext_lookup(TASKEXT_X86_XSTATE_SIGNAL_STACK)
        .and_then(|payload| payload.downcast::<UserXStateSignalStack>().ok())
    else {
        return;
    };
    let snapshot = stack.lock().pop();
    if snapshot.is_none() {
        let _ = task.ext_remove(TASKEXT_X86_XSTATE_SIGNAL_STACK);
    }
    match snapshot.flatten() {
        Some(state) => {
            if let Some(current) = state_for_task(task) {
                *current.lock() = state;
            } else {
                task.ext_install(TASKEXT_X86_XSTATE, Arc::new(Mutex::new(state)));
            }
            if let (Some(frame), Some(current)) = (trap_frame_mut(context), state_for_task(task)) {
                current.lock().copy_legacy_to_trap(frame);
            }
        }
        None => {
            // A task that had no extended owner before signal delivery must not
            // inherit state lazily created by a signal handler.
            let _ = task.ext_remove(TASKEXT_X86_XSTATE);
        }
    }
}

/// Replace the post-pop signal snapshot with the fpstate supplied by userspace.
pub(crate) fn restore_signal_image(task: &Task, context: usize, bytes: Option<&[u8]>) -> bool {
    let Some(frame) = trap_frame_mut(context) else {
        return false;
    };
    let enabled = fpu::enabled_mask();
    let mut state = if enabled & !fpu::XFEATURE_BASE != 0 {
        match UserXState::new(enabled) {
            Ok(state) => Some(state),
            Err(()) => return false,
        }
    } else {
        None
    };

    let mut legacy = TrapFrame::initial_fxsave();
    if let Some(image) = bytes {
        if image.len() < FXSAVE_SIZE {
            return false;
        }
        legacy.copy_from_slice(&image[..FXSAVE_SIZE]);
        legacy[super::ptrace::FXSAVE_SW_BYTES_OFFSET..FXSAVE_SIZE].fill(0);
        if !fpu::sanitize_fxsave_area(&mut legacy) {
            return false;
        }

        if let Some(ref mut owner) = state {
            let extended = super::ptrace::linux_signal_xstate_encoded_size(image)
                .is_some_and(|size| size > FXSAVE_SIZE);
            if extended {
                if image.len() < owner.len {
                    return false;
                }
                let owner_len = owner.len;
                owner.bytes_mut().copy_from_slice(&image[..owner_len]);
                super::ptrace::sanitize_absent_xsave_components(owner.bytes_mut());
                owner.bytes_mut()[super::ptrace::FXSAVE_SW_BYTES_OFFSET..FXSAVE_SIZE].fill(0);
                owner.legacy_mut().copy_from_slice(&legacy);
            } else {
                owner.legacy_mut().copy_from_slice(&legacy);
            }
        }
    } else if let Some(ref mut owner) = state {
        owner.legacy_mut().copy_from_slice(&legacy);
    }

    frame.fxsave = legacy;
    match state {
        Some(state) => {
            let shared = Arc::new(Mutex::new(state));
            if task
                .ext_replace(TASKEXT_X86_XSTATE, shared.clone())
                .is_err()
            {
                task.ext_install(TASKEXT_X86_XSTATE, shared);
            }
        }
        None => {
            let _ = task.ext_remove(TASKEXT_X86_XSTATE);
        }
    }
    true
}

/// Return whether a task currently owns extended xstate.
pub fn has_extended_state(task: &Task) -> bool {
    state_for_task(task).is_some()
}

/// Return the XCR0 mask associated with an owned image.
pub fn state_mask(task: &Task) -> Option<u64> {
    state_for_task(task).map(|state| state.lock().mask)
}

/// Copy an owned standard-layout image for ptrace/`NT_X86_XSTATE`.
pub fn read_image(task: &Task) -> Option<Vec<u8>> {
    state_for_task(task).map(|state| state.lock().image().to_vec())
}

/// Replace an owned standard-layout image after the ptrace validator has run.
pub fn write_image(task: &Task, image: &[u8]) -> bool {
    let Some(state) = state_for_task(task) else {
        return false;
    };
    state.lock().replace_image(image)
}

/// Read only the legacy FXSAVE portion of an owned image.
pub fn read_legacy(task: &Task) -> Option<Vec<u8>> {
    state_for_task(task).map(|state| state.lock().legacy().to_vec())
}

/// Update only the legacy FXSAVE portion of an owned image.
pub fn write_legacy(task: &Task, legacy: &[u8]) -> bool {
    if legacy.len() < FXSAVE_SIZE {
        return false;
    }
    let Some(state) = state_for_task(task) else {
        return false;
    };
    state
        .lock()
        .legacy_mut()
        .copy_from_slice(&legacy[..FXSAVE_SIZE]);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_image_is_aligned_and_initialized() {
        let mask = fpu::enabled_mask();
        let state = UserXState::new(mask).expect("x86 baseline xstate must allocate");
        assert_eq!((state.as_ptr() as usize) % fpu::XSAVE_ALIGNMENT, 0);
        assert!(state.len >= FXSAVE_SIZE);
        assert_eq!(
            u16::from_le_bytes([state.legacy()[0], state.legacy()[1]]),
            0x037f
        );
        assert_eq!(
            u32::from_le_bytes([
                state.legacy()[24],
                state.legacy()[25],
                state.legacy()[26],
                state.legacy()[27],
            ]),
            0x1f80
        );
    }

    #[test]
    fn early_slot_is_xsave_aligned_and_starts_idle() {
        let slot = EarlyXStateSlot::new();
        assert_eq!((slot.image.get() as usize) % fpu::XSAVE_ALIGNMENT, 0);
        assert!(!slot.valid.load(Ordering::Acquire));
        assert!(!slot.busy.load(Ordering::Acquire));
    }
}

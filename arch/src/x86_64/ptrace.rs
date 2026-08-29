//! x86_64 Linux ptrace 寄存器 ABI。

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use sched::{TASKEXT_PTRACE_FRAME, Task};

use super::trap_frame::{FXSAVE_SIZE, TrapFrame};

/// Architecture-private marker for a ptrace snapshot changed by the tracer.
///
/// The live frame is stack-resident in the interrupted kernel path, so ptrace
/// writes update `TASKEXT_PTRACE_FRAME` and set this marker.  The x86 return
/// hook consumes it immediately before `iretq`; an unmodified entry snapshot
/// must never overwrite syscall results produced after capture.
pub(crate) const TASKEXT_X86_PTRACE_FRAME_DIRTY: sched::TaskExtKey = 0x0001_0009;

/// `NT_FPREGSET`/`PTRACE_GETFPREGS` 的 legacy FXSAVE 大小。
pub const LINUX_FPREGSET_SIZE: usize = FXSAVE_SIZE;
/// `NT_X86_XSTATE` 的 software-reserved magic（信号帧 UAPI）。
pub const FP_XSTATE_MAGIC1: u32 = 0x4650_5853;
pub const FP_XSTATE_MAGIC2: u32 = 0x4650_5845;
pub const FP_XSTATE_MAGIC2_SIZE: usize = core::mem::size_of::<u32>();
/// FXSAVE software-reserved bytes used by Linux's `_fpx_sw_bytes`.
pub const FXSAVE_SW_BYTES_OFFSET: usize = 464;
pub const FXSAVE_SW_BYTES_SIZE: usize = 48;
/// Standard (non-compacted) XSAVE header location and size.
pub const XSAVE_HEADER_OFFSET: usize = FXSAVE_SIZE;
pub const XSAVE_HEADER_SIZE: usize = 64;
pub const XSAVE_MIN_SIZE: usize = XSAVE_HEADER_OFFSET + XSAVE_HEADER_SIZE;
/// `NT_X86_XSTATE` software word containing the OS-enabled XCR0 mask.
pub const XSTATE_SW_XCR0_OFFSET: usize = FXSAVE_SW_BYTES_OFFSET;
/// Header fields in a standard XSAVE image.
pub const XSTATE_HEADER_XFEATURES_OFFSET: usize = XSAVE_HEADER_OFFSET;
pub const XSTATE_HEADER_XCOMP_BV_OFFSET: usize = XSAVE_HEADER_OFFSET + 8;
pub const LINUX_SIGNAL_XSTATE_MAX_SIZE: usize = super::fpu::MAX_XSAVE_SIZE + FP_XSTATE_MAGIC2_SIZE;
/// x86 软件断点（`int3`）指令。
pub const BREAKPOINT_INSN: u32 = 0xCC;

/// Clear standard-layout XSAVE components that are not marked live in
/// `XSTATE_BV`.  XSAVE is allowed to leave such bytes untouched, so copying
/// an owned image verbatim could disclose a previous task's vector state.
pub(crate) fn sanitize_absent_xsave_components(image: &mut [u8]) {
    if image.len() < XSAVE_MIN_SIZE {
        return;
    }
    let features = read_u64(image, XSTATE_HEADER_XFEATURES_OFFSET).unwrap_or(0);
    for feature in [2u32, 3, 4, 5, 6, 7] {
        let bit = 1u64 << feature;
        if features & bit != 0 {
            continue;
        }
        let Some((offset, size)) = super::fpu::xsave_component_range(feature) else {
            continue;
        };
        let Some(end) = offset.checked_add(size) else {
            continue;
        };
        if end <= image.len() {
            image[offset..end].fill(0);
        }
    }
}

pub fn task_frame(task: &Task) -> Option<TrapFrame> {
    task.ext_lookup(TASKEXT_PTRACE_FRAME)
        .and_then(|payload| payload.downcast::<TrapFrame>().ok())
        .map(|frame| *frame)
}

fn replace_frame(task: &Task, frame: TrapFrame) -> bool {
    let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(frame);
    task.ext_replace(TASKEXT_PTRACE_FRAME, erased).is_ok()
}

fn mark_frame_dirty(task: &Task) {
    let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(true);
    if task
        .ext_replace(TASKEXT_X86_PTRACE_FRAME_DIRTY, erased)
        .is_err()
    {
        let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(true);
        task.ext_install(TASKEXT_X86_PTRACE_FRAME_DIRTY, erased);
    }
}

fn take_frame_dirty(task: &Task) -> bool {
    task.ext_remove(TASKEXT_X86_PTRACE_FRAME_DIRTY)
        .and_then(|payload| payload.downcast::<bool>().ok())
        .is_some_and(|dirty| *dirty)
}

/// Publish a current user trap frame before generic code can enter an
/// observable ptrace stop.
pub(crate) fn publish_trap_frame(task: &Task, frame: TrapFrame) {
    let _ = task.ext_remove(TASKEXT_X86_PTRACE_FRAME_DIRTY);
    let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(frame);
    if task.ext_replace(TASKEXT_PTRACE_FRAME, erased).is_err() {
        let erased: Arc<dyn core::any::Any + Send + Sync> = Arc::new(frame);
        task.ext_install(TASKEXT_PTRACE_FRAME, erased);
    }
}

/// Store a tracer-modified general-register snapshot for the active x86 stop.
///
/// The caller has already validated the Linux register ABI.  Returning false
/// means there is no active x86 user-stop frame to receive the update.
pub fn store_task_frame(task: &Task, frame: TrapFrame) -> bool {
    if !replace_frame(task, frame) {
        return false;
    }
    mark_frame_dirty(task);
    true
}

/// Complete one user trap return.  Only tracer-written snapshots are copied
/// back into the live frame; the transient snapshot is then removed so a later
/// asynchronous stop cannot expose stale registers.
pub(crate) fn finish_trap_frame(task: &Task, live: &mut TrapFrame) {
    let dirty = take_frame_dirty(task);
    let snapshot = task
        .ext_remove(TASKEXT_PTRACE_FRAME)
        .and_then(|payload| payload.downcast::<TrapFrame>().ok());
    merge_tracer_frame(live, snapshot.map(|frame| *frame), dirty);
}

fn merge_tracer_frame(live: &mut TrapFrame, snapshot: Option<TrapFrame>, dirty: bool) {
    if dirty && let Some(snapshot) = snapshot {
        *live = snapshot;
    }
}

pub fn read_linux_fpregs(task: &Task) -> Option<Vec<u8>> {
    if let Some(legacy) = super::xstate::read_legacy(task) {
        return Some(legacy);
    }
    if let Some(frame) = task
        .ext_lookup(TASKEXT_PTRACE_FRAME)
        .and_then(|payload| payload.downcast::<TrapFrame>().ok())
    {
        return Some(frame.fxsave.to_vec());
    }
    None
}

pub fn write_linux_fpregs(task: &Task, bytes: &[u8]) -> bool {
    if bytes.len() < LINUX_FPREGSET_SIZE {
        return false;
    }
    // Invalid reserved MXCSR bits make a subsequent FXRSTOR raise #GP. Reject
    // them at the ptrace boundary instead of deferring the fault to scheduling.
    let policy_mask = super::fpu::policy().mxcsr_mask;
    let mxcsr = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
    let supplied_mask = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
    if !super::fpu::validate_mxcsr(mxcsr, policy_mask)
        // `mxcsr_mask` is reported by FXSAVE and is not a user-selectable
        // capability.  Permit a zero legacy value, but reject bits that this
        // CPU cannot implement; the canonical policy mask is written below.
        || supplied_mask & !policy_mask != 0
    {
        return false;
    }
    let normalized = {
        let mut value = [0u8; LINUX_FPREGSET_SIZE];
        value.copy_from_slice(&bytes[..LINUX_FPREGSET_SIZE]);
        value[28..32].copy_from_slice(&policy_mask.to_le_bytes());
        value
    };
    let frame_present = task
        .ext_lookup(TASKEXT_PTRACE_FRAME)
        .and_then(|payload| payload.downcast::<TrapFrame>().ok())
        .is_some();
    let frame_result = if let Some(frame) = task
        .ext_lookup(TASKEXT_PTRACE_FRAME)
        .and_then(|payload| payload.downcast::<TrapFrame>().ok())
    {
        let mut new = *frame;
        new.fxsave = normalized;
        store_task_frame(task, new)
    } else {
        false
    };
    let owner = super::xstate::has_extended_state(task);
    let owner_result = if owner {
        super::xstate::write_legacy(task, &normalized)
    } else {
        false
    };
    if owner {
        owner_result && (!frame_present || frame_result)
    } else {
        frame_result
    }
}

/// 返回 Linux `NT_X86_XSTATE` 所需的标准布局长度。
///
/// `CPUID.(0xD,0).EBX` 是当前 OS-enabled XCR0 对应的标准布局大小；在不支持
/// XSAVE 的机器上，Linux 仍提供 512-byte legacy regset，因此回退到 FXSAVE 大小。
pub fn linux_xstate_size() -> usize {
    let features = super::fpu::init();
    if !features.xsave {
        return FXSAVE_SIZE;
    }
    // A standard XSAVE image always contains the 64-byte header.  Firmware may
    // report a stale/short EBX value before XCR0 is initialized; do not expose a
    // buffer that would make the header inaccessible.
    // Derive the layout from the mask that the kernel actually enabled.  The
    // CPUID EBX value describes the firmware's current XCR0 and may be stale
    // before boot policy selection; `size_for_mask` keeps AVX/AVX-512 regsets
    // large enough without exposing components outside that mask.
    features
        .size_for_mask(super::fpu::enabled_mask())
        .max(XSAVE_MIN_SIZE)
}

#[inline]
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset.checked_add(4)?)
        .map(|raw| u32::from_le_bytes(raw.try_into().unwrap()))
}

#[inline]
fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    bytes
        .get(offset..offset.checked_add(8)?)
        .map(|raw| u64::from_le_bytes(raw.try_into().unwrap()))
}

#[inline]
fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> bool {
    let Some(dst) = bytes.get_mut(offset..offset.checked_add(8).unwrap_or(usize::MAX)) else {
        return false;
    };
    dst.copy_from_slice(&value.to_le_bytes());
    true
}

#[inline]
fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> bool {
    let Some(dst) = bytes.get_mut(offset..offset.checked_add(4).unwrap_or(usize::MAX)) else {
        return false;
    };
    dst.copy_from_slice(&value.to_le_bytes());
    true
}

/// Determine the complete Linux signal-fpstate length from its 512-byte
/// legacy prefix.  A zero magic is the legacy-only ABI; an extended frame is
/// bounded by the kernel's fixed xstate allocation before any user copy grows.
pub fn linux_signal_xstate_encoded_size(prefix: &[u8]) -> Option<usize> {
    if prefix.len() < FXSAVE_SIZE {
        return None;
    }
    match read_u32(prefix, FXSAVE_SW_BYTES_OFFSET)? {
        0 => Some(FXSAVE_SIZE),
        FP_XSTATE_MAGIC1 => {
            let size = read_u32(prefix, FXSAVE_SW_BYTES_OFFSET + 4)? as usize;
            (size >= XSAVE_MIN_SIZE + FP_XSTATE_MAGIC2_SIZE && size <= LINUX_SIGNAL_XSTATE_MAX_SIZE)
                .then_some(size)
        }
        _ => None,
    }
}

/// Validate the software metadata in a Linux signal FPU frame.
///
/// This checks both magic words and all size/mask relationships documented by
/// `uapi/asm/sigcontext.h`.  It intentionally does not dereference the frame's
/// user pointer; callers must perform the normal user-copy and lifetime checks.
pub fn validate_signal_xstate_frame(bytes: &[u8]) -> bool {
    if bytes.len() < FXSAVE_SIZE {
        return false;
    }
    let Some(mxcsr) = read_u32(bytes, 24) else {
        return false;
    };
    let policy_mask = super::fpu::policy().mxcsr_mask;
    let Some(supplied_mask) = read_u32(bytes, 28) else {
        return false;
    };
    if !super::fpu::validate_mxcsr(mxcsr, policy_mask) || supplied_mask & !policy_mask != 0 {
        return false;
    }
    let Some(magic1) = read_u32(bytes, FXSAVE_SW_BYTES_OFFSET) else {
        return false;
    };
    // `_fpx_sw_bytes.padding[7]` is reserved by the Linux UAPI and must be
    // zero for both legacy and extended frames.  Check it before the legacy
    // magic-zero fast path so a forged old-style frame cannot smuggle bytes
    // into a future ABI extension.
    if bytes[FXSAVE_SW_BYTES_OFFSET + 20..FXSAVE_SIZE]
        .iter()
        .any(|byte| *byte != 0)
    {
        return false;
    }
    if magic1 == 0 {
        // A zero magic denotes the legacy `_fpstate`; its 512-byte area is all
        // that the old ABI promises.
        return true;
    }
    if magic1 != FP_XSTATE_MAGIC1 {
        return false;
    }
    let Some(extended_size) = read_u32(bytes, FXSAVE_SW_BYTES_OFFSET + 4) else {
        return false;
    };
    let Some(xfeatures) = read_u64(bytes, FXSAVE_SW_BYTES_OFFSET + 8) else {
        return false;
    };
    let Some(xstate_size) = read_u32(bytes, FXSAVE_SW_BYTES_OFFSET + 16) else {
        return false;
    };
    let extended_size = extended_size as usize;
    let xstate_size = xstate_size as usize;
    if extended_size < XSAVE_MIN_SIZE + FP_XSTATE_MAGIC2_SIZE
        || xstate_size < XSAVE_MIN_SIZE
        || xstate_size > extended_size.saturating_sub(FP_XSTATE_MAGIC2_SIZE)
        || xstate_size > linux_xstate_size()
        || extended_size > bytes.len()
    {
        return false;
    }

    let enabled = super::fpu::enabled_mask();
    if xfeatures != enabled {
        return false;
    }
    // Do not accept a header that claims a component whose standard-layout
    // bytes lie past `xstate_size`.  XRSTOR would otherwise consume a
    // truncated image and may raise #GP; the size is derived from CPUID leaf D
    // rather than guessed from the user-provided payload.
    let minimum_xstate_size = super::fpu::init()
        .size_for_mask(xfeatures)
        .max(XSAVE_MIN_SIZE);
    if xstate_size < minimum_xstate_size {
        return false;
    }
    if read_u32(bytes, extended_size - FP_XSTATE_MAGIC2_SIZE) != Some(FP_XSTATE_MAGIC2) {
        return false;
    }
    if xstate_size >= XSAVE_MIN_SIZE {
        let Some(header_features) = read_u64(bytes, XSTATE_HEADER_XFEATURES_OFFSET) else {
            return false;
        };
        let Some(header_compaction) = read_u64(bytes, XSTATE_HEADER_XCOMP_BV_OFFSET) else {
            return false;
        };
        if header_features & !xfeatures != 0 || header_compaction != 0 {
            return false;
        }
        // XSAVE components have mandatory dependencies when they are marked
        // present in XSTATE_BV.  The software xfeatures word above describes
        // all OS-enabled components and is not the in-use bitmap.
        if header_features & super::fpu::XFEATURE_YMM != 0
            && header_features & super::fpu::XFEATURE_BASE != super::fpu::XFEATURE_BASE
        {
            return false;
        }
        if header_features & super::fpu::XFEATURE_AVX512 != 0
            && header_features & super::fpu::XFEATURE_YMM == 0
        {
            return false;
        }
        if bytes[XSAVE_HEADER_OFFSET + 16..XSAVE_MIN_SIZE]
            .iter()
            .any(|byte| *byte != 0)
        {
            return false;
        }
    }
    true
}

/// Validate a standard (non-compacted) `NT_X86_XSTATE` regset image.
///
/// The current task representation owns the 512-byte legacy area.  Extended
/// bytes are accepted only when they are zero, so a caller cannot accidentally
/// believe AVX state was installed when no owner exists yet.  Once an xstate
/// owner is wired, this validator is the single place to relax that condition.
fn validate_linux_xstate_impl(bytes: &[u8], allow_owned_extended: bool) -> bool {
    let required = linux_xstate_size();
    if bytes.len() < required || bytes.len() < FXSAVE_SIZE {
        return false;
    }
    let Some(mxcsr) = read_u32(bytes, 24) else {
        return false;
    };
    let policy_mask = super::fpu::policy().mxcsr_mask;
    let Some(supplied_mask) = read_u32(bytes, 28) else {
        return false;
    };
    if !super::fpu::validate_mxcsr(mxcsr, policy_mask) || supplied_mask & !policy_mask != 0 {
        return false;
    }

    // The six software words are not processor state.  Linux uses only the
    // first one for the OS-enabled XCR0 mask and leaves the remainder zero.
    let Some(sw_mask) = read_u64(bytes, XSTATE_SW_XCR0_OFFSET) else {
        return false;
    };
    let enabled = super::fpu::enabled_mask();
    if sw_mask != enabled {
        return false;
    }
    if bytes[FXSAVE_SW_BYTES_OFFSET + 8..FXSAVE_SIZE]
        .iter()
        .any(|byte| *byte != 0)
    {
        return false;
    }

    if required == FXSAVE_SIZE {
        // No XSAVE header exists on a legacy-only CPU.  The software bytes are
        // still constrained above, matching the kernel's sanitized legacy image.
        return true;
    }
    let Some(xfeatures) = read_u64(bytes, XSTATE_HEADER_XFEATURES_OFFSET) else {
        return false;
    };
    let Some(xcomp_bv) = read_u64(bytes, XSTATE_HEADER_XCOMP_BV_OFFSET) else {
        return false;
    };
    if xfeatures & !enabled != 0 || xcomp_bv != 0 {
        return false;
    }
    let minimum_xstate_size = super::fpu::init()
        .size_for_mask(xfeatures)
        .max(XSAVE_MIN_SIZE);
    if required < minimum_xstate_size {
        return false;
    }
    // Without an owner, extended bytes must be zero: there is nowhere safe to
    // put debugger writes.  An owner-specific caller may opt into the full
    // standard-layout component bytes after the same header checks above.
    if !allow_owned_extended && xfeatures & !super::fpu::XFEATURE_BASE != 0 {
        return false;
    }
    if xfeatures & super::fpu::XFEATURE_YMM != 0
        && xfeatures & super::fpu::XFEATURE_BASE != super::fpu::XFEATURE_BASE
    {
        return false;
    }
    if xfeatures & super::fpu::XFEATURE_AVX512 != 0 && xfeatures & super::fpu::XFEATURE_YMM == 0 {
        return false;
    }
    if bytes[XSAVE_HEADER_OFFSET + 16..XSAVE_MIN_SIZE]
        .iter()
        .any(|byte| *byte != 0)
    {
        return false;
    }
    allow_owned_extended
        || bytes[XSAVE_MIN_SIZE..required]
            .iter()
            .all(|byte| *byte == 0)
}

/// Validate a standard (non-compacted) `NT_X86_XSTATE` regset image.
pub fn validate_linux_xstate(bytes: &[u8]) -> bool {
    validate_linux_xstate_impl(bytes, false)
}

/// Encode the legacy task state as a standard `NT_X86_XSTATE` image.
pub fn encode_linux_xstate_from_fpregs(fpregs: &[u8]) -> Option<Vec<u8>> {
    if fpregs.len() < FXSAVE_SIZE {
        return None;
    }
    let size = linux_xstate_size();
    let mut out = vec![0u8; size];
    out[..FXSAVE_SIZE].copy_from_slice(&fpregs[..FXSAVE_SIZE]);
    let policy_mask = super::fpu::policy().mxcsr_mask;
    out[28..32].copy_from_slice(&policy_mask.to_le_bytes());
    // The caller may have supplied arbitrary FXSAVE software-reserved bytes;
    // they are owned by the kernel in the xstate UAPI and must not leak into a
    // signal/ptrace image.
    out[FXSAVE_SW_BYTES_OFFSET + 8..FXSAVE_SIZE].fill(0);
    let enabled = super::fpu::enabled_mask();
    if size >= XSAVE_MIN_SIZE {
        // Linux's user_xstateregs ABI exposes the enabled XCR0 in the software
        // bytes, while the processor header reports the legacy state in use.
        if !write_u64(&mut out, XSTATE_SW_XCR0_OFFSET, enabled)
            || !write_u64(
                &mut out,
                XSTATE_HEADER_XFEATURES_OFFSET,
                enabled & super::fpu::XFEATURE_BASE,
            )
            || !write_u64(&mut out, XSTATE_HEADER_XCOMP_BV_OFFSET, 0)
        {
            return None;
        }
    } else if !write_u64(&mut out, XSTATE_SW_XCR0_OFFSET, enabled) {
        return None;
    }
    Some(out)
}

/// Encode the current task state as Linux's signal-frame fpstate object.
/// Unlike `NT_X86_XSTATE`, bytes 464..511 carry `_fpx_sw_bytes` magic/size
/// metadata and a trailing `FP_XSTATE_MAGIC2` word terminates the object.
pub fn encode_linux_signal_xstate(task: &Task, fpregs: &[u8]) -> Option<Vec<u8>> {
    if fpregs.len() < FXSAVE_SIZE {
        return None;
    }
    if !super::fpu::init().xsave {
        let mut legacy = fpregs[..FXSAVE_SIZE].to_vec();
        legacy[FXSAVE_SW_BYTES_OFFSET..FXSAVE_SIZE].fill(0);
        return Some(legacy);
    }

    let mut image = read_linux_xstate(task).or_else(|| encode_linux_xstate_from_fpregs(fpregs))?;
    let xstate_size = linux_xstate_size();
    if image.len() != xstate_size {
        return None;
    }
    image[FXSAVE_SW_BYTES_OFFSET..FXSAVE_SIZE].fill(0);
    let extended_size = xstate_size.checked_add(FP_XSTATE_MAGIC2_SIZE)?;
    if !write_u32(&mut image, FXSAVE_SW_BYTES_OFFSET, FP_XSTATE_MAGIC1)
        || !write_u32(&mut image, FXSAVE_SW_BYTES_OFFSET + 4, extended_size as u32)
        || !write_u64(
            &mut image,
            FXSAVE_SW_BYTES_OFFSET + 8,
            super::fpu::enabled_mask(),
        )
        || !write_u32(&mut image, FXSAVE_SW_BYTES_OFFSET + 16, xstate_size as u32)
    {
        return None;
    }
    image.extend_from_slice(&FP_XSTATE_MAGIC2.to_le_bytes());
    Some(image)
}

/// Install a validated Linux signal fpstate after the kernel has restored its
/// LIFO signal snapshot.  This lets legal handler edits replace that snapshot.
pub fn restore_linux_signal_xstate(task: &Task, context: usize, bytes: Option<&[u8]>) -> bool {
    if let Some(image) = bytes
        && !validate_signal_xstate_frame(image)
    {
        return false;
    }
    super::xstate::restore_signal_image(task, context, bytes)
}

/// Read the standard xstate image for a stopped task.
pub fn read_linux_xstate(task: &Task) -> Option<Vec<u8>> {
    if let Some(image) = super::xstate::read_image(task) {
        let required = linux_xstate_size();
        if image.len() > required {
            return None;
        }
        let mut out = vec![0u8; required];
        out[..image.len()].copy_from_slice(&image);
        let policy_mask = super::fpu::policy().mxcsr_mask;
        out[28..32].copy_from_slice(&policy_mask.to_le_bytes());
        out[FXSAVE_SW_BYTES_OFFSET + 8..FXSAVE_SIZE].fill(0);
        let enabled = super::fpu::enabled_mask();
        if !write_u64(&mut out, XSTATE_SW_XCR0_OFFSET, enabled) {
            return None;
        }
        if required >= XSAVE_MIN_SIZE {
            let features = read_u64(&out, XSTATE_HEADER_XFEATURES_OFFSET)
                .unwrap_or(enabled & super::fpu::XFEATURE_BASE);
            // Never silently discard a component bit from an owned image.  A
            // mismatch means the image was produced under a different XCR0
            // policy and must be rejected instead of being re-encoded.
            if features & !enabled != 0 {
                return None;
            }
            if !write_u64(&mut out, XSTATE_HEADER_XFEATURES_OFFSET, features)
                || !write_u64(&mut out, XSTATE_HEADER_XCOMP_BV_OFFSET, 0)
            {
                return None;
            }
            sanitize_absent_xsave_components(&mut out);
        }
        return Some(out);
    }
    let fpregs = read_linux_fpregs(task)?;
    encode_linux_xstate_from_fpregs(&fpregs)
}

/// Write a standard xstate image for a stopped task.
pub fn write_linux_xstate(task: &Task, bytes: &[u8]) -> bool {
    let owner = super::xstate::has_extended_state(task);
    if !validate_linux_xstate_impl(bytes, owner) {
        return false;
    }
    if owner {
        let required = linux_xstate_size();
        let Some(owner_mask) = super::xstate::state_mask(task) else {
            return false;
        };
        if read_u64(bytes, XSTATE_SW_XCR0_OFFSET) != Some(owner_mask) {
            return false;
        }
        if let Some(features) = read_u64(bytes, XSTATE_HEADER_XFEATURES_OFFSET) {
            if features & !owner_mask != 0 {
                return false;
            }
        }
        let Some(mut image) = super::xstate::read_image(task) else {
            return false;
        };
        if bytes.len() < image.len() || required < image.len() {
            return false;
        }
        if bytes[image.len()..required].iter().any(|byte| *byte != 0) {
            return false;
        }
        let image_len = image.len();
        image.copy_from_slice(&bytes[..image_len]);
        sanitize_absent_xsave_components(&mut image);
        // Keep software-owned fields canonical inside the task image.
        image[FXSAVE_SW_BYTES_OFFSET..FXSAVE_SIZE].fill(0);
        image[28..32].copy_from_slice(&super::fpu::policy().mxcsr_mask.to_le_bytes());
        if !super::xstate::write_image(task, &image) {
            return false;
        }
        // Keep a ptrace stop snapshot coherent with the owner.  The owner is
        // authoritative for extended bytes, while the fixed TrapFrame remains
        // the fast legacy view used by syscall return code.
        if let Some(frame) = task
            .ext_lookup(TASKEXT_PTRACE_FRAME)
            .and_then(|payload| payload.downcast::<TrapFrame>().ok())
        {
            let mut updated = *frame;
            updated.fxsave.copy_from_slice(&image[..FXSAVE_SIZE]);
            if !store_task_frame(task, updated) {
                return false;
            }
        }
        return true;
    }
    // No owner: only the legacy half is accepted and all extended bytes were
    // checked as zero by the validator above.
    let mut legacy = [0u8; FXSAVE_SIZE];
    legacy.copy_from_slice(&bytes[..FXSAVE_SIZE]);
    legacy[FXSAVE_SW_BYTES_OFFSET..].fill(0);
    write_linux_fpregs(task, &legacy)
}

/// 把 FXSAVE 区中的 MXCSR 取出；供 signal/ptrace 校验路径复用。
pub fn mxcsr_from_fpregs(bytes: &[u8]) -> Option<u32> {
    (bytes.len() >= 28).then(|| u32::from_le_bytes(bytes[24..28].try_into().unwrap()))
}

/// 生成一个清零的 legacy FXSAVE 初始区。
pub fn initial_fpregs() -> Vec<u8> {
    TrapFrame::initial_fxsave().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn only_a_dirty_ptrace_snapshot_replaces_the_live_frame() {
        let mut live = TrapFrame::default();
        live.rax = 1;
        let mut snapshot = live;
        snapshot.rax = 2;

        merge_tracer_frame(&mut live, Some(snapshot), false);
        assert_eq!(live.rax, 1);

        merge_tracer_frame(&mut live, Some(snapshot), true);
        assert_eq!(live.rax, 2);
    }

    #[test]
    fn xstate_encoder_emits_linux_standard_header() {
        let image = encode_linux_xstate_from_fpregs(&TrapFrame::initial_fxsave()).unwrap();
        assert_eq!(image.len(), linux_xstate_size());
        assert!(image.len() >= FXSAVE_SIZE);
        let enabled = super::super::fpu::enabled_mask();
        assert_eq!(read_u64(&image, XSTATE_SW_XCR0_OFFSET), Some(enabled));
        if image.len() >= XSAVE_MIN_SIZE {
            assert_eq!(read_u64(&image, XSTATE_HEADER_XCOMP_BV_OFFSET), Some(0));
            assert!(validate_linux_xstate(&image));
        }
    }

    #[test]
    fn absent_xsave_components_are_cleared_before_export() {
        let size = linux_xstate_size();
        if size < XSAVE_MIN_SIZE {
            return;
        }
        let mut image = vec![0u8; size];
        put_u64(
            &mut image,
            XSTATE_HEADER_XFEATURES_OFFSET,
            super::super::fpu::XFEATURE_BASE,
        );
        for feature in [2u32, 3, 4, 5, 6, 7] {
            if let Some((offset, component_size)) =
                super::super::fpu::xsave_component_range(feature)
                && let Some(end) = offset.checked_add(component_size)
                && end <= image.len()
            {
                image[offset..end].fill(0xa5);
            }
        }
        sanitize_absent_xsave_components(&mut image);
        for feature in [2u32, 3, 4, 5, 6, 7] {
            if let Some((offset, component_size)) =
                super::super::fpu::xsave_component_range(feature)
                && let Some(end) = offset.checked_add(component_size)
                && end <= image.len()
            {
                assert!(image[offset..end].iter().all(|byte| *byte == 0));
            }
        }
    }

    #[test]
    fn signal_xstate_magic_and_end_marker_are_checked() {
        let xstate_size = XSAVE_MIN_SIZE;
        let extended_size = xstate_size + FP_XSTATE_MAGIC2_SIZE;
        let mut image = vec![0u8; extended_size];
        put_u32(&mut image, FXSAVE_SW_BYTES_OFFSET, FP_XSTATE_MAGIC1);
        put_u32(&mut image, FXSAVE_SW_BYTES_OFFSET + 4, extended_size as u32);
        let enabled = super::super::fpu::enabled_mask();
        let active = enabled & super::super::fpu::XFEATURE_BASE;
        put_u64(&mut image, FXSAVE_SW_BYTES_OFFSET + 8, active);
        put_u32(&mut image, FXSAVE_SW_BYTES_OFFSET + 16, xstate_size as u32);
        put_u64(&mut image, XSTATE_HEADER_XFEATURES_OFFSET, active);
        put_u64(&mut image, XSTATE_HEADER_XCOMP_BV_OFFSET, 0);
        put_u32(
            &mut image,
            extended_size - FP_XSTATE_MAGIC2_SIZE,
            FP_XSTATE_MAGIC2,
        );
        assert!(validate_signal_xstate_frame(&image));

        put_u32(&mut image, extended_size - FP_XSTATE_MAGIC2_SIZE, 0);
        assert!(!validate_signal_xstate_frame(&image));
    }

    #[test]
    fn xstate_validator_rejects_compacted_or_extended_claims_without_owner() {
        let image = encode_linux_xstate_from_fpregs(&TrapFrame::initial_fxsave()).unwrap();
        if image.len() < XSAVE_MIN_SIZE {
            return;
        }
        let mut compacted = image.clone();
        put_u64(&mut compacted, XSTATE_HEADER_XCOMP_BV_OFFSET, 1u64 << 63);
        assert!(!validate_linux_xstate(&compacted));

        let mut extended = image;
        let enabled = super::super::fpu::enabled_mask();
        if enabled & super::super::fpu::XFEATURE_YMM != 0 {
            put_u64(
                &mut extended,
                XSTATE_HEADER_XFEATURES_OFFSET,
                super::super::fpu::XFEATURE_BASE | super::super::fpu::XFEATURE_YMM,
            );
            assert!(!validate_linux_xstate(&extended));
        }
    }
}

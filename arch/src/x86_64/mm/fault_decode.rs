//! x86_64 缺页 fault 解码与 `__ex_table` fixup。

use core::sync::atomic::{AtomicUsize, Ordering};

use general::TrapFramePtr;
use general::mm::{FaultDecodeOps, FaultKind};

use crate::x86_64::interrupt::PAGE_FAULT;
use crate::x86_64::trap_frame::TrapFrame;

/// Page-fault error-code bits from Intel SDM.
const PF_PRESENT: usize = 1 << 0;
const PF_WRITE: usize = 1 << 1;
const PF_INSTRUCTION: usize = 1 << 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct ExTableEntry {
    fault_pc: usize,
    fixup_pc: usize,
}

#[cfg(target_os = "none")]
unsafe extern "C" {
    static __ex_table_start: u8;
    static __ex_table_end: u8;
    fn stext();
    fn etext();
}

static FAULT_ADDRESS: AtomicUsize = AtomicUsize::new(0);

/// Trap entry may publish CR2 before doing any nested work.  This is also useful
/// for tests and for a future IST entry that cannot safely read CR2 twice.
pub fn set_fault_address(address: usize) {
    FAULT_ADDRESS.store(address, Ordering::Release);
}

#[inline]
fn frame(ptr: TrapFramePtr) -> &'static TrapFrame {
    // Safety: callers pass a pointer produced by the x86 trap entry.
    unsafe { &*(ptr.as_usize() as *const TrapFrame) }
}

#[inline]
fn frame_mut(ptr: TrapFramePtr) -> &'static mut TrapFrame {
    // Safety: caller owns the active trap frame and has exclusive write access.
    unsafe { &mut *(ptr.as_usize() as *mut TrapFrame) }
}

fn fault_kind(ptr: TrapFramePtr) -> FaultKind {
    let tf = frame(ptr);
    if tf.vector != PAGE_FAULT as usize {
        return FaultKind::Privilege;
    }
    let error = tf.error_code;
    if error & PF_PRESENT == 0 {
        if error & PF_INSTRUCTION != 0 {
            FaultKind::Exec
        } else if error & PF_WRITE != 0 {
            FaultKind::Store
        } else {
            FaultKind::Load
        }
    } else if error & PF_INSTRUCTION != 0 {
        FaultKind::PermExec
    } else if error & PF_WRITE != 0 {
        FaultKind::PermWrite
    } else {
        FaultKind::PermRead
    }
}

fn fault_addr(_ptr: TrapFramePtr) -> usize {
    #[cfg(target_os = "none")]
    {
        let address: usize;
        // CR2 is architecturally updated before the page-fault handler starts.
        unsafe {
            core::arch::asm!("mov {}, cr2", out(reg) address, options(nostack, nomem));
        }
        FAULT_ADDRESS.store(address, Ordering::Release);
        address
    }
    #[cfg(not(target_os = "none"))]
    {
        FAULT_ADDRESS.load(Ordering::Acquire)
    }
}

fn fault_from_user(ptr: TrapFramePtr) -> bool {
    frame(ptr).from_user()
}

#[cfg(target_os = "none")]
fn try_fixup_kernel_access(ptr: TrapFramePtr) -> bool {
    let pc = frame(ptr).rip;
    let start = core::ptr::addr_of!(__ex_table_start) as usize;
    let end = core::ptr::addr_of!(__ex_table_end) as usize;
    let entry_size = core::mem::size_of::<ExTableEntry>();
    if end <= start || (end - start) % entry_size != 0 {
        return false;
    }
    let entries = (end - start) / entry_size;
    let table = start as *const ExTableEntry;
    for index in 0..entries {
        // Safety: linker places a read-only, entry-size-aligned table between
        // the exported bounds.
        let entry = unsafe { core::ptr::read_volatile(table.add(index)) };
        if entry.fault_pc == pc {
            frame_mut(ptr).rip = entry.fixup_pc;
            return true;
        }
    }
    false
}

#[cfg(not(target_os = "none"))]
fn try_fixup_kernel_access(_ptr: TrapFramePtr) -> bool {
    false
}

/// Validate linker-provided exception-table bounds during debug boot.
#[cfg(target_os = "none")]
pub(super) fn validate_exception_table() {
    if !cfg!(debug_assertions) {
        return;
    }
    let start = core::ptr::addr_of!(__ex_table_start) as usize;
    let end = core::ptr::addr_of!(__ex_table_end) as usize;
    let size = core::mem::size_of::<ExTableEntry>();
    assert!(start <= end, "[x86][mm] invalid __ex_table bounds");
    assert_eq!((end - start) % size, 0, "[x86][mm] malformed __ex_table");
    let text_start = stext as *const () as usize;
    let text_end = etext as *const () as usize;
    let table = start as *const ExTableEntry;
    let count = (end - start) / size;
    for index in 0..count {
        let entry = unsafe { core::ptr::read_volatile(table.add(index)) };
        assert!((text_start..text_end).contains(&entry.fault_pc));
        assert!((text_start..text_end).contains(&entry.fixup_pc));
        // The linker preserves input order rather than sorting arbitrary Rust
        // objects; lookup is linear, so no ordering invariant is required.
    }
}

#[cfg(not(target_os = "none"))]
pub(super) fn validate_exception_table() {}

pub(super) static FAULT_DECODE_OPS: FaultDecodeOps = FaultDecodeOps {
    fault_kind,
    fault_addr,
    fault_from_user,
    try_fixup_kernel_access,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame(vector: usize, error_code: usize) -> TrapFrame {
        let mut frame = TrapFrame::default();
        frame.vector = vector;
        frame.error_code = error_code;
        frame.cs = crate::x86_64::trap_frame::USER_CS as usize;
        frame
    }

    #[test]
    fn page_fault_error_code_maps_to_access_kind() {
        let mut frame = test_frame(PAGE_FAULT as usize, 0);
        let ptr = TrapFramePtr::new(&mut frame as *mut _ as usize);
        assert_eq!(fault_kind(ptr), FaultKind::Load);
        frame.error_code = PF_WRITE;
        assert_eq!(fault_kind(ptr), FaultKind::Store);
        frame.error_code = PF_PRESENT;
        assert_eq!(fault_kind(ptr), FaultKind::PermRead);
        frame.error_code = PF_PRESENT | PF_WRITE;
        assert_eq!(fault_kind(ptr), FaultKind::PermWrite);
        frame.error_code = PF_PRESENT | PF_INSTRUCTION;
        assert_eq!(fault_kind(ptr), FaultKind::PermExec);
    }

    #[test]
    fn source_privilege_comes_from_cs_rpl() {
        let mut frame = test_frame(PAGE_FAULT as usize, 0);
        let ptr = TrapFramePtr::new(&mut frame as *mut _ as usize);
        assert!(fault_from_user(ptr));
        frame.cs = crate::x86_64::trap_frame::KERNEL_CS as usize;
        assert!(!fault_from_user(ptr));
    }
}

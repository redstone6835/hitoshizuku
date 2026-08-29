//! x86_64 IDT 与本地中断控制。

use super::descriptor::{DescriptorTablePointer, load_idt};

pub const IDT_ENTRIES: usize = 256;
pub const PRESENT: u8 = 1 << 7;
pub const INTERRUPT_GATE: u8 = 0x0e;
pub const TRAP_GATE: u8 = 0x0f;

/// x86_64 中断门（16 字节）。
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdtEntry {
    pub offset_low: u16,
    pub selector: u16,
    pub ist: u8,
    pub attributes: u8,
    pub offset_mid: u16,
    pub offset_high: u32,
    pub reserved: u32,
}

impl IdtEntry {
    pub const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            attributes: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    pub fn new(handler: usize, selector: u16, attributes: u8, ist: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector,
            ist: ist & 0x7,
            attributes,
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }

    pub fn handler_address(self) -> usize {
        (self.offset_low as usize)
            | ((self.offset_mid as usize) << 16)
            | ((self.offset_high as usize) << 32)
    }

    pub fn is_present(self) -> bool {
        self.attributes & PRESENT != 0
    }
}

/// 静态 IDT 表；启动代码填充 handler 后调用 [`Idt::load`]。
#[repr(C, align(16))]
pub struct Idt {
    pub entries: [IdtEntry; IDT_ENTRIES],
}

impl Idt {
    pub const fn new() -> Self {
        Self {
            entries: [IdtEntry::missing(); IDT_ENTRIES],
        }
    }

    pub fn set_handler(
        &mut self,
        vector: u8,
        handler: usize,
        selector: u16,
        attributes: u8,
        ist: u8,
    ) {
        self.entries[vector as usize] = IdtEntry::new(handler, selector, attributes, ist);
    }

    pub fn clear_handler(&mut self, vector: u8) {
        self.entries[vector as usize] = IdtEntry::missing();
    }

    pub fn pointer(&self) -> DescriptorTablePointer {
        DescriptorTablePointer::new(
            self.entries.as_ptr() as u64,
            (core::mem::size_of_val(&self.entries) - 1) as u16,
        )
    }

    /// # Safety
    /// IDT 内存必须在整个 CPU 生命周期内保持有效。
    pub unsafe fn load(&'static self) {
        let pointer = self.pointer();
        unsafe { load_idt(&pointer) };
    }
}

/// 处理器异常向量编号。
pub const DIVIDE_ERROR: u8 = 0;
pub const DEBUG: u8 = 1;
pub const NMI: u8 = 2;
pub const BREAKPOINT: u8 = 3;
pub const OVERFLOW: u8 = 4;
pub const BOUND_RANGE: u8 = 5;
pub const INVALID_OPCODE: u8 = 6;
pub const DEVICE_NOT_AVAILABLE: u8 = 7;
pub const DOUBLE_FAULT: u8 = 8;
pub const INVALID_TSS: u8 = 10;
pub const SEGMENT_NOT_PRESENT: u8 = 11;
pub const STACK_SEGMENT: u8 = 12;
pub const GENERAL_PROTECTION: u8 = 13;
pub const PAGE_FAULT: u8 = 14;
pub const X87_FLOATING_POINT: u8 = 16;
pub const ALIGNMENT_CHECK: u8 = 17;
pub const MACHINE_CHECK: u8 = 18;
pub const SIMD_FLOATING_POINT: u8 = 19;
pub const VIRTUALIZATION: u8 = 20;
pub const CONTROL_PROTECTION: u8 = 21;
pub const VMM_COMMUNICATION: u8 = 29;
pub const SECURITY_EXCEPTION: u8 = 30;

#[inline]
pub fn read_flags() -> usize {
    #[cfg(target_os = "none")]
    {
        let flags: usize;
        // The pair balances RSP but still touches the active stack; `nostack`
        // would give LLVM an invalid model of this instruction sequence.
        unsafe {
            core::arch::asm!("pushfq", "pop {}", out(reg) flags);
        }
        flags
    }
    #[cfg(not(target_os = "none"))]
    {
        1 << 9
    }
}

#[inline]
pub fn disable() {
    #[cfg(target_os = "none")]
    unsafe {
        // Linux's native_irq_disable() carries a "memory" clobber.  `nomem`
        // would allow compiler loads/stores to cross the interrupt boundary,
        // which is incorrect for irq-protected spin-lock critical sections.
        core::arch::asm!("cli", options(nostack));
    }
}

#[inline]
pub fn enable() {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("sti", options(nostack));
    }
}

#[inline]
pub fn save_and_disable() -> usize {
    let state = read_flags();
    disable();
    state
}

#[inline]
pub fn restore(state: usize) {
    if state & (1 << 9) != 0 {
        enable();
    } else {
        // `local_irq_restore(0)` must actively keep interrupts disabled.  A
        // caller may have enabled IRQs while doing non-critical work between
        // saving and restoring the token; silently returning here would leak
        // that state across an irq-protected critical section.
        disable();
    }
}

#[inline]
pub fn halt() -> ! {
    loop {
        #[cfg(target_os = "none")]
        unsafe {
            // Keep the memory clobber: wake-up predicates are commonly written
            // immediately before HLT and must be visible before sleeping.
            core::arch::asm!("hlt", options(nostack));
        }
        #[cfg(not(target_os = "none"))]
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idt_gate_roundtrip() {
        let address = 0xffff_8000_1234_5678usize;
        let entry = IdtEntry::new(address, 0x10, PRESENT | INTERRUPT_GATE, 2);
        assert_eq!(entry.handler_address(), address);
        assert!(entry.is_present());
        assert_eq!(entry.ist, 2);
    }

    #[test]
    fn idt_table_has_all_vectors() {
        let mut idt = Idt::new();
        idt.set_handler(BREAKPOINT, 0x1000, 0x10, PRESENT | TRAP_GATE, 0);
        assert_eq!(idt.entries[BREAKPOINT as usize].handler_address(), 0x1000);
    }
}

//! x86_64 GDT、TSS 和 GDTR/IDTR 装载框架。

use core::mem::size_of;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// 传给 `lgdt`/`lidt` 的伪描述符。
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DescriptorTablePointer {
    pub limit: u16,
    pub base: u64,
}

impl DescriptorTablePointer {
    pub const fn new(base: u64, limit: u16) -> Self {
        Self { base, limit }
    }
}

/// 64 位代码/数据段描述符的原始 8 字节编码。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct SegmentDescriptor(pub u64);

impl SegmentDescriptor {
    pub const NULL: Self = Self(0);

    /// 构造 flat 64-bit code/data descriptor。
    pub const fn flat(access: u8, long_mode: bool) -> Self {
        let mut value = (access as u64) << 40;
        value |= 0x00cf_0000_0000_0000; // G=1, D/B=1, limit=0xfffff
        if long_mode {
            value &= !(1 << 54); // 清 D/B
            value |= 1 << 53; // L
        }
        Self(value)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// 64 位 TSS 描述符（占两个 GDT 槽）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C, packed)]
pub struct TssDescriptor {
    pub low: u64,
    pub high: u64,
}

impl TssDescriptor {
    pub fn new(base: u64, limit: u32) -> Self {
        let limit = limit.min(0x000f_ffff);
        let mut low = (limit as u64 & 0xffff)
            | ((base & 0x00ff_ffff) << 16)
            | (0x89u64 << 40)
            | (((limit as u64 >> 16) & 0xf) << 48)
            | (((base >> 24) & 0xff) << 56);
        // TSS is a system descriptor, therefore clear the code/data D/B bit.
        low &= !(1 << 54);
        let high = base >> 32;
        Self { low, high }
    }
}

/// x86_64 long-mode TSS。
///
/// The hardware layout is packed: in long mode `rsp0` starts at byte 4 (the
/// first four bytes are reserved), not at the naturally aligned byte 8.  This
/// mirrors Linux's `struct x86_hw_tss`; using a naturally aligned `repr(C)`
/// struct would make the CPU load the wrong stack pointers on a ring change.
///
/// The one-byte I/O bitmap terminator is deliberately part of the object.  A
/// descriptor whose limit includes that byte makes ports 0..7 consult an all-1
/// bitmap, while ports 8 and above fall beyond the limit and trap with #GP.
/// `tss_pointer` sets the limit to this exact byte, so trailing Rust padding can
/// never accidentally become permissive bitmap bytes.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct TaskStateSegment {
    pub reserved0: u32,
    pub rsp0: u64,
    pub rsp1: u64,
    pub rsp2: u64,
    pub reserved1: u64,
    pub ist: [u64; 7],
    pub reserved2: u64,
    pub reserved3: u16,
    pub iomap_base: u16,
    /// All-ones bitmap terminator; see Intel SDM and Linux `x86_io_bitmap`.
    pub io_bitmap_terminator: u8,
}

impl Default for TaskStateSegment {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskStateSegment {
    /// Const constructor used for per-CPU static storage before the allocator
    /// and descriptor loader are available.
    pub const fn new() -> Self {
        Self {
            reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            // Keep the bitmap inside the descriptor but expose no writable
            // ports.  The terminator is immediately after iomap_base in the
            // packed hardware layout (offset 104).
            iomap_base: Self::IO_BITMAP_TERMINATOR_OFFSET as u16,
            io_bitmap_terminator: 0xff,
        }
    }
}

impl TaskStateSegment {
    /// Offset of the first (and only) deny-all bitmap byte.
    pub const IO_BITMAP_TERMINATOR_OFFSET: usize = 104;

    /// Inclusive TSS descriptor limit required for the deny-all bitmap.
    pub const TSS_DESCRIPTOR_LIMIT: u32 = Self::IO_BITMAP_TERMINATOR_OFFSET as u32;

    pub fn set_kernel_stack(&mut self, stack_top: usize) {
        self.rsp0 = stack_top as u64;
    }

    pub fn set_interrupt_stack(&mut self, index: usize, stack_top: usize) -> bool {
        if index >= 7 {
            return false;
        }
        // `TaskStateSegment` is intentionally packed to match hardware, so a
        // direct reference to `ist[index]` would be an unaligned reference.
        // The raw pointer plus `write_unaligned` preserves the public API while
        // remaining valid for a TSS allocated at any byte alignment.
        let slot = core::ptr::addr_of_mut!(self.ist) as *mut u64;
        unsafe { slot.add(index).write_unaligned(stack_top as u64) };
        true
    }
}

/// 常用 GDT selector（与 [`crate::x86_64::trap_frame`] 保持一致）。
pub const KERNEL_CS: u16 = 0x10;
pub const KERNEL_SS: u16 = 0x18;
pub const USER_CS: u16 = 0x33;
pub const USER_SS: u16 = 0x2b;
// Keep the Linux-compatible user selectors above.  A 64-bit TSS occupies two
// GDT slots and therefore must not share the user-data slot (0x2b / index 5).
// Index 8 leaves slots 5/6 available for USER_SS/USER_CS and gives each CPU a
// private pair in its GDT.
pub const TSS_SELECTOR: u16 = 0x40;

const GDT_ENTRIES: usize = 10;
const TSS_GDT_INDEX: usize = (TSS_SELECTOR / 8) as usize;

/// A per-CPU GDT.  Keeping the TSS descriptor in the same table as the code
/// and data descriptors avoids the cross-CPU overwrite that a single global
/// TSS slot would cause during AP bring-up.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct CpuGdt([SegmentDescriptor; GDT_ENTRIES]);

impl CpuGdt {
    const fn new() -> Self {
        Self([
            SegmentDescriptor::NULL,
            // The first code slot is retained for the 32-bit bootstrap far
            // jump.  Long-mode kernel code uses selector 0x10 below.
            SegmentDescriptor::flat(0x9a, true),
            SegmentDescriptor::flat(0x9a, true), // KERNEL_CS = 0x10
            SegmentDescriptor::flat(0x92, false), // KERNEL_SS = 0x18
            SegmentDescriptor::NULL,
            SegmentDescriptor::flat(0xf2, false), // USER_SS = 0x2b
            SegmentDescriptor::flat(0xfa, true),  // USER_CS = 0x33
            SegmentDescriptor::NULL,
            SegmentDescriptor::NULL, // TSS low
            SegmentDescriptor::NULL, // TSS high
        ])
    }
}

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct TssStorage(TaskStateSegment);

static mut CPU_GDTS: [CpuGdt; sched::NR_CPUS] = [const { CpuGdt::new() }; sched::NR_CPUS];
static mut CPU_TSS_STORAGE: [TssStorage; sched::NR_CPUS] =
    [const { TssStorage(TaskStateSegment::new()) }; sched::NR_CPUS];

/// Emergency stacks used when the interrupted kernel stack cannot be trusted.
///
/// Linux assigns separate IST entries to #DF, NMI and #MC so nesting one of
/// these events cannot overwrite another event's frame.  Keep the same
/// separation here.  Sixteen KiB is enough for the fixed assembly frame and a
/// bounded Rust dispatcher while avoiding allocator dependencies during BSP/AP
/// descriptor setup.
pub const IST_STACK_SIZE: usize = 16 * 1024;
pub const DOUBLE_FAULT_IST: u8 = 1;
pub const NMI_IST: u8 = 2;
pub const MACHINE_CHECK_IST: u8 = 3;

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct IstStack([u8; IST_STACK_SIZE]);

static mut CPU_DOUBLE_FAULT_STACKS: [IstStack; sched::NR_CPUS] =
    [const { IstStack([0; IST_STACK_SIZE]) }; sched::NR_CPUS];
static mut CPU_NMI_STACKS: [IstStack; sched::NR_CPUS] =
    [const { IstStack([0; IST_STACK_SIZE]) }; sched::NR_CPUS];
static mut CPU_MACHINE_CHECK_STACKS: [IstStack; sched::NR_CPUS] =
    [const { IstStack([0; IST_STACK_SIZE]) }; sched::NR_CPUS];
static GDT_INIT_MASK: AtomicUsize = AtomicUsize::new(0);

// The boot path installs one TSS per logical CPU.  Keep one pointer per CPU;
// a single global pointer would let an AP registration overwrite the BSP's
// stack and make a later rsp0 update write to the wrong TSS.  A null slot is a
// valid early-boot state and simply leaves the software stack mirror untouched.
static CURRENT_TSS: [AtomicPtr<TaskStateSegment>; sched::NR_CPUS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; sched::NR_CPUS];

#[inline]
fn current_tss_slot() -> &'static AtomicPtr<TaskStateSegment> {
    // `current_cpu_id()` is constrained by the scheduler contract to identify
    // a valid logical CPU.  Clamp defensively while AP discovery is still
    // coming online, so a malformed firmware id cannot index past the table.
    let cpu = crate::x86_64::specific::current_cpu_id().min(sched::NR_CPUS - 1);
    &CURRENT_TSS[cpu]
}

/// Publish the TSS used by the current CPU.
///
/// # Safety
/// `tss` must remain valid and exclusively owned by the current CPU for the
/// lifetime of the registration.
pub unsafe fn install_current_tss(tss: *mut TaskStateSegment) {
    current_tss_slot().store(tss, Ordering::Release);
}

/// Update the current CPU's `rsp0` when a kernel stack is switched in.
pub fn set_kernel_stack(stack_top: usize) {
    let ptr = current_tss_slot().load(Ordering::Acquire);
    if !ptr.is_null() {
        // Safety: install_current_tss requires a live, current-CPU TSS pointer.
        unsafe { (*ptr).set_kernel_stack(stack_top) };
    }
}

/// Return the currently installed TSS pointer, if descriptor setup completed.
pub fn current_tss() -> Option<*mut TaskStateSegment> {
    let ptr = current_tss_slot().load(Ordering::Acquire);
    (!ptr.is_null()).then_some(ptr)
}

#[inline]
fn current_cpu_index() -> usize {
    crate::x86_64::specific::current_cpu_id().min(sched::NR_CPUS - 1)
}

/// Initialize and load the descriptor tables for the current logical CPU.
///
/// The bootstrap assembly starts with a tiny identity GDT.  Once the kernel
/// allocator and scheduler are available this function replaces it with the
/// complete table, installs a deny-all I/O bitmap TSS, reloads the data
/// segments and executes `ltr`.  Hosted builds still publish the software
/// mirrors, which keeps ABI/layout tests meaningful without executing
/// privileged instructions.
pub unsafe fn initialize_current_cpu(kernel_stack_top: usize) {
    let cpu = current_cpu_index();
    let bit = 1usize.checked_shl(cpu as u32).unwrap_or(0);
    let gdt = unsafe { (core::ptr::addr_of_mut!(CPU_GDTS) as *mut CpuGdt).add(cpu) };
    let tss_storage =
        unsafe { (core::ptr::addr_of_mut!(CPU_TSS_STORAGE) as *mut TssStorage).add(cpu) };
    let tss = unsafe { core::ptr::addr_of_mut!((*tss_storage).0) };
    let double_fault_stack =
        unsafe { (core::ptr::addr_of_mut!(CPU_DOUBLE_FAULT_STACKS) as *mut IstStack).add(cpu) };
    let nmi_stack = unsafe { (core::ptr::addr_of_mut!(CPU_NMI_STACKS) as *mut IstStack).add(cpu) };
    let machine_check_stack =
        unsafe { (core::ptr::addr_of_mut!(CPU_MACHINE_CHECK_STACKS) as *mut IstStack).add(cpu) };
    unsafe {
        core::ptr::write(tss, TaskStateSegment::default());
        (*tss).set_kernel_stack(kernel_stack_top);
        let double_fault_top = core::ptr::addr_of!((*double_fault_stack).0)
            .cast::<u8>()
            .add(IST_STACK_SIZE) as usize;
        let nmi_top = core::ptr::addr_of!((*nmi_stack).0)
            .cast::<u8>()
            .add(IST_STACK_SIZE) as usize;
        let machine_check_top = core::ptr::addr_of!((*machine_check_stack).0)
            .cast::<u8>()
            .add(IST_STACK_SIZE) as usize;
        assert!((*tss).set_interrupt_stack(usize::from(DOUBLE_FAULT_IST - 1), double_fault_top));
        assert!((*tss).set_interrupt_stack(usize::from(NMI_IST - 1), nmi_top));
        assert!((*tss).set_interrupt_stack(usize::from(MACHINE_CHECK_IST - 1), machine_check_top,));
        install_current_tss(tss);

        let descriptor =
            TssDescriptor::new(tss as usize as u64, TaskStateSegment::TSS_DESCRIPTOR_LIMIT);
        let entries = core::ptr::addr_of_mut!((*gdt).0) as *mut SegmentDescriptor;
        (entries.add(TSS_GDT_INDEX) as *mut u64).write_unaligned(descriptor.low);
        (entries.add(TSS_GDT_INDEX + 1) as *mut u64).write_unaligned(descriptor.high);
    }

    #[cfg(target_os = "none")]
    unsafe {
        let pointer = gdt_pointer(core::ptr::addr_of!((*gdt).0).cast(), GDT_ENTRIES);
        load_gdt(&pointer);
        reload_kernel_segments();
        load_tr(TSS_SELECTOR);
    }
    GDT_INIT_MASK.fetch_or(bit, Ordering::Release);
}

/// Return whether the current CPU has a complete GDT/TSS installed.
pub fn current_cpu_initialized() -> bool {
    let cpu = current_cpu_index();
    let bit = 1usize.checked_shl(cpu as u32).unwrap_or(0);
    GDT_INIT_MASK.load(Ordering::Acquire) & bit != 0
}

#[cfg(target_os = "none")]
#[inline(never)]
unsafe fn reload_kernel_segments() {
    // A far return reloads CS; the data selectors can then be loaded normally.
    // Do not mark this asm `nostack`: the push/retfq sequence intentionally uses
    // the active kernel stack, just like Linux's native_load_gdt path.
    unsafe {
        core::arch::asm!(
            "push {cs}",
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",
            "2:",
            "mov ax, {ss}",
            "mov ds, ax",
            "mov es, ax",
            "mov ss, ax",
            cs = const KERNEL_CS as usize,
            ss = const KERNEL_SS as usize,
            out("rax") _,
        );
    }
}

/// 在裸机上装载 GDT；hosted 路径仅执行编译期布局检查。
pub unsafe fn load_gdt(pointer: &DescriptorTablePointer) {
    #[cfg(target_os = "none")]
    unsafe {
        // Keep the implicit memory clobber across the descriptor-table switch.
        core::arch::asm!("lgdt [{}]", in(reg) pointer, options(nostack));
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = pointer;
    }
}

/// 在裸机上装载 IDT。
pub unsafe fn load_idt(pointer: &DescriptorTablePointer) {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("lidt [{}]", in(reg) pointer, options(nostack));
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = pointer;
    }
}

/// 装载当前任务的 TSS selector。
pub unsafe fn load_tr(selector: u16) {
    #[cfg(target_os = "none")]
    unsafe {
        // Linux's `load_tr()` uses a volatile asm with a memory clobber.  The
        // selector load changes the task-state context used by subsequent
        // privilege transitions, so allowing surrounding memory operations to
        // move across it would make the TSS publication/order observable out
        // of sequence on SMP bring-up.
        core::arch::asm!("ltr {0:x}", in(reg) selector, options(nostack));
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = selector;
    }
}

pub fn gdt_pointer(base: *const SegmentDescriptor, entries: usize) -> DescriptorTablePointer {
    DescriptorTablePointer::new(
        base as u64,
        entries
            .saturating_mul(size_of::<SegmentDescriptor>())
            .saturating_sub(1) as u16,
    )
}

pub fn tss_pointer(base: *const TaskStateSegment) -> DescriptorTablePointer {
    DescriptorTablePointer::new(base as u64, TaskStateSegment::TSS_DESCRIPTOR_LIMIT as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tss_defaults_to_denied_io_bitmap() {
        let tss = TaskStateSegment::default();
        assert_eq!(size_of::<TaskStateSegment>(), 105);
        assert_eq!(
            tss.iomap_base as usize,
            TaskStateSegment::IO_BITMAP_TERMINATOR_OFFSET
        );
        assert_eq!(tss.io_bitmap_terminator, 0xff);
        let descriptor_limit = tss_pointer(&tss).limit;
        assert_eq!(
            descriptor_limit,
            TaskStateSegment::TSS_DESCRIPTOR_LIMIT as u16
        );
    }

    #[test]
    fn tss_matches_packed_long_mode_offsets() {
        assert_eq!(core::mem::offset_of!(TaskStateSegment, rsp0), 4);
        assert_eq!(core::mem::offset_of!(TaskStateSegment, rsp1), 12);
        assert_eq!(core::mem::offset_of!(TaskStateSegment, ist), 36);
        assert_eq!(core::mem::offset_of!(TaskStateSegment, iomap_base), 102);
        assert_eq!(
            core::mem::offset_of!(TaskStateSegment, io_bitmap_terminator),
            104
        );
    }

    #[test]
    fn tss_ist_bounds_are_checked() {
        let mut tss = TaskStateSegment::default();
        assert!(tss.set_interrupt_stack(0, 0x1000));
        assert!(!tss.set_interrupt_stack(7, 0x1000));
    }

    #[test]
    fn emergency_ist_slots_are_distinct_and_architecturally_numbered() {
        assert_eq!(DOUBLE_FAULT_IST, 1);
        assert_eq!(NMI_IST, 2);
        assert_eq!(MACHINE_CHECK_IST, 3);
        assert_ne!(DOUBLE_FAULT_IST, NMI_IST);
        assert_ne!(NMI_IST, MACHINE_CHECK_IST);
        assert!(IST_STACK_SIZE >= 4096);
        assert_eq!(IST_STACK_SIZE % 16, 0);
    }
}

//! x86_64 local APIC/IOAPIC IRQ domain.
//!
//! The generic IRQ core deliberately does not know how an ACPI GSI becomes an
//! interrupt vector.  This module owns that translation and the corresponding
//! register programming.  The implementation follows the same ordering used
//! by Linux's IOAPIC code: mask a pin before changing trigger/polarity/vector,
//! write the high destination dword first, then publish the low dword.
//!
//! Hosted builds retain the validation and routing model but never dereference
//! a firmware supplied address.  This makes the parser and IRQ-domain tests
//! useful without pretending that a process can access APIC MMIO.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use general::dev::irq::{IrqDomain, IrqLine, IrqLineOps, IrqPolarity, IrqTrigger};
use general::firmware::acpi::{AcpiInterruptOverride, AcpiMadtInfo};
use spin::Mutex;

/// The controller id is intentionally outside the DTB phandle namespace.
pub const X86_ACPI_IRQ_CONTROLLER: u32 = 0x5846_3634; // "XF64"
pub const FIRST_EXTERNAL_VECTOR: u8 = 32;
pub const LAST_EXTERNAL_VECTOR: u8 = 255;
/// Vectors owned by the architecture rather than an IOAPIC device line.
/// Device vectors are allocated around these holes so a GSI can never be
/// mistaken for the scheduler timer, an IPI, or the LAPIC error/spurious
/// paths.
pub const TIMER_VECTOR: u8 = 0x20;
pub const RESCHEDULE_VECTOR: u8 = 0xf0;
pub const IPI_VECTOR: u8 = 0xf1;
pub const ERROR_VECTOR: u8 = 0xfe;
pub const SPURIOUS_VECTOR: u8 = 0xff;
const IOAPIC_REG_SELECT: usize = 0x00;
const IOAPIC_REG_WINDOW: usize = 0x10;
const IOAPIC_REG_VERSION: u8 = 1;
#[cfg(any(test, target_os = "none"))]
const IOAPIC_REDIR_BASE: u8 = 0x10;
const LAPIC_EOI: usize = 0x0b0;
#[cfg(target_os = "none")]
const LAPIC_ID: usize = 0x020;
#[cfg(target_os = "none")]
const LAPIC_ISR_BASE: usize = 0x100;
#[cfg(target_os = "none")]
const LAPIC_SVR: usize = 0x0f0;
#[cfg(target_os = "none")]
const LAPIC_ICR_HIGH: usize = 0x310;
#[cfg(target_os = "none")]
const LAPIC_ICR_LOW: usize = 0x300;
#[cfg(target_os = "none")]
const LAPIC_ICR_DELIVERY_STATUS: u32 = 1 << 12;
#[cfg(target_os = "none")]
const LAPIC_ICR_LEVEL_ASSERT: u32 = 1 << 14;
#[cfg(target_os = "none")]
const LAPIC_ICR_TRIGGER_LEVEL: u32 = 1 << 15;
#[cfg(target_os = "none")]
const LAPIC_ICR_DELIVERY_INIT: u32 = 0b101 << 8;
#[cfg(target_os = "none")]
const LAPIC_ICR_DELIVERY_STARTUP: u32 = 0b110 << 8;
#[cfg(target_os = "none")]
const LAPIC_ENABLE_BIT: u32 = 1 << 8;
/// Intel's MP startup sequence requires a settling interval after INIT
/// de-assertion and a short interval between the two SIPIs.  Use the stable
/// architectural counter rather than a CPU-dependent pause-loop count.
#[cfg(any(test, target_os = "none"))]
const INIT_DEASSERT_DELAY_NS: u64 = 10_000_000;
#[cfg(any(test, target_os = "none"))]
const SIPI_INTERVAL_NS: u64 = 200_000;
#[cfg(any(test, target_os = "none"))]
const NSEC_PER_SEC: u128 = 1_000_000_000;
// The IOAPIC register-select window is eight bits wide.  Redirection entries
// start at 0x10 and consume two register numbers, so entries beyond 120 would
// wrap the selector and program an unrelated register.  Reject such hardware
// advertisements rather than allowing an arithmetic truncation.
const MAX_IOAPIC_REDIRECTION_ENTRIES: u32 = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApicInitError {
    MissingMadt,
    InvalidAddress,
    InvalidGsiRange,
    OverlappingGsiRange,
    MappingUnavailable,
    NoInterruptController,
    AlreadyInitialized,
    Registry,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApicInitReport {
    pub local_apic: bool,
    pub ioapics: usize,
    pub gsi_count: usize,
    pub overrides: usize,
    pub detected_cpus: usize,
    pub online_cpus: usize,
}

#[derive(Clone, Copy, Debug)]
struct IoApic {
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    virt: usize,
    gsi_base: u32,
    gsi_end: u32,
    redirection_count: u32,
}

impl IoApic {
    fn contains(self, gsi: u32) -> bool {
        (self.gsi_base..=self.gsi_end).contains(&gsi)
    }

    #[cfg(target_os = "none")]
    #[inline]
    fn select_addr(self) -> usize {
        self.virt.saturating_add(IOAPIC_REG_SELECT)
    }

    #[cfg(target_os = "none")]
    #[inline]
    fn window_addr(self) -> usize {
        self.virt.saturating_add(IOAPIC_REG_WINDOW)
    }
}

#[derive(Debug)]
struct ApicState {
    local_apic: Option<usize>,
    ioapics: Vec<IoApic>,
    overrides: Vec<AcpiInterruptOverride>,
    has_legacy_pic: bool,
    /// Software mirrors make hosted tests deterministic and avoid MMIO access.
    #[cfg(not(target_os = "none"))]
    hosted_redirection: [u64; 256],
}

impl ApicState {
    fn ioapic_for(&self, gsi: u32) -> Option<IoApic> {
        self.ioapics
            .iter()
            .copied()
            .find(|ioapic| ioapic.contains(gsi))
    }

    fn override_for_isa(&self, irq: u8) -> Option<AcpiInterruptOverride> {
        self.overrides
            .iter()
            .copied()
            .find(|entry| entry.bus == 0 && entry.source == irq)
    }
}

/// ACPI default IRQ domain for an x86 machine.
pub struct X86AcpiIrqDomain {
    state: Mutex<ApicState>,
}

impl X86AcpiIrqDomain {
    fn new(state: ApicState) -> Self {
        Self {
            state: Mutex::new(state),
        }
    }

    pub fn gsi_for_isa_irq(&self, irq: u8) -> Option<u32> {
        if irq >= 16 {
            return None;
        }
        let state = self.state.lock();
        Some(
            state
                .override_for_isa(irq)
                .map_or(u32::from(irq), |entry| entry.global_system_interrupt),
        )
    }

    pub fn vector_for_gsi(gsi: u32) -> Option<u8> {
        let mut remaining = gsi;
        for vector in FIRST_EXTERNAL_VECTOR..=LAST_EXTERNAL_VECTOR {
            if is_reserved_device_vector(vector) {
                continue;
            }
            if remaining == 0 {
                return Some(vector);
            }
            remaining -= 1;
        }
        None
    }

    pub fn gsi_for_vector(vector: u8) -> Option<u32> {
        if vector < FIRST_EXTERNAL_VECTOR || is_reserved_device_vector(vector) {
            None
        } else {
            let mut gsi = 0u32;
            for candidate in FIRST_EXTERNAL_VECTOR..vector {
                if !is_reserved_device_vector(candidate) {
                    gsi = gsi.checked_add(1)?;
                }
            }
            Some(gsi)
        }
    }

    #[cfg(target_os = "none")]
    fn read_ioapic(&self, ioapic: IoApic, register: u8) -> u32 {
        #[cfg(target_os = "none")]
        unsafe {
            // The mapper validated the base and the register window before it
            // was stored.  Volatile access is required for APIC registers.
            write_volatile(ioapic.select_addr() as *mut u32, u32::from(register));
            return read_volatile(ioapic.window_addr() as *const u32);
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = (ioapic, register);
            0
        }
    }

    #[cfg(target_os = "none")]
    fn write_ioapic(&self, ioapic: IoApic, register: u8, value: u32) {
        #[cfg(target_os = "none")]
        unsafe {
            write_volatile(ioapic.select_addr() as *mut u32, u32::from(register));
            write_volatile(ioapic.window_addr() as *mut u32, value);
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = (ioapic, register, value);
        }
    }

    fn read_redirection(&self, state: &mut ApicState, gsi: u32) -> Option<u64> {
        let ioapic = state.ioapic_for(gsi)?;
        let index = gsi.checked_sub(ioapic.gsi_base)?;
        if index >= ioapic.redirection_count {
            return None;
        }
        #[cfg(target_os = "none")]
        {
            let low_register = redirection_register(index, false)?;
            let high_register = redirection_register(index, true)?;
            let low = self.read_ioapic(ioapic, low_register);
            let high = self.read_ioapic(ioapic, high_register);
            return Some((u64::from(high) << 32) | u64::from(low));
        }
        #[cfg(not(target_os = "none"))]
        {
            let index = usize::try_from(gsi).ok()?;
            return state.hosted_redirection.get(index).copied();
        }
    }

    fn write_redirection(&self, state: &mut ApicState, gsi: u32, value: u64) -> bool {
        let Some(ioapic) = state.ioapic_for(gsi) else {
            return false;
        };
        let Some(index) = gsi.checked_sub(ioapic.gsi_base) else {
            return false;
        };
        if index >= ioapic.redirection_count {
            return false;
        }
        #[cfg(target_os = "none")]
        {
            let Some(low_register) = redirection_register(index, false) else {
                return false;
            };
            let Some(high_register) = redirection_register(index, true) else {
                return false;
            };
            // Linux writes the destination (high dword) first while the pin is
            // masked, then publishes the low dword containing vector/mode.
            self.write_ioapic(ioapic, high_register, (value >> 32) as u32);
            self.write_ioapic(ioapic, low_register, value as u32);
        }
        #[cfg(not(target_os = "none"))]
        {
            let Some(index) = usize::try_from(gsi).ok() else {
                return false;
            };
            let Some(slot) = state.hosted_redirection.get_mut(index) else {
                return false;
            };
            *slot = value;
        }
        true
    }

    fn configure_gsi(
        &self,
        state: &mut ApicState,
        gsi: u32,
        trigger: Option<IrqTrigger>,
        polarity: Option<IrqPolarity>,
    ) -> bool {
        let Some(vector) = Self::vector_for_gsi(gsi) else {
            return false;
        };
        let Some(mut entry) = self.read_redirection(state, gsi) else {
            // A legacy PIC line has no IOAPIC redirection entry.  It is still a
            // valid domain line and is configured by set_line_enabled below.
            return state.has_legacy_pic && gsi < 16;
        };
        entry = (entry & !0xff) | u64::from(vector);
        match polarity {
            Some(IrqPolarity::Low) => entry |= 1 << 13,
            Some(IrqPolarity::High) => entry &= !(1 << 13),
            None => {}
        }
        match trigger {
            Some(IrqTrigger::Level) => entry |= 1 << 15,
            Some(IrqTrigger::Edge) => entry &= !(1 << 15),
            None => {}
        }
        // Keep a pin masked until the IRQ registry explicitly enables it.
        entry |= 1 << 16;
        self.write_redirection(state, gsi, entry)
    }

    fn set_enabled(&self, state: &mut ApicState, gsi: u32, enabled: bool) -> bool {
        if let Some(mut entry) = self.read_redirection(state, gsi) {
            if enabled {
                entry &= !(1 << 16);
            } else {
                entry |= 1 << 16;
            }
            return self.write_redirection(state, gsi, entry);
        }
        if state.has_legacy_pic && gsi < 16 {
            #[cfg(target_os = "none")]
            {
                let irq = gsi as u8;
                let (port, bit) = if irq < 8 {
                    (0x21u16, irq)
                } else {
                    (0xa1u16, irq - 8)
                };
                unsafe {
                    let mut mask = super::io::inb(port);
                    if enabled {
                        mask &= !(1 << bit);
                    } else {
                        mask |= 1 << bit;
                    }
                    super::io::outb(port, mask);
                }
            }
            return true;
        }
        false
    }
}

impl IrqDomain for X86AcpiIrqDomain {
    fn translate(&self, cells: &[u32]) -> Option<IrqLine> {
        let gsi = *cells.first()?;
        // ACPI GSI resources are represented by one cell.  Accept additional
        // cells only when they are zero, preserving strictness without
        // rejecting a controller-specific padding cell.
        if cells.len() > 1 && cells[1..].iter().any(|cell| *cell != 0) {
            return None;
        }
        let state = self.state.lock();
        if state.ioapic_for(gsi).is_some() || state.has_legacy_pic && gsi < 16 {
            Some(IrqLine::Controller {
                controller: X86_ACPI_IRQ_CONTROLLER,
                hwirq: gsi,
            })
        } else {
            None
        }
    }

    fn set_line_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        self.set_enabled(&mut self.state.lock(), hwirq, enabled)
    }

    fn configure_line(
        &self,
        hwirq: u32,
        trigger: Option<IrqTrigger>,
        polarity: Option<IrqPolarity>,
    ) -> bool {
        self.configure_gsi(&mut self.state.lock(), hwirq, trigger, polarity)
    }
}

static DOMAIN: Mutex<Option<Arc<X86AcpiIrqDomain>>> = Mutex::new(None);
static INITIALIZED: AtomicBool = AtomicBool::new(false);
/// Published after the MADT mapping has been validated and the APIC domain is
/// installed.  Fast interrupt paths (notably the local timer) must not take
/// `DOMAIN`'s mutex, since an interrupt can arrive while another CPU-local
/// operation is holding it.
static LOCAL_APIC_BASE: AtomicUsize = AtomicUsize::new(0);
const INVALID_GSI: u32 = u32::MAX;
/// Vector-to-GSI mappings consumed from hard-interrupt context.
///
/// This is deliberately an atomic, one-way publication rather than a second
/// lock around `ApicState`: an IRQ can arrive while an IRQ line is being
/// configured and must never spin on the configuration mutex.  Entries are
/// populated only after the domain and line callbacks have been registered.
static VECTOR_TO_GSI: [AtomicU32; 256] = [const { AtomicU32::new(INVALID_GSI) }; 256];

/// Return the validated virtual base of the local APIC, if one was mapped.
///
/// The value is published with release ordering only after the corresponding
/// `ApicState` is fully initialized.  A zero or unaligned value is never
/// exposed to callers, so a failed/partial MADT setup cannot turn into a
/// speculative MMIO access.
#[inline]
pub(crate) fn local_apic_base() -> Option<usize> {
    let base = LOCAL_APIC_BASE.load(Ordering::Acquire);
    (base != 0 && base & 0xfff == 0).then_some(base)
}

/// Read the eight-bit xAPIC id of the current processor.
pub(crate) fn local_apic_id() -> Option<u32> {
    let base = local_apic_base()?;
    #[cfg(target_os = "none")]
    unsafe {
        Some(read_volatile((base + LAPIC_ID) as *const u32) >> 24)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = base;
        Some(0)
    }
}

/// Write one local-APIC register without acquiring the IRQ-domain lock.
///
/// Only offsets within the 4 KiB LAPIC page and naturally aligned dword
/// accesses are accepted.  Hosted builds deliberately return `false` and do
/// not dereference the firmware-provided address.
#[inline]
pub(crate) fn write_local_apic(offset: usize, value: u32) -> bool {
    if offset > 0xfff || offset & 3 != 0 {
        return false;
    }
    let Some(base) = local_apic_base() else {
        return false;
    };
    let Some(address) = base.checked_add(offset) else {
        return false;
    };
    #[cfg(target_os = "none")]
    unsafe {
        write_volatile(address as *mut u32, value);
        true
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (address, value);
        false
    }
}

fn line_enable(line: IrqLine) -> bool {
    let IrqLine::Hardware(gsi) = line else {
        return match line {
            IrqLine::Controller {
                controller: X86_ACPI_IRQ_CONTROLLER,
                hwirq,
            } => DOMAIN
                .lock()
                .as_ref()
                .is_some_and(|domain| domain.set_line_enabled(hwirq, true)),
            _ => false,
        };
    };
    DOMAIN
        .lock()
        .as_ref()
        .is_some_and(|domain| domain.set_line_enabled(gsi as u32, true))
}

fn line_disable(line: IrqLine) -> bool {
    let gsi = match line {
        IrqLine::Hardware(gsi) => u32::try_from(gsi).ok(),
        IrqLine::Controller {
            controller: X86_ACPI_IRQ_CONTROLLER,
            hwirq,
        } => Some(hwirq),
        _ => None,
    };
    gsi.is_some_and(|gsi| {
        DOMAIN
            .lock()
            .as_ref()
            .is_some_and(|domain| domain.set_line_enabled(gsi, false))
    })
}

/// Install the ACPI APIC/IOAPIC domain and architecture line callbacks.
pub fn initialize_from_madt(
    madt: Option<&AcpiMadtInfo>,
    device_mmio_to_virt: fn(usize) -> usize,
) -> Result<ApicInitReport, ApicInitError> {
    let madt = madt.ok_or(ApicInitError::MissingMadt)?;
    let mut domain_slot = DOMAIN.lock();
    if INITIALIZED.load(Ordering::Acquire) {
        let report = domain_slot
            .as_ref()
            .map(|domain| {
                let state = domain.state.lock();
                ApicInitReport {
                    local_apic: state.local_apic.is_some(),
                    ioapics: state.ioapics.len(),
                    gsi_count: state
                        .ioapics
                        .iter()
                        .map(|ioapic| ioapic.redirection_count as usize)
                        .sum(),
                    overrides: state.overrides.len(),
                    detected_cpus: madt.processors.len(),
                    online_cpus: madt.processors.iter().filter(|cpu| cpu.usable()).count(),
                }
            })
            .unwrap_or_default();
        // The first caller may have arrived before the trap/MSR contract was
        // installed.  Retry the timer setup on every idempotent invocation;
        // the helper is lock-free and becomes a no-op once armed.
        super::time::initialize_local_timer();
        return Ok(report);
    }

    let local_apic = if madt.local_apic_address != 0 {
        let phys =
            usize::try_from(madt.local_apic_address).map_err(|_| ApicInitError::InvalidAddress)?;
        let virt = device_mmio_to_virt(phys);
        if virt == 0 || virt & 0xfff != 0 {
            return Err(ApicInitError::MappingUnavailable);
        }
        Some(virt)
    } else {
        None
    };

    let mut ioapics = Vec::new();
    for source in &madt.io_apics {
        if source.address == 0 || source.address & 0xfff != 0 {
            return Err(ApicInitError::InvalidAddress);
        }
        let virt = device_mmio_to_virt(source.address as usize);
        if virt == 0 || virt & 0xfff != 0 {
            return Err(ApicInitError::MappingUnavailable);
        }
        let redirection_count = if cfg!(target_os = "none") {
            // Version register is safe to read after the mapper has returned a
            // non-null address.  Bits 8..15 and 24..31 are reserved; bits 0..7
            // hold the version and bits 16..23 hold the maximum redirection
            // entry.  Keep the field decode in a pure helper so the hosted
            // tests exercise the same validation as the MMIO path.
            let version = read_ioapic_version(virt);
            ioapic_redirection_count(version).ok_or(ApicInitError::InvalidGsiRange)?
        } else {
            24
        };
        if redirection_count == 0 || redirection_count > MAX_IOAPIC_REDIRECTION_ENTRIES {
            return Err(ApicInitError::InvalidGsiRange);
        }
        let gsi_end = source
            .global_system_interrupt_base
            .checked_add(redirection_count - 1)
            .ok_or(ApicInitError::InvalidGsiRange)?;
        let ioapic = IoApic {
            virt,
            gsi_base: source.global_system_interrupt_base,
            gsi_end,
            redirection_count,
        };
        if ioapics.iter().any(|existing: &IoApic| {
            ioapic.gsi_base <= existing.gsi_end && existing.gsi_base <= ioapic.gsi_end
        }) {
            return Err(ApicInitError::OverlappingGsiRange);
        }
        ioapics.push(ioapic);
    }
    // The interrupt entry resolves shared external gates through the local
    // APIC ISR bitmap, and this backend programs GSIs through an IOAPIC.  A
    // legacy-PIC-only machine would require a separately remapped 8259 vector
    // table and PIC EOI path; accepting it here would make the first IRQ halt
    // in the LAPIC resolver.  Fail closed until that distinct backend exists.
    if local_apic.is_none() || ioapics.is_empty() {
        return Err(ApicInitError::NoInterruptController);
    }

    let domain = Arc::new(X86AcpiIrqDomain::new(ApicState {
        local_apic,
        ioapics,
        overrides: madt.interrupt_overrides.clone(),
        has_legacy_pic: madt.has_legacy_pic,
        #[cfg(not(target_os = "none"))]
        hosted_redirection: [0; 256],
    }));

    #[cfg(target_os = "none")]
    if let Some(local_apic) = local_apic {
        enable_local_apic(local_apic);
    }
    if !domain.state.lock().ioapics.is_empty() && madt.has_legacy_pic {
        mask_legacy_pic();
    }

    let domain_dyn: Arc<dyn IrqDomain> = domain.clone();
    let default_handle = general::dev::irq::register_default_irq_domain(domain_dyn.clone())
        .map_err(|_| ApicInitError::Registry)?;
    if general::dev::irq::register_irq_domain(X86_ACPI_IRQ_CONTROLLER, domain_dyn).is_err() {
        let _ = general::dev::irq::unregister_default_irq_domain(default_handle);
        return Err(ApicInitError::Registry);
    }
    general::dev::irq::install_irq_line_ops(IrqLineOps {
        enable: line_enable,
        disable: line_disable,
    });
    *domain_slot = Some(domain);
    // Publish the immutable vector snapshot only after the domain is visible.
    // A concurrent interrupt can therefore either fail closed (old/empty
    // entry) or resolve a complete line, but can never observe a half-built
    // mutex-protected state.
    if let Some(domain) = domain_slot.as_ref() {
        publish_vector_snapshot(&domain.state.lock());
    }
    // Publish the mapping only after the domain and line operations are
    // visible.  The timer backend consumes this atomic and never dereferences
    // the mutex-protected state from interrupt context.
    LOCAL_APIC_BASE.store(local_apic.unwrap_or(0), Ordering::Release);
    INITIALIZED.store(true, Ordering::Release);

    let report = {
        let state = domain_slot.as_ref().expect("domain installed").state.lock();
        ApicInitReport {
            local_apic: state.local_apic.is_some(),
            ioapics: state.ioapics.len(),
            gsi_count: state
                .ioapics
                .iter()
                .map(|ioapic| ioapic.redirection_count as usize)
                .sum(),
            overrides: state.overrides.len(),
            detected_cpus: madt.processors.len(),
            online_cpus: madt.processors.iter().filter(|cpu| cpu.usable()).count(),
        }
    };
    // APIC setup can happen before scheduler registration (and therefore
    // before the IDT).  The helper records no false success in that case;
    // sched_ctx::register retries after installing the trap entry.
    super::time::initialize_local_timer();
    drop(domain_slot);
    Ok(report)
}

#[inline]
fn read_ioapic_version(base: usize) -> u32 {
    unsafe {
        write_volatile(
            (base + IOAPIC_REG_SELECT) as *mut u32,
            u32::from(IOAPIC_REG_VERSION),
        );
        read_volatile((base + IOAPIC_REG_WINDOW) as *const u32)
    }
}

/// Decode an IOAPIC version register into the number of redirection entries.
///
/// The maximum-redirection field is inclusive, so a value of `n` denotes
/// `n + 1` entries.  Only bits 8..15 and 24..31 are reserved; the low version
/// byte is valid data and must not be rejected.
fn ioapic_redirection_count(version: u32) -> Option<u32> {
    if version & 0xff00_ff00 != 0 {
        return None;
    }
    let count = ((version >> 16) & 0xff).checked_add(1)?;
    (count <= 256).then_some(count)
}

#[inline]
#[cfg(any(test, target_os = "none"))]
fn redirection_register(index: u32, high: bool) -> Option<u8> {
    let offset = u32::from(IOAPIC_REDIR_BASE)
        .checked_add(index.checked_mul(2)?)?
        .checked_add(u32::from(high))?;
    u8::try_from(offset).ok()
}

#[inline]
fn is_reserved_device_vector(vector: u8) -> bool {
    matches!(
        vector,
        TIMER_VECTOR | RESCHEDULE_VECTOR | IPI_VECTOR | ERROR_VECTOR | SPURIOUS_VECTOR
    )
}

#[cfg(target_os = "none")]
#[inline]
fn enable_local_apic(base: usize) {
    #[cfg(target_os = "none")]
    unsafe {
        let svr = (base + LAPIC_SVR) as *mut u32;
        let value = read_volatile(svr);
        write_volatile(svr, value | LAPIC_ENABLE_BIT | 0xff);
    }
    #[cfg(not(target_os = "none"))]
    let _ = base;
}

/// Enable the LAPIC software bit on the current processor.
///
/// The MADT mapping is shared, but SVR is a CPU-local register and therefore
/// must be initialized again by every AP before enabling interrupts.
#[cfg(target_os = "none")]
pub(crate) fn initialize_current_local_apic() -> bool {
    let Some(base) = local_apic_base() else {
        return false;
    };
    enable_local_apic(base);
    true
}

/// Send an EOI for a vector handled by the x86 local APIC.
pub fn end_of_interrupt() {
    // EOI is on the local APIC page, not in the IOAPIC domain.  Use the
    // published lock-free base so an interrupt cannot deadlock on `DOMAIN`.
    let _ = write_local_apic(LAPIC_EOI, 0);
}

/// Read the highest-priority in-service LAPIC vector.
///
/// The trap entry uses one IDT gate for all external vectors.  The local APIC
/// keeps the architectural vector in its eight 32-bit ISR registers, so the
/// dispatch path must resolve it before consulting the generic IRQ domain.  A
/// missing domain, an unmapped LAPIC, or an empty ISR bitmap is reported as
/// `None`; callers then fail closed instead of guessing a line.
pub fn in_service_vector() -> Option<u8> {
    let base = local_apic_base()?;
    #[cfg(target_os = "none")]
    {
        if base == 0 || base & 0xfff != 0 {
            return None;
        }
        for register in (0..8usize).rev() {
            let value =
                unsafe { read_volatile((base + LAPIC_ISR_BASE + register * 0x10) as *const u32) };
            if value != 0 {
                let bit = 31usize.saturating_sub(value.leading_zeros() as usize);
                return u8::try_from(register * 32 + bit).ok();
            }
        }
        None
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = base;
        None
    }
}

/// Send a fixed IPI to a physical APIC id.  The caller must ensure the target
/// is online.  This path takes no shared lock and is valid for synchronous TLB
/// shootdown; local interrupt exclusion serializes this CPU's xAPIC ICR pair.
pub fn send_ipi(apic_id: u32, vector: u8) -> bool {
    if vector < 16 || apic_id > u32::from(u8::MAX) {
        return false;
    }
    let Some(base) = local_apic_base() else {
        return false;
    };
    #[cfg(target_os = "none")]
    {
        // xAPIC has one ICR pair per CPU.  Follow Linux's local-IRQ
        // serialization model so an interrupt-side sender cannot interleave
        // its destination write with this request.  No shared lock is taken;
        // this path is also used by synchronous TLB shootdown.
        let irq_state = super::interrupt::save_and_disable();
        let delivered = if wait_icr_idle(base) {
            unsafe {
                write_volatile((base + LAPIC_ICR_HIGH) as *mut u32, apic_id << 24);
                write_volatile((base + LAPIC_ICR_LOW) as *mut u32, u32::from(vector));
            }
            wait_icr_idle(base)
        } else {
            false
        };
        super::interrupt::restore(irq_state);
        delivered
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (apic_id, vector, base);
        true
    }
}

#[cfg(target_os = "none")]
fn wait_icr_idle(base: usize) -> bool {
    // The delivery-status bit is architecturally cleared by the LAPIC after
    // the bus transaction completes.  A bounded poll prevents a dead LAPIC
    // from hanging the boot CPU forever.
    for _ in 0..1_000_000 {
        let value = unsafe { read_volatile((base + LAPIC_ICR_LOW) as *const u32) };
        if value & LAPIC_ICR_DELIVERY_STATUS == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

#[inline]
#[cfg(any(test, target_os = "none"))]
fn delay_ticks_for_ns(delay_ns: u64, counter_hz: u64) -> u64 {
    let product = u128::from(delay_ns).saturating_mul(u128::from(counter_hz));
    let rounded = product.saturating_add(NSEC_PER_SEC - 1) / NSEC_PER_SEC;
    u64::try_from(rounded).unwrap_or(u64::MAX).max(1)
}

#[cfg(target_os = "none")]
#[inline(never)]
fn wait_stable_counter_delay(delay_ns: u64) {
    let ticks = delay_ticks_for_ns(delay_ns, super::time::stable_counter_hz());
    let start = super::time::stable_counter_raw_ordered();
    while super::time::stable_counter_raw_ordered().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

/// Issue the Intel INIT, de-assert, SIPI, SIPI sequence to one xAPIC target.
///
/// APIC ids wider than eight bits require x2APIC MSR mode, which this backend
/// does not enable yet; rejecting them is preferable to waking an unintended
/// processor.  Hosted builds return false and never emulate privileged IPI.
#[cfg(target_os = "none")]
pub(crate) fn send_init_sipi(apic_id: u32, vector: u8) -> bool {
    if apic_id > 0xff || vector == 0 {
        return false;
    }
    let Some(base) = local_apic_base() else {
        return false;
    };
    unsafe {
        write_volatile((base + LAPIC_ICR_HIGH) as *mut u32, apic_id << 24);
        write_volatile(
            (base + LAPIC_ICR_LOW) as *mut u32,
            LAPIC_ICR_DELIVERY_INIT | LAPIC_ICR_LEVEL_ASSERT | LAPIC_ICR_TRIGGER_LEVEL,
        );
        if !wait_icr_idle(base) {
            return false;
        }
        write_volatile(
            (base + LAPIC_ICR_LOW) as *mut u32,
            LAPIC_ICR_DELIVERY_INIT | LAPIC_ICR_TRIGGER_LEVEL,
        );
        if !wait_icr_idle(base) {
            return false;
        }
        // Allow the target to finish its INIT reset before the first SIPI.
        wait_stable_counter_delay(INIT_DEASSERT_DELAY_NS);
        for attempt in 0..2 {
            write_volatile(
                (base + LAPIC_ICR_LOW) as *mut u32,
                LAPIC_ICR_DELIVERY_STARTUP | u32::from(vector),
            );
            if !wait_icr_idle(base) {
                return false;
            }
            if attempt == 0 {
                // Intel specifies roughly 200 us between SIPIs.  Do not add
                // an unnecessary delay after the second (final) SIPI.
                wait_stable_counter_delay(SIPI_INTERVAL_NS);
            }
        }
        true
    }
}

/// Convert a CPU interrupt vector into the line registered in the generic IRQ
/// registry.  Exceptions and the timer are handled by the trap module.
pub fn line_for_vector(vector: u8) -> Option<IrqLine> {
    if vector < FIRST_EXTERNAL_VECTOR || is_reserved_device_vector(vector) {
        return None;
    }
    let gsi = VECTOR_TO_GSI[usize::from(vector)].load(Ordering::Acquire);
    (gsi != INVALID_GSI).then_some(IrqLine::Controller {
        controller: X86_ACPI_IRQ_CONTROLLER,
        hwirq: gsi,
    })
}

fn publish_vector_snapshot(state: &ApicState) {
    for entry in &VECTOR_TO_GSI {
        entry.store(INVALID_GSI, Ordering::Relaxed);
    }

    let publish_gsi = |gsi: u32| {
        let Some(vector) = X86AcpiIrqDomain::vector_for_gsi(gsi) else {
            return;
        };
        VECTOR_TO_GSI[usize::from(vector)].store(gsi, Ordering::Release);
    };

    if state.has_legacy_pic {
        for gsi in 0..16 {
            publish_gsi(gsi);
        }
    }
    for ioapic in &state.ioapics {
        let mut gsi = ioapic.gsi_base;
        loop {
            publish_gsi(gsi);
            if gsi == ioapic.gsi_end {
                break;
            }
            gsi = gsi.saturating_add(1);
        }
    }
}

fn mask_legacy_pic() {
    #[cfg(target_os = "none")]
    unsafe {
        super::io::outb(0x21, 0xff);
        super::io::outb(0xa1, 0xff);
    }
}

/// Return the MADT ISO mapping for an ISA source, preserving ACPI's conforming
/// polarity/trigger values for the caller that programs the line.
pub fn isa_override(irq: u8) -> Option<AcpiInterruptOverride> {
    DOMAIN
        .lock()
        .as_ref()
        .and_then(|domain| domain.state.lock().override_for_isa(irq))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use general::firmware::acpi::{
        AcpiInterruptAttributes, AcpiInterruptPolarity, AcpiInterruptTrigger, AcpiIoApic,
    };

    fn madt() -> AcpiMadtInfo {
        AcpiMadtInfo {
            local_apic_address: 0xfee0_0000,
            has_legacy_pic: true,
            io_apics: vec![AcpiIoApic {
                id: 1,
                address: 0xfec0_0000,
                global_system_interrupt_base: 0,
            }],
            interrupt_overrides: vec![AcpiInterruptOverride {
                bus: 0,
                source: 0,
                global_system_interrupt: 2,
                attributes: AcpiInterruptAttributes {
                    polarity: AcpiInterruptPolarity::ActiveHigh,
                    trigger: AcpiInterruptTrigger::Edge,
                },
            }],
            ..AcpiMadtInfo::default()
        }
    }

    #[test]
    fn vector_mapping_is_bounded_and_round_trips() {
        assert_eq!(X86AcpiIrqDomain::vector_for_gsi(0), Some(33));
        assert_eq!(X86AcpiIrqDomain::gsi_for_vector(33), Some(0));
        assert_eq!(X86AcpiIrqDomain::gsi_for_vector(TIMER_VECTOR), None);
        assert_eq!(X86AcpiIrqDomain::gsi_for_vector(IPI_VECTOR), None);
        assert_ne!(X86AcpiIrqDomain::vector_for_gsi(209), Some(IPI_VECTOR));
        assert_eq!(X86AcpiIrqDomain::vector_for_gsi(224), None);
        assert_eq!(X86AcpiIrqDomain::gsi_for_vector(31), None);
    }

    #[test]
    fn ioapic_register_selector_is_checked_before_narrowing() {
        assert_eq!(redirection_register(0, false), Some(0x10));
        assert_eq!(redirection_register(119, true), Some(0xff));
        assert_eq!(redirection_register(120, false), None);
        assert_eq!(redirection_register(u32::MAX, false), None);
    }

    #[test]
    fn ioapic_version_accepts_valid_version_byte_and_decodes_inclusive_max() {
        // Version 0x20, maximum redirection entry 23 => 24 entries.
        assert_eq!(ioapic_redirection_count(0x0017_0020), Some(24));
        // The maximum field can legally advertise all 256 entries.
        assert_eq!(ioapic_redirection_count(0x00ff_0020), Some(256));
        // Reserved bits are rejected, while a non-zero low version byte is not.
        assert_eq!(ioapic_redirection_count(0x0000_0120), None);
        assert_eq!(ioapic_redirection_count(0x0100_0020), None);
    }

    #[test]
    fn hosted_domain_translates_gsi_and_iso() {
        let source = madt();
        let domain = X86AcpiIrqDomain::new(ApicState {
            local_apic: Some(0xfee0_0000),
            ioapics: vec![IoApic {
                virt: 0xfec0_0000,
                gsi_base: 0,
                gsi_end: 23,
                redirection_count: 24,
            }],
            overrides: source.interrupt_overrides,
            has_legacy_pic: true,
            hosted_redirection: [0; 256],
        });
        assert!(matches!(
            domain.translate(&[2]),
            Some(IrqLine::Controller {
                controller: X86_ACPI_IRQ_CONTROLLER,
                hwirq: 2
            })
        ));
        assert_eq!(domain.gsi_for_isa_irq(0), Some(2));
        assert!(domain.configure_line(2, Some(IrqTrigger::Edge), Some(IrqPolarity::High)));
        assert!(domain.set_line_enabled(2, true));
    }

    #[test]
    fn rejects_overlapping_gsi_ranges() {
        let mut source = madt();
        source.io_apics.push(AcpiIoApic {
            id: 2,
            address: 0xfec0_1000,
            global_system_interrupt_base: 16,
        });
        assert_eq!(
            initialize_from_madt(Some(&source), |address| address),
            Err(ApicInitError::OverlappingGsiRange)
        );
    }

    #[test]
    fn init_sipi_delays_convert_to_counter_ticks_with_ceil() {
        assert_eq!(
            delay_ticks_for_ns(INIT_DEASSERT_DELAY_NS, 1_000_000_000),
            10_000_000
        );
        assert_eq!(delay_ticks_for_ns(SIPI_INTERVAL_NS, 2_500_000_000), 500_000);
        // A non-zero duration always waits at least one counter tick, even
        // when the advertised frequency is below one tick per nanosecond.
        assert_eq!(delay_ticks_for_ns(1, 1), 1);
    }

    #[test]
    fn init_sipi_delay_constants_follow_mp_ordering() {
        assert!(INIT_DEASSERT_DELAY_NS >= 10_000_000);
        assert!(SIPI_INTERVAL_NS >= 200_000);
        assert!(INIT_DEASSERT_DELAY_NS > SIPI_INTERVAL_NS);
    }
}

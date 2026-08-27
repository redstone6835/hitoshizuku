use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpiTableError {
    InvalidSignature,
    InvalidLength,
    InvalidChecksum,
    InvalidAddress,
    InvalidFlags,
    InvalidReference,
    DuplicateEntry,
    OverlappingRange,
    TruncatedEntry,
    UnsupportedAddressSpace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpiAddressSpace {
    SystemMemory,
    SystemIo,
    PciConfig,
    EmbeddedController,
    SmBus,
    SystemCmos,
    PciBarTarget,
    Ipmi,
    GeneralPurposeIo,
    GenericSerialBus,
    PlatformCommunicationsChannel,
    FunctionalFixedHardware,
    Oem(u8),
    Unknown(u8),
}

impl AcpiAddressSpace {
    pub const fn from_raw(value: u8) -> Self {
        match value {
            0x00 => Self::SystemMemory,
            0x01 => Self::SystemIo,
            0x02 => Self::PciConfig,
            0x03 => Self::EmbeddedController,
            0x04 => Self::SmBus,
            0x05 => Self::SystemCmos,
            0x06 => Self::PciBarTarget,
            0x07 => Self::Ipmi,
            0x08 => Self::GeneralPurposeIo,
            0x09 => Self::GenericSerialBus,
            0x0a => Self::PlatformCommunicationsChannel,
            0x7f => Self::FunctionalFixedHardware,
            0xc0..=0xff => Self::Oem(value),
            _ => Self::Unknown(value),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiGenericAddress {
    pub address_space: AcpiAddressSpace,
    pub bit_width: u8,
    pub bit_offset: u8,
    pub access_size: u8,
    pub address: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpiInterruptPolarity {
    Conforms,
    ActiveHigh,
    ActiveLow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpiInterruptTrigger {
    Conforms,
    Edge,
    Level,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiInterruptAttributes {
    pub polarity: AcpiInterruptPolarity,
    pub trigger: AcpiInterruptTrigger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpiProcessorInterface {
    LocalApic,
    LocalX2Apic,
    LocalSapic,
    Gicc,
    LoongArchCorePic,
    RiscVIntc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiProcessor {
    pub interface: AcpiProcessorInterface,
    pub processor_uid: u32,
    pub hardware_id: u64,
    pub enabled: bool,
    pub online_capable: bool,
    /// The interrupt-controller interface requires explicit cache maintenance.
    /// This is currently defined by the GICC MADT entry; other interfaces set it to false.
    pub interrupt_controller_non_coherent: bool,
}

impl AcpiProcessor {
    pub const fn usable(self) -> bool {
        self.enabled || self.online_capable
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiIoApic {
    pub id: u8,
    pub address: u32,
    pub global_system_interrupt_base: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiInterruptOverride {
    pub bus: u8,
    pub source: u8,
    pub global_system_interrupt: u32,
    pub attributes: AcpiInterruptAttributes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiNmiSource {
    pub global_system_interrupt: u32,
    pub attributes: AcpiInterruptAttributes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiLocalNmi {
    pub processor_uid: Option<u32>,
    pub lint: u8,
    pub attributes: AcpiInterruptAttributes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiMultiprocessorWakeup {
    pub mailbox_version: u16,
    pub mailbox_address: u64,
    pub reset_vector: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpiMadtInfo {
    pub local_apic_address: u64,
    pub has_legacy_pic: bool,
    pub processors: Vec<AcpiProcessor>,
    pub io_apics: Vec<AcpiIoApic>,
    pub interrupt_overrides: Vec<AcpiInterruptOverride>,
    pub nmi_sources: Vec<AcpiNmiSource>,
    pub local_nmis: Vec<AcpiLocalNmi>,
    pub multiprocessor_wakeup: Option<AcpiMultiprocessorWakeup>,
    pub unknown_entry_types: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiPciConfigRegion {
    pub segment: u16,
    pub bus_start: u8,
    pub bus_end: u8,
    /// MCFG allocation base. Per the ACPI ECAM formula this address corresponds to bus 0,
    /// even when the allocation advertises a non-zero start bus.
    pub segment_base_address: usize,
    /// First byte of the advertised bus range (`segment_base_address + bus_start * 1 MiB`).
    pub physical_address: usize,
    pub size: usize,
}

impl AcpiPciConfigRegion {
    pub fn address(self, bus: u8, device: u8, function: u8, offset: u16) -> Option<usize> {
        if !(self.bus_start..=self.bus_end).contains(&bus)
            || device >= 32
            || function >= 8
            || offset >= 4096
        {
            return None;
        }
        self.segment_base_address
            .checked_add(usize::from(bus) << 20)?
            .checked_add(usize::from(device) << 15)?
            .checked_add(usize::from(function) << 12)?
            .checked_add(usize::from(offset))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiHpetInfo {
    pub event_timer_block_id: u32,
    pub base: AcpiGenericAddress,
    pub sequence: u8,
    pub minimum_tick: u16,
    pub page_protection: u8,
    pub oem_attributes: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpiSpcrInfo {
    pub interface_type: u8,
    pub base: AcpiGenericAddress,
    pub interrupt_type: u8,
    pub legacy_irq: Option<u8>,
    pub global_system_interrupt: Option<u32>,
    pub baud: Option<u32>,
    pub clock_hz: Option<u32>,
    pub namespace: Option<Box<str>>,
}

impl AcpiHpetInfo {
    pub const fn hardware_revision(self) -> u8 {
        self.event_timer_block_id as u8
    }

    pub const fn comparator_count(self) -> u8 {
        (((self.event_timer_block_id >> 8) & 0x1f) as u8) + 1
    }

    pub const fn counter_is_64_bit(self) -> bool {
        self.event_timer_block_id & (1 << 13) != 0
    }

    pub const fn legacy_replacement_capable(self) -> bool {
        self.event_timer_block_id & (1 << 15) != 0
    }

    pub const fn vendor_id(self) -> u16 {
        (self.event_timer_block_id >> 16) as u16
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiPmTimerInfo {
    pub register: AcpiGenericAddress,
    pub supports_32_bit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpiFadtInfo {
    pub preferred_profile: u8,
    pub sci_interrupt: u16,
    pub smi_command_port: u32,
    pub acpi_enable: u8,
    pub acpi_disable: u8,
    pub boot_architecture_flags: u16,
    pub flags: u32,
    pub pm_timer: Option<AcpiPmTimerInfo>,
    pub pm1a_control: Option<AcpiGenericAddress>,
    pub pm1b_control: Option<AcpiGenericAddress>,
    pub sleep_control: Option<AcpiGenericAddress>,
    pub sleep_status: Option<AcpiGenericAddress>,
    pub reset_register: Option<AcpiGenericAddress>,
    pub reset_value: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpiNumaProcessorKind {
    LocalApic,
    LocalX2Apic,
    Gicc,
    RiscVIntc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiNumaProcessorAffinity {
    pub kind: AcpiNumaProcessorKind,
    /// Interrupt-controller hardware ID. LAPIC/x2APIC entries identify CPUs this way.
    pub hardware_id: Option<u64>,
    /// ACPI processor UID. GICC and RINTC affinity entries identify CPUs this way.
    pub processor_uid: Option<u32>,
    pub proximity_domain: u32,
    pub clock_domain: u32,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiNumaMemoryAffinity {
    pub proximity_domain: u32,
    pub base: u64,
    pub length: u64,
    pub enabled: bool,
    pub hot_pluggable: bool,
    pub non_volatile: bool,
    pub specific_purpose: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpiNumaInitiatorKind {
    GicIts,
    GenericInitiator,
    GenericPort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpiNumaDeviceHandle {
    GicIts {
        id: u32,
    },
    Acpi {
        hid: u64,
        uid: u32,
    },
    Pci {
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiNumaInitiatorAffinity {
    pub kind: AcpiNumaInitiatorKind,
    pub handle: AcpiNumaDeviceHandle,
    pub proximity_domain: u32,
    pub enabled: bool,
    pub architectural_transactions: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpiSratInfo {
    pub processor_affinities: Vec<AcpiNumaProcessorAffinity>,
    pub memory_affinities: Vec<AcpiNumaMemoryAffinity>,
    pub initiator_affinities: Vec<AcpiNumaInitiatorAffinity>,
    pub unknown_entry_types: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpiSlitInfo {
    pub locality_count: usize,
    pub distances: Vec<u8>,
}

impl AcpiSlitInfo {
    pub fn distance(&self, from: usize, to: usize) -> Option<u8> {
        let index = from.checked_mul(self.locality_count)?.checked_add(to)?;
        self.distances.get(index).copied()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpiPpttProcessor {
    pub table_offset: u32,
    pub parent_offset: Option<u32>,
    pub processor_uid: Option<u32>,
    pub physical_package: bool,
    pub is_thread: bool,
    pub is_leaf: bool,
    pub identical_implementation: bool,
    pub private_resource_offsets: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiPpttCache {
    pub table_offset: u32,
    pub next_level_offset: Option<u32>,
    pub size: Option<u32>,
    pub sets: Option<u32>,
    pub associativity: Option<u8>,
    pub allocation_type: Option<u8>,
    pub cache_type: Option<u8>,
    pub write_through: Option<bool>,
    pub line_size: Option<u16>,
    pub cache_id: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpiPpttInfo {
    pub processors: Vec<AcpiPpttProcessor>,
    pub caches: Vec<AcpiPpttCache>,
    pub unknown_structure_types: Vec<u8>,
}

impl AcpiPpttInfo {
    pub fn processor_for_uid(&self, uid: u32) -> Option<&AcpiPpttProcessor> {
        let mut matches = self
            .processors
            .iter()
            .filter(|processor| processor.is_leaf && processor.processor_uid == Some(uid));
        let processor = matches.next()?;
        matches.next().is_none().then_some(processor)
    }

    pub fn processor_at(&self, offset: u32) -> Option<&AcpiPpttProcessor> {
        self.processors
            .iter()
            .find(|processor| processor.table_offset == offset)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpiPlatformInfo {
    pub madt: Option<AcpiMadtInfo>,
    pub fadt: Option<AcpiFadtInfo>,
    pub pci_config_regions: Vec<AcpiPciConfigRegion>,
    /// 按 HPET Sequence Number 排序的全部 Event Timer Block。
    pub hpets: Vec<AcpiHpetInfo>,
    pub srat: Option<AcpiSratInfo>,
    pub slit: Option<AcpiSlitInfo>,
    pub pptt: Option<AcpiPpttInfo>,
}

static PLATFORM_INFO: Spinlock<Option<Arc<AcpiPlatformInfo>>> = Spinlock::new(None);

pub fn install_platform_info(info: AcpiPlatformInfo) -> Arc<AcpiPlatformInfo> {
    let info = Arc::new(info);
    *PLATFORM_INFO.lock() = Some(Arc::clone(&info));
    info
}

pub fn platform_info() -> Option<Arc<AcpiPlatformInfo>> {
    PLATFORM_INFO.lock().as_ref().map(Arc::clone)
}

pub(super) fn checked_sdt<'a>(
    bytes: &'a [u8],
    signature: &[u8; 4],
    minimum_length: usize,
) -> Result<&'a [u8], AcpiTableError> {
    if bytes.get(..4) != Some(signature.as_slice()) {
        return Err(AcpiTableError::InvalidSignature);
    }
    let length = read_u32(bytes, 4)
        .and_then(|length| usize::try_from(length).ok())
        .ok_or(AcpiTableError::InvalidLength)?;
    if length < minimum_length || length > bytes.len() {
        return Err(AcpiTableError::InvalidLength);
    }
    let table = &bytes[..length];
    if table.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
        return Err(AcpiTableError::InvalidChecksum);
    }
    Ok(table)
}

pub(super) fn parse_gas(bytes: &[u8], offset: usize) -> Option<AcpiGenericAddress> {
    let gas = bytes.get(offset..offset.checked_add(12)?)?;
    Some(AcpiGenericAddress {
        address_space: AcpiAddressSpace::from_raw(gas[0]),
        bit_width: gas[1],
        bit_offset: gas[2],
        access_size: gas[3],
        address: read_u64(gas, 4)?,
    })
}

pub(super) fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

pub(super) fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

pub(super) fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}

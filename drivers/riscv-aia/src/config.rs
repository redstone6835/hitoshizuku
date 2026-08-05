//! RISC-V AIA DT binding 与 APLIC/IMSIC 布局的纯逻辑校验。

extern crate alloc;

use alloc::vec::Vec;

pub(crate) const IMSIC_PAGE_SIZE: u64 = 0x1000;
pub(crate) const IMSIC_MIN_ID: u32 = 63;
pub(crate) const IMSIC_MAX_ID: u32 = 2047;
pub(crate) const APLIC_MAX_SOURCE: u32 = 1023;
pub(crate) const APLIC_DOMAINCFG: usize = 0x0000;
pub(crate) const APLIC_SOURCECFG_BASE: usize = 0x0004;
pub(crate) const APLIC_SETIPNUM_LE: usize = 0x2000;
pub(crate) const APLIC_SETIENUM: usize = 0x1edc;
pub(crate) const APLIC_CLRIENUM: usize = 0x1fdc;
pub(crate) const APLIC_TARGET_BASE: usize = 0x3004;
pub(crate) const APLIC_IDC_BASE: usize = 0x4000;
pub(crate) const APLIC_IDC_SIZE: usize = 0x20;
pub(crate) const APLIC_IDC_IDELIVERY: usize = 0x00;
pub(crate) const APLIC_IDC_ITHRESHOLD: usize = 0x08;
pub(crate) const APLIC_IDC_CLAIMI: usize = 0x1c;
pub(crate) const APLIC_DOMAINCFG_IE: u32 = 1 << 8;
pub(crate) const APLIC_DOMAINCFG_DM: u32 = 1 << 2;

const RISCV_SUPERVISOR_EXTERNAL_IRQ: u32 = 9;
const REGISTER_WIDTH: usize = core::mem::size_of::<u32>();
const APLIC_TARGET_HART_SHIFT: u32 = 18;
const APLIC_TARGET_HART_MASK: u32 = 0x3fff;
const APLIC_TARGET_GUEST_SHIFT: u32 = 12;
const APLIC_TARGET_GUEST_MASK: u32 = 0x3f;
const APLIC_TARGET_EIID_MASK: u32 = 0x7ff;
const APLIC_TARGET_IPRIO_MASK: u32 = 0xff;
const APLIC_CLAIMI_ID_SHIFT: u32 = 16;
const APLIC_CLAIMI_ID_MASK: u32 = 0x3ff;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AiaConfigError {
    MissingNumIds,
    MalformedNumIds,
    InvalidNumIds,
    MalformedOptionalProperty,
    MissingInterruptContexts,
    MissingSupervisorContext,
    MalformedInterruptContext,
    UnsupportedInterruptContext,
    UnknownSupervisorContextCpu,
    DuplicateSupervisorContext,
    MalformedHartIndexes,
    InvalidHartIndex,
    DuplicateHartIndex,
    MissingMmio,
    UnalignedMmio,
    MmioWindowTooSmall,
    AddressOverflow,
    InvalidAddressScheme,
    DuplicateInterruptFile,
    MissingNumSources,
    MalformedNumSources,
    InvalidNumSources,
    MissingMsiParent,
    MalformedMsiParent,
    UnsupportedIrqType,
    OutOfMemory,
}

fn parse_be_u32(raw: Option<&[u8]>, missing: AiaConfigError) -> Result<u32, AiaConfigError> {
    let raw = raw.ok_or(missing)?;
    let bytes: [u8; 4] = raw.try_into().map_err(|_| match missing {
        AiaConfigError::MissingNumIds => AiaConfigError::MalformedNumIds,
        AiaConfigError::MissingNumSources => AiaConfigError::MalformedNumSources,
        AiaConfigError::MissingMsiParent => AiaConfigError::MalformedMsiParent,
        _ => AiaConfigError::MalformedOptionalProperty,
    })?;
    Ok(u32::from_be_bytes(bytes))
}

pub(crate) fn parse_num_ids(raw: Option<&[u8]>) -> Result<u32, AiaConfigError> {
    let num_ids = parse_be_u32(raw, AiaConfigError::MissingNumIds)?;
    if num_ids < IMSIC_MIN_ID || num_ids > IMSIC_MAX_ID || num_ids & IMSIC_MIN_ID != IMSIC_MIN_ID {
        return Err(AiaConfigError::InvalidNumIds);
    }
    Ok(num_ids)
}

pub(crate) fn parse_num_sources(raw: Option<&[u8]>) -> Result<u32, AiaConfigError> {
    let num_sources = parse_be_u32(raw, AiaConfigError::MissingNumSources)?;
    if num_sources == 0 || num_sources > APLIC_MAX_SOURCE {
        return Err(AiaConfigError::InvalidNumSources);
    }
    Ok(num_sources)
}

pub(crate) fn parse_msi_parent(raw: Option<&[u8]>) -> Result<u32, AiaConfigError> {
    let parent = parse_be_u32(raw, AiaConfigError::MissingMsiParent)?;
    if parent == 0 {
        return Err(AiaConfigError::MalformedMsiParent);
    }
    Ok(parent)
}

pub(crate) fn parse_optional_u32(raw: Option<&[u8]>) -> Result<Option<u32>, AiaConfigError> {
    raw.map(|raw| parse_be_u32(Some(raw), AiaConfigError::MalformedOptionalProperty))
        .transpose()
}

#[derive(Clone, Copy)]
pub(crate) struct ImsicInterruptContext<'a> {
    pub(crate) controller: Option<u32>,
    pub(crate) cells: &'a [u32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImsicSupervisorContext {
    pub(crate) logical_cpu: usize,
    pub(crate) hart_id: u64,
    pub(crate) file_index: usize,
}

pub(crate) fn select_supervisor_contexts<'a, I, F, G>(
    contexts: I,
    mut cpu_reg_for_controller: F,
    mut cpu_logical_id_for_controller: G,
) -> Result<Vec<ImsicSupervisorContext>, AiaConfigError>
where
    I: IntoIterator<Item = ImsicInterruptContext<'a>>,
    F: FnMut(u32) -> Option<u64>,
    G: FnMut(u32) -> Option<usize>,
{
    let mut saw_context = false;
    let mut selected = Vec::new();
    for (file_index, context) in contexts.into_iter().enumerate() {
        saw_context = true;
        let controller = context
            .controller
            .ok_or(AiaConfigError::MalformedInterruptContext)?;
        let [interrupt] = context.cells else {
            return Err(AiaConfigError::MalformedInterruptContext);
        };
        if *interrupt != RISCV_SUPERVISOR_EXTERNAL_IRQ {
            return Err(AiaConfigError::UnsupportedInterruptContext);
        }
        let hart_id = cpu_reg_for_controller(controller)
            .ok_or(AiaConfigError::UnknownSupervisorContextCpu)?;
        let logical_cpu = cpu_logical_id_for_controller(controller)
            .ok_or(AiaConfigError::UnknownSupervisorContextCpu)?;
        if selected
            .iter()
            .any(|entry: &ImsicSupervisorContext| entry.logical_cpu == logical_cpu)
        {
            return Err(AiaConfigError::DuplicateSupervisorContext);
        }
        selected
            .try_reserve(1)
            .map_err(|_| AiaConfigError::OutOfMemory)?;
        selected.push(ImsicSupervisorContext {
            logical_cpu,
            hart_id,
            file_index,
        });
    }
    if !saw_context {
        return Err(AiaConfigError::MissingInterruptContexts);
    }
    if selected.is_empty() {
        return Err(AiaConfigError::MissingSupervisorContext);
    }
    Ok(selected)
}

pub(crate) fn parse_aplic_hart_indexes(
    raw: Option<&[u8]>,
    context_count: usize,
) -> Result<Vec<u32>, AiaConfigError> {
    if context_count == 0 || context_count > APLIC_TARGET_HART_MASK as usize + 1 {
        return Err(AiaConfigError::InvalidHartIndex);
    }
    let expected_len = context_count
        .checked_mul(core::mem::size_of::<u32>())
        .ok_or(AiaConfigError::InvalidHartIndex)?;
    if raw.is_some_and(|value| value.len() != expected_len) {
        return Err(AiaConfigError::MalformedHartIndexes);
    }

    let mut indexes = Vec::new();
    indexes
        .try_reserve(context_count)
        .map_err(|_| AiaConfigError::OutOfMemory)?;
    match raw {
        Some(raw) => {
            for bytes in raw.chunks_exact(core::mem::size_of::<u32>()) {
                indexes.push(u32::from_be_bytes(
                    bytes
                        .try_into()
                        .map_err(|_| AiaConfigError::MalformedHartIndexes)?,
                ));
            }
        }
        None => indexes.extend((0..context_count).map(|index| index as u32)),
    }
    validate_aplic_hart_indexes(&indexes)?;
    Ok(indexes)
}

fn validate_aplic_hart_indexes(indexes: &[u32]) -> Result<(), AiaConfigError> {
    if indexes.is_empty() || indexes.len() > APLIC_TARGET_HART_MASK as usize + 1 {
        return Err(AiaConfigError::InvalidHartIndex);
    }
    const SEEN_WORDS: usize = (APLIC_TARGET_HART_MASK as usize + 1).div_ceil(u64::BITS as usize);
    let mut seen = [0u64; SEEN_WORDS];
    for &hart_index in indexes {
        if hart_index > APLIC_TARGET_HART_MASK {
            return Err(AiaConfigError::InvalidHartIndex);
        }
        let word = hart_index as usize / u64::BITS as usize;
        let bit = 1u64 << (hart_index as usize % u64::BITS as usize);
        if seen[word] & bit != 0 {
            return Err(AiaConfigError::DuplicateHartIndex);
        }
        seen[word] |= bit;
    }
    Ok(())
}

pub(crate) fn aplic_service_hart_index(
    contexts: &[ImsicSupervisorContext],
    hart_indexes: &[u32],
    boot_hart_id: u64,
) -> Result<Option<u32>, AiaConfigError> {
    if contexts.len() != hart_indexes.len() {
        return Err(AiaConfigError::MalformedHartIndexes);
    }
    validate_aplic_hart_indexes(hart_indexes)?;
    Ok(contexts
        .iter()
        .find(|context| context.hart_id == boot_hart_id)
        .or_else(|| contexts.first())
        .and_then(|context| hart_indexes.get(context.file_index))
        .copied())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MmioRange {
    pub(crate) phys: u64,
    pub(crate) size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImsicAddressScheme {
    pub(crate) guest_index_bits: u8,
    pub(crate) hart_index_bits: u8,
    pub(crate) group_index_bits: u8,
    pub(crate) group_index_shift: u8,
    pub(crate) base_addr: u64,
}

impl ImsicAddressScheme {
    fn checked_mask(bits: u8) -> Option<u64> {
        match bits {
            0 => Some(0),
            1..=63 => Some((1u64 << bits) - 1),
            _ => None,
        }
    }

    fn normalized_base(self, address: u64) -> Option<u64> {
        let low_bits = 12u8
            .checked_add(self.guest_index_bits)?
            .checked_add(self.hart_index_bits)?;
        let low_mask = Self::checked_mask(low_bits)?;
        let group_mask = Self::checked_mask(self.group_index_bits)?
            .checked_shl(u32::from(self.group_index_shift))?;
        Some(address & !low_mask & !group_mask)
    }

    fn interrupt_file_key(self, address: u64) -> Option<(u64, u64)> {
        if !address.is_multiple_of(IMSIC_PAGE_SIZE)
            || self.normalized_base(address)? != self.base_addr
        {
            return None;
        }
        let ppn = address >> 12;
        let guest_mask = Self::checked_mask(self.guest_index_bits)?;
        let hart_mask = Self::checked_mask(self.hart_index_bits)?;
        let group_mask = Self::checked_mask(self.group_index_bits)?;
        let guest = ppn & guest_mask;
        let low_hart = (ppn >> self.guest_index_bits) & hart_mask;
        let group = (address >> self.group_index_shift) & group_mask;
        Some((guest, low_hart | (group << self.hart_index_bits)))
    }

    pub(crate) fn encode_aplic_target(self, address: u64, eiid: u32) -> Option<u32> {
        if !address.is_multiple_of(IMSIC_PAGE_SIZE)
            || self.guest_index_bits > 7
            || self.hart_index_bits > 15
            || self.group_index_bits > 7
            || self.group_index_shift < 24
            || self.group_index_shift > 55
            || eiid == 0
            || eiid > APLIC_TARGET_EIID_MASK
            || self.normalized_base(address)? != self.base_addr
        {
            return None;
        }
        let (guest, hart) = self.interrupt_file_key(address)?;
        if guest > u64::from(APLIC_TARGET_GUEST_MASK) || hart > u64::from(APLIC_TARGET_HART_MASK) {
            return None;
        }
        Some(
            ((hart as u32) << APLIC_TARGET_HART_SHIFT)
                | ((guest as u32) << APLIC_TARGET_GUEST_SHIFT)
                | eiid,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImsicLayout {
    pub(crate) scheme: ImsicAddressScheme,
    pub(crate) interrupt_files: Vec<u64>,
}

fn default_hart_index_bits(contexts: usize) -> Option<u8> {
    if contexts <= 1 {
        return Some(0);
    }
    u8::try_from(usize::BITS - (contexts - 1).leading_zeros()).ok()
}

impl ImsicLayout {
    pub(crate) fn new(
        ranges: &[MmioRange],
        context_count: usize,
        guest_index_bits: Option<u32>,
        hart_index_bits: Option<u32>,
        group_index_bits: Option<u32>,
        group_index_shift: Option<u32>,
    ) -> Result<Self, AiaConfigError> {
        if ranges.is_empty() {
            return Err(AiaConfigError::MissingMmio);
        }
        if context_count == 0 {
            return Err(AiaConfigError::MissingSupervisorContext);
        }
        let guest_index_bits = u8::try_from(guest_index_bits.unwrap_or(0))
            .map_err(|_| AiaConfigError::InvalidAddressScheme)?;
        let hart_index_bits = match hart_index_bits {
            Some(bits) => u8::try_from(bits).map_err(|_| AiaConfigError::InvalidAddressScheme)?,
            None => default_hart_index_bits(context_count)
                .ok_or(AiaConfigError::InvalidAddressScheme)?,
        };
        let group_index_bits = u8::try_from(group_index_bits.unwrap_or(0))
            .map_err(|_| AiaConfigError::InvalidAddressScheme)?;
        let group_index_shift = u8::try_from(group_index_shift.unwrap_or(24))
            .map_err(|_| AiaConfigError::InvalidAddressScheme)?;
        if guest_index_bits > 7
            || hart_index_bits > 15
            || group_index_bits > 7
            || group_index_shift > 55
            || u16::from(group_index_shift) + u16::from(group_index_bits) > 64
            || u16::from(guest_index_bits)
                + u16::from(hart_index_bits)
                + u16::from(group_index_bits)
                + 12
                > 64
        {
            return Err(AiaConfigError::InvalidAddressScheme);
        }
        let per_hart_span = IMSIC_PAGE_SIZE
            .checked_shl(u32::from(guest_index_bits))
            .ok_or(AiaConfigError::AddressOverflow)?;
        let mut interrupt_files = Vec::new();
        interrupt_files
            .try_reserve(context_count)
            .map_err(|_| AiaConfigError::OutOfMemory)?;
        for range in ranges {
            if range.size == 0 || !range.phys.is_multiple_of(IMSIC_PAGE_SIZE) {
                return Err(AiaConfigError::UnalignedMmio);
            }
            range
                .phys
                .checked_add(range.size)
                .ok_or(AiaConfigError::AddressOverflow)?;
        }

        for file_index in 0..context_count {
            let mut relative = u64::try_from(file_index)
                .ok()
                .and_then(|index| index.checked_mul(per_hart_span))
                .ok_or(AiaConfigError::AddressOverflow)?;
            let mut address = None;
            for range in ranges {
                if relative < range.size {
                    let end = relative
                        .checked_add(IMSIC_PAGE_SIZE)
                        .ok_or(AiaConfigError::AddressOverflow)?;
                    if end > range.size {
                        return Err(AiaConfigError::MmioWindowTooSmall);
                    }
                    address = Some(
                        range
                            .phys
                            .checked_add(relative)
                            .ok_or(AiaConfigError::AddressOverflow)?,
                    );
                    break;
                }
                let aligned_size = range
                    .size
                    .checked_add(per_hart_span - 1)
                    .map(|size| size / per_hart_span * per_hart_span)
                    .ok_or(AiaConfigError::AddressOverflow)?;
                relative = relative
                    .checked_sub(aligned_size)
                    .ok_or(AiaConfigError::MmioWindowTooSmall)?;
            }
            let address = address.ok_or(AiaConfigError::MmioWindowTooSmall)?;
            if interrupt_files.contains(&address) {
                return Err(AiaConfigError::DuplicateInterruptFile);
            }
            interrupt_files.push(address);
        }
        let provisional = ImsicAddressScheme {
            guest_index_bits,
            hart_index_bits,
            group_index_bits,
            group_index_shift,
            base_addr: 0,
        };
        let base_addr = provisional
            .normalized_base(interrupt_files[0])
            .ok_or(AiaConfigError::InvalidAddressScheme)?;
        let scheme = ImsicAddressScheme {
            base_addr,
            ..provisional
        };
        if ranges
            .iter()
            .any(|range| scheme.normalized_base(range.phys) != Some(base_addr))
        {
            return Err(AiaConfigError::InvalidAddressScheme);
        }
        let mut file_keys = Vec::new();
        file_keys
            .try_reserve(context_count)
            .map_err(|_| AiaConfigError::OutOfMemory)?;
        for &address in &interrupt_files {
            let (guest, key) = scheme
                .interrupt_file_key(address)
                .ok_or(AiaConfigError::InvalidAddressScheme)?;
            if guest != 0 {
                return Err(AiaConfigError::InvalidAddressScheme);
            }
            if file_keys.contains(&key) {
                return Err(AiaConfigError::DuplicateInterruptFile);
            }
            file_keys.push(key);
        }
        Ok(Self {
            scheme,
            interrupt_files,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum AplicSourceMode {
    Inactive = 0,
    EdgeRise = 4,
    EdgeFall = 5,
    LevelHigh = 6,
    LevelLow = 7,
}

pub(crate) fn aplic_source_mode(flags: u32) -> Result<AplicSourceMode, AiaConfigError> {
    match flags {
        1 => Ok(AplicSourceMode::EdgeRise),
        2 => Ok(AplicSourceMode::EdgeFall),
        4 => Ok(AplicSourceMode::LevelHigh),
        8 => Ok(AplicSourceMode::LevelLow),
        _ => Err(AiaConfigError::UnsupportedIrqType),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AplicLayout {
    num_sources: u32,
}

/// APLIC direct-delivery 的 IDC 布局与本地可服务 hart 目标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AplicDirectLayout {
    max_hart_index: u32,
    service_hart_index: Option<u32>,
}

impl AplicDirectLayout {
    pub(crate) fn new(
        hart_indexes: &[u32],
        service_hart_index: Option<u32>,
        size: usize,
        virt: usize,
    ) -> Result<Self, AiaConfigError> {
        validate_aplic_hart_indexes(hart_indexes)?;
        if service_hart_index.is_some_and(|index| !hart_indexes.contains(&index)) {
            return Err(AiaConfigError::InvalidHartIndex);
        }
        let max_hart_index = hart_indexes
            .iter()
            .copied()
            .max()
            .ok_or(AiaConfigError::InvalidHartIndex)?;
        let required = APLIC_IDC_BASE
            .checked_add(
                (max_hart_index as usize + 1)
                    .checked_mul(APLIC_IDC_SIZE)
                    .ok_or(AiaConfigError::AddressOverflow)?,
            )
            .ok_or(AiaConfigError::AddressOverflow)?;
        if size < required {
            return Err(AiaConfigError::MmioWindowTooSmall);
        }
        virt.checked_add(required)
            .ok_or(AiaConfigError::AddressOverflow)?;
        Ok(Self {
            max_hart_index,
            service_hart_index,
        })
    }

    pub(crate) const fn service_hart_index(self) -> Option<u32> {
        self.service_hart_index
    }

    pub(crate) fn service_idc_offset(self, register: usize) -> Option<usize> {
        if !matches!(
            register,
            APLIC_IDC_IDELIVERY | APLIC_IDC_ITHRESHOLD | APLIC_IDC_CLAIMI
        ) {
            return None;
        }
        let hart_index = self.service_hart_index?;
        debug_assert!(hart_index <= self.max_hart_index);
        APLIC_IDC_BASE
            .checked_add((hart_index as usize).checked_mul(APLIC_IDC_SIZE)?)?
            .checked_add(register)
    }

    pub(crate) fn target(self) -> Option<u32> {
        self.service_hart_index.map(|hart_index| {
            (hart_index << APLIC_TARGET_HART_SHIFT) | (1 & APLIC_TARGET_IPRIO_MASK)
        })
    }

    pub(crate) const fn claimed_source(claimi: u32) -> Option<u32> {
        let source = (claimi >> APLIC_CLAIMI_ID_SHIFT) & APLIC_CLAIMI_ID_MASK;
        if source == 0 { None } else { Some(source) }
    }
}

impl AplicLayout {
    pub(crate) fn new(
        num_sources: u32,
        phys: usize,
        size: usize,
        virt: usize,
    ) -> Result<Self, AiaConfigError> {
        if num_sources == 0 || num_sources > APLIC_MAX_SOURCE {
            return Err(AiaConfigError::InvalidNumSources);
        }
        if !phys.is_multiple_of(REGISTER_WIDTH) || !virt.is_multiple_of(REGISTER_WIDTH) {
            return Err(AiaConfigError::UnalignedMmio);
        }
        phys.checked_add(size)
            .ok_or(AiaConfigError::AddressOverflow)?;
        let required = APLIC_TARGET_BASE
            .checked_add(num_sources as usize * REGISTER_WIDTH)
            .ok_or(AiaConfigError::AddressOverflow)?;
        if size < required {
            return Err(AiaConfigError::MmioWindowTooSmall);
        }
        virt.checked_add(required)
            .ok_or(AiaConfigError::AddressOverflow)?;
        Ok(Self { num_sources })
    }

    pub(crate) fn sourcecfg_offset(self, source: u32) -> Option<usize> {
        (source != 0 && source <= self.num_sources)
            .then(|| APLIC_SOURCECFG_BASE + (source as usize - 1) * REGISTER_WIDTH)
    }

    pub(crate) fn target_offset(self, source: u32) -> Option<usize> {
        (source != 0 && source <= self.num_sources)
            .then(|| APLIC_TARGET_BASE + (source as usize - 1) * REGISTER_WIDTH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be(value: u32) -> [u8; 4] {
        value.to_be_bytes()
    }

    #[test]
    fn qemu_two_hart_layout_encodes_aplic_targets() {
        let layout = ImsicLayout::new(
            &[MmioRange {
                phys: 0x2800_0000,
                size: 0x2000,
            }],
            2,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(layout.interrupt_files, [0x2800_0000, 0x2800_1000]);
        assert_eq!(layout.scheme.base_addr, 0x2800_0000);
        assert_eq!(layout.scheme.hart_index_bits, 1);
        assert_eq!(layout.scheme.encode_aplic_target(0x2800_0000, 17), Some(17));
        assert_eq!(
            layout.scheme.encode_aplic_target(0x2800_1000, 18),
            Some((1 << APLIC_TARGET_HART_SHIFT) | 18)
        );
    }

    #[test]
    fn qemu_direct_aplic_layout_covers_idcs_and_claims() {
        let layout = AplicDirectLayout::new(&[0, 1], Some(1), 0x8000, 0xd000_000).unwrap();
        assert_eq!(layout.service_hart_index(), Some(1));
        assert_eq!(layout.target(), Some((1 << APLIC_TARGET_HART_SHIFT) | 1));
        assert_eq!(
            layout.service_idc_offset(APLIC_IDC_IDELIVERY),
            Some(APLIC_IDC_BASE + APLIC_IDC_SIZE)
        );
        assert_eq!(
            layout.service_idc_offset(APLIC_IDC_CLAIMI),
            Some(APLIC_IDC_BASE + APLIC_IDC_SIZE + APLIC_IDC_CLAIMI)
        );
        assert_eq!(AplicDirectLayout::claimed_source(0x002a_0001), Some(42));
        assert_eq!(AplicDirectLayout::claimed_source(0), None);
    }

    #[test]
    fn direct_aplic_rejects_truncated_idc_window() {
        assert_eq!(
            AplicDirectLayout::new(
                &[0, 1],
                Some(0),
                APLIC_IDC_BASE + APLIC_IDC_SIZE,
                0xd000_000,
            ),
            Err(AiaConfigError::MmioWindowTooSmall)
        );
    }

    #[test]
    fn non_contiguous_aplic_hart_indexes_drive_target_and_idc_layout() {
        let raw = [0, 0, 0, 3, 0, 0, 0, 17];
        let indexes = parse_aplic_hart_indexes(Some(&raw), 2).unwrap();
        assert_eq!(indexes, [3, 17]);
        let size = APLIC_IDC_BASE + 18 * APLIC_IDC_SIZE;
        let layout = AplicDirectLayout::new(&indexes, Some(17), size, 0xd000_000).unwrap();
        assert_eq!(layout.target(), Some((17 << APLIC_TARGET_HART_SHIFT) | 1));
        assert_eq!(
            layout.service_idc_offset(APLIC_IDC_CLAIMI),
            Some(APLIC_IDC_BASE + 17 * APLIC_IDC_SIZE + APLIC_IDC_CLAIMI)
        );
        assert_eq!(
            AplicDirectLayout::new(&indexes, Some(17), size - 1, 0xd000_000),
            Err(AiaConfigError::MmioWindowTooSmall)
        );
    }

    #[test]
    fn direct_domain_without_boot_hart_uses_its_first_supervisor_context() {
        let supervisor = [9];
        let contexts = select_supervisor_contexts(
            [
                ImsicInterruptContext {
                    controller: Some(0x10),
                    cells: &supervisor,
                },
                ImsicInterruptContext {
                    controller: Some(0x20),
                    cells: &supervisor,
                },
            ],
            |controller| Some(u64::from(controller)),
            |controller| Some((controller >> 4) as usize),
        )
        .unwrap();
        let indexes = parse_aplic_hart_indexes(None, contexts.len()).unwrap();
        assert_eq!(
            aplic_service_hart_index(&contexts, &indexes, 0),
            Ok(Some(0))
        );
        let layout = AplicDirectLayout::new(&indexes, Some(0), 0x8000, 0xd000_000).unwrap();
        assert_eq!(layout.target(), Some(1));
        assert_eq!(layout.service_idc_offset(APLIC_IDC_CLAIMI), Some(0x401c));
    }

    #[test]
    fn guest_pages_are_skipped_between_supervisor_files() {
        let layout = ImsicLayout::new(
            &[MmioRange {
                phys: 0x2800_0000,
                size: 0x8000,
            }],
            2,
            Some(2),
            Some(1),
            None,
            None,
        )
        .unwrap();
        assert_eq!(layout.interrupt_files, [0x2800_0000, 0x2800_4000]);
        assert_eq!(
            layout.scheme.encode_aplic_target(0x2800_4000, 7),
            Some((1 << APLIC_TARGET_HART_SHIFT) | 7)
        );
    }

    #[test]
    fn supervisor_context_selection_uses_cpu_intc_identity() {
        let supervisor = [9];
        let contexts = [
            ImsicInterruptContext {
                controller: Some(0x10),
                cells: &supervisor,
            },
            ImsicInterruptContext {
                controller: Some(0x20),
                cells: &supervisor,
            },
        ];
        let selected = select_supervisor_contexts(
            contexts,
            |controller| match controller {
                0x10 => Some(7),
                0x20 => Some(42),
                _ => None,
            },
            |controller| match controller {
                0x10 => Some(0),
                0x20 => Some(1),
                _ => None,
            },
        )
        .unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].hart_id, 7);
        assert_eq!(selected[1].logical_cpu, 1);
        assert_eq!(selected[1].file_index, 1);
    }

    #[test]
    fn machine_only_imsic_is_not_a_supervisor_controller() {
        let machine = [11];
        assert_eq!(
            select_supervisor_contexts(
                [ImsicInterruptContext {
                    controller: Some(1),
                    cells: &machine,
                }],
                |_| Some(0),
                |_| Some(0),
            ),
            Err(AiaConfigError::UnsupportedInterruptContext)
        );
    }

    #[test]
    fn mixed_privilege_interrupt_files_are_rejected() {
        let machine = [11];
        let supervisor = [9];
        assert_eq!(
            select_supervisor_contexts(
                [
                    ImsicInterruptContext {
                        controller: Some(1),
                        cells: &supervisor,
                    },
                    ImsicInterruptContext {
                        controller: Some(2),
                        cells: &machine,
                    },
                ],
                |controller| Some(u64::from(controller - 1)),
                |controller| Some((controller - 1) as usize),
            ),
            Err(AiaConfigError::UnsupportedInterruptContext)
        );
    }

    #[test]
    fn standard_num_id_constraints_are_enforced() {
        assert_eq!(parse_num_ids(Some(&be(255))), Ok(255));
        assert_eq!(
            parse_num_ids(Some(&be(64))),
            Err(AiaConfigError::InvalidNumIds)
        );
        assert_eq!(
            parse_num_ids(Some(&[0, 1])),
            Err(AiaConfigError::MalformedNumIds)
        );
    }

    #[test]
    fn aplic_layout_and_irq_type_are_strict() {
        let layout = AplicLayout::new(96, 0x0d00_0000, 0x8000, 0xffff_0000).unwrap();
        assert_eq!(layout.sourcecfg_offset(1), Some(4));
        assert_eq!(layout.target_offset(96), Some(0x3180));
        assert_eq!(aplic_source_mode(4), Ok(AplicSourceMode::LevelHigh));
        assert_eq!(
            aplic_source_mode(3),
            Err(AiaConfigError::UnsupportedIrqType)
        );
        assert_eq!(
            AplicLayout::new(1024, 0x0d00_0000, 0x8000, 0xffff_0000),
            Err(AiaConfigError::InvalidNumSources)
        );
    }

    #[test]
    fn mmio_holes_and_full_binding_widths_are_supported() {
        let layout = ImsicLayout::new(
            &[
                MmioRange {
                    phys: 0x2800_0000,
                    size: 0x1800,
                },
                MmioRange {
                    phys: 0x2900_0000,
                    size: 0x1000,
                },
            ],
            2,
            Some(1),
            Some(15),
            Some(1),
            Some(24),
        )
        .unwrap();
        assert_eq!(layout.interrupt_files, [0x2800_0000, 0x2900_0000]);
        assert_eq!(layout.scheme.guest_index_bits, 1);
        assert_eq!(layout.scheme.hart_index_bits, 15);
        assert_eq!(layout.scheme.group_index_bits, 1);
        assert!(
            layout
                .scheme
                .encode_aplic_target(layout.interrupt_files[1], 1)
                .is_none()
        );

        let guest_width = ImsicLayout::new(
            &[MmioRange {
                phys: 0x3000_0000,
                size: 0x8_0000,
            }],
            1,
            Some(7),
            Some(0),
            None,
            None,
        )
        .unwrap();
        assert_eq!(guest_width.interrupt_files, [0x3000_0000]);
    }

    #[test]
    fn truncated_layout_and_duplicate_files_fail_closed() {
        assert_eq!(
            ImsicLayout::new(
                &[MmioRange {
                    phys: 0x2800_0000,
                    size: 0x1000,
                }],
                2,
                None,
                None,
                None,
                None,
            ),
            Err(AiaConfigError::MmioWindowTooSmall)
        );
        assert_eq!(
            ImsicLayout::new(
                &[
                    MmioRange {
                        phys: 0x2800_0000,
                        size: 0x1000,
                    },
                    MmioRange {
                        phys: 0x2800_0000,
                        size: 0x1000,
                    },
                ],
                2,
                None,
                None,
                None,
                None,
            ),
            Err(AiaConfigError::DuplicateInterruptFile)
        );
    }
}

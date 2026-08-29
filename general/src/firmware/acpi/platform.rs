use alloc::boxed::Box;
use alloc::vec::Vec;

use super::types::{
    AcpiAddressSpace, AcpiFadtInfo, AcpiGenericAddress, AcpiHpetInfo, AcpiPciConfigRegion,
    AcpiPmTimerInfo, AcpiPpttCache, AcpiPpttInfo, AcpiPpttProcessor, AcpiSpcrInfo, AcpiTableError,
    checked_sdt, parse_gas, read_u16, read_u32, read_u64,
};

const FADT_V1_SIZE: usize = 116;
const MCFG_HEADER_SIZE: usize = 44;
const HPET_TABLE_SIZE: usize = 56;
const SPCR_V1_SIZE: usize = 80;
const PPTT_HEADER_SIZE: usize = 36;
const FADT_RESET_REG_SUP: u32 = 1 << 10;
const FADT_HW_REDUCED_ACPI: u32 = 1 << 20;

pub fn parse_fadt(bytes: &[u8]) -> Result<AcpiFadtInfo, AcpiTableError> {
    let table = checked_sdt(bytes, b"FACP", FADT_V1_SIZE)?;
    // The header revision is the FADT major version.  Extended fields were not
    // part of the ACPI 1.0 layout; do not interpret trailing bytes in a legacy
    // table as RESET_REG/X_* fields merely because a malformed producer made
    // the declared length large enough.
    let revision = table[8];
    let has_extended_fields = revision >= 2;
    let has_hw_reduced_fields = revision >= 5;
    let flags = read_u32(table, 112).ok_or(AcpiTableError::InvalidLength)?;
    let legacy_pm_timer_address = read_u32(table, 76).unwrap_or(0);
    let legacy_pm_timer_length = table.get(91).copied().unwrap_or(0);
    let extended_pm_timer = has_extended_fields
        .then(|| parse_gas(table, 208).filter(valid_pm_timer_register))
        .flatten();
    let legacy_pm_timer = (legacy_pm_timer_address != 0 && legacy_pm_timer_length == 4).then_some(
        AcpiGenericAddress {
            address_space: AcpiAddressSpace::SystemIo,
            bit_width: 32,
            bit_offset: 0,
            access_size: 0,
            address: u64::from(legacy_pm_timer_address),
        },
    );
    let hardware_reduced = has_hw_reduced_fields && flags & FADT_HW_REDUCED_ACPI != 0;
    let pm_timer = (!hardware_reduced)
        .then(|| extended_pm_timer.or(legacy_pm_timer))
        .flatten()
        .map(|register| AcpiPmTimerInfo {
            register,
            // TMR_VAL_EXT is defined in the original ACPI 1.0 flags word.
            supports_32_bit: flags & (1 << 8) != 0,
        });

    let reset_register = (has_extended_fields && flags & FADT_RESET_REG_SUP != 0)
        .then(|| parse_gas(table, 116).filter(valid_reset_register))
        .flatten();
    let legacy_pm1_width = table[89];
    let pm1a_control = (!hardware_reduced)
        .then(|| {
            select_fadt_register(
                has_extended_fields.then(|| parse_gas(table, 172)).flatten(),
                legacy_system_io_register(read_u32(table, 64), legacy_pm1_width),
                valid_pm1_control_register,
            )
        })
        .flatten();
    let pm1b_control = (!hardware_reduced)
        .then(|| {
            select_fadt_register(
                has_extended_fields.then(|| parse_gas(table, 184)).flatten(),
                legacy_system_io_register(read_u32(table, 68), legacy_pm1_width),
                valid_pm1_control_register,
            )
        })
        .flatten();
    let sleep_control = (hardware_reduced && has_hw_reduced_fields)
        .then(|| parse_gas(table, 244).filter(valid_sleep_register))
        .flatten();
    let sleep_status = (hardware_reduced && has_hw_reduced_fields)
        .then(|| parse_gas(table, 256).filter(valid_sleep_register))
        .flatten();

    Ok(AcpiFadtInfo {
        preferred_profile: table[45],
        sci_interrupt: read_u16(table, 46).ok_or(AcpiTableError::InvalidLength)?,
        smi_command_port: read_u32(table, 48).ok_or(AcpiTableError::InvalidLength)?,
        acpi_enable: table[52],
        acpi_disable: table[53],
        boot_architecture_flags: if has_extended_fields {
            read_u16(table, 109).ok_or(AcpiTableError::InvalidLength)?
        } else {
            0
        },
        flags,
        pm_timer,
        pm1a_control,
        pm1b_control,
        sleep_control,
        sleep_status,
        reset_register,
        reset_value: has_extended_fields
            .then(|| table.get(128).copied().unwrap_or(0))
            .unwrap_or(0),
    })
}

fn select_fadt_register(
    extended: Option<AcpiGenericAddress>,
    legacy: Option<AcpiGenericAddress>,
    validator: fn(&AcpiGenericAddress) -> bool,
) -> Option<AcpiGenericAddress> {
    extended.filter(validator).or(legacy.filter(validator))
}

fn legacy_system_io_register(address: Option<u32>, length: u8) -> Option<AcpiGenericAddress> {
    let address = address.filter(|address| *address != 0)?;
    matches!(length, 1 | 2 | 4).then_some(AcpiGenericAddress {
        address_space: AcpiAddressSpace::SystemIo,
        bit_width: length * 8,
        bit_offset: 0,
        access_size: match length {
            1 => 1,
            2 => 2,
            4 => 3,
            _ => 0,
        },
        address: u64::from(address),
    })
}

fn valid_pm1_control_register(register: &AcpiGenericAddress) -> bool {
    register.address != 0
        && register.bit_offset == 0
        && matches!(register.bit_width, 16 | 32)
        && matches!(register.access_size, 0 | 2 | 3)
        && matches!(
            register.address_space,
            AcpiAddressSpace::SystemMemory | AcpiAddressSpace::SystemIo
        )
}

fn valid_sleep_register(register: &AcpiGenericAddress) -> bool {
    register.address != 0
        && register.bit_width == 8
        && register.bit_offset == 0
        && matches!(register.access_size, 0 | 1)
        && matches!(
            register.address_space,
            AcpiAddressSpace::SystemMemory | AcpiAddressSpace::SystemIo
        )
}

pub fn parse_mcfg(bytes: &[u8]) -> Result<Vec<AcpiPciConfigRegion>, AcpiTableError> {
    let table = checked_sdt(bytes, b"MCFG", MCFG_HEADER_SIZE)?;
    if !table[36..44].iter().all(|byte| *byte == 0)
        || !(table.len() - MCFG_HEADER_SIZE).is_multiple_of(16)
    {
        return Err(AcpiTableError::InvalidLength);
    }

    let mut regions = Vec::new();
    for entry in table[MCFG_HEADER_SIZE..].chunks_exact(16) {
        if !entry[12..16].iter().all(|byte| *byte == 0) {
            return Err(AcpiTableError::InvalidFlags);
        }
        let base = read_u64(entry, 0).ok_or(AcpiTableError::TruncatedEntry)?;
        let segment = read_u16(entry, 8).ok_or(AcpiTableError::TruncatedEntry)?;
        let bus_start = entry[10];
        let bus_end = entry[11];
        if base == 0 || base & ((1 << 20) - 1) != 0 || bus_start > bus_end {
            return Err(AcpiTableError::InvalidAddress);
        }
        let bus_count = usize::from(bus_end - bus_start) + 1;
        let size = bus_count
            .checked_mul(1 << 20)
            .ok_or(AcpiTableError::InvalidAddress)?;
        let segment_base_address =
            usize::try_from(base).map_err(|_| AcpiTableError::InvalidAddress)?;
        let physical_address = segment_base_address
            .checked_add(usize::from(bus_start) << 20)
            .ok_or(AcpiTableError::InvalidAddress)?;
        physical_address
            .checked_add(size)
            .ok_or(AcpiTableError::InvalidAddress)?;
        if regions.iter().any(|region: &AcpiPciConfigRegion| {
            region.segment == segment && bus_start <= region.bus_end && region.bus_start <= bus_end
        }) {
            return Err(AcpiTableError::OverlappingRange);
        }
        regions.push(AcpiPciConfigRegion {
            segment,
            bus_start,
            bus_end,
            segment_base_address,
            physical_address,
            size,
        });
    }
    Ok(regions)
}

pub fn parse_hpet(bytes: &[u8]) -> Result<AcpiHpetInfo, AcpiTableError> {
    let table = checked_sdt(bytes, b"HPET", HPET_TABLE_SIZE)?;
    if table[8] != 1 {
        return Err(AcpiTableError::InvalidFlags);
    }
    let base = parse_gas(table, 40).ok_or(AcpiTableError::InvalidLength)?;
    if base.address == 0 || base.address_space != AcpiAddressSpace::SystemMemory {
        return Err(if base.address == 0 {
            AcpiTableError::InvalidAddress
        } else {
            AcpiTableError::UnsupportedAddressSpace
        });
    }
    if base.bit_width != 0
        || base.bit_offset != 0
        || base.access_size != 0
        || base.address & 0x7 != 0
        || base.address.checked_add(0x400).is_none()
    {
        return Err(AcpiTableError::InvalidFlags);
    }
    let event_timer_block_id = read_u32(table, 36).ok_or(AcpiTableError::InvalidLength)?;
    if event_timer_block_id & (1 << 14) != 0 || event_timer_block_id & 0xff == 0 {
        return Err(AcpiTableError::InvalidFlags);
    }
    let page_protection = table[55] & 0x0f;
    if page_protection > 2 {
        return Err(AcpiTableError::InvalidFlags);
    }
    Ok(AcpiHpetInfo {
        event_timer_block_id,
        base,
        sequence: table[52],
        minimum_tick: read_u16(table, 53).ok_or(AcpiTableError::InvalidLength)?,
        page_protection,
        oem_attributes: table[55] >> 4,
    })
}

pub fn parse_spcr(bytes: &[u8]) -> Result<AcpiSpcrInfo, AcpiTableError> {
    let table = checked_sdt(bytes, b"SPCR", SPCR_V1_SIZE)?;
    let revision = table[8];
    if revision == 0 {
        return Err(AcpiTableError::InvalidFlags);
    }
    if table[37..40].iter().any(|byte| *byte != 0) || table[52] & !0x1f != 0 || table[63] != 0 {
        return Err(AcpiTableError::InvalidFlags);
    }
    // SPCR rev. 1 only defines the full 16550/16450 interface.  Newer
    // revisions add DBG2 subtypes; accepting those values in a rev. 1 table
    // would make the kernel instantiate a device with an undefined layout.
    if revision == 1 && table[36] > 1 {
        return Err(AcpiTableError::InvalidFlags);
    }
    // RISC-V PLIC/APLIC (bit 4) was introduced by SPCR rev. 4.  Keep older
    // tables strict while allowing future revisions to retain the known bit.
    if revision < 4 && table[52] & (1 << 4) != 0 {
        return Err(AcpiTableError::InvalidFlags);
    }
    let base = parse_gas(table, 40).ok_or(AcpiTableError::InvalidLength)?;
    if base.address == 0 {
        return Err(AcpiTableError::InvalidAddress);
    }
    let interrupt_type = table[52];
    let legacy_irq = (interrupt_type & 1 != 0).then_some(table[53]);
    if legacy_irq.is_some_and(|irq| !valid_spcr_isa_irq(irq)) {
        return Err(AcpiTableError::InvalidFlags);
    }
    let routed_interrupt_types = interrupt_type & !1;
    let global_system_interrupt = if routed_interrupt_types == 0 {
        None
    } else {
        let interrupt = read_u32(table, 54).ok_or(AcpiTableError::InvalidLength)?;
        // I/O APIC and I/O SAPIC use GSIs, for which zero is valid. Other SPCR
        // interrupt-controller encodings use architecture interrupt IDs whose zero value is
        // reserved and cannot identify a UART interrupt.
        if interrupt == 0 && routed_interrupt_types & 0x06 == 0 {
            return Err(AcpiTableError::InvalidAddress);
        }
        Some(interrupt)
    };
    let configured_baud = match table[58] {
        3 => Some(9_600),
        4 => Some(19_200),
        6 => Some(57_600),
        7 => Some(115_200),
        _ => None,
    };
    // UART clock frequency was added in revision 3; precise baud and the
    // namespace suffix were added together in revision 4.  Length checks are
    // still required because old firmware often emits a minimal 80-byte SPCR.
    let precise_baud = (revision >= 4)
        .then(|| read_u32(table, 80))
        .flatten()
        .filter(|baud| *baud != 0);
    let namespace = if revision < 4 {
        None
    } else {
        match (read_u16(table, 84), read_u16(table, 86)) {
            (None, None) => None,
            (Some(0), Some(0)) => None,
            (Some(length), Some(offset)) if length != 0 => {
                let start = usize::from(offset);
                let end = start
                    .checked_add(usize::from(length))
                    .ok_or(AcpiTableError::InvalidReference)?;
                if start < 88 || end > table.len() {
                    return Err(AcpiTableError::InvalidReference);
                }
                let value = core::str::from_utf8(&table[start..end])
                    .map_err(|_| AcpiTableError::InvalidFlags)?
                    .trim_end_matches('\0');
                (!value.is_empty() && value != ".").then(|| Box::<str>::from(value))
            }
            _ => return Err(AcpiTableError::InvalidReference),
        }
    };
    Ok(AcpiSpcrInfo {
        interface_type: table[36],
        base,
        interrupt_type,
        legacy_irq,
        global_system_interrupt,
        baud: precise_baud.or(configured_baud),
        clock_hz: (revision >= 3)
            .then(|| read_u32(table, 76))
            .flatten()
            .filter(|clock| *clock != 0),
        namespace,
    })
}

fn valid_spcr_isa_irq(irq: u8) -> bool {
    matches!(irq, 2..=7 | 9..=12 | 14..=15)
}

pub fn parse_pptt(bytes: &[u8]) -> Result<AcpiPpttInfo, AcpiTableError> {
    let table = checked_sdt(bytes, b"PPTT", PPTT_HEADER_SIZE)?;
    if table[8] == 0 {
        return Err(AcpiTableError::InvalidFlags);
    }
    let mut info = AcpiPpttInfo::default();
    let mut structure_offsets = Vec::new();
    let mut offset = PPTT_HEADER_SIZE;

    while offset < table.len() {
        let header = table
            .get(offset..offset.checked_add(2).ok_or(AcpiTableError::InvalidLength)?)
            .ok_or(AcpiTableError::TruncatedEntry)?;
        let structure_type = header[0];
        let length = usize::from(header[1]);
        if length < 4 {
            return Err(AcpiTableError::InvalidLength);
        }
        let end = offset
            .checked_add(length)
            .ok_or(AcpiTableError::InvalidLength)?;
        let structure = table
            .get(offset..end)
            .ok_or(AcpiTableError::TruncatedEntry)?;
        let table_offset = u32::try_from(offset).map_err(|_| AcpiTableError::InvalidLength)?;
        structure_offsets.push(table_offset);
        match structure_type {
            0 => parse_pptt_processor(structure, table_offset, table[8], &mut info)?,
            1 => parse_pptt_cache(structure, table_offset, table[8], &mut info)?,
            other => {
                if !info.unknown_structure_types.contains(&other) {
                    info.unknown_structure_types.push(other);
                }
            }
        }
        offset = end;
    }

    validate_pptt_references(&mut info, &structure_offsets, table[8])?;
    Ok(info)
}

fn parse_pptt_processor(
    structure: &[u8],
    table_offset: u32,
    table_revision: u8,
    info: &mut AcpiPpttInfo,
) -> Result<(), AcpiTableError> {
    if structure.len() < 20 {
        return Err(AcpiTableError::TruncatedEntry);
    }
    if structure[2..4].iter().any(|byte| *byte != 0) {
        return Err(AcpiTableError::InvalidFlags);
    }
    let flags = read_u32(structure, 4).ok_or(AcpiTableError::TruncatedEntry)?;
    let allowed_flags = if table_revision == 1 { 0x03 } else { 0x1f };
    if table_revision == 0 || flags & !allowed_flags != 0 {
        return Err(AcpiTableError::InvalidFlags);
    }
    let parent = read_u32(structure, 8).ok_or(AcpiTableError::TruncatedEntry)?;
    let resource_count =
        usize::try_from(read_u32(structure, 16).ok_or(AcpiTableError::TruncatedEntry)?)
            .map_err(|_| AcpiTableError::InvalidLength)?;
    let resources_end = 20usize
        .checked_add(
            resource_count
                .checked_mul(4)
                .ok_or(AcpiTableError::InvalidLength)?,
        )
        .ok_or(AcpiTableError::InvalidLength)?;
    if resources_end != structure.len() {
        return Err(AcpiTableError::InvalidLength);
    }
    let mut private_resource_offsets = Vec::with_capacity(resource_count);
    for resource in structure[20..].chunks_exact(4) {
        private_resource_offsets.push(read_u32(resource, 0).ok_or(AcpiTableError::TruncatedEntry)?);
    }
    let processor_uid = (flags & (1 << 1) != 0)
        .then(|| read_u32(structure, 12))
        .flatten();
    if flags & (1 << 1) != 0 && processor_uid.is_none() {
        return Err(AcpiTableError::TruncatedEntry);
    }
    info.processors.push(AcpiPpttProcessor {
        table_offset,
        parent_offset: (parent != 0).then_some(parent),
        processor_uid,
        physical_package: flags & 1 != 0,
        is_thread: flags & (1 << 2) != 0,
        is_leaf: flags & (1 << 3) != 0,
        identical_implementation: flags & (1 << 4) != 0,
        private_resource_offsets,
    });
    Ok(())
}

fn parse_pptt_cache(
    structure: &[u8],
    table_offset: u32,
    table_revision: u8,
    info: &mut AcpiPpttInfo,
) -> Result<(), AcpiTableError> {
    if structure.len() < 24 {
        return Err(AcpiTableError::TruncatedEntry);
    }
    if structure[2..4].iter().any(|byte| *byte != 0) {
        return Err(AcpiTableError::InvalidFlags);
    }
    let flags = read_u32(structure, 4).ok_or(AcpiTableError::TruncatedEntry)?;
    if flags & !0xff != 0 {
        return Err(AcpiTableError::InvalidFlags);
    }
    let next_level = read_u32(structure, 8).ok_or(AcpiTableError::TruncatedEntry)?;
    let cache_id_valid = flags & (1 << 7) != 0;
    if cache_id_valid && table_revision < 3 {
        return Err(AcpiTableError::InvalidFlags);
    }
    if cache_id_valid && structure.len() < 28 {
        return Err(AcpiTableError::TruncatedEntry);
    }
    if table_revision >= 3 && structure.len() != 28 || table_revision < 3 && structure.len() != 24 {
        return Err(AcpiTableError::InvalidLength);
    }
    let attributes = structure[21];
    if attributes & 0xe0 != 0 {
        return Err(AcpiTableError::InvalidFlags);
    }
    let cache = AcpiPpttCache {
        table_offset,
        next_level_offset: (next_level != 0).then_some(next_level),
        size: (flags & 1 != 0).then(|| read_u32(structure, 12)).flatten(),
        sets: (flags & (1 << 1) != 0)
            .then(|| read_u32(structure, 16))
            .flatten(),
        associativity: (flags & (1 << 2) != 0).then_some(structure[20]),
        allocation_type: (flags & (1 << 3) != 0).then_some(attributes & 0x03),
        cache_type: (flags & (1 << 4) != 0).then_some((attributes >> 2) & 0x03),
        write_through: (flags & (1 << 5) != 0).then_some(attributes & (1 << 4) != 0),
        line_size: (flags & (1 << 6) != 0)
            .then(|| read_u16(structure, 22))
            .flatten(),
        cache_id: cache_id_valid.then(|| read_u32(structure, 24)).flatten(),
    };
    if cache.cache_id == Some(0)
        || cache.cache_id.is_some_and(|id| {
            info.caches
                .iter()
                .any(|existing| existing.cache_id == Some(id))
        })
    {
        return Err(
            if cache.cache_id.is_some_and(|id| {
                info.caches
                    .iter()
                    .any(|existing| existing.cache_id == Some(id))
            }) {
                AcpiTableError::DuplicateEntry
            } else {
                AcpiTableError::InvalidFlags
            },
        );
    }
    info.caches.push(cache);
    Ok(())
}

fn validate_pptt_references(
    info: &mut AcpiPpttInfo,
    structures: &[u32],
    table_revision: u8,
) -> Result<(), AcpiTableError> {
    if table_revision == 1 {
        let leaf_offsets: Vec<u32> = info
            .processors
            .iter()
            .filter(|candidate| {
                !info
                    .processors
                    .iter()
                    .any(|processor| processor.parent_offset == Some(candidate.table_offset))
            })
            .map(|processor| processor.table_offset)
            .collect();
        for processor in &mut info.processors {
            processor.is_leaf = leaf_offsets.contains(&processor.table_offset);
        }
    }
    for processor in &info.processors {
        if processor
            .parent_offset
            .is_some_and(|parent| info.processor_at(parent).is_none())
            || processor
                .private_resource_offsets
                .iter()
                .any(|offset| !structures.contains(offset) || info.processor_at(*offset).is_some())
        {
            return Err(AcpiTableError::InvalidReference);
        }
        let mut parent = processor.parent_offset;
        for _ in 0..=info.processors.len() {
            let Some(offset) = parent else {
                break;
            };
            let node = info
                .processor_at(offset)
                .ok_or(AcpiTableError::InvalidReference)?;
            if node.table_offset == processor.table_offset {
                return Err(AcpiTableError::InvalidReference);
            }
            parent = node.parent_offset;
        }
        if parent.is_some() {
            return Err(AcpiTableError::InvalidReference);
        }
    }
    for processor in info.processors.iter().filter(|processor| processor.is_leaf) {
        let Some(uid) = processor.processor_uid else {
            return Err(AcpiTableError::InvalidReference);
        };
        if info
            .processors
            .iter()
            .filter(|candidate| candidate.is_leaf && candidate.processor_uid == Some(uid))
            .count()
            != 1
        {
            return Err(AcpiTableError::DuplicateEntry);
        }
        if processor.is_thread && processor.parent_offset.is_none() {
            return Err(AcpiTableError::InvalidReference);
        }
        if processor.is_thread {
            let sibling_threads = info
                .processors
                .iter()
                .filter(|candidate| {
                    candidate.parent_offset == processor.parent_offset
                        && candidate.is_leaf
                        && candidate.is_thread
                })
                .count();
            let non_thread_sibling = info.processors.iter().any(|candidate| {
                candidate.parent_offset == processor.parent_offset
                    && candidate.is_leaf
                    && !candidate.is_thread
            });
            if sibling_threads < 2 || non_thread_sibling {
                return Err(AcpiTableError::InvalidReference);
            }
        }
        let mut package_count = usize::from(processor.physical_package);
        let mut parent = processor.parent_offset;
        while let Some(offset) = parent {
            let node = info
                .processor_at(offset)
                .ok_or(AcpiTableError::InvalidReference)?;
            package_count += usize::from(node.physical_package);
            parent = node.parent_offset;
        }
        if package_count != 1 {
            return Err(AcpiTableError::InvalidReference);
        }
    }
    for processor in &info.processors {
        let has_children = info
            .processors
            .iter()
            .any(|candidate| candidate.parent_offset == Some(processor.table_offset));
        if table_revision >= 2 && processor.is_leaf == has_children
            || processor.is_thread && !processor.is_leaf
        {
            return Err(AcpiTableError::InvalidReference);
        }
    }
    for cache in &info.caches {
        if cache
            .next_level_offset
            .is_some_and(|next| !info.caches.iter().any(|entry| entry.table_offset == next))
        {
            return Err(AcpiTableError::InvalidReference);
        }
        let mut next = cache.next_level_offset;
        for _ in 0..=info.caches.len() {
            let Some(offset) = next else {
                break;
            };
            let node = info
                .caches
                .iter()
                .find(|entry| entry.table_offset == offset)
                .ok_or(AcpiTableError::InvalidReference)?;
            if node.table_offset == cache.table_offset {
                return Err(AcpiTableError::InvalidReference);
            }
            next = node.next_level_offset;
        }
        if next.is_some() {
            return Err(AcpiTableError::InvalidReference);
        }
    }
    Ok(())
}

fn valid_pm_timer_register(register: &AcpiGenericAddress) -> bool {
    register.address != 0
        && register.bit_width == 32
        && register.bit_offset == 0
        && matches!(register.access_size, 0 | 3)
        && matches!(
            register.address_space,
            AcpiAddressSpace::SystemMemory | AcpiAddressSpace::SystemIo
        )
}

fn valid_reset_register(register: &AcpiGenericAddress) -> bool {
    register.address != 0
        && register.bit_width == 8
        && register.bit_offset == 0
        && matches!(register.access_size, 0 | 1)
        && matches!(
            register.address_space,
            AcpiAddressSpace::SystemMemory
                | AcpiAddressSpace::SystemIo
                | AcpiAddressSpace::PciConfig
        )
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn finish(table: &mut [u8], signature: &[u8; 4]) {
        let length = table.len() as u32;
        table[..4].copy_from_slice(signature);
        table[4..8].copy_from_slice(&length.to_le_bytes());
        table[9] = 0;
        table[9] = 0u8.wrapping_sub(table.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
    }

    #[test]
    fn parses_nonzero_mcfg_bus_start_without_rebasing_error() {
        let mut table = vec![0u8; MCFG_HEADER_SIZE];
        let mut entry = [0u8; 16];
        entry[0..8].copy_from_slice(&0xe000_0000u64.to_le_bytes());
        entry[8..10].copy_from_slice(&2u16.to_le_bytes());
        entry[10] = 0x20;
        entry[11] = 0x2f;
        table.extend_from_slice(&entry);
        finish(&mut table, b"MCFG");
        let region = parse_mcfg(&table).unwrap()[0];
        assert_eq!(region.bus_start, 0x20);
        assert_eq!(region.segment_base_address, 0xe000_0000);
        assert_eq!(region.physical_address, 0xe200_0000);
        assert_eq!(region.size, 16 << 20);
        assert_eq!(region.address(0x21, 3, 2, 0x120), Some(0xe211_a120));
    }

    #[test]
    fn parses_pptt_parent_and_cache_references() {
        let mut table = vec![0u8; PPTT_HEADER_SIZE];
        table[8] = 2;
        let package_offset = table.len() as u32;
        let mut package = [0u8; 20];
        package[0] = 0;
        package[1] = 20;
        package[4..8].copy_from_slice(&1u32.to_le_bytes());
        table.extend_from_slice(&package);
        let cache_offset = table.len() as u32;
        let mut cache = [0u8; 24];
        cache[0] = 1;
        cache[1] = 24;
        cache[4..8].copy_from_slice(&0x47u32.to_le_bytes());
        cache[12..16].copy_from_slice(&0x8000u32.to_le_bytes());
        cache[16..20].copy_from_slice(&64u32.to_le_bytes());
        cache[20] = 8;
        cache[22..24].copy_from_slice(&64u16.to_le_bytes());
        table.extend_from_slice(&cache);
        let mut cpu = [0u8; 24];
        cpu[0] = 0;
        cpu[1] = 24;
        cpu[4..8].copy_from_slice(&0x0au32.to_le_bytes());
        cpu[8..12].copy_from_slice(&package_offset.to_le_bytes());
        cpu[12..16].copy_from_slice(&7u32.to_le_bytes());
        cpu[16..20].copy_from_slice(&1u32.to_le_bytes());
        cpu[20..24].copy_from_slice(&cache_offset.to_le_bytes());
        table.extend_from_slice(&cpu);
        finish(&mut table, b"PPTT");

        let pptt = parse_pptt(&table).unwrap();
        let processor = pptt.processor_for_uid(7).unwrap();
        assert_eq!(processor.parent_offset, Some(package_offset));
        assert_eq!(processor.private_resource_offsets, vec![cache_offset]);
    }

    #[test]
    fn fadt_falls_back_when_extended_pm_timer_is_unusable() {
        let mut table = vec![0u8; 220];
        table[8] = 2;
        table[76..80].copy_from_slice(&0x408u32.to_le_bytes());
        table[91] = 4;
        table[208] = 0x7f;
        table[209] = 32;
        table[211] = 3;
        table[212..220].copy_from_slice(&0x1234u64.to_le_bytes());
        finish(&mut table, b"FACP");

        let timer = parse_fadt(&table).unwrap().pm_timer.unwrap();
        assert_eq!(timer.register.address_space, AcpiAddressSpace::SystemIo);
        assert_eq!(timer.register.address, 0x408);
    }

    #[test]
    fn fadt_only_exposes_advertised_byte_reset_register() {
        let mut table = vec![0u8; 220];
        table[8] = 2;
        table[116] = 1;
        table[117] = 8;
        table[119] = 1;
        table[120..128].copy_from_slice(&0xcf9u64.to_le_bytes());
        finish(&mut table, b"FACP");
        assert!(parse_fadt(&table).unwrap().reset_register.is_none());

        table[112..116].copy_from_slice(&FADT_RESET_REG_SUP.to_le_bytes());
        finish(&mut table, b"FACP");
        assert_eq!(
            parse_fadt(&table).unwrap().reset_register.unwrap().address,
            0xcf9
        );

        table[117] = 16;
        finish(&mut table, b"FACP");
        assert!(parse_fadt(&table).unwrap().reset_register.is_none());
    }

    #[test]
    fn hardware_reduced_fadt_ignores_pm_timer() {
        let mut table = vec![0u8; 220];
        table[8] = 5;
        table[76..80].copy_from_slice(&0x408u32.to_le_bytes());
        table[91] = 4;
        table[112..116].copy_from_slice(&FADT_HW_REDUCED_ACPI.to_le_bytes());
        finish(&mut table, b"FACP");
        assert!(parse_fadt(&table).unwrap().pm_timer.is_none());
    }

    #[test]
    fn fadt_selects_valid_pm1_control_and_hw_reduced_sleep_registers() {
        let mut table = vec![0u8; 268];
        table[8] = 5;
        table[64..68].copy_from_slice(&0x404u32.to_le_bytes());
        table[89] = 2;
        // A non-zero but unusable extended GAS must not hide the legacy block.
        table[172] = 0x7f;
        table[173] = 16;
        table[175] = 2;
        table[176..184].copy_from_slice(&0x1234u64.to_le_bytes());
        finish(&mut table, b"FACP");
        let fadt = parse_fadt(&table).unwrap();
        assert_eq!(fadt.pm1a_control.unwrap().address, 0x404);
        assert!(fadt.sleep_control.is_none());

        table[112..116].copy_from_slice(&FADT_HW_REDUCED_ACPI.to_le_bytes());
        table[244] = 0;
        table[245] = 8;
        table[247] = 1;
        table[248..256].copy_from_slice(&0x1000u64.to_le_bytes());
        table[256] = 0;
        table[257] = 8;
        table[259] = 1;
        table[260..268].copy_from_slice(&0x1001u64.to_le_bytes());
        finish(&mut table, b"FACP");
        let fadt = parse_fadt(&table).unwrap();
        assert!(fadt.pm1a_control.is_none());
        assert_eq!(fadt.sleep_control.unwrap().address, 0x1000);
        assert_eq!(fadt.sleep_status.unwrap().address, 0x1001);
    }

    #[test]
    fn hpet_preserves_oem_attributes_and_rejects_register_width() {
        let mut table = vec![0u8; HPET_TABLE_SIZE];
        table[8] = 1;
        table[36] = 1;
        table[44..52].copy_from_slice(&0xfed0_0000u64.to_le_bytes());
        table[55] = 0xa1;
        finish(&mut table, b"HPET");
        let hpet = parse_hpet(&table).unwrap();
        assert_eq!(hpet.page_protection, 1);
        assert_eq!(hpet.oem_attributes, 0x0a);

        table[41] = 64;
        finish(&mut table, b"HPET");
        assert_eq!(parse_hpet(&table), Err(AcpiTableError::InvalidFlags));
    }

    #[test]
    fn hpet_rejects_reserved_id_revision_and_unaligned_block() {
        let mut table = vec![0u8; HPET_TABLE_SIZE];
        table[8] = 1;
        table[36] = 1;
        table[44..52].copy_from_slice(&0xfed0_0000u64.to_le_bytes());
        finish(&mut table, b"HPET");
        assert!(parse_hpet(&table).is_ok());

        table[37] |= 1 << 6;
        finish(&mut table, b"HPET");
        assert_eq!(parse_hpet(&table), Err(AcpiTableError::InvalidFlags));

        table[37] &= !(1 << 6);
        table[36] = 0;
        finish(&mut table, b"HPET");
        assert_eq!(parse_hpet(&table), Err(AcpiTableError::InvalidFlags));

        table[36] = 1;
        table[44..52].copy_from_slice(&0xfed0_0101u64.to_le_bytes());
        finish(&mut table, b"HPET");
        assert_eq!(parse_hpet(&table), Err(AcpiTableError::InvalidFlags));
    }

    #[test]
    fn spcr_parses_legacy_length_system_io_and_baud() {
        let mut table = vec![0u8; SPCR_V1_SIZE];
        table[8] = 3;
        table[36] = 0;
        table[40] = 1;
        table[41] = 8;
        table[43] = 1;
        table[44..52].copy_from_slice(&0x3f8u64.to_le_bytes());
        table[52] = 1;
        table[53] = 4;
        table[58] = 7;
        table[76..80].copy_from_slice(&1_843_200u32.to_le_bytes());
        finish(&mut table, b"SPCR");

        let spcr = parse_spcr(&table).unwrap();
        assert_eq!(spcr.base.address_space, AcpiAddressSpace::SystemIo);
        assert_eq!(spcr.base.address, 0x3f8);
        assert_eq!(spcr.legacy_irq, Some(4));
        assert_eq!(spcr.baud, Some(115_200));
        assert_eq!(spcr.clock_hz, Some(1_843_200));
    }

    #[test]
    fn spcr_preserves_io_apic_gsi_zero() {
        let mut table = vec![0u8; SPCR_V1_SIZE];
        table[8] = 2;
        table[36] = 0;
        table[40] = 1;
        table[41] = 8;
        table[43] = 1;
        table[44..52].copy_from_slice(&0x3f8u64.to_le_bytes());
        table[52] = 1 << 1;
        table[54..58].copy_from_slice(&0u32.to_le_bytes());
        finish(&mut table, b"SPCR");

        let spcr = parse_spcr(&table).unwrap();
        assert_eq!(spcr.global_system_interrupt, Some(0));
        assert_eq!(spcr.legacy_irq, None);
    }

    #[test]
    fn spcr_preserves_io_sapic_gsi_zero() {
        let mut table = vec![0u8; SPCR_V1_SIZE];
        table[8] = 2;
        table[36] = 0;
        table[40] = 1;
        table[41] = 8;
        table[43] = 1;
        table[44..52].copy_from_slice(&0x3f8u64.to_le_bytes());
        table[52] = 1 << 2;
        table[54..58].copy_from_slice(&0u32.to_le_bytes());
        finish(&mut table, b"SPCR");

        let spcr = parse_spcr(&table).unwrap();
        assert_eq!(spcr.global_system_interrupt, Some(0));
        assert_eq!(spcr.legacy_irq, None);
    }

    #[test]
    fn pptt_rejects_cache_reference_cycles() {
        let mut table = vec![0u8; PPTT_HEADER_SIZE];
        table[8] = 1;
        let first_offset = table.len() as u32;
        let second_offset = first_offset + 24;
        let mut first = [0u8; 24];
        first[0] = 1;
        first[1] = 24;
        first[8..12].copy_from_slice(&second_offset.to_le_bytes());
        table.extend_from_slice(&first);
        let mut second = [0u8; 24];
        second[0] = 1;
        second[1] = 24;
        second[8..12].copy_from_slice(&first_offset.to_le_bytes());
        table.extend_from_slice(&second);
        finish(&mut table, b"PPTT");
        assert_eq!(parse_pptt(&table), Err(AcpiTableError::InvalidReference));
    }

    #[test]
    fn pptt_revision_three_requires_full_cache_structure() {
        let mut table = vec![0u8; PPTT_HEADER_SIZE];
        table[8] = 3;
        let mut cache = [0u8; 28];
        cache[0] = 1;
        cache[1] = 28;
        table.extend_from_slice(&cache);
        finish(&mut table, b"PPTT");
        assert_eq!(parse_pptt(&table).unwrap().caches.len(), 1);

        table.truncate(PPTT_HEADER_SIZE + 24);
        table[PPTT_HEADER_SIZE + 1] = 24;
        finish(&mut table, b"PPTT");
        assert_eq!(parse_pptt(&table), Err(AcpiTableError::InvalidLength));
    }
}

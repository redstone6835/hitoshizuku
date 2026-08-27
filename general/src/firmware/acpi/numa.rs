use super::types::{
    AcpiNumaDeviceHandle, AcpiNumaInitiatorAffinity, AcpiNumaInitiatorKind, AcpiNumaMemoryAffinity,
    AcpiNumaProcessorAffinity, AcpiNumaProcessorKind, AcpiSlitInfo, AcpiSratInfo, AcpiTableError,
    checked_sdt, read_u16, read_u32, read_u64,
};

const SRAT_HEADER_SIZE: usize = 48;
const SLIT_HEADER_SIZE: usize = 44;

pub fn parse_srat(bytes: &[u8]) -> Result<AcpiSratInfo, AcpiTableError> {
    let table = checked_sdt(bytes, b"SRAT", SRAT_HEADER_SIZE)?;
    if read_u32(table, 36) != Some(1) || read_u64(table, 40) != Some(0) {
        return Err(AcpiTableError::InvalidFlags);
    }

    let mut info = AcpiSratInfo::default();
    let mut offset = SRAT_HEADER_SIZE;
    while offset < table.len() {
        let header = table
            .get(offset..offset.checked_add(2).ok_or(AcpiTableError::InvalidLength)?)
            .ok_or(AcpiTableError::TruncatedEntry)?;
        let entry_type = header[0];
        let length = usize::from(header[1]);
        if length < 2 {
            return Err(AcpiTableError::InvalidLength);
        }
        let end = offset
            .checked_add(length)
            .ok_or(AcpiTableError::InvalidLength)?;
        let entry = table
            .get(offset..end)
            .ok_or(AcpiTableError::TruncatedEntry)?;
        parse_srat_entry(entry_type, entry, &mut info)?;
        offset = end;
    }
    validate_memory_ranges(&info.memory_affinities)?;
    Ok(info)
}

fn parse_srat_entry(
    entry_type: u8,
    entry: &[u8],
    info: &mut AcpiSratInfo,
) -> Result<(), AcpiTableError> {
    let processor = match entry_type {
        0 => {
            require_exact(entry, 16)?;
            let flags = checked_flags(entry, 4, 0x01)?;
            if flags & 1 == 0 {
                return Ok(());
            }
            let domain = u32::from(entry[2])
                | (u32::from(entry[9]) << 8)
                | (u32::from(entry[10]) << 16)
                | (u32::from(entry[11]) << 24);
            Some(AcpiNumaProcessorAffinity {
                kind: AcpiNumaProcessorKind::LocalApic,
                hardware_id: Some(u64::from(entry[3]) | (u64::from(entry[8]) << 8)),
                processor_uid: None,
                proximity_domain: domain,
                clock_domain: read_u32(entry, 12).ok_or(AcpiTableError::TruncatedEntry)?,
                enabled: flags & 1 != 0,
            })
        }
        1 => {
            require_exact(entry, 40)?;
            let flags = checked_flags(entry, 28, 0x0f)?;
            if flags & 1 == 0 {
                return Ok(());
            }
            if entry[6..8].iter().any(|byte| *byte != 0)
                || entry[24..28].iter().any(|byte| *byte != 0)
                || entry[32..40].iter().any(|byte| *byte != 0)
            {
                return Err(AcpiTableError::InvalidFlags);
            }
            let base = read_u64(entry, 8).ok_or(AcpiTableError::TruncatedEntry)?;
            let length = read_u64(entry, 16).ok_or(AcpiTableError::TruncatedEntry)?;
            if length == 0
                || base & 0xffff != 0
                || length & 0xffff != 0
                || base.checked_add(length).is_none()
            {
                return Err(AcpiTableError::InvalidAddress);
            }
            info.memory_affinities.push(AcpiNumaMemoryAffinity {
                proximity_domain: read_u32(entry, 2).ok_or(AcpiTableError::TruncatedEntry)?,
                base,
                length,
                enabled: flags & 1 != 0,
                hot_pluggable: flags & 2 != 0,
                non_volatile: flags & 4 != 0,
                specific_purpose: flags & 8 != 0,
            });
            None
        }
        2 => {
            require_exact(entry, 24)?;
            let flags = checked_flags(entry, 12, 0x01)?;
            if flags & 1 == 0 {
                return Ok(());
            }
            if entry[2..4].iter().any(|byte| *byte != 0)
                || entry[20..24].iter().any(|byte| *byte != 0)
            {
                return Err(AcpiTableError::InvalidFlags);
            }
            Some(AcpiNumaProcessorAffinity {
                kind: AcpiNumaProcessorKind::LocalX2Apic,
                hardware_id: Some(u64::from(
                    read_u32(entry, 8).ok_or(AcpiTableError::TruncatedEntry)?,
                )),
                processor_uid: None,
                proximity_domain: read_u32(entry, 4).ok_or(AcpiTableError::TruncatedEntry)?,
                clock_domain: read_u32(entry, 16).ok_or(AcpiTableError::TruncatedEntry)?,
                enabled: flags & 1 != 0,
            })
        }
        3 => {
            require_exact(entry, 18)?;
            let flags = checked_flags(entry, 10, 0x01)?;
            if flags & 1 == 0 {
                return Ok(());
            }
            Some(AcpiNumaProcessorAffinity {
                kind: AcpiNumaProcessorKind::Gicc,
                hardware_id: None,
                processor_uid: Some(read_u32(entry, 6).ok_or(AcpiTableError::TruncatedEntry)?),
                proximity_domain: read_u32(entry, 2).ok_or(AcpiTableError::TruncatedEntry)?,
                clock_domain: read_u32(entry, 14).ok_or(AcpiTableError::TruncatedEntry)?,
                enabled: flags & 1 != 0,
            })
        }
        4 => {
            require_exact(entry, 12)?;
            if entry[6..8].iter().any(|byte| *byte != 0) {
                return Err(AcpiTableError::InvalidFlags);
            }
            push_initiator(
                info,
                AcpiNumaInitiatorAffinity {
                    kind: AcpiNumaInitiatorKind::GicIts,
                    handle: AcpiNumaDeviceHandle::GicIts {
                        id: read_u32(entry, 8).ok_or(AcpiTableError::TruncatedEntry)?,
                    },
                    proximity_domain: read_u32(entry, 2).ok_or(AcpiTableError::TruncatedEntry)?,
                    enabled: true,
                    architectural_transactions: true,
                },
            )?;
            None
        }
        5 | 6 => {
            require_exact(entry, 32)?;
            let flags = checked_flags(entry, 24, 0x03)?;
            if flags & 1 == 0 {
                return Ok(());
            }
            if entry[2] != 0 || entry[28..32].iter().any(|byte| *byte != 0) {
                return Err(AcpiTableError::InvalidFlags);
            }
            let handle = parse_device_handle(entry[3], &entry[8..24])?;
            push_initiator(
                info,
                AcpiNumaInitiatorAffinity {
                    kind: if entry_type == 5 {
                        AcpiNumaInitiatorKind::GenericInitiator
                    } else {
                        AcpiNumaInitiatorKind::GenericPort
                    },
                    handle,
                    proximity_domain: read_u32(entry, 4).ok_or(AcpiTableError::TruncatedEntry)?,
                    enabled: flags & 1 != 0,
                    architectural_transactions: flags & 2 != 0,
                },
            )?;
            None
        }
        7 => {
            require_exact(entry, 20)?;
            let flags = checked_flags(entry, 12, 0x01)?;
            if flags & 1 == 0 {
                return Ok(());
            }
            if entry[2..4].iter().any(|byte| *byte != 0) {
                return Err(AcpiTableError::InvalidFlags);
            }
            Some(AcpiNumaProcessorAffinity {
                kind: AcpiNumaProcessorKind::RiscVIntc,
                hardware_id: None,
                processor_uid: Some(read_u32(entry, 8).ok_or(AcpiTableError::TruncatedEntry)?),
                proximity_domain: read_u32(entry, 4).ok_or(AcpiTableError::TruncatedEntry)?,
                clock_domain: read_u32(entry, 16).ok_or(AcpiTableError::TruncatedEntry)?,
                enabled: flags & 1 != 0,
            })
        }
        _ => {
            if !info.unknown_entry_types.contains(&entry_type) {
                info.unknown_entry_types.push(entry_type);
            }
            None
        }
    };

    if let Some(processor) = processor {
        if info.processor_affinities.iter().any(|existing| {
            existing.kind == processor.kind
                && ((processor.hardware_id.is_some()
                    && existing.hardware_id == processor.hardware_id)
                    || (processor.processor_uid.is_some()
                        && existing.processor_uid == processor.processor_uid))
        }) {
            return Err(AcpiTableError::DuplicateEntry);
        }
        info.processor_affinities.push(processor);
    }
    Ok(())
}

fn checked_flags(entry: &[u8], offset: usize, allowed: u32) -> Result<u32, AcpiTableError> {
    let flags = read_u32(entry, offset).ok_or(AcpiTableError::TruncatedEntry)?;
    if flags & !allowed != 0 {
        Err(AcpiTableError::InvalidFlags)
    } else {
        Ok(flags)
    }
}

fn parse_device_handle(
    handle_type: u8,
    bytes: &[u8],
) -> Result<AcpiNumaDeviceHandle, AcpiTableError> {
    match handle_type {
        0 => {
            if bytes[12..16].iter().any(|byte| *byte != 0) {
                return Err(AcpiTableError::InvalidFlags);
            }
            Ok(AcpiNumaDeviceHandle::Acpi {
                hid: read_u64(bytes, 0).ok_or(AcpiTableError::TruncatedEntry)?,
                uid: read_u32(bytes, 8).ok_or(AcpiTableError::TruncatedEntry)?,
            })
        }
        1 => {
            if bytes[4..16].iter().any(|byte| *byte != 0) {
                return Err(AcpiTableError::InvalidFlags);
            }
            let segment = read_u16(bytes, 0).ok_or(AcpiTableError::TruncatedEntry)?;
            Ok(AcpiNumaDeviceHandle::Pci {
                segment,
                bus: bytes[2],
                device: bytes[3] >> 3,
                function: bytes[3] & 0x07,
            })
        }
        _ => Err(AcpiTableError::InvalidFlags),
    }
}

fn push_initiator(
    info: &mut AcpiSratInfo,
    initiator: AcpiNumaInitiatorAffinity,
) -> Result<(), AcpiTableError> {
    if info
        .initiator_affinities
        .iter()
        .any(|existing| existing.kind == initiator.kind && existing.handle == initiator.handle)
    {
        return Err(AcpiTableError::DuplicateEntry);
    }
    info.initiator_affinities.push(initiator);
    Ok(())
}

fn validate_memory_ranges(ranges: &[AcpiNumaMemoryAffinity]) -> Result<(), AcpiTableError> {
    for (index, range) in ranges.iter().enumerate() {
        if !range.enabled {
            continue;
        }
        let end = range
            .base
            .checked_add(range.length)
            .ok_or(AcpiTableError::InvalidAddress)?;
        if ranges[..index]
            .iter()
            .filter(|other| other.enabled)
            .any(|other| {
                let other_end = other.base.saturating_add(other.length);
                range.base < other_end && other.base < end
            })
        {
            return Err(AcpiTableError::OverlappingRange);
        }
    }
    Ok(())
}

pub fn parse_slit(bytes: &[u8]) -> Result<AcpiSlitInfo, AcpiTableError> {
    let table = checked_sdt(bytes, b"SLIT", SLIT_HEADER_SIZE)?;
    let locality_count = usize::try_from(read_u64(table, 36).ok_or(AcpiTableError::InvalidLength)?)
        .map_err(|_| AcpiTableError::InvalidLength)?;
    if locality_count == 0 {
        return Err(AcpiTableError::InvalidLength);
    }
    let distance_count = locality_count
        .checked_mul(locality_count)
        .ok_or(AcpiTableError::InvalidLength)?;
    let end = SLIT_HEADER_SIZE
        .checked_add(distance_count)
        .ok_or(AcpiTableError::InvalidLength)?;
    if end != table.len() {
        return Err(AcpiTableError::InvalidLength);
    }
    let distances = table[SLIT_HEADER_SIZE..end].to_vec();
    for from in 0..locality_count {
        for to in 0..locality_count {
            let distance = distances[from * locality_count + to];
            if (from == to && distance != 10) || (from != to && distance < 10) {
                return Err(AcpiTableError::InvalidFlags);
            }
        }
    }
    Ok(AcpiSlitInfo {
        locality_count,
        distances,
    })
}

fn require_exact(entry: &[u8], length: usize) -> Result<(), AcpiTableError> {
    if entry.len() == length {
        Ok(())
    } else {
        Err(AcpiTableError::InvalidLength)
    }
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
    fn parses_x2apic_and_memory_affinity() {
        let mut table = vec![0u8; SRAT_HEADER_SIZE];
        table[36..40].copy_from_slice(&1u32.to_le_bytes());
        let mut processor = [0u8; 24];
        processor[0] = 2;
        processor[1] = 24;
        processor[4..8].copy_from_slice(&3u32.to_le_bytes());
        processor[8..12].copy_from_slice(&0x42u32.to_le_bytes());
        processor[12..16].copy_from_slice(&1u32.to_le_bytes());
        table.extend_from_slice(&processor);
        let mut memory = [0u8; 40];
        memory[0] = 1;
        memory[1] = 40;
        memory[2..6].copy_from_slice(&3u32.to_le_bytes());
        memory[8..16].copy_from_slice(&0x1_0000u64.to_le_bytes());
        memory[16..24].copy_from_slice(&0x2_0000u64.to_le_bytes());
        memory[28..32].copy_from_slice(&1u32.to_le_bytes());
        table.extend_from_slice(&memory);
        finish(&mut table, b"SRAT");

        let info = parse_srat(&table).unwrap();
        assert_eq!(info.processor_affinities[0].hardware_id, Some(0x42));
        assert_eq!(info.memory_affinities[0].proximity_domain, 3);
    }

    #[test]
    fn parses_pci_generic_initiator_affinity() {
        let mut table = vec![0u8; SRAT_HEADER_SIZE];
        table[36..40].copy_from_slice(&1u32.to_le_bytes());
        let mut initiator = [0u8; 32];
        initiator[0] = 5;
        initiator[1] = 32;
        initiator[3] = 1;
        initiator[4..8].copy_from_slice(&4u32.to_le_bytes());
        initiator[8..10].copy_from_slice(&2u16.to_le_bytes());
        initiator[10] = 1;
        initiator[11] = (5 << 3) | 3;
        initiator[24..28].copy_from_slice(&3u32.to_le_bytes());
        table.extend_from_slice(&initiator);
        finish(&mut table, b"SRAT");

        let info = parse_srat(&table).unwrap();
        assert_eq!(
            info.initiator_affinities[0].handle,
            AcpiNumaDeviceHandle::Pci {
                segment: 2,
                bus: 1,
                device: 5,
                function: 3,
            }
        );
        assert!(info.initiator_affinities[0].architectural_transactions);
    }

    #[test]
    fn slit_keeps_asymmetric_but_valid_distances() {
        let mut table = vec![0u8; SLIT_HEADER_SIZE];
        table[36..44].copy_from_slice(&2u64.to_le_bytes());
        table.extend_from_slice(&[10, 20, 30, 10]);
        finish(&mut table, b"SLIT");
        let slit = parse_slit(&table).unwrap();
        assert_eq!(slit.distance(0, 1), Some(20));
        assert_eq!(slit.distance(1, 0), Some(30));
    }

    #[test]
    fn slit_allows_equal_local_and_remote_distance() {
        let mut table = vec![0u8; SLIT_HEADER_SIZE];
        table[36..44].copy_from_slice(&2u64.to_le_bytes());
        table.extend_from_slice(&[10, 10, 10, 10]);
        finish(&mut table, b"SLIT");
        assert_eq!(parse_slit(&table).unwrap().distance(0, 1), Some(10));
    }

    #[test]
    fn disabled_memory_affinity_does_not_poison_the_table() {
        let mut table = vec![0u8; SRAT_HEADER_SIZE];
        table[36..40].copy_from_slice(&1u32.to_le_bytes());
        let mut memory = [0xffu8; 40];
        memory[0] = 1;
        memory[1] = 40;
        memory[28..32].copy_from_slice(&0u32.to_le_bytes());
        table.extend_from_slice(&memory);
        finish(&mut table, b"SRAT");
        assert!(parse_srat(&table).unwrap().memory_affinities.is_empty());
    }

    #[test]
    fn disabled_processor_affinity_does_not_conflict_with_enabled_entry() {
        let mut table = vec![0u8; SRAT_HEADER_SIZE];
        table[36..40].copy_from_slice(&1u32.to_le_bytes());
        let mut disabled = [0xffu8; 24];
        disabled[0] = 2;
        disabled[1] = 24;
        disabled[12..16].copy_from_slice(&0u32.to_le_bytes());
        table.extend_from_slice(&disabled);
        let mut enabled = [0u8; 24];
        enabled[0] = 2;
        enabled[1] = 24;
        enabled[4..8].copy_from_slice(&3u32.to_le_bytes());
        enabled[8..12].copy_from_slice(&7u32.to_le_bytes());
        enabled[12..16].copy_from_slice(&1u32.to_le_bytes());
        table.extend_from_slice(&enabled);
        finish(&mut table, b"SRAT");
        let srat = parse_srat(&table).unwrap();
        assert_eq!(srat.processor_affinities.len(), 1);
        assert_eq!(srat.processor_affinities[0].hardware_id, Some(7));
    }
}

use alloc::vec::Vec;

use super::types::{
    AcpiInterruptAttributes, AcpiInterruptOverride, AcpiInterruptPolarity, AcpiInterruptTrigger,
    AcpiIoApic, AcpiLocalNmi, AcpiMadtInfo, AcpiMultiprocessorWakeup, AcpiNmiSource, AcpiProcessor,
    AcpiProcessorInterface, AcpiTableError, checked_sdt, read_u16, read_u32, read_u64,
};

const MADT_HEADER_SIZE: usize = 44;
const PROCESSOR_ENABLED: u32 = 1 << 0;
const PROCESSOR_ONLINE_CAPABLE: u32 = 1 << 1;
const GICC_ONLINE_CAPABLE: u32 = 1 << 3;

pub fn parse_madt(bytes: &[u8]) -> Result<AcpiMadtInfo, AcpiTableError> {
    let table = checked_sdt(bytes, b"APIC", MADT_HEADER_SIZE)?;
    let table_flags = read_u32(table, 40).ok_or(AcpiTableError::InvalidLength)?;
    if table_flags & !1 != 0 {
        return Err(AcpiTableError::InvalidFlags);
    }
    let mut info = AcpiMadtInfo {
        local_apic_address: u64::from(read_u32(table, 36).ok_or(AcpiTableError::InvalidLength)?),
        has_legacy_pic: table_flags & 1 != 0,
        ..AcpiMadtInfo::default()
    };
    let mut local_apic_override_seen = false;
    let table_revision = table[8];

    let mut offset = MADT_HEADER_SIZE;
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
        parse_entry(
            entry_type,
            entry,
            table_revision,
            &mut info,
            &mut local_apic_override_seen,
        )?;
        offset = end;
    }

    Ok(info)
}

fn parse_entry(
    entry_type: u8,
    entry: &[u8],
    table_revision: u8,
    info: &mut AcpiMadtInfo,
    local_apic_override_seen: &mut bool,
) -> Result<(), AcpiTableError> {
    match entry_type {
        0 => {
            require_exact(entry, 8)?;
            let entry_flags = checked_processor_flags(entry, 4, 0x03)?;
            let (enabled, online_capable) =
                processor_availability(entry_flags, PROCESSOR_ONLINE_CAPABLE)?;
            push_processor(
                info,
                AcpiProcessor {
                    interface: AcpiProcessorInterface::LocalApic,
                    processor_uid: u32::from(entry[2]),
                    hardware_id: u64::from(entry[3]),
                    enabled,
                    online_capable,
                    interrupt_controller_non_coherent: false,
                },
            )?;
        }
        1 => {
            require_exact(entry, 12)?;
            if entry[3] != 0 {
                return Err(AcpiTableError::InvalidFlags);
            }
            let io_apic = AcpiIoApic {
                id: entry[2],
                address: read_u32(entry, 4).ok_or(AcpiTableError::TruncatedEntry)?,
                global_system_interrupt_base: read_u32(entry, 8)
                    .ok_or(AcpiTableError::TruncatedEntry)?,
            };
            if io_apic.address == 0
                || info
                    .io_apics
                    .iter()
                    .any(|existing| existing.id == io_apic.id)
            {
                return Err(if io_apic.address == 0 {
                    AcpiTableError::InvalidAddress
                } else {
                    AcpiTableError::DuplicateEntry
                });
            }
            info.io_apics.push(io_apic);
        }
        2 => {
            require_exact(entry, 10)?;
            if entry[2] != 0 || entry[3] >= 16 {
                return Err(AcpiTableError::InvalidFlags);
            }
            let interrupt = AcpiInterruptOverride {
                bus: entry[2],
                source: entry[3],
                global_system_interrupt: read_u32(entry, 4)
                    .ok_or(AcpiTableError::TruncatedEntry)?,
                attributes: parse_interrupt_flags(
                    read_u16(entry, 8).ok_or(AcpiTableError::TruncatedEntry)?,
                )?,
            };
            if info.interrupt_overrides.iter().any(|existing| {
                existing.bus == interrupt.bus && existing.source == interrupt.source
            }) {
                return Err(AcpiTableError::DuplicateEntry);
            }
            info.interrupt_overrides.push(interrupt);
        }
        3 => {
            require_exact(entry, 8)?;
            let source = AcpiNmiSource {
                global_system_interrupt: read_u32(entry, 4)
                    .ok_or(AcpiTableError::TruncatedEntry)?,
                attributes: parse_interrupt_flags(
                    read_u16(entry, 2).ok_or(AcpiTableError::TruncatedEntry)?,
                )?,
            };
            if info
                .nmi_sources
                .iter()
                .any(|existing| existing.global_system_interrupt == source.global_system_interrupt)
            {
                return Err(AcpiTableError::DuplicateEntry);
            }
            info.nmi_sources.push(source);
        }
        4 => {
            require_exact(entry, 6)?;
            let lint = entry[5];
            if lint > 1 {
                return Err(AcpiTableError::InvalidFlags);
            }
            let local_nmi = AcpiLocalNmi {
                processor_uid: (entry[2] != u8::MAX).then_some(u32::from(entry[2])),
                lint,
                attributes: parse_interrupt_flags(
                    read_u16(entry, 3).ok_or(AcpiTableError::TruncatedEntry)?,
                )?,
            };
            push_local_nmi(info, local_nmi)?;
        }
        5 => {
            require_exact(entry, 12)?;
            if entry[2..4].iter().any(|byte| *byte != 0) {
                return Err(AcpiTableError::InvalidFlags);
            }
            let address = read_u64(entry, 4).ok_or(AcpiTableError::TruncatedEntry)?;
            if address == 0 {
                return Err(AcpiTableError::InvalidAddress);
            }
            if *local_apic_override_seen {
                return Err(AcpiTableError::DuplicateEntry);
            }
            *local_apic_override_seen = true;
            info.local_apic_address = address;
        }
        7 => {
            require(entry, 16)?;
            // Local SAPIC layout (ACPI MADT type 7): processor id @2,
            // SAPIC id @3, SAPIC EID @4, reserved[5..8], flags @8, numeric
            // ACPI UID @12, followed by an optional NUL-terminated UID
            // string at @16.  The flags/UID fields are four-byte fields; the
            // older parser used two-byte offsets and consequently rejected
            // valid Itanium/SAPIC entries or associated the wrong CPU.
            if entry[5..8].iter().any(|byte| *byte != 0) {
                return Err(AcpiTableError::InvalidFlags);
            }
            let entry_flags = checked_processor_flags(entry, 8, 0x03)?;
            let (enabled, online_capable) =
                processor_availability(entry_flags, PROCESSOR_ONLINE_CAPABLE)?;
            if entry.len() > 16 {
                let uid_string = &entry[16..];
                // The variable string occupies the descriptor tail and its
                // terminator must be the final byte.  Embedded terminators or
                // non-zero bytes after one would make the next descriptor
                // indistinguishable from part of the UID.
                if uid_string.last() != Some(&0) || uid_string[..uid_string.len() - 1].contains(&0)
                {
                    return Err(AcpiTableError::InvalidReference);
                }
            }
            push_processor(
                info,
                AcpiProcessor {
                    interface: AcpiProcessorInterface::LocalSapic,
                    processor_uid: read_u32(entry, 12).ok_or(AcpiTableError::TruncatedEntry)?,
                    hardware_id: u64::from(entry[3]) | (u64::from(entry[4]) << 8),
                    enabled,
                    online_capable,
                    interrupt_controller_non_coherent: false,
                },
            )?;
        }
        9 => {
            require_exact(entry, 16)?;
            if entry[2..4].iter().any(|byte| *byte != 0) {
                return Err(AcpiTableError::InvalidFlags);
            }
            let entry_flags = checked_processor_flags(entry, 8, 0x03)?;
            let (enabled, online_capable) =
                processor_availability(entry_flags, PROCESSOR_ONLINE_CAPABLE)?;
            push_processor(
                info,
                AcpiProcessor {
                    interface: AcpiProcessorInterface::LocalX2Apic,
                    processor_uid: read_u32(entry, 12).ok_or(AcpiTableError::TruncatedEntry)?,
                    hardware_id: u64::from(
                        read_u32(entry, 4).ok_or(AcpiTableError::TruncatedEntry)?,
                    ),
                    enabled,
                    online_capable,
                    interrupt_controller_non_coherent: false,
                },
            )?;
        }
        10 => {
            require_exact(entry, 12)?;
            if entry[9..12].iter().any(|byte| *byte != 0) {
                return Err(AcpiTableError::InvalidFlags);
            }
            let lint = entry[8];
            if lint > 1 {
                return Err(AcpiTableError::InvalidFlags);
            }
            let uid = read_u32(entry, 4).ok_or(AcpiTableError::TruncatedEntry)?;
            let local_nmi = AcpiLocalNmi {
                processor_uid: (uid != u32::MAX).then_some(uid),
                lint,
                attributes: parse_interrupt_flags(
                    read_u16(entry, 2).ok_or(AcpiTableError::TruncatedEntry)?,
                )?,
            };
            push_local_nmi(info, local_nmi)?;
        }
        11 => {
            let allowed_flags = match (table_revision, entry.len()) {
                (3, 40) => 0x03,
                (3, 76 | 80) | (4 | 5, 80) => 0x07,
                (6, 82) => 0x0f,
                (7, 82) => 0x1f,
                _ => return Err(AcpiTableError::InvalidLength),
            };
            if entry[2..4].iter().any(|byte| *byte != 0) {
                return Err(AcpiTableError::InvalidFlags);
            }
            let entry_flags = checked_processor_flags(entry, 12, allowed_flags)?;
            let (enabled, online_capable) =
                processor_availability(entry_flags, GICC_ONLINE_CAPABLE)?;
            // The 40-byte ACPI 5.0 entry predates MPIDR. Preserve its CPU Interface
            // Number as the stable controller identity for topology matching.
            let hardware_id = if entry.len() >= 76 {
                read_u64(entry, 68).ok_or(AcpiTableError::TruncatedEntry)?
            } else {
                u64::from(read_u32(entry, 4).ok_or(AcpiTableError::TruncatedEntry)?)
            };
            push_processor(
                info,
                AcpiProcessor {
                    interface: AcpiProcessorInterface::Gicc,
                    processor_uid: read_u32(entry, 8).ok_or(AcpiTableError::TruncatedEntry)?,
                    hardware_id,
                    enabled,
                    online_capable,
                    interrupt_controller_non_coherent: entry_flags & (1 << 4) != 0,
                },
            )?;
        }
        16 => {
            require(entry, 16)?;
            let version = read_u16(entry, 2).ok_or(AcpiTableError::TruncatedEntry)?;
            if read_u32(entry, 4) != Some(0) {
                return Err(AcpiTableError::InvalidFlags);
            }
            let reset_vector = match (table_revision, version, entry.len()) {
                (5..=u8::MAX, 0, 16) => None,
                (7, 1, 24) => Some(read_u64(entry, 16).ok_or(AcpiTableError::TruncatedEntry)?),
                _ => return Err(AcpiTableError::InvalidLength),
            };
            let wakeup = AcpiMultiprocessorWakeup {
                mailbox_version: version,
                mailbox_address: read_u64(entry, 8).ok_or(AcpiTableError::TruncatedEntry)?,
                reset_vector,
            };
            if wakeup.mailbox_address == 0 || wakeup.mailbox_address & 0xfff != 0 {
                return Err(AcpiTableError::InvalidAddress);
            }
            if info.multiprocessor_wakeup.replace(wakeup).is_some() {
                return Err(AcpiTableError::DuplicateEntry);
            }
        }
        17 => {
            require_exact(entry, 15)?;
            if entry[2] != 1 {
                return Err(AcpiTableError::InvalidFlags);
            }
            let entry_flags = checked_processor_flags(entry, 11, 0x01)?;
            let hardware_id = read_u32(entry, 7).ok_or(AcpiTableError::TruncatedEntry)?;
            if hardware_id == u32::MAX {
                return Ok(());
            }
            push_processor(
                info,
                AcpiProcessor {
                    interface: AcpiProcessorInterface::LoongArchCorePic,
                    processor_uid: read_u32(entry, 3).ok_or(AcpiTableError::TruncatedEntry)?,
                    hardware_id: u64::from(hardware_id),
                    enabled: entry_flags & PROCESSOR_ENABLED != 0,
                    online_capable: false,
                    interrupt_controller_non_coherent: false,
                },
            )?;
        }
        24 => {
            require_exact(entry, 36)?;
            if entry[2] != 1 || entry[3] != 0 {
                return Err(AcpiTableError::InvalidFlags);
            }
            let entry_flags = checked_processor_flags(entry, 4, 0x03)?;
            let (enabled, online_capable) =
                processor_availability(entry_flags, PROCESSOR_ONLINE_CAPABLE)?;
            push_processor(
                info,
                AcpiProcessor {
                    interface: AcpiProcessorInterface::RiscVIntc,
                    processor_uid: read_u32(entry, 16).ok_or(AcpiTableError::TruncatedEntry)?,
                    hardware_id: read_u64(entry, 8).ok_or(AcpiTableError::TruncatedEntry)?,
                    enabled,
                    online_capable,
                    interrupt_controller_non_coherent: false,
                },
            )?;
        }
        _ => push_unique(&mut info.unknown_entry_types, entry_type),
    }
    Ok(())
}

fn push_processor(info: &mut AcpiMadtInfo, processor: AcpiProcessor) -> Result<(), AcpiTableError> {
    if info.processors.iter().any(|existing| {
        existing.processor_uid == processor.processor_uid
            || existing.interface == processor.interface
                && existing.hardware_id == processor.hardware_id
    }) {
        return Err(AcpiTableError::DuplicateEntry);
    }
    if processor.interface == AcpiProcessorInterface::Gicc
        && info.processors.iter().any(|existing| {
            existing.interface == AcpiProcessorInterface::Gicc
                && existing.interrupt_controller_non_coherent
                    != processor.interrupt_controller_non_coherent
        })
    {
        return Err(AcpiTableError::InvalidFlags);
    }
    info.processors.push(processor);
    Ok(())
}

fn push_local_nmi(info: &mut AcpiMadtInfo, local_nmi: AcpiLocalNmi) -> Result<(), AcpiTableError> {
    if info.local_nmis.iter().any(|existing| {
        existing.processor_uid == local_nmi.processor_uid && existing.lint == local_nmi.lint
    }) {
        return Err(AcpiTableError::DuplicateEntry);
    }
    info.local_nmis.push(local_nmi);
    Ok(())
}

fn parse_interrupt_flags(raw: u16) -> Result<AcpiInterruptAttributes, AcpiTableError> {
    if raw & !0x0f != 0 {
        return Err(AcpiTableError::InvalidFlags);
    }
    let polarity = match raw & 0b11 {
        0 => AcpiInterruptPolarity::Conforms,
        1 => AcpiInterruptPolarity::ActiveHigh,
        3 => AcpiInterruptPolarity::ActiveLow,
        _ => return Err(AcpiTableError::InvalidFlags),
    };
    let trigger = match (raw >> 2) & 0b11 {
        0 => AcpiInterruptTrigger::Conforms,
        1 => AcpiInterruptTrigger::Edge,
        3 => AcpiInterruptTrigger::Level,
        _ => return Err(AcpiTableError::InvalidFlags),
    };
    Ok(AcpiInterruptAttributes { polarity, trigger })
}

fn flags(entry: &[u8], offset: usize) -> Result<u32, AcpiTableError> {
    read_u32(entry, offset).ok_or(AcpiTableError::TruncatedEntry)
}

fn checked_processor_flags(
    entry: &[u8],
    offset: usize,
    allowed: u32,
) -> Result<u32, AcpiTableError> {
    let value = flags(entry, offset)?;
    if value & !allowed != 0 {
        Err(AcpiTableError::InvalidFlags)
    } else {
        Ok(value)
    }
}

fn processor_availability(
    flags: u32,
    online_capable_bit: u32,
) -> Result<(bool, bool), AcpiTableError> {
    let enabled = flags & PROCESSOR_ENABLED != 0;
    let online_capable = flags & online_capable_bit != 0;
    // ACPI defines Processor Enabled and Online Capable as independent
    // capabilities.  A processor may be currently enabled while also being
    // eligible for online/offline transitions (both bits set).
    Ok((enabled, online_capable))
}

fn require(entry: &[u8], minimum: usize) -> Result<(), AcpiTableError> {
    if entry.len() < minimum {
        Err(AcpiTableError::TruncatedEntry)
    } else {
        Ok(())
    }
}

fn require_exact(entry: &[u8], length: usize) -> Result<(), AcpiTableError> {
    if entry.len() == length {
        Ok(())
    } else {
        Err(AcpiTableError::InvalidLength)
    }
}

fn push_unique(values: &mut Vec<u8>, value: u8) {
    if !values.contains(&value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn finish(table: &mut [u8]) {
        let length = table.len() as u32;
        table[..4].copy_from_slice(b"APIC");
        table[4..8].copy_from_slice(&length.to_le_bytes());
        table[9] = 0;
        table[9] = 0u8.wrapping_sub(table.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
    }

    #[test]
    fn parses_x2apic_ioapic_override_and_wakeup() {
        let mut table = vec![0u8; MADT_HEADER_SIZE];
        table[8] = 6;
        table[36..40].copy_from_slice(&0xfee0_0000u32.to_le_bytes());
        table[40..44].copy_from_slice(&1u32.to_le_bytes());

        table.extend_from_slice(&[9, 16, 0, 0, 0x34, 0x12, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0]);
        table.extend_from_slice(&[1, 12, 2, 0, 0, 0, 0xc0, 0xfe, 0, 0, 0, 0]);
        table.extend_from_slice(&[2, 10, 0, 0, 2, 0, 0, 0, 0, 0]);
        table.extend_from_slice(&[16, 16, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0]);
        finish(&mut table);

        let info = parse_madt(&table).unwrap();
        assert!(info.has_legacy_pic);
        assert_eq!(info.processors[0].hardware_id, 0x1234);
        assert!(info.processors[0].usable());
        assert_eq!(info.io_apics[0].address, 0xfec0_0000);
        assert_eq!(info.interrupt_overrides[0].global_system_interrupt, 2);
        assert_eq!(
            info.multiprocessor_wakeup.unwrap().mailbox_address,
            0x0000_1000
        );
    }

    #[test]
    fn accepts_legacy_multiprocessor_wakeup_on_newer_madt_revision() {
        let mut table = vec![0u8; MADT_HEADER_SIZE];
        table[8] = 7;
        table.extend_from_slice(&[
            16, 16, // type, length
            0, 0, 0, 0, 0, 0, // version and reserved
            0, 0, 0, 0, 0, 0, 0, 0, // mailbox address (filled below)
        ]);
        table[MADT_HEADER_SIZE + 8..MADT_HEADER_SIZE + 16]
            .copy_from_slice(&0x2000u64.to_le_bytes());
        finish(&mut table);

        let wakeup = parse_madt(&table).unwrap().multiprocessor_wakeup.unwrap();
        assert_eq!(wakeup.mailbox_version, 0);
        assert_eq!(wakeup.mailbox_address, 0x2000);
        assert_eq!(wakeup.reset_vector, None);
    }

    #[test]
    fn rejects_reserved_interrupt_flags() {
        let mut table = vec![0u8; MADT_HEADER_SIZE];
        table.extend_from_slice(&[2, 10, 0, 0, 2, 0, 0, 0, 2, 0]);
        finish(&mut table);
        assert_eq!(parse_madt(&table), Err(AcpiTableError::InvalidFlags));
    }

    #[test]
    fn preserves_processor_availability_flags() {
        let mut table = vec![0u8; MADT_HEADER_SIZE];
        table.extend_from_slice(&[0, 8, 1, 2, 0, 0, 0, 0]);
        finish(&mut table);
        let processor = parse_madt(&table).unwrap().processors[0];
        assert!(!processor.usable());

        table[MADT_HEADER_SIZE + 4..MADT_HEADER_SIZE + 8].copy_from_slice(&3u32.to_le_bytes());
        finish(&mut table);
        let processor = parse_madt(&table).unwrap().processors[0];
        assert!(processor.enabled);
        assert!(processor.online_capable);
    }

    #[test]
    fn preserves_gicc_non_coherent_flag() {
        let mut table = vec![0u8; MADT_HEADER_SIZE];
        table[8] = 7;
        let mut gicc = [0u8; 82];
        gicc[0] = 11;
        gicc[1] = 82;
        gicc[8..12].copy_from_slice(&7u32.to_le_bytes());
        gicc[12..16].copy_from_slice(&((1 << 4) | PROCESSOR_ENABLED).to_le_bytes());
        gicc[68..76].copy_from_slice(&0x8000_0000u64.to_le_bytes());
        table.extend_from_slice(&gicc);
        finish(&mut table);
        let processor = parse_madt(&table).unwrap().processors[0];
        assert!(processor.interrupt_controller_non_coherent);
    }

    #[test]
    fn preserves_disabled_and_ignores_invalid_core_pic_entries() {
        let mut table = vec![0u8; MADT_HEADER_SIZE];
        let mut disabled = [0u8; 15];
        disabled[0] = 17;
        disabled[1] = 15;
        disabled[2] = 1;
        disabled[3..7].copy_from_slice(&1u32.to_le_bytes());
        disabled[7..11].copy_from_slice(&2u32.to_le_bytes());
        table.extend_from_slice(&disabled);
        let mut invalid_id = disabled;
        invalid_id[7..11].copy_from_slice(&u32::MAX.to_le_bytes());
        invalid_id[11..15].copy_from_slice(&1u32.to_le_bytes());
        table.extend_from_slice(&invalid_id);
        finish(&mut table);
        let processors = parse_madt(&table).unwrap().processors;
        assert_eq!(processors.len(), 1);
        assert!(!processors[0].usable());
    }

    #[test]
    fn parses_local_sapic_field_offsets_and_uid_string() {
        let mut table = vec![0u8; MADT_HEADER_SIZE];
        let mut entry = vec![0u8; 20];
        entry[0] = 7;
        entry[1] = entry.len() as u8;
        entry[2] = 0xaa; // processor id
        entry[3] = 0x12; // SAPIC id
        entry[4] = 0x34; // SAPIC EID
        entry[8..12].copy_from_slice(&1u32.to_le_bytes()); // enabled
        entry[12..16].copy_from_slice(&0x7856_3412u32.to_le_bytes());
        // The string must be a single NUL-terminated tail.
        entry[16..20].copy_from_slice(b"CPU\0");
        table.extend_from_slice(&entry);
        finish(&mut table);

        let processor = parse_madt(&table).unwrap().processors[0];
        assert_eq!(processor.interface, AcpiProcessorInterface::LocalSapic);
        assert_eq!(processor.hardware_id, 0x3412);
        assert_eq!(processor.processor_uid, 0x7856_3412);
        assert!(processor.usable());
    }
}

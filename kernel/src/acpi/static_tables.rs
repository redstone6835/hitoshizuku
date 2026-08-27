//! ACPI 静态表目录与启动期结构审计。
//!
//! 这里不安装设备或中断控制器，只把 RSDT/XSDT 引用的静态表完整走查一遍，
//! 并提取启动阶段需要的 CPU 数量。所有变长表都使用显式边界检查，避免把固件
//! 提供的长度直接交给 crate 内部的裸指针迭代器。

use core::{mem, slice, str};

use acpi::bgrt::Bgrt;
use acpi::madt::Madt;
use acpi::mcfg::Mcfg;
use acpi::sdt::{SdtHeader, Signature};
use acpi::{AcpiTable, AcpiTables, PhysicalMapping};
use general::firmware::FirmwareTableMapping;
use log::printk;

use super::AcpiMapper;

const SDT_HEADER_SIZE: usize = mem::size_of::<SdtHeader>();
const MADT_HEADER_SIZE: usize = mem::size_of::<Madt>();
const MCFG_HEADER_SIZE: usize = mem::size_of::<Mcfg>();
const MCFG_ENTRY_SIZE: usize = 16;
const SRAT_HEADER_SIZE: usize = SDT_HEADER_SIZE + 12;
const SLIT_HEADER_SIZE: usize = SDT_HEADER_SIZE + 8;
const MAX_LOGGED_SLIT_CELLS: usize = 256;
const MADT_PROCESSOR_ENABLED: u32 = 1 << 0;
const MADT_PROCESSOR_ONLINE_CAPABLE: u32 = 1 << 1;
const MADT_GICC_ONLINE_CAPABLE: u32 = 1 << 3;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct StaticAcpiSummary {
    pub(super) table_count: usize,
    pub(super) copied_mapping_count: usize,
    pub(super) copied_table_bytes: usize,
    pub(super) cpu_count: usize,
    pub(super) fadt_present: bool,
    pub(super) madt: MadtSummary,
    pub(super) mcfg: McfgSummary,
    pub(super) hpet_present: bool,
    pub(super) bgrt_present: bool,
    pub(super) spcr_present: bool,
    pub(super) srat: SratSummary,
    pub(super) slit: SlitSummary,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MadtSummary {
    pub(super) entry_count: usize,
    pub(super) usable_processors: usize,
    pub(super) io_apics: usize,
    pub(super) interrupt_overrides: usize,
    pub(super) nmi_entries: usize,
    pub(super) gic_components: usize,
    pub(super) loongarch_components: usize,
    pub(super) riscv_components: usize,
    pub(super) unknown_entries: usize,
    pub(super) malformed_entries: usize,
    pub(super) complete: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct McfgSummary {
    pub(super) entry_count: usize,
    pub(super) valid_entries: usize,
    pub(super) malformed_entries: usize,
    pub(super) complete: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SratSummary {
    pub(super) entry_count: usize,
    pub(super) processor_affinities: usize,
    pub(super) enabled_processors: usize,
    pub(super) memory_affinities: usize,
    pub(super) enabled_memory_affinities: usize,
    pub(super) initiator_affinities: usize,
    pub(super) unknown_entries: usize,
    pub(super) malformed_entries: usize,
    pub(super) complete: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SlitSummary {
    pub(super) locality_count: usize,
    pub(super) distance_count: usize,
    pub(super) minimum_distance: u8,
    pub(super) maximum_distance: u8,
    pub(super) invalid_distances: usize,
    pub(super) asymmetric_pairs: usize,
    pub(super) complete: bool,
}

macro_rules! header_only_table {
    ($name:ident, $signature:expr) => {
        #[repr(transparent)]
        struct $name {
            header: SdtHeader,
        }

        // SAFETY: The type represents exactly the common SDT header. Variable payloads are
        // consumed only through the checked byte-slice parsers below.
        unsafe impl AcpiTable for $name {
            const SIGNATURE: Signature = $signature;

            fn header(&self) -> &SdtHeader {
                &self.header
            }
        }
    };
}

header_only_table!(RawHpetTable, Signature::HPET);
header_only_table!(RawFadtTable, Signature::FADT);
header_only_table!(RawSpcrTable, Signature::SPCR);
header_only_table!(RawSratTable, Signature::SRAT);
header_only_table!(RawSlitTable, Signature::SLIT);

pub(super) fn inspect(
    tables: &AcpiTables<AcpiMapper>,
    mappings: &'static [FirmwareTableMapping],
) -> StaticAcpiSummary {
    let mut summary = StaticAcpiSummary {
        copied_mapping_count: mappings.len(),
        copied_table_bytes: mappings.iter().fold(0usize, |total, mapping| {
            total.saturating_add(mapping.length)
        }),
        ..StaticAcpiSummary::default()
    };

    for header in tables.headers() {
        summary.table_count += 1;
        let signature = header.signature;
        let length = header.length;
        let revision = header.revision;
        let oem_revision = header.oem_revision;
        printk!(
            "[kernel-start][acpi] SDT {} rev={} len={} OEM={} table={} oem-rev={:#x}",
            signature,
            revision,
            length,
            header.oem_id(),
            header.oem_table_id(),
            oem_revision,
        );
    }

    summary.fadt_present = inspect_fadt(tables);
    summary.madt = inspect_madt(tables);
    summary.mcfg = inspect_mcfg(tables);
    summary.hpet_present = inspect_hpet(tables);
    summary.bgrt_present = inspect_bgrt(tables);
    summary.spcr_present = inspect_spcr(tables);
    summary.srat = inspect_srat(tables);
    summary.slit = inspect_slit(tables);
    summary.cpu_count = if summary.madt.complete {
        summary.madt.usable_processors.max(1)
    } else {
        1
    };

    printk!(
        "[kernel-start][acpi] static tables: tables={} mappings={} bytes={} cpus={} MCFG={} SRAT={} SLIT={} HPET={} BGRT={} SPCR={}",
        summary.table_count,
        summary.copied_mapping_count,
        summary.copied_table_bytes,
        summary.cpu_count,
        summary.mcfg.valid_entries,
        summary.srat.entry_count,
        summary.slit.locality_count,
        summary.hpet_present as usize,
        summary.bgrt_present as usize,
        summary.spcr_present as usize,
    );
    summary
}

fn mapping_bytes<T>(mapping: &PhysicalMapping<AcpiMapper, T>) -> &[u8] {
    // SAFETY: `PhysicalMapping` guarantees that `region_length` bytes beginning at
    // `virtual_start` remain mapped for the lifetime of the mapping.
    unsafe {
        slice::from_raw_parts(
            mapping.virtual_start().as_ptr().cast::<u8>(),
            mapping.region_length(),
        )
    }
}

fn inspect_fadt(tables: &AcpiTables<AcpiMapper>) -> bool {
    let mapping = match tables.find_table::<RawFadtTable>() {
        Ok(mapping) => mapping,
        Err(err) => {
            log::debug!("[kernel-start][acpi] FADT unavailable: {:?}", err);
            return false;
        }
    };
    let bytes = mapping_bytes(&mapping);
    if bytes.len() < 116 {
        printk!(
            "[kernel-start][acpi] FADT is too short: len={} minimum=116",
            bytes.len()
        );
        return false;
    }

    let revision = bytes[8];
    let profile = bytes[45];
    let sci = read_u16(bytes, 46).unwrap_or(0);
    let smi_command = read_u32(bytes, 48).unwrap_or(0);
    let acpi_enable = bytes[52];
    let acpi_disable = bytes[53];
    let legacy_facs = read_u32(bytes, 36).unwrap_or(0) as u64;
    let legacy_dsdt = read_u32(bytes, 40).unwrap_or(0) as u64;
    let extended_facs = read_u64(bytes, 132).filter(|address| *address != 0);
    let extended_dsdt = read_u64(bytes, 140).filter(|address| *address != 0);
    let facs = extended_facs.unwrap_or(legacy_facs);
    let dsdt = extended_dsdt.unwrap_or(legacy_dsdt);
    let flags = read_u32(bytes, 112).unwrap_or(0);
    let iapc_boot_arch = read_u16(bytes, 109).unwrap_or(0);

    printk!(
        "[kernel-start][acpi] FADT rev={} profile={} SCI={} SMI={:#x} enable={:#x} disable={:#x} flags={:#x} IA-PC={:#x}",
        revision,
        profile,
        sci,
        smi_command,
        acpi_enable,
        acpi_disable,
        flags,
        iapc_boot_arch,
    );
    printk!(
        "[kernel-start][acpi] FADT closure: FACS={:#x} DSDT={:#x} century={} C2={}us C3={}us",
        facs,
        dsdt,
        bytes[108],
        read_u16(bytes, 96).unwrap_or(0),
        read_u16(bytes, 98).unwrap_or(0),
    );
    printk!(
        "[kernel-start][acpi] FADT legacy blocks: PM1_EVT=({:#x},{:#x}) PM1_CNT=({:#x},{:#x}) PM2={:#x} PM_TMR={:#x} GPE=({:#x},{:#x})",
        read_u32(bytes, 56).unwrap_or(0),
        read_u32(bytes, 60).unwrap_or(0),
        read_u32(bytes, 64).unwrap_or(0),
        read_u32(bytes, 68).unwrap_or(0),
        read_u32(bytes, 72).unwrap_or(0),
        read_u32(bytes, 76).unwrap_or(0),
        read_u32(bytes, 80).unwrap_or(0),
        read_u32(bytes, 84).unwrap_or(0),
    );
    printk!(
        "[kernel-start][acpi] FADT block lengths: PM1_EVT={} PM1_CNT={} PM2={} PM_TMR={} GPE0={} GPE1={} GPE1_BASE={}",
        bytes[88],
        bytes[89],
        bytes[90],
        bytes[91],
        bytes[92],
        bytes[93],
        bytes[94],
    );
    if bytes.len() >= 132 {
        printk!(
            "[kernel-start][acpi] FADT reset-value={:#x} ARM-boot={:#x} minor-revision={}",
            bytes[128],
            read_u16(bytes, 129).unwrap_or(0),
            bytes[131],
        );
    }
    if let Some(hypervisor_vendor) = read_u64(bytes, 268)
        && hypervisor_vendor != 0
    {
        printk!(
            "[kernel-start][acpi] FADT hypervisor-vendor={:#x}",
            hypervisor_vendor,
        );
    }

    for (name, offset) in [
        ("RESET", 116usize),
        ("X_PM1A_EVT", 148),
        ("X_PM1B_EVT", 160),
        ("X_PM1A_CNT", 172),
        ("X_PM1B_CNT", 184),
        ("X_PM2_CNT", 196),
        ("X_PM_TMR", 208),
        ("X_GPE0", 220),
        ("X_GPE1", 232),
        ("SLEEP_CONTROL", 244),
        ("SLEEP_STATUS", 256),
    ] {
        if let Some(gas) = parse_gas(bytes, offset)
            && gas.address != 0
        {
            printk!(
                "[kernel-start][acpi] FADT {}: space={} addr={:#x} width={} offset={} access={}",
                name,
                gas.address_space,
                gas.address,
                gas.bit_width,
                gas.bit_offset,
                gas.access_size,
            );
        }
    }

    true
}

fn inspect_madt(tables: &AcpiTables<AcpiMapper>) -> MadtSummary {
    let mapping = match tables.find_table::<Madt>() {
        Ok(mapping) => mapping,
        Err(err) => {
            log::debug!("[kernel-start][acpi] MADT unavailable: {:?}", err);
            return MadtSummary::default();
        }
    };
    let bytes = mapping_bytes(&mapping);
    if bytes.len() < MADT_HEADER_SIZE {
        printk!(
            "[kernel-start][acpi] MADT is too short: len={} minimum={}",
            bytes.len(),
            MADT_HEADER_SIZE
        );
        return MadtSummary {
            malformed_entries: 1,
            ..MadtSummary::default()
        };
    }
    printk!(
        "[kernel-start][acpi] MADT LAPIC/base={:#x} flags={:#x}",
        read_u32(bytes, SDT_HEADER_SIZE).unwrap_or(0),
        read_u32(bytes, SDT_HEADER_SIZE + 4).unwrap_or(0),
    );
    parse_madt_entries(bytes)
}

fn parse_madt_entries(bytes: &[u8]) -> MadtSummary {
    let mut summary = MadtSummary::default();
    if bytes.len() < MADT_HEADER_SIZE {
        summary.malformed_entries = 1;
        return summary;
    }

    let mut offset = MADT_HEADER_SIZE;
    while offset < bytes.len() {
        let Some(header) = bytes.get(offset..offset.saturating_add(2)) else {
            summary.malformed_entries += 1;
            return summary;
        };
        let entry_type = header[0];
        let entry_len = usize::from(header[1]);
        let Some(end) = offset.checked_add(entry_len) else {
            summary.malformed_entries += 1;
            return summary;
        };
        if entry_len < 2 || end > bytes.len() {
            printk!(
                "[kernel-start][acpi] malformed MADT entry type={} offset={} len={} remaining={}",
                entry_type,
                offset,
                entry_len,
                bytes.len().saturating_sub(offset),
            );
            summary.malformed_entries += 1;
            return summary;
        }

        summary.entry_count += 1;
        inspect_madt_entry(entry_type, &bytes[offset..end], &mut summary);
        offset = end;
    }
    summary.complete = summary.malformed_entries == 0;
    summary
}

fn inspect_madt_entry(entry_type: u8, entry: &[u8], summary: &mut MadtSummary) {
    macro_rules! need {
        ($length:expr) => {
            if entry.len() < $length {
                printk!(
                    "[kernel-start][acpi] short MADT entry type={} len={} minimum={}",
                    entry_type,
                    entry.len(),
                    $length,
                );
                summary.malformed_entries += 1;
                return;
            }
        };
    }

    match entry_type {
        0 => {
            need!(8);
            let flags = read_u32(entry, 4).unwrap_or(0);
            summary.usable_processors += usize::from(madt_processor_usable(flags));
            printk!(
                "[kernel-start][acpi] MADT LAPIC processor={} apic={} flags={:#x}",
                entry[2],
                entry[3],
                flags,
            );
        }
        1 => {
            need!(12);
            summary.io_apics += 1;
            printk!(
                "[kernel-start][acpi] MADT IOAPIC id={} addr={:#x} gsi-base={}",
                entry[2],
                read_u32(entry, 4).unwrap_or(0),
                read_u32(entry, 8).unwrap_or(0),
            );
        }
        2 => {
            need!(10);
            summary.interrupt_overrides += 1;
            printk!(
                "[kernel-start][acpi] MADT ISO bus={} source={} gsi={} flags={:#x}",
                entry[2],
                entry[3],
                read_u32(entry, 4).unwrap_or(0),
                read_u16(entry, 8).unwrap_or(0),
            );
        }
        3 => {
            need!(8);
            summary.nmi_entries += 1;
            printk!(
                "[kernel-start][acpi] MADT NMI-source gsi={} flags={:#x}",
                read_u32(entry, 4).unwrap_or(0),
                read_u16(entry, 2).unwrap_or(0),
            );
        }
        4 => {
            need!(6);
            summary.nmi_entries += 1;
            printk!(
                "[kernel-start][acpi] MADT LAPIC-NMI processor={} LINT={} flags={:#x}",
                entry[2],
                entry[5],
                read_u16(entry, 3).unwrap_or(0),
            );
        }
        5 => {
            need!(12);
            printk!(
                "[kernel-start][acpi] MADT LAPIC-address-override addr={:#x}",
                read_u64(entry, 4).unwrap_or(0),
            );
        }
        6 => {
            need!(16);
            printk!(
                "[kernel-start][acpi] MADT IOSAPIC id={} addr={:#x} gsi-base={}",
                entry[2],
                read_u64(entry, 8).unwrap_or(0),
                read_u32(entry, 4).unwrap_or(0),
            );
        }
        7 => {
            need!(16);
            let flags = read_u32(entry, 8).unwrap_or(0);
            summary.usable_processors += usize::from(madt_processor_usable(flags));
            printk!(
                "[kernel-start][acpi] MADT local-SAPIC processor={} id={} eid={} uid={} flags={:#x}",
                entry[2],
                entry[3],
                entry[4],
                read_u32(entry, 12).unwrap_or(0),
                flags,
            );
        }
        8 => {
            need!(16);
            printk!(
                "[kernel-start][acpi] MADT platform-interrupt type={} processor={}:{} vector={} gsi={} flags={:#x}/{:#x}",
                entry[4],
                entry[5],
                entry[6],
                entry[7],
                read_u32(entry, 8).unwrap_or(0),
                read_u16(entry, 2).unwrap_or(0),
                read_u32(entry, 12).unwrap_or(0),
            );
        }
        9 => {
            need!(16);
            let flags = read_u32(entry, 8).unwrap_or(0);
            summary.usable_processors += usize::from(madt_processor_usable(flags));
            printk!(
                "[kernel-start][acpi] MADT x2APIC id={} uid={} flags={:#x}",
                read_u32(entry, 4).unwrap_or(0),
                read_u32(entry, 12).unwrap_or(0),
                flags,
            );
        }
        10 => {
            need!(12);
            summary.nmi_entries += 1;
            printk!(
                "[kernel-start][acpi] MADT x2APIC-NMI uid={} LINT={} flags={:#x}",
                read_u32(entry, 4).unwrap_or(0),
                entry[8],
                read_u16(entry, 2).unwrap_or(0),
            );
        }
        11 => {
            // The original GICC ends at MPIDR (76 bytes). Later revisions append
            // efficiency class, SPE, TRBE, IAff and IRS fields.
            need!(76);
            let flags = read_u32(entry, 12).unwrap_or(0);
            summary.usable_processors +=
                usize::from(flags & (MADT_PROCESSOR_ENABLED | MADT_GICC_ONLINE_CAPABLE) != 0);
            summary.gic_components += 1;
            printk!(
                "[kernel-start][acpi] MADT GICC cpu-if={} uid={} flags={:#x} GIC={:#x} GICV={:#x} GICH={:#x} GICR={:#x} MPIDR={:#x}",
                read_u32(entry, 4).unwrap_or(0),
                read_u32(entry, 8).unwrap_or(0),
                flags,
                read_u64(entry, 32).unwrap_or(0),
                read_u64(entry, 40).unwrap_or(0),
                read_u64(entry, 48).unwrap_or(0),
                read_u64(entry, 60).unwrap_or(0),
                read_u64(entry, 68).unwrap_or(0),
            );
            printk!(
                "[kernel-start][acpi] MADT GICC parking={} perf-GSIV={} parked={:#x} VGIC={} efficiency={} SPE={} TRBE={} IAff={} IRS={}",
                read_u32(entry, 16).unwrap_or(0),
                read_u32(entry, 20).unwrap_or(0),
                read_u64(entry, 24).unwrap_or(0),
                read_u32(entry, 56).unwrap_or(0),
                entry.get(76).copied().unwrap_or(0),
                read_u16(entry, 78).unwrap_or(0),
                read_u16(entry, 80).unwrap_or(0),
                read_u16(entry, 82).unwrap_or(0),
                read_u32(entry, 84).unwrap_or(0),
            );
        }
        12 => {
            need!(24);
            summary.gic_components += 1;
            printk!(
                "[kernel-start][acpi] MADT GICD id={} addr={:#x} gsi-base={} version={}",
                read_u32(entry, 4).unwrap_or(0),
                read_u64(entry, 8).unwrap_or(0),
                read_u32(entry, 16).unwrap_or(0),
                entry[20],
            );
        }
        13 => {
            need!(24);
            summary.gic_components += 1;
            printk!(
                "[kernel-start][acpi] MADT GIC-MSI frame={} addr={:#x} SPI-base={} count={} flags={:#x}",
                read_u32(entry, 4).unwrap_or(0),
                read_u64(entry, 8).unwrap_or(0),
                read_u16(entry, 22).unwrap_or(0),
                read_u16(entry, 20).unwrap_or(0),
                read_u32(entry, 16).unwrap_or(0),
            );
        }
        14 => {
            need!(16);
            summary.gic_components += 1;
            printk!(
                "[kernel-start][acpi] MADT GICR addr={:#x} len={:#x} flags={:#x}",
                read_u64(entry, 4).unwrap_or(0),
                read_u32(entry, 12).unwrap_or(0),
                entry[2],
            );
        }
        15 => {
            need!(20);
            summary.gic_components += 1;
            printk!(
                "[kernel-start][acpi] MADT GIC-ITS id={} addr={:#x} flags={:#x}",
                read_u32(entry, 4).unwrap_or(0),
                read_u64(entry, 8).unwrap_or(0),
                entry[2],
            );
        }
        16 => {
            need!(16);
            let version = read_u16(entry, 2).unwrap_or(0);
            match version {
                0 if entry.len() == 16 => {
                    printk!(
                        "[kernel-start][acpi] MADT MP-wakeup v0 mailbox={:#x}",
                        read_u64(entry, 8).unwrap_or(0),
                    );
                }
                1 if entry.len() == 24 => {
                    printk!(
                        "[kernel-start][acpi] MADT MP-wakeup v1 mailbox={:#x} reset-vector={:#x}",
                        read_u64(entry, 8).unwrap_or(0),
                        read_u64(entry, 16).unwrap_or(0),
                    );
                }
                0 | 1 => {
                    printk!(
                        "[kernel-start][acpi] MADT MP-wakeup version={} has invalid length={} expected={}",
                        version,
                        entry.len(),
                        if version == 0 { 16 } else { 24 },
                    );
                    summary.malformed_entries += 1;
                }
                _ => {
                    printk!(
                        "[kernel-start][acpi] MADT MP-wakeup has reserved version={} len={}",
                        version,
                        entry.len(),
                    );
                    summary.malformed_entries += 1;
                }
            }
        }
        17 => {
            need!(15);
            let flags = read_u32(entry, 11).unwrap_or(0);
            let physical_id = read_u32(entry, 7).unwrap_or(u32::MAX);
            if entry.len() != 15 || entry[2] != 1 || flags & !1 != 0 {
                summary.malformed_entries += 1;
                return;
            }
            if physical_id != u32::MAX && flags & MADT_PROCESSOR_ENABLED != 0 {
                summary.usable_processors += 1;
            }
            summary.loongarch_components += 1;
            printk!(
                "[kernel-start][acpi] MADT Core-PIC version={} processor={} core={} flags={:#x}",
                entry[2],
                read_u32(entry, 3).unwrap_or(0),
                read_u32(entry, 7).unwrap_or(0),
                flags,
            );
        }
        18 => {
            need!(23);
            summary.loongarch_components += 1;
            printk!(
                "[kernel-start][acpi] MADT LIO-PIC version={} addr={:#x} size={:#x} cascade={:?} map={:#x}/{:#x}",
                entry[2],
                read_u64(entry, 3).unwrap_or(0),
                read_u16(entry, 11).unwrap_or(0),
                &entry[13..15],
                read_u32(entry, 15).unwrap_or(0),
                read_u32(entry, 19).unwrap_or(0),
            );
        }
        19 => {
            need!(21);
            summary.loongarch_components += 1;
            printk!(
                "[kernel-start][acpi] MADT HT-PIC version={} addr={:#x} size={:#x}",
                entry[2],
                read_u64(entry, 3).unwrap_or(0),
                read_u16(entry, 11).unwrap_or(0),
            );
        }
        20 => {
            need!(13);
            summary.loongarch_components += 1;
            printk!(
                "[kernel-start][acpi] MADT EIO-PIC version={} cascade={} node={} map={:#x}",
                entry[2],
                entry[3],
                entry[4],
                read_u64(entry, 5).unwrap_or(0),
            );
        }
        21 => {
            need!(19);
            summary.loongarch_components += 1;
            printk!(
                "[kernel-start][acpi] MADT MSI-PIC version={} msg={:#x} start={} count={}",
                entry[2],
                read_u64(entry, 3).unwrap_or(0),
                read_u32(entry, 11).unwrap_or(0),
                read_u32(entry, 15).unwrap_or(0),
            );
        }
        22 => {
            need!(17);
            summary.loongarch_components += 1;
            printk!(
                "[kernel-start][acpi] MADT BIO-PIC version={} addr={:#x} size={:#x} id={} gsi-base={}",
                entry[2],
                read_u64(entry, 3).unwrap_or(0),
                read_u16(entry, 11).unwrap_or(0),
                read_u16(entry, 13).unwrap_or(0),
                read_u16(entry, 15).unwrap_or(0),
            );
        }
        23 => {
            need!(14);
            summary.loongarch_components += 1;
            printk!(
                "[kernel-start][acpi] MADT LPC-PIC version={} addr={:#x} size={:#x} cascade={}",
                entry[2],
                read_u64(entry, 3).unwrap_or(0),
                read_u16(entry, 11).unwrap_or(0),
                entry[13],
            );
        }
        24 => {
            need!(36);
            let flags = read_u32(entry, 4).unwrap_or(0);
            summary.usable_processors += usize::from(madt_processor_usable(flags));
            summary.riscv_components += 1;
            printk!(
                "[kernel-start][acpi] MADT RINTC version={} hart={} uid={} ext-intc={} IMSIC={:#x}/{:#x} flags={:#x}",
                entry[2],
                read_u64(entry, 8).unwrap_or(0),
                read_u32(entry, 16).unwrap_or(0),
                read_u32(entry, 20).unwrap_or(0),
                read_u64(entry, 24).unwrap_or(0),
                read_u32(entry, 32).unwrap_or(0),
                flags,
            );
        }
        25 => {
            need!(16);
            summary.riscv_components += 1;
            printk!(
                "[kernel-start][acpi] MADT IMSIC version={} ids={} guest-ids={} index-bits={}/{}/{} shift={} flags={:#x}",
                entry[2],
                read_u16(entry, 8).unwrap_or(0),
                read_u16(entry, 10).unwrap_or(0),
                entry[12],
                entry[13],
                entry[14],
                entry[15],
                read_u32(entry, 4).unwrap_or(0),
            );
        }
        26 => {
            need!(36);
            summary.riscv_components += 1;
            printk!(
                "[kernel-start][acpi] MADT APLIC version={} id={} IDCs={} sources={} gsi-base={} addr={:#x}/{:#x} flags={:#x}",
                entry[2],
                entry[3],
                read_u16(entry, 16).unwrap_or(0),
                read_u16(entry, 18).unwrap_or(0),
                read_u32(entry, 20).unwrap_or(0),
                read_u64(entry, 24).unwrap_or(0),
                read_u32(entry, 32).unwrap_or(0),
                read_u32(entry, 4).unwrap_or(0),
            );
        }
        27 => {
            need!(36);
            summary.riscv_components += 1;
            printk!(
                "[kernel-start][acpi] MADT PLIC version={} id={} IRQs={} priority={} addr={:#x}/{:#x} gsi-base={} flags={:#x}",
                entry[2],
                entry[3],
                read_u16(entry, 12).unwrap_or(0),
                read_u16(entry, 14).unwrap_or(0),
                read_u64(entry, 24).unwrap_or(0),
                read_u32(entry, 20).unwrap_or(0),
                read_u32(entry, 32).unwrap_or(0),
                read_u32(entry, 16).unwrap_or(0),
            );
        }
        28 => {
            need!(32);
            summary.gic_components += 1;
            printk!(
                "[kernel-start][acpi] MADT GICv5 IRS version={} id={} flags={:#x} config={:#x} SETLPI={:#x}",
                entry[2],
                read_u32(entry, 4).unwrap_or(0),
                read_u32(entry, 8).unwrap_or(0),
                read_u64(entry, 16).unwrap_or(0),
                read_u64(entry, 24).unwrap_or(0),
            );
        }
        29 => {
            need!(16);
            summary.gic_components += 1;
            printk!(
                "[kernel-start][acpi] MADT GICv5 ITS id={} addr={:#x} flags={:#x}",
                read_u32(entry, 4).unwrap_or(0),
                read_u64(entry, 8).unwrap_or(0),
                entry[2],
            );
        }
        30 => {
            need!(24);
            summary.gic_components += 1;
            printk!(
                "[kernel-start][acpi] MADT GICv5 translate linked-ITS={} frame={} addr={:#x}",
                read_u32(entry, 4).unwrap_or(0),
                read_u32(entry, 8).unwrap_or(0),
                read_u64(entry, 16).unwrap_or(0),
            );
        }
        _ => {
            summary.unknown_entries += 1;
            printk!(
                "[kernel-start][acpi] MADT unknown entry type={} len={} skipped",
                entry_type,
                entry.len(),
            );
        }
    }
}

#[inline]
const fn madt_processor_usable(flags: u32) -> bool {
    flags & (MADT_PROCESSOR_ENABLED | MADT_PROCESSOR_ONLINE_CAPABLE) != 0
}

fn inspect_mcfg(tables: &AcpiTables<AcpiMapper>) -> McfgSummary {
    let mapping = match tables.find_table::<Mcfg>() {
        Ok(mapping) => mapping,
        Err(err) => {
            log::debug!("[kernel-start][acpi] MCFG unavailable: {:?}", err);
            return McfgSummary::default();
        }
    };
    parse_mcfg_entries(mapping_bytes(&mapping))
}

fn parse_mcfg_entries(bytes: &[u8]) -> McfgSummary {
    let mut summary = McfgSummary::default();
    if bytes.len() < MCFG_HEADER_SIZE {
        summary.malformed_entries = 1;
        return summary;
    }
    if read_u64(bytes, SDT_HEADER_SIZE) != Some(0) {
        summary.malformed_entries += 1;
        printk!("[kernel-start][acpi] MCFG header reserved field is non-zero");
    }
    let payload = &bytes[MCFG_HEADER_SIZE..];
    if payload.len() % MCFG_ENTRY_SIZE != 0 {
        summary.malformed_entries += 1;
        printk!(
            "[kernel-start][acpi] MCFG payload has {} trailing byte(s)",
            payload.len() % MCFG_ENTRY_SIZE
        );
    }

    for entry in payload.chunks_exact(MCFG_ENTRY_SIZE) {
        summary.entry_count += 1;
        let base = read_u64(entry, 0).unwrap_or(0);
        let segment = read_u16(entry, 8).unwrap_or(0);
        let bus_start = entry[10];
        let bus_end = entry[11];
        let reserved = read_u32(entry, 12).unwrap_or(u32::MAX);
        let buses = usize::from(bus_end).checked_sub(usize::from(bus_start));
        let size = buses
            .and_then(|count| count.checked_add(1))
            .and_then(|count| count.checked_shl(20));
        let valid = base != 0 && base & ((1 << 20) - 1) == 0 && size.is_some() && reserved == 0;
        if valid {
            summary.valid_entries += 1;
        } else {
            summary.malformed_entries += 1;
        }
        printk!(
            "[kernel-start][acpi] MCFG segment={} buses={:#x}..={:#x} ECAM={:#x} size={:#x} valid={}",
            segment,
            bus_start,
            bus_end,
            base,
            size.unwrap_or(0),
            valid as usize,
        );
    }
    summary.complete = summary.malformed_entries == 0;
    summary
}

fn inspect_hpet(tables: &AcpiTables<AcpiMapper>) -> bool {
    let mapping = match tables.find_table::<RawHpetTable>() {
        Ok(mapping) => mapping,
        Err(err) => {
            log::debug!("[kernel-start][acpi] HPET unavailable: {:?}", err);
            return false;
        }
    };
    let bytes = mapping_bytes(&mapping);
    if bytes.len() < 56 {
        printk!(
            "[kernel-start][acpi] HPET is too short: len={}",
            bytes.len()
        );
        return false;
    }
    let id = read_u32(bytes, 36).unwrap_or(0);
    let Some(address) = parse_gas(bytes, 40) else {
        printk!("[kernel-start][acpi] HPET has malformed address");
        return false;
    };
    let minimum_tick = read_u16(bytes, 53).unwrap_or(0);
    let page = bytes[55] & 0x0f;
    printk!(
        "[kernel-start][acpi] HPET rev={} comparators={} counter64={} legacy={} vendor={:#x} space={} addr={:#x} number={} min-tick={} page={}",
        id & 0xff,
        ((id >> 8) & 0x1f) + 1,
        ((id >> 13) & 1),
        ((id >> 15) & 1),
        id >> 16,
        address.address_space,
        address.address,
        bytes[52],
        minimum_tick,
        page,
    );
    if address.address_space != 0 || address.address == 0 {
        printk!("[kernel-start][acpi] HPET address is not valid system memory");
        return false;
    }
    true
}

fn inspect_bgrt(tables: &AcpiTables<AcpiMapper>) -> bool {
    let mapping = match tables.find_table::<Bgrt>() {
        Ok(mapping) => mapping,
        Err(err) => {
            log::debug!("[kernel-start][acpi] BGRT unavailable: {:?}", err);
            return false;
        }
    };
    if mapping.region_length() < mem::size_of::<Bgrt>() {
        printk!(
            "[kernel-start][acpi] BGRT is too short: len={}",
            mapping.region_length()
        );
        return false;
    }
    let bgrt = &*mapping;
    let version = bgrt.version;
    let image_address = bgrt.image_address;
    let (offset_x, offset_y) = bgrt.image_offset();
    printk!(
        "[kernel-start][acpi] BGRT version={} displayed={} type={:?} image={:#x} offset=({}, {}) orientation={}",
        version,
        bgrt.was_displayed() as usize,
        bgrt.image_type(),
        image_address,
        offset_x,
        offset_y,
        bgrt.orientation_offset(),
    );
    true
}

fn inspect_spcr(tables: &AcpiTables<AcpiMapper>) -> bool {
    let raw_mapping = match tables.find_table::<RawSpcrTable>() {
        Ok(mapping) => mapping,
        Err(err) => {
            log::debug!("[kernel-start][acpi] SPCR unavailable: {:?}", err);
            return false;
        }
    };
    let bytes = mapping_bytes(&raw_mapping);
    // Older valid table revisions can end after UART clock frequency. Newer
    // revisions append the fields represented by acpi 5.2's complete `Spcr`.
    if bytes.len() < 80 {
        printk!(
            "[kernel-start][acpi] SPCR is too short: len={} minimum=80",
            bytes.len(),
        );
        return false;
    }
    let Some(base) = parse_gas(bytes, 40) else {
        printk!("[kernel-start][acpi] SPCR has malformed base address");
        return false;
    };
    printk!(
        "[kernel-start][acpi] SPCR interface={} base=(space={} addr={:#x} width={} offset={} access={}) interrupt={:#x} IRQ={} GSI={} baud-code={} clock={}Hz precise={}bps",
        bytes[36],
        base.address_space,
        base.address,
        base.bit_width,
        base.bit_offset,
        base.access_size,
        bytes[52],
        bytes[53],
        read_u32(bytes, 54).unwrap_or(0),
        bytes[58],
        read_u32(bytes, 76).unwrap_or(0),
        read_u32(bytes, 80).unwrap_or(0),
    );
    printk!(
        "[kernel-start][acpi] SPCR parity={} stop={} flow={:#x} terminal={:#x} PCI={:#06x}:{:#06x}:{:02x}:{:02x}.{} segment={} flags={:#x} language={}",
        bytes[59],
        bytes[60],
        bytes[61],
        bytes[62],
        read_u16(bytes, 66).unwrap_or(0),
        read_u16(bytes, 64).unwrap_or(0),
        bytes[68],
        bytes[69],
        bytes[70],
        bytes[75],
        read_u32(bytes, 71).unwrap_or(0),
        bytes[63],
    );

    if let Some(namespace) = bounded_spcr_namespace(bytes) {
        printk!("[kernel-start][acpi] SPCR namespace={}", namespace);
    } else if bytes.len() >= 88 {
        printk!("[kernel-start][acpi] SPCR namespace is absent or malformed");
    }
    true
}

fn bounded_spcr_namespace(bytes: &[u8]) -> Option<&str> {
    let length = usize::from(read_u16(bytes, 84)?);
    let offset = usize::from(read_u16(bytes, 86)?);
    if length == 0 || offset < 88 {
        return None;
    }
    let value = bytes.get(offset..offset.checked_add(length)?)?;
    str::from_utf8(value)
        .ok()
        .map(|value| value.trim_end_matches('\0'))
}

fn inspect_srat(tables: &AcpiTables<AcpiMapper>) -> SratSummary {
    let mapping = match tables.find_table::<RawSratTable>() {
        Ok(mapping) => mapping,
        Err(err) => {
            log::debug!("[kernel-start][acpi] SRAT unavailable: {:?}", err);
            return SratSummary::default();
        }
    };
    let bytes = mapping_bytes(&mapping);
    let table_revision = read_u32(bytes, SDT_HEADER_SIZE).unwrap_or(0);
    let reserved = read_u64(bytes, SDT_HEADER_SIZE + 4).unwrap_or(u64::MAX);
    printk!(
        "[kernel-start][acpi] SRAT table-revision={} reserved={:#x}",
        table_revision,
        reserved,
    );
    parse_srat_entries(bytes)
}

fn parse_srat_entries(bytes: &[u8]) -> SratSummary {
    let mut summary = SratSummary::default();
    if bytes.len() < SRAT_HEADER_SIZE {
        summary.malformed_entries = 1;
        return summary;
    }
    if read_u32(bytes, SDT_HEADER_SIZE) != Some(1)
        || read_u64(bytes, SDT_HEADER_SIZE + 4) != Some(0)
    {
        summary.malformed_entries += 1;
        printk!("[kernel-start][acpi] SRAT header fields violate the ACPI-defined values");
    }
    let mut offset = SRAT_HEADER_SIZE;
    while offset < bytes.len() {
        let Some(header) = bytes.get(offset..offset.saturating_add(2)) else {
            summary.malformed_entries += 1;
            return summary;
        };
        let entry_type = header[0];
        let entry_len = usize::from(header[1]);
        let Some(end) = offset.checked_add(entry_len) else {
            summary.malformed_entries += 1;
            return summary;
        };
        if entry_len < 2 || end > bytes.len() {
            printk!(
                "[kernel-start][acpi] malformed SRAT entry type={} offset={} len={} remaining={}",
                entry_type,
                offset,
                entry_len,
                bytes.len().saturating_sub(offset),
            );
            summary.malformed_entries += 1;
            return summary;
        }
        summary.entry_count += 1;
        inspect_srat_entry(entry_type, &bytes[offset..end], &mut summary);
        offset = end;
    }
    summary.complete = summary.malformed_entries == 0;
    summary
}

fn inspect_srat_entry(entry_type: u8, entry: &[u8], summary: &mut SratSummary) {
    macro_rules! need {
        ($length:expr) => {
            if entry.len() < $length {
                printk!(
                    "[kernel-start][acpi] short SRAT entry type={} len={} minimum={}",
                    entry_type,
                    entry.len(),
                    $length,
                );
                summary.malformed_entries += 1;
                return;
            }
        };
    }

    match entry_type {
        0 => {
            need!(16);
            let domain = u32::from(entry[2])
                | (u32::from(entry[9]) << 8)
                | (u32::from(entry[10]) << 16)
                | (u32::from(entry[11]) << 24);
            let flags = read_u32(entry, 4).unwrap_or(0);
            summary.processor_affinities += 1;
            summary.enabled_processors += usize::from(flags & 1 != 0);
            printk!(
                "[kernel-start][acpi] SRAT LAPIC-affinity domain={} apic={} sapic-eid={} clock={} flags={:#x}",
                domain,
                entry[3],
                entry[8],
                read_u32(entry, 12).unwrap_or(0),
                flags,
            );
        }
        1 => {
            need!(40);
            let flags = read_u32(entry, 28).unwrap_or(0);
            let base = read_u64(entry, 8).unwrap_or(0);
            let length = read_u64(entry, 16).unwrap_or(0);
            let end = base.checked_add(length);
            summary.memory_affinities += 1;
            summary.enabled_memory_affinities += usize::from(flags & 1 != 0);
            if length == 0 || end.is_none() {
                summary.malformed_entries += 1;
            }
            printk!(
                "[kernel-start][acpi] SRAT memory domain={} range={:#x}..{:#x} flags={:#x} enabled={} hotplug={} nonvolatile={} specific={}",
                read_u32(entry, 2).unwrap_or(0),
                base,
                end.unwrap_or(0),
                flags,
                (flags & 1 != 0) as usize,
                (flags & 2 != 0) as usize,
                (flags & 4 != 0) as usize,
                (flags & 8 != 0) as usize,
            );
        }
        2 => {
            need!(24);
            let flags = read_u32(entry, 12).unwrap_or(0);
            summary.processor_affinities += 1;
            summary.enabled_processors += usize::from(flags & 1 != 0);
            printk!(
                "[kernel-start][acpi] SRAT x2APIC-affinity domain={} apic={} clock={} flags={:#x}",
                read_u32(entry, 4).unwrap_or(0),
                read_u32(entry, 8).unwrap_or(0),
                read_u32(entry, 16).unwrap_or(0),
                flags,
            );
        }
        3 => {
            need!(18);
            let flags = read_u32(entry, 10).unwrap_or(0);
            summary.processor_affinities += 1;
            summary.enabled_processors += usize::from(flags & 1 != 0);
            printk!(
                "[kernel-start][acpi] SRAT GICC-affinity domain={} uid={} clock={} flags={:#x}",
                read_u32(entry, 2).unwrap_or(0),
                read_u32(entry, 6).unwrap_or(0),
                read_u32(entry, 14).unwrap_or(0),
                flags,
            );
        }
        4 => {
            need!(12);
            summary.initiator_affinities += 1;
            printk!(
                "[kernel-start][acpi] SRAT GIC-ITS-affinity domain={} ITS={}",
                read_u32(entry, 2).unwrap_or(0),
                read_u32(entry, 8).unwrap_or(0),
            );
        }
        5 | 6 => {
            need!(32);
            let flags = read_u32(entry, 24).unwrap_or(0);
            summary.initiator_affinities += 1;
            printk!(
                "[kernel-start][acpi] SRAT generic-affinity type={} handle-type={} domain={} flags={:#x} enabled={} architectural={}",
                entry_type,
                entry[3],
                read_u32(entry, 4).unwrap_or(0),
                flags,
                (flags & 1 != 0) as usize,
                (flags & 2 != 0) as usize,
            );
        }
        7 => {
            need!(20);
            let flags = read_u32(entry, 12).unwrap_or(0);
            summary.processor_affinities += 1;
            summary.enabled_processors += usize::from(flags & 1 != 0);
            printk!(
                "[kernel-start][acpi] SRAT RINTC-affinity domain={} uid={} clock={} flags={:#x}",
                read_u32(entry, 4).unwrap_or(0),
                read_u32(entry, 8).unwrap_or(0),
                read_u32(entry, 16).unwrap_or(0),
                flags,
            );
        }
        _ => {
            summary.unknown_entries += 1;
            printk!(
                "[kernel-start][acpi] SRAT unknown entry type={} len={} skipped",
                entry_type,
                entry.len(),
            );
        }
    }
}

fn inspect_slit(tables: &AcpiTables<AcpiMapper>) -> SlitSummary {
    let mapping = match tables.find_table::<RawSlitTable>() {
        Ok(mapping) => mapping,
        Err(err) => {
            log::debug!("[kernel-start][acpi] SLIT unavailable: {:?}", err);
            return SlitSummary::default();
        }
    };
    parse_slit(mapping_bytes(&mapping))
}

fn parse_slit(bytes: &[u8]) -> SlitSummary {
    let mut summary = SlitSummary::default();
    if bytes.len() < SLIT_HEADER_SIZE {
        return summary;
    }
    let Some(locality_count) =
        read_u64(bytes, SDT_HEADER_SIZE).and_then(|count| usize::try_from(count).ok())
    else {
        return summary;
    };
    let Some(distance_count) = locality_count.checked_mul(locality_count) else {
        return summary;
    };
    let Some(end) = SLIT_HEADER_SIZE.checked_add(distance_count) else {
        return summary;
    };
    let Some(distances) = bytes.get(SLIT_HEADER_SIZE..end) else {
        printk!(
            "[kernel-start][acpi] SLIT matrix truncated: localities={} cells={} available={}",
            locality_count,
            distance_count,
            bytes.len().saturating_sub(SLIT_HEADER_SIZE),
        );
        summary.locality_count = locality_count;
        summary.distance_count = distance_count;
        return summary;
    };
    if locality_count == 0 || end != bytes.len() {
        printk!(
            "[kernel-start][acpi] SLIT has invalid size: localities={} expected={} actual={}",
            locality_count,
            end,
            bytes.len(),
        );
        summary.locality_count = locality_count;
        summary.distance_count = distance_count;
        return summary;
    }

    summary.locality_count = locality_count;
    summary.distance_count = distance_count;
    summary.minimum_distance = u8::MAX;
    for from in 0..locality_count {
        for to in 0..locality_count {
            let index = from * locality_count + to;
            let distance = distances[index];
            summary.minimum_distance = summary.minimum_distance.min(distance);
            summary.maximum_distance = summary.maximum_distance.max(distance);
            if (from == to && distance != 10) || (from != to && distance < 10) {
                summary.invalid_distances += 1;
            }
            if to > from && distance != distances[to * locality_count + from] {
                summary.asymmetric_pairs += 1;
            }
            if distance_count <= MAX_LOGGED_SLIT_CELLS {
                printk!(
                    "[kernel-start][acpi] SLIT distance {} -> {} = {}",
                    from,
                    to,
                    distance,
                );
            }
        }
    }
    if distance_count > MAX_LOGGED_SLIT_CELLS {
        printk!(
            "[kernel-start][acpi] SLIT matrix has {} cells; per-cell log capped at {}",
            distance_count,
            MAX_LOGGED_SLIT_CELLS,
        );
    }
    summary.complete = summary.invalid_distances == 0;
    printk!(
        "[kernel-start][acpi] SLIT localities={} min={} max={} invalid={} asymmetric={}",
        summary.locality_count,
        summary.minimum_distance,
        summary.maximum_distance,
        summary.invalid_distances,
        summary.asymmetric_pairs,
    );
    summary
}

#[derive(Clone, Copy, Debug)]
struct GasSummary {
    address_space: u8,
    bit_width: u8,
    bit_offset: u8,
    access_size: u8,
    address: u64,
}

fn parse_gas(bytes: &[u8], offset: usize) -> Option<GasSummary> {
    let gas = bytes.get(offset..offset.checked_add(12)?)?;
    Some(GasSummary {
        address_space: gas[0],
        bit_width: gas[1],
        bit_offset: gas[2],
        access_size: gas[3],
        address: read_u64(gas, 4)?,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}

#[cfg(feature = "kernel-tests")]
mod tests {
    use alloc::vec;

    use ktest::ktest;

    use super::*;

    #[ktest]
    fn parses_madt_x2apic_and_skips_unknown_entry() {
        let mut bytes = vec![0u8; MADT_HEADER_SIZE];
        bytes.extend_from_slice(&[9, 16, 0, 0, 0x34, 0x12, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0]);
        bytes.extend_from_slice(&[0x7f, 2]);

        let summary = parse_madt_entries(&bytes);
        assert!(summary.complete);
        assert_eq!(summary.entry_count, 2);
        assert_eq!(summary.usable_processors, 1);
        assert_eq!(summary.unknown_entries, 1);
    }

    #[ktest]
    fn counts_architecture_specific_online_capable_processors() {
        let mut bytes = vec![0u8; MADT_HEADER_SIZE];
        let mut x2apic = [0u8; 16];
        x2apic[0] = 9;
        x2apic[1] = 16;
        x2apic[8..12].copy_from_slice(&MADT_PROCESSOR_ONLINE_CAPABLE.to_le_bytes());
        bytes.extend_from_slice(&x2apic);

        let mut gicc = [0u8; 76];
        gicc[0] = 11;
        gicc[1] = 76;
        gicc[12..16].copy_from_slice(&MADT_GICC_ONLINE_CAPABLE.to_le_bytes());
        bytes.extend_from_slice(&gicc);

        let summary = parse_madt_entries(&bytes);
        assert!(summary.complete);
        assert_eq!(summary.usable_processors, 2);
    }

    #[ktest]
    fn validates_madt_multiprocessor_wakeup_versions() {
        let mut bytes = vec![0u8; MADT_HEADER_SIZE];
        let mut version_zero = [0u8; 16];
        version_zero[0] = 16;
        version_zero[1] = 16;
        bytes.extend_from_slice(&version_zero);

        let mut version_one = [0u8; 24];
        version_one[0] = 16;
        version_one[1] = 24;
        version_one[2..4].copy_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&version_one);

        let summary = parse_madt_entries(&bytes);
        assert!(summary.complete);
        assert_eq!(summary.entry_count, 2);

        version_zero[2..4].copy_from_slice(&1u16.to_le_bytes());
        let mut invalid = vec![0u8; MADT_HEADER_SIZE];
        invalid.extend_from_slice(&version_zero);
        let summary = parse_madt_entries(&invalid);
        assert!(!summary.complete);
        assert_eq!(summary.malformed_entries, 1);
    }

    #[ktest]
    fn rejects_truncated_madt_entry() {
        let mut bytes = vec![0u8; MADT_HEADER_SIZE];
        bytes.extend_from_slice(&[1, 12, 0, 0]);

        let summary = parse_madt_entries(&bytes);
        assert!(!summary.complete);
        assert_eq!(summary.malformed_entries, 1);
    }

    #[ktest]
    fn rejects_short_but_bounded_madt_entry() {
        let mut bytes = vec![0u8; MADT_HEADER_SIZE];
        bytes.extend_from_slice(&[1, 4, 0, 0]);

        let summary = parse_madt_entries(&bytes);
        assert!(!summary.complete);
        assert_eq!(summary.entry_count, 1);
        assert_eq!(summary.malformed_entries, 1);
    }

    #[ktest]
    fn parses_srat_affinities_and_unknown_entry() {
        let mut bytes = vec![0u8; SRAT_HEADER_SIZE];
        bytes[SDT_HEADER_SIZE..SDT_HEADER_SIZE + 4].copy_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 16, 2, 9, 1, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0]);
        let mut memory = [0u8; 40];
        memory[0] = 1;
        memory[1] = 40;
        memory[2..6].copy_from_slice(&2u32.to_le_bytes());
        memory[8..16].copy_from_slice(&0x1000u64.to_le_bytes());
        memory[16..24].copy_from_slice(&0x2000u64.to_le_bytes());
        memory[28..32].copy_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&memory);
        bytes.extend_from_slice(&[0x7f, 2]);

        let summary = parse_srat_entries(&bytes);
        assert!(summary.complete);
        assert_eq!(summary.processor_affinities, 1);
        assert_eq!(summary.enabled_processors, 1);
        assert_eq!(summary.memory_affinities, 1);
        assert_eq!(summary.enabled_memory_affinities, 1);
        assert_eq!(summary.unknown_entries, 1);
    }

    #[ktest]
    fn rejects_truncated_srat_entry() {
        let mut bytes = vec![0u8; SRAT_HEADER_SIZE];
        bytes[SDT_HEADER_SIZE..SDT_HEADER_SIZE + 4].copy_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 40, 0, 0]);

        let summary = parse_srat_entries(&bytes);
        assert!(!summary.complete);
        assert_eq!(summary.malformed_entries, 1);
    }

    #[ktest]
    fn rejects_short_but_bounded_srat_entry() {
        let mut bytes = vec![0u8; SRAT_HEADER_SIZE];
        bytes[SDT_HEADER_SIZE..SDT_HEADER_SIZE + 4].copy_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 4, 0, 0]);

        let summary = parse_srat_entries(&bytes);
        assert!(!summary.complete);
        assert_eq!(summary.entry_count, 1);
        assert_eq!(summary.malformed_entries, 1);
    }

    #[ktest]
    fn parses_valid_slit_matrix() {
        let mut bytes = vec![0u8; SLIT_HEADER_SIZE];
        bytes[SDT_HEADER_SIZE..SLIT_HEADER_SIZE].copy_from_slice(&2u64.to_le_bytes());
        bytes.extend_from_slice(&[10, 20, 20, 10]);

        let summary = parse_slit(&bytes);
        assert!(summary.complete);
        assert_eq!(summary.locality_count, 2);
        assert_eq!(summary.distance_count, 4);
        assert_eq!(summary.minimum_distance, 10);
        assert_eq!(summary.maximum_distance, 20);
    }

    #[ktest]
    fn rejects_truncated_slit_matrix() {
        let mut bytes = vec![0u8; SLIT_HEADER_SIZE];
        bytes[SDT_HEADER_SIZE..SLIT_HEADER_SIZE].copy_from_slice(&3u64.to_le_bytes());
        bytes.extend_from_slice(&[10, 20, 20, 10]);

        let summary = parse_slit(&bytes);
        assert!(!summary.complete);
        assert_eq!(summary.locality_count, 3);
        assert_eq!(summary.distance_count, 9);
    }

    #[ktest]
    fn bounded_spcr_namespace_rejects_out_of_table_range() {
        let mut bytes = vec![0u8; 88];
        bytes[84..86].copy_from_slice(&8u16.to_le_bytes());
        bytes[86..88].copy_from_slice(&84u16.to_le_bytes());
        assert!(bounded_spcr_namespace(&bytes).is_none());
    }

    #[ktest]
    fn bounded_spcr_namespace_accepts_in_table_string() {
        let mut bytes = vec![0u8; 96];
        bytes[84..86].copy_from_slice(&8u16.to_le_bytes());
        bytes[86..88].copy_from_slice(&88u16.to_le_bytes());
        bytes[88..96].copy_from_slice(b"\\_SB.UA0");
        assert_eq!(bounded_spcr_namespace(&bytes), Some("\\_SB.UA0"));
    }

    #[ktest]
    fn validates_mcfg_entry_boundaries_and_reserved_fields() {
        let mut bytes = vec![0u8; MCFG_HEADER_SIZE];
        let mut valid = [0u8; MCFG_ENTRY_SIZE];
        valid[0..8].copy_from_slice(&0xe000_0000u64.to_le_bytes());
        valid[10] = 0;
        valid[11] = 0xff;
        bytes.extend_from_slice(&valid);

        let summary = parse_mcfg_entries(&bytes);
        assert!(summary.complete);
        assert_eq!(summary.entry_count, 1);
        assert_eq!(summary.valid_entries, 1);

        bytes[MCFG_HEADER_SIZE + 12] = 1;
        let summary = parse_mcfg_entries(&bytes);
        assert!(!summary.complete);
        assert_eq!(summary.malformed_entries, 1);
    }
}

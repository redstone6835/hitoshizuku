//! 类型化 ACPI 平台数据的解析与固件无关拓扑发布。

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::slice;

use allocator::MemorySegment;
use general::dev::cpu::{CpuNumaEntry, CpuTopologyEntry};
use general::dev::numa::{NumaDistance, NumaMemoryRange};
use general::firmware::FirmwareTableMapping;
use general::firmware::acpi::{
    AcpiMadtInfo, AcpiPlatformInfo, AcpiPpttInfo, AcpiPpttProcessor, AcpiProcessor,
    AcpiProcessorInterface, AcpiTableError,
};
use general::{StartMemoryRegion, StartMemoryRegionKind};
use log::printk;

pub(super) fn parse_and_publish(
    mappings: &'static [FirmwareTableMapping],
    boot_hardware_id: usize,
    usable_memory: &[MemorySegment],
    boot_memory: Option<&[StartMemoryRegion]>,
) -> AcpiPlatformInfo {
    let mut info = AcpiPlatformInfo::default();
    info.madt = parse_single(mappings, b"APIC", general::firmware::acpi::parse_madt);
    info.fadt = parse_single(mappings, b"FACP", general::firmware::acpi::parse_fadt);
    info.hpets = parse_hpet_tables(mappings);
    info.srat = parse_single(mappings, b"SRAT", general::firmware::acpi::parse_srat);
    info.slit = parse_single(mappings, b"SLIT", general::firmware::acpi::parse_slit);
    info.pptt = parse_single(mappings, b"PPTT", general::firmware::acpi::parse_pptt);
    info.pci_config_regions = parse_mcfg_tables(mappings);
    validate_cross_table_relations(&mut info, boot_memory);

    publish_cpu_topology(&info, boot_hardware_id);
    publish_numa_topology(&info, usable_memory);
    let _ = general::firmware::acpi::install_platform_info(info.clone());

    printk!(
        "[kernel-start][acpi] platform data published: processors={} ioapics={} mcfg={} hpet={} numa-memory={} pptt={}",
        info.madt.as_ref().map_or(0, |madt| madt.processors.len()),
        info.madt.as_ref().map_or(0, |madt| madt.io_apics.len()),
        info.pci_config_regions.len(),
        info.hpets.len(),
        info.srat
            .as_ref()
            .map_or(0, |srat| srat.memory_affinities.len()),
        info.pptt.as_ref().map_or(0, |pptt| pptt.processors.len()),
    );
    info
}

fn parse_hpet_tables(
    mappings: &'static [FirmwareTableMapping],
) -> Vec<general::firmware::acpi::AcpiHpetInfo> {
    let mut hpets = Vec::new();
    for bytes in mapped_tables(mappings, b"HPET") {
        match general::firmware::acpi::parse_hpet(bytes) {
            Ok(hpet)
                if hpets
                    .iter()
                    .any(|existing: &general::firmware::acpi::AcpiHpetInfo| {
                        existing.sequence == hpet.sequence
                    }) =>
            {
                printk!(
                    "[kernel-start][acpi] rejected duplicate HPET sequence {}",
                    hpet.sequence
                );
                return Vec::new();
            }
            Ok(hpet) => hpets.push(hpet),
            Err(error) => {
                printk!("[kernel-start][acpi] rejected HPET table: {:?}", error);
                return Vec::new();
            }
        }
    }
    hpets.sort_by_key(|hpet| hpet.sequence);
    if hpets.len() > 8 {
        printk!("[kernel-start][acpi] rejected more than eight HPET blocks");
        return Vec::new();
    }
    if hpets
        .iter()
        .enumerate()
        .any(|(sequence, hpet)| usize::from(hpet.sequence) != sequence)
    {
        printk!("[kernel-start][acpi] rejected non-contiguous HPET sequence numbers");
        return Vec::new();
    }
    if hpets.iter().enumerate().any(|(index, hpet)| {
        let Some(end) = hpet.base.address.checked_add(0x400) else {
            return true;
        };
        hpets[..index].iter().any(|other| {
            let other_end = other.base.address + 0x400;
            hpet.base.address < other_end && other.base.address < end
        })
    }) {
        printk!("[kernel-start][acpi] rejected overlapping HPET register blocks");
        return Vec::new();
    }
    hpets
}

fn parse_single<T>(
    mappings: &'static [FirmwareTableMapping],
    signature: &'static [u8; 4],
    parser: fn(&[u8]) -> Result<T, AcpiTableError>,
) -> Option<T> {
    let mut parsed = None;
    for bytes in mapped_tables(mappings, signature) {
        if parsed.is_some() {
            printk!(
                "[kernel-start][acpi] duplicate {} table ignored",
                core::str::from_utf8(signature).unwrap_or("????")
            );
            continue;
        }
        match parser(bytes) {
            Ok(value) => parsed = Some(value),
            Err(error) => printk!(
                "[kernel-start][acpi] rejected {} table: {:?}",
                core::str::from_utf8(signature).unwrap_or("????"),
                error
            ),
        }
    }
    parsed
}

fn parse_mcfg_tables(
    mappings: &'static [FirmwareTableMapping],
) -> Vec<general::firmware::acpi::AcpiPciConfigRegion> {
    let mut regions = Vec::new();
    for bytes in mapped_tables(mappings, b"MCFG") {
        match general::firmware::acpi::parse_mcfg(bytes) {
            Ok(entries) => {
                for entry in entries {
                    let overlaps = regions.iter().any(
                        |existing: &general::firmware::acpi::AcpiPciConfigRegion| {
                            existing.segment == entry.segment
                                && entry.bus_start <= existing.bus_end
                                && existing.bus_start <= entry.bus_end
                        },
                    );
                    if overlaps {
                        printk!(
                            "[kernel-start][acpi] rejected overlapping MCFG segment={} buses={}..={}",
                            entry.segment,
                            entry.bus_start,
                            entry.bus_end
                        );
                    } else {
                        regions.push(entry);
                    }
                }
            }
            Err(error) => printk!("[kernel-start][acpi] rejected MCFG table: {:?}", error),
        }
    }
    regions
}

fn mapped_tables(
    mappings: &'static [FirmwareTableMapping],
    signature: &'static [u8; 4],
) -> impl Iterator<Item = &'static [u8]> {
    mappings.iter().filter_map(move |mapping| {
        if mapping.virtual_start == 0 || mapping.length < 36 {
            return None;
        }
        // SAFETY: StartAcpiTables guarantees immutable, kernel-lifetime snapshots. Bounds are
        // taken from the loader-provided mapping rather than firmware-controlled pointers.
        let bytes =
            unsafe { slice::from_raw_parts(mapping.virtual_start as *const u8, mapping.length) };
        (bytes.get(..4) == Some(signature.as_slice())).then_some(bytes)
    })
}

fn publish_cpu_topology(info: &AcpiPlatformInfo, boot_hardware_id: usize) {
    let Some(madt) = info.madt.as_ref() else {
        general::dev::cpu::install_topology(Vec::new());
        general::dev::cpu::install_numa_topology(Vec::new());
        return;
    };
    let mut processors: Vec<&AcpiProcessor> = madt
        .processors
        .iter()
        .filter(|processor| processor.usable())
        .collect();
    processors
        .sort_by_key(|processor| usize::from(processor.hardware_id != boot_hardware_id as u64));
    let topology = processors
        .iter()
        .enumerate()
        .map(|(logical_id, processor)| {
            let pptt = info
                .pptt
                .as_ref()
                .and_then(|pptt| pptt.processor_for_uid(processor.processor_uid));
            let hierarchy = pptt_hierarchy(info.pptt.as_ref(), pptt);
            let (socket_id, cluster_path, core_id, thread_id) = pptt_topology_ids(pptt, &hierarchy);
            CpuTopologyEntry {
                logical_id: logical_id as u32,
                reg: processor.hardware_id,
                phandle: None,
                interrupt_controller_phandles: Box::new([]),
                compatible: Vec::new(),
                socket_id,
                cluster_path: cluster_path.into_boxed_slice(),
                core_id,
                thread_id,
                capacity_dmips_mhz: None,
            }
        })
        .collect();
    general::dev::cpu::install_topology(topology);

    let cpu_numa = processors
        .iter()
        .enumerate()
        .filter_map(|(logical_id, processor)| {
            let affinity = info
                .srat
                .as_ref()?
                .processor_affinities
                .iter()
                .find(|affinity| {
                    affinity.enabled && processor_affinity_matches(processor, affinity)
                })?;
            Some(CpuNumaEntry {
                logical_id: logical_id as u32,
                node_id: affinity.proximity_domain,
            })
        })
        .collect();
    general::dev::cpu::install_numa_topology(cpu_numa);
}

fn publish_numa_topology(info: &AcpiPlatformInfo, usable_memory: &[MemorySegment]) {
    let cpu_nodes: Vec<u32> = general::dev::cpu::snapshot_numa_topology()
        .into_iter()
        .map(|entry| entry.node_id)
        .collect();
    let mut memory = Vec::new();
    for affinity in info
        .srat
        .iter()
        .flat_map(|srat| &srat.memory_affinities)
        .filter(|range| range.enabled && !range.specific_purpose)
    {
        let (Ok(affinity_start), Ok(affinity_size)) = (
            usize::try_from(affinity.base),
            usize::try_from(affinity.length),
        ) else {
            continue;
        };
        let Some(affinity_end) = affinity_start.checked_add(affinity_size) else {
            continue;
        };
        for segment in usable_memory {
            let Some(segment_end) = segment.start.checked_add(segment.size) else {
                continue;
            };
            let start = affinity_start.max(segment.start);
            let end = affinity_end.min(segment_end);
            if start < end {
                memory.push(NumaMemoryRange {
                    start,
                    size: end - start,
                    node_id: affinity.proximity_domain,
                });
            }
        }
    }
    let distances = info
        .slit
        .iter()
        .flat_map(|slit| {
            (0..slit.locality_count).flat_map(move |from| {
                (0..slit.locality_count).filter_map(move |to| {
                    Some(NumaDistance {
                        from: u32::try_from(from).ok()?,
                        to: u32::try_from(to).ok()?,
                        distance: u32::from(slit.distance(from, to)?),
                    })
                })
            })
        })
        .collect();
    general::dev::numa::install_topology(cpu_nodes, distances, memory);
}

fn validate_cross_table_relations(
    info: &mut AcpiPlatformInfo,
    boot_memory: Option<&[StartMemoryRegion]>,
) {
    if let Some(wakeup) = info
        .madt
        .as_ref()
        .and_then(|madt| madt.multiprocessor_wakeup)
    {
        let mailbox_start = usize::try_from(wakeup.mailbox_address).ok();
        let mailbox_is_nvs = mailbox_start
            .and_then(|start| start.checked_add(0x1000).map(|end| (start, end)))
            .is_some_and(|(start, end)| {
                boot_memory.is_some_and(|regions| range_is_acpi_nvs(regions, start, end))
            });
        if !mailbox_is_nvs {
            printk!(
                "[kernel-start][acpi] ignored MADT multiprocessor wakeup mailbox outside ACPI NVS"
            );
            if let Some(madt) = info.madt.as_mut() {
                madt.multiprocessor_wakeup = None;
            }
        }
    }

    if let (Some(madt), Some(pptt)) = (info.madt.as_ref(), info.pptt.as_ref())
        && !pptt_matches_madt(pptt, madt)
    {
        printk!("[kernel-start][acpi] rejected PPTT: processor leaves do not match MADT");
        info.pptt = None;
    }

    if let (Some(madt), Some(srat)) = (info.madt.as_ref(), info.srat.as_ref())
        && !srat_matches_madt(srat, madt)
    {
        printk!("[kernel-start][acpi] rejected SRAT: processor affinities do not match MADT");
        info.srat = None;
        info.slit = None;
    }

    if let (Some(srat), Some(slit)) = (info.srat.as_ref(), info.slit.as_ref()) {
        let locality_count = slit.locality_count;
        let invalid_domain = srat
            .processor_affinities
            .iter()
            .filter(|entry| entry.enabled)
            .map(|entry| entry.proximity_domain)
            .chain(
                srat.memory_affinities
                    .iter()
                    .filter(|entry| entry.enabled)
                    .map(|entry| entry.proximity_domain),
            )
            .chain(
                srat.initiator_affinities
                    .iter()
                    .filter(|entry| entry.enabled)
                    .map(|entry| entry.proximity_domain),
            )
            .any(|domain| usize::try_from(domain).map_or(true, |domain| domain >= locality_count));
        if invalid_domain {
            printk!("[kernel-start][acpi] rejected SLIT: SRAT proximity domain is out of range");
            info.slit = None;
        }
    }
}

fn range_is_acpi_nvs(regions: &[StartMemoryRegion], start: usize, end: usize) -> bool {
    if start >= end {
        return false;
    }
    let mut cursor = start;
    while cursor < end {
        let Some(next) = regions
            .iter()
            .filter(|region| {
                region.kind == StartMemoryRegionKind::AcpiNonVolatileStorage
                    && region.range.start <= cursor
                    && cursor < region.range.end
            })
            .map(|region| region.range.end.min(end))
            .max()
        else {
            return false;
        };
        cursor = next;
    }
    true
}

fn srat_matches_madt(srat: &general::firmware::acpi::AcpiSratInfo, madt: &AcpiMadtInfo) -> bool {
    let processors: Vec<&AcpiProcessor> = madt
        .processors
        .iter()
        .filter(|processor| processor.usable())
        .collect();
    let affinities: Vec<&general::firmware::acpi::AcpiNumaProcessorAffinity> = srat
        .processor_affinities
        .iter()
        .filter(|affinity| affinity.enabled)
        .collect();
    processors.len() == affinities.len()
        && processors.iter().all(|processor| {
            affinities
                .iter()
                .filter(|affinity| processor_affinity_matches(processor, affinity))
                .count()
                == 1
        })
        && affinities.iter().all(|affinity| {
            processors
                .iter()
                .filter(|processor| processor_affinity_matches(processor, affinity))
                .count()
                == 1
        })
}

fn pptt_matches_madt(pptt: &AcpiPpttInfo, madt: &AcpiMadtInfo) -> bool {
    let leaves: Vec<&AcpiPpttProcessor> = pptt
        .processors
        .iter()
        .filter(|processor| processor.is_leaf)
        .collect();
    let processors: Vec<&AcpiProcessor> = madt.processors.iter().collect();
    leaves.len() == processors.len()
        && leaves.iter().all(|leaf| {
            leaf.processor_uid.is_some_and(|uid| {
                processors
                    .iter()
                    .filter(|processor| processor.processor_uid == uid)
                    .count()
                    == 1
            })
        })
}

fn processor_affinity_matches(
    processor: &AcpiProcessor,
    affinity: &general::firmware::acpi::AcpiNumaProcessorAffinity,
) -> bool {
    use general::firmware::acpi::AcpiNumaProcessorKind;
    match (processor.interface, affinity.kind) {
        (AcpiProcessorInterface::LocalApic, AcpiNumaProcessorKind::LocalApic)
            if affinity
                .hardware_id
                .is_some_and(|id| id & 0xff == processor.hardware_id) =>
        {
            true
        }
        (AcpiProcessorInterface::LocalSapic, AcpiNumaProcessorKind::LocalApic)
        | (AcpiProcessorInterface::LocalX2Apic, AcpiNumaProcessorKind::LocalX2Apic) => {
            affinity.hardware_id == Some(processor.hardware_id)
        }
        (AcpiProcessorInterface::Gicc, AcpiNumaProcessorKind::Gicc)
        | (AcpiProcessorInterface::RiscVIntc, AcpiNumaProcessorKind::RiscVIntc) => {
            affinity.processor_uid == Some(processor.processor_uid)
        }
        _ => false,
    }
}

fn pptt_hierarchy<'a>(
    pptt: Option<&'a AcpiPpttInfo>,
    leaf: Option<&'a AcpiPpttProcessor>,
) -> Vec<&'a AcpiPpttProcessor> {
    let Some(pptt) = pptt else {
        return Vec::new();
    };
    let mut hierarchy = Vec::new();
    let mut node = leaf;
    while let Some(current) = node {
        hierarchy.push(current);
        node = current
            .parent_offset
            .and_then(|offset| pptt.processor_at(offset));
    }
    hierarchy
}

fn pptt_topology_ids(
    leaf: Option<&AcpiPpttProcessor>,
    hierarchy: &[&AcpiPpttProcessor],
) -> (Option<u32>, Vec<u32>, Option<u32>, Option<u32>) {
    let socket_id = hierarchy
        .iter()
        .find(|node| node.physical_package)
        .map(|node| node.table_offset);
    let thread_id = leaf
        .filter(|node| node.is_thread)
        .map(|node| node.table_offset);
    let core_id = if thread_id.is_some() {
        hierarchy
            .iter()
            .skip(1)
            .find(|node| !node.physical_package)
            .map(|node| node.table_offset)
    } else {
        leaf.map(|node| node.table_offset)
    };
    let mut cluster_path: Vec<u32> = hierarchy
        .iter()
        .rev()
        .filter(|node| {
            Some(node.table_offset) != socket_id
                && Some(node.table_offset) != core_id
                && Some(node.table_offset) != thread_id
        })
        .map(|node| node.table_offset)
        .collect();
    cluster_path.dedup();
    (socket_id, cluster_path, core_id, thread_id)
}

#[allow(dead_code)]
fn _madt_for_arch(info: &AcpiPlatformInfo) -> Option<&AcpiMadtInfo> {
    info.madt.as_ref()
}

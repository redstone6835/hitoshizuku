//! ACPI table snapshot helpers used by the loader path.

use acpi::sdt::fadt::Fadt;

use super::{checksum_valid, read_u32_le, read_u64_le};

const ACPI_TABLE_HEADER_SIZE: usize = 36;
const ACPI_RSDP_V1_SIZE: usize = 20;
const ACPI_RSDP_V2_SIZE: usize = 36;
const ACPI_MAX_RSDP_SIZE: usize = 4096;
const ACPI_MAX_TABLE_SIZE: usize = 1024 * 1024;
const ACPI_SIG_RSDP: &[u8; 8] = b"RSD PTR ";

pub type AcpiReadPhysicalBytesFn = fn(usize, usize) -> &'static [u8];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpiRootTableKind {
    Rsdt,
    Xsdt,
}

impl AcpiRootTableKind {
    pub const fn entry_size(self) -> usize {
        match self {
            Self::Rsdt => 4,
            Self::Xsdt => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiSnapshotRootInfo {
    pub rsdp_copy_len: usize,
    pub root_phys: usize,
    pub root_kind: AcpiRootTableKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiFadtClosure {
    pub dsdt_phys: Option<usize>,
    pub facs_phys: Option<usize>,
}

pub fn snapshot_root_info(
    rsdp_phys: usize,
    read_physical: AcpiReadPhysicalBytesFn,
) -> Result<AcpiSnapshotRootInfo, &'static str> {
    let rsdp_probe = read_physical(rsdp_phys, ACPI_RSDP_V2_SIZE);
    if rsdp_probe.get(..8) != Some(ACPI_SIG_RSDP) {
        return Err("[loader][acpi] invalid RSDP signature");
    }
    if !checksum_valid(
        rsdp_probe
            .get(..ACPI_RSDP_V1_SIZE)
            .ok_or("[loader][acpi] truncated RSDP")?,
    ) {
        return Err("[loader][acpi] RSDP v1 checksum mismatch");
    }

    let revision = *rsdp_probe.get(15).ok_or("[loader][acpi] truncated RSDP")?;
    let mut rsdp_copy_len = ACPI_RSDP_V2_SIZE;
    let mut root_phys = read_u32_le(rsdp_probe, 16).ok_or("[loader][acpi] missing RSDT")? as usize;
    let mut root_kind = AcpiRootTableKind::Rsdt;

    if revision >= 2 {
        let length =
            read_u32_le(rsdp_probe, 20).ok_or("[loader][acpi] missing RSDP length")? as usize;
        if !(ACPI_RSDP_V2_SIZE..=ACPI_MAX_RSDP_SIZE).contains(&length) {
            return Err("[loader][acpi] invalid RSDP length");
        }
        let rsdp = read_physical(rsdp_phys, length);
        if !checksum_valid(rsdp) {
            return Err("[loader][acpi] RSDP extended checksum mismatch");
        }
        let xsdt_phys = read_u64_le(rsdp, 24).ok_or("[loader][acpi] missing XSDT")? as usize;
        if xsdt_phys != 0 {
            root_phys = xsdt_phys;
            root_kind = AcpiRootTableKind::Xsdt;
        }
        rsdp_copy_len = length.max(ACPI_RSDP_V2_SIZE);
    }

    if root_phys == 0 {
        return Err("[loader][acpi] missing ACPI root table");
    }

    Ok(AcpiSnapshotRootInfo {
        rsdp_copy_len,
        root_phys,
        root_kind,
    })
}

pub fn table_length(
    phys_addr: usize,
    read_physical: AcpiReadPhysicalBytesFn,
) -> Result<usize, &'static str> {
    let header = read_physical(phys_addr, ACPI_TABLE_HEADER_SIZE);
    let len = read_u32_le(header, 4).ok_or("[loader][acpi] malformed ACPI table header")? as usize;
    if !(ACPI_TABLE_HEADER_SIZE..=ACPI_MAX_TABLE_SIZE).contains(&len) {
        return Err("[loader][acpi] invalid ACPI table length");
    }
    Ok(len)
}

pub fn facs_length(
    phys_addr: usize,
    read_physical: AcpiReadPhysicalBytesFn,
) -> Result<usize, &'static str> {
    let header = read_physical(phys_addr, 8);
    if header.get(..4) != Some(b"FACS") {
        return Err("[loader][acpi] invalid FACS signature");
    }
    let len = read_u32_le(header, 4).ok_or("[loader][acpi] malformed FACS header")? as usize;
    if !(8..=ACPI_MAX_TABLE_SIZE).contains(&len) {
        return Err("[loader][acpi] invalid FACS length");
    }
    Ok(len)
}

pub fn validate_sdt(table: &[u8]) -> Result<(), &'static str> {
    if !checksum_valid(table) {
        return Err("[loader][acpi] ACPI table checksum mismatch");
    }
    Ok(())
}

pub fn validate_root_table(root: &[u8], root_kind: AcpiRootTableKind) -> Result<(), &'static str> {
    let expected = match root_kind {
        AcpiRootTableKind::Rsdt => b"RSDT".as_slice(),
        AcpiRootTableKind::Xsdt => b"XSDT".as_slice(),
    };
    if root.get(..4) != Some(expected) {
        return Err("[loader][acpi] ACPI root table signature mismatch");
    }
    if !checksum_valid(root) {
        return Err("[loader][acpi] ACPI root table checksum mismatch");
    }
    Ok(())
}

pub fn for_each_root_table_entry(
    root: &[u8],
    root_kind: AcpiRootTableKind,
    mut visitor: impl FnMut(usize) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let entries = root
        .get(ACPI_TABLE_HEADER_SIZE..)
        .ok_or("[loader][acpi] invalid ACPI root table")?;
    let entry_size = root_kind.entry_size();
    for entry in entries.chunks_exact(entry_size) {
        let table_phys = if entry_size == 8 {
            read_u64_le(entry, 0).unwrap_or(0) as usize
        } else {
            read_u32_le(entry, 0).unwrap_or(0) as usize
        };
        if table_phys != 0 {
            visitor(table_phys)?;
        }
    }
    Ok(())
}

pub fn fadt_closure(table: &[u8]) -> Result<Option<AcpiFadtClosure>, &'static str> {
    if table.get(..4) != Some(b"FACP") {
        return Ok(None);
    }

    let fadt = unsafe { &*table.as_ptr().cast::<Fadt>() };
    fadt.validate()
        .map_err(|_| "[loader][acpi] invalid FADT checksum or signature")?;

    Ok(Some(AcpiFadtClosure {
        dsdt_phys: fadt.dsdt_address().ok().filter(|phys| *phys != 0),
        facs_phys: fadt.facs_address().ok().filter(|phys| *phys != 0),
    }))
}

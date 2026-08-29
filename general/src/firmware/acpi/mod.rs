//! Architecture-neutral ACPI table validation, snapshots, and platform data.

mod madt;
mod numa;
mod platform;
mod types;

pub use madt::parse_madt;
pub use numa::{parse_slit, parse_srat};
pub use platform::{parse_fadt, parse_hpet, parse_mcfg, parse_pptt, parse_spcr};
pub use types::*;

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
    /// 固件中经过校验、允许读取的 RSDP 字节数。
    pub rsdp_source_len: usize,
    /// 交给 `acpi` crate 的映射长度。v1 RSDP 必须把 source 后的字节清零，
    /// 不能从固件地址继续读取并复制。
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
    let rsdp_v1 = read_physical(rsdp_phys, ACPI_RSDP_V1_SIZE);
    if rsdp_v1.get(..8) != Some(ACPI_SIG_RSDP) {
        return Err("[loader][acpi] invalid RSDP signature");
    }
    if !checksum_valid(
        rsdp_v1
            .get(..ACPI_RSDP_V1_SIZE)
            .ok_or("[loader][acpi] truncated RSDP")?,
    ) {
        return Err("[loader][acpi] RSDP v1 checksum mismatch");
    }

    let revision = *rsdp_v1.get(15).ok_or("[loader][acpi] truncated RSDP")?;
    if revision == 1 {
        return Err("[loader][acpi] reserved RSDP revision");
    }
    let mut rsdp_source_len = ACPI_RSDP_V1_SIZE;
    let mut rsdp_copy_len = ACPI_RSDP_V2_SIZE;
    let mut root_phys = read_u32_le(rsdp_v1, 16).ok_or("[loader][acpi] missing RSDT")? as usize;
    let mut root_kind = AcpiRootTableKind::Rsdt;

    if revision >= 2 {
        let rsdp_probe = read_physical(rsdp_phys, ACPI_RSDP_V2_SIZE);
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
        if xsdt_phys == 0 {
            return Err("[loader][acpi] extended RSDP is missing XSDT");
        }
        root_phys = xsdt_phys;
        root_kind = AcpiRootTableKind::Xsdt;
        rsdp_source_len = length;
        rsdp_copy_len = length.max(ACPI_RSDP_V2_SIZE);
    }

    if root_phys == 0 {
        return Err("[loader][acpi] missing ACPI root table");
    }

    Ok(AcpiSnapshotRootInfo {
        rsdp_source_len,
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
    if !(64..=ACPI_MAX_TABLE_SIZE).contains(&len) {
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
    let declared_length =
        read_u32_le(root, 4).ok_or("[loader][acpi] truncated ACPI root table")? as usize;
    if declared_length != root.len()
        || root
            .len()
            .checked_sub(ACPI_TABLE_HEADER_SIZE)
            .is_none_or(|payload| !payload.is_multiple_of(root_kind.entry_size()))
    {
        return Err("[loader][acpi] malformed ACPI root table entries");
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

    const FADT_LEGACY_POINTERS_END: usize = 44;
    if table.len() < FADT_LEGACY_POINTERS_END || !checksum_valid(table) {
        return Err("[loader][acpi] invalid FADT checksum or length");
    }

    // ACPI 1.0 addresses are always present at offsets 36 and 40. ACPI 2.0 added the
    // preferred 64-bit X_FIRMWARE_CTRL and X_DSDT fields at offsets 132 and 140. Parse the
    // byte representation explicitly: old FADT revisions are shorter than the latest Rust
    // `Fadt` structure, so constructing `&Fadt` over their storage would already violate
    // Rust's reference validity rules before the crate could inspect the revision.
    let firmware_ctrl = read_u32_le(table, 36).unwrap_or(0) as usize;
    let dsdt = read_u32_le(table, 40).unwrap_or(0) as usize;
    // X_FIRMWARE_CTRL/X_DSDT were added with the ACPI 2.0 FADT.  A legacy
    // revision may legally be followed by padding in a snapshot, but those
    // bytes are not part of its schema and must not override the 32-bit
    // closure pointers.
    let has_extended_fields = table.get(8).copied().is_some_and(|revision| revision >= 2);
    let x_firmware_ctrl = has_extended_fields
        .then(|| read_u64_le(table, 132))
        .flatten()
        .and_then(|address| usize::try_from(address).ok())
        .unwrap_or(0);
    let x_dsdt = has_extended_fields
        .then(|| read_u64_le(table, 140))
        .flatten()
        .and_then(|address| usize::try_from(address).ok())
        .unwrap_or(0);

    Ok(Some(AcpiFadtClosure {
        dsdt_phys: (x_dsdt != 0)
            .then_some(x_dsdt)
            .or((dsdt != 0).then_some(dsdt)),
        facs_phys: (x_firmware_ctrl != 0)
            .then_some(x_firmware_ctrl)
            .or((firmware_ctrl != 0).then_some(firmware_ctrl)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finish_sdt(table: &mut [u8], revision: u8) {
        let length = table.len() as u32;
        table[..4].copy_from_slice(b"FACP");
        table[4..8].copy_from_slice(&length.to_le_bytes());
        table[8] = revision;
        table[9] = 0;
        let checksum = table.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        table[9] = 0u8.wrapping_sub(checksum);
    }

    #[test]
    fn parses_short_acpi_v1_fadt_without_constructing_latest_layout() {
        let mut table = [0u8; 44];
        table[36..40].copy_from_slice(&0x1234u32.to_le_bytes());
        table[40..44].copy_from_slice(&0x5678u32.to_le_bytes());
        finish_sdt(&mut table, 1);

        assert_eq!(
            fadt_closure(&table),
            Ok(Some(AcpiFadtClosure {
                dsdt_phys: Some(0x5678),
                facs_phys: Some(0x1234),
            }))
        );
    }

    #[test]
    fn rejects_fadt_without_legacy_closure_pointers() {
        let mut table = [0u8; ACPI_TABLE_HEADER_SIZE];
        finish_sdt(&mut table, 1);

        assert_eq!(
            fadt_closure(&table),
            Err("[loader][acpi] invalid FADT checksum or length")
        );
    }

    #[test]
    fn extended_fadt_addresses_take_precedence() {
        let mut table = [0u8; 148];
        table[36..40].copy_from_slice(&0x1234u32.to_le_bytes());
        table[40..44].copy_from_slice(&0x5678u32.to_le_bytes());
        table[132..140].copy_from_slice(&0x1_0000_1234u64.to_le_bytes());
        table[140..148].copy_from_slice(&0x1_0000_5678u64.to_le_bytes());
        finish_sdt(&mut table, 2);

        let closure = fadt_closure(&table).unwrap().unwrap();
        if let (Ok(facs), Ok(dsdt)) = (
            usize::try_from(0x1_0000_1234u64),
            usize::try_from(0x1_0000_5678u64),
        ) {
            assert_eq!(closure.facs_phys, Some(facs));
            assert_eq!(closure.dsdt_phys, Some(dsdt));
        } else {
            assert_eq!(closure.facs_phys, Some(0x1234));
            assert_eq!(closure.dsdt_phys, Some(0x5678));
        }
    }

    #[test]
    fn rejects_root_table_with_partial_trailing_entry() {
        let mut root = [0u8; ACPI_TABLE_HEADER_SIZE + 5];
        let root_len = root.len() as u32;
        root[..4].copy_from_slice(b"RSDT");
        root[4..8].copy_from_slice(&root_len.to_le_bytes());
        root[9] = 0u8.wrapping_sub(root.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));

        assert_eq!(
            validate_root_table(&root, AcpiRootTableKind::Rsdt),
            Err("[loader][acpi] malformed ACPI root table entries")
        );
    }
}

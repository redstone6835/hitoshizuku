use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const CATALOG_SCHEMA: u32 = 1;
pub const CATALOG_RELATIVE_PATH: &str = "configs/platforms.toml";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformCatalog {
    schema: u32,
    platforms: Vec<PlatformSpec>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformSpec {
    pub id: String,
    pub board: String,
    pub target: String,
    pub default_for_board: bool,
    pub config: String,
    pub target_dir: String,
    pub interface_dir: String,
    #[serde(default)]
    pub rust_cfg: Vec<String>,
    pub link: LinkSpec,
    pub image: ImageSpec,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinkSpec {
    pub layout: LinkLayout,
    pub physical_base: HexAddress,
    pub virtual_base: HexAddress,
    pub alignment: HexAddress,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkLayout {
    Loongarch64Dmw1,
    Riscv64Sv48,
}

impl LinkLayout {
    pub const fn architecture(self) -> &'static str {
        match self {
            Self::Loongarch64Dmw1 => "loongarch64",
            Self::Riscv64Sv48 => "riscv64",
        }
    }

    pub const fn elf_machine(self) -> u16 {
        match self {
            Self::Loongarch64Dmw1 => 258,
            Self::Riscv64Sv48 => 243,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageSpec {
    pub output_dir: String,
    pub default_formats: Vec<ImageFormat>,
    pub allowed_formats: Vec<ImageFormat>,
    pub uimage: Option<UImageSpec>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Elf,
    Raw,
    Uimage,
}

impl ImageFormat {
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Elf => "kernel.elf",
            Self::Raw => "kernel.bin",
            Self::Uimage => "uImage",
        }
    }
}

impl std::str::FromStr for ImageFormat {
    type Err = CatalogError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "elf" => Ok(Self::Elf),
            "raw" => Ok(Self::Raw),
            "uimage" => Ok(Self::Uimage),
            other => Err(CatalogError::new(format!(
                "unsupported image format {other:?}; expected elf, raw, uimage, or all"
            ))),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UImageSpec {
    pub architecture: String,
    pub os: String,
    pub image_type: String,
    pub compression: String,
    pub name: String,
}

impl UImageSpec {
    /// Numeric values stored in a legacy uImage header, in
    /// `os, architecture, type, compression` order.
    pub fn legacy_header_ids(&self) -> Result<[u8; 4], CatalogError> {
        let architecture = match self.architecture.as_str() {
            "riscv" => 26,
            // The LoongArch U-Boot port appends this after RISC-V.
            "loongarch" => 27,
            other => {
                return Err(CatalogError::new(format!(
                    "unsupported legacy uImage architecture {other:?}"
                )));
            }
        };
        let os = match self.os.as_str() {
            "linux" => 5,
            other => {
                return Err(CatalogError::new(format!(
                    "unsupported legacy uImage operating system {other:?}"
                )));
            }
        };
        let image_type = match self.image_type.as_str() {
            "kernel" => 2,
            other => {
                return Err(CatalogError::new(format!(
                    "unsupported legacy uImage type {other:?}"
                )));
            }
        };
        let compression = match self.compression.as_str() {
            "none" => 0,
            other => {
                return Err(CatalogError::new(format!(
                    "unsupported legacy uImage compression {other:?}"
                )));
            }
        };
        Ok([os, architecture, image_type, compression])
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub struct HexAddress(u64);

impl HexAddress {
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for HexAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for HexAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = format!("{:016x}", self.0);
        write!(
            formatter,
            "0x{}_{}_{}_{}",
            &value[0..4],
            &value[4..8],
            &value[8..12],
            &value[12..16]
        )
    }
}

impl Serialize for HexAddress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for HexAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_hex_address(&value).map_err(serde::de::Error::custom)
    }
}

fn parse_hex_address(value: &str) -> Result<HexAddress, CatalogError> {
    let bytes = value.as_bytes();
    let grouped = bytes.len() == 21
        && value.starts_with("0x")
        && bytes[6] == b'_'
        && bytes[11] == b'_'
        && bytes[16] == b'_';
    if !grouped {
        return Err(CatalogError::new(format!(
            "address {value:?} must use canonical 0x0000_0000_0000_0000 form"
        )));
    }
    let digits = value[2..].replace('_', "");
    if !digits
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CatalogError::new(format!(
            "address {value:?} must contain lowercase hexadecimal digits"
        )));
    }
    u64::from_str_radix(&digits, 16)
        .map(HexAddress)
        .map_err(|error| CatalogError::new(format!("invalid address {value:?}: {error}")))
}

#[derive(Debug)]
pub struct CatalogError(String);

impl CatalogError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CatalogError {}

impl PlatformCatalog {
    pub fn parse(source: &str) -> Result<Self, CatalogError> {
        let catalog: Self = toml::from_str(source)
            .map_err(|error| CatalogError::new(format!("parse platform catalog: {error}")))?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, CatalogError> {
        let path = path.as_ref();
        let source = std::fs::read_to_string(path).map_err(|error| {
            CatalogError::new(format!("read platform catalog {}: {error}", path.display()))
        })?;
        Self::parse(&source)
    }

    pub fn platforms(&self) -> &[PlatformSpec] {
        &self.platforms
    }

    pub fn get(&self, id: &str) -> Result<&PlatformSpec, CatalogError> {
        self.platforms
            .iter()
            .find(|platform| platform.id == id)
            .ok_or_else(|| CatalogError::new(format!("unknown platform {id:?}")))
    }

    pub fn select(
        &self,
        platform: Option<&str>,
        board: Option<&str>,
        target: Option<&str>,
    ) -> Result<&PlatformSpec, CatalogError> {
        if let Some(id) = platform {
            if board.is_some() || target.is_some() {
                return Err(CatalogError::new(
                    "--platform cannot be combined with --board or --target",
                ));
            }
            return self.get(id);
        }

        let board = board.unwrap_or("qemu");
        let mut matches = self
            .platforms
            .iter()
            .filter(|candidate| candidate.board == board)
            .filter(|candidate| target.is_none_or(|target| candidate.target == target));
        if let Some(found) = matches.next()
            && matches.next().is_none()
            && target.is_some()
        {
            return Ok(found);
        }

        if let Some(target) = target {
            return Err(CatalogError::new(format!(
                "board {board:?} does not support target {target:?}"
            )));
        }
        self.platforms
            .iter()
            .find(|candidate| candidate.board == board && candidate.default_for_board)
            .ok_or_else(|| CatalogError::new(format!("unknown board {board:?}")))
    }

    /// Resolve the platform seen by a Cargo build script.
    ///
    /// Direct Cargo builds without `HITOSHIZUKU_PLATFORM` retain the QEMU
    /// layout for their target. Orchestrated builds pass an exact platform ID.
    pub fn select_for_build(
        &self,
        platform: Option<&str>,
        target: &str,
    ) -> Result<&PlatformSpec, CatalogError> {
        let selected = match platform.filter(|value| !value.is_empty()) {
            Some(id) => self.get(id)?,
            None => self.select(None, Some("qemu"), Some(target))?,
        };
        if selected.target != target {
            return Err(CatalogError::new(format!(
                "platform {:?} requires target {:?}, but Cargo is building {:?}",
                selected.id, selected.target, target
            )));
        }
        Ok(selected)
    }

    fn validate(&self) -> Result<(), CatalogError> {
        if self.schema != CATALOG_SCHEMA {
            return Err(CatalogError::new(format!(
                "unsupported platform catalog schema {}; expected {CATALOG_SCHEMA}",
                self.schema
            )));
        }
        if self.platforms.is_empty() {
            return Err(CatalogError::new("platform catalog is empty"));
        }

        let mut ids = BTreeSet::new();
        let mut identity_tags = BTreeMap::new();
        let mut board_targets = BTreeSet::new();
        let mut board_defaults = BTreeMap::<&str, usize>::new();
        for platform in &self.platforms {
            validate_identifier("platform id", &platform.id, true)?;
            validate_identifier("board", &platform.board, true)?;
            if !ids.insert(platform.id.as_str()) {
                return Err(CatalogError::new(format!(
                    "duplicate platform id {:?}",
                    platform.id
                )));
            }
            if let Some(other) = identity_tags.insert(platform.identity_tag(), platform.id.as_str())
            {
                return Err(CatalogError::new(format!(
                    "platform ids {:?} and {:?} have the same ELF identity tag",
                    other, platform.id
                )));
            }
            if !board_targets.insert((platform.board.as_str(), platform.target.as_str())) {
                return Err(CatalogError::new(format!(
                    "duplicate board/target pair {:?}/{:?}",
                    platform.board, platform.target
                )));
            }
            *board_defaults.entry(platform.board.as_str()).or_default() +=
                usize::from(platform.default_for_board);
            platform.validate()?;
        }
        for (board, defaults) in board_defaults {
            if defaults != 1 {
                return Err(CatalogError::new(format!(
                    "board {board:?} must have exactly one default platform; found {defaults}"
                )));
            }
        }
        Ok(())
    }
}

impl PlatformSpec {
    pub fn architecture(&self) -> &'static str {
        self.link.layout.architecture()
    }

    /// Stable provenance tag embedded in the kernel ELF by the linker.
    ///
    /// This is an identity marker, not a cryptographic digest. The catalog
    /// rejects collisions, while artifact integrity remains the responsibility
    /// of the normal release/signing pipeline.
    pub fn identity_tag(&self) -> u64 {
        fnv1a64(self.id.as_bytes())
    }

    fn validate(&self) -> Result<(), CatalogError> {
        let expected_target = match self.link.layout {
            LinkLayout::Loongarch64Dmw1 => "loongarch64-unknown-none",
            LinkLayout::Riscv64Sv48 => "riscv64gc-unknown-none-elf",
        };
        if self.target != expected_target {
            return Err(CatalogError::new(format!(
                "platform {:?} layout {:?} requires target {expected_target:?}, not {:?}",
                self.id, self.link.layout, self.target
            )));
        }

        let alignment = self.link.alignment.get();
        if alignment < 0x1000 || !alignment.is_power_of_two() {
            return Err(CatalogError::new(format!(
                "platform {:?} alignment {} must be a power of two of at least 0x1000",
                self.id, self.link.alignment
            )));
        }
        if !self.link.physical_base.get().is_multiple_of(alignment)
            || !self.link.virtual_base.get().is_multiple_of(alignment)
        {
            return Err(CatalogError::new(format!(
                "platform {:?} link bases must be aligned to {}",
                self.id, self.link.alignment
            )));
        }

        let expected_virtual = match self.link.layout {
            LinkLayout::Loongarch64Dmw1 => {
                if self.link.physical_base.get() >= 1 << 60 {
                    return Err(CatalogError::new(format!(
                        "platform {:?} physical base does not fit DMW1",
                        self.id
                    )));
                }
                0x9000_0000_0000_0000 | self.link.physical_base.get()
            }
            LinkLayout::Riscv64Sv48 => {
                if self.link.physical_base.get() >= 0x80_0000_0000 {
                    return Err(CatalogError::new(format!(
                        "platform {:?} physical base does not fit the Sv48 kernel window",
                        self.id
                    )));
                }
                0xffff_ff80_0000_0000 | self.link.physical_base.get()
            }
        };
        if self.link.virtual_base.get() != expected_virtual {
            return Err(CatalogError::new(format!(
                "platform {:?} virtual base {} does not match {:?} mapping {}; expected {}",
                self.id,
                self.link.virtual_base,
                self.link.layout,
                self.link.physical_base,
                HexAddress(expected_virtual)
            )));
        }

        for path in [
            &self.config,
            &self.target_dir,
            &self.interface_dir,
            &self.image.output_dir,
        ] {
            validate_relative_path(&self.id, path)?;
        }
        let mut cfgs = BTreeSet::new();
        for cfg in &self.rust_cfg {
            validate_identifier("rust cfg", cfg, false)?;
            if !cfgs.insert(cfg) {
                return Err(CatalogError::new(format!(
                    "platform {:?} repeats rust cfg {cfg:?}",
                    self.id
                )));
            }
        }
        self.image.validate(&self.id)?;
        if self.image.uimage.is_some() && self.link.physical_base.get() > u64::from(u32::MAX) {
            return Err(CatalogError::new(format!(
                "platform {:?} uImage load address does not fit the legacy 32-bit header",
                self.id
            )));
        }
        Ok(())
    }
}

impl ImageSpec {
    fn validate(&self, platform: &str) -> Result<(), CatalogError> {
        if self.default_formats.is_empty() || self.allowed_formats.is_empty() {
            return Err(CatalogError::new(format!(
                "platform {platform:?} image formats cannot be empty"
            )));
        }
        let allowed = self
            .allowed_formats
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if allowed.len() != self.allowed_formats.len() {
            return Err(CatalogError::new(format!(
                "platform {platform:?} repeats an allowed image format"
            )));
        }
        let defaults = self
            .default_formats
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if defaults.len() != self.default_formats.len() || !defaults.is_subset(&allowed) {
            return Err(CatalogError::new(format!(
                "platform {platform:?} default image formats must be unique and allowed"
            )));
        }
        if !allowed.contains(&ImageFormat::Elf) {
            return Err(CatalogError::new(format!(
                "platform {platform:?} must allow the canonical ELF image"
            )));
        }
        if allowed.contains(&ImageFormat::Uimage) != self.uimage.is_some() {
            return Err(CatalogError::new(format!(
                "platform {platform:?} must define uimage settings exactly when uimage is allowed"
            )));
        }
        if allowed.contains(&ImageFormat::Uimage) && !allowed.contains(&ImageFormat::Raw) {
            return Err(CatalogError::new(format!(
                "platform {platform:?} must allow raw images when uimage is allowed"
            )));
        }
        if let Some(uimage) = &self.uimage {
            for (name, value) in [
                ("architecture", &uimage.architecture),
                ("os", &uimage.os),
                ("image_type", &uimage.image_type),
                ("compression", &uimage.compression),
                ("name", &uimage.name),
            ] {
                if value.is_empty() || value.chars().any(char::is_control) {
                    return Err(CatalogError::new(format!(
                        "platform {platform:?} uimage {name} is invalid"
                    )));
                }
            }
            if !uimage.name.is_ascii() || uimage.name.len() > 32 {
                return Err(CatalogError::new(format!(
                    "platform {platform:?} uimage name must be at most 32 ASCII bytes"
                )));
            }
            for (name, value) in [
                ("architecture", &uimage.architecture),
                ("os", &uimage.os),
                ("image_type", &uimage.image_type),
                ("compression", &uimage.compression),
            ] {
                if !value.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'
                        || byte == b'-'
                }) {
                    return Err(CatalogError::new(format!(
                        "platform {platform:?} uimage {name} must be a lowercase mkimage token"
                    )));
                }
            }
            uimage.legacy_header_ids()?;
        }
        Ok(())
    }
}

fn validate_identifier(kind: &str, value: &str, allow_hyphen: bool) -> Result<(), CatalogError> {
    let valid_start = value.bytes().next().is_some_and(|byte| {
        byte.is_ascii_lowercase() || (allow_hyphen && byte.is_ascii_digit()) || byte == b'_'
    });
    let valid = valid_start
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || byte == b'_'
                || (allow_hyphen && byte == b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(CatalogError::new(format!(
            "invalid {kind} {value:?}; use lowercase ASCII letters, digits{}",
            if allow_hyphen {
                ", '_' or '-'"
            } else {
                " or '_'"
            }
        )))
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn validate_relative_path(platform: &str, value: &str) -> Result<(), CatalogError> {
    let path = Path::new(value);
    let valid = !value.is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
    if valid {
        Ok(())
    } else {
        Err(CatalogError::new(format!(
            "platform {platform:?} path {value:?} must be repository-relative and cannot contain '..'"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = include_str!("../../configs/platforms.toml");

    #[test]
    fn repository_catalog_is_valid() {
        let catalog = PlatformCatalog::parse(CATALOG).expect("valid repository catalog");
        assert_eq!(catalog.platforms().len(), 4);
        let ls2k = catalog.get("ls2k1000").expect("LS2K1000 platform");
        assert_eq!(ls2k.link.physical_base.get(), 0x20_0000);
        assert_eq!(ls2k.link.virtual_base.get(), 0x9000_0000_0020_0000);
        assert_ne!(
            ls2k.identity_tag(),
            catalog
                .get("qemu-loongarch64")
                .expect("QEMU LoongArch64 platform")
                .identity_tag()
        );
    }

    #[test]
    fn platform_identity_tag_has_a_stable_golden_value() {
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn qemu_target_selects_an_exact_platform() {
        let catalog = PlatformCatalog::parse(CATALOG).expect("valid catalog");
        assert_eq!(
            catalog
                .select(None, Some("qemu"), Some("riscv64gc-unknown-none-elf"))
                .expect("RISC-V QEMU")
                .id,
            "qemu-riscv64"
        );
        assert_eq!(
            catalog.select(None, None, None).expect("default QEMU").id,
            "qemu-loongarch64"
        );
    }

    #[test]
    fn explicit_platform_is_mutually_exclusive() {
        let catalog = PlatformCatalog::parse(CATALOG).expect("valid catalog");
        let error = catalog
            .select(Some("ls2k1000"), Some("ls2k1000"), None)
            .expect_err("mixed selectors must fail");
        assert!(error.to_string().contains("cannot be combined"));
    }

    #[test]
    fn rejects_unknown_fields() {
        let source = CATALOG.replacen("schema = 1", "schema = 1\nunknown = true", 1);
        assert!(PlatformCatalog::parse(&source).is_err());
    }

    #[test]
    fn rejects_noncanonical_addresses() {
        let source = CATALOG.replacen("0x0000_0000_0020_0000", "0x200000", 1);
        let error = PlatformCatalog::parse(&source).expect_err("short address must fail");
        assert!(error.to_string().contains("canonical"));
    }

    #[test]
    fn rejects_wrong_dmw_mapping() {
        let source = CATALOG.replacen("0x9000_0000_0020_0000", "0x9000_0000_0200_0000", 1);
        let error = PlatformCatalog::parse(&source).expect_err("wrong DMW mapping must fail");
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn rejects_wrong_sv48_mapping() {
        let source = CATALOG.replacen("0xffff_ff80_8020_0000", "0xffff_ffc0_8020_0000", 1);
        let error = PlatformCatalog::parse(&source).expect_err("wrong Sv48 mapping must fail");
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn rejects_unaligned_link_bases() {
        let source = CATALOG.replacen("0x0000_0000_0020_0000", "0x0000_0000_0020_0001", 1);
        let error = PlatformCatalog::parse(&source).expect_err("unaligned base must fail");
        assert!(error.to_string().contains("aligned"));
    }

    #[test]
    fn rejects_target_layout_mismatch() {
        let source = CATALOG.replacen(
            "target = \"riscv64gc-unknown-none-elf\"",
            "target = \"riscv64imac-unknown-none-elf\"",
            1,
        );
        let error = PlatformCatalog::parse(&source).expect_err("target mismatch must fail");
        assert!(error.to_string().contains("requires target"));
    }

    #[test]
    fn rejects_multiple_board_defaults() {
        let source = CATALOG.replacen("default_for_board = false", "default_for_board = true", 1);
        let error = PlatformCatalog::parse(&source).expect_err("duplicate default must fail");
        assert!(error.to_string().contains("exactly one default"));
    }

    #[test]
    fn rejects_duplicate_platform_ids() {
        let source = CATALOG.replacen("id = \"visionfive2\"", "id = \"ls2k1000\"", 1);
        let error = PlatformCatalog::parse(&source).expect_err("duplicate id must fail");
        assert!(error.to_string().contains("duplicate platform id"));
    }
}

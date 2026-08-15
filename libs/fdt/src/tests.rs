use std::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

use crate::{DTB_MAGIC, Error, Fdt, PropertyError};

const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const END: u32 = 9;

struct StructureBuilder {
    version: u32,
    structure: Vec<u8>,
    strings: Vec<u8>,
}

impl StructureBuilder {
    fn new(version: u32) -> Self {
        Self {
            version,
            structure: Vec::new(),
            strings: Vec::new(),
        }
    }

    fn begin(&mut self, name: &str) {
        push_u32(&mut self.structure, BEGIN_NODE);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        pad(&mut self.structure, 4);
    }

    fn property(&mut self, name: &str, value: &[u8]) {
        let name_offset = self.strings.len() as u32;
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);

        push_u32(&mut self.structure, PROP);
        push_u32(&mut self.structure, value.len() as u32);
        push_u32(&mut self.structure, name_offset);
        if self.version < 16 && value.len() >= 8 {
            pad(&mut self.structure, 8);
        }
        self.structure.extend_from_slice(value);
        pad(&mut self.structure, 4);
    }

    fn end_node(&mut self) {
        push_u32(&mut self.structure, END_NODE);
    }

    fn end(mut self) -> (Vec<u8>, Vec<u8>) {
        push_u32(&mut self.structure, END);
        (self.structure, self.strings)
    }
}

fn basic_blob(version: u32) -> Vec<u8> {
    let mut builder = StructureBuilder::new(version);
    builder.begin(if version < 16 { "/" } else { "" });
    builder.property("compatible", b"test,board\0");
    builder.begin(if version < 16 { "/soc" } else { "soc" });
    builder.property("device_type", b"soc\0");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    assemble(version, structure, strings, &[(0x1000, 0x200)])
}

fn assemble(
    version: u32,
    structure: Vec<u8>,
    strings: Vec<u8>,
    reservations: &[(u64, u64)],
) -> Vec<u8> {
    let header_size = match version {
        1 => 28,
        2 => 32,
        3..=16 => 36,
        _ => 40,
    };
    let mut blob = vec![0; header_size];
    pad(&mut blob, 8);
    let reserve_offset = blob.len();
    for &(address, size) in reservations {
        push_u64(&mut blob, address);
        push_u64(&mut blob, size);
    }
    push_u64(&mut blob, 0);
    push_u64(&mut blob, 0);
    pad(&mut blob, 4);
    let structure_offset = blob.len();
    blob.extend_from_slice(&structure);
    let strings_offset = blob.len();
    blob.extend_from_slice(&strings);
    let total_size = blob.len();

    set_u32(&mut blob, 0, DTB_MAGIC);
    set_u32(&mut blob, 4, total_size as u32);
    set_u32(&mut blob, 8, structure_offset as u32);
    set_u32(&mut blob, 12, strings_offset as u32);
    set_u32(&mut blob, 16, reserve_offset as u32);
    set_u32(&mut blob, 20, version);
    set_u32(&mut blob, 24, if version >= 17 { 16 } else { 1 });
    if version >= 2 {
        set_u32(&mut blob, 28, 7);
    }
    if version >= 3 {
        set_u32(&mut blob, 32, strings.len() as u32);
    }
    if version >= 17 {
        set_u32(&mut blob, 36, structure.len() as u32);
    }
    blob
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn set_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn pad(bytes: &mut Vec<u8>, alignment: usize) {
    while !bytes.len().is_multiple_of(alignment) {
        bytes.push(0);
    }
}

#[cfg(feature = "alloc")]
fn cells(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

#[test]
fn parses_every_complete_read_only_version() {
    for version in 1..=18 {
        let blob = basic_blob(version);
        let fdt = Fdt::parse(&blob).unwrap_or_else(|error| panic!("v{version}: {error:?}"));
        assert_eq!(fdt.header().version, version);
        assert_eq!(
            fdt.header().size(),
            if version == 1 {
                28
            } else if version == 2 {
                32
            } else if version < 17 {
                36
            } else {
                40
            }
        );
        assert_eq!(fdt.header().boot_cpuid_phys, (version >= 2).then_some(7));
        assert_eq!(fdt.root().name(), "");
        assert_eq!(fdt.find_node("/soc").unwrap().name(), "soc");
        assert!(fdt.find_node("soc").is_none());
        assert_eq!(
            fdt.root().property("compatible").unwrap().as_str(),
            Ok("test,board")
        );
        assert_eq!(
            fdt.reservations().collect::<Vec<_>>(),
            vec![crate::ReserveEntry {
                address: 0x1000,
                size: 0x200,
            }]
        );
    }
}

#[test]
fn legacy_property_values_use_eight_byte_alignment() {
    let mut builder = StructureBuilder::new(15);
    builder.begin("/");
    builder.property("wide", &[1, 2, 3, 4, 5, 6, 7, 8]);
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(15, structure, strings, &[]);
    let fdt = Fdt::parse(&blob).unwrap();
    let property = fdt.root().property("wide").unwrap();
    assert_eq!(property.value(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    let encoded = property.encoded_structure_range();
    assert_eq!(encoded.start, property.structure_offset());
    assert!(encoded.len().is_multiple_of(4));

    let mut nopped = blob.clone();
    let structure_offset = get_u32(&nopped, 8) as usize;
    for token in
        nopped[structure_offset + encoded.start..structure_offset + encoded.end].chunks_exact_mut(4)
    {
        token.copy_from_slice(&4u32.to_be_bytes());
    }
    let nopped = Fdt::parse(&nopped).unwrap();
    assert!(nopped.root().property("wide").is_none());

    let mut nonzero_padding = blob.clone();
    let structure_offset = get_u32(&nonzero_padding, 8) as usize;
    // Root header occupies 8 bytes; the legacy property data begins at +24,
    // with four alignment bytes immediately before it.
    nonzero_padding[structure_offset + 20] = 1;
    assert!(matches!(
        Fdt::parse_strict(&nonzero_padding),
        Err(Error::NonZeroPadding { .. })
    ));
}

#[test]
fn modern_property_does_not_apply_legacy_alignment() {
    let mut builder = StructureBuilder::new(16);
    builder.begin("");
    builder.property("wide", &[1, 2, 3, 4, 5, 6, 7, 8]);
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(16, structure, strings, &[]);
    assert_eq!(
        Fdt::parse(&blob)
            .unwrap()
            .root()
            .property("wide")
            .unwrap()
            .value(),
        &[1, 2, 3, 4, 5, 6, 7, 8]
    );

    // DTSpec 要求 token 间隙必须清零；libfdt 的历史宽松行为不改变规范输入约束。
    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("compatible", b"qemu\0");
    builder.end_node();
    let (structure, strings) = builder.end();
    let mut blob = assemble(17, structure, strings, &[]);
    let parsed = Fdt::parse(&blob).unwrap();
    let encoded = parsed
        .root()
        .property("compatible")
        .unwrap()
        .encoded_structure_range();
    let structure_offset = get_u32(&blob, 8) as usize;
    blob[structure_offset + encoded.end - 1] = b'x';
    assert!(matches!(
        Fdt::parse_strict(&blob),
        Err(Error::NonZeroPadding { .. })
    ));
}

#[test]
fn traversal_is_direct_and_paths_are_component_bounded() {
    let blob = basic_blob(17);
    let fdt = Fdt::parse(&blob).unwrap();
    assert_eq!(
        fdt.nodes()
            .map(|node| String::from(node.name()))
            .collect::<Vec<_>>(),
        vec![String::from(""), String::from("soc")]
    );
    assert_eq!(
        fdt.root()
            .children()
            .map(|node| node.name())
            .collect::<Vec<_>>(),
        vec!["soc"]
    );
    assert_eq!(fdt.root().properties().count(), 1);
    assert!(fdt.find_node("/").is_some());
    assert!(fdt.find_node("/soc/").is_none());
    assert!(fdt.find_node("//soc").is_none());
    assert!(fdt.find_node("/so").is_none());
}

#[test]
fn paths_resolve_unambiguous_omitted_unit_addresses() {
    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("soc@0");
    builder.begin("serial@1000");
    builder.end_node();
    builder.end_node();
    builder.begin("aliases");
    builder.property("serial0", b"/soc/serial\0");
    builder.end_node();
    builder.begin("chosen");
    builder.property("stdout-path", b"serial0:115200n8\0");
    builder.end_node();
    builder.begin("uart@2000");
    builder.end_node();
    builder.begin("uart@3000");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);

    let fdt = Fdt::parse(&blob).unwrap();
    assert_eq!(fdt.find_node("/soc/serial").unwrap().name(), "serial@1000");
    assert!(fdt.find_node("/uart").is_none());
    let stdout = fdt.chosen_stdout().unwrap().unwrap();
    assert_eq!(stdout.path, "/soc/serial");
    assert_eq!(stdout.node.name(), "serial@1000");

    #[cfg(feature = "alloc")]
    {
        use crate::Tree;

        let tree = Tree::parse(&blob).unwrap();
        let serial = tree.find_node("/soc/serial").unwrap();
        assert_eq!(tree.node(serial).unwrap().name(), "serial@1000");
        assert_eq!(tree.resolve_path_or_alias("serial0"), Some(serial));
        assert_eq!(tree.chosen_stdout().unwrap().unwrap().node, serial);
        assert!(tree.find_node("/uart").is_none());
    }
}

#[test]
fn property_decoders_are_explicit() {
    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("flag", &[]);
    builder.property("number", &42u32.to_be_bytes());
    builder.property("list", b"one\0two\0");
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let root = Fdt::parse(&blob).unwrap().root();
    assert_eq!(root.property("flag").unwrap().as_bool(), Ok(true));
    assert_eq!(root.property("number").unwrap().as_u32(), Ok(42));
    assert_eq!(
        root.property("list")
            .unwrap()
            .as_string_list()
            .unwrap()
            .collect::<Vec<_>>(),
        vec!["one", "two"]
    );
    assert_eq!(
        root.property("list").unwrap().as_str(),
        Err(PropertyError::MultipleStrings)
    );
}

#[test]
fn chosen_stdout_is_available_without_allocation() {
    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("aliases");
    builder.property("serial0", b"/soc/uart@1000\0");
    builder.end_node();
    builder.begin("chosen@0");
    builder.property("linux,stdout-path", b"serial0:57600e7\0");
    builder.end_node();
    builder.begin("soc");
    builder.begin("uart@1000");
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let stdout = Fdt::parse(&blob).unwrap().chosen_stdout().unwrap().unwrap();
    assert_eq!(stdout.raw, "serial0:57600e7");
    assert_eq!(stdout.path, "/soc/uart@1000");
    assert_eq!(stdout.node.name(), "uart@1000");
    assert_eq!(stdout.options, Some("57600e7"));
}

#[test]
fn chosen_bootargs_is_strict_and_supports_legacy_chosen_name() {
    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("chosen@0");
    builder.property("bootargs", b"console=ttyS0 root=/dev/vda\0");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);

    assert_eq!(
        Fdt::parse(&blob).unwrap().chosen_bootargs(),
        Ok(Some("console=ttyS0 root=/dev/vda"))
    );

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("chosen");
    builder.property("bootargs", b"missing-nul");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    assert_eq!(
        Fdt::parse(&blob).unwrap().chosen_bootargs(),
        Err(PropertyError::MissingNul)
    );
}

#[test]
fn riscv_cpu_binding_prefers_split_isa_and_decodes_cache_blocks() {
    use crate::{RiscvCpuBinding, RiscvIsaSource};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("cpus");
    builder.begin("cpu@0");
    builder.property("riscv,isa-base", b"rv64i\0");
    builder.property(
        "riscv,isa-extensions",
        b"i\0m\0a\0v\0zicbom\0zicboz\0zicbop\0sstc\0",
    );
    // legacy 字符串故意不含 V，用于证明新 binding 优先。
    builder.property("riscv,isa", b"rv64imac_zicboz\0");
    builder.property("mmu-type", b"riscv,sv57\0");
    builder.property("riscv,cbom-block-size", &64u32.to_be_bytes());
    builder.property("riscv,cboz-block-size", &128u32.to_be_bytes());
    builder.property("riscv,cbop-block-size", &32u32.to_be_bytes());
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let fdt = Fdt::parse(&blob).unwrap();
    let binding = RiscvCpuBinding::parse(fdt.find_node("/cpus/cpu@0").unwrap()).unwrap();

    assert_eq!(binding.isa_source(), RiscvIsaSource::Split);
    assert_eq!(binding.isa_base(), "rv64i");
    assert!(binding.has_isa_extension("v"));
    assert!(binding.has_isa_extension("zicbom"));
    assert!(binding.has_isa_extension("zicboz"));
    assert!(binding.has_isa_extension("zicbop"));
    assert!(binding.has_isa_extension("sstc"));
    assert_eq!(binding.mmu_type(), "riscv,sv57");
    assert_eq!(binding.cbom_block_size(), Some(64));
    assert_eq!(binding.cboz_block_size(), Some(128));
    assert_eq!(binding.cbop_block_size(), Some(32));
}

#[test]
fn riscv_cpu_binding_falls_back_to_versioned_legacy_isa() {
    use crate::{RiscvCpuBinding, RiscvIsaSource};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("cpu@0");
    builder.property(
        "riscv,isa",
        b"rv64imafdcv_zicbom1p0_zicboz1p0_zicbop1p0_sstc1p0\0",
    );
    builder.property("mmu-type", b"riscv,sv48\0");
    builder.property("riscv,cbom-block-size", &64u32.to_be_bytes());
    builder.property("riscv,cboz-block-size", &64u32.to_be_bytes());
    builder.property("riscv,cbop-block-size", &64u32.to_be_bytes());
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let fdt = Fdt::parse(&blob).unwrap();
    let binding = RiscvCpuBinding::parse(fdt.find_node("/cpu@0").unwrap()).unwrap();

    assert_eq!(binding.isa_source(), RiscvIsaSource::Legacy);
    assert_eq!(binding.isa_base(), "rv64i");
    assert!(binding.has_isa_extension("v"));
    assert!(binding.has_isa_extension("zicbom"));
    assert!(binding.has_isa_extension("zicboz"));
    assert!(binding.has_isa_extension("zicbop"));
    assert!(binding.has_isa_extension("sstc"));
}

#[test]
fn riscv_cpu_binding_rejects_partial_split_isa() {
    use crate::{RiscvCpuBinding, RiscvCpuError};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("cpu@0");
    builder.property("riscv,isa-base", b"rv64i\0");
    builder.property("mmu-type", b"riscv,sv48\0");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let fdt = Fdt::parse(&blob).unwrap();

    assert_eq!(
        RiscvCpuBinding::parse(fdt.find_node("/cpu@0").unwrap()).unwrap_err(),
        RiscvCpuError::IncompleteIsaPair
    );
}

#[test]
fn parse_uses_declared_total_size_only() {
    let mut blob = basic_blob(17);
    let declared = blob.len();
    blob.extend_from_slice(b"unrelated bytes");
    assert_eq!(Fdt::parse(&blob).unwrap().as_bytes().len(), declared);
}

#[cfg(feature = "alloc")]
#[test]
fn owned_tree_round_trips_legacy_input_as_canonical_v17() {
    use crate::{OwnedNode, OwnedTree};

    let blob = basic_blob(1);
    let mut owned = OwnedTree::from_fdt(Fdt::parse(&blob).unwrap()).unwrap();
    owned
        .find_node_mut("/soc")
        .unwrap()
        .set_property("enabled", Vec::new());
    let mut child = OwnedNode::new("device@10");
    child.set_property("reg", cells(&[0x10, 0x20]));
    owned.find_node_mut("/soc").unwrap().children.push(child);

    let encoded = owned.to_dtb().unwrap();
    let reparsed = Fdt::parse(&encoded).unwrap();
    assert_eq!(reparsed.header().version, 17);
    assert_eq!(
        reparsed.reservations().collect::<Vec<_>>(),
        owned.reservations
    );
    assert!(
        reparsed
            .find_node("/soc")
            .unwrap()
            .property("enabled")
            .is_some()
    );
    assert_eq!(
        reparsed
            .find_node("/soc/device@10")
            .unwrap()
            .property("reg")
            .unwrap()
            .value(),
        cells(&[0x10, 0x20])
    );
}

#[cfg(feature = "alloc")]
#[test]
fn dtc_overlay_applies_external_and_local_fixups_atomically() {
    use crate::{OverlayError, OwnedTree};
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    if Command::new("dtc").arg("--version").output().is_err() {
        return;
    }
    let compile = |source: &[u8]| {
        let mut child = Command::new("dtc")
            .args(["-q", "-@", "-I", "dts", "-O", "dtb", "-o", "-", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(source).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "dtc: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let base_blob = compile(
        br#"/dts-v1/;
/ {
    #address-cells = <1>;
    #size-cells = <1>;
    clock: clock@1000 {
        compatible = "fixed-clock";
        #clock-cells = <1>;
        reg = <0x1000 0x100>;
    };
    target: bus@2000 {
        compatible = "simple-bus";
        #address-cells = <1>;
        #size-cells = <1>;
        ranges = <0 0x2000 0x100>;
        status = "disabled";
    };
};
"#,
    );
    let overlay_blob = compile(
        br#"/dts-v1/;
/plugin/;
/ {
    fragment@0 {
        target = <&target>;
        __overlay__ {
            status = "okay";
            newdev: device@10 {
                reg = <0x10 0x10>;
                clocks = <&clock 3>;
            };
            consumer@20 {
                reg = <0x20 0x10>;
                link = <&newdev>;
            };
        };
    };
};
"#,
    );

    let mut base = OwnedTree::parse(&base_blob).unwrap();
    base.apply_overlay(Fdt::parse(&overlay_blob).unwrap())
        .unwrap();
    let encoded = base.to_dtb().unwrap();
    let parsed = Fdt::parse(&encoded).unwrap();
    let clock_phandle = parsed
        .find_node("/clock@1000")
        .unwrap()
        .property("phandle")
        .unwrap()
        .as_u32()
        .unwrap();
    let device = parsed.find_node("/bus@2000/device@10").unwrap();
    let device_phandle = device.property("phandle").unwrap().as_u32().unwrap();
    assert_eq!(
        parsed
            .find_node("/bus@2000")
            .unwrap()
            .property("status")
            .unwrap()
            .as_str(),
        Ok("okay")
    );
    assert_eq!(
        device
            .property("clocks")
            .unwrap()
            .cells()
            .unwrap()
            .collect::<Vec<_>>(),
        vec![clock_phandle, 3]
    );
    assert_eq!(
        parsed
            .find_node("/bus@2000/consumer@20")
            .unwrap()
            .property("link")
            .unwrap()
            .as_u32(),
        Ok(device_phandle)
    );
    assert_eq!(
        parsed
            .find_node("/__symbols__")
            .unwrap()
            .property("newdev")
            .unwrap()
            .as_str(),
        Ok("/bus@2000/device@10")
    );

    let bad_overlay = compile(
        br#"/dts-v1/;
/plugin/;
/ {
    fragment@0 {
        target = <&missing_from_base>;
        __overlay__ { marker; };
    };
};
"#,
    );
    let mut unchanged = OwnedTree::parse(&base_blob).unwrap();
    let snapshot = unchanged.clone();
    assert!(matches!(
        unchanged.apply_overlay(Fdt::parse(&bad_overlay).unwrap()),
        Err(OverlayError::UnknownSymbol(_))
    ));
    assert_eq!(unchanged, snapshot);
}

#[test]
fn rejects_header_and_version_errors() {
    assert!(matches!(
        Fdt::parse(&DTB_MAGIC.to_be_bytes()),
        Err(Error::TruncatedHeader { .. })
    ));
    let mut blob = basic_blob(17);
    set_u32(&mut blob, 0, 0);
    assert!(matches!(Fdt::parse(&blob), Err(Error::BadMagic(0))));

    let mut blob = basic_blob(17);
    set_u32(&mut blob, 20, 0);
    set_u32(&mut blob, 24, 0);
    assert!(matches!(
        Fdt::parse(&blob),
        Err(Error::UnsupportedVersion { version: 0, .. })
    ));

    let mut blob = basic_blob(18);
    set_u32(&mut blob, 24, 18);
    assert!(matches!(
        Fdt::parse(&blob),
        Err(Error::UnsupportedVersion {
            version: 18,
            last_compatible: 18,
        })
    ));
    let mut blob = basic_blob(16);
    set_u32(&mut blob, 24, 17);
    assert!(matches!(
        Fdt::parse(&blob),
        Err(Error::InvalidVersion { .. })
    ));
}

#[test]
fn every_truncation_is_reported_without_panicking() {
    let blob = basic_blob(17);
    for length in 0..blob.len() {
        assert!(
            Fdt::parse(&blob[..length]).is_err(),
            "prefix length {length}"
        );
    }
}

#[test]
fn rejects_misaligned_overlapping_and_unterminated_blocks() {
    let mut misaligned = basic_blob(17);
    let reserve = get_u32(&misaligned, 16);
    set_u32(&mut misaligned, 16, reserve + 4);
    assert!(matches!(
        Fdt::parse(&misaligned),
        Err(Error::MisalignedBlock { .. })
    ));

    let mut overlap = basic_blob(17);
    let structure = get_u32(&overlap, 8);
    set_u32(&mut overlap, 12, structure);
    assert!(matches!(
        Fdt::parse(&overlap),
        Err(Error::BlocksOverlap { .. })
    ));

    let mut unterminated = basic_blob(17);
    let reserve = get_u32(&unterminated, 16) as usize;
    for byte in &mut unterminated[reserve..] {
        *byte = 0xff;
    }
    assert!(matches!(
        Fdt::parse(&unterminated),
        Err(Error::MissingReservationTerminator { .. })
            | Err(Error::TruncatedReservation { .. })
            | Err(Error::OverlappingReservations { .. })
            | Err(Error::InvalidReservationRange { .. })
    ));
}

#[test]
fn rejects_bad_structure_order_and_names() {
    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("child");
    builder.end_node();
    builder.property("late", &[]);
    builder.end_node();
    let (structure, strings) = builder.end();
    assert!(matches!(
        Fdt::parse_strict(&assemble(17, structure, strings, &[])),
        Err(Error::PropertyAfterChild { .. })
    ));

    let mut builder = StructureBuilder::new(17);
    builder.begin("not-root");
    builder.end_node();
    let (structure, strings) = builder.end();
    assert!(matches!(
        Fdt::parse_strict(&assemble(17, structure, strings, &[])),
        Err(Error::InvalidRootName { .. })
    ));

    let mut builder = StructureBuilder::new(15);
    builder.begin("/");
    builder.begin("child-without-full-path");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    assert!(matches!(
        Fdt::parse_strict(&assemble(15, structure, strings, &[])),
        Err(Error::InvalidNodeName { .. })
    ));

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("abcdefghijklmnopqrstuvwxyzabcdef");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    assert!(matches!(
        Fdt::parse_strict(&assemble(17, structure, strings, &[])),
        Err(Error::InvalidNodeName { .. })
    ));

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("abcdefghijklmnopqrstuvwxyzabcdef", &[]);
    builder.end_node();
    let (structure, strings) = builder.end();
    assert!(matches!(
        Fdt::parse_strict(&assemble(17, structure, strings, &[])),
        Err(Error::InvalidPropertyName { .. })
    ));

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("1device@0");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    assert!(matches!(
        Fdt::parse_strict(&assemble(17, structure, strings, &[])),
        Err(Error::InvalidNodeName { .. })
    ));

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("star*property", &[]);
    builder.end_node();
    let (structure, strings) = builder.end();
    assert!(matches!(
        Fdt::parse_strict(&assemble(17, structure, strings, &[])),
        Err(Error::InvalidPropertyName { .. })
    ));
}

#[test]
fn accepts_nops_between_root_end_node_and_end() {
    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("child");
    builder.end_node();
    builder.end_node();
    push_u32(&mut builder.structure, 4);
    push_u32(&mut builder.structure, 4);
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let fdt = Fdt::parse(&blob).unwrap();
    assert_eq!(
        fdt.nodes().map(|node| node.name()).collect::<Vec<_>>(),
        vec!["", "child"]
    );
}

#[test]
fn fdt_end_is_the_exact_end_of_the_declared_structure_block() {
    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.end_node();
    let (mut structure, strings) = builder.end();
    structure.extend_from_slice(&[0; 4]);
    let blob = assemble(17, structure, strings, &[]);
    assert!(matches!(
        Fdt::parse(&blob),
        Err(Error::TrailingStructureData { length: 4, .. })
    ));
}

#[test]
fn memory_reservation_entries_must_not_overlap() {
    let overlapping = assemble(
        17,
        {
            let mut builder = StructureBuilder::new(17);
            builder.begin("");
            builder.end_node();
            builder.end().0
        },
        Vec::new(),
        &[(0x1000, 0x100), (0x1080, 0x100)],
    );
    assert!(matches!(
        Fdt::parse(&overlapping),
        Err(Error::OverlappingReservations {
            first: 0,
            second: 1,
        })
    ));

    let adjacent = assemble(
        17,
        {
            let mut builder = StructureBuilder::new(17);
            builder.begin("");
            builder.end_node();
            builder.end().0
        },
        Vec::new(),
        &[(0x1000, 0x100), (0x1100, 0x100)],
    );
    assert!(Fdt::parse(&adjacent).is_ok());

    let wrapping = assemble(
        17,
        {
            let mut builder = StructureBuilder::new(17);
            builder.begin("");
            builder.end_node();
            builder.end().0
        },
        Vec::new(),
        &[(u64::MAX, 2)],
    );
    assert!(matches!(
        Fdt::parse(&wrapping),
        Err(Error::InvalidReservationRange {
            entry: 0,
            address: u64::MAX,
            size: 2,
        })
    ));
}

#[test]
fn structure_alignment_padding_must_be_zero() {
    let mut node_builder = StructureBuilder::new(17);
    node_builder.begin("");
    let node_start = node_builder.structure.len();
    node_builder.begin("ab");
    node_builder.structure[node_start + 7] = 0x7f;
    node_builder.end_node();
    node_builder.end_node();
    let (structure, strings) = node_builder.end();
    assert!(matches!(
        Fdt::parse_strict(&assemble(17, structure, strings, &[])),
        Err(Error::NonZeroPadding { .. })
    ));

    let mut property_builder = StructureBuilder::new(17);
    property_builder.begin("");
    let property_start = property_builder.structure.len();
    property_builder.property("byte", &[0xaa]);
    property_builder.structure[property_start + 13] = 0x55;
    property_builder.end_node();
    let (structure, strings) = property_builder.end();
    assert!(matches!(
        Fdt::parse_strict(&assemble(17, structure, strings, &[])),
        Err(Error::NonZeroPadding { .. })
    ));

    let mut legacy_builder = StructureBuilder::new(15);
    legacy_builder.begin("/");
    let property_start = legacy_builder.structure.len();
    legacy_builder.property("wide", &[0; 8]);
    // FDT_PROP token + len/nameoff 后从 20 对齐到 24，四个旧格式 padding 字节。
    legacy_builder.structure[property_start + 12] = 0x33;
    legacy_builder.end_node();
    let (structure, strings) = legacy_builder.end();
    assert!(matches!(
        Fdt::parse_strict(&assemble(15, structure, strings, &[])),
        Err(Error::NonZeroPadding { .. })
    ));
}

#[test]
fn version_17_accepts_noncanonical_block_order() {
    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("compatible", b"order,test\0");
    builder.end_node();
    let (structure, strings) = builder.end();

    let mut blob = vec![0; 40];
    let strings_offset = blob.len();
    blob.extend_from_slice(&strings);
    pad(&mut blob, 8);
    let reserve_offset = blob.len();
    push_u64(&mut blob, 0);
    push_u64(&mut blob, 0);
    pad(&mut blob, 4);
    let structure_offset = blob.len();
    blob.extend_from_slice(&structure);
    let total = blob.len();
    set_u32(&mut blob, 0, DTB_MAGIC);
    set_u32(&mut blob, 4, total as u32);
    set_u32(&mut blob, 8, structure_offset as u32);
    set_u32(&mut blob, 12, strings_offset as u32);
    set_u32(&mut blob, 16, reserve_offset as u32);
    set_u32(&mut blob, 20, 17);
    set_u32(&mut blob, 24, 16);
    set_u32(&mut blob, 28, 0);
    set_u32(&mut blob, 32, strings.len() as u32);
    set_u32(&mut blob, 36, structure.len() as u32);
    assert_eq!(
        Fdt::parse(&blob)
            .unwrap()
            .root()
            .property("compatible")
            .unwrap()
            .as_str(),
        Ok("order,test")
    );
}

#[test]
fn version_3_accepts_noncanonical_block_order() {
    let mut builder = StructureBuilder::new(3);
    builder.begin("/");
    builder.property("compatible", b"old-order,test\0");
    builder.end_node();
    let (structure, strings) = builder.end();

    let mut blob = vec![0; 36];
    let strings_offset = blob.len();
    blob.extend_from_slice(&strings);
    pad(&mut blob, 8);
    let reserve_offset = blob.len();
    push_u64(&mut blob, 0);
    push_u64(&mut blob, 0);
    pad(&mut blob, 4);
    let structure_offset = blob.len();
    blob.extend_from_slice(&structure);
    let total = blob.len();
    set_u32(&mut blob, 0, DTB_MAGIC);
    set_u32(&mut blob, 4, total as u32);
    set_u32(&mut blob, 8, structure_offset as u32);
    set_u32(&mut blob, 12, strings_offset as u32);
    set_u32(&mut blob, 16, reserve_offset as u32);
    set_u32(&mut blob, 20, 3);
    set_u32(&mut blob, 24, 1);
    set_u32(&mut blob, 28, 0);
    set_u32(&mut blob, 32, strings.len() as u32);
    assert_eq!(
        Fdt::parse(&blob)
            .unwrap()
            .root()
            .property("compatible")
            .unwrap()
            .as_str(),
        Ok("old-order,test")
    );
}

#[test]
fn duplicate_node_and_property_names_are_rejected() {
    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("same", &[]);
    builder.property("same", &[]);
    builder.end_node();
    let (structure, strings) = builder.end();
    let property_duplicates = assemble(17, structure, strings, &[]);
    assert_eq!(
        Fdt::parse(&property_duplicates)
            .unwrap()
            .root()
            .properties()
            .count(),
        2
    );
    #[cfg(feature = "alloc")]
    assert!(matches!(
        crate::Tree::parse(&property_duplicates),
        Err(crate::TreeError::InvalidFdt(
            Error::DuplicatePropertyName { .. }
        ))
    ));

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("same");
    builder.end_node();
    builder.begin("same");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let node_duplicates = assemble(17, structure, strings, &[]);
    assert_eq!(
        Fdt::parse(&node_duplicates)
            .unwrap()
            .root()
            .children()
            .count(),
        2
    );
    #[cfg(feature = "alloc")]
    assert!(matches!(
        crate::Tree::parse(&node_duplicates),
        Err(crate::TreeError::InvalidFdt(
            Error::DuplicateNodeName { .. }
        ))
    ));
}

#[cfg(feature = "alloc")]
#[test]
fn unitless_child_names_must_not_collide_with_parent_properties() {
    use crate::{Tree, TreeError};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("mailbox", &[]);
    builder.begin("mailbox");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    assert!(Fdt::parse(&blob).is_ok());
    assert!(matches!(
        Tree::parse(&blob),
        Err(TreeError::InvalidFdt(
            Error::NodePropertyNameConflict { .. }
        ))
    ));

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("mailbox", &[]);
    builder.begin("mailbox@0");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    assert!(Tree::parse(&assemble(17, structure, strings, &[])).is_ok());
}

#[test]
fn parses_dtc_generated_legacy_and_modern_blobs_when_dtc_is_available() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    if Command::new("dtc").arg("--version").output().is_err() {
        return;
    }
    let source = br#"/dts-v1/;
/memreserve/ 0x1000 0x100;
/ {
    compatible = "dtc,interop";
    #address-cells = <2>;
    #size-cells = <1>;
    soc {
        compatible = "simple-bus";
        question?property;
    };
};
"#;
    for version in [1u32, 2, 3, 16, 17] {
        let mut child = Command::new("dtc")
            .args(["-q", "-I", "dts", "-O", "dtb", "-V"])
            .arg(version.to_string())
            .args(["-o", "-", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(source).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "dtc v{version}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let fdt = Fdt::parse(&output.stdout)
            .unwrap_or_else(|error| panic!("dtc v{version} output: {error:?}"));
        assert_eq!(fdt.header().version, version);
        assert_eq!(
            fdt.find_node("/soc")
                .unwrap()
                .property("question?property")
                .unwrap()
                .as_bool(),
            Ok(true)
        );
    }
}

#[cfg(feature = "alloc")]
fn semantic_blob(include_ranges: bool, empty_ranges: bool) -> Vec<u8> {
    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[2]));
    builder.property("#size-cells", &cells(&[2]));

    builder.begin("aliases");
    builder.property("serial0", b"/soc/serial@1000\0");
    builder.end_node();

    builder.begin("chosen");
    builder.property("stdout-path", b"serial0:115200n8r\0");
    builder.end_node();

    builder.begin("soc");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    if include_ranges {
        let value = if empty_ranges {
            Vec::new()
        } else {
            cells(&[0, 0, 0x4000_0000, 0x1_0000])
        };
        builder.property("ranges", &value);
    }

    builder.begin("serial@1000");
    builder.property("compatible", b"ns16550a\0vendor,uart\0");
    builder.property("reg", &cells(&[0x1000, 0x100]));
    builder.property("status", b"okay\0");
    builder.property("phandle", &cells(&[42]));
    builder.end_node();

    builder.begin("off@2000");
    builder.property("status", b"disabled\0");
    builder.end_node();
    builder.end_node();

    builder.end_node();
    let (structure, strings) = builder.end();
    assemble(17, structure, strings, &[])
}

#[cfg(feature = "alloc")]
#[test]
fn indexed_tree_covers_paths_aliases_phandles_status_and_chosen() {
    use crate::{NodeStatus, Tree};

    let blob = semantic_blob(true, false);
    let tree = Tree::parse(&blob).unwrap();
    let serial = tree.find_node("/soc/serial@1000").unwrap();
    assert_eq!(tree.path(serial).as_deref(), Some("/soc/serial@1000"));
    assert_eq!(tree.resolve_path_or_alias("serial0"), Some(serial));
    assert_eq!(tree.alias_path("serial0"), Some("/soc/serial@1000"));
    assert_eq!(tree.node_by_phandle(42), Some(serial));
    assert_eq!(tree.phandle(serial), Some(42));
    assert_eq!(tree.status(serial), Ok(NodeStatus::Okay));
    assert_eq!(tree.is_available(serial), Ok(true));
    let off = tree.find_node("/soc/off@2000").unwrap();
    assert_eq!(tree.status(off), Ok(NodeStatus::Disabled));
    assert_eq!(tree.is_available(off), Ok(false));

    let stdout = tree.chosen_stdout().unwrap().unwrap();
    assert_eq!(stdout.raw, "serial0:115200n8r");
    assert_eq!(stdout.path, "/soc/serial@1000");
    assert_eq!(stdout.node, serial);
    assert_eq!(stdout.options, Some("115200n8r"));
}

#[cfg(feature = "alloc")]
#[test]
fn indexed_tree_ancestor_stack_tracks_nested_siblings_once() {
    use crate::Tree;

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("left");
    builder.begin("left-child");
    builder.begin("left-grandchild");
    builder.end_node();
    builder.end_node();
    builder.end_node();
    builder.begin("right");
    builder.begin("right-child");
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();

    let root = tree.root_id();
    let left = tree.find_node("/left").unwrap();
    let left_child = tree.find_node("/left/left-child").unwrap();
    let left_grandchild = tree.find_node("/left/left-child/left-grandchild").unwrap();
    let right = tree.find_node("/right").unwrap();
    let right_child = tree.find_node("/right/right-child").unwrap();

    assert_eq!(tree.parent(left), Some(root));
    assert_eq!(tree.parent(left_child), Some(left));
    assert_eq!(tree.parent(left_grandchild), Some(left_child));
    assert_eq!(tree.parent(right), Some(root));
    assert_eq!(tree.parent(right_child), Some(right));
    assert_eq!(tree.children(root), Some([left, right].as_slice()));
    assert_eq!(tree.node_ids().count(), 6);
}

#[cfg(feature = "alloc")]
#[test]
fn deeply_nested_tree_builds_paths_on_demand() {
    use crate::Tree;

    const DEPTH: usize = 1_500;
    const NAME: &str = "node-with-thirty-one-char-value";

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    for _ in 0..DEPTH {
        builder.begin(NAME);
    }
    for _ in 0..DEPTH {
        builder.end_node();
    }
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();

    assert_eq!(tree.len(), DEPTH + 1);
    let deepest = tree.node_ids().last().unwrap();
    let path = tree.path(deepest).unwrap();
    assert_eq!(path.len(), DEPTH * (NAME.len() + 1));
    assert_eq!(tree.find_node(&path), Some(deepest));
}

#[cfg(feature = "alloc")]
#[test]
fn ordinary_reg_and_ranges_translation_is_lossless() {
    use crate::{AddressRange, RangeMapping, RegEntry, Tree};

    let blob = semantic_blob(true, false);
    let tree = Tree::parse(&blob).unwrap();
    let soc = tree.find_node("/soc").unwrap();
    let serial = tree.find_node("/soc/serial@1000").unwrap();
    assert_eq!(
        tree.reg(serial).unwrap(),
        vec![RegEntry {
            address: 0x1000,
            size: Some(0x100),
        }]
    );
    assert_eq!(
        tree.ranges(soc).unwrap(),
        Some(vec![RangeMapping {
            child_address: 0,
            parent_address: 0x4000_0000,
            size: Some(0x1_0000),
        }])
    );
    assert_eq!(
        tree.translated_reg(serial).unwrap(),
        vec![AddressRange {
            address: 0x4000_1000,
            size: Some(0x100),
        }]
    );
}

#[cfg(feature = "alloc")]
#[test]
fn missing_and_empty_ranges_have_distinct_semantics() {
    use crate::{AddressError, Tree};

    let blob = semantic_blob(false, false);
    let tree = Tree::parse(&blob).unwrap();
    let soc = tree.find_node("/soc").unwrap();
    let serial = tree.find_node("/soc/serial@1000").unwrap();
    assert_eq!(tree.ranges(soc).unwrap(), None);
    assert_eq!(
        tree.translated_reg(serial),
        Err(AddressError::MissingRanges(soc))
    );

    let blob = semantic_blob(true, true);
    let tree = Tree::parse(&blob).unwrap();
    let soc = tree.find_node("/soc").unwrap();
    let serial = tree.find_node("/soc/serial@1000").unwrap();
    assert_eq!(tree.ranges(soc).unwrap(), Some(Vec::new()));
    assert_eq!(tree.translated_reg(serial).unwrap()[0].address, 0x1000);
}

#[cfg(feature = "alloc")]
#[test]
fn range_end_is_exclusive_for_zero_length_translation() {
    use crate::{AddressError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.begin("bus");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.property("ranges", &cells(&[0x10, 0x100, 0x10]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let bus = tree.find_node("/bus").unwrap();

    for size in [None, Some(0)] {
        assert_eq!(
            tree.translate_address(bus, 0x20, size),
            Err(AddressError::UnmappedAddress {
                bus,
                address: 0x20,
                size,
            })
        );
    }
    assert_eq!(tree.translate_address(bus, 0x1f, None), Ok(0x10f));
}

#[cfg(feature = "alloc")]
#[test]
fn translated_addresses_must_fit_each_parent_cell_width() {
    use crate::{AddressError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.begin("bus");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.property("ranges", &cells(&[0, 0xffff_fff0, 0x100]));
    builder.end_node();
    builder.begin("identity-bus");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.property("ranges", &[]);
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let root = tree.root_id();
    let bus = tree.find_node("/bus").unwrap();
    let identity_bus = tree.find_node("/identity-bus").unwrap();

    assert_eq!(
        tree.translate_address(bus, 0x20, Some(1)),
        Err(AddressError::AddressOutOfRange {
            bus: root,
            address: 0xffff_fff0,
            size: Some(0x100),
            cells: 1,
        })
    );

    assert_eq!(
        tree.translate_address(identity_bus, 0xffff_fff0, Some(0x20)),
        Err(AddressError::AddressOutOfRange {
            bus: identity_bus,
            address: 0xffff_fff0,
            size: Some(0x20),
            cells: 1,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn zero_size_cells_remain_absent_in_reg_and_ranges() {
    use crate::{RangeMapping, RegEntry, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[0]));
    builder.begin("bus");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[0]));
    builder.property("ranges", &cells(&[0x10, 0x20]));
    builder.begin("device@10");
    builder.property("reg", &cells(&[0x10]));
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let bus = tree.find_node("/bus").unwrap();
    let device = tree.find_node("/bus/device@10").unwrap();
    assert_eq!(
        tree.reg(device).unwrap(),
        vec![RegEntry {
            address: 0x10,
            size: None,
        }]
    );
    assert_eq!(
        tree.ranges(bus).unwrap(),
        Some(vec![RangeMapping {
            child_address: 0x10,
            parent_address: 0x20,
            size: None,
        }])
    );
    assert_eq!(tree.translated_reg(device).unwrap()[0].address, 0x20);
}

#[cfg(feature = "alloc")]
#[test]
fn duplicate_phandles_are_rejected() {
    use crate::{Tree, TreeError};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("a");
    builder.property("phandle", &cells(&[1]));
    builder.end_node();
    builder.begin("b");
    builder.property("linux,phandle", &cells(&[1]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    assert!(matches!(
        Tree::parse(&blob),
        Err(TreeError::DuplicatePhandle { value: 1, .. })
    ));
}

#[cfg(feature = "alloc")]
#[test]
fn arbitrary_reg_layout_properties_share_address_translation() {
    use crate::{AddressError, AddressRange, RegEntry, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[2]));
    builder.property("#size-cells", &cells(&[1]));
    builder.begin("bus");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.property("ranges", &cells(&[0, 0, 0x8000_0000, 0x1_0000]));
    builder.begin("device@1000");
    builder.property("vendor,windows", &cells(&[0x1000, 0x200]));
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let device = tree.find_node("/bus/device@1000").unwrap();

    assert_eq!(
        tree.reg_property(device, "vendor,windows").unwrap(),
        vec![RegEntry {
            address: 0x1000,
            size: Some(0x200),
        }]
    );
    assert_eq!(
        tree.translated_reg_property(device, "vendor,windows")
            .unwrap(),
        vec![AddressRange {
            address: 0x8000_1000,
            size: Some(0x200),
        }]
    );
    assert_eq!(
        tree.reg_property(device, "vendor,absent").unwrap(),
        Vec::new()
    );
    assert_eq!(tree.reg(tree.root_id()).unwrap(), Vec::new());
    assert_eq!(tree.translated_reg(tree.root_id()).unwrap(), Vec::new());
    assert_eq!(tree.ranges(tree.root_id()).unwrap(), None);

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.begin("broken");
    builder.property("vendor,windows", &cells(&[1]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let broken = tree.find_node("/broken").unwrap();
    assert_eq!(
        tree.reg_property(broken, "vendor,windows"),
        Err(AddressError::IncompleteEntry {
            node: broken,
            property: "vendor,windows",
            cells: 1,
            cells_per_entry: 2,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn memory_description_preserves_every_firmware_source() {
    use crate::{PhysicalRange, ReservedMemoryPlacement, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[2]));
    builder.property("#size-cells", &cells(&[2]));

    builder.begin("memory@0");
    builder.property("device_type", b"memory\0");
    builder.property("reg", &cells(&[0, 0, 0, 0x8000_0000, 1, 0, 0, 0x4000_0000]));
    builder.property(
        "linux,usable-memory",
        &cells(&[0, 0x1000_0000, 0, 0x2000_0000]),
    );
    builder.property("hotpluggable", &[]);
    builder.end_node();

    builder.begin("memory@90000000");
    builder.property("reg", &cells(&[0, 0x9000_0000, 0, 0x1000]));
    builder.end_node();

    builder.begin("memory@a0000000");
    builder.property("device_type", b"memory\0");
    builder.property("reg", &cells(&[0, 0xa000_0000, 0, 0x1000]));
    builder.property("status", b"disabled\0");
    builder.property("hotpluggable", &[1]);
    builder.end_node();

    builder.begin("bus");
    builder.begin("memory@b0000000");
    builder.property("device_type", b"memory\0");
    builder.property("reg", &cells(&[0, 0xb000_0000, 0, 0x1000]));
    builder.end_node();
    builder.end_node();

    builder.begin("chosen@0");
    builder.property(
        "linux,usable-memory-range",
        &cells(&[0, 0x1000_0000, 0, 0x0100_0000, 1, 0, 0, 0x0200_0000]),
    );
    builder.end_node();

    builder.begin("reserved-memory");
    builder.property("#address-cells", &cells(&[2]));
    builder.property("#size-cells", &cells(&[2]));
    builder.property("ranges", &cells(&[0, 0, 0, 0x8000_0000, 0, 0x1000_0000]));

    // 已部署的 LS2K1000 固件使用无 unit-address 的静态 framebuffer 节点。
    // Linux 仍按 reg 处理；解析器必须保留同样的兼容行为。
    builder.begin("framebuffer");
    builder.property("reg", &cells(&[0, 0x2000, 0, 0x1000]));
    builder.property("size", &[1]);
    builder.property("compatible", b"vendor,fb\0shared-dma-pool\0");
    builder.property("phandle", &cells(&[7]));
    builder.property("no-map", &[]);
    builder.end_node();

    builder.begin("video-pool");
    builder.property("size", &cells(&[0, 0x2000]));
    builder.property("alignment", &cells(&[0, 0x1000]));
    builder.property("alloc-ranges", &cells(&[0, 0x4000, 0, 0x8000]));
    builder.property("reusable", &[]);
    builder.end_node();

    builder.begin("ignored");
    builder.property("status", b"disabled\0");
    builder.end_node();
    builder.end_node();

    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(
        17,
        structure,
        strings,
        &[(0x80, 0x20), (0x1_0000_0000, 0x3000)],
    );
    let tree = Tree::parse(&blob).unwrap();
    let description = tree.memory_description().unwrap();

    assert_eq!(description.memory_banks.len(), 1);
    let memory = tree.find_node("/memory@0").unwrap();
    assert_eq!(description.memory_banks[0].node, memory);
    assert!(description.memory_banks[0].hotpluggable);
    assert_eq!(
        description.memory_banks[0].ranges,
        vec![PhysicalRange {
            address: 0x1000_0000,
            size: 0x2000_0000,
        }]
    );
    assert_eq!(
        description.chosen_usable_ranges,
        vec![
            PhysicalRange {
                address: 0x1000_0000,
                size: 0x0100_0000,
            },
            PhysicalRange {
                address: 0x1_0000_0000,
                size: 0x0200_0000,
            },
        ]
    );
    assert_eq!(
        description.reservation_block_ranges,
        vec![
            PhysicalRange {
                address: 0x80,
                size: 0x20,
            },
            PhysicalRange {
                address: 0x1_0000_0000,
                size: 0x3000,
            },
        ]
    );

    assert_eq!(description.reserved_memory.len(), 2);
    let static_region = &description.reserved_memory[0];
    assert_eq!(static_region.path, "/reserved-memory/framebuffer");
    assert_eq!(static_region.purpose, "framebuffer");
    assert_eq!(static_region.phandle, Some(7));
    assert_eq!(
        static_region.compatible,
        vec!["vendor,fb".to_string(), "shared-dma-pool".to_string()]
    );
    assert!(static_region.no_map);
    assert!(!static_region.reusable);
    assert_eq!(
        static_region.placement,
        ReservedMemoryPlacement::Static(vec![PhysicalRange {
            address: 0x8000_2000,
            size: 0x1000,
        }])
    );

    let dynamic_region = &description.reserved_memory[1];
    assert_eq!(dynamic_region.purpose, "video-pool");
    assert!(!dynamic_region.no_map);
    assert!(dynamic_region.reusable);
    assert_eq!(
        dynamic_region.placement,
        ReservedMemoryPlacement::Dynamic {
            size: 0x2000,
            alignment: Some(0x1000),
            alloc_ranges: vec![PhysicalRange {
                address: 0x8000_4000,
                size: 0x8000,
            }],
        }
    );
}

#[cfg(feature = "alloc")]
#[test]
fn memory_ranges_retain_four_cell_u128_values() {
    use crate::{PhysicalRange, ReservedMemoryPlacement, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[4]));
    builder.property("#size-cells", &cells(&[4]));
    builder.begin("memory@0");
    builder.property("device_type", b"memory\0");
    builder.property(
        "reg",
        &cells(&[
            0x0123_4567,
            0x89ab_cdef,
            0xfedc_ba98,
            0x7654_3210,
            0x1111_1111,
            0x2222_2222,
            0x3333_3333,
            0x4444_4444,
        ]),
    );
    builder.end_node();
    builder.begin("reserved-memory");
    builder.property("#address-cells", &cells(&[4]));
    builder.property("#size-cells", &cells(&[4]));
    builder.property("ranges", &[]);
    builder.begin("pool");
    builder.property(
        "size",
        &cells(&[0xaaaa_aaaa, 0xbbbb_bbbb, 0xcccc_cccc, 0xdddd_dddd]),
    );
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let description = Tree::parse(&blob).unwrap().memory_description().unwrap();

    assert_eq!(
        description.memory_banks[0].ranges,
        vec![PhysicalRange {
            address: 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210,
            size: 0x1111_1111_2222_2222_3333_3333_4444_4444,
        }]
    );
    assert_eq!(
        description.reserved_memory[0].placement,
        ReservedMemoryPlacement::Dynamic {
            size: 0xaaaa_aaaa_bbbb_bbbb_cccc_cccc_dddd_dddd,
            alignment: None,
            alloc_ranges: Vec::new(),
        }
    );
}

#[cfg(feature = "alloc")]
#[test]
fn arbitrary_width_reg_and_ranges_have_a_lossless_path() {
    use crate::{AddressError, CellValue, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[5]));
    builder.property("#size-cells", &cells(&[5]));
    builder.begin("bus");
    builder.property("#address-cells", &cells(&[5]));
    builder.property("#size-cells", &cells(&[5]));
    builder.property(
        "ranges",
        &cells(&[
            1, 0, 0, 0, 0, // child base
            2, 0, 0, 0, 0, // parent base
            0, 0, 0, 0, 0x1000, // size
        ]),
    );
    builder.begin("device@20");
    builder.property(
        "reg",
        &cells(&[
            1, 0, 0, 0, 0x20, // address
            0, 0, 0, 0, 0x40, // size
        ]),
    );
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let device = tree.find_node("/bus/device@20").unwrap();

    assert!(matches!(
        tree.reg(device),
        Err(AddressError::UnsupportedCellCount { count: 5, .. })
    ));
    let raw = tree.reg_cells(device).unwrap();
    assert_eq!(raw[0].address.cells(), &[1, 0, 0, 0, 0x20]);
    assert_eq!(raw[0].size.as_ref().unwrap().to_u128(), Some(0x40));

    let translated = tree.translated_reg_cells(device).unwrap();
    assert_eq!(
        translated[0].address,
        CellValue::from_cells(&[2, 0, 0, 0, 0x20])
    );
}

#[cfg(feature = "alloc")]
#[test]
fn malformed_memory_bindings_report_structured_errors() {
    use crate::{MemoryError, PropertyError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.begin("memory@0");
    builder.property("device_type", b"memory\0");
    builder.property("reg", &cells(&[0, 0x1000]));
    builder.property("hotpluggable", &[1]);
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let memory = tree.find_node("/memory@0").unwrap();
    assert_eq!(
        tree.memory_description(),
        Err(MemoryError::InvalidProperty {
            node: memory,
            property: "hotpluggable",
            error: PropertyError::InvalidLength {
                actual: 1,
                expected: Some(0),
            },
        })
    );

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.begin("reserved-memory");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.property("ranges", &[]);
    builder.begin("pool");
    builder.property("size", &cells(&[0x1000]));
    builder.property("no-map", &[]);
    builder.property("reusable", &[]);
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let pool = tree.find_node("/reserved-memory/pool").unwrap();
    assert_eq!(
        tree.memory_description(),
        Err(MemoryError::MutuallyExclusiveProperties {
            node: pool,
            first: "no-map",
            second: "reusable",
        })
    );

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[2]));
    builder.begin("reserved-memory");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[2]));
    builder.property("ranges", &[]);
    builder.begin("pool");
    builder.property("size", &cells(&[0x1000]));
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let pool = tree.find_node("/reserved-memory/pool").unwrap();
    assert_eq!(
        tree.memory_description(),
        Err(MemoryError::InvalidProperty {
            node: pool,
            property: "size",
            error: PropertyError::InvalidLength {
                actual: 4,
                expected: Some(8),
            },
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn memory_and_reserved_nodes_require_normative_identity_and_placement() {
    use crate::{MemoryError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.begin("ram@0");
    builder.property("device_type", b"memory\0");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let ram = tree.find_node("/ram@0").unwrap();
    assert_eq!(
        tree.memory_description(),
        Err(MemoryError::InvalidMemoryUnitName {
            node: ram,
            name: "ram@0".to_string(),
        })
    );

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.begin("reserved-memory");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.property("ranges", &[]);
    builder.begin("pool");
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let pool = tree.find_node("/reserved-memory/pool").unwrap();
    assert_eq!(
        tree.memory_description(),
        Err(MemoryError::MissingProperty {
            node: pool,
            property: "size",
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn memory_description_allows_no_ram_and_ignores_disabled_reserved_subtrees() {
    use crate::Tree;

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("memory@0");
    builder.property("reg", &cells(&[0, 0, 0x1000]));
    builder.end_node();
    builder.begin("reserved-memory");
    builder.property("status", b"disabled\0");
    builder.begin("malformed-child");
    builder.property("no-map", &[1]);
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let description = Tree::parse(&blob).unwrap().memory_description().unwrap();

    assert!(description.memory_banks.is_empty());
    assert!(description.chosen_usable_ranges.is_empty());
    assert!(description.reservation_block_ranges.is_empty());
    assert!(description.reserved_memory.is_empty());
}

#[cfg(feature = "alloc")]
#[test]
fn reserved_boolean_properties_must_have_empty_values() {
    use crate::{MemoryError, PropertyError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.begin("reserved-memory");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.property("ranges", &[]);
    builder.begin("pool");
    builder.property("size", &cells(&[0x1000]));
    builder.property("no-map", &[0]);
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let pool = tree.find_node("/reserved-memory/pool").unwrap();

    assert_eq!(
        tree.memory_description(),
        Err(MemoryError::InvalidProperty {
            node: pool,
            property: "no-map",
            error: PropertyError::InvalidLength {
                actual: 1,
                expected: Some(0),
            },
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn interrupts_resolve_implicit_provider_and_preserve_cell_width() {
    use crate::Tree;

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("intc");
    builder.property("#interrupt-cells", &cells(&[3]));
    builder.property("interrupt-controller", &[]);
    builder.begin("device@0");
    builder.property("interrupts", &cells(&[0, 37, 4, 0, 38, 1]));
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let provider = tree.find_node("/intc").unwrap();
    let device = tree.find_node("/intc/device@0").unwrap();
    let decoded = tree.interrupts(device).unwrap().unwrap();

    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].provider, provider);
    assert_eq!(decoded[0].phandle, None);
    assert_eq!(decoded[0].cells, vec![0, 37, 4]);
    assert_eq!(decoded[1].cells, vec![0, 38, 1]);
}

#[cfg(feature = "alloc")]
#[test]
fn interrupt_controller_must_use_boolean_encoding() {
    use crate::{InterruptError, PropertyError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("intc");
    builder.property("phandle", &cells(&[1]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-controller", &cells(&[1]));
    builder.end_node();
    builder.begin("device");
    builder.property("interrupt-parent", &cells(&[1]));
    builder.property("interrupts", &cells(&[5]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let intc = tree.find_node("/intc").unwrap();
    let device = tree.find_node("/device").unwrap();

    assert_eq!(
        tree.interrupts(device),
        Err(InterruptError::InvalidProperty {
            node: intc,
            property: "interrupt-controller",
            error: PropertyError::InvalidLength {
                actual: 4,
                expected: Some(0),
            },
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn zero_cell_interrupt_specifiers_work_in_delimited_bindings() {
    use crate::{InterruptError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("zero-intc");
    builder.property("phandle", &cells(&[1]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[0]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();
    builder.begin("one-intc");
    builder.property("phandle", &cells(&[2]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();
    builder.begin("nexus");
    builder.property("phandle", &cells(&[3]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-map", &cells(&[5, 1]));
    builder.end_node();
    builder.begin("bad-nexus");
    builder.property("phandle", &cells(&[4]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-map", &cells(&[5, 1, 6, 0xdead]));
    builder.end_node();
    builder.begin("extended-device");
    builder.property("interrupts-extended", &cells(&[1, 2, 9]));
    builder.end_node();
    builder.begin("bad-extended-device");
    builder.property("interrupt-parent", &cells(&[2]));
    builder.property("interrupts", &cells(&[7]));
    builder.property("interrupts-extended", &cells(&[1, 0xdead]));
    builder.end_node();
    builder.begin("mapped-device");
    builder.property("interrupt-parent", &cells(&[3]));
    builder.property("interrupts", &cells(&[5]));
    builder.end_node();
    builder.begin("bad-mapped-device");
    builder.property("interrupt-parent", &cells(&[4]));
    builder.property("interrupts", &cells(&[5]));
    builder.end_node();
    builder.begin("plain-device");
    builder.property("interrupt-parent", &cells(&[1]));
    builder.property("interrupts", &[]);
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let zero_intc = tree.find_node("/zero-intc").unwrap();
    let one_intc = tree.find_node("/one-intc").unwrap();

    let extended_device = tree.find_node("/extended-device").unwrap();
    let decoded = tree.interrupts(extended_device).unwrap().unwrap();
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].provider, zero_intc);
    assert!(decoded[0].cells.is_empty());
    assert_eq!(decoded[1].provider, one_intc);
    assert_eq!(decoded[1].cells, vec![9]);

    let mapped_device = tree.find_node("/mapped-device").unwrap();
    let decoded = tree.interrupts(mapped_device).unwrap().unwrap();
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].provider, zero_intc);
    assert!(decoded[0].cells.is_empty());

    let bad_extended_device = tree.find_node("/bad-extended-device").unwrap();
    assert_eq!(
        tree.interrupts(bad_extended_device),
        Err(InterruptError::UnknownPhandle {
            node: bad_extended_device,
            property: "interrupts-extended",
            entry: 1,
            phandle: 0xdead,
        })
    );

    let bad_nexus = tree.find_node("/bad-nexus").unwrap();
    let bad_mapped_device = tree.find_node("/bad-mapped-device").unwrap();
    assert_eq!(
        tree.interrupts(bad_mapped_device),
        Err(InterruptError::UnknownPhandle {
            node: bad_nexus,
            property: "interrupt-map",
            entry: 1,
            phandle: 0xdead,
        })
    );

    let plain_device = tree.find_node("/plain-device").unwrap();
    assert_eq!(
        tree.interrupts(plain_device),
        Err(InterruptError::InvalidInterruptCells {
            provider: zero_intc,
            cells: 0,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn malformed_interrupts_extended_never_exposes_a_prefix_or_fallback() {
    use crate::{InterruptError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("intc");
    builder.property("phandle", &cells(&[7]));
    builder.property("#interrupt-cells", &cells(&[2]));
    builder.end_node();
    builder.begin("device@0");
    builder.property("interrupts", &cells(&[99, 1]));
    builder.property(
        "interrupts-extended",
        &cells(&[7, 33, 4, 7, 34, 4, 0xdead, 35, 4]),
    );
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let device = tree.find_node("/device@0").unwrap();

    assert_eq!(
        tree.interrupts(device),
        Err(InterruptError::UnknownPhandle {
            node: device,
            property: "interrupts-extended",
            entry: 2,
            phandle: 0xdead,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn interrupt_maps_recursively_translate_addresses_masks_and_extended_entries() {
    use crate::Tree;

    let mut builder = StructureBuilder::new(17);
    builder.begin("");

    builder.begin("root-intc");
    builder.property("phandle", &cells(&[3]));
    builder.property("#address-cells", &cells(&[2]));
    builder.property("#interrupt-cells", &cells(&[3]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();

    builder.begin("second-nexus");
    builder.property("phandle", &cells(&[2]));
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#interrupt-cells", &cells(&[2]));
    builder.property(
        "interrupt-map",
        &cells(&[0x77, 0xaa, 0xbb, 3, 0x10, 0x20, 0, 55, 4]),
    );
    builder.end_node();

    builder.begin("first-nexus");
    builder.property("phandle", &cells(&[1]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-map-mask", &cells(&[0xffff_0000, 0, 0x7]));
    builder.property(
        "interrupt-map",
        &cells(&[0x1234_0000, 0, 3, 2, 0x77, 0xaa, 0xbb]),
    );
    builder.end_node();

    builder.begin("device@1234abcd");
    builder.property("reg", &cells(&[0x1234_abcd, 0xdead_beef]));
    builder.property("interrupt-parent", &cells(&[1]));
    builder.property("interrupts", &cells(&[0xb]));
    builder.end_node();

    builder.begin("extended-device@1234abcd");
    builder.property("reg", &cells(&[0x1234_abcd, 0xfeed_face]));
    builder.property("interrupts-extended", &cells(&[1, 0xb]));
    builder.end_node();

    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let first_nexus = tree.find_node("/first-nexus").unwrap();
    let root_intc = tree.find_node("/root-intc").unwrap();
    let device = tree.find_node("/device@1234abcd").unwrap();
    assert_eq!(tree.interrupt_provider(device), Ok(Some(first_nexus)));

    for path in ["/device@1234abcd", "/extended-device@1234abcd"] {
        let device = tree.find_node(path).unwrap();
        let decoded = tree.interrupts(device).unwrap().unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].provider, root_intc);
        assert_eq!(decoded[0].phandle, Some(3));
        assert_eq!(decoded[0].cells, vec![0, 55, 4]);
    }
}

#[cfg(feature = "alloc")]
#[test]
fn interrupt_map_pass_thru_preserves_selected_child_bits() {
    use crate::{InterruptError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("intc");
    builder.property("phandle", &cells(&[3]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[3]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();
    builder.begin("nexus");
    builder.property("phandle", &cells(&[2]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[2]));
    builder.property("interrupt-map-mask", &cells(&[u32::MAX, 0]));
    builder.property("interrupt-map-pass-thru", &cells(&[0, 0xff]));
    builder.property("interrupt-map", &cells(&[0x10, 0, 3, 0x20, 4, 0x99]));
    builder.end_node();
    builder.begin("device");
    builder.property("interrupts-extended", &cells(&[2, 0x10, 1]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let intc = tree.find_node("/intc").unwrap();
    let device = tree.find_node("/device").unwrap();
    let decoded = tree.interrupts(device).unwrap().unwrap();

    assert_eq!(decoded[0].provider, intc);
    assert_eq!(decoded[0].cells, vec![0x20, 1, 0x99]);

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("intc");
    builder.property("phandle", &cells(&[3]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[3]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();
    builder.begin("nexus");
    builder.property("phandle", &cells(&[2]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[2]));
    builder.property("interrupt-map-pass-thru", &cells(&[0xff]));
    builder.property("interrupt-map", &cells(&[0x10, 0, 3, 0x20, 4, 0x99]));
    builder.end_node();
    builder.begin("device");
    builder.property("interrupts-extended", &cells(&[2, 0x10, 1]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let nexus = tree.find_node("/nexus").unwrap();
    let device = tree.find_node("/device").unwrap();
    assert_eq!(
        tree.interrupts(device),
        Err(InterruptError::IncompleteEntry {
            node: nexus,
            property: "interrupt-map-pass-thru",
            entry: 0,
            remaining_cells: 1,
            required_cells: 2,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn interrupt_nexus_without_map_walks_to_a_real_controller_or_fails() {
    use crate::{InterruptError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("intc");
    builder.property("phandle", &cells(&[9]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();
    builder.begin("bridge");
    builder.property("phandle", &cells(&[8]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-parent", &cells(&[9]));
    builder.end_node();
    builder.begin("orphan-bridge");
    builder.property("phandle", &cells(&[7]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.end_node();
    builder.begin("device");
    builder.property("interrupt-parent", &cells(&[8]));
    builder.property("interrupts", &cells(&[4]));
    builder.end_node();
    builder.begin("orphan-device");
    builder.property("interrupt-parent", &cells(&[7]));
    builder.property("interrupts", &cells(&[5]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let intc = tree.find_node("/intc").unwrap();
    let bridge = tree.find_node("/bridge").unwrap();
    let orphan_bridge = tree.find_node("/orphan-bridge").unwrap();
    let device = tree.find_node("/device").unwrap();
    let orphan_device = tree.find_node("/orphan-device").unwrap();

    let decoded = tree.interrupts(device).unwrap().unwrap();
    assert_eq!(decoded[0].provider, intc);
    assert_eq!(decoded[0].cells, vec![4]);
    assert_eq!(
        tree.interrupts(orphan_device),
        Err(InterruptError::MissingProvider(orphan_bridge))
    );
    assert_eq!(tree.interrupt_provider(device), Ok(Some(bridge)));
}

#[cfg(feature = "alloc")]
#[test]
fn interrupt_map_skips_disabled_targets_and_validates_the_complete_table() {
    use crate::{InterruptError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("disabled-intc");
    builder.property("phandle", &cells(&[2]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-controller", &[]);
    builder.property("status", b"disabled\0");
    builder.end_node();
    builder.begin("live-intc");
    builder.property("phandle", &cells(&[3]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();
    builder.begin("nexus");
    builder.property("phandle", &cells(&[1]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-map", &cells(&[5, 2, 11, 5, 3, 22]));
    builder.end_node();
    builder.begin("bad-nexus");
    builder.property("phandle", &cells(&[4]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-map", &cells(&[5, 3, 22, 6, 3]));
    builder.end_node();
    builder.begin("device");
    builder.property("interrupt-parent", &cells(&[1]));
    builder.property("interrupts", &cells(&[5]));
    builder.end_node();
    builder.begin("bad-device");
    builder.property("interrupt-parent", &cells(&[4]));
    builder.property("interrupts", &cells(&[5]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let live_intc = tree.find_node("/live-intc").unwrap();
    let bad_nexus = tree.find_node("/bad-nexus").unwrap();
    let device = tree.find_node("/device").unwrap();
    let bad_device = tree.find_node("/bad-device").unwrap();

    let decoded = tree.interrupts(device).unwrap().unwrap();
    assert_eq!(decoded[0].provider, live_intc);
    assert_eq!(decoded[0].cells, vec![22]);
    assert_eq!(
        tree.interrupts(bad_device),
        Err(InterruptError::IncompleteEntry {
            node: bad_nexus,
            property: "interrupt-map",
            entry: 1,
            remaining_cells: 0,
            required_cells: 1,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn interrupt_map_requires_a_unit_address_and_a_matching_entry() {
    use crate::{InterruptError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("intc");
    builder.property("phandle", &cells(&[2]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();
    builder.begin("nexus");
    builder.property("phandle", &cells(&[1]));
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-map", &cells(&[0x10, 5, 2, 9]));
    builder.end_node();
    builder.begin("missing-reg");
    builder.property("interrupt-parent", &cells(&[1]));
    builder.property("interrupts", &cells(&[5]));
    builder.end_node();
    builder.begin("unmatched@20");
    builder.property("reg", &cells(&[0x20]));
    builder.property("interrupt-parent", &cells(&[1]));
    builder.property("interrupts", &cells(&[5]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let nexus = tree.find_node("/nexus").unwrap();
    let missing_reg = tree.find_node("/missing-reg").unwrap();
    let unmatched = tree.find_node("/unmatched@20").unwrap();

    assert_eq!(
        tree.interrupts(missing_reg),
        Err(InterruptError::MissingUnitAddress {
            node: missing_reg,
            nexus,
        })
    );
    assert_eq!(
        tree.interrupts(unmatched),
        Err(InterruptError::MissingMapEntry(nexus))
    );
}

#[cfg(feature = "alloc")]
#[test]
fn msi_parent_preserves_provider_identity_and_variable_specifiers() {
    use crate::Tree;

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("msi-zero");
    builder.property("phandle", &cells(&[1]));
    builder.property("msi-controller", &[]);
    builder.end_node();
    builder.begin("msi-one");
    builder.property("phandle", &cells(&[2]));
    builder.property("msi-controller", &[]);
    builder.property("#msi-cells", &cells(&[1]));
    builder.end_node();
    builder.begin("msi-two");
    builder.property("phandle", &cells(&[3]));
    builder.property("msi-controller", &[]);
    builder.property("#msi-cells", &cells(&[2]));
    builder.end_node();
    builder.begin("pcie");
    builder.property("msi-parent", &cells(&[1, 2, 0x17, 3, 0x53, 0x54]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let host = tree.find_node("/pcie").unwrap();

    let parents = tree.msi_parents(host).unwrap().unwrap();
    assert_eq!(parents.len(), 3);
    assert_eq!(parents[0].controller, tree.find_node("/msi-zero").unwrap());
    assert_eq!(parents[0].controller_phandle, 1);
    assert!(parents[0].msi_specifier.is_empty());
    assert_eq!(parents[1].msi_specifier, vec![0x17]);
    assert_eq!(parents[2].msi_specifier, vec![0x53, 0x54]);
    assert_eq!(tree.msi_parents(tree.root_id()).unwrap(), None);
}

#[cfg(feature = "alloc")]
#[test]
fn malformed_msi_parent_rejects_the_complete_property() {
    use crate::{MsiError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("msi-zero");
    builder.property("phandle", &cells(&[1]));
    builder.end_node();
    builder.begin("msi-two");
    builder.property("phandle", &cells(&[2]));
    builder.property("#msi-cells", &cells(&[2]));
    builder.end_node();
    builder.begin("pcie");
    builder.property("msi-parent", &cells(&[1, 2, 0x53]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let host = tree.find_node("/pcie").unwrap();

    assert_eq!(
        tree.msi_parents(host),
        Err(MsiError::IncompleteEntry {
            node: host,
            entry: 1,
            remaining_cells: 1,
            required_cells: 2,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn generic_provider_specifiers_preserve_holes_names_and_widths() {
    use crate::{SpecifierError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("clock");
    builder.property("phandle", &cells(&[1]));
    builder.property("#clock-cells", &cells(&[2]));
    builder.end_node();
    builder.begin("device");
    builder.property("clocks", &cells(&[1, 10, 20, 0, 1, 30, 40]));
    builder.property("clock-names", b"core\0unused\0bus\0");
    builder.property("memory-region", &cells(&[1, 0]));
    builder.property("memory-region-names", b"buffer\0unused\0");
    builder.end_node();
    builder.begin("bad-device");
    builder.property("clocks", &cells(&[1, 10, 20, 1, 30, 40]));
    builder.property("clock-names", b"only-one\0");
    builder.property("memory-region", &cells(&[1, 0]));
    builder.property("memory-region-names", b"only-one\0");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let clock = tree.find_node("/clock").unwrap();
    let device = tree.find_node("/device").unwrap();

    let clocks = tree.clocks(device).unwrap().unwrap();
    assert_eq!(clocks.len(), 3);
    assert_eq!(clocks[0].name.as_deref(), Some("core"));
    assert_eq!(clocks[0].specifier.provider, Some(clock));
    assert_eq!(clocks[0].specifier.args, vec![10, 20]);
    assert!(clocks[1].specifier.is_empty());
    assert_eq!(clocks[2].specifier.args, vec![30, 40]);

    let memory_regions = tree.named_memory_regions(device).unwrap().unwrap();
    assert_eq!(memory_regions.len(), 2);
    assert_eq!(memory_regions[0].name.as_deref(), Some("buffer"));
    assert_eq!(memory_regions[0].specifier.provider, Some(clock));
    assert!(memory_regions[0].specifier.args.is_empty());
    assert_eq!(memory_regions[1].name.as_deref(), Some("unused"));
    assert!(memory_regions[1].specifier.is_empty());

    let bad = tree.find_node("/bad-device").unwrap();
    assert!(matches!(
        tree.clocks(bad),
        Err(SpecifierError::NameCountMismatch {
            names: 1,
            entries: 2,
            ..
        })
    ));
    assert!(matches!(
        tree.named_memory_regions(bad),
        Err(SpecifierError::NameCountMismatch {
            names: 1,
            entries: 2,
            ..
        })
    ));
}

#[cfg(feature = "alloc")]
#[test]
fn generic_nexus_maps_recursively_across_widths_and_skips_disabled_targets() {
    use crate::Tree;

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("final-clock");
    builder.property("phandle", &cells(&[4]));
    builder.property("#clock-cells", &cells(&[3]));
    builder.end_node();
    builder.begin("disabled-clock");
    builder.property("phandle", &cells(&[5]));
    builder.property("#clock-cells", &cells(&[3]));
    builder.property("status", b"disabled\0");
    builder.end_node();
    builder.begin("second-nexus");
    builder.property("phandle", &cells(&[3]));
    builder.property("#clock-cells", &cells(&[1]));
    builder.property("clock-map-mask", &cells(&[0xf0]));
    builder.property("clock-map-pass-thru", &cells(&[0x0f]));
    builder.property(
        "clock-map",
        &cells(&[
            0xb0, 5, 0xdead, 0xbeef, 0xcafe, 0xb0, 4, 0x1200, 0x34, 0x5678, 0xc0, 4, 1, 2, 3,
        ]),
    );
    builder.end_node();
    builder.begin("first-nexus");
    builder.property("phandle", &cells(&[2]));
    builder.property("#clock-cells", &cells(&[2]));
    builder.property("clock-map-mask", &cells(&[0xf0, u32::MAX]));
    builder.property("clock-map-pass-thru", &cells(&[0x0f, 0]));
    builder.property("clock-map", &cells(&[0xa0, 0x25, 3, 0xb0]));
    builder.end_node();
    builder.begin("default-modifiers-nexus");
    builder.property("phandle", &cells(&[6]));
    builder.property("#clock-cells", &cells(&[1]));
    builder.property("clock-map", &cells(&[7, 4, 9, 8, 7]));
    builder.end_node();
    builder.begin("device");
    builder.property("clocks", &cells(&[2, 0xa5, 0x25, 0, 6, 7]));
    builder.property("clock-names", b"core\0unused\0aux\0");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let device = tree.find_node("/device").unwrap();
    let final_clock = tree.find_node("/final-clock").unwrap();

    let direct = tree
        .phandle_array(device, "clocks", "#clock-cells")
        .unwrap()
        .unwrap();
    assert_eq!(
        direct[0].provider,
        Some(tree.find_node("/first-nexus").unwrap())
    );
    assert_eq!(direct[0].args, vec![0xa5, 0x25]);
    let resolved = tree.resolve_phandle_args_map(&direct[0], "clock").unwrap();
    assert_eq!(resolved.provider, Some(final_clock));
    assert_eq!(resolved.phandle, 4);
    assert_eq!(resolved.args, vec![0x1205, 0x34, 0x5678]);

    let clocks = tree.clocks(device).unwrap().unwrap();
    assert_eq!(clocks.len(), 3);
    assert_eq!(clocks[0].name.as_deref(), Some("core"));
    assert_eq!(clocks[0].specifier, resolved);
    assert_eq!(clocks[1].name.as_deref(), Some("unused"));
    assert!(clocks[1].specifier.is_empty());
    assert_eq!(clocks[2].name.as_deref(), Some("aux"));
    assert_eq!(clocks[2].specifier.provider, Some(final_clock));
    assert_eq!(clocks[2].specifier.args, vec![9, 8, 7]);
}

#[cfg(feature = "alloc")]
#[test]
fn generic_nexus_rejects_bad_suffix_modifier_and_cycles_atomically() {
    use crate::{PropertyError, SpecifierError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("clock");
    builder.property("phandle", &cells(&[3]));
    builder.property("#clock-cells", &cells(&[1]));
    builder.end_node();
    builder.begin("bad-suffix");
    builder.property("phandle", &cells(&[1]));
    builder.property("#clock-cells", &cells(&[1]));
    builder.property("clock-map", &cells(&[0, 3, 10, 1, 3]));
    builder.end_node();
    builder.begin("bad-mask");
    builder.property("phandle", &cells(&[2]));
    builder.property("#clock-cells", &cells(&[2]));
    builder.property("clock-map-mask", &cells(&[u32::MAX]));
    builder.property("clock-map", &cells(&[1, 2, 3, 10]));
    builder.end_node();
    builder.begin("cycle-a");
    builder.property("phandle", &cells(&[4]));
    builder.property("#clock-cells", &cells(&[1]));
    builder.property("clock-map", &cells(&[0, 5, 0]));
    builder.end_node();
    builder.begin("cycle-b");
    builder.property("phandle", &cells(&[5]));
    builder.property("#clock-cells", &cells(&[1]));
    builder.property("clock-map", &cells(&[0, 4, 0]));
    builder.end_node();
    builder.begin("bad-suffix-device");
    builder.property("clocks", &cells(&[1, 0]));
    builder.end_node();
    builder.begin("bad-mask-device");
    builder.property("clocks", &cells(&[2, 1, 2]));
    builder.end_node();
    builder.begin("cycle-device");
    builder.property("clocks", &cells(&[4, 0]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let bad_suffix = tree.find_node("/bad-suffix").unwrap();
    let bad_mask = tree.find_node("/bad-mask").unwrap();
    let cycle_a = tree.find_node("/cycle-a").unwrap();

    assert_eq!(
        tree.clocks(tree.find_node("/bad-suffix-device").unwrap()),
        Err(SpecifierError::IncompleteEntry {
            node: bad_suffix,
            property: "clock-map".into(),
            entry: 1,
            remaining_cells: 2,
            required_cells: 3,
        })
    );
    assert_eq!(
        tree.clocks(tree.find_node("/bad-mask-device").unwrap()),
        Err(SpecifierError::InvalidProperty {
            node: bad_mask,
            property: "clock-map-mask".into(),
            error: PropertyError::InvalidLength {
                actual: 4,
                expected: Some(8),
            },
        })
    );
    assert_eq!(
        tree.clocks(tree.find_node("/cycle-device").unwrap()),
        Err(SpecifierError::MapCycle {
            nexus: cycle_a,
            property: "clock-map".into(),
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn generic_iommu_map_preserves_entries_and_translates_only_unambiguous_widths() {
    use crate::{IdMapError, IdMapTranslationError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("iommu-zero");
    builder.property("phandle", &cells(&[1]));
    builder.property("#iommu-cells", &cells(&[0]));
    builder.end_node();
    builder.begin("iommu-one");
    builder.property("phandle", &cells(&[2]));
    builder.property("#iommu-cells", &cells(&[1]));
    builder.end_node();
    builder.begin("iommu-wide");
    builder.property("phandle", &cells(&[3]));
    builder.property("#iommu-cells", &cells(&[2]));
    builder.end_node();
    builder.begin("legacy-iommu");
    builder.property("phandle", &cells(&[4]));
    builder.end_node();
    builder.begin("zero-host");
    builder.property("iommu-map-mask", &cells(&[0xffff]));
    builder.property("iommu-map", &cells(&[0x100, 1, 0x20]));
    builder.end_node();
    builder.begin("one-host");
    builder.property("iommu-map", &cells(&[0x200, 2, u32::MAX - 1, 3]));
    builder.end_node();
    builder.begin("wide-single-host");
    builder.property("iommu-map", &cells(&[0x300, 3, 7, 8, 1]));
    builder.end_node();
    builder.begin("wide-range-host");
    builder.property("iommu-map-mask", &cells(&[0xffff]));
    builder.property("iommu-map", &cells(&[0x400, 3, u32::MAX, u32::MAX, 0x20]));
    builder.end_node();
    builder.begin("legacy-host");
    builder.property("iommu-map", &cells(&[0, 4, 0x40, 2]));
    builder.end_node();
    builder.begin("empty-range-host");
    builder.property("iommu-map", &cells(&[0, 2, 0, 0, 1, 2, 1, 2]));
    builder.end_node();
    builder.begin("bad-host");
    builder.property("iommu-map", &cells(&[0, 3, 7]));
    builder.end_node();
    builder.begin("empty-host");
    builder.property("iommu-map", &[]);
    builder.end_node();
    builder.begin("input-overflow-host");
    builder.property("iommu-map", &cells(&[u32::MAX, 2, 0, 2]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let iommu_zero = tree.find_node("/iommu-zero").unwrap();
    let iommu_one = tree.find_node("/iommu-one").unwrap();
    let iommu_wide = tree.find_node("/iommu-wide").unwrap();

    let zero = tree
        .iommu_map(tree.find_node("/zero-host").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(zero.mask, 0xffff);
    assert_eq!(zero.entries[0].provider, iommu_zero);
    assert!(zero.entries[0].output_base.is_empty());
    assert_eq!(zero.entries[0].length, 0x20);
    let zero_mapped = zero.map_id(0xabcd_011f).unwrap().unwrap();
    assert_eq!(zero_mapped.provider, iommu_zero);
    assert_eq!(zero_mapped.provider_phandle, 1);
    assert!(zero_mapped.args.is_empty());
    assert_eq!(zero.map_id(0x120), Ok(None));

    let one = tree
        .iommu_map(tree.find_node("/one-host").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(one.map_id(0x201).unwrap().unwrap().args, vec![u32::MAX]);
    assert_eq!(
        one.map_id(0x202),
        Err(IdMapTranslationError::OutputOverflow {
            provider: iommu_one,
            provider_phandle: 2,
            output_base: u32::MAX - 1,
            offset: 2,
        })
    );

    let wide_single = tree
        .iommu_map(tree.find_node("/wide-single-host").unwrap())
        .unwrap()
        .unwrap();
    let wide_single_mapped = wide_single.map_id(0x300).unwrap().unwrap();
    assert_eq!(wide_single_mapped.provider, iommu_wide);
    assert_eq!(wide_single_mapped.args, vec![7, 8]);

    let wide_range = tree
        .iommu_map(tree.find_node("/wide-range-host").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(wide_range.entries[0].output_base, vec![u32::MAX; 2]);
    let matched = wide_range.match_id(0xabcd_0410).unwrap();
    assert_eq!(matched.entry, &wide_range.entries[0]);
    assert_eq!(matched.offset, 0x10);
    assert_eq!(
        wide_range.map_id(0xabcd_0410),
        Err(IdMapTranslationError::AmbiguousMultiCellRange {
            provider: iommu_wide,
            provider_phandle: 3,
            cells: 2,
            length: 0x20,
            offset: 0x10,
        })
    );

    let legacy = tree
        .iommu_map(tree.find_node("/legacy-host").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(legacy.entries[0].output_base, vec![0x40]);
    assert_eq!(legacy.map_id(1).unwrap().unwrap().args, vec![0x41]);

    let empty_range = tree
        .iommu_map(tree.find_node("/empty-range-host").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(empty_range.entries.len(), 2);
    assert_eq!(empty_range.entries[0].length, 0);
    assert_eq!(empty_range.map_id(0), Ok(None));
    assert_eq!(empty_range.map_id(1).unwrap().unwrap().args, vec![1]);
    assert_eq!(empty_range.map_id(2).unwrap().unwrap().args, vec![2]);

    let bad = tree.find_node("/bad-host").unwrap();
    assert!(matches!(
        tree.iommu_map(bad),
        Err(IdMapError::IncompleteEntry { entry: 0, .. })
    ));
    assert!(matches!(
        tree.iommu_map(tree.find_node("/empty-host").unwrap()),
        Err(IdMapError::IncompleteEntry {
            entry: 0,
            remaining_cells: 0,
            required_cells: 3,
            ..
        })
    ));
    let overflow = tree.find_node("/input-overflow-host").unwrap();
    assert!(matches!(
        tree.iommu_map(overflow),
        Err(IdMapError::InvalidRange { entry: 0, .. })
    ));
}

#[cfg(feature = "alloc")]
#[test]
fn graph_binding_resolves_direct_and_ports_container_endpoints() {
    use crate::Tree;

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("display");
    builder.begin("ports");
    builder.begin("port@1");
    builder.property("reg", &cells(&[1]));
    builder.begin("endpoint@2");
    builder.property("reg", &cells(&[2]));
    builder.property("phandle", &cells(&[10]));
    builder.property("remote-endpoint", &cells(&[20]));
    builder.end_node();
    builder.end_node();
    builder.end_node();
    builder.end_node();
    builder.begin("bridge");
    builder.begin("port");
    builder.begin("endpoint");
    builder.property("phandle", &cells(&[20]));
    builder.property("remote-endpoint", &cells(&[10]));
    builder.end_node();
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let display = tree.find_node("/display").unwrap();
    let bridge_endpoint = tree.find_node("/bridge/port/endpoint").unwrap();

    let endpoints = tree.graph_endpoints(display).unwrap();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].port_id, Some(1));
    assert_eq!(endpoints[0].endpoint_id, Some(2));
    assert_eq!(endpoints[0].remote, Some(bridge_endpoint));
    assert_eq!(endpoints[0].remote_phandle, Some(20));
}

#[cfg(feature = "alloc")]
#[test]
fn graph_remote_must_be_an_endpoint_below_a_port() {
    use crate::{GraphError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("display");
    builder.begin("port");
    builder.begin("endpoint");
    builder.property("remote-endpoint", &cells(&[20]));
    builder.end_node();
    builder.end_node();
    builder.end_node();
    builder.begin("endpoint@20");
    builder.property("phandle", &cells(&[20]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let display = tree.find_node("/display").unwrap();

    assert!(matches!(
        tree.graph_endpoints(display),
        Err(GraphError::RemoteIsNotEndpoint { .. })
    ));
}

#[cfg(feature = "alloc")]
#[test]
fn graph_remote_back_reference_cannot_point_to_a_third_endpoint() {
    use crate::{GraphError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("display");
    builder.begin("port");
    builder.begin("endpoint");
    builder.property("phandle", &cells(&[10]));
    builder.property("remote-endpoint", &cells(&[20]));
    builder.end_node();
    builder.end_node();
    builder.end_node();
    builder.begin("bridge");
    builder.begin("port");
    builder.begin("endpoint@0");
    builder.property("phandle", &cells(&[20]));
    builder.property("remote-endpoint", &cells(&[30]));
    builder.end_node();
    builder.begin("endpoint@1");
    builder.property("phandle", &cells(&[30]));
    builder.end_node();
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let display = tree.find_node("/display").unwrap();
    let endpoint = tree.find_node("/display/port/endpoint").unwrap();
    let remote = tree.find_node("/bridge/port/endpoint@0").unwrap();
    let target = tree.find_node("/bridge/port/endpoint@1").unwrap();

    assert_eq!(
        tree.graph_endpoints(display),
        Err(GraphError::RemoteBackReferenceMismatch {
            node: endpoint,
            remote,
            target,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn pci_bindings_preserve_masks_widths_and_parent_identity() {
    use crate::{PciAddressSpace, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[2]));
    builder.property("#size-cells", &cells(&[2]));
    builder.begin("intc");
    builder.property("phandle", &cells(&[1]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();
    builder.begin("pci-intc-nexus");
    builder.property("phandle", &cells(&[3]));
    builder.property("#address-cells", &cells(&[3]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-map", &cells(&[0, 0, 0, 1, 1, 9]));
    builder.end_node();
    builder.begin("msi");
    builder.property("phandle", &cells(&[2]));
    // 非标准的两 cell MSI 表仍由底层解析器无损保留，但通用 PCI MSI
    // 运行时只接受 binding 定义的零或一 cell 格式。
    builder.property("#msi-cells", &cells(&[2]));
    builder.end_node();
    builder.begin("msi-zero");
    builder.property("phandle", &cells(&[4]));
    builder.property("#msi-cells", &cells(&[0]));
    builder.end_node();
    builder.begin("pcie@30000000");
    builder.property("#address-cells", &cells(&[3]));
    builder.property("#size-cells", &cells(&[2]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property(
        "ranges",
        &cells(&[0x4300_0000, 0, 0x4000_0000, 0, 0x4000_0000, 0, 0x0100_0000]),
    );
    builder.property("interrupt-map-pass-thru", &cells(&[0, 0, 0, 0x7]));
    builder.property("interrupt-map", &cells(&[0, 0, 0, 1, 3, 0, 0, 0, 5]));
    builder.property("msi-map-mask", &cells(&[0xffff]));
    builder.property(
        "msi-map",
        &cells(&[0, 2, 100, 200, 0x100, 0x200, 2, 400, 500, 0x20]),
    );
    builder.end_node();

    builder.begin("pcie-legacy");
    builder.property("msi-map", &cells(&[0, 2, 100, 1, 0x200, 2, 400, 1]));
    builder.end_node();
    builder.begin("pcie-zero-msi");
    builder.property("msi-map", &cells(&[0, 4, 1]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let host = tree.find_node("/pcie@30000000").unwrap();

    let ranges = tree.pci_ranges(host).unwrap().unwrap();
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].space, PciAddressSpace::Memory64);
    assert!(ranges[0].prefetchable);
    assert_eq!(ranges[0].parent_address, 0x4000_0000);

    let irq = tree.pci_interrupt_map(host).unwrap().unwrap();
    assert_eq!(irq.mask, vec![u32::MAX; 4]);
    assert_eq!(irq.pass_thru, vec![0, 0, 0, 0x7]);
    assert_eq!(irq.entries[0].parent_phandle, 3);
    assert_eq!(irq.entries[0].parent_specifier, vec![5]);
    let route = tree
        .resolve_pci_interrupt(&irq, &[0, 0, 0], &[1])
        .unwrap()
        .expect("pass-thru key must resolve through the direct parent nexus");
    assert_eq!(route.provider_phandle, 1);
    assert_eq!(route.specifier, vec![9]);

    let msi = tree.pci_msi_map(host).unwrap().unwrap();
    assert_eq!(msi.mask, 0xffff);
    assert_eq!(msi.entries.len(), 2);
    assert_eq!(msi.entries[0].msi_specifier, vec![100, 200]);
    assert_eq!(msi.entries[1].requester_base, 0x200);
    assert_eq!(msi.entries[1].msi_specifier, vec![400, 500]);

    let legacy = tree.find_node("/pcie-legacy").unwrap();
    let legacy = tree.pci_msi_map(legacy).unwrap().unwrap();
    assert_eq!(legacy.entries.len(), 2);
    assert_eq!(legacy.entries[0].msi_specifier, vec![100]);
    assert_eq!(legacy.entries[1].msi_specifier, vec![400]);

    let zero = tree.find_node("/pcie-zero-msi").unwrap();
    let zero = tree.pci_msi_map(zero).unwrap().unwrap();
    assert_eq!(zero.entries.len(), 1);
    assert!(zero.entries[0].msi_specifier.is_empty());
}

#[cfg(feature = "alloc")]
#[test]
fn pci_host_rejects_nonstandard_child_interrupt_width() {
    use crate::{PciError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("intc");
    builder.property("phandle", &cells(&[1]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();
    builder.begin("pcie");
    builder.property("#address-cells", &cells(&[3]));
    builder.property("#interrupt-cells", &cells(&[2]));
    builder.property("interrupt-map", &cells(&[0, 0, 0, 1, 2, 1, 9]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let host = tree.find_node("/pcie").unwrap();

    assert_eq!(
        tree.pci_interrupt_map(host),
        Err(PciError::InvalidCellCount {
            node: host,
            property: "#interrupt-cells",
            expected: Some(1),
            actual: 2,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn pci_interrupt_resolver_revalidates_public_map_widths() {
    use crate::{PciError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("intc");
    builder.property("phandle", &cells(&[1]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();
    builder.begin("pcie");
    builder.property("#address-cells", &cells(&[3]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-map", &cells(&[0, 0, 0, 1, 1, 9]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let host = tree.find_node("/pcie").unwrap();
    let map = tree.pci_interrupt_map(host).unwrap().unwrap();

    let mut malformed = map.clone();
    malformed.mask.pop();
    assert_eq!(
        tree.resolve_pci_interrupt(&malformed, &[0, 0, 0], &[1]),
        Err(PciError::IncompleteEntry {
            node: host,
            property: "interrupt-map-mask",
            entry: 0,
            remaining_cells: 3,
            required_cells: 4,
        })
    );

    let mut malformed = map.clone();
    malformed.pass_thru.pop();
    assert_eq!(
        tree.resolve_pci_interrupt(&malformed, &[0, 0, 0], &[1]),
        Err(PciError::IncompleteEntry {
            node: host,
            property: "interrupt-map-pass-thru",
            entry: 0,
            remaining_cells: 3,
            required_cells: 4,
        })
    );

    let mut malformed = map;
    malformed.entries[0].child_address.pop();
    assert_eq!(
        tree.resolve_pci_interrupt(&malformed, &[0, 0, 0], &[1]),
        Err(PciError::IncompleteEntry {
            node: host,
            property: "interrupt-map",
            entry: 0,
            remaining_cells: 3,
            required_cells: 4,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn pci_interrupt_map_accepts_qemu_loongarch_legacy_cells() {
    use crate::Tree;

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("pch-pic");
    builder.property("phandle", &cells(&[1]));
    builder.property("compatible", b"loongson,pch-pic-1.0\0");
    builder.property("interrupt-controller", &[]);
    builder.property("#interrupt-cells", &cells(&[2]));
    builder.end_node();
    builder.begin("pcie");
    builder.property("#address-cells", &cells(&[3]));
    builder.property("interrupt-map-mask", &cells(&[0x1800, 0, 0, 7]));
    builder.property(
        "interrupt-map",
        &cells(&[0, 0, 0, 1, 1, 0x10, 0x800, 0, 0, 2, 1, 0x11]),
    );
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let host = tree.find_node("/pcie").unwrap();
    let pic = tree.find_node("/pch-pic").unwrap();

    let map = tree.pci_interrupt_map(host).unwrap().unwrap();
    assert_eq!(map.child_interrupt_cells, 1);
    assert_eq!(map.entries.len(), 2);
    assert_eq!(map.entries[0].parent, pic);
    assert_eq!(map.entries[0].parent_specifier, vec![0x10]);
    let route = tree
        .resolve_pci_interrupt(&map, &[0x800, 0, 0], &[2])
        .unwrap()
        .unwrap();
    assert_eq!(route.provider, pic);
    assert_eq!(route.provider_phandle, 1);
    assert_eq!(route.specifier, vec![0x11]);
}

#[cfg(feature = "alloc")]
#[test]
fn msi_map_can_cover_the_last_requester_id() {
    use crate::Tree;

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("msi");
    builder.property("phandle", &cells(&[1]));
    builder.property("#msi-cells", &cells(&[1]));
    builder.end_node();
    builder.begin("pcie");
    builder.property("msi-map", &cells(&[u32::MAX, 1, 7, 1]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let host = tree.find_node("/pcie").unwrap();

    let map = tree.pci_msi_map(host).unwrap().unwrap();
    assert_eq!(map.entries.len(), 1);
    assert_eq!(map.entries[0].requester_base, u32::MAX);
    assert_eq!(map.entries[0].length, 1);
}

#[cfg(feature = "alloc")]
#[test]
fn pci_interrupt_map_skips_disabled_targets() {
    use crate::Tree;

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("disabled-intc");
    builder.property("phandle", &cells(&[1]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-controller", &[]);
    builder.property("status", b"disabled\0");
    builder.end_node();
    builder.begin("active-intc");
    builder.property("phandle", &cells(&[2]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();
    builder.begin("pcie");
    builder.property("#address-cells", &cells(&[3]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property(
        "interrupt-map",
        &cells(&[0, 0, 0, 1, 1, 7, 0, 0, 0, 1, 2, 9]),
    );
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let host = tree.find_node("/pcie").unwrap();

    let map = tree.pci_interrupt_map(host).unwrap().unwrap();
    assert_eq!(map.entries.len(), 1);
    assert_eq!(map.entries[0].parent_phandle, 2);
    assert_eq!(map.entries[0].parent_specifier, vec![9]);
}

#[cfg(feature = "alloc")]
#[test]
fn pci_interrupt_map_accepts_a_zero_cell_target() {
    use crate::Tree;

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("zero-intc");
    builder.property("phandle", &cells(&[1]));
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[0]));
    builder.property("interrupt-controller", &[]);
    builder.end_node();
    builder.begin("pcie");
    builder.property("#address-cells", &cells(&[3]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-map", &cells(&[0, 0, 0, 1, 1]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let host = tree.find_node("/pcie").unwrap();
    let zero_intc = tree.find_node("/zero-intc").unwrap();

    let map = tree.pci_interrupt_map(host).unwrap().unwrap();
    assert_eq!(map.entries.len(), 1);
    assert_eq!(map.entries[0].parent, zero_intc);
    assert_eq!(map.entries[0].parent_phandle, 1);
    assert!(map.entries[0].parent_specifier.is_empty());
}

#[cfg(feature = "alloc")]
#[test]
fn pci_interrupt_map_bounds_declared_width_before_allocating_mask() {
    use crate::{PciError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("pcie");
    builder.property("#address-cells", &cells(&[3]));
    builder.property("#interrupt-cells", &cells(&[u32::MAX]));
    builder.property("interrupt-map", &[]);
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let host = tree.find_node("/pcie").unwrap();

    assert_eq!(
        tree.pci_interrupt_map(host),
        Err(PciError::InvalidCellCount {
            node: host,
            property: "#interrupt-cells",
            expected: Some(1),
            actual: u32::MAX,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn malformed_pci_range_suffix_rejects_the_complete_property() {
    use crate::{PciError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.begin("pcie@0");
    builder.property("#address-cells", &cells(&[3]));
    builder.property("#size-cells", &cells(&[2]));
    builder.property(
        "ranges",
        &cells(&[
            0x0200_0000,
            0,
            0x4000_0000,
            0x4000_0000,
            0,
            0x1000,
            0x0200_0000,
        ]),
    );
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let host = tree.find_node("/pcie@0").unwrap();

    assert_eq!(
        tree.pci_ranges(host),
        Err(PciError::IncompleteEntry {
            node: host,
            property: "ranges",
            entry: 1,
            remaining_cells: 1,
            required_cells: 6,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn numa_binding_preserves_assignments_inheritance_memory_and_symmetric_distance() {
    use crate::{NUMA_LOCAL_DISTANCE, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.property("#address-cells", &cells(&[1]));
    builder.property("#size-cells", &cells(&[1]));
    builder.begin("distance-map");
    builder.property("compatible", b"numa-distance-map-v1\0");
    builder.property(
        "distance-matrix",
        &cells(&[1, 1, NUMA_LOCAL_DISTANCE, 1, 2, 20, 2, 2, 10]),
    );
    builder.end_node();
    builder.begin("memory@1000");
    builder.property("device_type", b"memory\0");
    builder.property("reg", &cells(&[0x1000, 0x2000]));
    builder.property("numa-node-id", &cells(&[1]));
    builder.end_node();
    builder.begin("soc");
    builder.property("numa-node-id", &cells(&[2]));
    builder.begin("device@0");
    builder.end_node();
    builder.begin("disabled@1");
    builder.property("status", b"disabled\0");
    builder.property("numa-node-id", &cells(&[9]));
    builder.end_node();
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();

    let memory_node = tree.find_node("/memory@1000").unwrap();
    let device = tree.find_node("/soc/device@0").unwrap();
    let numa = tree.numa_description().unwrap();
    assert_eq!(tree.effective_numa_node_id(device), Ok(Some(2)));
    assert!(
        numa.assignments
            .iter()
            .any(|entry| entry.node == memory_node && entry.node_id == 1)
    );
    assert!(!numa.assignments.iter().any(|entry| entry.node_id == 9));
    assert_eq!(numa.distance(1, 2), Some(20));
    assert_eq!(numa.distance(2, 1), Some(20));
    assert_eq!(numa.distance(7, 7), Some(NUMA_LOCAL_DISTANCE));
    assert_eq!(numa.distance(1, 7), None);
    assert_eq!(
        tree.memory_description().unwrap().memory_banks[0].numa_node_id,
        Some(1)
    );
}

#[cfg(feature = "alloc")]
#[test]
fn numa_distance_map_rejects_asymmetric_pairs() {
    use crate::{NumaError, Tree};

    let mut builder = StructureBuilder::new(17);
    builder.begin("");
    builder.begin("distance-map");
    builder.property("compatible", b"numa-distance-map-v1\0");
    builder.property("distance-matrix", &cells(&[0, 1, 20, 1, 0, 30]));
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let map = tree.find_node("/distance-map").unwrap();

    assert_eq!(
        tree.numa_description(),
        Err(NumaError::AsymmetricDistance {
            node: map,
            from: 1,
            to: 0,
            forward: 30,
            reverse: 20,
        })
    );
}

#[cfg(feature = "alloc")]
#[test]
fn numa_distance_map_requires_normative_root_name_and_order() {
    use crate::{NumaError, Tree};

    let mut nested = StructureBuilder::new(17);
    nested.begin("");
    nested.begin("container");
    nested.begin("distance-map");
    nested.property("compatible", b"numa-distance-map-v1\0");
    nested.property("distance-matrix", &cells(&[0, 0, 10]));
    nested.end_node();
    nested.end_node();
    nested.end_node();
    let (structure, strings) = nested.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let map = tree.find_node("/container/distance-map").unwrap();
    assert_eq!(
        tree.numa_description(),
        Err(NumaError::DistanceMapOutsideRoot { node: map })
    );

    let mut named = StructureBuilder::new(17);
    named.begin("");
    named.begin("distance-map@0");
    named.property("compatible", b"numa-distance-map-v1\0");
    named.property("distance-matrix", &cells(&[0, 0, 10]));
    named.end_node();
    named.end_node();
    let (structure, strings) = named.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let map = tree.find_node("/distance-map@0").unwrap();
    assert_eq!(
        tree.numa_description(),
        Err(NumaError::InvalidDistanceMapName { node: map })
    );

    let mut unordered = StructureBuilder::new(17);
    unordered.begin("");
    unordered.begin("distance-map");
    unordered.property("compatible", b"numa-distance-map-v1\0");
    unordered.property("distance-matrix", &cells(&[1, 1, 10, 0, 0, 10]));
    unordered.end_node();
    unordered.end_node();
    let (structure, strings) = unordered.end();
    let blob = assemble(17, structure, strings, &[]);
    let tree = Tree::parse(&blob).unwrap();
    let map = tree.find_node("/distance-map").unwrap();
    assert_eq!(
        tree.numa_description(),
        Err(NumaError::UnorderedDistance {
            node: map,
            previous_from: 1,
            previous_to: 1,
            from: 0,
            to: 0,
        })
    );
}

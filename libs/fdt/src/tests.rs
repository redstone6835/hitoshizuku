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
    set_u32(&mut blob, 28, 7);
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
    for version in 2..=18 {
        let blob = basic_blob(version);
        let fdt = Fdt::parse(&blob).unwrap_or_else(|error| panic!("v{version}: {error:?}"));
        assert_eq!(fdt.header().version, version);
        assert_eq!(
            fdt.header().size(),
            if version == 2 {
                32
            } else if version < 17 {
                36
            } else {
                40
            }
        );
        assert_eq!(fdt.header().boot_cpuid_phys, 7);
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
    assert_eq!(
        Fdt::parse(&nonzero_padding)
            .unwrap()
            .root()
            .property("wide")
            .unwrap()
            .value(),
        &[1, 2, 3, 4, 5, 6, 7, 8]
    );
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

    // libfdt 仅按对齐跳过 token 间隙，不要求填充内容为零。QEMU 生成的
    // DTB 会在重用缓冲区时保留这些字节。
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
    assert_eq!(
        Fdt::parse(&blob)
            .unwrap()
            .root()
            .property("compatible")
            .unwrap()
            .as_str(),
        Ok("qemu")
    );
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
fn parse_uses_declared_total_size_only() {
    let mut blob = basic_blob(17);
    let declared = blob.len();
    blob.extend_from_slice(b"unrelated bytes");
    assert_eq!(Fdt::parse(&blob).unwrap().as_bytes().len(), declared);
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
    set_u32(&mut blob, 20, 1);
    assert!(matches!(
        Fdt::parse(&blob),
        Err(Error::UnsupportedVersion { version: 1, .. })
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
        Err(Error::MissingReservationTerminator { .. }) | Err(Error::TruncatedReservation { .. })
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
        Fdt::parse(&assemble(17, structure, strings, &[])),
        Err(Error::PropertyAfterChild { .. })
    ));

    let mut builder = StructureBuilder::new(17);
    builder.begin("not-root");
    builder.end_node();
    let (structure, strings) = builder.end();
    assert!(matches!(
        Fdt::parse(&assemble(17, structure, strings, &[])),
        Err(Error::InvalidRootName { .. })
    ));

    let mut builder = StructureBuilder::new(15);
    builder.begin("/");
    builder.begin("child-without-full-path");
    builder.end_node();
    builder.end_node();
    let (structure, strings) = builder.end();
    assert!(matches!(
        Fdt::parse(&assemble(15, structure, strings, &[])),
        Err(Error::InvalidNodeName { .. })
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
        star*property;
    };
};
"#;
    for version in [2u32, 3, 16, 17] {
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
                .property("star*property")
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
    const NAME: &str = "node-with-thirty-one-characters-x";

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
    builder.property("#address-cells", &cells(&[0]));
    builder.property("#interrupt-cells", &cells(&[1]));
    builder.property("interrupt-map", &cells(&[5, 1, 9]));
    builder.end_node();
    builder.begin("msi");
    builder.property("phandle", &cells(&[2]));
    // Modern msi-map entries preserve the target's two-cell output specifier.
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
    builder.property("interrupt-map", &cells(&[0, 0, 0, 1, 3, 5]));
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
    assert_eq!(irq.entries[0].parent_phandle, 1);
    assert_eq!(irq.entries[0].parent_specifier, vec![9]);

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
        Err(PciError::IncompleteEntry {
            node: host,
            property: "interrupt-map",
            entry: 0,
            remaining_cells: 0,
            required_cells: u32::MAX as usize + 4,
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

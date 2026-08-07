use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use soyo_linker::elf::{ElfErrorKind, TargetArch, read_object};

static RV64_PROBE: OnceLock<Vec<u8>> = OnceLock::new();
static LA64_PROBE: OnceLock<Vec<u8>> = OnceLock::new();

fn compile_probe(target: &str, extra_flags: &[&str]) -> Vec<u8> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/probe.c");
    let output = std::env::temp_dir().join(format!(
        "soyo-linker-{}-{}-probe.o",
        std::process::id(),
        target
    ));
    let mut command = Command::new("clang");
    command
        .arg(format!("--target={target}"))
        .args([
            "-ffreestanding",
            "-fno-stack-protector",
            "-fno-pic",
            "-fno-pie",
            "-O0",
            "-c",
        ])
        .args(extra_flags)
        .arg(fixture)
        .arg("-o")
        .arg(&output);
    let status = command.status().expect("应能启动 clang");
    assert!(status.success(), "clang 未能生成 {target} ET_REL");
    let bytes = fs::read(&output).expect("应能读取 clang 输出");
    fs::remove_file(output).expect("应能清理临时对象");
    bytes
}

fn rv64_probe() -> Vec<u8> {
    RV64_PROBE
        .get_or_init(|| compile_probe("riscv64-unknown-none-elf", &["-mno-relax"]))
        .clone()
}

fn la64_probe() -> Vec<u8> {
    LA64_PROBE
        .get_or_init(|| compile_probe("loongarch64-unknown-none", &[]))
        .clone()
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn section_header_offset(bytes: &[u8], index: usize) -> usize {
    read_u64(bytes, 40) as usize + usize::from(read_u16(bytes, 58)) * index
}

fn section_index_by_type(bytes: &[u8], section_type: u32) -> usize {
    let section_count = usize::from(read_u16(bytes, 60));
    (0..section_count)
        .find(|index| {
            let header = section_header_offset(bytes, *index);
            u32::from_le_bytes(bytes[header + 4..header + 8].try_into().unwrap()) == section_type
        })
        .expect("fixture 应包含指定 section")
}

#[test]
fn reads_real_rv64_relocatable_object() {
    let bytes = rv64_probe();
    let object = read_object(PathBuf::from("probe-rv64.o"), &bytes).unwrap();

    assert_eq!(object.target_arch(), TargetArch::Riscv64);
    assert_eq!(object.section_by_name(".text").unwrap().alignment(), 2);
    assert!(object.symbol_by_name("_start").is_some());
    assert!(
        object
            .symbol_by_name("external_value")
            .unwrap()
            .is_undefined()
    );
    assert!(!object.relocations().is_empty());
}

#[test]
fn reads_real_la64_relocatable_object() {
    let bytes = la64_probe();
    let object = read_object(PathBuf::from("probe-la64.o"), &bytes).unwrap();

    assert_eq!(object.target_arch(), TargetArch::LoongArch64);
    assert_eq!(object.section_by_name(".text").unwrap().alignment(), 32);
    assert!(object.symbol_by_name("_start").is_some());
    assert!(
        object
            .symbol_by_name("external_value")
            .unwrap()
            .is_undefined()
    );
    assert!(!object.relocations().is_empty());
}

#[test]
fn rejects_non_relocatable_elf_type() {
    let mut bytes = rv64_probe();
    bytes[16..18].copy_from_slice(&2u16.to_le_bytes());

    let error = read_object(PathBuf::from("executable.o"), &bytes).unwrap_err();
    assert_eq!(error.kind(), ElfErrorKind::UnsupportedType);
}

#[test]
fn rejects_symbol_table_link_to_non_string_table() {
    let mut bytes = rv64_probe();
    let symbol_table = section_index_by_type(&bytes, 2);
    let header = section_header_offset(&bytes, symbol_table);
    bytes[header + 40..header + 44].copy_from_slice(&2u32.to_le_bytes());

    let error = read_object(PathBuf::from("bad-link.o"), &bytes).unwrap_err();
    assert_eq!(error.kind(), ElfErrorKind::InvalidSectionLink);
}

#[test]
fn rejects_rel_without_explicit_addend() {
    let mut bytes = rv64_probe();
    let relocation_table = section_index_by_type(&bytes, 4);
    let header = section_header_offset(&bytes, relocation_table);
    bytes[header + 4..header + 8].copy_from_slice(&9u32.to_le_bytes());

    let error = read_object(PathBuf::from("rel.o"), &bytes).unwrap_err();
    assert_eq!(error.kind(), ElfErrorKind::InvalidRelocation);
}

#[test]
fn rejects_string_table_without_leading_nul() {
    let mut bytes = rv64_probe();
    let section_name_index = usize::from(read_u16(&bytes, 62));
    let header = section_header_offset(&bytes, section_name_index);
    let string_offset = read_u64(&bytes, header + 24) as usize;
    bytes[string_offset] = b'x';

    let error = read_object(PathBuf::from("bad-strtab.o"), &bytes).unwrap_err();
    assert_eq!(error.kind(), ElfErrorKind::InvalidString);
}

#[test]
fn rejects_nonzero_null_symbol() {
    let mut bytes = rv64_probe();
    let symbol_table = section_index_by_type(&bytes, 2);
    let header = section_header_offset(&bytes, symbol_table);
    let symbol_offset = read_u64(&bytes, header + 24) as usize;
    bytes[symbol_offset + 8] = 1;

    let error = read_object(PathBuf::from("bad-null-symbol.o"), &bytes).unwrap_err();
    assert_eq!(error.kind(), ElfErrorKind::InvalidSymbolTable);
}

#[test]
fn rejects_bad_symbol_entry_size_as_symbol_table_error() {
    let mut bytes = rv64_probe();
    let symbol_table = section_index_by_type(&bytes, 2);
    let header = section_header_offset(&bytes, symbol_table);
    bytes[header + 56..header + 64].copy_from_slice(&16u64.to_le_bytes());

    let error = read_object(PathBuf::from("bad-symbol-size.o"), &bytes).unwrap_err();
    assert_eq!(error.kind(), ElfErrorKind::InvalidSymbolTable);
}

#[test]
fn rejects_section_table_outside_object() {
    let mut bytes = rv64_probe();
    let outside = (bytes.len() as u64 + 7) & !7;
    bytes[40..48].copy_from_slice(&outside.to_le_bytes());

    let error = read_object(PathBuf::from("bad-section-range.o"), &bytes).unwrap_err();
    assert_eq!(error.kind(), ElfErrorKind::InvalidSectionTable);
}

#[test]
fn rejects_section_count_over_resource_limit() {
    let mut bytes = rv64_probe();
    bytes[60..62].copy_from_slice(&4097u16.to_le_bytes());

    let error = read_object(PathBuf::from("too-many-sections.o"), &bytes).unwrap_err();
    assert_eq!(error.kind(), ElfErrorKind::TooManySections);
}

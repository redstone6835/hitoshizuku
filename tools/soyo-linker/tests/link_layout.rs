use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use native_abi::TargetArch;
use soyo::registry::SegmentKind;
use soyo_linker::link::{InputObject, LinkErrorKind, LinkRequest, build_link_image};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn compile_object(fixture: &str, target: &str, extra_flags: &[&str]) -> InputObject {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let output = std::env::temp_dir().join(format!(
        "soyo-linker-layout-{}-{}-{}.o",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        fixture
    ));
    let status = Command::new("clang")
        .arg(format!("--target={target}"))
        .args([
            "-ffreestanding",
            "-fno-stack-protector",
            "-fno-pic",
            "-fno-pie",
            "-fno-asynchronous-unwind-tables",
            "-fno-unwind-tables",
            "-fvisibility=hidden",
            "-O0",
            "-c",
        ])
        .args(extra_flags)
        .arg(source)
        .arg("-o")
        .arg(&output)
        .status()
        .expect("应能启动 clang");
    assert!(status.success(), "clang 未能生成 {target} 对象");
    let bytes = fs::read(&output).expect("应能读取目标对象");
    fs::remove_file(&output).expect("应能清理目标对象");
    InputObject::new(PathBuf::from(fixture), bytes)
}

fn rv64_objects() -> Vec<InputObject> {
    ["entry.c", "library.c"]
        .into_iter()
        .map(|fixture| {
            compile_object(
                fixture,
                "riscv64-unknown-none-elf",
                &["-mno-relax", "-msmall-data-limit=0", "-mcmodel=medany"],
            )
        })
        .collect()
}

fn with_elf_flags(object: InputObject, flags: u32) -> InputObject {
    let mut bytes = object.bytes().to_vec();
    bytes[48..52].copy_from_slice(&flags.to_le_bytes());
    InputObject::new(object.path().to_path_buf(), bytes)
}

#[test]
fn merges_real_sections_and_resolves_cross_object_symbols() {
    let objects = rv64_objects();
    let image = build_link_image(LinkRequest {
        target_arch: TargetArch::Riscv64,
        entry_symbol: "_start",
        objects: &objects,
    })
    .unwrap();

    assert_eq!(image.target_arch(), TargetArch::Riscv64);
    assert_eq!(image.entry_offset() % 2, 0);
    assert_eq!(
        image
            .segments()
            .iter()
            .map(|segment| segment.kind())
            .collect::<Vec<_>>(),
        vec![
            SegmentKind::Code,
            SegmentKind::Rodata,
            SegmentKind::Data,
            SegmentKind::Bss,
        ]
    );
    assert!(image.symbol("helper").is_some());
    assert!(image.symbol("writable_value").is_some());
    assert!(!image.pending_relocations().is_empty());
}

#[test]
fn layout_is_byte_deterministic_for_identical_inputs() {
    let objects = rv64_objects();
    let request = || LinkRequest {
        target_arch: TargetArch::Riscv64,
        entry_symbol: "_start",
        objects: &objects,
    };

    assert_eq!(
        build_link_image(request()).unwrap(),
        build_link_image(request()).unwrap()
    );
}

#[test]
fn rejects_duplicate_strong_symbol() {
    let mut objects = rv64_objects();
    objects.push(compile_object(
        "library.c",
        "riscv64-unknown-none-elf",
        &["-mno-relax", "-msmall-data-limit=0", "-mcmodel=medany"],
    ));

    let error = build_link_image(LinkRequest {
        target_arch: TargetArch::Riscv64,
        entry_symbol: "_start",
        objects: &objects,
    })
    .unwrap_err();
    assert_eq!(error.kind(), LinkErrorKind::DuplicateSymbol);
}

#[test]
fn rejects_unresolved_global_symbol() {
    let objects = vec![compile_object(
        "entry.c",
        "riscv64-unknown-none-elf",
        &["-mno-relax", "-msmall-data-limit=0", "-mcmodel=medany"],
    )];

    let error = build_link_image(LinkRequest {
        target_arch: TargetArch::Riscv64,
        entry_symbol: "_start",
        objects: &objects,
    })
    .unwrap_err();
    assert_eq!(error.kind(), LinkErrorKind::UndefinedSymbol);
}

#[test]
fn rejects_mixed_target_objects() {
    let mut objects = rv64_objects();
    objects.push(compile_object("library.c", "loongarch64-unknown-none", &[]));

    let error = build_link_image(LinkRequest {
        target_arch: TargetArch::Riscv64,
        entry_symbol: "_start",
        objects: &objects,
    })
    .unwrap_err();
    assert_eq!(error.kind(), LinkErrorKind::TargetMismatch);
}

#[test]
fn rejects_mixed_rv64_float_abis() {
    let mut objects = rv64_objects();
    objects[0] = with_elf_flags(objects[0].clone(), 0x1);
    objects[1] = with_elf_flags(objects[1].clone(), 0x5);

    let error = build_link_image(LinkRequest {
        target_arch: TargetArch::Riscv64,
        entry_symbol: "_start",
        objects: &objects,
    })
    .unwrap_err();
    assert_eq!(error.kind(), LinkErrorKind::TargetMismatch);
}

#[test]
fn rv64_rvc_flag_does_not_change_the_calling_convention() {
    let mut objects = rv64_objects();
    objects[0] = with_elf_flags(objects[0].clone(), 0x0);
    objects[1] = with_elf_flags(objects[1].clone(), 0x1);

    build_link_image(LinkRequest {
        target_arch: TargetArch::Riscv64,
        entry_symbol: "_start",
        objects: &objects,
    })
    .unwrap();
}

#[test]
fn rejects_mixed_la64_float_abis() {
    let mut objects = vec![
        compile_object("entry.c", "loongarch64-unknown-none", &[]),
        compile_object("library.c", "loongarch64-unknown-none", &[]),
    ];
    objects[0] = with_elf_flags(objects[0].clone(), 0x41);
    objects[1] = with_elf_flags(objects[1].clone(), 0x43);

    let error = build_link_image(LinkRequest {
        target_arch: TargetArch::LoongArch64,
        entry_symbol: "_start",
        objects: &objects,
    })
    .unwrap_err();
    assert_eq!(error.kind(), LinkErrorKind::TargetMismatch);
}

#[test]
fn rejects_real_comdat_section_group() {
    let objects = vec![compile_object(
        "comdat-rv64.s",
        "riscv64-unknown-none-elf",
        &["-mno-relax"],
    )];

    let error = build_link_image(LinkRequest {
        target_arch: TargetArch::Riscv64,
        entry_symbol: "_start",
        objects: &objects,
    })
    .unwrap_err();
    assert_eq!(error.kind(), LinkErrorKind::UnsupportedSection);
}

#[test]
fn entry_must_resolve_into_code() {
    let objects = rv64_objects();
    let error = build_link_image(LinkRequest {
        target_arch: TargetArch::Riscv64,
        entry_symbol: "writable_value",
        objects: &objects,
    })
    .unwrap_err();
    assert_eq!(error.kind(), LinkErrorKind::EntryNotCode);
}

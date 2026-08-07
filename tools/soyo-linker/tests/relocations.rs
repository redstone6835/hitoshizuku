use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use native_abi::TargetArch;
use soyo::registry::RelocationKind;
use soyo_linker::link::{
    InputObject, LinkRequest, SymbolValue, apply_relocations, build_link_image,
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn compile_objects(target: TargetArch) -> Vec<InputObject> {
    compile_objects_with_optimization(target, "-O0")
}

fn compile_objects_with_optimization(target: TargetArch, optimization: &str) -> Vec<InputObject> {
    ["entry.c", "library.c", "pointer.c"]
        .into_iter()
        .map(|fixture| {
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(fixture);
            let output = std::env::temp_dir().join(format!(
                "soyo-linker-reloc-{}-{}-{}.o",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
                fixture
            ));
            let (triple, arch_flags): (&str, &[&str]) = match target {
                TargetArch::Riscv64 => (
                    "riscv64-unknown-none-elf",
                    &["-mno-relax", "-msmall-data-limit=0", "-mcmodel=medany"],
                ),
                TargetArch::LoongArch64 => ("loongarch64-unknown-none", &[]),
            };
            let status = Command::new("clang")
                .arg(format!("--target={triple}"))
                .args([
                    "-ffreestanding",
                    "-fno-stack-protector",
                    "-fno-pic",
                    "-fno-pie",
                    "-fno-asynchronous-unwind-tables",
                    "-fno-unwind-tables",
                    "-fvisibility=hidden",
                    optimization,
                    "-c",
                ])
                .args(arch_flags)
                .arg(source)
                .arg("-o")
                .arg(&output)
                .status()
                .expect("应能启动 clang");
            assert!(status.success(), "clang 未能生成 {triple} 对象");
            let bytes = fs::read(&output).expect("应能读取目标对象");
            fs::remove_file(&output).expect("应能清理目标对象");
            InputObject::new(PathBuf::from(fixture), bytes)
        })
        .collect()
}

#[test]
fn la64_optimized_signed_i12_relocations_are_supported() {
    let objects = compile_objects_with_optimization(TargetArch::LoongArch64, "-O2");
    let pending = build_link_image(LinkRequest {
        target_arch: TargetArch::LoongArch64,
        entry_symbol: "_start",
        objects: &objects,
    })
    .unwrap();

    apply_relocations(pending).unwrap();
}

fn signed(value: u32, bits: u32) -> i64 {
    ((i64::from(value) << (64 - bits)) >> (64 - bits)) as i64
}

fn word(bytes: &[u8], offset: u64) -> u32 {
    let offset = offset as usize;
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

#[test]
fn rv64_relocations_resolve_to_linked_symbols() {
    let objects = compile_objects(TargetArch::Riscv64);
    let pending = build_link_image(LinkRequest {
        target_arch: TargetArch::Riscv64,
        entry_symbol: "_start",
        objects: &objects,
    })
    .unwrap();
    let original = pending.pending_relocations().to_vec();
    let linked = apply_relocations(pending).unwrap();

    for relocation in &original {
        let segment = &linked.segments()[relocation.target_segment_index()];
        match relocation.kind() {
            19 => {
                let high = signed(
                    word(segment.payload(), relocation.target_offset()) >> 12,
                    20,
                ) << 12;
                let low = signed(
                    word(segment.payload(), relocation.target_offset() + 4) >> 20,
                    12,
                );
                let target = (relocation.place_offset() as i64 + high + low) as u64;
                let SymbolValue::Image(symbol) = relocation.symbol_value() else {
                    panic!("CALL_PLT 目标必须位于映像内")
                };
                assert_eq!(target, symbol);
            }
            23 => {
                let low_relocation = original
                    .iter()
                    .find(|candidate| {
                        candidate.kind() == 24
                            && candidate.symbol_value()
                                == SymbolValue::Image(relocation.place_offset())
                    })
                    .expect("PCREL_HI20 必须有 LO12_I 配对");
                let high = signed(
                    word(segment.payload(), relocation.target_offset()) >> 12,
                    20,
                ) << 12;
                let low = signed(
                    word(segment.payload(), low_relocation.target_offset()) >> 20,
                    12,
                );
                let SymbolValue::Image(symbol) = relocation.symbol_value() else {
                    panic!("PCREL_HI20 目标必须位于映像内")
                };
                assert_eq!(
                    (relocation.place_offset() as i64 + high + low) as u64,
                    symbol
                );
            }
            24 => {}
            2 => {}
            other => panic!("测试输入出现未冻结的 RV64 relocation {other}"),
        }
    }
    assert_eq!(linked.runtime_relocations().len(), 1);
    assert_eq!(
        linked.runtime_relocations()[0].kind,
        RelocationKind::ImageBase64
    );
    assert_eq!(
        linked.runtime_relocations()[0].addend as u64,
        match linked.symbol("helper").unwrap().value() {
            SymbolValue::Image(value) => value,
            _ => panic!("helper 必须是映像符号"),
        }
    );
}

#[test]
fn la64_relocations_resolve_to_linked_symbols() {
    let objects = compile_objects(TargetArch::LoongArch64);
    let pending = build_link_image(LinkRequest {
        target_arch: TargetArch::LoongArch64,
        entry_symbol: "_start",
        objects: &objects,
    })
    .unwrap();
    let original = pending.pending_relocations().to_vec();
    let linked = apply_relocations(pending).unwrap();

    for relocation in &original {
        let segment = &linked.segments()[relocation.target_segment_index()];
        match relocation.kind() {
            66 => {
                let instruction = word(segment.payload(), relocation.target_offset());
                let immediate = ((instruction >> 10) & 0xffff) | ((instruction & 0x3ff) << 16);
                let target =
                    (relocation.place_offset() as i64 + (signed(immediate, 26) << 2)) as u64;
                let SymbolValue::Image(symbol) = relocation.symbol_value() else {
                    panic!("B26 目标必须位于映像内")
                };
                assert_eq!(target, symbol);
            }
            71 => {
                let low_relocation = original
                    .iter()
                    .find(|candidate| {
                        candidate.kind() == 72
                            && candidate.symbol_value() == relocation.symbol_value()
                            && candidate.target_offset() == relocation.target_offset() + 4
                    })
                    .expect("PCALA_HI20 必须有 LO12 配对");
                let high_instruction = word(segment.payload(), relocation.target_offset());
                let high = signed((high_instruction >> 5) & 0xfffff, 20) << 12;
                let low_instruction = word(segment.payload(), low_relocation.target_offset());
                let low = signed((low_instruction >> 10) & 0xfff, 12);
                let page = relocation.place_offset() & !0xfff;
                let SymbolValue::Image(symbol) = relocation.symbol_value() else {
                    panic!("PCALA 目标必须位于映像内")
                };
                assert_eq!((page as i64 + high + low) as u64, symbol);
            }
            72 => {}
            110 => {
                let high_instruction = word(segment.payload(), relocation.target_offset());
                let low_instruction = word(segment.payload(), relocation.target_offset() + 4);
                let high = signed((high_instruction >> 5) & 0xfffff, 20) << 18;
                let low = signed((low_instruction >> 10) & 0xffff, 16) << 2;
                let target = (relocation.place_offset() as i64 + high + low) as u64;
                let SymbolValue::Image(symbol) = relocation.symbol_value() else {
                    panic!("CALL36 目标必须位于映像内")
                };
                assert_eq!(target, symbol);
            }
            2 => {}
            other => panic!("测试输入出现未冻结的 LA64 relocation {other}"),
        }
    }
    assert_eq!(linked.runtime_relocations().len(), 1);
    assert_eq!(
        linked.runtime_relocations()[0].kind,
        RelocationKind::ImageBase64
    );
}

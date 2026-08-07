use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use native_abi::{OperationId, RequirementId, TargetArch};
use soyo::{SliceSoyoReader, SoyoReadLimits, SoyoTargetPolicy, read_soyo, validate_soyo};
use soyo_linker::contract::parse_manifest;
use soyo_linker::link::{InputObject, LinkRequest, apply_relocations, build_link_image};
use soyo_linker::writer::{EncodeErrorKind, encode_soyo};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const MANIFEST: &str = r#"
{
  "entry": "_start",
  "imports": [
    { "name": "STREAM_WRITE", "required": true },
    { "name": "PROCESS_EXIT", "required": true }
  ],
  "capabilities": [
    { "name": "STDOUT", "rights": ["WRITE"], "required": true },
    { "name": "SELF_PROCESS", "rights": ["TERMINATE_SELF"], "required": true }
  ],
  "runtime": {
    "stack_size": 65536,
    "stack_guard_size": 4096,
    "start_info_max_size": 4096
  }
}
"#;

fn compile_rv64_objects() -> Vec<InputObject> {
    ["entry.c", "library.c", "pointer.c"]
        .into_iter()
        .map(|fixture| {
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(fixture);
            let output = std::env::temp_dir().join(format!(
                "soyo-linker-writer-{}-{}-{fixture}.o",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            ));
            let status = Command::new("clang")
                .arg("--target=riscv64-unknown-none-elf")
                .args([
                    "-ffreestanding",
                    "-fno-stack-protector",
                    "-fno-pic",
                    "-fno-pie",
                    "-fno-asynchronous-unwind-tables",
                    "-fno-unwind-tables",
                    "-fvisibility=hidden",
                    "-mno-relax",
                    "-msmall-data-limit=0",
                    "-mcmodel=medany",
                    "-O0",
                    "-c",
                ])
                .arg(source)
                .arg("-o")
                .arg(&output)
                .status()
                .expect("应能启动 clang");
            assert!(status.success(), "clang 未能生成 RV64 对象");
            let bytes = fs::read(&output).expect("应能读取目标对象");
            fs::remove_file(output).expect("应能清理目标对象");
            InputObject::new(PathBuf::from(fixture), bytes)
        })
        .collect()
}

fn linked_image() -> soyo_linker::link::LinkedImage {
    let objects = compile_rv64_objects();
    apply_relocations(
        build_link_image(LinkRequest {
            target_arch: TargetArch::Riscv64,
            entry_symbol: "_start",
            objects: &objects,
        })
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn canonical_writer_round_trips_real_linked_image() {
    let image = linked_image();
    let contract = parse_manifest(MANIFEST).unwrap();
    let first = encode_soyo(&image, &contract).unwrap();
    let second = encode_soyo(&image, &contract).unwrap();
    assert_eq!(first, second);

    let metadata = read_soyo(&SliceSoyoReader::new(&first), SoyoReadLimits::portable()).unwrap();
    let plan = validate_soyo(&metadata, SoyoTargetPolicy::for_kernel(TargetArch::Riscv64)).unwrap();

    assert_eq!(plan.entry_offset, image.entry_offset());
    assert_eq!(metadata.header.build_id, metadata.header.content_hash);
    assert_eq!(
        metadata
            .imports
            .iter()
            .map(|import| import.operation_id)
            .collect::<Vec<_>>(),
        [
            OperationId::ProcessExit as u32,
            OperationId::StreamWrite as u32
        ]
    );
    assert_eq!(
        metadata
            .capabilities
            .iter()
            .map(|capability| capability.requirement_id)
            .collect::<Vec<_>>(),
        [
            RequirementId::SelfProcess as u32,
            RequirementId::Stdout as u32
        ]
    );
    assert_eq!(metadata.relocations, image.runtime_relocations());
    assert_eq!(metadata.runtime.stack_size, 65536);
}

#[test]
fn writer_rejects_contract_required_operation_not_supported_by_kernel() {
    let image = linked_image();
    let manifest = MANIFEST
        .replace("STREAM_WRITE", "STREAM_READ")
        .replace("STDOUT", "STDIN")
        .replace(r#"["WRITE"]"#, r#"["READ"]"#);
    let contract = parse_manifest(&manifest).unwrap();

    assert_eq!(
        encode_soyo(&image, &contract).unwrap_err().kind(),
        EncodeErrorKind::SelfValidation
    );
}

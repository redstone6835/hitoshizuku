use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use native_abi::TargetArch;
use soyo_linker::contract::parse_manifest;
use soyo_linker::rust_bindings::generate_rust_module;

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

#[test]
fn generated_rust_module_compiles_with_program_contract() {
    let contract = parse_manifest(MANIFEST).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "soyo-rust-bindings-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&directory).unwrap();
    let binding_path = directory.join("mygo_program.rs");
    let probe_path = directory.join("probe.rs");
    fs::write(
        &binding_path,
        generate_rust_module(TargetArch::Riscv64, &contract),
    )
    .unwrap();
    fs::write(
        &probe_path,
        r#"
#![no_std]

include!("mygo_program.rs");

const _: () = assert!(MYGO_TARGET_ARCH == 1);
const _: () = assert!(MYGO_ABI_FAMILY == 1);
const _: () = assert!(MYGO_ABI_EPOCH == 1);
const _: () = assert!(MYGO_FEATURE_STATIC_TLS == 1);
const _: () = assert!(MYGO_SLOT_PROCESS_EXIT == 0);
const _: () = assert!(MYGO_SLOT_STREAM_WRITE == 1);
const _: () = assert!(MYGO_REQUIREMENT_SELF_PROCESS == 1);
const _: () = assert!(MYGO_REQUIREMENT_STDOUT == 4);
const _: () = assert!(MYGO_RIGHT_WRITE == 2);
const _: () = assert!(MYGO_RIGHT_TERMINATE_SELF == 16);
const _: () = assert!(MYGO_STATUS_OK == 0x0000_0000);
const _: () = assert!(MYGO_STATUS_IO_CLOSED == 0x0500_0003);
const _: () = assert!(MYGO_CAP_SELF_PROCESS_RIGHTS == 16);
const _: () = assert!(MYGO_CAP_STDOUT_RIGHTS == 2);
const _: () = assert!(core::mem::size_of::<MygoNativeCall>() == 64);
const _: () = assert!(core::mem::offset_of!(MygoNativeCall, args) == 16);
const _: () = assert!(core::mem::size_of::<MygoNativeResult>() == 24);
const _: () = assert!(core::mem::offset_of!(MygoNativeResult, value0) == 8);
"#,
    )
    .unwrap();

    let output = Command::new("rustc")
        .args(["--edition=2024", "--crate-type=lib"])
        .arg(&probe_path)
        .arg("-o")
        .arg(directory.join("libprobe.rlib"))
        .output()
        .expect("应能启动 rustc");
    let _ = fs::remove_dir_all(&directory);
    assert!(
        output.status.success(),
        "生成的 Rust module 无法编译: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

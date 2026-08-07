use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use native_abi::TargetArch;
use soyo_linker::bindings::generate_c_header;
use soyo_linker::contract::parse_manifest;

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
fn c_header_uses_registry_order_for_program_slots() {
    let contract = parse_manifest(MANIFEST).unwrap();
    let header = String::from_utf8(generate_c_header(TargetArch::Riscv64, &contract)).unwrap();

    let exit = header.find("#define MYGO_SLOT_PROCESS_EXIT 0u\n").unwrap();
    let write = header.find("#define MYGO_SLOT_STREAM_WRITE 1u\n").unwrap();
    assert!(exit < write);
    assert!(header.contains("#define MYGO_TARGET_ARCH 1u\n"));
    assert!(header.contains("#define MYGO_ABI_FAMILY 1u\n"));
    assert!(header.contains("#define MYGO_ABI_EPOCH 1u\n"));
    assert!(header.contains("#define MYGO_FEATURE_STATIC_TLS UINT64_C(1)\n"));
    assert!(header.contains("#define MYGO_CALL_SLOT_COUNT 2u\n"));
    assert!(header.contains("#define MYGO_CAPABILITY_COUNT 2u\n"));
    assert!(header.contains("#define MYGO_RUNTIME_STACK_SIZE UINT64_C(65536)\n"));
    assert!(header.contains("#define MYGO_RUNTIME_STACK_GUARD_SIZE UINT64_C(4096)\n"));
    assert!(header.contains("#define MYGO_START_INFO_MAX_SIZE 4096u\n"));
    assert!(header.ends_with('\n'));
}

#[test]
fn c_header_compiles_with_wire_layout_and_registry_values() {
    let contract = parse_manifest(MANIFEST).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "soyo-bindings-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&directory).unwrap();
    let header_path = directory.join("mygo_program.h");
    let probe_path = directory.join("probe.c");
    fs::write(
        &header_path,
        generate_c_header(TargetArch::Riscv64, &contract),
    )
    .unwrap();
    fs::write(
        &probe_path,
        r#"
#include "mygo_program.h"

_Static_assert(sizeof(struct mygo_string_ref) == 8, "StringRef size");
_Static_assert(sizeof(struct mygo_start_info) == 192, "StartInfo size");
_Static_assert(offsetof(struct mygo_start_info, image_base) == 0x20, "image_base offset");
_Static_assert(offsetof(struct mygo_start_info, initial_handle_offset) == 0x68, "handle offset");
_Static_assert(offsetof(struct mygo_start_info, random_seed) == 0x70, "seed offset");
_Static_assert(offsetof(struct mygo_start_info, reserved2) == 0x98, "reserved offset");
_Static_assert(sizeof(struct mygo_initial_handle) == 32, "InitialHandle size");
_Static_assert(offsetof(struct mygo_initial_handle, handle) == 0x08, "handle value offset");
_Static_assert(offsetof(struct mygo_initial_handle, granted_rights) == 0x10, "rights offset");
_Static_assert(sizeof(struct mygo_native_call) == 64, "call size");
_Static_assert(sizeof(struct mygo_native_result) == 24, "result size");
_Static_assert(MYGO_START_INFO_IMAGE_BASE_OFFSET == 0x20u, "image base wire offset");
_Static_assert(MYGO_START_INFO_INITIAL_HANDLE_OFFSET_OFFSET == 0x68u, "handle wire offset");
_Static_assert(MYGO_INITIAL_HANDLE_GRANTED_RIGHTS_OFFSET == 0x10u, "rights wire offset");

_Static_assert(MYGO_INTERFACE_PROCESS == 1u, "process interface");
_Static_assert(MYGO_INTERFACE_STREAM == 3u, "stream interface");
_Static_assert(MYGO_REQUIREMENT_SELF_PROCESS == 1u, "self requirement");
_Static_assert(MYGO_REQUIREMENT_STDOUT == 4u, "stdout requirement");
_Static_assert(MYGO_RIGHT_WRITE == UINT64_C(2), "write right");
_Static_assert(MYGO_RIGHT_TERMINATE_SELF == UINT64_C(16), "terminate right");
_Static_assert(MYGO_STATUS_OK == UINT32_C(0x00000000), "OK status");
_Static_assert(MYGO_STATUS_IO_CLOSED == UINT32_C(0x05000003), "closed status");
_Static_assert(MYGO_STATUS_IO_ERROR == UINT32_C(0x05000004), "I/O status");
_Static_assert(MYGO_CAP_SELF_PROCESS_REQUIRED == 1u, "self required");
_Static_assert(MYGO_CAP_SELF_PROCESS_RIGHTS == UINT64_C(16), "self rights");
_Static_assert(MYGO_CAP_STDOUT_REQUIRED == 1u, "stdout required");
_Static_assert(MYGO_CAP_STDOUT_RIGHTS == UINT64_C(2), "stdout rights");

int main(void) { return 0; }
"#,
    )
    .unwrap();

    let output = Command::new("clang")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-fsyntax-only"])
        .arg(&probe_path)
        .output()
        .expect("应能启动 clang");
    let _ = fs::remove_dir_all(&directory);
    assert!(
        output.status.success(),
        "生成的 C header 无法编译: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

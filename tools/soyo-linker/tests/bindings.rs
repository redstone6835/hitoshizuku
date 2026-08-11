use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use native_abi::TargetArch;
use soyo_linker::bindings::generate_c_header;
use soyo_linker::contract::parse_manifest;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const MANIFEST: &str = r#"
{
  "manifest_version": 1,
  "abi_epoch": 1,
  "entry": "_start",
  "imports": [
    { "operation": "stream.write", "required": true },
    { "operation": "process.exit", "required": true }
  ],
  "capabilities": [
    { "requirement": "stdout", "rights": ["write"], "required": true },
    { "requirement": "self_process", "rights": ["exit"], "required": true }
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

    let exit = header.find("#define MYGO_SLOT_process_exit 0u\n").unwrap();
    let write = header.find("#define MYGO_SLOT_stream_write 1u\n").unwrap();
    assert!(exit < write);
    assert!(header.contains("#define MYGO_TARGET_ARCH 1u\n"));
    assert!(header.contains("#define MYGO_ABI_FAMILY 1u\n"));
    assert!(header.contains("#define MYGO_ABI_EPOCH 1u\n"));
    assert!(header.contains("#define MYGO_FEATURE_STATIC_TLS UINT64_C(1)\n"));
    assert!(header.contains("#define MYGO_FEATURE_INIT_FINI_ARRAY UINT64_C(2)\n"));
    assert!(header.contains("#define MYGO_FEATURE_DYNAMIC_COMPONENTS UINT64_C(4)\n"));
    assert!(header.contains("#define MYGO_RUNTIME_RUN_INIT_ARRAY UINT64_C(1)\n"));
    assert!(header.contains("#define MYGO_RUNTIME_RUN_FINI_ARRAY UINT64_C(2)\n"));
    assert!(header.contains("#define MYGO_CALL_SLOT_COUNT 2u\n"));
    assert!(header.contains("#define MYGO_HAS_image_create 0u\n"));
    assert!(header.contains("#define MYGO_SLOT_image_create UINT64_MAX\n"));
    assert!(header.contains("#define MYGO_CAPABILITY_COUNT 2u\n"));
    assert!(header.contains("#define MYGO_CAPABILITY_CONTRACT(X) \\\n"));
    assert!(
        header
            .contains("X(MYGO_REQUIREMENT_self_process, MYGO_INTERFACE_process, UINT64_C(32), 1u)")
    );
    assert!(header.contains("X(MYGO_REQUIREMENT_stdout, MYGO_INTERFACE_stream, UINT64_C(2), 1u)"));
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
_Static_assert(offsetof(struct mygo_start_info, init_array_offset) == 0x98, "init offset");
_Static_assert(offsetof(struct mygo_start_info, init_array_count) == 0xa0, "init count");
_Static_assert(offsetof(struct mygo_start_info, init_array_entry_size) == 0xa4, "init size");
_Static_assert(offsetof(struct mygo_start_info, fini_array_offset) == 0xa8, "fini offset");
_Static_assert(offsetof(struct mygo_start_info, fini_array_count) == 0xb0, "fini count");
_Static_assert(offsetof(struct mygo_start_info, fini_array_entry_size) == 0xb4, "fini size");
_Static_assert(offsetof(struct mygo_start_info, reserved4) == 0xb8, "reserved offset");
_Static_assert(sizeof(struct mygo_initial_handle) == 32, "InitialHandle size");
_Static_assert(offsetof(struct mygo_initial_handle, handle) == 0x08, "handle value offset");
_Static_assert(offsetof(struct mygo_initial_handle, granted_rights) == 0x10, "rights offset");
_Static_assert(sizeof(struct mygo_native_call) == 64, "call size");
_Static_assert(sizeof(struct mygo_native_result) == 24, "result size");
_Static_assert(sizeof(mygo_spawn_request) == 64, "spawn size");
_Static_assert(sizeof(mygo_process_result) == 32, "process result size");
_Static_assert(sizeof(mygo_image_info) == 144, "image info size");
_Static_assert(offsetof(mygo_image_info, component_identity) == 0x20, "image component offset");
_Static_assert(offsetof(mygo_image_info, content_hash) == 0x60, "image hash offset");
_Static_assert(sizeof(mygo_event_record) == 40, "event record size");
_Static_assert(sizeof(mygo_component_load_request) == 64, "component load size");
_Static_assert(sizeof(mygo_component_lifecycle) == 64, "component lifecycle size");
_Static_assert(sizeof(mygo_component_query) == 64, "component query size");
_Static_assert(sizeof(mygo_interface_request) == 48, "interface request size");
_Static_assert(sizeof(mygo_component_call_state) == 64, "component call state size");
_Static_assert(sizeof(mygo_component_context) == 64, "component context size");
_Static_assert(sizeof(mygo_component_capability_record) == 32, "component capability size");
_Static_assert(sizeof(mygo_component_interface_gate) == 32, "component gate size");
_Static_assert(sizeof(mygo_thread_create_request) == 64, "thread create size");
_Static_assert(sizeof(mygo_thread_result) == 32, "thread result size");
_Static_assert(sizeof(mygo_thread_info) == 48, "thread info size");
_Static_assert(sizeof(mygo_memory_create_request) == 64, "memory create size");
_Static_assert(sizeof(mygo_memory_map_request) == 64, "memory map size");
_Static_assert(sizeof(mygo_memory_info) == 64, "memory info size");
_Static_assert(sizeof(mygo_memory_region) == 32, "memory region size");
_Static_assert(sizeof(mygo_path_ref) == 16, "path ref size");
_Static_assert(sizeof(mygo_directory_request) == 64, "directory request size");
_Static_assert(sizeof(mygo_directory_info) == 64, "directory info size");
_Static_assert(sizeof(mygo_file_info) == 64, "file info size");
_Static_assert(sizeof(mygo_channel_handle_transfer) == 32, "channel transfer size");
_Static_assert(sizeof(mygo_channel_message) == 64, "channel message size");
_Static_assert(sizeof(mygo_submission_descriptor) == 64, "submission size");
_Static_assert(sizeof(mygo_completion_record) == 32, "completion size");
_Static_assert(sizeof(mygo_ring_shared_state) == 64, "ring shared state size");
_Static_assert(sizeof(mygo_ring_info) == 64, "ring info size");
_Static_assert(offsetof(mygo_thread_create_request, tls_memory) == 0x20, "thread tls offset");
_Static_assert(offsetof(mygo_memory_map_request, permissions) == 0x28, "memory permissions offset");
_Static_assert(offsetof(mygo_directory_request, path) == 0x00, "directory path offset");
_Static_assert(offsetof(mygo_file_info, granted_rights) == 0x20, "file rights offset");
_Static_assert(offsetof(mygo_channel_message, handles_ptr) == 0x10, "channel handles offset");
_Static_assert(offsetof(mygo_ring_shared_state, sq_head) == 0x10, "ring SQ head offset");
_Static_assert(offsetof(mygo_ring_shared_state, cq_tail) == 0x1c, "ring CQ tail offset");
_Static_assert(offsetof(mygo_ring_shared_state, generation) == 0x30, "ring generation offset");
_Static_assert(MYGO_START_INFO_IMAGE_BASE_OFFSET == 0x20u, "image base wire offset");
_Static_assert(MYGO_START_INFO_INITIAL_HANDLE_OFFSET_OFFSET == 0x68u, "handle wire offset");
_Static_assert(MYGO_START_INFO_INIT_ARRAY_OFFSET_OFFSET == 0x98u, "init wire offset");
_Static_assert(MYGO_START_INFO_FINI_ARRAY_OFFSET_OFFSET == 0xa8u, "fini wire offset");
_Static_assert(MYGO_INITIAL_HANDLE_GRANTED_RIGHTS_OFFSET == 0x10u, "rights wire offset");
_Static_assert(MYGO_HANDLE_TRANSFER_SOURCE_HANDLE_OFFSET == 0x08u, "transfer source offset");
_Static_assert(MYGO_SPAWN_REQUEST_TRANSFERS_OFFSET == 0x28u, "transfer list offset");
_Static_assert(MYGO_PROCESS_RESULT_DETAIL1_OFFSET == 0x18u, "fault address offset");
_Static_assert(MYGO_EVENT_RECORD_VALUE1_OFFSET == 0x20u, "event value offset");
_Static_assert(MYGO_COMPONENT_LOAD_REQUEST_BINDINGS_OFFSET == 0x18u, "component bindings offset");
_Static_assert(MYGO_COMPONENT_LIFECYCLE_ENTRY_OFFSET == 0x10u, "component entry offset");
_Static_assert(MYGO_COMPONENT_QUERY_ACTIVE_CALLS_OFFSET == 0x30u, "component active offset");
_Static_assert(MYGO_INTERFACE_REQUEST_SIGNATURE_HASH_OFFSET == 0x10u, "interface hash offset");
_Static_assert(MYGO_COMPONENT_CALL_STATE_ACTIVE_CALLS_OFFSET == 0x10u, "call count offset");
_Static_assert(MYGO_COMPONENT_CONTEXT_CALL_STATE_OFFSET == 0x08u, "context call state offset");
_Static_assert(MYGO_COMPONENT_CONTEXT_CALL_SLOT_COUNT_OFFSET == 0x20u, "context slot count offset");
_Static_assert(MYGO_COMPONENT_CONTEXT_CAPABILITY_COUNT_OFFSET == 0x28u, "context capability count offset");
_Static_assert(MYGO_COMPONENT_CONTEXT_CAPABILITIES_OFFSET == 0x30u, "context capabilities offset");
_Static_assert(MYGO_COMPONENT_CAPABILITY_RECORD_HANDLE_OFFSET == 0x08u, "component capability handle offset");
_Static_assert(MYGO_COMPONENT_CAPABILITY_RECORD_GRANTED_RIGHTS_OFFSET == 0x10u, "component capability rights offset");
_Static_assert(MYGO_COMPONENT_INTERFACE_GATE_COMPONENT_OFFSET == 0x10u, "gate component offset");

_Static_assert(MYGO_INTERFACE_process == 1u, "process interface");
_Static_assert(MYGO_INTERFACE_stream == 3u, "stream interface");
_Static_assert(MYGO_INTERFACE_image == 5u, "image interface");
_Static_assert(MYGO_INTERFACE_component == 7u, "component interface");
_Static_assert(MYGO_INTERFACE_component_transaction == 8u, "transaction interface");
_Static_assert(MYGO_INTERFACE_interface == 9u, "exported interface");
_Static_assert(MYGO_INTERFACE_thread == 10u, "thread interface");
_Static_assert(MYGO_INTERFACE_memory_object == 11u, "memory object interface");
_Static_assert(MYGO_INTERFACE_directory == 12u, "directory interface");
_Static_assert(MYGO_INTERFACE_file == 13u, "file interface");
_Static_assert(MYGO_INTERFACE_channel == 14u, "channel interface");
_Static_assert(MYGO_INTERFACE_submission_ring == 15u, "ring interface");
_Static_assert(MYGO_REQUIREMENT_self_process == 1u, "self requirement");
_Static_assert(MYGO_REQUIREMENT_stdout == 4u, "stdout requirement");
_Static_assert(MYGO_RIGHT_write == UINT64_C(2), "write right");
_Static_assert(MYGO_RIGHT_exit == UINT64_C(32), "terminate right");
_Static_assert(MYGO_RIGHT_load == UINT64_C(32768), "load right");
_Static_assert(MYGO_RIGHT_unload == UINT64_C(65536), "unload right");
_Static_assert(MYGO_RIGHT_register == UINT64_C(8388608), "register right");
_Static_assert(MYGO_RIGHT_submit == UINT64_C(16777216), "submit right");
_Static_assert(MYGO_RIGHT_cancel == UINT64_C(33554432), "cancel right");
_Static_assert(MYGO_OPERATION_component_load == 21u, "component load operation");
_Static_assert(MYGO_OPERATION_component_wake == 27u, "component wake operation");
_Static_assert(MYGO_OPERATION_image_query == 66u, "image query operation");
_Static_assert(MYGO_OPERATION_ring_create == 48u, "ring create operation");
_Static_assert(MYGO_OPERATION_ring_wait == 53u, "ring wait operation");
_Static_assert(MYGO_OPERATION_ring_query == 54u, "ring query operation");
_Static_assert(MYGO_IMAGE_ARTIFACT_SHARED_COMPONENT == 2u, "component artifact");
_Static_assert(MYGO_STATUS_ok == UINT32_C(0x00000000), "OK status");
_Static_assert(MYGO_STATUS_stream_closed == UINT32_C(0x05000004), "closed status");
_Static_assert(MYGO_STATUS_stream_error == UINT32_C(0x05000005), "I/O status");
_Static_assert(MYGO_STATUS_component_unloaded == UINT32_C(0x0a00000a), "component unloaded status");
_Static_assert(MYGO_STATUS_thread_invalid == UINT32_C(0x0b000001), "thread status");
_Static_assert(MYGO_STATUS_filesystem_invalid_path == UINT32_C(0x0c000001), "filesystem status");
_Static_assert(MYGO_STATUS_channel_full == UINT32_C(0x0d000001), "channel status");
_Static_assert(MYGO_STATUS_ring_full == UINT32_C(0x0e000001), "ring full status");
_Static_assert(MYGO_STATUS_ring_cancelled == UINT32_C(0x0e000006), "ring cancelled status");
_Static_assert(MYGO_RING_SHARED_MAGIC == UINT32_C(1735289202), "ring shared magic");
_Static_assert(MYGO_RING_SHARED_VERSION == 1u, "ring shared version");
_Static_assert(MYGO_PROCESS_STATE_FAULTED == 4u, "fault state");
_Static_assert(MYGO_PROCESS_FAULT_MEMORY == 1u, "memory fault");
_Static_assert(MYGO_EVENT_KIND_TIMER_EXPIRED == 4u, "timer event");
_Static_assert(MYGO_HANDLE_TRANSFER_MOVE == UINT64_C(1), "move transfer");
_Static_assert(MYGO_COMPONENT_ACTION_INITIALIZE == 1u, "initialize action");
_Static_assert(MYGO_COMPONENT_STATE_ACTIVE == 3u, "active state");
_Static_assert(MYGO_CAP_self_process_required == 1u, "self required");
_Static_assert(MYGO_CAP_self_process_rights == UINT64_C(32), "self rights");
_Static_assert(MYGO_CAP_stdout_required == 1u, "stdout required");
_Static_assert(MYGO_CAP_stdout_rights == UINT64_C(2), "stdout rights");

int main(void) { return 0; }
"#,
    )
    .unwrap();

    let output = Command::new("clang")
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-Wno-unused-command-line-argument",
            "-fsyntax-only",
        ])
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

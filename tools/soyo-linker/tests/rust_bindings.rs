use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use native_abi::TargetArch;
use soyo_linker::contract::parse_manifest;
use soyo_linker::rust_bindings::generate_rust_module;

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
const _: () = assert!(MYGO_FEATURE_DYNAMIC_COMPONENTS == 4);
const _: () = assert!(MYGO_SLOT_process_exit == 0);
const _: () = assert!(MYGO_SLOT_stream_write == 1);
const _: () = assert!(!MYGO_HAS_image_create);
const _: () = assert!(MYGO_SLOT_image_create == u64::MAX);
const _: () = assert!(MYGO_REQUIREMENT_self_process == 1);
const _: () = assert!(MYGO_REQUIREMENT_stdout == 4);
const _: () = assert!(MYGO_RIGHT_write == 2);
const _: () = assert!(MYGO_RIGHT_exit == 32);
const _: () = assert!(MYGO_RIGHT_load == 32768);
const _: () = assert!(MYGO_RIGHT_unload == 65536);
const _: () = assert!(MYGO_INTERFACE_image == 5);
const _: () = assert!(MYGO_INTERFACE_component == 7);
const _: () = assert!(MYGO_INTERFACE_component_transaction == 8);
const _: () = assert!(MYGO_INTERFACE_interface == 9);
const _: () = assert!(MYGO_OPERATION_component_load == 21);
const _: () = assert!(MYGO_OPERATION_component_wake == 27);
const _: () = assert!(MYGO_OPERATION_image_query == 66);
const _: () = assert!(MYGO_IMAGE_ARTIFACT_SHARED_COMPONENT == 2);
const _: () = assert!(MYGO_STATUS_ok == 0x0000_0000);
const _: () = assert!(MYGO_STATUS_stream_closed == 0x0500_0004);
const _: () = assert!(MYGO_STATUS_component_unloaded == 0x0a00_000a);
const _: () = assert!(MYGO_CAP_self_process_rights == 32);
const _: () = assert!(MYGO_CAP_stdout_rights == 2);
const _: () = assert!(core::mem::size_of::<MygoNativeCall>() == 64);
const _: () = assert!(core::mem::offset_of!(MygoNativeCall, args) == 16);
const _: () = assert!(core::mem::size_of::<MygoNativeResult>() == 24);
const _: () = assert!(core::mem::offset_of!(MygoNativeResult, value0) == 8);
const _: () = assert!(MYGO_PROCESS_STATE_FAULTED == 4);
const _: () = assert!(MYGO_PROCESS_FAULT_MEMORY == 1);
const _: () = assert!(MYGO_EVENT_KIND_TIMER_EXPIRED == 4);
const _: () = assert!(MYGO_HANDLE_TRANSFER_MOVE == 1);
const _: () = assert!(core::mem::size_of::<MygoSpawnRequest>() == 64);
const _: () = assert!(core::mem::offset_of!(MygoSpawnRequest, transfers) == 40);
const _: () = assert!(core::mem::size_of::<MygoProcessResult>() == 32);
const _: () = assert!(core::mem::offset_of!(MygoProcessResult, detail1) == 24);
const _: () = assert!(core::mem::size_of::<MygoImageInfo>() == 144);
const _: () = assert!(core::mem::offset_of!(MygoImageInfo, component_identity) == 32);
const _: () = assert!(core::mem::offset_of!(MygoImageInfo, content_hash) == 96);
const _: () = assert!(core::mem::size_of::<MygoEventRecord>() == 40);
const _: () = assert!(core::mem::offset_of!(MygoEventRecord, value1) == 32);
const _: () = assert!(core::mem::size_of::<MygoComponentLoadRequest>() == 64);
const _: () = assert!(core::mem::offset_of!(MygoComponentLoadRequest, bindings) == 24);
const _: () = assert!(core::mem::size_of::<MygoComponentLifecycle>() == 64);
const _: () = assert!(core::mem::offset_of!(MygoComponentLifecycle, entry) == 16);
const _: () = assert!(core::mem::size_of::<MygoComponentQuery>() == 64);
const _: () = assert!(core::mem::offset_of!(MygoComponentQuery, active_calls) == 48);
const _: () = assert!(core::mem::size_of::<MygoInterfaceRequest>() == 48);
const _: () = assert!(core::mem::size_of::<MygoComponentCallState>() == 64);
const _: () = assert!(core::mem::size_of::<MygoComponentContext>() == 64);
const _: () = assert!(core::mem::offset_of!(MygoComponentContext, call_state) == 8);
const _: () = assert!(core::mem::offset_of!(MygoComponentContext, call_slot_count) == 32);
const _: () = assert!(core::mem::offset_of!(MygoComponentContext, capability_count) == 40);
const _: () = assert!(core::mem::offset_of!(MygoComponentContext, capabilities) == 48);
const _: () = assert!(core::mem::size_of::<MygoComponentCapabilityRecord>() == 32);
const _: () = assert!(core::mem::offset_of!(MygoComponentCapabilityRecord, handle) == 8);
const _: () = assert!(core::mem::offset_of!(MygoComponentCapabilityRecord, granted_rights) == 16);
const _: () = assert!(core::mem::size_of::<MygoComponentInterfaceGate>() == 32);
const _: () = assert!(core::mem::offset_of!(MygoComponentInterfaceGate, component) == 16);
const _: () = assert!(MYGO_INTERFACE_thread == 10);
const _: () = assert!(MYGO_INTERFACE_memory_object == 11);
const _: () = assert!(MYGO_INTERFACE_directory == 12);
const _: () = assert!(MYGO_INTERFACE_file == 13);
const _: () = assert!(MYGO_INTERFACE_channel == 14);
const _: () = assert!(MYGO_INTERFACE_submission_ring == 15);
const _: () = assert!(MYGO_RIGHT_register == 8_388_608);
const _: () = assert!(MYGO_RIGHT_submit == 16_777_216);
const _: () = assert!(MYGO_RIGHT_cancel == 33_554_432);
const _: () = assert!(MYGO_OPERATION_ring_create == 48);
const _: () = assert!(MYGO_OPERATION_ring_wait == 53);
const _: () = assert!(MYGO_OPERATION_ring_query == 54);
const _: () = assert!(MYGO_STATUS_thread_invalid == 0x0b00_0001);
const _: () = assert!(MYGO_STATUS_filesystem_invalid_path == 0x0c00_0001);
const _: () = assert!(MYGO_STATUS_channel_full == 0x0d00_0001);
const _: () = assert!(MYGO_STATUS_ring_full == 0x0e00_0001);
const _: () = assert!(MYGO_STATUS_ring_cancelled == 0x0e00_0006);
const _: () = assert!(MYGO_RING_SHARED_MAGIC == 1_735_289_202);
const _: () = assert!(MYGO_RING_SHARED_VERSION == 1);
const _: () = assert!(core::mem::size_of::<MygoThreadCreateRequest>() == 64);
const _: () = assert!(core::mem::offset_of!(MygoThreadCreateRequest, tls_memory) == 32);
const _: () = assert!(core::mem::size_of::<MygoThreadResult>() == 32);
const _: () = assert!(core::mem::size_of::<MygoThreadInfo>() == 48);
const _: () = assert!(core::mem::size_of::<MygoMemoryCreateRequest>() == 64);
const _: () = assert!(core::mem::size_of::<MygoMemoryMapRequest>() == 64);
const _: () = assert!(core::mem::offset_of!(MygoMemoryMapRequest, permissions) == 40);
const _: () = assert!(core::mem::size_of::<MygoMemoryInfo>() == 64);
const _: () = assert!(core::mem::size_of::<MygoMemoryRegion>() == 32);
const _: () = assert!(core::mem::size_of::<MygoPathRef>() == 16);
const _: () = assert!(core::mem::size_of::<MygoDirectoryRequest>() == 64);
const _: () = assert!(core::mem::size_of::<MygoDirectoryInfo>() == 64);
const _: () = assert!(core::mem::size_of::<MygoFileInfo>() == 64);
const _: () = assert!(core::mem::size_of::<MygoChannelHandleTransfer>() == 32);
const _: () = assert!(core::mem::size_of::<MygoChannelMessage>() == 64);
const _: () = assert!(core::mem::size_of::<MygoSubmissionDescriptor>() == 64);
const _: () = assert!(core::mem::size_of::<MygoCompletionRecord>() == 32);
const _: () = assert!(core::mem::size_of::<MygoRingSharedState>() == 64);
const _: () = assert!(core::mem::offset_of!(MygoRingSharedState, sq_head) == 16);
const _: () = assert!(core::mem::offset_of!(MygoRingSharedState, cq_tail) == 28);
const _: () = assert!(core::mem::offset_of!(MygoRingSharedState, generation) == 48);
const _: () = assert!(core::mem::size_of::<MygoRingInfo>() == 64);
const _: () = assert!(MYGO_COMPONENT_ACTION_INITIALIZE == 1);
const _: () = assert!(MYGO_COMPONENT_STATE_ACTIVE == 3);
"#,
    )
    .unwrap();

    let output = Command::new("rustc")
        .args(["--edition=2024", "--crate-type=lib", "--deny=warnings"])
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

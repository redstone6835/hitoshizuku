//! 程序 manifest 到 C ABI binding 的确定性投影。

use std::fmt::Write;

use native_abi::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, INTERFACES, REQUIREMENTS, RIGHTS, TargetArch, status,
    wire as native_wire,
};
use soyo::registry::FeatureFlags;
use soyo::registry::RuntimeFlags;

use crate::contract::ProgramContract;

fn public_ident(name: &str) -> String {
    name.replace('.', "_")
}

pub fn generate_c_header(target: TargetArch, contract: &ProgramContract) -> Vec<u8> {
    let mut output = String::new();
    writeln!(output, "#ifndef MYGO_PROGRAM_H").unwrap();
    writeln!(output, "#define MYGO_PROGRAM_H").unwrap();
    writeln!(output).unwrap();
    writeln!(output, "#include <stddef.h>").unwrap();
    writeln!(output, "#include <stdint.h>").unwrap();
    writeln!(output).unwrap();
    writeln!(output, "#define MYGO_TARGET_ARCH {}u", target as u16).unwrap();
    writeln!(
        output,
        "#define MYGO_ABI_FAMILY {}u",
        ABI_FAMILY_MYGO_NATIVE
    )
    .unwrap();
    writeln!(output, "#define MYGO_ABI_EPOCH {}u", ABI_EPOCH).unwrap();
    writeln!(
        output,
        "#define MYGO_PAGE_SIZE UINT64_C({})",
        native_abi::PAGE_SIZE
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_FEATURE_STATIC_TLS UINT64_C({})",
        FeatureFlags::STATIC_TLS.bits()
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_FEATURE_INIT_FINI_ARRAY UINT64_C({})",
        FeatureFlags::INIT_FINI_ARRAY.bits()
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_FEATURE_DYNAMIC_COMPONENTS UINT64_C({})",
        FeatureFlags::DYNAMIC_COMPONENTS.bits()
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_RUNTIME_RUN_INIT_ARRAY UINT64_C({})",
        RuntimeFlags::RUN_INIT_ARRAY.bits()
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_RUNTIME_RUN_FINI_ARRAY UINT64_C({})",
        RuntimeFlags::RUN_FINI_ARRAY.bits()
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_CALL_SLOT_COUNT {}u",
        contract.imports().len()
    )
    .unwrap();
    for operation_spec in native_abi::OPERATIONS {
        let slot = contract
            .imports()
            .iter()
            .position(|import| import.operation == operation_spec.id);
        writeln!(
            output,
            "#define MYGO_HAS_{} {}u",
            public_ident(operation_spec.name),
            u32::from(slot.is_some())
        )
        .unwrap();
        writeln!(
            output,
            "#define MYGO_SLOT_{} {}",
            public_ident(operation_spec.name),
            slot.map_or_else(|| "UINT64_MAX".to_string(), |slot| format!("{slot}u"))
        )
        .unwrap();
    }
    writeln!(
        output,
        "#define MYGO_RUNTIME_STACK_SIZE UINT64_C({})",
        contract.runtime().stack_size
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_RUNTIME_STACK_GUARD_SIZE UINT64_C({})",
        contract.runtime().stack_guard_size
    )
    .unwrap();
    writeln!(
        output,
        "#define MYGO_START_INFO_MAX_SIZE {}u",
        contract.runtime().start_info_max_size
    )
    .unwrap();
    writeln!(output).unwrap();

    write_registry_definitions(&mut output);
    write_capability_definitions(&mut output, contract);
    write_wire_types(&mut output);

    writeln!(output, "#endif").unwrap();
    output.into_bytes()
}

fn write_registry_definitions(output: &mut String) {
    for spec in INTERFACES {
        writeln!(
            output,
            "#define MYGO_INTERFACE_{} {}u",
            spec.name, spec.interface as u16
        )
        .unwrap();
    }

    writeln!(output, "#define MYGO_RIGHT_NONE UINT64_C(0)").unwrap();
    for right in RIGHTS {
        writeln!(
            output,
            "#define MYGO_RIGHT_{} UINT64_C({})",
            public_ident(right.name),
            right.right.bits()
        )
        .unwrap();
    }

    for requirement in REQUIREMENTS {
        writeln!(
            output,
            "#define MYGO_REQUIREMENT_{} {}u",
            public_ident(requirement.name),
            requirement.id as u32
        )
        .unwrap();
    }

    for operation in native_abi::OPERATIONS {
        writeln!(
            output,
            "#define MYGO_OPERATION_{} {}u",
            public_ident(operation.name),
            operation.id as u32
        )
        .unwrap();
    }

    for (name, value) in [
        ("PROCESS_STATE_RUNNING", native_wire::PROCESS_STATE_RUNNING),
        ("PROCESS_STATE_TERMINATING", native_wire::PROCESS_STATE_TERMINATING),
        ("PROCESS_STATE_EXITED", native_wire::PROCESS_STATE_EXITED),
        ("PROCESS_STATE_FAULTED", native_wire::PROCESS_STATE_FAULTED),
        ("PROCESS_STATE_REAPED", native_wire::PROCESS_STATE_REAPED),
        ("PROCESS_FAULT_MEMORY", native_wire::PROCESS_FAULT_MEMORY),
        ("PROCESS_FAULT_ILLEGAL_INSTRUCTION", native_wire::PROCESS_FAULT_ILLEGAL_INSTRUCTION),
        ("PROCESS_FAULT_BREAKPOINT", native_wire::PROCESS_FAULT_BREAKPOINT),
        ("PROCESS_FAULT_ADDRESS", native_wire::PROCESS_FAULT_ADDRESS),
        ("PROCESS_FAULT_ARITHMETIC", native_wire::PROCESS_FAULT_ARITHMETIC),
        ("PROCESS_FAULT_RESOURCE", native_wire::PROCESS_FAULT_RESOURCE),
        ("PROCESS_FAULT_OTHER", native_wire::PROCESS_FAULT_OTHER),
        ("EVENT_KIND_PROCESS_EXITED", native_wire::EVENT_KIND_PROCESS_EXITED),
        ("EVENT_KIND_PROCESS_FAULT", native_wire::EVENT_KIND_PROCESS_FAULT),
        ("EVENT_KIND_STREAM_READY", native_wire::EVENT_KIND_STREAM_READY),
        ("EVENT_KIND_TIMER_EXPIRED", native_wire::EVENT_KIND_TIMER_EXPIRED),
        ("EVENT_STREAM_READABLE", native_wire::EVENT_STREAM_READABLE),
        ("EVENT_STREAM_WRITABLE", native_wire::EVENT_STREAM_WRITABLE),
        ("EVENT_STREAM_ERROR", native_wire::EVENT_STREAM_ERROR),
        ("EVENT_STREAM_CLOSED", native_wire::EVENT_STREAM_CLOSED),
        ("COMPONENT_ACTION_NONE", native_wire::COMPONENT_ACTION_NONE),
        (
            "COMPONENT_ACTION_INITIALIZE",
            native_wire::COMPONENT_ACTION_INITIALIZE,
        ),
        (
            "COMPONENT_ACTION_FINALIZE",
            native_wire::COMPONENT_ACTION_FINALIZE,
        ),
        (
            "COMPONENT_STATE_PREPARING",
            native_wire::COMPONENT_STATE_PREPARING,
        ),
        (
            "COMPONENT_STATE_INITIALIZING",
            native_wire::COMPONENT_STATE_INITIALIZING,
        ),
        ("COMPONENT_STATE_ACTIVE", native_wire::COMPONENT_STATE_ACTIVE),
        (
            "COMPONENT_STATE_DRAINING",
            native_wire::COMPONENT_STATE_DRAINING,
        ),
        (
            "COMPONENT_STATE_FINALIZING",
            native_wire::COMPONENT_STATE_FINALIZING,
        ),
        (
            "COMPONENT_STATE_UNLOADED",
            native_wire::COMPONENT_STATE_UNLOADED,
        ),
        ("COMPONENT_STATE_FAILED", native_wire::COMPONENT_STATE_FAILED),
        ("IMAGE_ARTIFACT_EXECUTABLE", native_wire::IMAGE_ARTIFACT_EXECUTABLE),
        ("IMAGE_ARTIFACT_SHARED_COMPONENT", native_wire::IMAGE_ARTIFACT_SHARED_COMPONENT),
        ("THREAD_STATE_RUNNING", native_wire::THREAD_STATE_RUNNING),
        ("THREAD_STATE_EXITED", native_wire::THREAD_STATE_EXITED),
        ("THREAD_STATE_FAULTED", native_wire::THREAD_STATE_FAULTED),
        ("MEMORY_KIND_ANONYMOUS", native_wire::MEMORY_KIND_ANONYMOUS),
        ("MEMORY_KIND_FILE", native_wire::MEMORY_KIND_FILE),
        ("MEMORY_KIND_IMAGE", native_wire::MEMORY_KIND_IMAGE),
        ("MEMORY_KIND_DMA", native_wire::MEMORY_KIND_DMA),
        ("MEMORY_FLAG_SHARED", native_wire::MEMORY_FLAG_SHARED),
        ("MEMORY_FLAG_DEVICE_READ", native_wire::MEMORY_FLAG_DEVICE_READ),
        ("MEMORY_FLAG_DEVICE_WRITE", native_wire::MEMORY_FLAG_DEVICE_WRITE),
        ("MEMORY_STATE_ACTIVE", native_wire::MEMORY_STATE_ACTIVE),
        ("MEMORY_STATE_REVOKED", native_wire::MEMORY_STATE_REVOKED),
        ("MEMORY_STATE_POISONED", native_wire::MEMORY_STATE_POISONED),
        ("MEMORY_PERMISSION_READ", native_wire::MEMORY_PERMISSION_READ),
        ("MEMORY_PERMISSION_WRITE", native_wire::MEMORY_PERMISSION_WRITE),
        ("MEMORY_PERMISSION_EXECUTE", native_wire::MEMORY_PERMISSION_EXECUTE),
        ("MEMORY_MAP_FIXED", native_wire::MEMORY_MAP_FIXED),
        ("DIRECTORY_ENTRY_FILE", native_wire::DIRECTORY_ENTRY_FILE),
        ("DIRECTORY_ENTRY_DIRECTORY", native_wire::DIRECTORY_ENTRY_DIRECTORY),
        ("DIRECTORY_REMOVE_DIRECTORY", native_wire::DIRECTORY_REMOVE_DIRECTORY),
    ] {
        writeln!(output, "#define MYGO_{name} {value}u").unwrap();
    }
    writeln!(
        output,
        "#define MYGO_HANDLE_TRANSFER_MOVE UINT64_C({})",
        native_wire::HANDLE_TRANSFER_MOVE
    )
    .unwrap();
    writeln!(output, "#define MYGO_MAX_EVENT_PORT_CAPACITY {}u", native_wire::MAX_EVENT_PORT_CAPACITY).unwrap();
    writeln!(output, "#define MYGO_MAX_EVENT_BATCH {}u", native_wire::MAX_EVENT_BATCH).unwrap();
    writeln!(output, "#define MYGO_MAX_COMPONENT_IMAGES {}u", native_wire::MAX_COMPONENT_IMAGES).unwrap();
    writeln!(output, "#define MYGO_MAX_COMPONENT_BINDINGS {}u", native_wire::MAX_COMPONENT_BINDINGS).unwrap();
    writeln!(output, "#define MYGO_MAX_PATH_BYTES {}u", native_wire::MAX_PATH_BYTES).unwrap();
    writeln!(output, "#define MYGO_MAX_CHANNEL_MESSAGE_BYTES {}u", native_wire::MAX_CHANNEL_MESSAGE_BYTES).unwrap();
    writeln!(output, "#define MYGO_MAX_CHANNEL_MESSAGE_HANDLES {}u", native_wire::MAX_CHANNEL_MESSAGE_HANDLES).unwrap();
    writeln!(output, "#define MYGO_MAX_CHANNEL_QUEUE_MESSAGES {}u", native_wire::MAX_CHANNEL_QUEUE_MESSAGES).unwrap();
    writeln!(output, "#define MYGO_CHANNEL_TRANSFER_MOVE UINT64_C({})", native_wire::CHANNEL_TRANSFER_MOVE).unwrap();
    writeln!(output, "#define MYGO_MAX_RING_ENTRIES {}u", native_wire::MAX_RING_ENTRIES).unwrap();
    writeln!(output, "#define MYGO_MAX_RING_BATCH {}u", native_wire::MAX_RING_BATCH).unwrap();
    writeln!(output, "#define MYGO_MAX_RING_IO_BYTES {}u", native_wire::MAX_RING_IO_BYTES).unwrap();
    writeln!(output, "#define MYGO_RING_SHARED_MAGIC UINT32_C({})", native_wire::RING_SHARED_MAGIC).unwrap();
    writeln!(output, "#define MYGO_RING_SHARED_VERSION {}u", native_wire::RING_SHARED_VERSION).unwrap();
    for (name, value) in [
        ("NETWORK_FAMILY_IPV4", native_wire::NETWORK_FAMILY_IPV4),
        ("NETWORK_FAMILY_IPV6", native_wire::NETWORK_FAMILY_IPV6),
        ("SOCKET_KIND_STREAM", native_wire::SOCKET_KIND_STREAM),
        ("SOCKET_KIND_DATAGRAM", native_wire::SOCKET_KIND_DATAGRAM),
    ] {
        writeln!(output, "#define MYGO_{name} {value}u").unwrap();
    }
    for (name, value) in [
        ("SOCKET_SHUTDOWN_READ", native_wire::SOCKET_SHUTDOWN_READ),
        ("SOCKET_SHUTDOWN_WRITE", native_wire::SOCKET_SHUTDOWN_WRITE),
        ("SOCKET_SHUTDOWN_BOTH", native_wire::SOCKET_SHUTDOWN_BOTH),
    ] {
        writeln!(output, "#define MYGO_{name} {value}u").unwrap();
    }

    for (name, value) in [
        ("ok", status::OK),
        ("core.invalid_argument", status::CORE_INVALID_ARGUMENT),
        ("core.out_of_range", status::CORE_OUT_OF_RANGE),
        ("core.resource_exhausted", status::CORE_RESOURCE_EXHAUSTED),
        ("abi.bad_slot", status::ABI_BAD_SLOT),
        ("abi.signature_mismatch", status::ABI_SIGNATURE_MISMATCH),
        (
            "abi.unsupported_operation",
            status::ABI_UNSUPPORTED_OPERATION,
        ),
        ("handle.invalid", status::HANDLE_INVALID),
        ("handle.stale", status::HANDLE_STALE),
        ("handle.wrong_interface", status::HANDLE_WRONG_INTERFACE),
        ("security.rights_denied", status::SECURITY_RIGHTS_DENIED),
        ("stream.fault", status::STREAM_FAULT),
        ("stream.would_block", status::STREAM_WOULD_BLOCK),
        ("stream.end", status::STREAM_END),
        ("stream.closed", status::STREAM_CLOSED),
        ("stream.error", status::STREAM_ERROR),
        ("memory.invalid_range", status::MEMORY_INVALID_RANGE),
        ("memory.invalid_alignment", status::MEMORY_INVALID_ALIGNMENT),
        ("memory.not_owned", status::MEMORY_NOT_OWNED),
        ("memory.revoked", status::MEMORY_REVOKED),
        ("memory.poisoned", status::MEMORY_POISONED),
        ("process.not_child", status::PROCESS_NOT_CHILD),
        ("process.already_reaped", status::PROCESS_ALREADY_REAPED),
        ("process.would_block", status::PROCESS_WOULD_BLOCK),
        ("process.invalid_state", status::PROCESS_INVALID_STATE),
        ("process.wait_in_progress", status::PROCESS_WAIT_IN_PROGRESS),
        ("image.invalid", status::IMAGE_INVALID),
        ("image.arch_mismatch", status::IMAGE_ARCH_MISMATCH),
        ("image.not_executable", status::IMAGE_NOT_EXECUTABLE),
        ("event.invalid_token", status::EVENT_INVALID_TOKEN),
        ("event.source_unsupported", status::EVENT_SOURCE_UNSUPPORTED),
        ("event.would_block", status::EVENT_WOULD_BLOCK),
        ("event.timeout", status::EVENT_TIMEOUT),
        ("event.queue_exhausted", status::EVENT_QUEUE_EXHAUSTED),
        ("event.cancelled", status::EVENT_CANCELLED),
        ("component.invalid_image", status::COMPONENT_INVALID_IMAGE),
        (
            "component.dependency_missing",
            status::COMPONENT_DEPENDENCY_MISSING,
        ),
        (
            "component.dependency_conflict",
            status::COMPONENT_DEPENDENCY_CONFLICT,
        ),
        (
            "component.dependency_cycle",
            status::COMPONENT_DEPENDENCY_CYCLE,
        ),
        ("component.initializing", status::COMPONENT_INITIALIZING),
        ("component.active", status::COMPONENT_ACTIVE),
        ("component.in_use", status::COMPONENT_IN_USE),
        ("component.draining", status::COMPONENT_DRAINING),
        ("component.timeout", status::COMPONENT_TIMEOUT),
        ("component.unloaded", status::COMPONENT_UNLOADED),
        ("component.self_unload", status::COMPONENT_SELF_UNLOAD),
        (
            "component.lifecycle_failed",
            status::COMPONENT_LIFECYCLE_FAILED,
        ),
        (
            "component.invalid_transaction",
            status::COMPONENT_INVALID_TRANSACTION,
        ),
        ("thread.invalid", status::THREAD_INVALID),
        ("thread.would_block", status::THREAD_WOULD_BLOCK),
        ("thread.timeout", status::THREAD_TIMEOUT),
        ("thread.already_exited", status::THREAD_ALREADY_EXITED),
        ("thread.self", status::THREAD_SELF),
        ("filesystem.invalid_path", status::FILESYSTEM_INVALID_PATH),
        ("filesystem.not_found", status::FILESYSTEM_NOT_FOUND),
        (
            "filesystem.already_exists",
            status::FILESYSTEM_ALREADY_EXISTS,
        ),
        (
            "filesystem.not_directory",
            status::FILESYSTEM_NOT_DIRECTORY,
        ),
        ("filesystem.is_directory", status::FILESYSTEM_IS_DIRECTORY),
        ("filesystem.not_empty", status::FILESYSTEM_NOT_EMPTY),
        ("filesystem.read_only", status::FILESYSTEM_READ_ONLY),
        ("filesystem.end", status::FILESYSTEM_END),
        ("filesystem.changed", status::FILESYSTEM_CHANGED),
        ("filesystem.error", status::FILESYSTEM_ERROR),
        ("channel.full", status::CHANNEL_FULL),
        ("channel.empty", status::CHANNEL_EMPTY),
        ("channel.peer_closed", status::CHANNEL_PEER_CLOSED),
        (
            "channel.message_too_large",
            status::CHANNEL_MESSAGE_TOO_LARGE,
        ),
        (
            "channel.buffer_too_small",
            status::CHANNEL_BUFFER_TOO_SMALL,
        ),
        (
            "channel.transfer_invalid",
            status::CHANNEL_TRANSFER_INVALID,
        ),
        ("ring.full", status::RING_FULL),
        ("ring.empty", status::RING_EMPTY),
        ("ring.invalid_descriptor", status::RING_INVALID_DESCRIPTOR),
        ("ring.token_stale", status::RING_TOKEN_STALE),
        ("ring.not_found", status::RING_NOT_FOUND),
        ("ring.cancelled", status::RING_CANCELLED),
        ("ring.unsupported", status::RING_UNSUPPORTED),
        ("ring.timeout", status::RING_TIMEOUT),
        ("ring.busy", status::RING_BUSY),
        ("socket.invalid_address", status::SOCKET_INVALID_ADDRESS),
        ("socket.invalid_state", status::SOCKET_INVALID_STATE),
        ("socket.would_block", status::SOCKET_WOULD_BLOCK),
        ("socket.timeout", status::SOCKET_TIMEOUT),
        ("socket.peer_closed", status::SOCKET_PEER_CLOSED),
        ("socket.address_in_use", status::SOCKET_ADDRESS_IN_USE),
        ("socket.connection_refused", status::SOCKET_CONNECTION_REFUSED),
        ("socket.network_unreachable", status::SOCKET_NETWORK_UNREACHABLE),
        ("socket.error", status::SOCKET_ERROR),
        ("device.gone", status::DEVICE_GONE),
        ("device.busy", status::DEVICE_BUSY),
        ("device.unsupported", status::DEVICE_UNSUPPORTED),
        ("device.fault", status::DEVICE_FAULT),
        ("device.invalid_request", status::DEVICE_INVALID_REQUEST),
    ] {
        writeln!(
            output,
            "#define MYGO_STATUS_{} UINT32_C(0x{value:08x})",
            public_ident(name)
        )
        .unwrap();
    }
    writeln!(output).unwrap();
}

fn write_capability_definitions(output: &mut String, contract: &ProgramContract) {
    writeln!(
        output,
        "#define MYGO_CAPABILITY_COUNT {}u",
        contract.capabilities().len()
    )
    .unwrap();
    for capability in contract.capabilities() {
        let spec = native_abi::requirement(capability.requirement)
            .expect("manifest capability 已由 registry 归一化");
        writeln!(
            output,
            "#define MYGO_CAP_{}_required {}u",
            public_ident(spec.name),
            u32::from(capability.required)
        )
        .unwrap();
        writeln!(
            output,
            "#define MYGO_CAP_{}_rights UINT64_C({})",
            public_ident(spec.name),
            capability.rights.bits()
        )
        .unwrap();
    }
    writeln!(output, "#define MYGO_CAPABILITY_CONTRACT(X) \\").unwrap();
    for (index, capability) in contract.capabilities().iter().enumerate() {
        let spec = native_abi::requirement(capability.requirement)
            .expect("manifest capability 已由 registry 归一化");
        let interface = native_abi::interface_spec(spec.interface).name;
        let suffix = if index + 1 == contract.capabilities().len() {
            ""
        } else {
            concat!(" ", "\\")
        };
        writeln!(
            output,
            "    X(MYGO_REQUIREMENT_{}, MYGO_INTERFACE_{}, UINT64_C({}), {}u){}",
            public_ident(spec.name),
            interface,
            capability.rights.bits(),
            u32::from(capability.required),
            suffix
        )
        .unwrap();
    }
    writeln!(output).unwrap();
}

fn write_wire_types(output: &mut String) {
    writeln!(
        output,
        "typedef struct mygo_string_ref {{ uint32_t offset; uint32_t length; }} mygo_string_ref;"
    )
    .unwrap();
    writeln!(output, "struct mygo_start_info {{").unwrap();
    writeln!(output, "    uint8_t magic[4];").unwrap();
    writeln!(output, "    uint16_t version;").unwrap();
    writeln!(output, "    uint16_t header_size;").unwrap();
    writeln!(output, "    uint32_t total_size;").unwrap();
    writeln!(output, "    uint32_t flags;").unwrap();
    writeln!(output, "    uint16_t abi_epoch;").unwrap();
    writeln!(output, "    uint16_t target_arch;").unwrap();
    writeln!(output, "    uint32_t reserved0;").unwrap();
    writeln!(output, "    uint64_t enabled_features;").unwrap();
    writeln!(output, "    uint64_t image_base;").unwrap();
    writeln!(output, "    uint64_t page_size;").unwrap();
    writeln!(output, "    uint64_t initial_tls_base;").unwrap();
    writeln!(output, "    uint64_t initial_tls_size;").unwrap();
    writeln!(output, "    uint64_t initial_thread_pointer;").unwrap();
    writeln!(output, "    uint32_t argc;").unwrap();
    writeln!(output, "    uint32_t envc;").unwrap();
    writeln!(output, "    uint32_t argv_offset;").unwrap();
    writeln!(output, "    uint32_t env_offset;").unwrap();
    writeln!(output, "    uint32_t string_bytes_offset;").unwrap();
    writeln!(output, "    uint32_t string_bytes_size;").unwrap();
    writeln!(output, "    uint32_t initial_handle_count;").unwrap();
    writeln!(output, "    uint16_t initial_handle_record_size;").unwrap();
    writeln!(output, "    uint16_t reserved1;").unwrap();
    writeln!(output, "    uint32_t initial_handle_offset;").unwrap();
    writeln!(output, "    uint32_t call_slot_count;").unwrap();
    writeln!(output, "    uint8_t random_seed[32];").unwrap();
    writeln!(output, "    uint64_t runtime_flags;").unwrap();
    writeln!(output, "    uint64_t init_array_offset;").unwrap();
    writeln!(output, "    uint32_t init_array_count;").unwrap();
    writeln!(output, "    uint16_t init_array_entry_size;").unwrap();
    writeln!(output, "    uint16_t reserved2;").unwrap();
    writeln!(output, "    uint64_t fini_array_offset;").unwrap();
    writeln!(output, "    uint32_t fini_array_count;").unwrap();
    writeln!(output, "    uint16_t fini_array_entry_size;").unwrap();
    writeln!(output, "    uint16_t reserved3;").unwrap();
    writeln!(output, "    uint64_t reserved4;").unwrap();
    writeln!(output, "}};").unwrap();

    writeln!(output, "struct mygo_initial_handle {{").unwrap();
    writeln!(output, "    uint32_t requirement_id;").unwrap();
    writeln!(output, "    uint16_t object_interface;").unwrap();
    writeln!(output, "    uint16_t flags;").unwrap();
    writeln!(output, "    uint64_t handle;").unwrap();
    writeln!(output, "    uint64_t granted_rights;").unwrap();
    writeln!(output, "    uint64_t reserved;").unwrap();
    writeln!(output, "}};").unwrap();

    writeln!(output, "struct mygo_native_call {{").unwrap();
    writeln!(output, "    uint64_t slot;").unwrap();
    writeln!(output, "    uint64_t object_handle;").unwrap();
    writeln!(output, "    uint64_t args[5];").unwrap();
    writeln!(output, "    uint64_t reserved_arg;").unwrap();
    writeln!(output, "}};").unwrap();

    writeln!(output, "struct mygo_native_result {{").unwrap();
    writeln!(output, "    uint32_t status;").unwrap();
    writeln!(output, "    uint32_t reserved;").unwrap();
    writeln!(output, "    uint64_t value0;").unwrap();
    writeln!(output, "    uint64_t value1;").unwrap();
    writeln!(output, "}};").unwrap();

    writeln!(output, "typedef struct mygo_process_string_ref {{ uint64_t ptr; uint64_t len; }} mygo_process_string_ref;").unwrap();
    writeln!(output, "typedef struct mygo_process_array_ref {{ uint64_t ptr; uint32_t count; uint32_t reserved; }} mygo_process_array_ref;").unwrap();
    writeln!(output, "typedef struct mygo_handle_transfer {{ uint32_t requirement_id; uint32_t reserved; uint64_t source_handle; uint64_t requested_rights; uint64_t flags; }} mygo_handle_transfer;").unwrap();
    writeln!(output, "typedef struct mygo_spawn_request {{ uint64_t image; mygo_process_array_ref argv; mygo_process_array_ref env; mygo_process_array_ref transfers; uint64_t resource_policy; }} mygo_spawn_request;").unwrap();
    writeln!(output, "typedef struct mygo_process_result {{ uint32_t state; uint32_t flags; uint32_t exit_code; uint32_t fault_kind; uint64_t detail0; uint64_t detail1; }} mygo_process_result;").unwrap();
    writeln!(output, "typedef struct mygo_image_info {{ uint32_t artifact_kind; uint16_t target_arch; uint16_t abi_epoch; uint64_t enabled_features; uint64_t file_size; uint64_t image_virtual_size; uint8_t component_identity[16]; uint8_t abi_identity[16]; uint8_t build_id[32]; uint8_t content_hash[32]; uint64_t reserved[2]; }} mygo_image_info;").unwrap();
    writeln!(output, "typedef struct mygo_event_record {{ uint32_t event_kind; uint32_t status; uint64_t source_handle; uint64_t sequence; uint64_t value0; uint64_t value1; }} mygo_event_record;").unwrap();
    writeln!(output, "typedef struct mygo_component_load_request {{ uint64_t root_image; mygo_process_array_ref images; mygo_process_array_ref bindings; uint64_t flags; uint64_t reserved[2]; }} mygo_component_load_request;").unwrap();
    writeln!(output, "typedef struct mygo_component_lifecycle {{ uint32_t action; uint32_t state; uint64_t component; uint64_t entry; uint64_t context; uint64_t tls_identity; uint64_t generation; uint64_t call_state; uint64_t reserved; }} mygo_component_lifecycle;").unwrap();
    writeln!(output, "typedef struct mygo_component_query {{ uint32_t state; uint32_t flags; uint64_t generation; uint8_t component_identity[16]; uint8_t abi_identity[16]; uint64_t active_calls; uint32_t dependent_count; uint32_t reserved; }} mygo_component_query;").unwrap();
    writeln!(output, "typedef struct mygo_interface_request {{ uint8_t interface_identity[16]; uint8_t signature_hash[32]; }} mygo_interface_request;").unwrap();
    writeln!(output, "typedef struct mygo_component_call_state {{ uint32_t state; uint32_t flags; uint64_t generation; uint64_t active_calls; uint32_t drain_waiter; uint32_t reserved0; uint64_t reserved[4]; }} mygo_component_call_state;").unwrap();
    writeln!(output, "typedef struct mygo_component_context {{ uint64_t image_base; uint64_t call_state; uint64_t tls_base; uint64_t tls_identity; uint32_t call_slot_count; uint32_t interface_count; uint32_t capability_count; uint32_t flags; uint64_t capabilities; uint64_t reserved; }} mygo_component_context;").unwrap();
    writeln!(output, "typedef struct mygo_component_capability_record {{ uint32_t requirement_id; uint32_t reserved0; uint64_t handle; uint64_t granted_rights; uint64_t reserved1; }} mygo_component_capability_record;").unwrap();
    writeln!(output, "typedef struct mygo_component_interface_gate {{ uint64_t call_state; uint64_t target; uint64_t component; uint64_t generation; }} mygo_component_interface_gate;").unwrap();
    writeln!(output, "typedef struct mygo_thread_create_request {{ uint64_t entry; uint64_t stack_memory; uint64_t stack_offset; uint64_t stack_size; uint64_t tls_memory; uint64_t tls_offset; uint64_t argument; uint64_t flags; }} mygo_thread_create_request;").unwrap();
    writeln!(output, "typedef struct mygo_thread_result {{ uint32_t state; uint32_t flags; uint32_t exit_code; uint32_t fault_kind; uint64_t detail0; uint64_t detail1; }} mygo_thread_result;").unwrap();
    writeln!(output, "typedef struct mygo_thread_info {{ uint32_t state; uint32_t flags; uint64_t identity; uint64_t cpu_time_ns; uint32_t exit_code; uint32_t fault_kind; uint64_t tls_base; uint64_t reserved; }} mygo_thread_info;").unwrap();
    writeln!(output, "typedef struct mygo_memory_create_request {{ uint64_t size; uint64_t alignment; uint32_t flags; uint32_t kind; uint64_t source_handle; uint64_t source_offset; uint64_t reserved[3]; }} mygo_memory_create_request;").unwrap();
    writeln!(output, "typedef struct mygo_memory_map_request {{ uint64_t address_space; uint64_t offset; uint64_t length; uint64_t alignment; uint64_t address_hint; uint32_t permissions; uint32_t flags; uint64_t reserved[2]; }} mygo_memory_map_request;").unwrap();
    writeln!(output, "typedef struct mygo_memory_info {{ uint64_t size; uint64_t alignment; uint32_t kind; uint32_t flags; uint32_t mapping_count; uint32_t state; uint64_t generation; uint64_t source_size; uint64_t reserved[2]; }} mygo_memory_info;").unwrap();
    writeln!(output, "typedef struct mygo_memory_statistics {{ uint64_t materialized_pages; uint64_t resident_mappings; uint64_t mapped_pages; uint64_t shared_resident_mappings; uint64_t read_operations; uint64_t write_operations; uint64_t bytes_read; uint64_t bytes_written; uint64_t writeback_operations; uint64_t reserved; }} mygo_memory_statistics;").unwrap();
    writeln!(output, "typedef struct mygo_memory_region {{ uint64_t memory; uint64_t offset; uint64_t length; uint64_t generation; }} mygo_memory_region;").unwrap();
    writeln!(output, "typedef struct mygo_path_ref {{ uint64_t ptr; uint32_t length; uint32_t flags; }} mygo_path_ref;").unwrap();
    writeln!(output, "typedef struct mygo_directory_request {{ mygo_path_ref path; uint32_t kind; uint32_t flags; uint64_t requested_rights; uint64_t reserved[4]; }} mygo_directory_request;").unwrap();
    writeln!(output, "typedef struct mygo_directory_info {{ uint32_t flags; uint32_t reserved0; uint64_t generation; uint64_t entry_count; uint64_t change_counter; uint64_t reserved[4]; }} mygo_directory_info;").unwrap();
    writeln!(output, "typedef struct mygo_file_info {{ uint32_t kind; uint32_t flags; uint64_t size; uint64_t generation; uint64_t modified_ns; uint64_t granted_rights; uint64_t reserved[3]; }} mygo_file_info;").unwrap();
    writeln!(output, "typedef struct mygo_channel_handle_transfer {{ uint64_t source_handle; uint64_t requested_rights; uint64_t flags; uint64_t reserved; }} mygo_channel_handle_transfer;").unwrap();
    writeln!(output, "typedef struct mygo_channel_message {{ uint64_t data_ptr; uint32_t data_size; uint32_t data_capacity; uint64_t handles_ptr; uint32_t handle_count; uint32_t handle_capacity; uint64_t flags; uint64_t reserved[3]; }} mygo_channel_message;").unwrap();
    writeln!(output, "typedef struct mygo_submission_descriptor {{ uint64_t slot; uint64_t handle; uint64_t arg0; uint64_t arg1; uint64_t arg2; uint64_t arg3; uint64_t arg4; uint64_t user_data; }} mygo_submission_descriptor;").unwrap();
    writeln!(output, "typedef struct mygo_completion_record {{ uint64_t user_data; uint32_t status; uint32_t reserved; uint64_t value0; uint64_t value1; }} mygo_completion_record;").unwrap();
    writeln!(output, "typedef struct mygo_ring_shared_state {{ uint32_t magic; uint16_t version; uint16_t flags; uint32_t entries; uint32_t mask; uint32_t sq_head; uint32_t sq_tail; uint32_t cq_head; uint32_t cq_tail; uint64_t sq_offset; uint64_t cq_offset; uint64_t generation; uint64_t reserved; }} mygo_ring_shared_state;").unwrap();
    writeln!(output, "typedef struct mygo_ring_info {{ uint32_t capacity; uint32_t reserved0; uint32_t queued; uint32_t registered; uint64_t generation; uint64_t completed; uint64_t cancelled; uint64_t reserved[3]; }} mygo_ring_info;").unwrap();
    writeln!(output, "typedef struct mygo_socket_create_request {{ uint16_t family; uint16_t kind; uint16_t protocol; uint16_t flags; uint64_t reserved[3]; }} mygo_socket_create_request;").unwrap();
    writeln!(output, "typedef struct mygo_network_address {{ uint16_t family; uint16_t flags; uint16_t port; uint16_t reserved0; uint8_t address[16]; uint32_t scope_id; uint32_t reserved1; }} mygo_network_address;").unwrap();
    writeln!(output, "typedef struct mygo_socket_info {{ uint16_t family; uint16_t kind; uint16_t protocol; uint16_t state; uint32_t flags; uint32_t reserved0; mygo_network_address local; mygo_network_address peer; uint64_t generation; uint64_t reserved[2]; }} mygo_socket_info;").unwrap();
    writeln!(output, "typedef struct mygo_device_request {{ uint32_t opcode; uint32_t flags; mygo_memory_region input; mygo_memory_region output; uint64_t deadline_ns; uint64_t reserved[2]; }} mygo_device_request;").unwrap();
    writeln!(output, "typedef struct mygo_device_info {{ uint64_t class_id; uint64_t generation; uint32_t state; uint32_t flags; uint8_t contract_hash[32]; uint8_t name_hash[32]; uint64_t reserved; }} mygo_device_info;").unwrap();

    for (name, type_name, size) in [
        (
            "MYGO_STRING_REF_SIZE",
            "struct mygo_string_ref",
            native_wire::STRING_REF_SIZE,
        ),
        (
            "MYGO_START_INFO_SIZE",
            "struct mygo_start_info",
            native_wire::START_INFO_SIZE,
        ),
        (
            "MYGO_INITIAL_HANDLE_SIZE",
            "struct mygo_initial_handle",
            native_wire::INITIAL_HANDLE_SIZE,
        ),
        ("MYGO_NATIVE_CALL_SIZE", "struct mygo_native_call", 64),
        ("MYGO_NATIVE_RESULT_SIZE", "struct mygo_native_result", 24),
        ("MYGO_PROCESS_STRING_REF_SIZE", "mygo_process_string_ref", native_wire::PROCESS_STRING_REF_SIZE),
        ("MYGO_PROCESS_ARRAY_REF_SIZE", "mygo_process_array_ref", native_wire::PROCESS_ARRAY_REF_SIZE),
        ("MYGO_HANDLE_TRANSFER_SIZE", "mygo_handle_transfer", native_wire::HANDLE_TRANSFER_SIZE),
        ("MYGO_SPAWN_REQUEST_SIZE", "mygo_spawn_request", native_wire::SPAWN_REQUEST_SIZE),
        ("MYGO_PROCESS_RESULT_SIZE", "mygo_process_result", native_wire::PROCESS_RESULT_SIZE),
        ("MYGO_IMAGE_INFO_SIZE", "mygo_image_info", native_wire::IMAGE_INFO_SIZE),
        ("MYGO_EVENT_RECORD_SIZE", "mygo_event_record", native_wire::EVENT_RECORD_SIZE),
        ("MYGO_COMPONENT_LOAD_REQUEST_SIZE", "mygo_component_load_request", native_wire::COMPONENT_LOAD_REQUEST_SIZE),
        ("MYGO_COMPONENT_LIFECYCLE_SIZE", "mygo_component_lifecycle", native_wire::COMPONENT_LIFECYCLE_SIZE),
        ("MYGO_COMPONENT_QUERY_SIZE", "mygo_component_query", native_wire::COMPONENT_QUERY_SIZE),
        ("MYGO_INTERFACE_REQUEST_SIZE", "mygo_interface_request", native_wire::INTERFACE_REQUEST_SIZE),
        ("MYGO_COMPONENT_CALL_STATE_SIZE", "mygo_component_call_state", native_wire::COMPONENT_CALL_STATE_SIZE),
        ("MYGO_COMPONENT_CONTEXT_SIZE", "mygo_component_context", native_wire::COMPONENT_CONTEXT_SIZE),
        ("MYGO_COMPONENT_CAPABILITY_RECORD_SIZE", "mygo_component_capability_record", native_wire::COMPONENT_CAPABILITY_RECORD_SIZE),
        ("MYGO_COMPONENT_INTERFACE_GATE_SIZE", "mygo_component_interface_gate", native_wire::COMPONENT_INTERFACE_GATE_SIZE),
        ("MYGO_THREAD_CREATE_REQUEST_SIZE", "mygo_thread_create_request", native_wire::THREAD_CREATE_REQUEST_SIZE),
        ("MYGO_THREAD_RESULT_SIZE", "mygo_thread_result", native_wire::THREAD_RESULT_SIZE),
        ("MYGO_THREAD_INFO_SIZE", "mygo_thread_info", native_wire::THREAD_INFO_SIZE),
        ("MYGO_MEMORY_CREATE_REQUEST_SIZE", "mygo_memory_create_request", native_wire::MEMORY_CREATE_REQUEST_SIZE),
        ("MYGO_MEMORY_MAP_REQUEST_SIZE", "mygo_memory_map_request", native_wire::MEMORY_MAP_REQUEST_SIZE),
        ("MYGO_MEMORY_INFO_SIZE", "mygo_memory_info", native_wire::MEMORY_INFO_SIZE),
        ("MYGO_MEMORY_STATISTICS_SIZE", "mygo_memory_statistics", native_wire::MEMORY_STATISTICS_SIZE),
        ("MYGO_MEMORY_REGION_SIZE", "mygo_memory_region", native_wire::MEMORY_REGION_SIZE),
        ("MYGO_PATH_REF_SIZE", "mygo_path_ref", native_wire::PATH_REF_SIZE),
        ("MYGO_DIRECTORY_REQUEST_SIZE", "mygo_directory_request", native_wire::DIRECTORY_REQUEST_SIZE),
        ("MYGO_DIRECTORY_INFO_SIZE", "mygo_directory_info", native_wire::DIRECTORY_INFO_SIZE),
        ("MYGO_FILE_INFO_SIZE", "mygo_file_info", native_wire::FILE_INFO_SIZE),
        ("MYGO_CHANNEL_HANDLE_TRANSFER_SIZE", "mygo_channel_handle_transfer", native_wire::CHANNEL_HANDLE_TRANSFER_SIZE),
        ("MYGO_CHANNEL_MESSAGE_SIZE", "mygo_channel_message", native_wire::CHANNEL_MESSAGE_SIZE),
        ("MYGO_SUBMISSION_DESCRIPTOR_SIZE", "mygo_submission_descriptor", native_wire::SUBMISSION_DESCRIPTOR_SIZE),
        ("MYGO_COMPLETION_RECORD_SIZE", "mygo_completion_record", native_wire::COMPLETION_RECORD_SIZE),
        ("MYGO_RING_SHARED_STATE_SIZE", "mygo_ring_shared_state", native_wire::RING_SHARED_STATE_SIZE),
        ("MYGO_RING_INFO_SIZE", "mygo_ring_info", native_wire::RING_INFO_SIZE),
        ("MYGO_SOCKET_CREATE_REQUEST_SIZE", "mygo_socket_create_request", native_wire::SOCKET_CREATE_REQUEST_SIZE),
        ("MYGO_NETWORK_ADDRESS_SIZE", "mygo_network_address", native_wire::NETWORK_ADDRESS_SIZE),
        ("MYGO_SOCKET_INFO_SIZE", "mygo_socket_info", native_wire::SOCKET_INFO_SIZE),
        ("MYGO_DEVICE_REQUEST_SIZE", "mygo_device_request", native_wire::DEVICE_REQUEST_SIZE),
        ("MYGO_DEVICE_INFO_SIZE", "mygo_device_info", native_wire::DEVICE_INFO_SIZE),
    ] {
        writeln!(
            output,
            "#define {name} {size}u\n_Static_assert(sizeof({type_name}) == {size}, \"{name}\");"
        )
        .unwrap();
    }
    write_wire_offsets(
        output,
        "STRING_REF",
        "struct mygo_string_ref",
        &[
            ("OFFSET", "offset", native_wire::string_ref::OFFSET),
            ("LENGTH", "length", native_wire::string_ref::LENGTH),
        ],
    );
    write_wire_offsets(
        output,
        "START_INFO",
        "struct mygo_start_info",
        &[
            ("MAGIC", "magic", native_wire::start_info::MAGIC),
            ("VERSION", "version", native_wire::start_info::VERSION),
            (
                "HEADER_SIZE",
                "header_size",
                native_wire::start_info::HEADER_SIZE,
            ),
            (
                "TOTAL_SIZE",
                "total_size",
                native_wire::start_info::TOTAL_SIZE,
            ),
            ("FLAGS", "flags", native_wire::start_info::FLAGS),
            ("ABI_EPOCH", "abi_epoch", native_wire::start_info::ABI_EPOCH),
            (
                "TARGET_ARCH",
                "target_arch",
                native_wire::start_info::TARGET_ARCH,
            ),
            ("RESERVED0", "reserved0", native_wire::start_info::RESERVED0),
            (
                "ENABLED_FEATURES",
                "enabled_features",
                native_wire::start_info::ENABLED_FEATURES,
            ),
            (
                "IMAGE_BASE",
                "image_base",
                native_wire::start_info::IMAGE_BASE,
            ),
            ("PAGE_SIZE", "page_size", native_wire::start_info::PAGE_SIZE),
            (
                "INITIAL_TLS_BASE",
                "initial_tls_base",
                native_wire::start_info::INITIAL_TLS_BASE,
            ),
            (
                "INITIAL_TLS_SIZE",
                "initial_tls_size",
                native_wire::start_info::INITIAL_TLS_SIZE,
            ),
            (
                "INITIAL_THREAD_POINTER",
                "initial_thread_pointer",
                native_wire::start_info::INITIAL_THREAD_POINTER,
            ),
            ("ARGC", "argc", native_wire::start_info::ARGC),
            ("ENVC", "envc", native_wire::start_info::ENVC),
            (
                "ARGV_OFFSET",
                "argv_offset",
                native_wire::start_info::ARGV_OFFSET,
            ),
            (
                "ENV_OFFSET",
                "env_offset",
                native_wire::start_info::ENV_OFFSET,
            ),
            (
                "STRING_BYTES_OFFSET",
                "string_bytes_offset",
                native_wire::start_info::STRING_BYTES_OFFSET,
            ),
            (
                "STRING_BYTES_SIZE",
                "string_bytes_size",
                native_wire::start_info::STRING_BYTES_SIZE,
            ),
            (
                "INITIAL_HANDLE_COUNT",
                "initial_handle_count",
                native_wire::start_info::INITIAL_HANDLE_COUNT,
            ),
            (
                "INITIAL_HANDLE_RECORD_SIZE",
                "initial_handle_record_size",
                native_wire::start_info::INITIAL_HANDLE_RECORD_SIZE,
            ),
            ("RESERVED1", "reserved1", native_wire::start_info::RESERVED1),
            (
                "INITIAL_HANDLE_OFFSET",
                "initial_handle_offset",
                native_wire::start_info::INITIAL_HANDLE_OFFSET,
            ),
            (
                "CALL_SLOT_COUNT",
                "call_slot_count",
                native_wire::start_info::CALL_SLOT_COUNT,
            ),
            (
                "RANDOM_SEED",
                "random_seed",
                native_wire::start_info::RANDOM_SEED,
            ),
            (
                "RUNTIME_FLAGS",
                "runtime_flags",
                native_wire::start_info::RUNTIME_FLAGS,
            ),
            (
                "INIT_ARRAY_OFFSET",
                "init_array_offset",
                native_wire::start_info::INIT_ARRAY_OFFSET,
            ),
            (
                "INIT_ARRAY_COUNT",
                "init_array_count",
                native_wire::start_info::INIT_ARRAY_COUNT,
            ),
            (
                "INIT_ARRAY_ENTRY_SIZE",
                "init_array_entry_size",
                native_wire::start_info::INIT_ARRAY_ENTRY_SIZE,
            ),
            ("RESERVED2", "reserved2", native_wire::start_info::RESERVED2),
            (
                "FINI_ARRAY_OFFSET",
                "fini_array_offset",
                native_wire::start_info::FINI_ARRAY_OFFSET,
            ),
            (
                "FINI_ARRAY_COUNT",
                "fini_array_count",
                native_wire::start_info::FINI_ARRAY_COUNT,
            ),
            (
                "FINI_ARRAY_ENTRY_SIZE",
                "fini_array_entry_size",
                native_wire::start_info::FINI_ARRAY_ENTRY_SIZE,
            ),
            ("RESERVED3", "reserved3", native_wire::start_info::RESERVED3),
            ("RESERVED4", "reserved4", native_wire::start_info::RESERVED4),
        ],
    );
    write_wire_offsets(
        output,
        "PROCESS_STRING_REF",
        "mygo_process_string_ref",
        &[
            ("PTR", "ptr", native_wire::process_string_ref::PTR),
            ("LEN", "len", native_wire::process_string_ref::LEN),
        ],
    );
    write_wire_offsets(
        output,
        "PROCESS_ARRAY_REF",
        "mygo_process_array_ref",
        &[
            ("PTR", "ptr", native_wire::process_array_ref::PTR),
            ("COUNT", "count", native_wire::process_array_ref::COUNT),
            ("RESERVED", "reserved", native_wire::process_array_ref::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "HANDLE_TRANSFER",
        "mygo_handle_transfer",
        &[
            ("REQUIREMENT_ID", "requirement_id", native_wire::handle_transfer::REQUIREMENT_ID),
            ("RESERVED", "reserved", native_wire::handle_transfer::RESERVED),
            ("SOURCE_HANDLE", "source_handle", native_wire::handle_transfer::SOURCE_HANDLE),
            ("REQUESTED_RIGHTS", "requested_rights", native_wire::handle_transfer::REQUESTED_RIGHTS),
            ("FLAGS", "flags", native_wire::handle_transfer::FLAGS),
        ],
    );
    write_wire_offsets(
        output,
        "SPAWN_REQUEST",
        "mygo_spawn_request",
        &[
            ("IMAGE", "image", native_wire::spawn_request::IMAGE),
            ("ARGV", "argv", native_wire::spawn_request::ARGV),
            ("ENV", "env", native_wire::spawn_request::ENV),
            ("TRANSFERS", "transfers", native_wire::spawn_request::TRANSFERS),
            ("RESOURCE_POLICY", "resource_policy", native_wire::spawn_request::RESOURCE_POLICY),
        ],
    );
    write_wire_offsets(
        output,
        "PROCESS_RESULT",
        "mygo_process_result",
        &[
            ("STATE", "state", native_wire::process_result::STATE),
            ("FLAGS", "flags", native_wire::process_result::FLAGS),
            ("EXIT_CODE", "exit_code", native_wire::process_result::EXIT_CODE),
            ("FAULT_KIND", "fault_kind", native_wire::process_result::FAULT_KIND),
            ("DETAIL0", "detail0", native_wire::process_result::DETAIL0),
            ("DETAIL1", "detail1", native_wire::process_result::DETAIL1),
        ],
    );
    write_wire_offsets(
        output,
        "IMAGE_INFO",
        "mygo_image_info",
        &[
            ("ARTIFACT_KIND", "artifact_kind", native_wire::image_info::ARTIFACT_KIND),
            ("TARGET_ARCH", "target_arch", native_wire::image_info::TARGET_ARCH),
            ("ABI_EPOCH", "abi_epoch", native_wire::image_info::ABI_EPOCH),
            ("ENABLED_FEATURES", "enabled_features", native_wire::image_info::ENABLED_FEATURES),
            ("FILE_SIZE", "file_size", native_wire::image_info::FILE_SIZE),
            ("IMAGE_VIRTUAL_SIZE", "image_virtual_size", native_wire::image_info::IMAGE_VIRTUAL_SIZE),
            ("COMPONENT_IDENTITY", "component_identity", native_wire::image_info::COMPONENT_IDENTITY),
            ("ABI_IDENTITY", "abi_identity", native_wire::image_info::ABI_IDENTITY),
            ("BUILD_ID", "build_id", native_wire::image_info::BUILD_ID),
            ("CONTENT_HASH", "content_hash", native_wire::image_info::CONTENT_HASH),
            ("RESERVED", "reserved", native_wire::image_info::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "EVENT_RECORD",
        "mygo_event_record",
        &[
            ("EVENT_KIND", "event_kind", native_wire::event_record::EVENT_KIND),
            ("STATUS", "status", native_wire::event_record::STATUS),
            ("SOURCE_HANDLE", "source_handle", native_wire::event_record::SOURCE_HANDLE),
            ("SEQUENCE", "sequence", native_wire::event_record::SEQUENCE),
            ("VALUE0", "value0", native_wire::event_record::VALUE0),
            ("VALUE1", "value1", native_wire::event_record::VALUE1),
        ],
    );
    write_wire_offsets(
        output,
        "COMPONENT_LOAD_REQUEST",
        "mygo_component_load_request",
        &[
            ("ROOT_IMAGE", "root_image", native_wire::component_load_request::ROOT_IMAGE),
            ("IMAGES", "images", native_wire::component_load_request::IMAGES),
            ("BINDINGS", "bindings", native_wire::component_load_request::BINDINGS),
            ("FLAGS", "flags", native_wire::component_load_request::FLAGS),
            ("RESERVED", "reserved", native_wire::component_load_request::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "COMPONENT_LIFECYCLE",
        "mygo_component_lifecycle",
        &[
            ("ACTION", "action", native_wire::component_lifecycle::ACTION),
            ("STATE", "state", native_wire::component_lifecycle::STATE),
            ("COMPONENT", "component", native_wire::component_lifecycle::COMPONENT),
            ("ENTRY", "entry", native_wire::component_lifecycle::ENTRY),
            ("CONTEXT", "context", native_wire::component_lifecycle::CONTEXT),
            ("TLS_IDENTITY", "tls_identity", native_wire::component_lifecycle::TLS_IDENTITY),
            ("GENERATION", "generation", native_wire::component_lifecycle::GENERATION),
            ("CALL_STATE", "call_state", native_wire::component_lifecycle::CALL_STATE),
            ("RESERVED", "reserved", native_wire::component_lifecycle::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "COMPONENT_QUERY",
        "mygo_component_query",
        &[
            ("STATE", "state", native_wire::component_query::STATE),
            ("FLAGS", "flags", native_wire::component_query::FLAGS),
            ("GENERATION", "generation", native_wire::component_query::GENERATION),
            ("COMPONENT_IDENTITY", "component_identity", native_wire::component_query::COMPONENT_IDENTITY),
            ("ABI_IDENTITY", "abi_identity", native_wire::component_query::ABI_IDENTITY),
            ("ACTIVE_CALLS", "active_calls", native_wire::component_query::ACTIVE_CALLS),
            ("DEPENDENT_COUNT", "dependent_count", native_wire::component_query::DEPENDENT_COUNT),
            ("RESERVED", "reserved", native_wire::component_query::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "INTERFACE_REQUEST",
        "mygo_interface_request",
        &[
            ("INTERFACE_IDENTITY", "interface_identity", native_wire::interface_request::INTERFACE_IDENTITY),
            ("SIGNATURE_HASH", "signature_hash", native_wire::interface_request::SIGNATURE_HASH),
        ],
    );
    write_wire_offsets(
        output,
        "COMPONENT_CALL_STATE",
        "mygo_component_call_state",
        &[
            ("STATE", "state", native_wire::component_call_state::STATE),
            ("FLAGS", "flags", native_wire::component_call_state::FLAGS),
            ("GENERATION", "generation", native_wire::component_call_state::GENERATION),
            ("ACTIVE_CALLS", "active_calls", native_wire::component_call_state::ACTIVE_CALLS),
            ("DRAIN_WAITER", "drain_waiter", native_wire::component_call_state::DRAIN_WAITER),
            ("RESERVED0", "reserved0", native_wire::component_call_state::RESERVED0),
            ("RESERVED", "reserved", native_wire::component_call_state::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "COMPONENT_CONTEXT",
        "mygo_component_context",
        &[
            ("IMAGE_BASE", "image_base", native_wire::component_context::IMAGE_BASE),
            ("CALL_STATE", "call_state", native_wire::component_context::CALL_STATE),
            ("TLS_BASE", "tls_base", native_wire::component_context::TLS_BASE),
            ("TLS_IDENTITY", "tls_identity", native_wire::component_context::TLS_IDENTITY),
            ("CALL_SLOT_COUNT", "call_slot_count", native_wire::component_context::CALL_SLOT_COUNT),
            ("INTERFACE_COUNT", "interface_count", native_wire::component_context::INTERFACE_COUNT),
            ("CAPABILITY_COUNT", "capability_count", native_wire::component_context::CAPABILITY_COUNT),
            ("FLAGS", "flags", native_wire::component_context::FLAGS),
            ("CAPABILITIES", "capabilities", native_wire::component_context::CAPABILITIES),
            ("RESERVED", "reserved", native_wire::component_context::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "COMPONENT_CAPABILITY_RECORD",
        "mygo_component_capability_record",
        &[
            ("REQUIREMENT_ID", "requirement_id", native_wire::component_capability_record::REQUIREMENT_ID),
            ("RESERVED0", "reserved0", native_wire::component_capability_record::RESERVED0),
            ("HANDLE", "handle", native_wire::component_capability_record::HANDLE),
            ("GRANTED_RIGHTS", "granted_rights", native_wire::component_capability_record::GRANTED_RIGHTS),
            ("RESERVED1", "reserved1", native_wire::component_capability_record::RESERVED1),
        ],
    );
    write_wire_offsets(
        output,
        "COMPONENT_INTERFACE_GATE",
        "mygo_component_interface_gate",
        &[
            ("CALL_STATE", "call_state", native_wire::component_interface_gate::CALL_STATE),
            ("TARGET", "target", native_wire::component_interface_gate::TARGET),
            ("COMPONENT", "component", native_wire::component_interface_gate::COMPONENT),
            ("GENERATION", "generation", native_wire::component_interface_gate::GENERATION),
        ],
    );
    write_wire_offsets(
        output,
        "THREAD_CREATE_REQUEST",
        "mygo_thread_create_request",
        &[
            ("ENTRY", "entry", native_wire::thread_create_request::ENTRY),
            ("STACK_MEMORY", "stack_memory", native_wire::thread_create_request::STACK_MEMORY),
            ("STACK_OFFSET", "stack_offset", native_wire::thread_create_request::STACK_OFFSET),
            ("STACK_SIZE", "stack_size", native_wire::thread_create_request::STACK_SIZE),
            ("TLS_MEMORY", "tls_memory", native_wire::thread_create_request::TLS_MEMORY),
            ("TLS_OFFSET", "tls_offset", native_wire::thread_create_request::TLS_OFFSET),
            ("ARGUMENT", "argument", native_wire::thread_create_request::ARGUMENT),
            ("FLAGS", "flags", native_wire::thread_create_request::FLAGS),
        ],
    );
    write_wire_offsets(
        output,
        "THREAD_RESULT",
        "mygo_thread_result",
        &[
            ("STATE", "state", native_wire::thread_result::STATE),
            ("FLAGS", "flags", native_wire::thread_result::FLAGS),
            ("EXIT_CODE", "exit_code", native_wire::thread_result::EXIT_CODE),
            ("FAULT_KIND", "fault_kind", native_wire::thread_result::FAULT_KIND),
            ("DETAIL0", "detail0", native_wire::thread_result::DETAIL0),
            ("DETAIL1", "detail1", native_wire::thread_result::DETAIL1),
        ],
    );
    write_wire_offsets(
        output,
        "THREAD_INFO",
        "mygo_thread_info",
        &[
            ("STATE", "state", native_wire::thread_info::STATE),
            ("FLAGS", "flags", native_wire::thread_info::FLAGS),
            ("IDENTITY", "identity", native_wire::thread_info::IDENTITY),
            ("CPU_TIME_NS", "cpu_time_ns", native_wire::thread_info::CPU_TIME_NS),
            ("EXIT_CODE", "exit_code", native_wire::thread_info::EXIT_CODE),
            ("FAULT_KIND", "fault_kind", native_wire::thread_info::FAULT_KIND),
            ("TLS_BASE", "tls_base", native_wire::thread_info::TLS_BASE),
            ("RESERVED", "reserved", native_wire::thread_info::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "MEMORY_CREATE_REQUEST",
        "mygo_memory_create_request",
        &[
            ("SIZE", "size", native_wire::memory_create_request::SIZE),
            ("ALIGNMENT", "alignment", native_wire::memory_create_request::ALIGNMENT),
            ("FLAGS", "flags", native_wire::memory_create_request::FLAGS),
            ("KIND", "kind", native_wire::memory_create_request::KIND),
            ("SOURCE_HANDLE", "source_handle", native_wire::memory_create_request::SOURCE_HANDLE),
            ("SOURCE_OFFSET", "source_offset", native_wire::memory_create_request::SOURCE_OFFSET),
            ("RESERVED", "reserved", native_wire::memory_create_request::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "MEMORY_MAP_REQUEST",
        "mygo_memory_map_request",
        &[
            ("ADDRESS_SPACE", "address_space", native_wire::memory_map_request::ADDRESS_SPACE),
            ("OFFSET", "offset", native_wire::memory_map_request::OFFSET),
            ("LENGTH", "length", native_wire::memory_map_request::LENGTH),
            ("ALIGNMENT", "alignment", native_wire::memory_map_request::ALIGNMENT),
            ("ADDRESS_HINT", "address_hint", native_wire::memory_map_request::ADDRESS_HINT),
            ("PERMISSIONS", "permissions", native_wire::memory_map_request::PERMISSIONS),
            ("FLAGS", "flags", native_wire::memory_map_request::FLAGS),
            ("RESERVED", "reserved", native_wire::memory_map_request::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "MEMORY_INFO",
        "mygo_memory_info",
        &[
            ("SIZE", "size", native_wire::memory_info::SIZE),
            ("ALIGNMENT", "alignment", native_wire::memory_info::ALIGNMENT),
            ("KIND", "kind", native_wire::memory_info::KIND),
            ("FLAGS", "flags", native_wire::memory_info::FLAGS),
            ("MAPPING_COUNT", "mapping_count", native_wire::memory_info::MAPPING_COUNT),
            ("STATE", "state", native_wire::memory_info::STATE),
            ("GENERATION", "generation", native_wire::memory_info::GENERATION),
            ("SOURCE_SIZE", "source_size", native_wire::memory_info::SOURCE_SIZE),
            ("RESERVED", "reserved", native_wire::memory_info::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "MEMORY_REGION",
        "mygo_memory_region",
        &[
            ("MEMORY", "memory", native_wire::memory_region::MEMORY),
            ("OFFSET", "offset", native_wire::memory_region::OFFSET),
            ("LENGTH", "length", native_wire::memory_region::LENGTH),
            ("GENERATION", "generation", native_wire::memory_region::GENERATION),
        ],
    );
    write_wire_offsets(
        output,
        "MEMORY_STATISTICS",
        "mygo_memory_statistics",
        &[
            ("MATERIALIZED_PAGES", "materialized_pages", native_wire::memory_statistics::MATERIALIZED_PAGES),
            ("RESIDENT_MAPPINGS", "resident_mappings", native_wire::memory_statistics::RESIDENT_MAPPINGS),
            ("MAPPED_PAGES", "mapped_pages", native_wire::memory_statistics::MAPPED_PAGES),
            ("SHARED_RESIDENT_MAPPINGS", "shared_resident_mappings", native_wire::memory_statistics::SHARED_RESIDENT_MAPPINGS),
            ("READ_OPERATIONS", "read_operations", native_wire::memory_statistics::READ_OPERATIONS),
            ("WRITE_OPERATIONS", "write_operations", native_wire::memory_statistics::WRITE_OPERATIONS),
            ("BYTES_READ", "bytes_read", native_wire::memory_statistics::BYTES_READ),
            ("BYTES_WRITTEN", "bytes_written", native_wire::memory_statistics::BYTES_WRITTEN),
            ("WRITEBACK_OPERATIONS", "writeback_operations", native_wire::memory_statistics::WRITEBACK_OPERATIONS),
            ("RESERVED", "reserved", native_wire::memory_statistics::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "PATH_REF",
        "mygo_path_ref",
        &[
            ("PTR", "ptr", native_wire::path_ref::PTR),
            ("LENGTH", "length", native_wire::path_ref::LENGTH),
            ("FLAGS", "flags", native_wire::path_ref::FLAGS),
        ],
    );
    write_wire_offsets(
        output,
        "DIRECTORY_REQUEST",
        "mygo_directory_request",
        &[
            ("PATH", "path", native_wire::directory_request::PATH),
            ("KIND", "kind", native_wire::directory_request::KIND),
            ("FLAGS", "flags", native_wire::directory_request::FLAGS),
            ("REQUESTED_RIGHTS", "requested_rights", native_wire::directory_request::REQUESTED_RIGHTS),
            ("RESERVED", "reserved", native_wire::directory_request::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "DIRECTORY_INFO",
        "mygo_directory_info",
        &[
            ("FLAGS", "flags", native_wire::directory_info::FLAGS),
            ("RESERVED0", "reserved0", native_wire::directory_info::RESERVED0),
            ("GENERATION", "generation", native_wire::directory_info::GENERATION),
            ("ENTRY_COUNT", "entry_count", native_wire::directory_info::ENTRY_COUNT),
            ("CHANGE_COUNTER", "change_counter", native_wire::directory_info::CHANGE_COUNTER),
            ("RESERVED", "reserved", native_wire::directory_info::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "FILE_INFO",
        "mygo_file_info",
        &[
            ("KIND", "kind", native_wire::file_info::KIND),
            ("FLAGS", "flags", native_wire::file_info::FLAGS),
            ("SIZE", "size", native_wire::file_info::SIZE),
            ("GENERATION", "generation", native_wire::file_info::GENERATION),
            ("MODIFIED_NS", "modified_ns", native_wire::file_info::MODIFIED_NS),
            ("GRANTED_RIGHTS", "granted_rights", native_wire::file_info::GRANTED_RIGHTS),
            ("RESERVED", "reserved", native_wire::file_info::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "CHANNEL_HANDLE_TRANSFER",
        "mygo_channel_handle_transfer",
        &[
            ("SOURCE_HANDLE", "source_handle", native_wire::channel_handle_transfer::SOURCE_HANDLE),
            ("REQUESTED_RIGHTS", "requested_rights", native_wire::channel_handle_transfer::REQUESTED_RIGHTS),
            ("FLAGS", "flags", native_wire::channel_handle_transfer::FLAGS),
            ("RESERVED", "reserved", native_wire::channel_handle_transfer::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "CHANNEL_MESSAGE",
        "mygo_channel_message",
        &[
            ("DATA_PTR", "data_ptr", native_wire::channel_message::DATA_PTR),
            ("DATA_SIZE", "data_size", native_wire::channel_message::DATA_SIZE),
            ("DATA_CAPACITY", "data_capacity", native_wire::channel_message::DATA_CAPACITY),
            ("HANDLES_PTR", "handles_ptr", native_wire::channel_message::HANDLES_PTR),
            ("HANDLE_COUNT", "handle_count", native_wire::channel_message::HANDLE_COUNT),
            ("HANDLE_CAPACITY", "handle_capacity", native_wire::channel_message::HANDLE_CAPACITY),
            ("FLAGS", "flags", native_wire::channel_message::FLAGS),
            ("RESERVED", "reserved", native_wire::channel_message::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "RING_SHARED_STATE",
        "mygo_ring_shared_state",
        &[
            ("MAGIC", "magic", native_wire::ring_shared_state::MAGIC),
            ("VERSION", "version", native_wire::ring_shared_state::VERSION),
            ("FLAGS", "flags", native_wire::ring_shared_state::FLAGS),
            ("ENTRIES", "entries", native_wire::ring_shared_state::ENTRIES),
            ("MASK", "mask", native_wire::ring_shared_state::MASK),
            ("SQ_HEAD", "sq_head", native_wire::ring_shared_state::SQ_HEAD),
            ("SQ_TAIL", "sq_tail", native_wire::ring_shared_state::SQ_TAIL),
            ("CQ_HEAD", "cq_head", native_wire::ring_shared_state::CQ_HEAD),
            ("CQ_TAIL", "cq_tail", native_wire::ring_shared_state::CQ_TAIL),
            ("SQ_OFFSET", "sq_offset", native_wire::ring_shared_state::SQ_OFFSET),
            ("CQ_OFFSET", "cq_offset", native_wire::ring_shared_state::CQ_OFFSET),
            ("GENERATION", "generation", native_wire::ring_shared_state::GENERATION),
            ("RESERVED", "reserved", native_wire::ring_shared_state::RESERVED),
        ],
    );
    write_wire_offsets(
        output,
        "INITIAL_HANDLE",
        "struct mygo_initial_handle",
        &[
            (
                "REQUIREMENT_ID",
                "requirement_id",
                native_wire::initial_handle::REQUIREMENT_ID,
            ),
            (
                "OBJECT_INTERFACE",
                "object_interface",
                native_wire::initial_handle::OBJECT_INTERFACE,
            ),
            ("FLAGS", "flags", native_wire::initial_handle::FLAGS),
            ("HANDLE", "handle", native_wire::initial_handle::HANDLE),
            (
                "GRANTED_RIGHTS",
                "granted_rights",
                native_wire::initial_handle::GRANTED_RIGHTS,
            ),
            (
                "RESERVED",
                "reserved",
                native_wire::initial_handle::RESERVED,
            ),
        ],
    );
    writeln!(output).unwrap();
}

fn write_wire_offsets(
    output: &mut String,
    prefix: &str,
    type_name: &str,
    fields: &[(&str, &str, usize)],
) {
    for (name, field, offset) in fields {
        writeln!(
            output,
            "#define MYGO_{prefix}_{name}_OFFSET {offset}u\n_Static_assert(offsetof({type_name}, {field}) == MYGO_{prefix}_{name}_OFFSET, \"{type_name}.{field}\");"
        )
        .unwrap();
    }
}

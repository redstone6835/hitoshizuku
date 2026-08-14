//! 程序 manifest 到 Rust ABI binding 的确定性投影。

use std::fmt::Write;

use native_abi::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, INTERFACES, REQUIREMENTS, RIGHTS, TargetArch, status,
    wire as native_wire,
};
use soyo::registry::FeatureFlags;

use crate::contract::ProgramContract;

fn public_ident(name: &str) -> String {
    name.replace('.', "_")
}

pub fn generate_rust_module(target: TargetArch, contract: &ProgramContract) -> Vec<u8> {
    let mut output = String::new();
    writeln!(output, "// 由 soyo-ld 生成，请勿手工修改。").unwrap();
    writeln!(
        output,
        "pub const MYGO_TARGET_ARCH: u16 = {};",
        target as u16
    )
    .unwrap();
    writeln!(
        output,
        "pub const MYGO_ABI_FAMILY: u16 = {ABI_FAMILY_MYGO_NATIVE};"
    )
    .unwrap();
    writeln!(output, "pub const MYGO_ABI_EPOCH: u16 = {ABI_EPOCH};").unwrap();
    writeln!(
        output,
        "pub const MYGO_PAGE_SIZE: u64 = {};",
        native_abi::PAGE_SIZE
    )
    .unwrap();
    writeln!(
        output,
        "pub const MYGO_FEATURE_STATIC_TLS: u64 = {};",
        FeatureFlags::STATIC_TLS.bits()
    )
    .unwrap();
    writeln!(
        output,
        "pub const MYGO_FEATURE_DYNAMIC_COMPONENTS: u64 = {};",
        FeatureFlags::DYNAMIC_COMPONENTS.bits()
    )
    .unwrap();
    writeln!(
        output,
        "pub const MYGO_CALL_SLOT_COUNT: u32 = {};",
        contract.imports().len()
    )
    .unwrap();
    for operation_spec in native_abi::OPERATIONS {
        let slot = contract
            .imports()
            .iter()
            .position(|import| import.operation == operation_spec.id);
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_HAS_{}: bool = {};",
            public_ident(operation_spec.name),
            slot.is_some()
        )
        .unwrap();
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_SLOT_{}: u64 = {};",
            public_ident(operation_spec.name),
            slot.map_or(u64::MAX, |slot| slot as u64)
        )
        .unwrap();
    }
    writeln!(
        output,
        "pub const MYGO_RUNTIME_STACK_SIZE: u64 = {};",
        contract.runtime().stack_size
    )
    .unwrap();
    writeln!(
        output,
        "pub const MYGO_RUNTIME_STACK_GUARD_SIZE: u64 = {};",
        contract.runtime().stack_guard_size
    )
    .unwrap();
    writeln!(
        output,
        "pub const MYGO_START_INFO_MAX_SIZE: u32 = {};",
        contract.runtime().start_info_max_size
    )
    .unwrap();
    writeln!(output).unwrap();

    write_registry_definitions(&mut output);
    write_capability_definitions(&mut output, contract);
    write_wire_types(&mut output);
    output.into_bytes()
}

fn write_registry_definitions(output: &mut String) {
    for spec in INTERFACES {
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_INTERFACE_{}: u16 = {};",
            spec.name, spec.interface as u16
        )
        .unwrap();
    }

    writeln!(output, "pub const MYGO_RIGHT_NONE: u64 = 0;").unwrap();
    for right in RIGHTS {
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_RIGHT_{}: u64 = {};",
            public_ident(right.name),
            right.right.bits()
        )
        .unwrap();
    }

    for requirement in REQUIREMENTS {
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_REQUIREMENT_{}: u32 = {};",
            public_ident(requirement.name),
            requirement.id as u32
        )
        .unwrap();
    }

    for operation in native_abi::OPERATIONS {
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_OPERATION_{}: u32 = {};",
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
        writeln!(output, "pub const MYGO_{name}: u32 = {value};").unwrap();
    }
    writeln!(output, "pub const MYGO_HANDLE_TRANSFER_MOVE: u64 = {};", native_wire::HANDLE_TRANSFER_MOVE).unwrap();
    writeln!(output, "pub const MYGO_MAX_EVENT_PORT_CAPACITY: u32 = {};", native_wire::MAX_EVENT_PORT_CAPACITY).unwrap();
    writeln!(output, "pub const MYGO_MAX_EVENT_BATCH: u32 = {};", native_wire::MAX_EVENT_BATCH).unwrap();
    writeln!(output, "pub const MYGO_MAX_COMPONENT_IMAGES: u32 = {};", native_wire::MAX_COMPONENT_IMAGES).unwrap();
    writeln!(output, "pub const MYGO_MAX_COMPONENT_BINDINGS: u32 = {};", native_wire::MAX_COMPONENT_BINDINGS).unwrap();
    writeln!(output, "pub const MYGO_MAX_PATH_BYTES: u32 = {};", native_wire::MAX_PATH_BYTES).unwrap();
    writeln!(output, "pub const MYGO_MAX_CHANNEL_MESSAGE_BYTES: u32 = {};", native_wire::MAX_CHANNEL_MESSAGE_BYTES).unwrap();
    writeln!(output, "pub const MYGO_MAX_CHANNEL_MESSAGE_HANDLES: u32 = {};", native_wire::MAX_CHANNEL_MESSAGE_HANDLES).unwrap();
    writeln!(output, "pub const MYGO_MAX_CHANNEL_QUEUE_MESSAGES: u32 = {};", native_wire::MAX_CHANNEL_QUEUE_MESSAGES).unwrap();
    writeln!(output, "pub const MYGO_CHANNEL_TRANSFER_MOVE: u64 = {};", native_wire::CHANNEL_TRANSFER_MOVE).unwrap();
    writeln!(output, "pub const MYGO_MAX_RING_ENTRIES: u32 = {};", native_wire::MAX_RING_ENTRIES).unwrap();
    writeln!(output, "pub const MYGO_MAX_RING_BATCH: u32 = {};", native_wire::MAX_RING_BATCH).unwrap();
    writeln!(output, "pub const MYGO_MAX_RING_IO_BYTES: u32 = {};", native_wire::MAX_RING_IO_BYTES).unwrap();
    writeln!(output, "pub const MYGO_RING_SHARED_MAGIC: u32 = {};", native_wire::RING_SHARED_MAGIC).unwrap();
    writeln!(output, "pub const MYGO_RING_SHARED_VERSION: u16 = {};", native_wire::RING_SHARED_VERSION).unwrap();
    for (name, value) in [
        ("NETWORK_FAMILY_IPV4", native_wire::NETWORK_FAMILY_IPV4),
        ("NETWORK_FAMILY_IPV6", native_wire::NETWORK_FAMILY_IPV6),
        ("SOCKET_KIND_STREAM", native_wire::SOCKET_KIND_STREAM),
        ("SOCKET_KIND_DATAGRAM", native_wire::SOCKET_KIND_DATAGRAM),
    ] {
        writeln!(output, "pub const MYGO_{name}: u16 = {value};").unwrap();
    }
    for (name, value) in [
        ("SOCKET_SHUTDOWN_READ", native_wire::SOCKET_SHUTDOWN_READ),
        ("SOCKET_SHUTDOWN_WRITE", native_wire::SOCKET_SHUTDOWN_WRITE),
        ("SOCKET_SHUTDOWN_BOTH", native_wire::SOCKET_SHUTDOWN_BOTH),
    ] {
        writeln!(output, "pub const MYGO_{name}: u32 = {value};").unwrap();
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
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_STATUS_{}: u32 = 0x{value:08x};",
            public_ident(name)
        )
        .unwrap();
    }
    writeln!(output).unwrap();
}

fn write_capability_definitions(output: &mut String, contract: &ProgramContract) {
    writeln!(
        output,
        "pub const MYGO_CAPABILITY_COUNT: u32 = {};",
        contract.capabilities().len()
    )
    .unwrap();
    for capability in contract.capabilities() {
        let spec = native_abi::requirement(capability.requirement)
            .expect("manifest capability 已由 registry 归一化");
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_CAP_{}_required: bool = {};",
            public_ident(spec.name),
            capability.required
        )
        .unwrap();
        writeln!(output, "#[allow(non_upper_case_globals)]").unwrap();
        writeln!(
            output,
            "pub const MYGO_CAP_{}_rights: u64 = {};",
            public_ident(spec.name),
            capability.rights.bits()
        )
        .unwrap();
    }
    writeln!(output).unwrap();
}

fn write_wire_types(output: &mut String) {
    writeln!(output, "#[repr(C)]").unwrap();
    writeln!(
        output,
        "#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]"
    )
    .unwrap();
    writeln!(output, "pub struct MygoNativeCall {{").unwrap();
    writeln!(output, "    pub slot: u64,").unwrap();
    writeln!(output, "    pub object_handle: u64,").unwrap();
    writeln!(output, "    pub args: [u64; 5],").unwrap();
    writeln!(output, "    pub reserved_arg: u64,").unwrap();
    writeln!(output, "}}").unwrap();
    writeln!(output).unwrap();

    writeln!(output, "#[repr(C)]").unwrap();
    writeln!(
        output,
        "#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]"
    )
    .unwrap();
    writeln!(output, "pub struct MygoNativeResult {{").unwrap();
    writeln!(output, "    pub status: u32,").unwrap();
    writeln!(output, "    pub reserved: u32,").unwrap();
    writeln!(output, "    pub value0: u64,").unwrap();
    writeln!(output, "    pub value1: u64,").unwrap();
    writeln!(output, "}}").unwrap();
    writeln!(output).unwrap();

    writeln!(
        output,
        "const _: () = assert!(core::mem::size_of::<MygoNativeCall>() == 64);"
    )
    .unwrap();
    writeln!(
        output,
        "const _: () = assert!(core::mem::offset_of!(MygoNativeCall, args) == 16);"
    )
    .unwrap();
    writeln!(
        output,
        "const _: () = assert!(core::mem::size_of::<MygoNativeResult>() == 24);"
    )
    .unwrap();
    writeln!(
        output,
        "const _: () = assert!(core::mem::offset_of!(MygoNativeResult, value0) == 8);"
    )
    .unwrap();

    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoProcessStringRef {{ pub ptr: u64, pub len: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoProcessArrayRef {{ pub ptr: u64, pub count: u32, pub reserved: u32 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoHandleTransfer {{ pub requirement_id: u32, pub reserved: u32, pub source_handle: u64, pub requested_rights: u64, pub flags: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoSpawnRequest {{ pub image: u64, pub argv: MygoProcessArrayRef, pub env: MygoProcessArrayRef, pub transfers: MygoProcessArrayRef, pub resource_policy: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoProcessResult {{ pub state: u32, pub flags: u32, pub exit_code: u32, pub fault_kind: u32, pub detail0: u64, pub detail1: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoImageInfo {{ pub artifact_kind: u32, pub target_arch: u16, pub abi_epoch: u16, pub enabled_features: u64, pub file_size: u64, pub image_virtual_size: u64, pub component_identity: [u8; 16], pub abi_identity: [u8; 16], pub build_id: [u8; 32], pub content_hash: [u8; 32], pub reserved: [u64; 2] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoEventRecord {{ pub event_kind: u32, pub status: u32, pub source_handle: u64, pub sequence: u64, pub value0: u64, pub value1: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoComponentLoadRequest {{ pub root_image: u64, pub images: MygoProcessArrayRef, pub bindings: MygoProcessArrayRef, pub flags: u64, pub reserved: [u64; 2] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoComponentLifecycle {{ pub action: u32, pub state: u32, pub component: u64, pub entry: u64, pub context: u64, pub tls_identity: u64, pub generation: u64, pub call_state: u64, pub reserved: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoComponentQuery {{ pub state: u32, pub flags: u32, pub generation: u64, pub component_identity: [u8; 16], pub abi_identity: [u8; 16], pub active_calls: u64, pub dependent_count: u32, pub reserved: u32 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoInterfaceRequest {{ pub interface_identity: [u8; 16], pub signature_hash: [u8; 32] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoComponentCallState {{ pub state: u32, pub flags: u32, pub generation: u64, pub active_calls: u64, pub drain_waiter: u32, pub reserved0: u32, pub reserved: [u64; 4] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoComponentContext {{ pub image_base: u64, pub call_state: u64, pub tls_base: u64, pub tls_identity: u64, pub call_slot_count: u32, pub interface_count: u32, pub capability_count: u32, pub flags: u32, pub capabilities: u64, pub reserved: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoComponentCapabilityRecord {{ pub requirement_id: u32, pub reserved0: u32, pub handle: u64, pub granted_rights: u64, pub reserved1: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoComponentInterfaceGate {{ pub call_state: u64, pub target: u64, pub component: u64, pub generation: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoThreadCreateRequest {{ pub entry: u64, pub stack_memory: u64, pub stack_offset: u64, pub stack_size: u64, pub tls_memory: u64, pub tls_offset: u64, pub argument: u64, pub flags: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoThreadResult {{ pub state: u32, pub flags: u32, pub exit_code: u32, pub fault_kind: u32, pub detail0: u64, pub detail1: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoThreadInfo {{ pub state: u32, pub flags: u32, pub identity: u64, pub cpu_time_ns: u64, pub exit_code: u32, pub fault_kind: u32, pub tls_base: u64, pub reserved: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoMemoryCreateRequest {{ pub size: u64, pub alignment: u64, pub flags: u32, pub kind: u32, pub source_handle: u64, pub source_offset: u64, pub reserved: [u64; 3] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoMemoryMapRequest {{ pub address_space: u64, pub offset: u64, pub length: u64, pub alignment: u64, pub address_hint: u64, pub permissions: u32, pub flags: u32, pub reserved: [u64; 2] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoMemoryInfo {{ pub size: u64, pub alignment: u64, pub kind: u32, pub flags: u32, pub mapping_count: u32, pub state: u32, pub generation: u64, pub source_size: u64, pub reserved: [u64; 2] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoMemoryStatistics {{ pub materialized_pages: u64, pub resident_mappings: u64, pub mapped_pages: u64, pub shared_resident_mappings: u64, pub read_operations: u64, pub write_operations: u64, pub bytes_read: u64, pub bytes_written: u64, pub writeback_operations: u64, pub reserved: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoMemoryRegion {{ pub memory: u64, pub offset: u64, pub length: u64, pub generation: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoPathRef {{ pub ptr: u64, pub length: u32, pub flags: u32 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoDirectoryRequest {{ pub path: MygoPathRef, pub kind: u32, pub flags: u32, pub requested_rights: u64, pub reserved: [u64; 4] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoDirectoryInfo {{ pub flags: u32, pub reserved0: u32, pub generation: u64, pub entry_count: u64, pub change_counter: u64, pub reserved: [u64; 4] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoFileInfo {{ pub kind: u32, pub flags: u32, pub size: u64, pub generation: u64, pub modified_ns: u64, pub granted_rights: u64, pub reserved: [u64; 3] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoChannelHandleTransfer {{ pub source_handle: u64, pub requested_rights: u64, pub flags: u64, pub reserved: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoChannelMessage {{ pub data_ptr: u64, pub data_size: u32, pub data_capacity: u32, pub handles_ptr: u64, pub handle_count: u32, pub handle_capacity: u32, pub flags: u64, pub reserved: [u64; 3] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoSubmissionDescriptor {{ pub slot: u64, pub handle: u64, pub arg0: u64, pub arg1: u64, pub arg2: u64, pub arg3: u64, pub arg4: u64, pub user_data: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoCompletionRecord {{ pub user_data: u64, pub status: u32, pub reserved: u32, pub value0: u64, pub value1: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoRingSharedState {{ pub magic: u32, pub version: u16, pub flags: u16, pub entries: u32, pub mask: u32, pub sq_head: u32, pub sq_tail: u32, pub cq_head: u32, pub cq_tail: u32, pub sq_offset: u64, pub cq_offset: u64, pub generation: u64, pub reserved: u64 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoRingInfo {{ pub capacity: u32, pub reserved0: u32, pub queued: u32, pub registered: u32, pub generation: u64, pub completed: u64, pub cancelled: u64, pub reserved: [u64; 3] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoSocketCreateRequest {{ pub family: u16, pub kind: u16, pub protocol: u16, pub flags: u16, pub reserved: [u64; 3] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoNetworkAddress {{ pub family: u16, pub flags: u16, pub port: u16, pub reserved0: u16, pub address: [u8; 16], pub scope_id: u32, pub reserved1: u32 }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoSocketInfo {{ pub family: u16, pub kind: u16, pub protocol: u16, pub state: u16, pub flags: u32, pub reserved0: u32, pub local: MygoNetworkAddress, pub peer: MygoNetworkAddress, pub generation: u64, pub reserved: [u64; 2] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoDeviceRequest {{ pub opcode: u32, pub flags: u32, pub input: MygoMemoryRegion, pub output: MygoMemoryRegion, pub deadline_ns: u64, pub reserved: [u64; 2] }}").unwrap();
    writeln!(output, "#[repr(C)] #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]").unwrap();
    writeln!(output, "pub struct MygoDeviceInfo {{ pub class_id: u64, pub generation: u64, pub state: u32, pub flags: u32, pub contract_hash: [u8; 32], pub name_hash: [u8; 32], pub reserved: u64 }}").unwrap();
    for (type_name, size) in [
        ("MygoProcessStringRef", native_wire::PROCESS_STRING_REF_SIZE),
        ("MygoProcessArrayRef", native_wire::PROCESS_ARRAY_REF_SIZE),
        ("MygoHandleTransfer", native_wire::HANDLE_TRANSFER_SIZE),
        ("MygoSpawnRequest", native_wire::SPAWN_REQUEST_SIZE),
        ("MygoProcessResult", native_wire::PROCESS_RESULT_SIZE),
        ("MygoImageInfo", native_wire::IMAGE_INFO_SIZE),
        ("MygoEventRecord", native_wire::EVENT_RECORD_SIZE),
        ("MygoComponentLoadRequest", native_wire::COMPONENT_LOAD_REQUEST_SIZE),
        ("MygoComponentLifecycle", native_wire::COMPONENT_LIFECYCLE_SIZE),
        ("MygoComponentQuery", native_wire::COMPONENT_QUERY_SIZE),
        ("MygoInterfaceRequest", native_wire::INTERFACE_REQUEST_SIZE),
        ("MygoComponentCallState", native_wire::COMPONENT_CALL_STATE_SIZE),
        ("MygoComponentContext", native_wire::COMPONENT_CONTEXT_SIZE),
        ("MygoComponentCapabilityRecord", native_wire::COMPONENT_CAPABILITY_RECORD_SIZE),
        ("MygoComponentInterfaceGate", native_wire::COMPONENT_INTERFACE_GATE_SIZE),
        ("MygoThreadCreateRequest", native_wire::THREAD_CREATE_REQUEST_SIZE),
        ("MygoThreadResult", native_wire::THREAD_RESULT_SIZE),
        ("MygoThreadInfo", native_wire::THREAD_INFO_SIZE),
        ("MygoMemoryCreateRequest", native_wire::MEMORY_CREATE_REQUEST_SIZE),
        ("MygoMemoryMapRequest", native_wire::MEMORY_MAP_REQUEST_SIZE),
        ("MygoMemoryInfo", native_wire::MEMORY_INFO_SIZE),
        ("MygoMemoryStatistics", native_wire::MEMORY_STATISTICS_SIZE),
        ("MygoMemoryRegion", native_wire::MEMORY_REGION_SIZE),
        ("MygoPathRef", native_wire::PATH_REF_SIZE),
        ("MygoDirectoryRequest", native_wire::DIRECTORY_REQUEST_SIZE),
        ("MygoDirectoryInfo", native_wire::DIRECTORY_INFO_SIZE),
        ("MygoFileInfo", native_wire::FILE_INFO_SIZE),
        ("MygoChannelHandleTransfer", native_wire::CHANNEL_HANDLE_TRANSFER_SIZE),
        ("MygoChannelMessage", native_wire::CHANNEL_MESSAGE_SIZE),
        ("MygoSubmissionDescriptor", native_wire::SUBMISSION_DESCRIPTOR_SIZE),
        ("MygoCompletionRecord", native_wire::COMPLETION_RECORD_SIZE),
        ("MygoRingSharedState", native_wire::RING_SHARED_STATE_SIZE),
        ("MygoRingInfo", native_wire::RING_INFO_SIZE),
        ("MygoSocketCreateRequest", native_wire::SOCKET_CREATE_REQUEST_SIZE),
        ("MygoNetworkAddress", native_wire::NETWORK_ADDRESS_SIZE),
        ("MygoSocketInfo", native_wire::SOCKET_INFO_SIZE),
        ("MygoDeviceRequest", native_wire::DEVICE_REQUEST_SIZE),
        ("MygoDeviceInfo", native_wire::DEVICE_INFO_SIZE),
    ] {
        writeln!(output, "const _: () = assert!(core::mem::size_of::<{type_name}>() == {size});").unwrap();
    }
    for (type_name, field, offset) in [
        ("MygoProcessStringRef", "ptr", native_wire::process_string_ref::PTR),
        ("MygoProcessStringRef", "len", native_wire::process_string_ref::LEN),
        ("MygoProcessArrayRef", "ptr", native_wire::process_array_ref::PTR),
        ("MygoProcessArrayRef", "count", native_wire::process_array_ref::COUNT),
        ("MygoProcessArrayRef", "reserved", native_wire::process_array_ref::RESERVED),
        ("MygoHandleTransfer", "requirement_id", native_wire::handle_transfer::REQUIREMENT_ID),
        ("MygoHandleTransfer", "source_handle", native_wire::handle_transfer::SOURCE_HANDLE),
        ("MygoHandleTransfer", "requested_rights", native_wire::handle_transfer::REQUESTED_RIGHTS),
        ("MygoHandleTransfer", "flags", native_wire::handle_transfer::FLAGS),
        ("MygoSpawnRequest", "image", native_wire::spawn_request::IMAGE),
        ("MygoSpawnRequest", "argv", native_wire::spawn_request::ARGV),
        ("MygoSpawnRequest", "env", native_wire::spawn_request::ENV),
        ("MygoSpawnRequest", "transfers", native_wire::spawn_request::TRANSFERS),
        ("MygoSpawnRequest", "resource_policy", native_wire::spawn_request::RESOURCE_POLICY),
        ("MygoProcessResult", "state", native_wire::process_result::STATE),
        ("MygoProcessResult", "exit_code", native_wire::process_result::EXIT_CODE),
        ("MygoProcessResult", "fault_kind", native_wire::process_result::FAULT_KIND),
        ("MygoProcessResult", "detail0", native_wire::process_result::DETAIL0),
        ("MygoProcessResult", "detail1", native_wire::process_result::DETAIL1),
        ("MygoEventRecord", "event_kind", native_wire::event_record::EVENT_KIND),
        ("MygoEventRecord", "source_handle", native_wire::event_record::SOURCE_HANDLE),
        ("MygoEventRecord", "sequence", native_wire::event_record::SEQUENCE),
        ("MygoEventRecord", "value0", native_wire::event_record::VALUE0),
        ("MygoEventRecord", "value1", native_wire::event_record::VALUE1),
        ("MygoComponentLoadRequest", "root_image", native_wire::component_load_request::ROOT_IMAGE),
        ("MygoComponentLoadRequest", "images", native_wire::component_load_request::IMAGES),
        ("MygoComponentLoadRequest", "bindings", native_wire::component_load_request::BINDINGS),
        ("MygoComponentLoadRequest", "flags", native_wire::component_load_request::FLAGS),
        ("MygoComponentLifecycle", "action", native_wire::component_lifecycle::ACTION),
        ("MygoComponentLifecycle", "state", native_wire::component_lifecycle::STATE),
        ("MygoComponentLifecycle", "component", native_wire::component_lifecycle::COMPONENT),
        ("MygoComponentLifecycle", "entry", native_wire::component_lifecycle::ENTRY),
        ("MygoComponentLifecycle", "context", native_wire::component_lifecycle::CONTEXT),
        ("MygoComponentLifecycle", "tls_identity", native_wire::component_lifecycle::TLS_IDENTITY),
        ("MygoComponentLifecycle", "generation", native_wire::component_lifecycle::GENERATION),
        ("MygoComponentLifecycle", "call_state", native_wire::component_lifecycle::CALL_STATE),
        ("MygoComponentQuery", "state", native_wire::component_query::STATE),
        ("MygoComponentQuery", "generation", native_wire::component_query::GENERATION),
        ("MygoComponentQuery", "component_identity", native_wire::component_query::COMPONENT_IDENTITY),
        ("MygoComponentQuery", "abi_identity", native_wire::component_query::ABI_IDENTITY),
        ("MygoComponentQuery", "active_calls", native_wire::component_query::ACTIVE_CALLS),
        ("MygoComponentQuery", "dependent_count", native_wire::component_query::DEPENDENT_COUNT),
        ("MygoInterfaceRequest", "interface_identity", native_wire::interface_request::INTERFACE_IDENTITY),
        ("MygoInterfaceRequest", "signature_hash", native_wire::interface_request::SIGNATURE_HASH),
        ("MygoComponentCallState", "state", native_wire::component_call_state::STATE),
        ("MygoComponentCallState", "generation", native_wire::component_call_state::GENERATION),
        ("MygoComponentCallState", "active_calls", native_wire::component_call_state::ACTIVE_CALLS),
        ("MygoComponentCallState", "drain_waiter", native_wire::component_call_state::DRAIN_WAITER),
        ("MygoComponentContext", "image_base", native_wire::component_context::IMAGE_BASE),
        ("MygoComponentContext", "call_state", native_wire::component_context::CALL_STATE),
        ("MygoComponentContext", "tls_base", native_wire::component_context::TLS_BASE),
        ("MygoComponentContext", "tls_identity", native_wire::component_context::TLS_IDENTITY),
        ("MygoComponentContext", "call_slot_count", native_wire::component_context::CALL_SLOT_COUNT),
        ("MygoComponentContext", "interface_count", native_wire::component_context::INTERFACE_COUNT),
        ("MygoComponentContext", "capability_count", native_wire::component_context::CAPABILITY_COUNT),
        ("MygoComponentContext", "flags", native_wire::component_context::FLAGS),
        ("MygoComponentContext", "capabilities", native_wire::component_context::CAPABILITIES),
        ("MygoComponentContext", "reserved", native_wire::component_context::RESERVED),
        ("MygoComponentCapabilityRecord", "requirement_id", native_wire::component_capability_record::REQUIREMENT_ID),
        ("MygoComponentCapabilityRecord", "reserved0", native_wire::component_capability_record::RESERVED0),
        ("MygoComponentCapabilityRecord", "handle", native_wire::component_capability_record::HANDLE),
        ("MygoComponentCapabilityRecord", "granted_rights", native_wire::component_capability_record::GRANTED_RIGHTS),
        ("MygoComponentCapabilityRecord", "reserved1", native_wire::component_capability_record::RESERVED1),
        ("MygoComponentInterfaceGate", "call_state", native_wire::component_interface_gate::CALL_STATE),
        ("MygoComponentInterfaceGate", "target", native_wire::component_interface_gate::TARGET),
        ("MygoComponentInterfaceGate", "component", native_wire::component_interface_gate::COMPONENT),
        ("MygoComponentInterfaceGate", "generation", native_wire::component_interface_gate::GENERATION),
        ("MygoThreadCreateRequest", "entry", native_wire::thread_create_request::ENTRY),
        ("MygoThreadCreateRequest", "stack_memory", native_wire::thread_create_request::STACK_MEMORY),
        ("MygoThreadCreateRequest", "stack_offset", native_wire::thread_create_request::STACK_OFFSET),
        ("MygoThreadCreateRequest", "stack_size", native_wire::thread_create_request::STACK_SIZE),
        ("MygoThreadCreateRequest", "tls_memory", native_wire::thread_create_request::TLS_MEMORY),
        ("MygoThreadCreateRequest", "tls_offset", native_wire::thread_create_request::TLS_OFFSET),
        ("MygoThreadCreateRequest", "argument", native_wire::thread_create_request::ARGUMENT),
        ("MygoThreadCreateRequest", "flags", native_wire::thread_create_request::FLAGS),
        ("MygoThreadResult", "state", native_wire::thread_result::STATE),
        ("MygoThreadResult", "flags", native_wire::thread_result::FLAGS),
        ("MygoThreadResult", "exit_code", native_wire::thread_result::EXIT_CODE),
        ("MygoThreadResult", "fault_kind", native_wire::thread_result::FAULT_KIND),
        ("MygoThreadResult", "detail0", native_wire::thread_result::DETAIL0),
        ("MygoThreadResult", "detail1", native_wire::thread_result::DETAIL1),
        ("MygoThreadInfo", "state", native_wire::thread_info::STATE),
        ("MygoThreadInfo", "flags", native_wire::thread_info::FLAGS),
        ("MygoThreadInfo", "identity", native_wire::thread_info::IDENTITY),
        ("MygoThreadInfo", "cpu_time_ns", native_wire::thread_info::CPU_TIME_NS),
        ("MygoThreadInfo", "exit_code", native_wire::thread_info::EXIT_CODE),
        ("MygoThreadInfo", "fault_kind", native_wire::thread_info::FAULT_KIND),
        ("MygoThreadInfo", "tls_base", native_wire::thread_info::TLS_BASE),
        ("MygoThreadInfo", "reserved", native_wire::thread_info::RESERVED),
        ("MygoMemoryCreateRequest", "size", native_wire::memory_create_request::SIZE),
        ("MygoMemoryCreateRequest", "alignment", native_wire::memory_create_request::ALIGNMENT),
        ("MygoMemoryCreateRequest", "flags", native_wire::memory_create_request::FLAGS),
        ("MygoMemoryCreateRequest", "kind", native_wire::memory_create_request::KIND),
        ("MygoMemoryCreateRequest", "source_handle", native_wire::memory_create_request::SOURCE_HANDLE),
        ("MygoMemoryCreateRequest", "source_offset", native_wire::memory_create_request::SOURCE_OFFSET),
        ("MygoMemoryCreateRequest", "reserved", native_wire::memory_create_request::RESERVED),
        ("MygoMemoryMapRequest", "address_space", native_wire::memory_map_request::ADDRESS_SPACE),
        ("MygoMemoryMapRequest", "offset", native_wire::memory_map_request::OFFSET),
        ("MygoMemoryMapRequest", "length", native_wire::memory_map_request::LENGTH),
        ("MygoMemoryMapRequest", "alignment", native_wire::memory_map_request::ALIGNMENT),
        ("MygoMemoryMapRequest", "address_hint", native_wire::memory_map_request::ADDRESS_HINT),
        ("MygoMemoryMapRequest", "permissions", native_wire::memory_map_request::PERMISSIONS),
        ("MygoMemoryMapRequest", "flags", native_wire::memory_map_request::FLAGS),
        ("MygoMemoryMapRequest", "reserved", native_wire::memory_map_request::RESERVED),
        ("MygoMemoryInfo", "size", native_wire::memory_info::SIZE),
        ("MygoMemoryInfo", "alignment", native_wire::memory_info::ALIGNMENT),
        ("MygoMemoryInfo", "kind", native_wire::memory_info::KIND),
        ("MygoMemoryInfo", "flags", native_wire::memory_info::FLAGS),
        ("MygoMemoryInfo", "mapping_count", native_wire::memory_info::MAPPING_COUNT),
        ("MygoMemoryInfo", "state", native_wire::memory_info::STATE),
        ("MygoMemoryInfo", "generation", native_wire::memory_info::GENERATION),
        ("MygoMemoryInfo", "source_size", native_wire::memory_info::SOURCE_SIZE),
        ("MygoMemoryInfo", "reserved", native_wire::memory_info::RESERVED),
        ("MygoMemoryStatistics", "materialized_pages", native_wire::memory_statistics::MATERIALIZED_PAGES),
        ("MygoMemoryStatistics", "resident_mappings", native_wire::memory_statistics::RESIDENT_MAPPINGS),
        ("MygoMemoryStatistics", "mapped_pages", native_wire::memory_statistics::MAPPED_PAGES),
        ("MygoMemoryStatistics", "shared_resident_mappings", native_wire::memory_statistics::SHARED_RESIDENT_MAPPINGS),
        ("MygoMemoryStatistics", "read_operations", native_wire::memory_statistics::READ_OPERATIONS),
        ("MygoMemoryStatistics", "write_operations", native_wire::memory_statistics::WRITE_OPERATIONS),
        ("MygoMemoryStatistics", "bytes_read", native_wire::memory_statistics::BYTES_READ),
        ("MygoMemoryStatistics", "bytes_written", native_wire::memory_statistics::BYTES_WRITTEN),
        ("MygoMemoryStatistics", "writeback_operations", native_wire::memory_statistics::WRITEBACK_OPERATIONS),
        ("MygoMemoryStatistics", "reserved", native_wire::memory_statistics::RESERVED),
        ("MygoMemoryRegion", "memory", native_wire::memory_region::MEMORY),
        ("MygoMemoryRegion", "offset", native_wire::memory_region::OFFSET),
        ("MygoMemoryRegion", "length", native_wire::memory_region::LENGTH),
        ("MygoMemoryRegion", "generation", native_wire::memory_region::GENERATION),
        ("MygoPathRef", "ptr", native_wire::path_ref::PTR),
        ("MygoPathRef", "length", native_wire::path_ref::LENGTH),
        ("MygoPathRef", "flags", native_wire::path_ref::FLAGS),
        ("MygoDirectoryRequest", "path", native_wire::directory_request::PATH),
        ("MygoDirectoryRequest", "kind", native_wire::directory_request::KIND),
        ("MygoDirectoryRequest", "flags", native_wire::directory_request::FLAGS),
        ("MygoDirectoryRequest", "requested_rights", native_wire::directory_request::REQUESTED_RIGHTS),
        ("MygoDirectoryRequest", "reserved", native_wire::directory_request::RESERVED),
        ("MygoDirectoryInfo", "flags", native_wire::directory_info::FLAGS),
        ("MygoDirectoryInfo", "reserved0", native_wire::directory_info::RESERVED0),
        ("MygoDirectoryInfo", "generation", native_wire::directory_info::GENERATION),
        ("MygoDirectoryInfo", "entry_count", native_wire::directory_info::ENTRY_COUNT),
        ("MygoDirectoryInfo", "change_counter", native_wire::directory_info::CHANGE_COUNTER),
        ("MygoDirectoryInfo", "reserved", native_wire::directory_info::RESERVED),
        ("MygoFileInfo", "kind", native_wire::file_info::KIND),
        ("MygoFileInfo", "flags", native_wire::file_info::FLAGS),
        ("MygoFileInfo", "size", native_wire::file_info::SIZE),
        ("MygoFileInfo", "generation", native_wire::file_info::GENERATION),
        ("MygoFileInfo", "modified_ns", native_wire::file_info::MODIFIED_NS),
        ("MygoFileInfo", "granted_rights", native_wire::file_info::GRANTED_RIGHTS),
        ("MygoFileInfo", "reserved", native_wire::file_info::RESERVED),
        ("MygoChannelHandleTransfer", "source_handle", native_wire::channel_handle_transfer::SOURCE_HANDLE),
        ("MygoChannelHandleTransfer", "requested_rights", native_wire::channel_handle_transfer::REQUESTED_RIGHTS),
        ("MygoChannelHandleTransfer", "flags", native_wire::channel_handle_transfer::FLAGS),
        ("MygoChannelHandleTransfer", "reserved", native_wire::channel_handle_transfer::RESERVED),
        ("MygoChannelMessage", "data_ptr", native_wire::channel_message::DATA_PTR),
        ("MygoChannelMessage", "data_size", native_wire::channel_message::DATA_SIZE),
        ("MygoChannelMessage", "data_capacity", native_wire::channel_message::DATA_CAPACITY),
        ("MygoChannelMessage", "handles_ptr", native_wire::channel_message::HANDLES_PTR),
        ("MygoChannelMessage", "handle_count", native_wire::channel_message::HANDLE_COUNT),
        ("MygoChannelMessage", "handle_capacity", native_wire::channel_message::HANDLE_CAPACITY),
        ("MygoChannelMessage", "flags", native_wire::channel_message::FLAGS),
        ("MygoChannelMessage", "reserved", native_wire::channel_message::RESERVED),
        ("MygoRingSharedState", "magic", native_wire::ring_shared_state::MAGIC),
        ("MygoRingSharedState", "version", native_wire::ring_shared_state::VERSION),
        ("MygoRingSharedState", "flags", native_wire::ring_shared_state::FLAGS),
        ("MygoRingSharedState", "entries", native_wire::ring_shared_state::ENTRIES),
        ("MygoRingSharedState", "mask", native_wire::ring_shared_state::MASK),
        ("MygoRingSharedState", "sq_head", native_wire::ring_shared_state::SQ_HEAD),
        ("MygoRingSharedState", "sq_tail", native_wire::ring_shared_state::SQ_TAIL),
        ("MygoRingSharedState", "cq_head", native_wire::ring_shared_state::CQ_HEAD),
        ("MygoRingSharedState", "cq_tail", native_wire::ring_shared_state::CQ_TAIL),
        ("MygoRingSharedState", "sq_offset", native_wire::ring_shared_state::SQ_OFFSET),
        ("MygoRingSharedState", "cq_offset", native_wire::ring_shared_state::CQ_OFFSET),
        ("MygoRingSharedState", "generation", native_wire::ring_shared_state::GENERATION),
        ("MygoRingSharedState", "reserved", native_wire::ring_shared_state::RESERVED),
    ] {
        writeln!(output, "const _: () = assert!(core::mem::offset_of!({type_name}, {field}) == {offset});").unwrap();
    }
}

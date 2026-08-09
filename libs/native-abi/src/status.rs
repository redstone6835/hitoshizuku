//! Native ABI status facility 与 code 注册值。

pub const OK: u32 = 0x0000_0000;
pub const CORE_INVALID_ARGUMENT: u32 = 0x0100_0001;
pub const CORE_OUT_OF_RANGE: u32 = 0x0100_0002;
pub const CORE_RESOURCE_EXHAUSTED: u32 = 0x0100_0003;
pub const ABI_BAD_SLOT: u32 = 0x0200_0001;
pub const ABI_SIGNATURE_MISMATCH: u32 = 0x0200_0002;
pub const ABI_UNSUPPORTED_OPERATION: u32 = 0x0200_0003;
pub const HANDLE_INVALID: u32 = 0x0300_0001;
pub const HANDLE_STALE: u32 = 0x0300_0002;
pub const HANDLE_WRONG_INTERFACE: u32 = 0x0300_0003;
pub const SECURITY_RIGHTS_DENIED: u32 = 0x0400_0001;
pub const STREAM_FAULT: u32 = 0x0500_0001;
pub const STREAM_WOULD_BLOCK: u32 = 0x0500_0002;
pub const STREAM_END: u32 = 0x0500_0003;
pub const STREAM_CLOSED: u32 = 0x0500_0004;
pub const STREAM_ERROR: u32 = 0x0500_0005;
pub const MEMORY_INVALID_RANGE: u32 = 0x0600_0001;
pub const MEMORY_INVALID_ALIGNMENT: u32 = 0x0600_0002;
pub const MEMORY_NOT_OWNED: u32 = 0x0600_0003;
pub const MEMORY_REVOKED: u32 = 0x0600_0004;
pub const MEMORY_POISONED: u32 = 0x0600_0005;
pub const PROCESS_NOT_CHILD: u32 = 0x0700_0001;
pub const PROCESS_ALREADY_REAPED: u32 = 0x0700_0002;
pub const PROCESS_WOULD_BLOCK: u32 = 0x0700_0003;
pub const PROCESS_INVALID_STATE: u32 = 0x0700_0004;
pub const PROCESS_WAIT_IN_PROGRESS: u32 = 0x0700_0005;
pub const IMAGE_INVALID: u32 = 0x0800_0001;
pub const IMAGE_ARCH_MISMATCH: u32 = 0x0800_0002;
pub const IMAGE_NOT_EXECUTABLE: u32 = 0x0800_0003;
pub const IMAGE_UNSIGNED: u32 = 0x0800_0004;
pub const IMAGE_UNKNOWN_KEY: u32 = 0x0800_0005;
pub const IMAGE_BAD_SIGNATURE: u32 = 0x0800_0006;
pub const IMAGE_REVOKED: u32 = 0x0800_0007;
pub const IMAGE_ROLLBACK: u32 = 0x0800_0008;
pub const EVENT_INVALID_TOKEN: u32 = 0x0900_0001;
pub const EVENT_SOURCE_UNSUPPORTED: u32 = 0x0900_0002;
pub const EVENT_WOULD_BLOCK: u32 = 0x0900_0003;
pub const EVENT_TIMEOUT: u32 = 0x0900_0004;
pub const EVENT_QUEUE_EXHAUSTED: u32 = 0x0900_0005;
pub const EVENT_CANCELLED: u32 = 0x0900_0006;
pub const COMPONENT_INVALID_IMAGE: u32 = 0x0a00_0001;
pub const COMPONENT_DEPENDENCY_MISSING: u32 = 0x0a00_0002;
pub const COMPONENT_DEPENDENCY_CONFLICT: u32 = 0x0a00_0003;
pub const COMPONENT_DEPENDENCY_CYCLE: u32 = 0x0a00_0004;
pub const COMPONENT_INITIALIZING: u32 = 0x0a00_0005;
pub const COMPONENT_ACTIVE: u32 = 0x0a00_0006;
pub const COMPONENT_IN_USE: u32 = 0x0a00_0007;
pub const COMPONENT_DRAINING: u32 = 0x0a00_0008;
pub const COMPONENT_TIMEOUT: u32 = 0x0a00_0009;
pub const COMPONENT_UNLOADED: u32 = 0x0a00_000a;
pub const COMPONENT_SELF_UNLOAD: u32 = 0x0a00_000b;
pub const COMPONENT_LIFECYCLE_FAILED: u32 = 0x0a00_000c;
pub const COMPONENT_INVALID_TRANSACTION: u32 = 0x0a00_000d;
pub const THREAD_INVALID: u32 = 0x0b00_0001;
pub const THREAD_WOULD_BLOCK: u32 = 0x0b00_0002;
pub const THREAD_TIMEOUT: u32 = 0x0b00_0003;
pub const THREAD_ALREADY_EXITED: u32 = 0x0b00_0004;
pub const THREAD_SELF: u32 = 0x0b00_0005;
pub const FILESYSTEM_INVALID_PATH: u32 = 0x0c00_0001;
pub const FILESYSTEM_NOT_FOUND: u32 = 0x0c00_0002;
pub const FILESYSTEM_ALREADY_EXISTS: u32 = 0x0c00_0003;
pub const FILESYSTEM_NOT_DIRECTORY: u32 = 0x0c00_0004;
pub const FILESYSTEM_IS_DIRECTORY: u32 = 0x0c00_0005;
pub const FILESYSTEM_NOT_EMPTY: u32 = 0x0c00_0006;
pub const FILESYSTEM_READ_ONLY: u32 = 0x0c00_0007;
pub const FILESYSTEM_END: u32 = 0x0c00_0008;
pub const FILESYSTEM_CHANGED: u32 = 0x0c00_0009;
pub const FILESYSTEM_ERROR: u32 = 0x0c00_000a;
pub const CHANNEL_FULL: u32 = 0x0d00_0001;
pub const CHANNEL_EMPTY: u32 = 0x0d00_0002;
pub const CHANNEL_PEER_CLOSED: u32 = 0x0d00_0003;
pub const CHANNEL_MESSAGE_TOO_LARGE: u32 = 0x0d00_0004;
pub const CHANNEL_BUFFER_TOO_SMALL: u32 = 0x0d00_0005;
pub const CHANNEL_TRANSFER_INVALID: u32 = 0x0d00_0006;
pub const RING_FULL: u32 = 0x0e00_0001;
pub const RING_EMPTY: u32 = 0x0e00_0002;
pub const RING_INVALID_DESCRIPTOR: u32 = 0x0e00_0003;
pub const RING_TOKEN_STALE: u32 = 0x0e00_0004;
pub const RING_NOT_FOUND: u32 = 0x0e00_0005;
pub const RING_CANCELLED: u32 = 0x0e00_0006;
pub const RING_UNSUPPORTED: u32 = 0x0e00_0007;
pub const RING_TIMEOUT: u32 = 0x0e00_0008;
pub const RING_BUSY: u32 = 0x0e00_0009;
pub const SOCKET_INVALID_ADDRESS: u32 = 0x0f00_0001;
pub const SOCKET_INVALID_STATE: u32 = 0x0f00_0002;
pub const SOCKET_WOULD_BLOCK: u32 = 0x0f00_0003;
pub const SOCKET_TIMEOUT: u32 = 0x0f00_0004;
pub const SOCKET_PEER_CLOSED: u32 = 0x0f00_0005;
pub const SOCKET_ADDRESS_IN_USE: u32 = 0x0f00_0006;
pub const SOCKET_CONNECTION_REFUSED: u32 = 0x0f00_0007;
pub const SOCKET_NETWORK_UNREACHABLE: u32 = 0x0f00_0008;
pub const SOCKET_ERROR: u32 = 0x0f00_0009;
pub const DEVICE_GONE: u32 = 0x1000_0001;
pub const DEVICE_BUSY: u32 = 0x1000_0002;
pub const DEVICE_UNSUPPORTED: u32 = 0x1000_0003;
pub const DEVICE_FAULT: u32 = 0x1000_0004;
pub const DEVICE_INVALID_REQUEST: u32 = 0x1000_0005;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSpec {
    pub name: &'static str,
    pub value: u32,
}

macro_rules! status_codes {
    ($(($name:literal, $value:ident)),+ $(,)?) => {
        pub const STATUS_CODES: &[StatusSpec] = &[
            $(StatusSpec { name: $name, value: $value },)+
        ];
    };
}

status_codes!(
    ("ok", OK),
    ("core.invalid_argument", CORE_INVALID_ARGUMENT),
    ("core.out_of_range", CORE_OUT_OF_RANGE),
    ("core.resource_exhausted", CORE_RESOURCE_EXHAUSTED),
    ("abi.bad_slot", ABI_BAD_SLOT),
    ("abi.signature_mismatch", ABI_SIGNATURE_MISMATCH),
    ("abi.unsupported_operation", ABI_UNSUPPORTED_OPERATION),
    ("handle.invalid", HANDLE_INVALID),
    ("handle.stale", HANDLE_STALE),
    ("handle.wrong_interface", HANDLE_WRONG_INTERFACE),
    ("security.rights_denied", SECURITY_RIGHTS_DENIED),
    ("stream.fault", STREAM_FAULT),
    ("stream.would_block", STREAM_WOULD_BLOCK),
    ("stream.end", STREAM_END),
    ("stream.closed", STREAM_CLOSED),
    ("stream.error", STREAM_ERROR),
    ("memory.invalid_range", MEMORY_INVALID_RANGE),
    ("memory.invalid_alignment", MEMORY_INVALID_ALIGNMENT),
    ("memory.not_owned", MEMORY_NOT_OWNED),
    ("memory.revoked", MEMORY_REVOKED),
    ("memory.poisoned", MEMORY_POISONED),
    ("process.not_child", PROCESS_NOT_CHILD),
    ("process.already_reaped", PROCESS_ALREADY_REAPED),
    ("process.would_block", PROCESS_WOULD_BLOCK),
    ("process.invalid_state", PROCESS_INVALID_STATE),
    ("process.wait_in_progress", PROCESS_WAIT_IN_PROGRESS),
    ("image.invalid", IMAGE_INVALID),
    ("image.arch_mismatch", IMAGE_ARCH_MISMATCH),
    ("image.not_executable", IMAGE_NOT_EXECUTABLE),
    ("image.unsigned", IMAGE_UNSIGNED),
    ("image.unknown_key", IMAGE_UNKNOWN_KEY),
    ("image.bad_signature", IMAGE_BAD_SIGNATURE),
    ("image.revoked", IMAGE_REVOKED),
    ("image.rollback", IMAGE_ROLLBACK),
    ("event.invalid_token", EVENT_INVALID_TOKEN),
    ("event.source_unsupported", EVENT_SOURCE_UNSUPPORTED),
    ("event.would_block", EVENT_WOULD_BLOCK),
    ("event.timeout", EVENT_TIMEOUT),
    ("event.queue_exhausted", EVENT_QUEUE_EXHAUSTED),
    ("event.cancelled", EVENT_CANCELLED),
    ("component.invalid_image", COMPONENT_INVALID_IMAGE),
    ("component.dependency_missing", COMPONENT_DEPENDENCY_MISSING),
    (
        "component.dependency_conflict",
        COMPONENT_DEPENDENCY_CONFLICT
    ),
    ("component.dependency_cycle", COMPONENT_DEPENDENCY_CYCLE),
    ("component.initializing", COMPONENT_INITIALIZING),
    ("component.active", COMPONENT_ACTIVE),
    ("component.in_use", COMPONENT_IN_USE),
    ("component.draining", COMPONENT_DRAINING),
    ("component.timeout", COMPONENT_TIMEOUT),
    ("component.unloaded", COMPONENT_UNLOADED),
    ("component.self_unload", COMPONENT_SELF_UNLOAD),
    ("component.lifecycle_failed", COMPONENT_LIFECYCLE_FAILED),
    (
        "component.invalid_transaction",
        COMPONENT_INVALID_TRANSACTION
    ),
    ("thread.invalid", THREAD_INVALID),
    ("thread.would_block", THREAD_WOULD_BLOCK),
    ("thread.timeout", THREAD_TIMEOUT),
    ("thread.already_exited", THREAD_ALREADY_EXITED),
    ("thread.self", THREAD_SELF),
    ("filesystem.invalid_path", FILESYSTEM_INVALID_PATH),
    ("filesystem.not_found", FILESYSTEM_NOT_FOUND),
    ("filesystem.already_exists", FILESYSTEM_ALREADY_EXISTS),
    ("filesystem.not_directory", FILESYSTEM_NOT_DIRECTORY),
    ("filesystem.is_directory", FILESYSTEM_IS_DIRECTORY),
    ("filesystem.not_empty", FILESYSTEM_NOT_EMPTY),
    ("filesystem.read_only", FILESYSTEM_READ_ONLY),
    ("filesystem.end", FILESYSTEM_END),
    ("filesystem.changed", FILESYSTEM_CHANGED),
    ("filesystem.error", FILESYSTEM_ERROR),
    ("channel.full", CHANNEL_FULL),
    ("channel.empty", CHANNEL_EMPTY),
    ("channel.peer_closed", CHANNEL_PEER_CLOSED),
    ("channel.message_too_large", CHANNEL_MESSAGE_TOO_LARGE),
    ("channel.buffer_too_small", CHANNEL_BUFFER_TOO_SMALL),
    ("channel.transfer_invalid", CHANNEL_TRANSFER_INVALID),
    ("ring.full", RING_FULL),
    ("ring.empty", RING_EMPTY),
    ("ring.invalid_descriptor", RING_INVALID_DESCRIPTOR),
    ("ring.token_stale", RING_TOKEN_STALE),
    ("ring.not_found", RING_NOT_FOUND),
    ("ring.cancelled", RING_CANCELLED),
    ("ring.unsupported", RING_UNSUPPORTED),
    ("ring.timeout", RING_TIMEOUT),
    ("ring.busy", RING_BUSY),
    ("socket.invalid_address", SOCKET_INVALID_ADDRESS),
    ("socket.invalid_state", SOCKET_INVALID_STATE),
    ("socket.would_block", SOCKET_WOULD_BLOCK),
    ("socket.timeout", SOCKET_TIMEOUT),
    ("socket.peer_closed", SOCKET_PEER_CLOSED),
    ("socket.address_in_use", SOCKET_ADDRESS_IN_USE),
    ("socket.connection_refused", SOCKET_CONNECTION_REFUSED),
    ("socket.network_unreachable", SOCKET_NETWORK_UNREACHABLE),
    ("socket.error", SOCKET_ERROR),
    ("device.gone", DEVICE_GONE),
    ("device.busy", DEVICE_BUSY),
    ("device.unsupported", DEVICE_UNSUPPORTED),
    ("device.fault", DEVICE_FAULT),
    ("device.invalid_request", DEVICE_INVALID_REQUEST),
);

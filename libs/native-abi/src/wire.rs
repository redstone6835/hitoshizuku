//! 内核生成的 Native ABI 启动区固定线格式。

pub const START_INFO_SIZE: usize = 192;
pub const STRING_REF_SIZE: usize = 8;
pub const INITIAL_HANDLE_SIZE: usize = 32;
pub const PROCESS_STRING_REF_SIZE: usize = 16;
pub const PROCESS_ARRAY_REF_SIZE: usize = 16;
pub const HANDLE_TRANSFER_SIZE: usize = 32;
pub const SPAWN_REQUEST_SIZE: usize = 64;
pub const PROCESS_RESULT_SIZE: usize = 32;
pub const IMAGE_INFO_SIZE: usize = 144;
pub const EVENT_RECORD_SIZE: usize = 40;
pub const COMPONENT_LOAD_REQUEST_SIZE: usize = 64;
pub const COMPONENT_LIFECYCLE_SIZE: usize = 64;
pub const COMPONENT_QUERY_SIZE: usize = 64;
pub const INTERFACE_REQUEST_SIZE: usize = 48;
pub const COMPONENT_CALL_STATE_SIZE: usize = 64;
pub const COMPONENT_CONTEXT_SIZE: usize = 64;
pub const COMPONENT_CAPABILITY_RECORD_SIZE: usize = 32;
pub const COMPONENT_INTERFACE_GATE_SIZE: usize = 32;
pub const THREAD_CREATE_REQUEST_SIZE: usize = 64;
pub const THREAD_RESULT_SIZE: usize = 32;
pub const THREAD_INFO_SIZE: usize = 48;
pub const MEMORY_CREATE_REQUEST_SIZE: usize = 64;
pub const MEMORY_MAP_REQUEST_SIZE: usize = 64;
pub const MEMORY_INFO_SIZE: usize = 64;
pub const MEMORY_STATISTICS_SIZE: usize = 80;
pub const MEMORY_REGION_SIZE: usize = 32;
pub const PATH_REF_SIZE: usize = 16;
pub const DIRECTORY_REQUEST_SIZE: usize = 64;
pub const DIRECTORY_INFO_SIZE: usize = 64;
pub const FILE_INFO_SIZE: usize = 64;
pub const CHANNEL_HANDLE_TRANSFER_SIZE: usize = 32;
pub const CHANNEL_MESSAGE_SIZE: usize = 64;
pub const SUBMISSION_DESCRIPTOR_SIZE: usize = 64;
pub const COMPLETION_RECORD_SIZE: usize = 32;
pub const RING_SHARED_STATE_SIZE: usize = 64;
pub const RING_INFO_SIZE: usize = 64;
pub const SOCKET_CREATE_REQUEST_SIZE: usize = 32;
pub const NETWORK_ADDRESS_SIZE: usize = 32;
pub const SOCKET_INFO_SIZE: usize = 104;
pub const DEVICE_REQUEST_SIZE: usize = 96;
pub const DEVICE_INFO_SIZE: usize = 96;
pub const MAX_EVENT_PORT_CAPACITY: u32 = 4096;
pub const MAX_EVENT_BATCH: u32 = 64;
pub const MAX_COMPONENT_IMAGES: u32 = 256;
pub const MAX_COMPONENT_BINDINGS: u32 = 4096;
pub const MAX_PATH_BYTES: u32 = 4096;
pub const MAX_CHANNEL_MESSAGE_BYTES: u32 = 1024 * 1024;
pub const MAX_CHANNEL_MESSAGE_HANDLES: u32 = 64;
pub const MAX_CHANNEL_QUEUE_MESSAGES: u32 = 1024;
pub const MAX_RING_ENTRIES: u32 = 4096;
pub const MAX_RING_BATCH: u32 = 64;
pub const MAX_RING_IO_BYTES: u32 = 1024 * 1024;
pub const RING_SHARED_MAGIC: u32 = u32::from_le_bytes(*b"ring");
pub const RING_SHARED_VERSION: u16 = 1;
pub const NETWORK_FAMILY_IPV4: u16 = 1;
pub const NETWORK_FAMILY_IPV6: u16 = 2;
pub const SOCKET_KIND_STREAM: u16 = 1;
pub const SOCKET_KIND_DATAGRAM: u16 = 2;
pub const SOCKET_SHUTDOWN_READ: u32 = 1;
pub const SOCKET_SHUTDOWN_WRITE: u32 = 2;
pub const SOCKET_SHUTDOWN_BOTH: u32 = 3;

/// `HandleTransfer::flags` 中要求在 child commit 时关闭父侧 source handle。
pub const HANDLE_TRANSFER_MOVE: u64 = 1;

pub const PROCESS_STATE_RUNNING: u32 = 1;
pub const PROCESS_STATE_TERMINATING: u32 = 2;
pub const PROCESS_STATE_EXITED: u32 = 3;
pub const PROCESS_STATE_FAULTED: u32 = 4;
pub const PROCESS_STATE_REAPED: u32 = 5;

pub const PROCESS_FAULT_MEMORY: u32 = 1;
pub const PROCESS_FAULT_ILLEGAL_INSTRUCTION: u32 = 2;
pub const PROCESS_FAULT_BREAKPOINT: u32 = 3;
pub const PROCESS_FAULT_ADDRESS: u32 = 4;
pub const PROCESS_FAULT_ARITHMETIC: u32 = 5;
pub const PROCESS_FAULT_RESOURCE: u32 = 6;
pub const PROCESS_FAULT_OTHER: u32 = 0xff;

pub const EVENT_KIND_PROCESS_EXITED: u32 = 1;
pub const EVENT_KIND_PROCESS_FAULT: u32 = 2;
pub const EVENT_KIND_STREAM_READY: u32 = 3;
pub const EVENT_KIND_TIMER_EXPIRED: u32 = 4;

pub const EVENT_STREAM_READABLE: u32 = 1 << 0;
pub const EVENT_STREAM_WRITABLE: u32 = 1 << 1;
pub const EVENT_STREAM_ERROR: u32 = 1 << 2;
pub const EVENT_STREAM_CLOSED: u32 = 1 << 3;

pub const COMPONENT_ACTION_NONE: u32 = 0;
pub const COMPONENT_ACTION_INITIALIZE: u32 = 1;
pub const COMPONENT_ACTION_FINALIZE: u32 = 2;

pub const COMPONENT_STATE_PREPARING: u32 = 1;
pub const COMPONENT_STATE_INITIALIZING: u32 = 2;
pub const COMPONENT_STATE_ACTIVE: u32 = 3;
pub const COMPONENT_STATE_DRAINING: u32 = 4;
pub const COMPONENT_STATE_FINALIZING: u32 = 5;
pub const COMPONENT_STATE_UNLOADED: u32 = 6;
pub const COMPONENT_STATE_FAILED: u32 = 7;

pub const IMAGE_ARTIFACT_EXECUTABLE: u32 = 1;
pub const IMAGE_ARTIFACT_SHARED_COMPONENT: u32 = 2;

pub const THREAD_STATE_RUNNING: u32 = 1;
pub const THREAD_STATE_EXITED: u32 = 2;
pub const THREAD_STATE_FAULTED: u32 = 3;

pub const MEMORY_KIND_ANONYMOUS: u32 = 1;
pub const MEMORY_KIND_FILE: u32 = 2;
pub const MEMORY_KIND_IMAGE: u32 = 3;
pub const MEMORY_KIND_DMA: u32 = 4;
pub const MEMORY_FLAG_SHARED: u32 = 1;
pub const MEMORY_FLAG_DEVICE_READ: u32 = 1 << 1;
pub const MEMORY_FLAG_DEVICE_WRITE: u32 = 1 << 2;
pub const MEMORY_STATE_ACTIVE: u32 = 1;
pub const MEMORY_STATE_REVOKED: u32 = 2;
pub const MEMORY_STATE_POISONED: u32 = 3;
pub const MEMORY_PERMISSION_READ: u32 = 1 << 0;
pub const MEMORY_PERMISSION_WRITE: u32 = 1 << 1;
pub const MEMORY_PERMISSION_EXECUTE: u32 = 1 << 2;
pub const MEMORY_MAP_FIXED: u32 = 1;

pub const DIRECTORY_ENTRY_FILE: u32 = 1;
pub const DIRECTORY_ENTRY_DIRECTORY: u32 = 2;
pub const DIRECTORY_REMOVE_DIRECTORY: u32 = 1;

pub const CHANNEL_TRANSFER_MOVE: u64 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ProcessStringRef {
    pub ptr: u64,
    pub len: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ProcessArrayRef {
    pub ptr: u64,
    pub count: u32,
    pub reserved: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct HandleTransfer {
    pub requirement_id: u32,
    pub reserved: u32,
    pub source_handle: u64,
    pub requested_rights: u64,
    pub flags: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct SpawnRequest {
    pub image: u64,
    pub argv: ProcessArrayRef,
    pub env: ProcessArrayRef,
    pub transfers: ProcessArrayRef,
    pub resource_policy: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ProcessResult {
    pub state: u32,
    pub flags: u32,
    pub exit_code: u32,
    pub fault_kind: u32,
    pub detail0: u64,
    pub detail1: u64,
}

/// 内核完成 SOYO 校验后发布的不可变映像身份。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ImageInfo {
    pub artifact_kind: u32,
    pub target_arch: u16,
    pub abi_epoch: u16,
    pub enabled_features: u64,
    pub file_size: u64,
    pub image_virtual_size: u64,
    pub component_identity: [u8; 16],
    pub abi_identity: [u8; 16],
    pub build_id: [u8; 32],
    pub content_hash: [u8; 32],
    pub reserved: [u64; 2],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct EventRecord {
    pub event_kind: u32,
    pub status: u32,
    pub source_handle: u64,
    pub sequence: u64,
    pub value0: u64,
    pub value1: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ComponentLoadRequest {
    pub root_image: u64,
    pub images: ProcessArrayRef,
    pub bindings: ProcessArrayRef,
    pub flags: u64,
    pub reserved: [u64; 2],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ComponentLifecycle {
    pub action: u32,
    pub state: u32,
    pub component: u64,
    pub entry: u64,
    pub context: u64,
    pub tls_identity: u64,
    pub generation: u64,
    pub call_state: u64,
    pub reserved: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ComponentQuery {
    pub state: u32,
    pub flags: u32,
    pub generation: u64,
    pub component_identity: [u8; 16],
    pub abi_identity: [u8; 16],
    pub active_calls: u64,
    pub dependent_count: u32,
    pub reserved: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct InterfaceRequest {
    pub interface_identity: [u8; 16],
    pub signature_hash: [u8; 32],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ComponentCallState {
    pub state: u32,
    pub flags: u32,
    pub generation: u64,
    pub active_calls: u64,
    pub drain_waiter: u32,
    pub reserved0: u32,
    pub reserved: [u64; 4],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ComponentContext {
    pub image_base: u64,
    pub call_state: u64,
    pub tls_base: u64,
    pub tls_identity: u64,
    pub call_slot_count: u32,
    pub interface_count: u32,
    pub capability_count: u32,
    pub flags: u32,
    pub capabilities: u64,
    pub reserved: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ComponentCapabilityRecord {
    pub requirement_id: u32,
    pub reserved0: u32,
    pub handle: u64,
    pub granted_rights: u64,
    pub reserved1: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ComponentInterfaceGate {
    pub call_state: u64,
    pub target: u64,
    pub component: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ThreadCreateRequest {
    pub entry: u64,
    pub stack_memory: u64,
    pub stack_offset: u64,
    pub stack_size: u64,
    pub tls_memory: u64,
    pub tls_offset: u64,
    pub argument: u64,
    pub flags: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ThreadResult {
    pub state: u32,
    pub flags: u32,
    pub exit_code: u32,
    pub fault_kind: u32,
    pub detail0: u64,
    pub detail1: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ThreadInfo {
    pub state: u32,
    pub flags: u32,
    pub identity: u64,
    pub cpu_time_ns: u64,
    pub exit_code: u32,
    pub fault_kind: u32,
    pub tls_base: u64,
    pub reserved: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct MemoryCreateRequest {
    pub size: u64,
    pub alignment: u64,
    pub flags: u32,
    pub kind: u32,
    pub source_handle: u64,
    pub source_offset: u64,
    pub reserved: [u64; 3],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct MemoryMapRequest {
    pub address_space: u64,
    pub offset: u64,
    pub length: u64,
    pub alignment: u64,
    pub address_hint: u64,
    pub permissions: u32,
    pub flags: u32,
    pub reserved: [u64; 2],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct MemoryInfo {
    pub size: u64,
    pub alignment: u64,
    pub kind: u32,
    pub flags: u32,
    pub mapping_count: u32,
    pub state: u32,
    pub generation: u64,
    pub source_size: u64,
    pub reserved: [u64; 2],
}

/// MemoryObject 的只读驻留与数据面快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct MemoryStatistics {
    pub materialized_pages: u64,
    pub resident_mappings: u64,
    pub mapped_pages: u64,
    pub shared_resident_mappings: u64,
    pub read_operations: u64,
    pub write_operations: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub writeback_operations: u64,
    pub reserved: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct MemoryRegion {
    pub memory: u64,
    pub offset: u64,
    pub length: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct PathRef {
    pub ptr: u64,
    pub length: u32,
    pub flags: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct DirectoryRequest {
    pub path: PathRef,
    pub kind: u32,
    pub flags: u32,
    pub requested_rights: u64,
    pub reserved: [u64; 4],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct DirectoryInfo {
    pub flags: u32,
    pub reserved0: u32,
    pub generation: u64,
    pub entry_count: u64,
    pub change_counter: u64,
    pub reserved: [u64; 4],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct FileInfo {
    pub kind: u32,
    pub flags: u32,
    pub size: u64,
    pub generation: u64,
    pub modified_ns: u64,
    pub granted_rights: u64,
    pub reserved: [u64; 3],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ChannelHandleTransfer {
    pub source_handle: u64,
    pub requested_rights: u64,
    pub flags: u64,
    pub reserved: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ChannelMessage {
    pub data_ptr: u64,
    pub data_size: u32,
    pub data_capacity: u32,
    pub handles_ptr: u64,
    pub handle_count: u32,
    pub handle_capacity: u32,
    pub flags: u64,
    pub reserved: [u64; 3],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct SubmissionDescriptor {
    pub slot: u64,
    pub handle: u64,
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    pub arg4: u64,
    pub user_data: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct CompletionRecord {
    pub user_data: u64,
    pub status: u32,
    pub reserved: u32,
    pub value0: u64,
    pub value1: u64,
}

/// 用户态与内核共同访问的 SubmissionRing 控制页。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct RingSharedState {
    pub magic: u32,
    pub version: u16,
    pub flags: u16,
    pub entries: u32,
    pub mask: u32,
    pub sq_head: u32,
    pub sq_tail: u32,
    pub cq_head: u32,
    pub cq_tail: u32,
    pub sq_offset: u64,
    pub cq_offset: u64,
    pub generation: u64,
    pub reserved: u64,
}

/// 计算单调 u32 head/tail 表示的队列长度，并拒绝损坏的共享状态。
pub const fn ring_queue_len(head: u32, tail: u32, entries: u32) -> Option<u32> {
    if entries < 2 || entries > MAX_RING_ENTRIES || !entries.is_power_of_two() {
        return None;
    }
    let queued = tail.wrapping_sub(head);
    if queued <= entries {
        Some(queued)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct RingInfo {
    pub capacity: u32,
    pub reserved0: u32,
    pub queued: u32,
    pub registered: u32,
    pub generation: u64,
    pub completed: u64,
    pub cancelled: u64,
    pub reserved: [u64; 3],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct SocketCreateRequest {
    pub family: u16,
    pub kind: u16,
    pub protocol: u16,
    pub flags: u16,
    pub reserved: [u64; 3],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct NetworkAddress {
    pub family: u16,
    pub flags: u16,
    pub port: u16,
    pub reserved0: u16,
    pub address: [u8; 16],
    pub scope_id: u32,
    pub reserved1: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct SocketInfo {
    pub family: u16,
    pub kind: u16,
    pub protocol: u16,
    pub state: u16,
    pub flags: u32,
    pub reserved0: u32,
    pub local: NetworkAddress,
    pub peer: NetworkAddress,
    pub generation: u64,
    pub reserved: [u64; 2],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct DeviceRequest {
    pub opcode: u32,
    pub flags: u32,
    pub input: MemoryRegion,
    pub output: MemoryRegion,
    pub deadline_ns: u64,
    pub reserved: [u64; 2],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct DeviceInfo {
    pub class_id: u64,
    pub generation: u64,
    pub state: u32,
    pub flags: u32,
    pub contract_hash: [u8; 32],
    pub name_hash: [u8; 32],
    pub reserved: u64,
}

const _: () = assert!(core::mem::size_of::<ProcessStringRef>() == PROCESS_STRING_REF_SIZE);
const _: () = assert!(core::mem::size_of::<ProcessArrayRef>() == PROCESS_ARRAY_REF_SIZE);
const _: () = assert!(core::mem::size_of::<HandleTransfer>() == HANDLE_TRANSFER_SIZE);
const _: () = assert!(core::mem::size_of::<SpawnRequest>() == SPAWN_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<ProcessResult>() == PROCESS_RESULT_SIZE);
const _: () = assert!(core::mem::size_of::<ImageInfo>() == IMAGE_INFO_SIZE);
const _: () = assert!(core::mem::size_of::<EventRecord>() == EVENT_RECORD_SIZE);
const _: () = assert!(core::mem::size_of::<ComponentLoadRequest>() == COMPONENT_LOAD_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<ComponentLifecycle>() == COMPONENT_LIFECYCLE_SIZE);
const _: () = assert!(core::mem::size_of::<ComponentQuery>() == COMPONENT_QUERY_SIZE);
const _: () = assert!(core::mem::size_of::<InterfaceRequest>() == INTERFACE_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<ComponentCallState>() == COMPONENT_CALL_STATE_SIZE);
const _: () = assert!(core::mem::size_of::<ComponentContext>() == COMPONENT_CONTEXT_SIZE);
const _: () =
    assert!(core::mem::size_of::<ComponentCapabilityRecord>() == COMPONENT_CAPABILITY_RECORD_SIZE);
const _: () =
    assert!(core::mem::size_of::<ComponentInterfaceGate>() == COMPONENT_INTERFACE_GATE_SIZE);
const _: () = assert!(core::mem::size_of::<ThreadCreateRequest>() == THREAD_CREATE_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<ThreadResult>() == THREAD_RESULT_SIZE);
const _: () = assert!(core::mem::size_of::<ThreadInfo>() == THREAD_INFO_SIZE);
const _: () = assert!(core::mem::size_of::<MemoryCreateRequest>() == MEMORY_CREATE_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<MemoryMapRequest>() == MEMORY_MAP_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<MemoryInfo>() == MEMORY_INFO_SIZE);
const _: () = assert!(core::mem::size_of::<MemoryStatistics>() == MEMORY_STATISTICS_SIZE);
const _: () = assert!(core::mem::size_of::<MemoryRegion>() == MEMORY_REGION_SIZE);
const _: () = assert!(core::mem::size_of::<PathRef>() == PATH_REF_SIZE);
const _: () = assert!(core::mem::size_of::<DirectoryRequest>() == DIRECTORY_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<DirectoryInfo>() == DIRECTORY_INFO_SIZE);
const _: () = assert!(core::mem::size_of::<FileInfo>() == FILE_INFO_SIZE);
const _: () =
    assert!(core::mem::size_of::<ChannelHandleTransfer>() == CHANNEL_HANDLE_TRANSFER_SIZE);
const _: () = assert!(core::mem::size_of::<ChannelMessage>() == CHANNEL_MESSAGE_SIZE);
const _: () = assert!(core::mem::size_of::<SubmissionDescriptor>() == SUBMISSION_DESCRIPTOR_SIZE);
const _: () = assert!(core::mem::size_of::<CompletionRecord>() == COMPLETION_RECORD_SIZE);
const _: () = assert!(core::mem::size_of::<RingSharedState>() == RING_SHARED_STATE_SIZE);
const _: () = assert!(core::mem::size_of::<RingInfo>() == RING_INFO_SIZE);
const _: () = assert!(core::mem::size_of::<SocketCreateRequest>() == SOCKET_CREATE_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<NetworkAddress>() == NETWORK_ADDRESS_SIZE);
const _: () = assert!(core::mem::size_of::<SocketInfo>() == SOCKET_INFO_SIZE);
const _: () = assert!(core::mem::size_of::<DeviceRequest>() == DEVICE_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<DeviceInfo>() == DEVICE_INFO_SIZE);

pub mod string_ref {
    pub const OFFSET: usize = 0x00;
    pub const LENGTH: usize = 0x04;
}

pub mod start_info {
    pub const MAGIC: usize = 0x00;
    pub const VERSION: usize = 0x04;
    pub const HEADER_SIZE: usize = 0x06;
    pub const TOTAL_SIZE: usize = 0x08;
    pub const FLAGS: usize = 0x0c;
    pub const ABI_EPOCH: usize = 0x10;
    pub const TARGET_ARCH: usize = 0x12;
    pub const RESERVED0: usize = 0x14;
    pub const ENABLED_FEATURES: usize = 0x18;
    pub const IMAGE_BASE: usize = 0x20;
    pub const PAGE_SIZE: usize = 0x28;
    pub const INITIAL_TLS_BASE: usize = 0x30;
    pub const INITIAL_TLS_SIZE: usize = 0x38;
    pub const INITIAL_THREAD_POINTER: usize = 0x40;
    pub const ARGC: usize = 0x48;
    pub const ENVC: usize = 0x4c;
    pub const ARGV_OFFSET: usize = 0x50;
    pub const ENV_OFFSET: usize = 0x54;
    pub const STRING_BYTES_OFFSET: usize = 0x58;
    pub const STRING_BYTES_SIZE: usize = 0x5c;
    pub const INITIAL_HANDLE_COUNT: usize = 0x60;
    pub const INITIAL_HANDLE_RECORD_SIZE: usize = 0x64;
    pub const RESERVED1: usize = 0x66;
    pub const INITIAL_HANDLE_OFFSET: usize = 0x68;
    pub const CALL_SLOT_COUNT: usize = 0x6c;
    pub const RANDOM_SEED: usize = 0x70;
    pub const RUNTIME_FLAGS: usize = 0x90;
    pub const INIT_ARRAY_OFFSET: usize = 0x98;
    pub const INIT_ARRAY_COUNT: usize = 0xa0;
    pub const INIT_ARRAY_ENTRY_SIZE: usize = 0xa4;
    pub const RESERVED2: usize = 0xa6;
    pub const FINI_ARRAY_OFFSET: usize = 0xa8;
    pub const FINI_ARRAY_COUNT: usize = 0xb0;
    pub const FINI_ARRAY_ENTRY_SIZE: usize = 0xb4;
    pub const RESERVED3: usize = 0xb6;
    pub const RESERVED4: usize = 0xb8;
}

pub mod initial_handle {
    pub const REQUIREMENT_ID: usize = 0x00;
    pub const OBJECT_INTERFACE: usize = 0x04;
    pub const FLAGS: usize = 0x06;
    pub const HANDLE: usize = 0x08;
    pub const GRANTED_RIGHTS: usize = 0x10;
    pub const RESERVED: usize = 0x18;
}

pub mod process_string_ref {
    pub const PTR: usize = 0x00;
    pub const LEN: usize = 0x08;
}

pub mod process_array_ref {
    pub const PTR: usize = 0x00;
    pub const COUNT: usize = 0x08;
    pub const RESERVED: usize = 0x0c;
}

pub mod handle_transfer {
    pub const REQUIREMENT_ID: usize = 0x00;
    pub const RESERVED: usize = 0x04;
    pub const SOURCE_HANDLE: usize = 0x08;
    pub const REQUESTED_RIGHTS: usize = 0x10;
    pub const FLAGS: usize = 0x18;
}

pub mod spawn_request {
    pub const IMAGE: usize = 0x00;
    pub const ARGV: usize = 0x08;
    pub const ENV: usize = 0x18;
    pub const TRANSFERS: usize = 0x28;
    pub const RESOURCE_POLICY: usize = 0x38;
}

pub mod process_result {
    pub const STATE: usize = 0x00;
    pub const FLAGS: usize = 0x04;
    pub const EXIT_CODE: usize = 0x08;
    pub const FAULT_KIND: usize = 0x0c;
    pub const DETAIL0: usize = 0x10;
    pub const DETAIL1: usize = 0x18;
}

pub mod image_info {
    pub const ARTIFACT_KIND: usize = 0x00;
    pub const TARGET_ARCH: usize = 0x04;
    pub const ABI_EPOCH: usize = 0x06;
    pub const ENABLED_FEATURES: usize = 0x08;
    pub const FILE_SIZE: usize = 0x10;
    pub const IMAGE_VIRTUAL_SIZE: usize = 0x18;
    pub const COMPONENT_IDENTITY: usize = 0x20;
    pub const ABI_IDENTITY: usize = 0x30;
    pub const BUILD_ID: usize = 0x40;
    pub const CONTENT_HASH: usize = 0x60;
    pub const RESERVED: usize = 0x80;
}

pub mod event_record {
    pub const EVENT_KIND: usize = 0x00;
    pub const STATUS: usize = 0x04;
    pub const SOURCE_HANDLE: usize = 0x08;
    pub const SEQUENCE: usize = 0x10;
    pub const VALUE0: usize = 0x18;
    pub const VALUE1: usize = 0x20;
}

pub mod component_load_request {
    pub const ROOT_IMAGE: usize = 0x00;
    pub const IMAGES: usize = 0x08;
    pub const BINDINGS: usize = 0x18;
    pub const FLAGS: usize = 0x28;
    pub const RESERVED: usize = 0x30;
}

pub mod component_lifecycle {
    pub const ACTION: usize = 0x00;
    pub const STATE: usize = 0x04;
    pub const COMPONENT: usize = 0x08;
    pub const ENTRY: usize = 0x10;
    pub const CONTEXT: usize = 0x18;
    pub const TLS_IDENTITY: usize = 0x20;
    pub const GENERATION: usize = 0x28;
    pub const CALL_STATE: usize = 0x30;
    pub const RESERVED: usize = 0x38;
}

pub mod component_query {
    pub const STATE: usize = 0x00;
    pub const FLAGS: usize = 0x04;
    pub const GENERATION: usize = 0x08;
    pub const COMPONENT_IDENTITY: usize = 0x10;
    pub const ABI_IDENTITY: usize = 0x20;
    pub const ACTIVE_CALLS: usize = 0x30;
    pub const DEPENDENT_COUNT: usize = 0x38;
    pub const RESERVED: usize = 0x3c;
}

pub mod interface_request {
    pub const INTERFACE_IDENTITY: usize = 0x00;
    pub const SIGNATURE_HASH: usize = 0x10;
}

pub mod component_call_state {
    pub const STATE: usize = 0x00;
    pub const FLAGS: usize = 0x04;
    pub const GENERATION: usize = 0x08;
    pub const ACTIVE_CALLS: usize = 0x10;
    pub const DRAIN_WAITER: usize = 0x18;
    pub const RESERVED0: usize = 0x1c;
    pub const RESERVED: usize = 0x20;
}

pub mod component_context {
    pub const IMAGE_BASE: usize = 0x00;
    pub const CALL_STATE: usize = 0x08;
    pub const TLS_BASE: usize = 0x10;
    pub const TLS_IDENTITY: usize = 0x18;
    pub const CALL_SLOT_COUNT: usize = 0x20;
    pub const INTERFACE_COUNT: usize = 0x24;
    pub const CAPABILITY_COUNT: usize = 0x28;
    pub const FLAGS: usize = 0x2c;
    pub const CAPABILITIES: usize = 0x30;
    pub const RESERVED: usize = 0x38;
}

pub mod component_capability_record {
    pub const REQUIREMENT_ID: usize = 0x00;
    pub const RESERVED0: usize = 0x04;
    pub const HANDLE: usize = 0x08;
    pub const GRANTED_RIGHTS: usize = 0x10;
    pub const RESERVED1: usize = 0x18;
}

pub mod component_interface_gate {
    pub const CALL_STATE: usize = 0x00;
    pub const TARGET: usize = 0x08;
    pub const COMPONENT: usize = 0x10;
    pub const GENERATION: usize = 0x18;
}

pub mod thread_create_request {
    pub const ENTRY: usize = 0x00;
    pub const STACK_MEMORY: usize = 0x08;
    pub const STACK_OFFSET: usize = 0x10;
    pub const STACK_SIZE: usize = 0x18;
    pub const TLS_MEMORY: usize = 0x20;
    pub const TLS_OFFSET: usize = 0x28;
    pub const ARGUMENT: usize = 0x30;
    pub const FLAGS: usize = 0x38;
}

pub mod thread_result {
    pub const STATE: usize = 0x00;
    pub const FLAGS: usize = 0x04;
    pub const EXIT_CODE: usize = 0x08;
    pub const FAULT_KIND: usize = 0x0c;
    pub const DETAIL0: usize = 0x10;
    pub const DETAIL1: usize = 0x18;
}

pub mod thread_info {
    pub const STATE: usize = 0x00;
    pub const FLAGS: usize = 0x04;
    pub const IDENTITY: usize = 0x08;
    pub const CPU_TIME_NS: usize = 0x10;
    pub const EXIT_CODE: usize = 0x18;
    pub const FAULT_KIND: usize = 0x1c;
    pub const TLS_BASE: usize = 0x20;
    pub const RESERVED: usize = 0x28;
}

pub mod memory_create_request {
    pub const SIZE: usize = 0x00;
    pub const ALIGNMENT: usize = 0x08;
    pub const FLAGS: usize = 0x10;
    pub const KIND: usize = 0x14;
    pub const SOURCE_HANDLE: usize = 0x18;
    pub const SOURCE_OFFSET: usize = 0x20;
    pub const RESERVED: usize = 0x28;
}

pub mod memory_map_request {
    pub const ADDRESS_SPACE: usize = 0x00;
    pub const OFFSET: usize = 0x08;
    pub const LENGTH: usize = 0x10;
    pub const ALIGNMENT: usize = 0x18;
    pub const ADDRESS_HINT: usize = 0x20;
    pub const PERMISSIONS: usize = 0x28;
    pub const FLAGS: usize = 0x2c;
    pub const RESERVED: usize = 0x30;
}

pub mod memory_info {
    pub const SIZE: usize = 0x00;
    pub const ALIGNMENT: usize = 0x08;
    pub const KIND: usize = 0x10;
    pub const FLAGS: usize = 0x14;
    pub const MAPPING_COUNT: usize = 0x18;
    pub const STATE: usize = 0x1c;
    pub const GENERATION: usize = 0x20;
    pub const SOURCE_SIZE: usize = 0x28;
    pub const RESERVED: usize = 0x30;
}

pub mod memory_statistics {
    pub const MATERIALIZED_PAGES: usize = 0x00;
    pub const RESIDENT_MAPPINGS: usize = 0x08;
    pub const MAPPED_PAGES: usize = 0x10;
    pub const SHARED_RESIDENT_MAPPINGS: usize = 0x18;
    pub const READ_OPERATIONS: usize = 0x20;
    pub const WRITE_OPERATIONS: usize = 0x28;
    pub const BYTES_READ: usize = 0x30;
    pub const BYTES_WRITTEN: usize = 0x38;
    pub const WRITEBACK_OPERATIONS: usize = 0x40;
    pub const RESERVED: usize = 0x48;
}

pub mod memory_region {
    pub const MEMORY: usize = 0x00;
    pub const OFFSET: usize = 0x08;
    pub const LENGTH: usize = 0x10;
    pub const GENERATION: usize = 0x18;
}

pub mod ring_shared_state {
    pub const MAGIC: usize = 0x00;
    pub const VERSION: usize = 0x04;
    pub const FLAGS: usize = 0x06;
    pub const ENTRIES: usize = 0x08;
    pub const MASK: usize = 0x0c;
    pub const SQ_HEAD: usize = 0x10;
    pub const SQ_TAIL: usize = 0x14;
    pub const CQ_HEAD: usize = 0x18;
    pub const CQ_TAIL: usize = 0x1c;
    pub const SQ_OFFSET: usize = 0x20;
    pub const CQ_OFFSET: usize = 0x28;
    pub const GENERATION: usize = 0x30;
    pub const RESERVED: usize = 0x38;
}

pub mod path_ref {
    pub const PTR: usize = 0x00;
    pub const LENGTH: usize = 0x08;
    pub const FLAGS: usize = 0x0c;
}

pub mod directory_request {
    pub const PATH: usize = 0x00;
    pub const KIND: usize = 0x10;
    pub const FLAGS: usize = 0x14;
    pub const REQUESTED_RIGHTS: usize = 0x18;
    pub const RESERVED: usize = 0x20;
}

pub mod directory_info {
    pub const FLAGS: usize = 0x00;
    pub const RESERVED0: usize = 0x04;
    pub const GENERATION: usize = 0x08;
    pub const ENTRY_COUNT: usize = 0x10;
    pub const CHANGE_COUNTER: usize = 0x18;
    pub const RESERVED: usize = 0x20;
}

pub mod file_info {
    pub const KIND: usize = 0x00;
    pub const FLAGS: usize = 0x04;
    pub const SIZE: usize = 0x08;
    pub const GENERATION: usize = 0x10;
    pub const MODIFIED_NS: usize = 0x18;
    pub const GRANTED_RIGHTS: usize = 0x20;
    pub const RESERVED: usize = 0x28;
}

pub mod channel_handle_transfer {
    pub const SOURCE_HANDLE: usize = 0x00;
    pub const REQUESTED_RIGHTS: usize = 0x08;
    pub const FLAGS: usize = 0x10;
    pub const RESERVED: usize = 0x18;
}

pub mod channel_message {
    pub const DATA_PTR: usize = 0x00;
    pub const DATA_SIZE: usize = 0x08;
    pub const DATA_CAPACITY: usize = 0x0c;
    pub const HANDLES_PTR: usize = 0x10;
    pub const HANDLE_COUNT: usize = 0x18;
    pub const HANDLE_CAPACITY: usize = 0x1c;
    pub const FLAGS: usize = 0x20;
    pub const RESERVED: usize = 0x28;
}

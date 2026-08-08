//! 内核生成的 Native ABI 启动区固定线格式。

pub const START_INFO_SIZE: usize = 192;
pub const STRING_REF_SIZE: usize = 8;
pub const INITIAL_HANDLE_SIZE: usize = 32;
pub const PROCESS_STRING_REF_SIZE: usize = 16;
pub const PROCESS_ARRAY_REF_SIZE: usize = 16;
pub const HANDLE_TRANSFER_SIZE: usize = 32;
pub const SPAWN_REQUEST_SIZE: usize = 64;
pub const PROCESS_RESULT_SIZE: usize = 32;
pub const EVENT_RECORD_SIZE: usize = 40;
pub const MAX_EVENT_PORT_CAPACITY: u32 = 4096;
pub const MAX_EVENT_BATCH: u32 = 64;

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

const _: () = assert!(core::mem::size_of::<ProcessStringRef>() == PROCESS_STRING_REF_SIZE);
const _: () = assert!(core::mem::size_of::<ProcessArrayRef>() == PROCESS_ARRAY_REF_SIZE);
const _: () = assert!(core::mem::size_of::<HandleTransfer>() == HANDLE_TRANSFER_SIZE);
const _: () = assert!(core::mem::size_of::<SpawnRequest>() == SPAWN_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<ProcessResult>() == PROCESS_RESULT_SIZE);
const _: () = assert!(core::mem::size_of::<EventRecord>() == EVENT_RECORD_SIZE);

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

pub mod event_record {
    pub const EVENT_KIND: usize = 0x00;
    pub const STATUS: usize = 0x04;
    pub const SOURCE_HANDLE: usize = 0x08;
    pub const SEQUENCE: usize = 0x10;
    pub const VALUE0: usize = 0x18;
    pub const VALUE1: usize = 0x20;
}

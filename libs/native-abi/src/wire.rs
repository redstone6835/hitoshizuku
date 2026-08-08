//! 内核生成的 Native ABI 启动区固定线格式。

pub const START_INFO_SIZE: usize = 192;
pub const STRING_REF_SIZE: usize = 8;
pub const INITIAL_HANDLE_SIZE: usize = 32;

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

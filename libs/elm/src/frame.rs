//! ELM 通用调用帧。
//!
//! 调用帧是能力织网中同步调用的固定布局边界。它只描述调用载荷，
//! 不描述底层文件格式，也不暴露内核指针。

pub const ELM_FRAME_PAYLOAD_LEN: usize = 256;

pub const ELM_CALL_FLAG_NONE: u32 = 0;

pub const ELM_CALL_STATUS_OK: i32 = 0;
pub const ELM_CALL_STATUS_NOT_FOUND: i32 = -2;
pub const ELM_CALL_STATUS_BUSY: i32 = -16;
pub const ELM_CALL_STATUS_INVALID: i32 = -22;
pub const ELM_CALL_STATUS_UNSUPPORTED: i32 = -95;
pub const ELM_CALL_STATUS_PROVIDER_FAULT: i32 = -4098;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmCallFrame {
    pub binding_id: u64,
    pub call_id: u64,
    pub opcode: u32,
    pub flags: u32,
    pub payload_len: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub payload: [u8; ELM_FRAME_PAYLOAD_LEN],
}

impl ElmCallFrame {
    pub const fn empty(binding_id: u64, call_id: u64, opcode: u32) -> Self {
        Self {
            binding_id,
            call_id,
            opcode,
            flags: ELM_CALL_FLAG_NONE,
            payload_len: 0,
            reserved0: 0,
            reserved1: 0,
            payload: [0; ELM_FRAME_PAYLOAD_LEN],
        }
    }

    pub fn new(binding_id: u64, call_id: u64, opcode: u32, payload: &[u8]) -> Self {
        let mut out = Self::empty(binding_id, call_id, opcode);
        let n = payload.len().min(ELM_FRAME_PAYLOAD_LEN);
        out.payload[..n].copy_from_slice(&payload[..n]);
        out.payload_len = n as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmReplyFrame {
    pub binding_id: u64,
    pub call_id: u64,
    pub status: i32,
    pub flags: u32,
    pub payload_len: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub payload: [u8; ELM_FRAME_PAYLOAD_LEN],
}

impl ElmReplyFrame {
    pub const fn empty(binding_id: u64, call_id: u64, status: i32) -> Self {
        Self {
            binding_id,
            call_id,
            status,
            flags: ELM_CALL_FLAG_NONE,
            payload_len: 0,
            reserved0: 0,
            reserved1: 0,
            payload: [0; ELM_FRAME_PAYLOAD_LEN],
        }
    }

    pub fn new(binding_id: u64, call_id: u64, status: i32, payload: &[u8]) -> Self {
        let mut out = Self::empty(binding_id, call_id, status);
        let n = payload.len().min(ELM_FRAME_PAYLOAD_LEN);
        out.payload[..n].copy_from_slice(&payload[..n]);
        out.payload_len = n as u16;
        out
    }
}

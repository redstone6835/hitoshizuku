//! 原生 ELM 开发侧需要的最小管理通道固定布局。
//!
//! 这些类型只用于安全包装内部，布局与运行时管理通道 v1 一致，但不会把完整
//! `mgr` 模型及其动态集合依赖带入外部模块镜像。

use crate::frame::{ELM_FRAME_PAYLOAD_LEN, ElmReplyFrame};

pub(crate) const EXTENSION_DISPATCH_FLAG_ALLOW_EMPTY: u32 = 1 << 1;
pub(crate) const MGR_EXTENSION_POINT_LEN: usize = 32;
pub(crate) const MGR_EXTENSION_CONTRACT_LEN: usize = 64;
pub(crate) const MGR_EXTENSION_PAYLOAD_LEN: usize = ELM_FRAME_PAYLOAD_LEN;
pub(crate) const MGR_EXTENSION_DISPATCH_REQUEST_SIZE: usize = 392;
pub(crate) const MGR_EXTENSION_DISPATCH_RESPONSE_SIZE: usize = 312;
pub(crate) const MGR_RESPONSE_HEADER_SIZE: usize = 16;
pub(crate) const MGR_STATUS_OK: i32 = 0;
pub(crate) const MIXIN_REPLY_CONTINUE: u32 = 0;
pub(crate) const MIXIN_REPLY_STOP: u32 = 1 << 0;
pub(crate) const MIXIN_REPLY_REPLACE: u32 = 1 << 1;
pub(crate) const MIXIN_REPLY_DENY: u32 = 1 << 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModuleExtensionDispatchRequest {
    pub target_cell_id: u64,
    pub extension_cell_id: u64,
    pub opcode: u32,
    pub flags: u32,
    pub point_len: u16,
    pub contract_len: u16,
    pub payload_len: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub point: [u8; MGR_EXTENSION_POINT_LEN],
    pub contract: [u8; MGR_EXTENSION_CONTRACT_LEN],
    pub payload: [u8; MGR_EXTENSION_PAYLOAD_LEN],
    pub reserved2: u32,
}

impl ModuleExtensionDispatchRequest {
    pub fn new(target_cell_id: u64, point: &str, contract: &str) -> Option<Self> {
        if point.is_empty()
            || point.len() > MGR_EXTENSION_POINT_LEN
            || contract.is_empty()
            || contract.len() > MGR_EXTENSION_CONTRACT_LEN
        {
            return None;
        }
        let mut output = Self {
            target_cell_id,
            extension_cell_id: 0,
            opcode: 0,
            flags: EXTENSION_DISPATCH_FLAG_ALLOW_EMPTY,
            point_len: point.len() as u16,
            contract_len: contract.len() as u16,
            payload_len: 0,
            reserved0: 0,
            reserved1: 0,
            point: [0; MGR_EXTENSION_POINT_LEN],
            contract: [0; MGR_EXTENSION_CONTRACT_LEN],
            payload: [0; MGR_EXTENSION_PAYLOAD_LEN],
            reserved2: 0,
        };
        output.point[..point.len()].copy_from_slice(point.as_bytes());
        output.contract[..contract.len()].copy_from_slice(contract.as_bytes());
        Some(output)
    }

    pub fn encode(&self) -> [u8; MGR_EXTENSION_DISPATCH_REQUEST_SIZE] {
        let mut output = [0u8; MGR_EXTENSION_DISPATCH_REQUEST_SIZE];
        output[0..8].copy_from_slice(&self.target_cell_id.to_le_bytes());
        output[8..16].copy_from_slice(&self.extension_cell_id.to_le_bytes());
        output[16..20].copy_from_slice(&self.opcode.to_le_bytes());
        output[20..24].copy_from_slice(&self.flags.to_le_bytes());
        output[24..26].copy_from_slice(&self.point_len.to_le_bytes());
        output[26..28].copy_from_slice(&self.contract_len.to_le_bytes());
        output[28..30].copy_from_slice(&self.payload_len.to_le_bytes());
        output[30..32].copy_from_slice(&self.reserved0.to_le_bytes());
        output[32..36].copy_from_slice(&self.reserved1.to_le_bytes());
        output[36..68].copy_from_slice(&self.point);
        output[68..132].copy_from_slice(&self.contract);
        output[132..388].copy_from_slice(&self.payload);
        output[388..392].copy_from_slice(&self.reserved2.to_le_bytes());
        output
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModuleExtensionDispatchResponse {
    pub status: i32,
    pub matched_extensions: u32,
    pub called_extensions: u32,
    pub mode: u32,
    pub blockers: u64,
    pub reply: ElmReplyFrame,
}

impl ModuleExtensionDispatchResponse {
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != MGR_EXTENSION_DISPATCH_RESPONSE_SIZE {
            return None;
        }
        let mut payload = [0u8; ELM_FRAME_PAYLOAD_LEN];
        payload.copy_from_slice(&bytes[56..312]);
        Some(Self {
            status: read_i32(bytes, 0)?,
            matched_extensions: read_u32(bytes, 4)?,
            called_extensions: read_u32(bytes, 8)?,
            mode: read_u32(bytes, 12)?,
            blockers: read_u64(bytes, 16)?,
            reply: ElmReplyFrame {
                binding_id: read_u64(bytes, 24)?,
                call_id: read_u64(bytes, 32)?,
                status: read_i32(bytes, 40)?,
                flags: read_u32(bytes, 44)?,
                payload_len: read_u16(bytes, 48)?,
                reserved0: read_u16(bytes, 50)?,
                reserved1: read_u32(bytes, 52)?,
                payload,
            },
        })
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModuleMgrResponseHeader {
    pub status: i32,
    pub payload_len: u32,
    pub reserved: u64,
}

impl ModuleMgrResponseHeader {
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != MGR_RESPONSE_HEADER_SIZE {
            return None;
        }
        Some(Self {
            status: read_i32(bytes, 0)?,
            payload_len: read_u32(bytes, 4)?,
            reserved: read_u64(bytes, 8)?,
        })
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_request_encoding_zeroes_abi_tail() {
        let request = ModuleExtensionDispatchRequest::new(7, "test.point", "test.frame@1")
            .expect("构造补缀请求");
        let bytes = request.encode();

        assert_eq!(bytes.len(), MGR_EXTENSION_DISPATCH_REQUEST_SIZE);
        assert_eq!(u64::from_le_bytes(bytes[0..8].try_into().unwrap()), 7);
        assert_eq!(&bytes[36..46], b"test.point");
        assert_eq!(&bytes[388..392], &[0; 4]);
    }

    #[test]
    fn extension_response_decoding_checks_exact_size() {
        let mut bytes = [0u8; MGR_EXTENSION_DISPATCH_RESPONSE_SIZE];
        bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&11u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&12u64.to_le_bytes());
        bytes[48..50].copy_from_slice(&1u16.to_le_bytes());
        bytes[56] = 9;

        let response = ModuleExtensionDispatchResponse::decode(&bytes).unwrap();
        assert_eq!(response.matched_extensions, 2);
        assert_eq!(response.called_extensions, 1);
        assert_eq!(response.reply.binding_id, 11);
        assert_eq!(response.reply.call_id, 12);
        assert_eq!(&response.reply.payload[..1], &[9]);
        assert!(ModuleExtensionDispatchResponse::decode(&bytes[..311]).is_none());
    }
}

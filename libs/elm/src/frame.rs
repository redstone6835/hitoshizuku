//! ELM 通用调用帧。
//!
//! 调用帧是枢纽连接层中同步调用的固定布局边界。它只描述调用载荷，
//! 不描述底层文件格式，也不暴露内核指针。

pub const ELM_FRAME_PAYLOAD_LEN: usize = 256;
pub const ELM_NATIVE_ENTRY_ABI_VERSION: u16 = 1;
pub const ELM_NATIVE_PROVIDER_CALL_ABI_VERSION: u16 = 1;
pub const ELM_NATIVE_MANAGED_CALL_ABI_VERSION: u16 = 1;
pub const ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION: u16 = 1;
pub const ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED: u16 = 1 << 0;
pub const ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE: u16 = 1 << 1;
pub const ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK: u16 =
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED | ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE;

pub const ELM_CALL_FLAG_NONE: u32 = 0;

pub const ELM_ACTION_OPCODE_INVOKE: u32 = 1;
pub const ELM_ACTION_RESULT_HEALTH: u32 = 1;

pub const ELM_CALL_STATUS_OK: i32 = 0;
pub const ELM_CALL_STATUS_NOT_FOUND: i32 = -2;
pub const ELM_CALL_STATUS_BUSY: i32 = -16;
pub const ELM_CALL_STATUS_INVALID: i32 = -22;
pub const ELM_CALL_STATUS_UNSUPPORTED: i32 = -95;
pub const ELM_CALL_STATUS_PROVIDER_FAULT: i32 = -4098;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmActionInvokeRequest {
    pub action_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmActionInvokeRequest {
    pub const fn new(action_id: u64) -> Self {
        Self {
            action_id,
            flags: ELM_CALL_FLAG_NONE,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmActionInvokeReply {
    pub action_id: u64,
    pub menu_item_id: u64,
    pub owner_cell_id: u64,
    pub result_kind: u32,
    pub result_code: i32,
    pub event_sequence: u64,
    pub reserved: u64,
}

impl ElmActionInvokeReply {
    pub const fn health(
        action_id: u64,
        menu_item_id: u64,
        owner_cell_id: u64,
        result_code: i32,
        event_sequence: u64,
    ) -> Self {
        Self {
            action_id,
            menu_item_id,
            owner_cell_id,
            result_kind: ELM_ACTION_RESULT_HEALTH,
            result_code,
            event_sequence,
            reserved: 0,
        }
    }
}

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

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNativeProviderCallV1 {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved0: u32,
    pub cell_id: u64,
    pub port_id: u64,
    pub lease_id: u64,
    pub binding_id: u64,
    pub request: ElmCallFrame,
    pub reply: ElmReplyFrame,
}

/// 受管 import/export 的固定原生调用帧。
///
/// import 槽只保存 `import_handle`；实际目标、代际和权限由运行时调用门解析。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNativeManagedCallV1 {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved0: u32,
    pub import_handle: u64,
    pub caller_cell_id: u64,
    pub caller_generation: u64,
    pub callee_cell_id: u64,
    pub callee_generation: u64,
    pub request: ElmCallFrame,
    pub reply: ElmReplyFrame,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNativeEntryFrameV1 {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved0: u32,
    pub cell_id: u64,
    pub parent_id: u64,
    pub generation: u64,
    pub state: u32,
    pub exit_code: i32,
    pub reserved1: u64,
}

impl ElmNativeEntryFrameV1 {
    pub const fn new(cell_id: u64, parent_id: u64, generation: u64, state: u32) -> Self {
        Self {
            abi_version: ELM_NATIVE_ENTRY_ABI_VERSION,
            flags: 0,
            reserved0: 0,
            cell_id,
            parent_id,
            generation,
            state,
            exit_code: 0,
            reserved1: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNativeProviderSnapshotV1 {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved0: u32,
    pub cell_id: u64,
    pub port_id: u64,
    pub binding_id: u64,
    pub lease_id: u64,
    pub status: i32,
    pub reserved1: u32,
    pub capacity: u32,
    pub payload_len: u32,
    pub record_count: u32,
    pub reserved2: u32,
    pub payload_addr: u64,
}

impl ElmNativeProviderSnapshotV1 {
    pub const fn new(
        cell_id: u64,
        port_id: u64,
        binding_id: u64,
        lease_id: u64,
        payload_addr: u64,
        capacity: u32,
    ) -> Self {
        Self {
            abi_version: ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION,
            flags: 0,
            reserved0: 0,
            cell_id,
            port_id,
            binding_id,
            lease_id,
            status: ELM_CALL_STATUS_PROVIDER_FAULT,
            reserved1: 0,
            capacity,
            payload_len: 0,
            record_count: 0,
            reserved2: 0,
            payload_addr,
        }
    }
}

impl ElmNativeProviderCallV1 {
    pub const fn new(cell_id: u64, port_id: u64, lease_id: u64, request: ElmCallFrame) -> Self {
        Self {
            abi_version: ELM_NATIVE_PROVIDER_CALL_ABI_VERSION,
            flags: 0,
            reserved0: 0,
            cell_id,
            port_id,
            lease_id,
            binding_id: request.binding_id,
            request,
            reply: ElmReplyFrame::empty(
                request.binding_id,
                request.call_id,
                ELM_CALL_STATUS_PROVIDER_FAULT,
            ),
        }
    }
}

impl ElmNativeManagedCallV1 {
    pub const fn new(
        import_handle: u64,
        caller_cell_id: u64,
        caller_generation: u64,
        callee_cell_id: u64,
        callee_generation: u64,
        request: ElmCallFrame,
    ) -> Self {
        Self {
            abi_version: ELM_NATIVE_MANAGED_CALL_ABI_VERSION,
            flags: 0,
            reserved0: 0,
            import_handle,
            caller_cell_id,
            caller_generation,
            callee_cell_id,
            callee_generation,
            request,
            reply: ElmReplyFrame::empty(
                request.binding_id,
                request.call_id,
                ELM_CALL_STATUS_PROVIDER_FAULT,
            ),
        }
    }
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

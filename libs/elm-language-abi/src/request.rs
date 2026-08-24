//! 请求、轮询和生命周期控制的固定布局结构。

use core::mem::size_of;

use crate::backend::{
    LANGUAGE_FRAME_PAYLOAD_LEN, LANGUAGE_MANAGED_FRAME_LEN, LANGUAGE_RUNTIME_ABI_VERSION,
};
use crate::ids::LanguageHandle;
use crate::status::LanguageRuntimeStatus;
use crate::validation::{
    LanguageValidationError, ValidationResult, validate_flags, validate_handle, validate_header,
    validate_identifier, validate_owner, validate_payload_length, validate_reserved,
};

/// 请求结构使用的 ABI 版本。
pub const LANGUAGE_REQUEST_ABI_VERSION: u16 = LANGUAGE_RUNTIME_ABI_VERSION;

/// 请求未设置可选行为。
pub const LANGUAGE_REQUEST_FLAG_NONE: u32 = 0;
/// 后端允许取消尚未完成的请求。
pub const LANGUAGE_REQUEST_FLAG_ALLOW_CANCEL: u32 = 1 << 0;
/// V1 认可的请求 flags 掩码。
pub const LANGUAGE_REQUEST_FLAGS_MASK: u32 = LANGUAGE_REQUEST_FLAG_ALLOW_CANCEL;

/// `language.runtime.*@1` 的操作编号。
pub const LANGUAGE_REQUEST_OPCODE_CATALOG: u32 = 1;
/// 注册后端操作编号。
pub const LANGUAGE_REQUEST_OPCODE_BACKEND_REGISTER: u32 = 2;
/// 注销后端操作编号。
pub const LANGUAGE_REQUEST_OPCODE_BACKEND_UNREGISTER: u32 = 3;
/// 创建后端实例操作编号。
pub const LANGUAGE_REQUEST_OPCODE_INSTANCE_OPEN: u32 = 4;
/// 关闭后端实例操作编号。
pub const LANGUAGE_REQUEST_OPCODE_INSTANCE_CLOSE: u32 = 5;
/// 提交异步请求操作编号。
pub const LANGUAGE_REQUEST_OPCODE_REQUEST_SUBMIT: u32 = 6;
/// 轮询请求操作编号。
pub const LANGUAGE_REQUEST_OPCODE_REQUEST_POLL: u32 = 7;
/// 取消请求操作编号。
pub const LANGUAGE_REQUEST_OPCODE_REQUEST_CANCEL: u32 = 8;
/// 排空 owner 操作编号。
pub const LANGUAGE_REQUEST_OPCODE_DRAIN: u32 = 9;
/// 释放 owner 已读取终态请求的操作编号。
pub const LANGUAGE_REQUEST_OPCODE_REQUEST_RELEASE: u32 = 10;
/// 后端领取下一项工作的操作编号。
pub const LANGUAGE_REQUEST_OPCODE_BACKEND_NEXT: u32 = 11;
/// 后端提交完成帧的操作编号。
pub const LANGUAGE_REQUEST_OPCODE_BACKEND_COMPLETE: u32 = 12;
/// 后端观察下一项取消通知的操作编号。
pub const LANGUAGE_REQUEST_OPCODE_BACKEND_CANCEL_NEXT: u32 = 13;
/// 后端确认一项取消通知的操作编号。
pub const LANGUAGE_REQUEST_OPCODE_BACKEND_CANCEL_ACK: u32 = 14;

/// owner 主动请求取消时使用的默认原因。
pub const LANGUAGE_CANCEL_REASON_REQUESTED: u32 = 1;
/// owner 或运行时进入排空流程时使用的原因。
pub const LANGUAGE_CANCEL_REASON_DRAIN: u32 = 2;
/// 整个 language-runtime 进入 quiesce 时使用的原因。
pub const LANGUAGE_CANCEL_REASON_QUIESCE: u32 = 3;

/// 运行时请求状态机的稳定状态编码。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageRequestState {
    /// 请求已经进入队列，尚未开始执行。
    Queued = 1,
    /// 请求正在由后端执行。
    Running = 2,
    /// 后端完成了请求，`status` 字段携带业务结果。
    Completed = 3,
    /// 后端或运行时发生故障，不能继续执行。
    Failed = 4,
    /// 请求被显式取消。
    Canceled = 5,
    /// 请求超过生命周期或 owner 已被撤销。
    Expired = 6,
}

impl LanguageRequestState {
    /// 从 wire 数值解析状态；未知值必须拒绝。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Queued),
            2 => Some(Self::Running),
            3 => Some(Self::Completed),
            4 => Some(Self::Failed),
            5 => Some(Self::Canceled),
            6 => Some(Self::Expired),
            _ => None,
        }
    }

    /// 返回该状态是否是终态。
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Expired
        )
    }

    /// 判断 V1 状态机是否允许迁移到 `next`。
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Running | Self::Canceled | Self::Expired)
                | (
                    Self::Running,
                    Self::Completed | Self::Failed | Self::Canceled | Self::Expired
                )
        )
    }
}

/// `backend.next@1` 返回给语言后端的固定工作帧。
///
/// 完整帧恰好占用 256 字节，其中 192 字节用于业务载荷。`owner_*` 是原始请求 owner；
/// 后端不得把它当作自身身份或授权凭据。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageBackendWorkV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// 原始请求的可选行为 flags。
    pub flags: u32,
    /// 原始请求 owner cell。
    pub owner_cell_id: u64,
    /// 原始请求 owner generation。
    pub owner_generation: u64,
    /// 目标后端编号。
    pub backend_id: u64,
    /// 目标实例句柄。
    pub instance_handle: LanguageHandle,
    /// 请求关联编号。
    pub request_id: u64,
    /// 后端定义的操作码。
    pub opcode: u32,
    /// `payload` 中有效字节数。
    pub payload_len: u16,
    /// 保留字段，V1 必须为零。
    pub reserved0: u16,
    /// 固定容量的业务载荷。
    pub payload: [u8; LANGUAGE_FRAME_PAYLOAD_LEN],
    /// 保留字段，V1 必须为零。
    pub reserved1: u64,
}

impl LanguageBackendWorkV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 从已经通过运行时鉴权的请求构造后端工作帧。
    pub fn from_request(request: &LanguageRequestV1) -> Result<Self, LanguageValidationError> {
        request.validate()?;
        Ok(Self {
            abi_version: LANGUAGE_REQUEST_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: request.flags,
            owner_cell_id: request.owner_cell_id,
            owner_generation: request.owner_generation,
            backend_id: request.backend_id,
            instance_handle: request.instance_handle,
            request_id: request.request_id,
            opcode: request.opcode,
            payload_len: request.payload_len,
            reserved0: 0,
            payload: request.payload,
            reserved1: 0,
        })
    }

    /// 返回工作帧中的有效业务载荷。
    pub fn payload(&self) -> Result<&[u8], LanguageValidationError> {
        validate_payload_length(self.payload_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        Ok(&self.payload[..self.payload_len as usize])
    }

    /// 验证工作帧的所有固定字段。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_REQUEST_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, LANGUAGE_REQUEST_FLAGS_MASK)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.backend_id)?;
        validate_handle(self.instance_handle)?;
        validate_identifier(self.request_id)?;
        if self.opcode == 0 {
            return Err(LanguageValidationError::Identifier);
        }
        validate_payload_length(self.payload_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        validate_reserved(self.reserved0 as u64)?;
        validate_reserved(self.reserved1)
    }
}

/// `backend.complete@1` 提交给运行时的固定完成帧。
///
/// 完整帧恰好占用 256 字节。`owner_*` 是完成工作的后端 owner，运行时必须将它与受信任
/// managed call 上下文以及已注册后端 owner 同时比较，不能只相信 payload。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageBackendCompleteRequestV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// V1 必须为零。
    pub flags: u32,
    /// 完成工作的后端 owner cell。
    pub owner_cell_id: u64,
    /// 完成工作的后端 owner generation。
    pub owner_generation: u64,
    /// 后端编号。
    pub backend_id: u64,
    /// 目标实例句柄。
    pub instance_handle: LanguageHandle,
    /// 请求关联编号。
    pub request_id: u64,
    /// 后端报告的终态。
    pub state: u32,
    /// 后端业务状态或故障码。
    pub status: i32,
    /// `result` 中有效字节数。
    pub result_len: u16,
    /// 保留字段，V1 必须为零。
    pub reserved0: u16,
    /// 保留字段，V1 必须为零。
    pub reserved1: u32,
    /// 固定容量的业务结果。
    pub result: [u8; LANGUAGE_FRAME_PAYLOAD_LEN],
}

impl LanguageBackendCompleteRequestV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造后端完成请求。
    pub fn new(
        owner_cell_id: u64,
        owner_generation: u64,
        backend_id: u64,
        instance_handle: LanguageHandle,
        request_id: u64,
        state: LanguageRequestState,
        status: LanguageRuntimeStatus,
        result: &[u8],
    ) -> Result<Self, LanguageValidationError> {
        if result.len() > LANGUAGE_FRAME_PAYLOAD_LEN {
            return Err(LanguageValidationError::PayloadLength);
        }
        let mut output = Self {
            abi_version: LANGUAGE_REQUEST_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            owner_cell_id,
            owner_generation,
            backend_id,
            instance_handle,
            request_id,
            state: state as u32,
            status: status.raw(),
            result_len: result.len() as u16,
            reserved0: 0,
            reserved1: 0,
            result: [0; LANGUAGE_FRAME_PAYLOAD_LEN],
        };
        output.result[..result.len()].copy_from_slice(result);
        output.validate()?;
        Ok(output)
    }

    /// 返回完成帧中的有效业务结果。
    pub fn result(&self) -> Result<&[u8], LanguageValidationError> {
        validate_payload_length(self.result_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        Ok(&self.result[..self.result_len as usize])
    }

    /// 验证完成帧的布局、owner、终态和结果边界。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_REQUEST_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, 0)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.backend_id)?;
        validate_handle(self.instance_handle)?;
        validate_identifier(self.request_id)?;
        let state =
            LanguageRequestState::from_raw(self.state).ok_or(LanguageValidationError::State)?;
        if !matches!(
            state,
            LanguageRequestState::Completed
                | LanguageRequestState::Failed
                | LanguageRequestState::Canceled
        ) {
            return Err(LanguageValidationError::State);
        }
        if state == LanguageRequestState::Failed && self.status == LanguageRuntimeStatus::OK.raw() {
            return Err(LanguageValidationError::State);
        }
        if state == LanguageRequestState::Completed
            && self.status != LanguageRuntimeStatus::OK.raw()
        {
            return Err(LanguageValidationError::State);
        }
        if state == LanguageRequestState::Canceled
            && self.status != LanguageRuntimeStatus::CANCELED.raw()
        {
            return Err(LanguageValidationError::State);
        }
        validate_payload_length(self.result_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        validate_reserved(self.reserved0 as u64)?;
        validate_reserved(self.reserved1 as u64)
    }

    /// 在结构校验后确认后端 owner 与受信任 managed call 上下文一致。
    pub fn validate_for_owner(
        &self,
        expected_cell_id: u64,
        expected_generation: u64,
    ) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected_cell_id,
            expected_generation,
        )
    }
}

/// `backend.cancel.next@1` 返回给后端的取消通知。
///
/// 运行中的请求只有在后端读取该通知并通过 [`LanguageBackendCancelAckV1`] 确认停止后才能
/// 进入终态。该通知不携带授权；后端身份仍必须来自受信任 managed call 上下文。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageBackendCancelWorkV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// V1 必须为零。
    pub flags: u32,
    /// 原请求 consumer owner cell。
    pub owner_cell_id: u64,
    /// 原请求 consumer owner generation。
    pub owner_generation: u64,
    /// 目标后端编号。
    pub backend_id: u64,
    /// 目标实例句柄。
    pub instance_handle: LanguageHandle,
    /// 请求关联编号。
    pub request_id: u64,
    /// 取消原因；只用于后端诊断与退出策略，不构成授权。
    pub reason: u32,
    /// 后端确认后应进入的终态，只能是 `Canceled` 或 `Expired`。
    pub terminal_state: u32,
    /// V1 必须为零。
    pub reserved: u64,
}

impl LanguageBackendCancelWorkV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造运行时已经鉴权的取消通知。
    pub const fn new(
        owner_cell_id: u64,
        owner_generation: u64,
        backend_id: u64,
        instance_handle: LanguageHandle,
        request_id: u64,
        reason: u32,
        terminal_state: LanguageRequestState,
    ) -> Self {
        Self {
            abi_version: LANGUAGE_REQUEST_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            owner_cell_id,
            owner_generation,
            backend_id,
            instance_handle,
            request_id,
            reason,
            terminal_state: terminal_state as u32,
            reserved: 0,
        }
    }

    /// 返回确认后应进入的终态。
    pub const fn terminal_state_kind(&self) -> Option<LanguageRequestState> {
        LanguageRequestState::from_raw(self.terminal_state)
    }

    /// 验证 owner、目标、原因和终态。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_REQUEST_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, 0)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.backend_id)?;
        validate_handle(self.instance_handle)?;
        validate_identifier(self.request_id)?;
        if self.reason == 0 {
            return Err(LanguageValidationError::Identifier);
        }
        if !matches!(
            self.terminal_state_kind(),
            Some(LanguageRequestState::Canceled | LanguageRequestState::Expired)
        ) {
            return Err(LanguageValidationError::State);
        }
        validate_reserved(self.reserved)
    }
}

/// `backend.cancel.ack@1` 提交给运行时的停止确认。
///
/// `owner_*` 是后端 provider owner，而非原请求 consumer owner。确认成功意味着该请求已经
/// 不会再执行、访问资源或产生异步回调。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageBackendCancelAckV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// V1 必须为零。
    pub flags: u32,
    /// 后端 provider owner cell。
    pub owner_cell_id: u64,
    /// 后端 provider owner generation。
    pub owner_generation: u64,
    /// 后端编号。
    pub backend_id: u64,
    /// 目标实例句柄。
    pub instance_handle: LanguageHandle,
    /// 请求关联编号。
    pub request_id: u64,
    /// 已确认的终态，必须与取消通知一致。
    pub terminal_state: u32,
    /// 取消结果状态，V1 必须为 `CANCELED`。
    pub status: i32,
    /// V1 必须为零。
    pub reserved: u64,
}

impl LanguageBackendCancelAckV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 从后端 owner 与已观察的通知构造确认帧。
    pub const fn new(
        backend_owner: crate::LanguageOwnerV1,
        notice: LanguageBackendCancelWorkV1,
    ) -> Self {
        Self {
            abi_version: LANGUAGE_REQUEST_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            owner_cell_id: backend_owner.cell_id,
            owner_generation: backend_owner.generation,
            backend_id: notice.backend_id,
            instance_handle: notice.instance_handle,
            request_id: notice.request_id,
            terminal_state: notice.terminal_state,
            status: LanguageRuntimeStatus::CANCELED.raw(),
            reserved: 0,
        }
    }

    /// 返回确认的终态。
    pub const fn terminal_state_kind(&self) -> Option<LanguageRequestState> {
        LanguageRequestState::from_raw(self.terminal_state)
    }

    /// 验证后端 owner、目标、终态与状态码。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_REQUEST_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, 0)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.backend_id)?;
        validate_handle(self.instance_handle)?;
        validate_identifier(self.request_id)?;
        if !matches!(
            self.terminal_state_kind(),
            Some(LanguageRequestState::Canceled | LanguageRequestState::Expired)
        ) || self.status != LanguageRuntimeStatus::CANCELED.raw()
        {
            return Err(LanguageValidationError::State);
        }
        validate_reserved(self.reserved)
    }

    /// 确认后端 owner 与受信任 managed call 上下文一致。
    pub fn validate_for_owner(
        &self,
        expected_cell_id: u64,
        expected_generation: u64,
    ) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected_cell_id,
            expected_generation,
        )
    }
}

/// 提交给 `request.submit@1` 的固定请求。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageRequestV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数，V1 必须等于 `size_of::<Self>()`。
    pub struct_size: u16,
    /// 请求行为 flags。
    pub flags: u32,
    /// 发起请求的 owner cell。
    pub owner_cell_id: u64,
    /// 发起请求的 owner generation。
    pub owner_generation: u64,
    /// 目标后端编号。
    pub backend_id: u64,
    /// 目标后端实例句柄。
    pub instance_handle: LanguageHandle,
    /// 请求关联编号，必须非零。
    pub request_id: u64,
    /// 后端定义的操作码，必须非零。
    pub opcode: u32,
    /// `payload` 中有效字节数。
    pub payload_len: u16,
    /// 保留字段，V1 必须为零。
    pub reserved0: u16,
    /// 固定容量的 inline 载荷。
    pub payload: [u8; LANGUAGE_FRAME_PAYLOAD_LEN],
}

impl LanguageRequestV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造空载荷请求；调用方可随后填写 payload 和长度。
    pub const fn empty(
        owner_cell_id: u64,
        owner_generation: u64,
        backend_id: u64,
        instance_handle: LanguageHandle,
        request_id: u64,
        opcode: u32,
    ) -> Self {
        Self {
            abi_version: LANGUAGE_REQUEST_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: LANGUAGE_REQUEST_FLAG_NONE,
            owner_cell_id,
            owner_generation,
            backend_id,
            instance_handle,
            request_id,
            opcode,
            payload_len: 0,
            reserved0: 0,
            payload: [0; LANGUAGE_FRAME_PAYLOAD_LEN],
        }
    }

    /// 从一段不超过 [`LANGUAGE_FRAME_PAYLOAD_LEN`] 字节的载荷构造请求。
    pub fn new(
        owner_cell_id: u64,
        owner_generation: u64,
        backend_id: u64,
        instance_handle: LanguageHandle,
        request_id: u64,
        opcode: u32,
        payload: &[u8],
    ) -> Result<Self, LanguageValidationError> {
        if payload.len() > LANGUAGE_FRAME_PAYLOAD_LEN {
            return Err(LanguageValidationError::PayloadLength);
        }
        let mut output = Self::empty(
            owner_cell_id,
            owner_generation,
            backend_id,
            instance_handle,
            request_id,
            opcode,
        );
        output.payload[..payload.len()].copy_from_slice(payload);
        output.payload_len = payload.len() as u16;
        output.validate()?;
        Ok(output)
    }

    /// 返回当前结构的有效载荷；调用前必须保证 `validate` 成功。
    pub fn payload(&self) -> Result<&[u8], LanguageValidationError> {
        validate_payload_length(self.payload_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        Ok(&self.payload[..self.payload_len as usize])
    }

    /// 验证版本、owner、目标、flags、长度和保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_REQUEST_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, LANGUAGE_REQUEST_FLAGS_MASK)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.backend_id)?;
        validate_handle(self.instance_handle)?;
        validate_identifier(self.request_id)?;
        if self.opcode == 0 {
            return Err(LanguageValidationError::Identifier);
        }
        validate_payload_length(self.payload_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        if self.reserved0 != 0 {
            return Err(LanguageValidationError::Reserved);
        }
        Ok(())
    }

    /// 在结构校验后确认请求 owner 与受信任 managed call 上下文一致。
    pub fn validate_for_owner(
        &self,
        expected_cell_id: u64,
        expected_generation: u64,
    ) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected_cell_id,
            expected_generation,
        )
    }
}

/// 轮询单个请求的固定输入。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguagePollRequestV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// V1 必须为零。
    pub flags: u32,
    /// 请求 owner cell。
    pub owner_cell_id: u64,
    /// 请求 owner generation。
    pub owner_generation: u64,
    /// 待查询请求编号。
    pub request_id: u64,
    /// 保留字段，V1 必须为零。
    pub reserved: u64,
}

/// `request.release@1` 使用的固定输入。
///
/// 布局复用 [`LanguagePollRequestV1`]；运行时只能在 owner 完全匹配且请求已经进入终态后
/// 删除记录，仍处于 `Queued` 或 `Running` 的请求必须返回 [`LanguageRuntimeStatus::BAD_STATE`]。
pub type LanguageRequestReleaseV1 = LanguagePollRequestV1;

impl LanguagePollRequestV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造轮询请求。
    pub const fn new(owner_cell_id: u64, owner_generation: u64, request_id: u64) -> Self {
        Self {
            abi_version: LANGUAGE_REQUEST_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            owner_cell_id,
            owner_generation,
            request_id,
            reserved: 0,
        }
    }

    /// 验证轮询请求。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_REQUEST_ABI_VERSION,
            Self::SIZE,
        )?;
        if self.flags != 0 {
            return Err(LanguageValidationError::Flags);
        }
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.request_id)?;
        validate_reserved(self.reserved)
    }

    /// 在结构校验后确认请求 owner 与受信任 managed call 上下文一致。
    pub fn validate_for_owner(
        &self,
        expected_cell_id: u64,
        expected_generation: u64,
    ) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected_cell_id,
            expected_generation,
        )
    }
}

/// 取消单个请求的固定输入。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageCancelRequestV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// V1 必须为零。
    pub flags: u32,
    /// 请求 owner cell。
    pub owner_cell_id: u64,
    /// 请求 owner generation。
    pub owner_generation: u64,
    /// 待取消请求编号。
    pub request_id: u64,
    /// 取消原因，由运行时记录但不作为权限依据。
    pub reason: u32,
    /// 保留字段，V1 必须为零。
    pub reserved0: u32,
    /// 保留字段，V1 必须为零。
    pub reserved1: u64,
}

impl LanguageCancelRequestV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造取消请求。
    pub const fn new(
        owner_cell_id: u64,
        owner_generation: u64,
        request_id: u64,
        reason: u32,
    ) -> Self {
        Self {
            abi_version: LANGUAGE_REQUEST_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            owner_cell_id,
            owner_generation,
            request_id,
            reason,
            reserved0: 0,
            reserved1: 0,
        }
    }

    /// 验证取消请求。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_REQUEST_ABI_VERSION,
            Self::SIZE,
        )?;
        if self.flags != 0 {
            return Err(LanguageValidationError::Flags);
        }
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.request_id)?;
        validate_reserved(self.reserved0 as u64)?;
        validate_reserved(self.reserved1)
    }

    /// 在结构校验后确认请求 owner 与受信任 managed call 上下文一致。
    pub fn validate_for_owner(
        &self,
        expected_cell_id: u64,
        expected_generation: u64,
    ) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected_cell_id,
            expected_generation,
        )
    }
}

/// owner 进入排空流程的固定输入。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageDrainRequestV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// V1 必须为零。
    pub flags: u32,
    /// 需要排空的 owner cell。
    pub owner_cell_id: u64,
    /// 需要排空的 owner generation。
    pub owner_generation: u64,
    /// 保留字段，V1 必须为零。
    pub reserved: u64,
}

/// owner 排空操作的固定回复。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageDrainResponseV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// V1 必须为零。
    pub flags: u32,
    /// 被撤销的后端数量。
    pub backend_count: u32,
    /// 被关闭的实例数量。
    pub instance_count: u32,
    /// 被终止的请求数量。
    pub request_count: u32,
    /// V1 必须为零。
    pub reserved0: u32,
    /// 保留字段，V1 必须为零。
    pub reserved1: u64,
}

impl LanguageDrainResponseV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造排空摘要。
    pub const fn new(backend_count: u32, instance_count: u32, request_count: u32) -> Self {
        Self {
            abi_version: LANGUAGE_REQUEST_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            backend_count,
            instance_count,
            request_count,
            reserved0: 0,
            reserved1: 0,
        }
    }

    /// 验证排空摘要的版本、尺寸、flags 和保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_REQUEST_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, 0)?;
        validate_reserved(self.reserved0 as u64)?;
        validate_reserved(self.reserved1)
    }
}

impl LanguageDrainRequestV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造排空请求。
    pub const fn new(owner_cell_id: u64, owner_generation: u64) -> Self {
        Self {
            abi_version: LANGUAGE_REQUEST_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            owner_cell_id,
            owner_generation,
            reserved: 0,
        }
    }

    /// 验证排空请求。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_REQUEST_ABI_VERSION,
            Self::SIZE,
        )?;
        if self.flags != 0 {
            return Err(LanguageValidationError::Flags);
        }
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_reserved(self.reserved)
    }

    /// 在结构校验后确认请求 owner 与受信任 managed call 上下文一致。
    pub fn validate_for_owner(
        &self,
        expected_cell_id: u64,
        expected_generation: u64,
    ) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected_cell_id,
            expected_generation,
        )
    }
}

/// 提交请求后的固定摘要回复。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageRequestSubmitResponseV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// V1 必须为零。
    pub flags: u32,
    /// 提交结果状态。
    pub status: i32,
    /// 保留字段，V1 必须为零。
    pub reserved0: u32,
    /// 被接受的请求编号。
    pub request_id: u64,
    /// 初始状态，成功提交时必须为 `Queued`。
    pub state: u32,
    /// 保留字段，V1 必须为零。
    pub reserved1: u32,
}

impl LanguageRequestSubmitResponseV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造成功入队的摘要回复。
    pub const fn queued(request_id: u64) -> Self {
        Self {
            abi_version: LANGUAGE_REQUEST_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            status: LanguageRuntimeStatus::OK.raw(),
            reserved0: 0,
            request_id,
            state: LanguageRequestState::Queued as u32,
            reserved1: 0,
        }
    }

    /// 验证提交回复。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_REQUEST_ABI_VERSION,
            Self::SIZE,
        )?;
        if self.flags != 0 {
            return Err(LanguageValidationError::Flags);
        }
        validate_identifier(self.request_id)?;
        let state =
            LanguageRequestState::from_raw(self.state).ok_or(LanguageValidationError::State)?;
        if self.status == LanguageRuntimeStatus::OK.raw() && state != LanguageRequestState::Queued {
            return Err(LanguageValidationError::State);
        }
        validate_reserved(self.reserved0 as u64)?;
        validate_reserved(self.reserved1 as u64)
    }
}

/// 轮询请求的完整固定回复。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguagePollResponseV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// 回复 flags；V1 必须为零。
    pub flags: u32,
    /// 请求状态编码。
    pub state: u32,
    /// 运行时或后端状态码。
    pub status: i32,
    /// 请求 owner cell。
    pub owner_cell_id: u64,
    /// 请求 owner generation。
    pub owner_generation: u64,
    /// 目标后端编号。
    pub backend_id: u64,
    /// 目标实例句柄。
    pub instance_handle: LanguageHandle,
    /// 请求关联编号。
    pub request_id: u64,
    /// `result` 中有效字节数。
    pub result_len: u16,
    /// 保留字段，V1 必须为零。
    pub reserved0: u16,
    /// 保留字段，V1 必须为零。
    pub reserved1: u32,
    /// 固定容量的结果载荷。
    pub result: [u8; LANGUAGE_FRAME_PAYLOAD_LEN],
}

impl LanguagePollResponseV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造一个尚未完成且无结果载荷的轮询回复。
    pub const fn pending(
        owner_cell_id: u64,
        owner_generation: u64,
        backend_id: u64,
        instance_handle: LanguageHandle,
        request_id: u64,
    ) -> Self {
        Self {
            abi_version: LANGUAGE_REQUEST_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            state: LanguageRequestState::Queued as u32,
            status: LanguageRuntimeStatus::OK.raw(),
            owner_cell_id,
            owner_generation,
            backend_id,
            instance_handle,
            request_id,
            result_len: 0,
            reserved0: 0,
            reserved1: 0,
            result: [0; LANGUAGE_FRAME_PAYLOAD_LEN],
        }
    }

    /// 返回状态的强类型表示。
    pub const fn state_kind(&self) -> Option<LanguageRequestState> {
        LanguageRequestState::from_raw(self.state)
    }

    /// 返回当前结构的有效结果载荷。
    pub fn result(&self) -> Result<&[u8], LanguageValidationError> {
        validate_payload_length(self.result_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        Ok(&self.result[..self.result_len as usize])
    }

    /// 验证回复版本、状态、owner、目标、结果长度和保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_REQUEST_ABI_VERSION,
            Self::SIZE,
        )?;
        if self.flags != 0 {
            return Err(LanguageValidationError::Flags);
        }
        if self.state_kind().is_none() {
            return Err(LanguageValidationError::State);
        }
        let state = self.state_kind().ok_or(LanguageValidationError::State)?;
        match state {
            LanguageRequestState::Queued | LanguageRequestState::Running
                if self.status != LanguageRuntimeStatus::OK.raw() =>
            {
                return Err(LanguageValidationError::State);
            }
            LanguageRequestState::Completed if self.status != LanguageRuntimeStatus::OK.raw() => {
                return Err(LanguageValidationError::State);
            }
            LanguageRequestState::Failed if self.status == LanguageRuntimeStatus::OK.raw() => {
                return Err(LanguageValidationError::State);
            }
            LanguageRequestState::Canceled | LanguageRequestState::Expired
                if self.status != LanguageRuntimeStatus::CANCELED.raw() =>
            {
                return Err(LanguageValidationError::State);
            }
            _ => {}
        }
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.backend_id)?;
        validate_handle(self.instance_handle)?;
        validate_identifier(self.request_id)?;
        validate_payload_length(self.result_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        validate_reserved(self.reserved0 as u64)?;
        validate_reserved(self.reserved1 as u64)
    }
}

// 这些尺寸是 ABI 的一部分；跨架构构建必须保持一致，并能装入一个 managed call。
const _: () = assert!(size_of::<LanguageBackendWorkV1>() == LANGUAGE_MANAGED_FRAME_LEN);
const _: () = assert!(size_of::<LanguageBackendCompleteRequestV1>() == LANGUAGE_MANAGED_FRAME_LEN);
const _: () = assert!(size_of::<LanguageBackendCancelWorkV1>() == 64);
const _: () = assert!(size_of::<LanguageBackendCancelAckV1>() == 64);
const _: () = assert!(size_of::<LanguageRequestV1>() == 248);
const _: () = assert!(size_of::<LanguagePollRequestV1>() == 40);
const _: () = assert!(size_of::<LanguageCancelRequestV1>() == 48);
const _: () = assert!(size_of::<LanguageDrainRequestV1>() == 32);
const _: () = assert!(size_of::<LanguageRequestSubmitResponseV1>() == 32);
const _: () = assert!(size_of::<LanguagePollResponseV1>() == 256);
const _: () = assert!(size_of::<LanguageDrainResponseV1>() == 32);

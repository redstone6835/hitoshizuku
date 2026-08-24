//! backend 代表原 consumer 调用资源与内核 API 时使用的受限委托协议。

use core::mem::size_of;

use crate::backend::{LANGUAGE_MANAGED_FRAME_LEN, LANGUAGE_RUNTIME_ABI_VERSION_V2};
use crate::ids::{LanguageHandle, LanguageOwnerV1};
use crate::request::LANGUAGE_REQUEST_FLAGS_MASK;
use crate::resource::LANGUAGE_CAPABILITY_FLAGS_MASK;
use crate::validation::{
    LanguageValidationError, ValidationResult, validate_flags, validate_handle, validate_header,
    validate_identifier, validate_owner, validate_payload_length, validate_reserved,
};

/// 委托策略结构的 ABI 版本。
pub const LANGUAGE_DELEGATION_ABI_VERSION: u16 = 1;
/// V2 submit/work 帧保留的最大内联业务载荷。
pub const LANGUAGE_REQUEST_V2_PAYLOAD_LEN: usize = 152;

/// 带委托策略的请求提交合约。
pub const LANGUAGE_RUNTIME_REQUEST_SUBMIT_V2_CONTRACT: &str = "language.runtime.request.submit@2";
/// 返回 opaque 委托 token 的 backend 取件合约。
pub const LANGUAGE_RUNTIME_BACKEND_NEXT_V2_CONTRACT: &str = "language.runtime.backend.next@2";
/// backend 代表 consumer 调用资源控制面的合约。
pub const LANGUAGE_RUNTIME_DELEGATED_RESOURCE_CONTRACT: &str =
    "language.runtime.resource.delegated@1";
/// backend 代表 consumer 调用固定 kernel operation 的合约。
pub const LANGUAGE_RUNTIME_DELEGATED_KERNEL_CALL_CONTRACT: &str =
    "language.runtime.kernel.call.delegated@1";
/// 委托扩展合约的规范顺序。
pub const LANGUAGE_RUNTIME_DELEGATION_CONTRACTS: &[&str] = &[
    LANGUAGE_RUNTIME_REQUEST_SUBMIT_V2_CONTRACT,
    LANGUAGE_RUNTIME_BACKEND_NEXT_V2_CONTRACT,
    LANGUAGE_RUNTIME_DELEGATED_RESOURCE_CONTRACT,
    LANGUAGE_RUNTIME_DELEGATED_KERNEL_CALL_CONTRACT,
];

/// 委托允许调用资源控制面。
pub const LANGUAGE_DELEGATION_FLAG_RESOURCE: u32 = 1 << 0;
/// 委托允许调用一个固定的 kernel operation。
pub const LANGUAGE_DELEGATION_FLAG_KERNEL_CALL: u32 = 1 << 1;
/// 委托策略认可的 flags。
pub const LANGUAGE_DELEGATION_FLAGS_MASK: u32 =
    LANGUAGE_DELEGATION_FLAG_RESOURCE | LANGUAGE_DELEGATION_FLAG_KERNEL_CALL;

/// 可由当前 64 位策略表示的资源操作 bit mask（bit 编号等于 opcode，bit 0 保留）。
pub const LANGUAGE_DELEGATION_RESOURCE_OPCODE_MASK: u64 = u64::MAX - 1;

/// 把资源 opcode 转为委托策略中的单个 bit。
pub const fn language_delegation_resource_opcode_bit(opcode: u32) -> Option<u64> {
    if opcode >= 1 && opcode < 64 {
        Some(1_u64 << opcode)
    } else {
        None
    }
}

/// 单个请求的委托范围。
///
/// `resource_rights` 使用 [`crate::LANGUAGE_CAPABILITY_*`](crate::LANGUAGE_CAPABILITY_FLAGS_MASK)
/// 位；`resource_opcode_mask` 进一步限制允许调用的资源操作。V1 每个 token 最多允许一个
/// kernel operation，后续如需集合应发布新结构而不是把 ID 解释为地址或范围。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageDelegationPolicyV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// 启用的委托控制面。
    pub flags: u32,
    /// 资源委托允许使用的 capability rights 上限。
    pub resource_rights: u64,
    /// 资源委托允许的 opcode bit 集合。
    pub resource_opcode_mask: u64,
    /// 唯一允许的 EKI/kernel operation ID；未启用时为零。
    pub kernel_operation_id: u64,
}

impl LanguageDelegationPolicyV1 {
    /// 当前结构的固定尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造显式委托范围。
    pub const fn new(
        flags: u32,
        resource_rights: u64,
        resource_opcode_mask: u64,
        kernel_operation_id: u64,
    ) -> Self {
        Self {
            abi_version: LANGUAGE_DELEGATION_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags,
            resource_rights,
            resource_opcode_mask,
            kernel_operation_id,
        }
    }

    /// 验证 flags、rights 与 operation 范围。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_DELEGATION_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, LANGUAGE_DELEGATION_FLAGS_MASK)?;
        if self.flags == 0 {
            return Err(LanguageValidationError::Flags);
        }
        if self.flags & LANGUAGE_DELEGATION_FLAG_RESOURCE != 0 {
            if self.resource_rights == 0
                || self.resource_rights & !LANGUAGE_CAPABILITY_FLAGS_MASK != 0
                || self.resource_opcode_mask == 0
                || self.resource_opcode_mask & !LANGUAGE_DELEGATION_RESOURCE_OPCODE_MASK != 0
            {
                return Err(LanguageValidationError::Capability);
            }
        } else if self.resource_rights != 0 || self.resource_opcode_mask != 0 {
            return Err(LanguageValidationError::Flags);
        }
        if self.flags & LANGUAGE_DELEGATION_FLAG_KERNEL_CALL != 0 {
            validate_identifier(self.kernel_operation_id)?;
        } else if self.kernel_operation_id != 0 {
            return Err(LanguageValidationError::Flags);
        }
        Ok(())
    }

    /// 判断策略是否允许资源 opcode 及其最小 capability rights。
    pub const fn allows_resource(self, opcode: u32, required_rights: u64) -> bool {
        let Some(opcode_bit) = language_delegation_resource_opcode_bit(opcode) else {
            return false;
        };
        self.flags & LANGUAGE_DELEGATION_FLAG_RESOURCE != 0
            && self.resource_opcode_mask & opcode_bit != 0
            && required_rights & !LANGUAGE_CAPABILITY_FLAGS_MASK == 0
            && self.resource_rights & required_rights == required_rights
    }

    /// 判断策略是否允许指定 kernel operation。
    pub const fn allows_kernel_operation(self, operation_id: u64) -> bool {
        self.flags & LANGUAGE_DELEGATION_FLAG_KERNEL_CALL != 0
            && operation_id != 0
            && self.kernel_operation_id == operation_id
    }
}

/// 带委托范围的 `request.submit@2` 输入。
///
/// 完整 wire 仍是 256 字节；为容纳策略，V2 内联 payload 上限为 152 字节。需要更大数据时
/// 使用 consumer 拥有的 buffer lease。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageRequestV2 {
    /// 结构遵循的 ABI 版本，固定为 2。
    pub abi_version: u16,
    /// 结构完整字节数。
    pub struct_size: u16,
    /// 请求行为 flags，与 V1 含义相同。
    pub flags: u32,
    /// consumer owner cell。
    pub owner_cell_id: u64,
    /// consumer owner generation。
    pub owner_generation: u64,
    /// 目标 backend ID。
    pub backend_id: u64,
    /// 目标实例句柄。
    pub instance_handle: LanguageHandle,
    /// 请求关联编号。
    pub request_id: u64,
    /// backend 定义的操作码。
    pub opcode: u32,
    /// `payload` 有效字节数。
    pub payload_len: u16,
    /// V2 必须为零。
    pub reserved0: u16,
    /// 当前请求可生成 token 的最大委托范围。
    pub delegation: LanguageDelegationPolicyV1,
    /// 固定业务载荷。
    pub payload: [u8; LANGUAGE_REQUEST_V2_PAYLOAD_LEN],
    /// V2 必须为零。
    pub reserved1: u64,
    /// V2 必须为零。
    pub reserved2: u64,
}

impl LanguageRequestV2 {
    /// 当前结构的固定尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造带显式委托策略的请求。
    pub fn new(
        owner: LanguageOwnerV1,
        backend_id: u64,
        instance_handle: LanguageHandle,
        request_id: u64,
        opcode: u32,
        delegation: LanguageDelegationPolicyV1,
        payload: &[u8],
    ) -> Result<Self, LanguageValidationError> {
        if payload.len() > LANGUAGE_REQUEST_V2_PAYLOAD_LEN {
            return Err(LanguageValidationError::PayloadLength);
        }
        let mut output = Self {
            abi_version: LANGUAGE_RUNTIME_ABI_VERSION_V2,
            struct_size: Self::SIZE as u16,
            flags: 0,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
            backend_id,
            instance_handle,
            request_id,
            opcode,
            payload_len: payload.len() as u16,
            reserved0: 0,
            delegation,
            payload: [0; LANGUAGE_REQUEST_V2_PAYLOAD_LEN],
            reserved1: 0,
            reserved2: 0,
        };
        output.payload[..payload.len()].copy_from_slice(payload);
        output.validate()?;
        Ok(output)
    }

    /// 返回 consumer owner。
    pub const fn owner(&self) -> LanguageOwnerV1 {
        LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 返回有效业务载荷。
    pub fn payload(&self) -> Result<&[u8], LanguageValidationError> {
        validate_payload_length(self.payload_len, LANGUAGE_REQUEST_V2_PAYLOAD_LEN)?;
        Ok(&self.payload[..self.payload_len as usize])
    }

    /// 验证 owner、目标、策略、载荷和保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RUNTIME_ABI_VERSION_V2,
            Self::SIZE,
        )?;
        validate_flags(self.flags, LANGUAGE_REQUEST_FLAGS_MASK)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.backend_id)?;
        validate_handle(self.instance_handle)?;
        validate_identifier(self.request_id)?;
        validate_identifier(self.opcode as u64)?;
        validate_payload_length(self.payload_len, LANGUAGE_REQUEST_V2_PAYLOAD_LEN)?;
        validate_reserved(self.reserved0 as u64)?;
        self.delegation.validate()?;
        validate_reserved(self.reserved1)?;
        validate_reserved(self.reserved2)
    }

    /// 验证 consumer owner 与受信任 managed call 上下文一致。
    pub fn validate_for_owner(&self, expected: LanguageOwnerV1) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected.cell_id,
            expected.generation,
        )
    }
}

/// `backend.next@2` 返回的工作与委托 token。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageBackendWorkV2 {
    /// 结构遵循的 ABI 版本，固定为 2。
    pub abi_version: u16,
    /// 结构完整字节数。
    pub struct_size: u16,
    /// 原请求 flags。
    pub flags: u32,
    /// consumer owner cell。
    pub owner_cell_id: u64,
    /// consumer owner generation。
    pub owner_generation: u64,
    /// backend ID。
    pub backend_id: u64,
    /// 实例句柄。
    pub instance_handle: LanguageHandle,
    /// 请求关联编号。
    pub request_id: u64,
    /// backend 操作码。
    pub opcode: u32,
    /// `payload` 有效字节数。
    pub payload_len: u16,
    /// V2 必须为零。
    pub reserved0: u16,
    /// runtime 分配的 opaque 委托 token。
    pub delegation_handle: LanguageHandle,
    /// token 实际绑定的策略。
    pub delegation: LanguageDelegationPolicyV1,
    /// 固定业务载荷。
    pub payload: [u8; LANGUAGE_REQUEST_V2_PAYLOAD_LEN],
    /// V2 必须为零。
    pub reserved1: u64,
}

impl LanguageBackendWorkV2 {
    /// 当前结构的固定尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 由 runtime 从已验证请求与新 token 构造工作帧。
    pub fn from_request(
        request: &LanguageRequestV2,
        delegation_handle: LanguageHandle,
    ) -> Result<Self, LanguageValidationError> {
        request.validate()?;
        validate_handle(delegation_handle)?;
        let output = Self {
            abi_version: LANGUAGE_RUNTIME_ABI_VERSION_V2,
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
            delegation_handle,
            delegation: request.delegation,
            payload: request.payload,
            reserved1: 0,
        };
        output.validate()?;
        Ok(output)
    }

    /// 返回 consumer owner。
    pub const fn owner(&self) -> LanguageOwnerV1 {
        LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 返回有效业务载荷。
    pub fn payload(&self) -> Result<&[u8], LanguageValidationError> {
        validate_payload_length(self.payload_len, LANGUAGE_REQUEST_V2_PAYLOAD_LEN)?;
        Ok(&self.payload[..self.payload_len as usize])
    }

    /// 验证 token、策略、owner 与业务载荷。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RUNTIME_ABI_VERSION_V2,
            Self::SIZE,
        )?;
        validate_flags(self.flags, LANGUAGE_REQUEST_FLAGS_MASK)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.backend_id)?;
        validate_handle(self.instance_handle)?;
        validate_identifier(self.request_id)?;
        validate_identifier(self.opcode as u64)?;
        validate_payload_length(self.payload_len, LANGUAGE_REQUEST_V2_PAYLOAD_LEN)?;
        validate_reserved(self.reserved0 as u64)?;
        validate_handle(self.delegation_handle)?;
        self.delegation.validate()?;
        validate_reserved(self.reserved1)
    }
}

const _: () = assert!(size_of::<LanguageDelegationPolicyV1>() == 32);
const _: () = assert!(size_of::<LanguageRequestV2>() == LANGUAGE_MANAGED_FRAME_LEN);
const _: () = assert!(size_of::<LanguageBackendWorkV2>() == LANGUAGE_MANAGED_FRAME_LEN);

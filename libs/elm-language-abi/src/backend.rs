//! 后端、实例和运行时目录的固定布局描述符。

use core::mem::size_of;

use crate::ids::{LanguageHandle, LanguageOwnerV1};
use crate::validation::{
    LanguageValidationError, ValidationResult, validate_flags, validate_header,
    validate_identifier, validate_name, validate_owner, validate_reserved,
};

/// language-runtime ABI 的主版本。
pub const LANGUAGE_RUNTIME_ABI_VERSION: u16 = 1;
/// 带 package/artifact 身份的实例协议版本。
pub const LANGUAGE_RUNTIME_ABI_VERSION_V2: u16 = 2;
/// 后端描述符使用的 ABI 版本。
pub const LANGUAGE_BACKEND_ABI_VERSION: u16 = LANGUAGE_RUNTIME_ABI_VERSION;
/// 运行时目录使用的 ABI 版本。
pub const LANGUAGE_CATALOG_ABI_VERSION: u16 = LANGUAGE_RUNTIME_ABI_VERSION;

/// 后端名称的固定字节容量。
pub const LANGUAGE_BACKEND_NAME_LEN: usize = 32;
/// V1 受管调用帧的总载荷字节上限。
pub const LANGUAGE_MANAGED_FRAME_LEN: usize = 256;
/// V1 请求和回复可携带的最大内联业务载荷。
///
/// 该容量使最大的 [`LanguageRequestV1`](crate::LanguageRequestV1) 和
/// [`LanguagePollResponseV1`](crate::LanguagePollResponseV1) 连同协议头都能装入一个
/// 256 字节 ELM managed call。
pub const LANGUAGE_FRAME_PAYLOAD_LEN: usize = 192;

/// `language.runtime.catalog@1` 合约。
pub const LANGUAGE_RUNTIME_CATALOG_CONTRACT: &str = "language.runtime.catalog@1";
/// `language.runtime.backend.register@1` 合约。
pub const LANGUAGE_RUNTIME_BACKEND_REGISTER_CONTRACT: &str = "language.runtime.backend.register@1";
/// `language.runtime.backend.unregister@1` 合约。
pub const LANGUAGE_RUNTIME_BACKEND_UNREGISTER_CONTRACT: &str =
    "language.runtime.backend.unregister@1";
/// `language.runtime.backend.next@1` 合约。
pub const LANGUAGE_RUNTIME_BACKEND_NEXT_CONTRACT: &str = "language.runtime.backend.next@1";
/// `language.runtime.backend.complete@1` 合约。
pub const LANGUAGE_RUNTIME_BACKEND_COMPLETE_CONTRACT: &str = "language.runtime.backend.complete@1";
/// `language.runtime.backend.cancel.next@1` 合约。
pub const LANGUAGE_RUNTIME_BACKEND_CANCEL_NEXT_CONTRACT: &str =
    "language.runtime.backend.cancel.next@1";
/// `language.runtime.backend.cancel.ack@1` 合约。
pub const LANGUAGE_RUNTIME_BACKEND_CANCEL_ACK_CONTRACT: &str =
    "language.runtime.backend.cancel.ack@1";
/// `language.runtime.instance.open@1` 合约。
pub const LANGUAGE_RUNTIME_INSTANCE_OPEN_CONTRACT: &str = "language.runtime.instance.open@1";
/// 带 package/artifact 构建身份的 `language.runtime.instance.open@2` 合约。
pub const LANGUAGE_RUNTIME_INSTANCE_OPEN_V2_CONTRACT: &str = "language.runtime.instance.open@2";
/// `language.runtime.instance.close@1` 合约。
pub const LANGUAGE_RUNTIME_INSTANCE_CLOSE_CONTRACT: &str = "language.runtime.instance.close@1";
/// `language.runtime.request.submit@1` 合约。
pub const LANGUAGE_RUNTIME_REQUEST_SUBMIT_CONTRACT: &str = "language.runtime.request.submit@1";
/// `language.runtime.request.poll@1` 合约。
pub const LANGUAGE_RUNTIME_REQUEST_POLL_CONTRACT: &str = "language.runtime.request.poll@1";
/// `language.runtime.request.cancel@1` 合约。
pub const LANGUAGE_RUNTIME_REQUEST_CANCEL_CONTRACT: &str = "language.runtime.request.cancel@1";
/// `language.runtime.request.release@1` 合约。
pub const LANGUAGE_RUNTIME_REQUEST_RELEASE_CONTRACT: &str = "language.runtime.request.release@1";
/// `language.runtime.drain@1` 合约。
pub const LANGUAGE_RUNTIME_DRAIN_CONTRACT: &str = "language.runtime.drain@1";

/// V1 目录包含的稳定合约数量。
pub const LANGUAGE_RUNTIME_CONTRACT_COUNT: u32 = 12;
/// 所有稳定合约的规范顺序。
pub const LANGUAGE_RUNTIME_CONTRACTS: &[&str] = &[
    LANGUAGE_RUNTIME_CATALOG_CONTRACT,
    LANGUAGE_RUNTIME_BACKEND_REGISTER_CONTRACT,
    LANGUAGE_RUNTIME_BACKEND_UNREGISTER_CONTRACT,
    LANGUAGE_RUNTIME_BACKEND_NEXT_CONTRACT,
    LANGUAGE_RUNTIME_BACKEND_COMPLETE_CONTRACT,
    LANGUAGE_RUNTIME_INSTANCE_OPEN_CONTRACT,
    LANGUAGE_RUNTIME_INSTANCE_CLOSE_CONTRACT,
    LANGUAGE_RUNTIME_REQUEST_SUBMIT_CONTRACT,
    LANGUAGE_RUNTIME_REQUEST_POLL_CONTRACT,
    LANGUAGE_RUNTIME_REQUEST_CANCEL_CONTRACT,
    LANGUAGE_RUNTIME_REQUEST_RELEASE_CONTRACT,
    LANGUAGE_RUNTIME_DRAIN_CONTRACT,
];

/// 不改变 V1 目录计数的生命周期扩展合约。
///
/// 消费者必须显式检查这些合约，不能仅凭 [`LANGUAGE_RUNTIME_CONTRACT_COUNT`] 推断支持。
pub const LANGUAGE_RUNTIME_LIFECYCLE_CONTRACTS: &[&str] = &[
    LANGUAGE_RUNTIME_BACKEND_CANCEL_NEXT_CONTRACT,
    LANGUAGE_RUNTIME_BACKEND_CANCEL_ACK_CONTRACT,
    LANGUAGE_RUNTIME_INSTANCE_OPEN_V2_CONTRACT,
];

/// SHA-256 构建身份的固定字节数。
pub const LANGUAGE_ARTIFACT_DIGEST_LEN: usize = 32;

/// 后端描述符 flags。
pub const LANGUAGE_BACKEND_FLAG_NONE: u32 = 0;
/// 后端接受同步调用。
pub const LANGUAGE_BACKEND_FLAG_SYNC: u32 = 1 << 0;
/// 后端接受异步请求。
pub const LANGUAGE_BACKEND_FLAG_ASYNC: u32 = 1 << 1;
/// 后端可以安全地取消未完成请求。
pub const LANGUAGE_BACKEND_FLAG_CANCEL: u32 = 1 << 2;
/// V1 认可的后端 flags 掩码。
pub const LANGUAGE_BACKEND_FLAGS_MASK: u32 =
    LANGUAGE_BACKEND_FLAG_SYNC | LANGUAGE_BACKEND_FLAG_ASYNC | LANGUAGE_BACKEND_FLAG_CANCEL;

/// 实例描述符 flags。
pub const LANGUAGE_INSTANCE_FLAG_NONE: u32 = 0;
/// 实例当前可以接受请求。
pub const LANGUAGE_INSTANCE_FLAG_ACTIVE: u32 = 1 << 0;
/// 实例已停止接收新请求，正在排空。
pub const LANGUAGE_INSTANCE_FLAG_DRAINING: u32 = 1 << 1;
/// V1 认可的实例 flags 掩码。
pub const LANGUAGE_INSTANCE_FLAGS_MASK: u32 =
    LANGUAGE_INSTANCE_FLAG_ACTIVE | LANGUAGE_INSTANCE_FLAG_DRAINING;

/// 运行时目录 flags。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageRuntimeFlags(pub u32);

impl LanguageRuntimeFlags {
    /// 目录已经注册后端能力。
    pub const CATALOG: Self = Self(1 << 0);
    /// 目录允许后端注册和注销。
    pub const BACKENDS: Self = Self(1 << 1);
    /// 目录允许实例生命周期操作。
    pub const INSTANCES: Self = Self(1 << 2);
    /// 目录允许请求排队和轮询。
    pub const REQUESTS: Self = Self(1 << 3);
    /// 目录正在排空，不再接受新对象。
    pub const DRAINING: Self = Self(1 << 4);

    /// 空 flags。
    pub const NONE: Self = Self(0);

    /// 返回 V1 认可的所有目录 flags。
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// 判断是否包含给定 flags。
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// 判断 flags 是否没有设置未知位。
    pub const fn is_valid(self) -> bool {
        self.0 & !LANGUAGE_RUNTIME_FLAGS_MASK == 0
    }
}

/// V1 认可的运行时目录 flags 掩码。
pub const LANGUAGE_RUNTIME_FLAGS_MASK: u32 = LanguageRuntimeFlags::CATALOG.0
    | LanguageRuntimeFlags::BACKENDS.0
    | LanguageRuntimeFlags::INSTANCES.0
    | LanguageRuntimeFlags::REQUESTS.0
    | LanguageRuntimeFlags::DRAINING.0;

/// 后端能力 flags 的强类型包装。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageBackendFlags(pub u32);

impl LanguageBackendFlags {
    /// 无后端能力。
    pub const NONE: Self = Self(LANGUAGE_BACKEND_FLAG_NONE);
    /// 支持同步调用。
    pub const SYNC: Self = Self(LANGUAGE_BACKEND_FLAG_SYNC);
    /// 支持异步请求。
    pub const ASYNC: Self = Self(LANGUAGE_BACKEND_FLAG_ASYNC);
    /// 支持请求取消。
    pub const CANCEL: Self = Self(LANGUAGE_BACKEND_FLAG_CANCEL);

    /// 返回原始 flags。
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// 判断是否包含给定能力。
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// 实例状态 flags 的强类型包装。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageInstanceFlags(pub u32);

impl LanguageInstanceFlags {
    /// 无状态 flags。
    pub const NONE: Self = Self(LANGUAGE_INSTANCE_FLAG_NONE);
    /// 实例活动中。
    pub const ACTIVE: Self = Self(LANGUAGE_INSTANCE_FLAG_ACTIVE);
    /// 实例排空中。
    pub const DRAINING: Self = Self(LANGUAGE_INSTANCE_FLAG_DRAINING);

    /// 返回原始 flags。
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// 判断是否包含给定状态。
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// 注册一个语言后端时提交的固定描述符。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageBackendDescriptorV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数，V1 必须等于 `size_of::<Self>()`。
    pub struct_size: u16,
    /// 后端生命周期和调用能力 flags。
    pub flags: u32,
    /// 实现该后端的语言编号。
    pub language_id: u64,
    /// 后端编号。
    pub backend_id: u64,
    /// 由后端定义的能力位。
    pub feature_flags: u64,
    /// 该后端允许创建的最大实例数，必须非零。
    pub max_instances: u32,
    /// 单个 owner 允许排队的最大请求数，必须非零。
    pub max_requests: u32,
    /// `name` 中有效字节数。
    pub name_len: u16,
    /// 保留字段，V1 必须为零。
    pub reserved0: u16,
    /// 后端的 ASCII 标识名，尾部必须为零。
    pub name: [u8; LANGUAGE_BACKEND_NAME_LEN],
    /// 尾部保留字段，V1 必须为零。
    pub reserved1: u32,
}

impl LanguageBackendDescriptorV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造零值描述符；调用方应随后填写字段或使用 [`Self::new`]。
    pub const fn empty() -> Self {
        Self {
            abi_version: LANGUAGE_BACKEND_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: LANGUAGE_BACKEND_FLAG_NONE,
            language_id: 0,
            backend_id: 0,
            feature_flags: 0,
            max_instances: 0,
            max_requests: 0,
            name_len: 0,
            reserved0: 0,
            name: [0; LANGUAGE_BACKEND_NAME_LEN],
            reserved1: 0,
        }
    }

    /// 构造一个带 ASCII 名称的后端描述符。
    pub fn new(
        language_id: u64,
        backend_id: u64,
        flags: u32,
        feature_flags: u64,
        max_instances: u32,
        max_requests: u32,
        name: &[u8],
    ) -> Result<Self, LanguageValidationError> {
        if name.is_empty() || name.len() > LANGUAGE_BACKEND_NAME_LEN {
            return Err(LanguageValidationError::Name);
        }
        let mut output = Self::empty();
        output.language_id = language_id;
        output.backend_id = backend_id;
        output.flags = flags;
        output.feature_flags = feature_flags;
        output.max_instances = max_instances;
        output.max_requests = max_requests;
        output.name_len = name.len() as u16;
        output.name[..name.len()].copy_from_slice(name);
        output.validate()?;
        Ok(output)
    }

    /// 验证版本、尺寸、flags、容量、名称和保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_BACKEND_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, LANGUAGE_BACKEND_FLAGS_MASK)?;
        let modes = self.flags & (LANGUAGE_BACKEND_FLAG_SYNC | LANGUAGE_BACKEND_FLAG_ASYNC);
        if modes == 0
            || (self.flags & LANGUAGE_BACKEND_FLAG_CANCEL != 0
                && modes & LANGUAGE_BACKEND_FLAG_ASYNC == 0)
        {
            return Err(LanguageValidationError::Flags);
        }
        validate_identifier(self.language_id)?;
        validate_identifier(self.backend_id)?;
        if self.max_instances == 0 || self.max_requests == 0 {
            return Err(LanguageValidationError::Capacity);
        }
        if self.reserved0 != 0 {
            return Err(LanguageValidationError::Reserved);
        }
        validate_name(&self.name, self.name_len)?;
        validate_reserved(self.reserved1 as u64)
    }
}

/// 已创建的语言后端实例描述符。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageInstanceDescriptorV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数，V1 必须等于 `size_of::<Self>()`。
    pub struct_size: u16,
    /// 实例当前状态 flags。
    pub flags: u32,
    /// 实例所属语言编号。
    pub language_id: u64,
    /// 实例所属后端编号。
    pub backend_id: u64,
    /// 实例编号。
    pub instance_id: u64,
    /// owner cell 编号。
    pub owner_cell_id: u64,
    /// owner generation。
    pub owner_generation: u64,
    /// 实例对应的 opaque 句柄。
    pub handle: LanguageHandle,
    /// 保留字段，V1 必须为零。
    pub reserved: u64,
}

impl LanguageInstanceDescriptorV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造一个新的活动实例描述符。
    pub const fn new(
        language_id: u64,
        backend_id: u64,
        instance_id: u64,
        owner: LanguageOwnerV1,
        handle: LanguageHandle,
    ) -> Self {
        Self {
            abi_version: LANGUAGE_RUNTIME_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: LANGUAGE_INSTANCE_FLAG_ACTIVE,
            language_id,
            backend_id,
            instance_id,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
            handle,
            reserved: 0,
        }
    }

    /// 验证实例身份、owner、句柄、flags 和保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RUNTIME_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, LANGUAGE_INSTANCE_FLAGS_MASK)?;
        if self.flags.count_ones() != 1 {
            return Err(LanguageValidationError::State);
        }
        validate_identifier(self.language_id)?;
        validate_identifier(self.backend_id)?;
        validate_identifier(self.instance_id)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        crate::validation::validate_handle(self.handle)?;
        validate_reserved(self.reserved as u64)
    }

    /// 返回实例的 owner 快照。
    pub const fn owner(&self) -> LanguageOwnerV1 {
        LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 在结构校验后确认实例 owner 与受信任调用上下文一致。
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

/// package、AOT artifact 与接口 schema 的不可变构建身份。
///
/// 该结构只把 loader 已经验证的身份绑定到实例生命周期；它本身不替代包签名、来源验证或
/// trust policy。三个 digest 都是原始 SHA-256 字节，不能传入十六进制文本。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageArtifactIdentityV2 {
    /// 结构遵循的 ABI 版本，固定为 2。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// V2 必须为零。
    pub flags: u32,
    /// package manifest 分配的稳定非零编号。
    pub package_id: u64,
    /// 当前目标 artifact 的稳定非零编号。
    pub artifact_id: u64,
    /// 规范化 package manifest 的 SHA-256。
    pub package_digest: [u8; LANGUAGE_ARTIFACT_DIGEST_LEN],
    /// 实际 AOT/ELM artifact 的 SHA-256。
    pub artifact_digest: [u8; LANGUAGE_ARTIFACT_DIGEST_LEN],
    /// 该 artifact 使用的语言无关接口 schema SHA-256。
    pub interface_digest: [u8; LANGUAGE_ARTIFACT_DIGEST_LEN],
    /// V2 必须为零。
    pub reserved: u64,
}

impl LanguageArtifactIdentityV2 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造一个完整的构建身份。
    pub const fn new(
        package_id: u64,
        artifact_id: u64,
        package_digest: [u8; LANGUAGE_ARTIFACT_DIGEST_LEN],
        artifact_digest: [u8; LANGUAGE_ARTIFACT_DIGEST_LEN],
        interface_digest: [u8; LANGUAGE_ARTIFACT_DIGEST_LEN],
    ) -> Self {
        Self {
            abi_version: LANGUAGE_RUNTIME_ABI_VERSION_V2,
            struct_size: Self::SIZE as u16,
            flags: 0,
            package_id,
            artifact_id,
            package_digest,
            artifact_digest,
            interface_digest,
            reserved: 0,
        }
    }

    /// 验证编号、digest、flags 与保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RUNTIME_ABI_VERSION_V2,
            Self::SIZE,
        )?;
        validate_flags(self.flags, 0)?;
        validate_identifier(self.package_id)?;
        validate_identifier(self.artifact_id)?;
        if self.package_digest.iter().all(|byte| *byte == 0)
            || self.artifact_digest.iter().all(|byte| *byte == 0)
            || self.interface_digest.iter().all(|byte| *byte == 0)
        {
            return Err(LanguageValidationError::Identifier);
        }
        validate_reserved(self.reserved)
    }
}

/// `instance.open@2` 的 owner 绑定输入。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageInstanceOpenRequestV2 {
    /// 结构遵循的 ABI 版本，固定为 2。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// V2 必须为零。
    pub flags: u32,
    /// consumer owner cell。
    pub owner_cell_id: u64,
    /// consumer owner generation。
    pub owner_generation: u64,
    /// 目标语言后端编号。
    pub backend_id: u64,
    /// package、artifact 与接口 schema 构建身份。
    pub artifact: LanguageArtifactIdentityV2,
    /// V2 必须为零。
    pub reserved: u64,
}

impl LanguageInstanceOpenRequestV2 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造带构建身份的实例创建请求。
    pub const fn new(
        owner: LanguageOwnerV1,
        backend_id: u64,
        artifact: LanguageArtifactIdentityV2,
    ) -> Self {
        Self {
            abi_version: LANGUAGE_RUNTIME_ABI_VERSION_V2,
            struct_size: Self::SIZE as u16,
            flags: 0,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
            backend_id,
            artifact,
            reserved: 0,
        }
    }

    /// 验证 owner、后端、构建身份和保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RUNTIME_ABI_VERSION_V2,
            Self::SIZE,
        )?;
        validate_flags(self.flags, 0)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.backend_id)?;
        self.artifact.validate()?;
        validate_reserved(self.reserved)
    }

    /// 返回请求中的 consumer owner。
    pub const fn owner(&self) -> LanguageOwnerV1 {
        LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 验证请求 owner 与受信任 managed call 上下文一致。
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

/// 带 package/artifact 构建身份的 V2 实例描述符。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageInstanceDescriptorV2 {
    /// 结构遵循的 ABI 版本，固定为 2。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// 实例当前状态 flags。
    pub flags: u32,
    /// 实例所属语言编号。
    pub language_id: u64,
    /// 实例所属后端编号。
    pub backend_id: u64,
    /// 实例编号。
    pub instance_id: u64,
    /// consumer owner cell。
    pub owner_cell_id: u64,
    /// consumer owner generation。
    pub owner_generation: u64,
    /// 实例 opaque 句柄。
    pub handle: LanguageHandle,
    /// package、artifact 与接口 schema 构建身份。
    pub artifact: LanguageArtifactIdentityV2,
    /// V2 必须为零。
    pub reserved: u64,
}

impl LanguageInstanceDescriptorV2 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 从已经分配的 V1 实例记录构造 V2 回复。
    pub const fn from_v1(
        instance: LanguageInstanceDescriptorV1,
        artifact: LanguageArtifactIdentityV2,
    ) -> Self {
        Self {
            abi_version: LANGUAGE_RUNTIME_ABI_VERSION_V2,
            struct_size: Self::SIZE as u16,
            flags: instance.flags,
            language_id: instance.language_id,
            backend_id: instance.backend_id,
            instance_id: instance.instance_id,
            owner_cell_id: instance.owner_cell_id,
            owner_generation: instance.owner_generation,
            handle: instance.handle,
            artifact,
            reserved: 0,
        }
    }

    /// 验证实例、owner、句柄、构建身份和保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RUNTIME_ABI_VERSION_V2,
            Self::SIZE,
        )?;
        validate_flags(self.flags, LANGUAGE_INSTANCE_FLAGS_MASK)?;
        if self.flags.count_ones() != 1 {
            return Err(LanguageValidationError::State);
        }
        validate_identifier(self.language_id)?;
        validate_identifier(self.backend_id)?;
        validate_identifier(self.instance_id)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        crate::validation::validate_handle(self.handle)?;
        self.artifact.validate()?;
        validate_reserved(self.reserved)
    }

    /// 返回 consumer owner 快照。
    pub const fn owner(&self) -> LanguageOwnerV1 {
        LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 验证描述符 owner 与受信任调用上下文一致。
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

/// `language.runtime.catalog@1` 返回的运行时能力目录。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageRuntimeCatalogV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数，V1 必须等于 `size_of::<Self>()`。
    pub struct_size: u16,
    /// 当前已启用的运行时能力 flags。
    pub flags: u32,
    /// 支持的最大业务内联载荷，V1 固定为 [`LANGUAGE_FRAME_PAYLOAD_LEN`]。
    pub max_inline_payload: u32,
    /// 可同时注册的后端上限。
    pub max_backends: u32,
    /// 所有后端实例的总上限。
    pub max_instances: u32,
    /// 单个 owner 的请求队列上限。
    pub max_requests_per_owner: u32,
    /// 当前目录列出的稳定合约数量。
    pub contract_count: u32,
    /// 保留字段，V1 必须为零。
    pub reserved: u32,
}

impl LanguageRuntimeCatalogV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造具有全部 V1 稳定合约的目录。
    pub const fn new(max_backends: u32, max_instances: u32, max_requests_per_owner: u32) -> Self {
        Self {
            abi_version: LANGUAGE_CATALOG_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: LANGUAGE_RUNTIME_FLAGS_MASK & !LanguageRuntimeFlags::DRAINING.bits(),
            max_inline_payload: LANGUAGE_FRAME_PAYLOAD_LEN as u32,
            max_backends,
            max_instances,
            max_requests_per_owner,
            contract_count: LANGUAGE_RUNTIME_CONTRACT_COUNT,
            reserved: 0,
        }
    }

    /// 验证目录版本、能力、固定载荷上限、容量和保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_CATALOG_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, LANGUAGE_RUNTIME_FLAGS_MASK)?;
        if self.max_inline_payload != LANGUAGE_FRAME_PAYLOAD_LEN as u32
            || self.max_backends == 0
            || self.max_instances == 0
            || self.max_requests_per_owner == 0
            || self.contract_count != LANGUAGE_RUNTIME_CONTRACT_COUNT
        {
            return Err(LanguageValidationError::Capacity);
        }
        validate_reserved(self.reserved as u64)
    }
}

// 固定布局的尺寸应在编译期失败，而不是等到某个架构第一次装载时才失败。
const _: () = assert!(size_of::<LanguageBackendDescriptorV1>() == 80);
const _: () = assert!(size_of::<LanguageInstanceDescriptorV1>() == 64);
const _: () = assert!(size_of::<LanguageArtifactIdentityV2>() == 128);
const _: () = assert!(size_of::<LanguageInstanceOpenRequestV2>() == 168);
const _: () = assert!(size_of::<LanguageInstanceDescriptorV2>() == 192);
const _: () = assert!(size_of::<LanguageRuntimeCatalogV1>() == 32);

/// 注销后端以及创建实例时使用的 owner 绑定请求。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageBackendRequestV1 {
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
    /// 目标后端编号。
    pub backend_id: u64,
    /// 保留字段，V1 必须为零。
    pub reserved: u64,
}

impl LanguageBackendRequestV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造 owner 绑定的后端请求。
    pub const fn new(owner: LanguageOwnerV1, backend_id: u64) -> Self {
        Self {
            abi_version: LANGUAGE_RUNTIME_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
            backend_id,
            reserved: 0,
        }
    }

    /// 验证后端请求。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RUNTIME_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, 0)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.backend_id)?;
        validate_reserved(self.reserved)
    }

    /// 返回请求中的 owner。
    pub const fn owner(&self) -> LanguageOwnerV1 {
        LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 在结构校验后确认 owner 与受信任 managed call 上下文一致。
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

/// 后端从运行时领取下一项工作的 owner 绑定请求。
///
/// 该类型与 [`LanguageBackendRequestV1`] 具有相同布局，但使用独立类型固定
/// `language.runtime.backend.next@1` 的语义，避免 SDK 把注销或创建实例请求误发到热路径。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageBackendNextRequestV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构的完整字节数。
    pub struct_size: u16,
    /// V1 必须为零。
    pub flags: u32,
    /// 后端 owner cell。
    pub owner_cell_id: u64,
    /// 后端 owner generation。
    pub owner_generation: u64,
    /// 要领取工作的后端编号。
    pub backend_id: u64,
    /// 保留字段，V1 必须为零。
    pub reserved: u64,
}

impl LanguageBackendNextRequestV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造后端工作领取请求。
    pub const fn new(owner: LanguageOwnerV1, backend_id: u64) -> Self {
        Self {
            abi_version: LANGUAGE_RUNTIME_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
            backend_id,
            reserved: 0,
        }
    }

    /// 验证版本、尺寸、owner、后端编号和保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RUNTIME_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, 0)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.backend_id)?;
        validate_reserved(self.reserved)
    }

    /// 返回请求中的 owner。
    pub const fn owner(&self) -> LanguageOwnerV1 {
        LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 在结构校验后确认 owner 与受信任 managed call 上下文一致。
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

/// 关闭实例时使用的 owner 绑定请求。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageInstanceCloseRequestV1 {
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
    /// 目标后端编号。
    pub backend_id: u64,
    /// 目标实例句柄。
    pub instance_handle: LanguageHandle,
    /// 保留字段，V1 必须为零。
    pub reserved: u64,
}

impl LanguageInstanceCloseRequestV1 {
    /// 返回当前结构的 ABI 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造 owner 绑定的实例关闭请求。
    pub const fn new(
        owner: LanguageOwnerV1,
        backend_id: u64,
        instance_handle: LanguageHandle,
    ) -> Self {
        Self {
            abi_version: LANGUAGE_RUNTIME_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
            backend_id,
            instance_handle,
            reserved: 0,
        }
    }

    /// 验证关闭请求。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RUNTIME_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, 0)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.backend_id)?;
        crate::validation::validate_handle(self.instance_handle)?;
        validate_reserved(self.reserved)
    }

    /// 返回请求中的 owner。
    pub const fn owner(&self) -> LanguageOwnerV1 {
        LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 在结构校验后确认 owner 与受信任 managed call 上下文一致。
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

const _: () = assert!(size_of::<LanguageBackendRequestV1>() == 40);
const _: () = assert!(size_of::<LanguageBackendNextRequestV1>() == 40);
const _: () = assert!(size_of::<LanguageInstanceCloseRequestV1>() == 48);

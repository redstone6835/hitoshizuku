//! 语言无关的 capability、资源和受管 buffer ABI。
//!
//! 资源操作通过 `language.runtime.resource@1` contract 传递。调用方只携带 opaque
//! handle；物理地址、内核虚拟地址和 Rust 引用不会跨越 ELM 边界。具体操作参数使用
//! 下列固定 payload 结构编码到 [`LanguageResourceRequestV1::payload`]，大块数据通过
//! buffer handle 和 lease 传递。

use core::mem::size_of;

use crate::backend::{
    LANGUAGE_FRAME_PAYLOAD_LEN, LANGUAGE_MANAGED_FRAME_LEN, LANGUAGE_RUNTIME_ABI_VERSION,
};
use crate::ids::LanguageHandle;
use crate::status::LanguageRuntimeStatus;
use crate::validation::{
    LanguageValidationError, ValidationResult, validate_flags, validate_handle, validate_header,
    validate_identifier, validate_owner, validate_payload_length, validate_range,
    validate_reserved,
};

/// 资源 contract 的 ABI 版本。
pub const LANGUAGE_RESOURCE_ABI_VERSION: u16 = LANGUAGE_RUNTIME_ABI_VERSION;

/// 语言运行时资源 contract。
pub const LANGUAGE_RUNTIME_RESOURCE_CONTRACT: &str = "language.runtime.resource@1";
/// 语言运行时直接内核调用 contract。
pub const LANGUAGE_RUNTIME_KERNEL_CALL_CONTRACT: &str = "language.runtime.kernel.call@1";

/// 资源扩展 contract 的规范顺序。
///
/// 这两个 contract 不加入旧的 `LANGUAGE_RUNTIME_CONTRACTS`/`contract_count`，因此旧目录
/// 消费者仍能按 V1 的 12 个基础 contract 工作；支持资源面的消费者应显式检查本列表。
pub const LANGUAGE_RUNTIME_RESOURCE_CONTRACTS: &[&str] = &[
    LANGUAGE_RUNTIME_RESOURCE_CONTRACT,
    LANGUAGE_RUNTIME_KERNEL_CALL_CONTRACT,
];

/// kernel call 没有可选行为。
pub const LANGUAGE_KERNEL_CALL_FLAG_NONE: u32 = 0;
/// kernel call 允许异步执行。
pub const LANGUAGE_KERNEL_CALL_FLAG_ASYNC: u32 = 1 << 0;
/// V1 认可的 kernel call flags 掩码。
pub const LANGUAGE_KERNEL_CALL_FLAGS_MASK: u32 = LANGUAGE_KERNEL_CALL_FLAG_ASYNC;

/// capability 位掩码：允许查询和发现设备。
pub const LANGUAGE_CAPABILITY_DEVICE_DISCOVERY: u64 = 1 << 0;
/// capability 位掩码：允许建立 MMIO 映射。
pub const LANGUAGE_CAPABILITY_MMIO_MAP: u64 = 1 << 1;
/// capability 位掩码：允许读取 MMIO。
pub const LANGUAGE_CAPABILITY_MMIO_READ: u64 = 1 << 2;
/// capability 位掩码：允许写入 MMIO。
pub const LANGUAGE_CAPABILITY_MMIO_WRITE: u64 = 1 << 3;
/// capability 位掩码：允许分配 DMA buffer。
pub const LANGUAGE_CAPABILITY_DMA_ALLOCATE: u64 = 1 << 4;
/// capability 位掩码：允许执行 DMA cache 同步。
pub const LANGUAGE_CAPABILITY_DMA_SYNC: u64 = 1 << 5;
/// capability 位掩码：允许读取受管 buffer。
pub const LANGUAGE_CAPABILITY_BUFFER_READ: u64 = 1 << 6;
/// capability 位掩码：允许写入受管 buffer。
pub const LANGUAGE_CAPABILITY_BUFFER_WRITE: u64 = 1 << 7;
/// V1 定义的全部 capability 位。
pub const LANGUAGE_CAPABILITY_FLAGS_MASK: u64 = LANGUAGE_CAPABILITY_DEVICE_DISCOVERY
    | LANGUAGE_CAPABILITY_MMIO_MAP
    | LANGUAGE_CAPABILITY_MMIO_READ
    | LANGUAGE_CAPABILITY_MMIO_WRITE
    | LANGUAGE_CAPABILITY_DMA_ALLOCATE
    | LANGUAGE_CAPABILITY_DMA_SYNC
    | LANGUAGE_CAPABILITY_BUFFER_READ
    | LANGUAGE_CAPABILITY_BUFFER_WRITE;

/// 资源请求没有可选行为。
pub const LANGUAGE_RESOURCE_REQUEST_FLAG_NONE: u32 = 0;
/// 请求携带了 capability handle。
pub const LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_CAPABILITY: u32 = 1 << 0;
/// 请求携带了目标资源 handle。
pub const LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_RESOURCE: u32 = 1 << 1;
/// 请求允许在资源层执行异步排队。
pub const LANGUAGE_RESOURCE_REQUEST_FLAG_ASYNC: u32 = 1 << 2;
/// V1 认可的资源请求 flags 掩码。
pub const LANGUAGE_RESOURCE_REQUEST_FLAGS_MASK: u32 = LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_CAPABILITY
    | LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_RESOURCE
    | LANGUAGE_RESOURCE_REQUEST_FLAG_ASYNC;

/// 资源请求的操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_CAPABILITY_ACQUIRE: u32 = 1;
/// 撤销 capability 操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_CAPABILITY_REVOKE: u32 = 2;
/// 建立 MMIO 映射操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_MMIO_MAP: u32 = 3;
/// 解除 MMIO 映射操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_MMIO_UNMAP: u32 = 4;
/// 读取 MMIO 操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_MMIO_READ: u32 = 5;
/// 写入 MMIO 操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_MMIO_WRITE: u32 = 6;
/// 分配 DMA buffer 操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_DMA_ALLOCATE: u32 = 7;
/// 执行 DMA cache 同步操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_DMA_SYNC: u32 = 8;
/// 释放 DMA buffer 操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_DMA_RELEASE: u32 = 9;
/// 创建受管 buffer 操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_BUFFER_CREATE: u32 = 10;
/// 创建 buffer lease 操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_BUFFER_LEASE: u32 = 11;
/// 从 buffer 读取数据操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_BUFFER_READ: u32 = 12;
/// 向 buffer 写入数据操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_BUFFER_WRITE: u32 = 13;
/// 释放 buffer 或 lease 操作编号。
pub const LANGUAGE_RESOURCE_OPCODE_BUFFER_RELEASE: u32 = 14;

/// 资源种类。该枚举值会写入 wire，未知值必须拒绝。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageResourceKind {
    /// capability token。
    Capability = 1,
    /// 受保护的 MMIO window。
    Mmio = 2,
    /// 设备可见 DMA buffer。
    Dma = 3,
    /// 内核拥有的受管 buffer。
    Buffer = 4,
    /// buffer 的受限 lease。
    BufferLease = 5,
}

impl LanguageResourceKind {
    /// 从 wire 数值解析资源种类。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Capability),
            2 => Some(Self::Mmio),
            3 => Some(Self::Dma),
            4 => Some(Self::Buffer),
            5 => Some(Self::BufferLease),
            _ => None,
        }
    }

    /// 返回 wire 数值。
    pub const fn raw(self) -> u32 {
        self as u32
    }
}

/// 通用资源句柄 flags：所有权和访问权限由内核授权。
pub const LANGUAGE_RESOURCE_FLAG_OWNED: u32 = 1 << 0;
/// 资源允许读取。
pub const LANGUAGE_RESOURCE_FLAG_READ: u32 = 1 << 1;
/// 资源允许写入。
pub const LANGUAGE_RESOURCE_FLAG_WRITE: u32 = 1 << 2;
/// 资源属于设备地址空间或设备队列。
pub const LANGUAGE_RESOURCE_FLAG_DEVICE: u32 = 1 << 3;
/// V1 认可的资源句柄 flags 掩码。
pub const LANGUAGE_RESOURCE_FLAGS_MASK: u32 = LANGUAGE_RESOURCE_FLAG_OWNED
    | LANGUAGE_RESOURCE_FLAG_READ
    | LANGUAGE_RESOURCE_FLAG_WRITE
    | LANGUAGE_RESOURCE_FLAG_DEVICE;

/// capability token 及其授予的 rights。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageCapabilityV1 {
    /// capability 的 opaque 句柄。
    pub handle: LanguageHandle,
    /// 该 token 实际授予的 capability 位。
    pub rights: u64,
    /// 所属 ELM cell。
    pub owner_cell_id: u64,
    /// 所属 ELM generation。
    pub owner_generation: u64,
}

impl LanguageCapabilityV1 {
    /// 当前结构的 wire 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造 capability token。
    pub const fn new(handle: LanguageHandle, rights: u64, owner: crate::LanguageOwnerV1) -> Self {
        Self {
            handle,
            rights,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
        }
    }

    /// 返回 token 的 owner。
    pub const fn owner(self) -> crate::LanguageOwnerV1 {
        crate::LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 验证句柄、rights 和 owner。
    pub fn validate(&self) -> ValidationResult {
        validate_handle(self.handle)?;
        if self.rights == 0 || self.rights & !LANGUAGE_CAPABILITY_FLAGS_MASK != 0 {
            return Err(LanguageValidationError::Capability);
        }
        validate_owner(self.owner_cell_id, self.owner_generation)
    }

    /// 在自身校验后确认 token 属于受信任调用 owner。
    pub fn validate_for_owner(&self, expected: crate::LanguageOwnerV1) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected.cell_id,
            expected.generation,
        )
    }

    /// 判断 token 是否包含指定 capability 集合。
    pub const fn grants(self, required: u64) -> bool {
        required != 0
            && required & !LANGUAGE_CAPABILITY_FLAGS_MASK == 0
            && self.rights & required == required
    }
}

/// 带资源种类和 owner 绑定的 opaque 资源句柄。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageResourceHandleV1 {
    /// 资源的 opaque 句柄。
    pub handle: LanguageHandle,
    /// 资源种类编码。
    pub kind: u32,
    /// 资源所有权和访问权限 flags。
    pub flags: u32,
    /// 所属 ELM cell。
    pub owner_cell_id: u64,
    /// 所属 ELM generation。
    pub owner_generation: u64,
}

impl LanguageResourceHandleV1 {
    /// 当前结构的 wire 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 无效资源句柄。
    pub const INVALID: Self = Self {
        handle: LanguageHandle::INVALID,
        kind: 0,
        flags: 0,
        owner_cell_id: 0,
        owner_generation: 0,
    };

    /// 构造资源句柄。
    pub const fn new(
        handle: LanguageHandle,
        kind: LanguageResourceKind,
        flags: u32,
        owner: crate::LanguageOwnerV1,
    ) -> Self {
        Self {
            handle,
            kind: kind.raw(),
            flags,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
        }
    }

    /// 返回资源种类。
    pub const fn kind(&self) -> Option<LanguageResourceKind> {
        LanguageResourceKind::from_raw(self.kind)
    }

    /// 返回资源 owner。
    pub const fn owner(&self) -> crate::LanguageOwnerV1 {
        crate::LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 验证句柄、资源种类、权限 flags 和 owner。
    pub fn validate(&self) -> ValidationResult {
        validate_handle(self.handle)?;
        if self.kind().is_none() {
            return Err(LanguageValidationError::ResourceKind);
        }
        validate_flags(self.flags, LANGUAGE_RESOURCE_FLAGS_MASK)?;
        validate_owner(self.owner_cell_id, self.owner_generation)
    }

    /// 在自身校验后确认资源属于受信任调用 owner。
    pub fn validate_for_owner(&self, expected: crate::LanguageOwnerV1) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected.cell_id,
            expected.generation,
        )
    }
}

/// MMIO 访问权限。
pub const LANGUAGE_MMIO_ACCESS_READ: u32 = 1 << 0;
/// MMIO 写权限。
pub const LANGUAGE_MMIO_ACCESS_WRITE: u32 = 1 << 1;
/// MMIO 访问必须按 volatile 语义执行。
pub const LANGUAGE_MMIO_ACCESS_VOLATILE: u32 = 1 << 2;
/// V1 认可的 MMIO 访问 flags 掩码。
pub const LANGUAGE_MMIO_ACCESS_FLAGS_MASK: u32 =
    LANGUAGE_MMIO_ACCESS_READ | LANGUAGE_MMIO_ACCESS_WRITE | LANGUAGE_MMIO_ACCESS_VOLATILE;

/// MMIO cache 属性。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageMmioCacheMode {
    /// 设备内存，不进行普通 cache。
    Device = 1,
    /// 不缓存的普通映射。
    Uncached = 2,
    /// write-combining 映射。
    WriteCombining = 3,
}

impl LanguageMmioCacheMode {
    /// 从 wire 数值解析 cache 属性。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Device),
            2 => Some(Self::Uncached),
            3 => Some(Self::WriteCombining),
            _ => None,
        }
    }
}

/// `mmio.map` 的固定 payload。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageMmioMapPayloadV1 {
    /// 请求映射的设备物理基址；运行时必须与 capability 授权范围交集校验。
    pub physical_base: u64,
    /// 映射长度，不能为零且不能溢出。
    pub length: u64,
    /// 访问权限和 volatile 语义。
    pub access_flags: u32,
    /// cache 属性编码。
    pub cache_mode: u32,
    /// 保留字段，V1 必须为零。
    pub reserved: u64,
}

impl LanguageMmioMapPayloadV1 {
    /// 当前 payload 的 wire 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 验证地址范围、访问权限、cache 属性和保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_range(self.physical_base, self.length)?;
        validate_flags(self.access_flags, LANGUAGE_MMIO_ACCESS_FLAGS_MASK)?;
        if self.access_flags & (LANGUAGE_MMIO_ACCESS_READ | LANGUAGE_MMIO_ACCESS_WRITE) == 0 {
            return Err(LanguageValidationError::Access);
        }
        if LanguageMmioCacheMode::from_raw(self.cache_mode).is_none() {
            return Err(LanguageValidationError::CacheMode);
        }
        validate_reserved(self.reserved)
    }
}

/// `mmio.read`/`mmio.write` 的固定 payload。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageMmioAccessPayloadV1 {
    /// 相对于映射基址的字节偏移。
    pub offset: u64,
    /// 写入值；读取时由运行时在回复 payload 中返回。
    pub value: u64,
    /// 访问宽度，只允许 1、2、4 或 8 字节。
    pub width: u32,
    /// V1 必须为零，访问权限由 map 时的资源 flags 决定。
    pub flags: u32,
    /// 保留字段，V1 必须为零。
    pub reserved: u64,
}

impl LanguageMmioAccessPayloadV1 {
    /// 当前 payload 的 wire 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 验证访问宽度、flags 和保留字段。
    pub fn validate(&self) -> ValidationResult {
        if !matches!(self.width, 1 | 2 | 4 | 8) {
            return Err(LanguageValidationError::Alignment);
        }
        if self.offset % self.width as u64 != 0 {
            return Err(LanguageValidationError::Alignment);
        }
        validate_flags(self.flags, 0)?;
        validate_reserved(self.reserved)
    }
}

/// DMA buffer 可见方向。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageDmaDirection {
    /// CPU 写入、设备读取。
    ToDevice = 1,
    /// 设备写入、CPU 读取。
    FromDevice = 2,
    /// CPU 和设备双向访问。
    Bidirectional = 3,
}

impl LanguageDmaDirection {
    /// 从 wire 数值解析 DMA 方向。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::ToDevice),
            2 => Some(Self::FromDevice),
            3 => Some(Self::Bidirectional),
            _ => None,
        }
    }
}

/// DMA 分配 flags。
pub const LANGUAGE_DMA_FLAG_COHERENT: u32 = 1 << 0;
/// DMA 分配允许 bounce buffer。
pub const LANGUAGE_DMA_FLAG_ALLOW_BOUNCE: u32 = 1 << 1;
/// V1 认可的 DMA flags 掩码。
pub const LANGUAGE_DMA_FLAGS_MASK: u32 =
    LANGUAGE_DMA_FLAG_COHERENT | LANGUAGE_DMA_FLAG_ALLOW_BOUNCE;

/// `dma.allocate` 的固定 payload。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageDmaAllocatePayloadV1 {
    /// 需要分配的字节数。
    pub length: u64,
    /// 对齐要求，必须是非零二的幂。
    pub alignment: u32,
    /// [`LanguageDmaDirection`] 编码。
    pub direction: u32,
    /// DMA 分配策略 flags。
    pub flags: u32,
    /// 保留字段，V1 必须为零。
    pub reserved: u32,
}

impl LanguageDmaAllocatePayloadV1 {
    /// 当前 payload 的 wire 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 验证长度、对齐、方向、flags 和保留字段。
    pub fn validate(&self) -> ValidationResult {
        if self.length == 0 {
            return Err(LanguageValidationError::Range);
        }
        if self.alignment == 0 || !self.alignment.is_power_of_two() {
            return Err(LanguageValidationError::Alignment);
        }
        if LanguageDmaDirection::from_raw(self.direction).is_none() {
            return Err(LanguageValidationError::Direction);
        }
        validate_flags(self.flags, LANGUAGE_DMA_FLAGS_MASK)?;
        validate_reserved(self.reserved as u64)
    }
}

/// `dma.sync` 的固定 payload。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageDmaSyncPayloadV1 {
    /// buffer 内的起始偏移。
    pub offset: u64,
    /// 需要同步的字节数。
    pub length: u64,
    /// [`LanguageDmaDirection`] 编码。
    pub direction: u32,
    /// 保留字段，V1 必须为零。
    pub reserved: u32,
}

impl LanguageDmaSyncPayloadV1 {
    /// 当前 payload 的 wire 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 验证方向、非溢出范围和保留字段。buffer 总长度由运行时资源表校验。
    pub fn validate(&self) -> ValidationResult {
        if self.length == 0 {
            return Err(LanguageValidationError::Range);
        }
        if self.offset.checked_add(self.length).is_none() {
            return Err(LanguageValidationError::Range);
        }
        if LanguageDmaDirection::from_raw(self.direction).is_none() {
            return Err(LanguageValidationError::Direction);
        }
        validate_reserved(self.reserved as u64)
    }
}

/// buffer lease 的访问权限。
pub const LANGUAGE_BUFFER_LEASE_READ: u32 = 1 << 0;
/// buffer lease 的写权限。
pub const LANGUAGE_BUFFER_LEASE_WRITE: u32 = 1 << 1;
/// V1 认可的 buffer lease flags 掩码。
pub const LANGUAGE_BUFFER_LEASE_FLAGS_MASK: u32 =
    LANGUAGE_BUFFER_LEASE_READ | LANGUAGE_BUFFER_LEASE_WRITE;

/// `buffer.lease` 的固定 payload。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageBufferLeasePayloadV1 {
    /// 被租借的 buffer opaque handle。
    pub buffer_handle: LanguageHandle,
    /// buffer 内起始偏移。
    pub offset: u64,
    /// lease 长度。
    pub length: u64,
    /// lease 访问权限。
    pub access_flags: u32,
    /// 保留字段，V1 必须为零。
    pub reserved: u32,
}

impl LanguageBufferLeasePayloadV1 {
    /// 当前 payload 的 wire 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 验证 buffer handle、范围、权限和保留字段。
    pub fn validate(&self) -> ValidationResult {
        validate_handle(self.buffer_handle)?;
        validate_range(self.offset, self.length)?;
        validate_flags(self.access_flags, LANGUAGE_BUFFER_LEASE_FLAGS_MASK)?;
        if self.access_flags == 0 {
            return Err(LanguageValidationError::Access);
        }
        validate_reserved(self.reserved as u64)
    }
}

/// `buffer.read`/`buffer.write` 的固定 payload。
///
/// `data_len` 不能超过 `data` 容量；较大的传输必须拆分成多个请求。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageBufferIoPayloadV1 {
    /// buffer 或 lease 内的起始偏移。
    pub offset: u64,
    /// `data` 中有效字节数。
    pub data_len: u16,
    /// V1 必须为零。
    pub reserved0: u16,
    /// 保留字段，V1 必须为零。
    pub reserved1: u32,
    /// 内联数据；总 payload 固定为 192 字节。
    pub data: [u8; 176],
}

impl LanguageBufferIoPayloadV1 {
    /// 内联数据容量。
    pub const DATA_CAPACITY: usize = 176;
    /// 当前 payload 的 wire 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造带内联数据的 buffer I/O payload。
    pub fn new(offset: u64, data: &[u8]) -> Result<Self, LanguageValidationError> {
        if data.len() > Self::DATA_CAPACITY {
            return Err(LanguageValidationError::PayloadLength);
        }
        let mut output = Self {
            offset,
            data_len: data.len() as u16,
            reserved0: 0,
            reserved1: 0,
            data: [0; Self::DATA_CAPACITY],
        };
        output.data[..data.len()].copy_from_slice(data);
        output.validate()?;
        Ok(output)
    }

    /// 返回内联数据。
    pub fn data(&self) -> Result<&[u8], LanguageValidationError> {
        validate_payload_length(self.data_len, Self::DATA_CAPACITY)?;
        Ok(&self.data[..self.data_len as usize])
    }

    /// 验证长度、保留字段和 inline payload。
    pub fn validate(&self) -> ValidationResult {
        validate_payload_length(self.data_len, Self::DATA_CAPACITY)?;
        if self.offset.checked_add(self.data_len as u64).is_none() {
            return Err(LanguageValidationError::Range);
        }
        validate_reserved(self.reserved0 as u64)?;
        validate_reserved(self.reserved1 as u64)
    }
}

/// 统一资源请求帧。完整 wire 尺寸固定为 256 字节。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageResourceRequestV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构完整字节数。
    pub struct_size: u16,
    /// 请求行为 flags。
    pub flags: u32,
    /// 发起请求的 owner cell。
    pub owner_cell_id: u64,
    /// 发起请求的 owner generation。
    pub owner_generation: u64,
    /// capability token；未使用时必须为无效句柄。
    pub capability_handle: LanguageHandle,
    /// 目标资源；创建类操作未使用时必须为无效句柄。
    pub resource_handle: LanguageHandle,
    /// 请求关联编号。
    pub request_id: u64,
    /// [`LANGUAGE_RESOURCE_OPCODE_CAPABILITY_ACQUIRE`] 等操作编号。
    pub opcode: u32,
    /// `payload` 中的有效字节数。
    pub payload_len: u16,
    /// 保留字段，V1 必须为零。
    pub reserved0: u16,
    /// 具体操作 payload。
    pub payload: [u8; LANGUAGE_FRAME_PAYLOAD_LEN],
    /// 尾部保留字段，V1 必须为零。
    pub reserved1: u64,
}

impl LanguageResourceRequestV1 {
    /// 当前结构的 wire 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造空 payload 资源请求。
    pub const fn empty(
        owner: crate::LanguageOwnerV1,
        capability_handle: LanguageHandle,
        resource_handle: LanguageHandle,
        request_id: u64,
        opcode: u32,
    ) -> Self {
        let mut flags = LANGUAGE_RESOURCE_REQUEST_FLAG_NONE;
        if capability_handle.is_valid() {
            flags |= LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_CAPABILITY;
        }
        if resource_handle.is_valid() {
            flags |= LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_RESOURCE;
        }
        Self {
            abi_version: LANGUAGE_RESOURCE_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
            capability_handle,
            resource_handle,
            request_id,
            opcode,
            payload_len: 0,
            reserved0: 0,
            payload: [0; LANGUAGE_FRAME_PAYLOAD_LEN],
            reserved1: 0,
        }
    }

    /// 从具体 payload 构造资源请求。
    pub fn new(
        owner: crate::LanguageOwnerV1,
        capability_handle: LanguageHandle,
        resource_handle: LanguageHandle,
        request_id: u64,
        opcode: u32,
        payload: &[u8],
    ) -> Result<Self, LanguageValidationError> {
        if payload.len() > LANGUAGE_FRAME_PAYLOAD_LEN {
            return Err(LanguageValidationError::PayloadLength);
        }
        let mut output = Self::empty(
            owner,
            capability_handle,
            resource_handle,
            request_id,
            opcode,
        );
        output.payload[..payload.len()].copy_from_slice(payload);
        output.payload_len = payload.len() as u16;
        output.validate()?;
        Ok(output)
    }

    /// 返回有效 payload。
    pub fn payload(&self) -> Result<&[u8], LanguageValidationError> {
        validate_payload_length(self.payload_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        Ok(&self.payload[..self.payload_len as usize])
    }

    /// 返回请求 owner。
    pub const fn owner(&self) -> crate::LanguageOwnerV1 {
        crate::LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 验证资源请求固定字段和可选句柄标志。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RESOURCE_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, LANGUAGE_RESOURCE_REQUEST_FLAGS_MASK)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        if self.flags & LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_CAPABILITY != 0 {
            validate_handle(self.capability_handle)?;
        } else if self.capability_handle.is_valid() {
            return Err(LanguageValidationError::Flags);
        }
        if self.flags & LANGUAGE_RESOURCE_REQUEST_FLAG_HAS_RESOURCE != 0 {
            validate_handle(self.resource_handle)?;
        } else if self.resource_handle.is_valid() {
            return Err(LanguageValidationError::Flags);
        }
        validate_identifier(self.request_id)?;
        validate_identifier(self.opcode as u64)?;
        validate_payload_length(self.payload_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        validate_reserved(self.reserved0 as u64)?;
        validate_reserved(self.reserved1)
    }

    /// 在结构校验后确认请求 owner 与受信任调用上下文一致。
    pub fn validate_for_owner(&self, expected: crate::LanguageOwnerV1) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected.cell_id,
            expected.generation,
        )
    }
}

/// 统一资源回复帧。完整 wire 尺寸固定为 256 字节。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageResourceResponseV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构完整字节数。
    pub struct_size: u16,
    /// 回复 flags；V1 必须为零。
    pub flags: u32,
    /// 运行时或内核操作状态。
    pub status: i32,
    /// 保留字段，V1 必须为零。
    pub reserved0: u32,
    /// 请求 owner cell。
    pub owner_cell_id: u64,
    /// 请求 owner generation。
    pub owner_generation: u64,
    /// 对应请求编号。
    pub request_id: u64,
    /// 返回或继续使用的资源句柄；无资源时为无效值。
    pub resource_handle: LanguageHandle,
    /// 返回资源种类；无资源时为零。
    pub resource_kind: u32,
    /// `payload` 中的有效字节数。
    pub payload_len: u16,
    /// 保留字段，V1 必须为零。
    pub reserved1: u16,
    /// 具体操作结果 payload。
    pub payload: [u8; LANGUAGE_FRAME_PAYLOAD_LEN],
    /// 尾部保留字段，V1 必须为零。
    pub reserved2: u64,
}

impl LanguageResourceResponseV1 {
    /// 当前结构的 wire 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造没有返回资源的回复。
    pub const fn empty(
        owner: crate::LanguageOwnerV1,
        request_id: u64,
        status: LanguageRuntimeStatus,
    ) -> Self {
        Self {
            abi_version: LANGUAGE_RESOURCE_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            status: status.raw(),
            reserved0: 0,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
            request_id,
            resource_handle: LanguageHandle::INVALID,
            resource_kind: 0,
            payload_len: 0,
            reserved1: 0,
            payload: [0; LANGUAGE_FRAME_PAYLOAD_LEN],
            reserved2: 0,
        }
    }

    /// 从资源 payload 构造回复。
    pub fn with_resource(
        owner: crate::LanguageOwnerV1,
        request_id: u64,
        status: LanguageRuntimeStatus,
        resource: LanguageResourceHandleV1,
        payload: &[u8],
    ) -> Result<Self, LanguageValidationError> {
        if payload.len() > LANGUAGE_FRAME_PAYLOAD_LEN {
            return Err(LanguageValidationError::PayloadLength);
        }
        resource.validate_for_owner(owner)?;
        let mut output = Self::empty(owner, request_id, status);
        output.resource_handle = resource.handle;
        output.resource_kind = resource.kind;
        output.payload[..payload.len()].copy_from_slice(payload);
        output.payload_len = payload.len() as u16;
        output.validate()?;
        Ok(output)
    }

    /// 返回回复 owner。
    pub const fn owner(&self) -> crate::LanguageOwnerV1 {
        crate::LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 返回有效 payload。
    pub fn payload(&self) -> Result<&[u8], LanguageValidationError> {
        validate_payload_length(self.payload_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        Ok(&self.payload[..self.payload_len as usize])
    }

    /// 验证资源回复固定字段、返回资源和 payload。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RESOURCE_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, 0)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.request_id)?;
        if self.resource_handle.is_valid() {
            if LanguageResourceKind::from_raw(self.resource_kind).is_none() {
                return Err(LanguageValidationError::ResourceKind);
            }
        } else if self.resource_kind != 0 {
            return Err(LanguageValidationError::ResourceKind);
        }
        validate_payload_length(self.payload_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        validate_reserved(self.reserved0 as u64)?;
        validate_reserved(self.reserved1 as u64)?;
        validate_reserved(self.reserved2)
    }

    /// 在结构校验后确认回复 owner 与受信任调用上下文一致。
    pub fn validate_for_owner(&self, expected: crate::LanguageOwnerV1) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected.cell_id,
            expected.generation,
        )
    }
}

// 资源帧必须能够直接放入一个 ELM managed call。
const _: () = assert!(size_of::<LanguageResourceRequestV1>() == LANGUAGE_MANAGED_FRAME_LEN);
const _: () = assert!(size_of::<LanguageResourceResponseV1>() == LANGUAGE_MANAGED_FRAME_LEN);
const _: () = assert!(size_of::<LanguageCapabilityV1>() == 32);
const _: () = assert!(size_of::<LanguageResourceHandleV1>() == 32);
const _: () = assert!(size_of::<LanguageMmioMapPayloadV1>() <= LANGUAGE_FRAME_PAYLOAD_LEN);
const _: () = assert!(size_of::<LanguageMmioAccessPayloadV1>() <= LANGUAGE_FRAME_PAYLOAD_LEN);
const _: () = assert!(size_of::<LanguageDmaAllocatePayloadV1>() <= LANGUAGE_FRAME_PAYLOAD_LEN);
const _: () = assert!(size_of::<LanguageDmaSyncPayloadV1>() <= LANGUAGE_FRAME_PAYLOAD_LEN);
const _: () = assert!(size_of::<LanguageBufferLeasePayloadV1>() <= LANGUAGE_FRAME_PAYLOAD_LEN);
const _: () = assert!(size_of::<LanguageBufferIoPayloadV1>() == LANGUAGE_FRAME_PAYLOAD_LEN);

/// `kernel.call@1` 的语言无关请求帧。
///
/// `operation_id` 由 EKI/schema 生成器分配，运行时只允许调用当前 ELM manifest 已声明且
/// 已通过 capability 校验的操作。输入数据仍然限制为一个 managed call 的 192 字节内联
/// payload，较大的输入必须通过资源 buffer handle 传递。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageKernelCallRequestV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构完整字节数。
    pub struct_size: u16,
    /// 调用行为 flags。
    pub flags: u32,
    /// 发起调用的 owner cell。
    pub owner_cell_id: u64,
    /// 发起调用的 owner generation。
    pub owner_generation: u64,
    /// 授权该调用的 capability token。
    pub capability_handle: LanguageHandle,
    /// EKI/schema 分配的稳定操作编号。
    pub operation_id: u64,
    /// 调用关联编号。
    pub call_id: u64,
    /// `input` 中有效字节数。
    pub input_len: u16,
    /// 保留字段，V1 必须为零。
    pub reserved0: u16,
    /// 固定容量的输入 payload。
    pub input: [u8; LANGUAGE_FRAME_PAYLOAD_LEN],
    /// 尾部保留字段，V1 必须为零。
    pub reserved1: u64,
}

impl LanguageKernelCallRequestV1 {
    /// 当前结构的 wire 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 构造同步 kernel call。
    pub fn new(
        owner: crate::LanguageOwnerV1,
        capability_handle: LanguageHandle,
        operation_id: u64,
        call_id: u64,
        input: &[u8],
    ) -> Result<Self, LanguageValidationError> {
        if input.len() > LANGUAGE_FRAME_PAYLOAD_LEN {
            return Err(LanguageValidationError::PayloadLength);
        }
        let mut output = Self {
            abi_version: LANGUAGE_RESOURCE_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: LANGUAGE_KERNEL_CALL_FLAG_NONE,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
            capability_handle,
            operation_id,
            call_id,
            input_len: input.len() as u16,
            reserved0: 0,
            input: [0; LANGUAGE_FRAME_PAYLOAD_LEN],
            reserved1: 0,
        };
        output.input[..input.len()].copy_from_slice(input);
        output.validate()?;
        Ok(output)
    }

    /// 返回调用 owner。
    pub const fn owner(&self) -> crate::LanguageOwnerV1 {
        crate::LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 返回有效输入 payload。
    pub fn input(&self) -> Result<&[u8], LanguageValidationError> {
        validate_payload_length(self.input_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        Ok(&self.input[..self.input_len as usize])
    }

    /// 验证调用帧的 owner、capability、操作编号和 payload。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RESOURCE_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, LANGUAGE_KERNEL_CALL_FLAGS_MASK)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_handle(self.capability_handle)?;
        validate_identifier(self.operation_id)?;
        validate_identifier(self.call_id)?;
        validate_payload_length(self.input_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        validate_reserved(self.reserved0 as u64)?;
        validate_reserved(self.reserved1)
    }

    /// 在结构校验后确认调用 owner 与受信任上下文一致。
    pub fn validate_for_owner(&self, expected: crate::LanguageOwnerV1) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected.cell_id,
            expected.generation,
        )
    }
}

/// `kernel.call@1` 的语言无关回复帧。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageKernelCallResponseV1 {
    /// 结构遵循的 ABI 版本。
    pub abi_version: u16,
    /// 结构完整字节数。
    pub struct_size: u16,
    /// 回复 flags；V1 必须为零。
    pub flags: u32,
    /// 内核操作状态。
    pub status: i32,
    /// 保留字段，V1 必须为零。
    pub reserved0: u32,
    /// 调用 owner cell。
    pub owner_cell_id: u64,
    /// 调用 owner generation。
    pub owner_generation: u64,
    /// EKI/schema 操作编号。
    pub operation_id: u64,
    /// 对应请求关联编号。
    pub call_id: u64,
    /// `output` 中有效字节数。
    pub output_len: u16,
    /// 保留字段，V1 必须为零。
    pub reserved1: u16,
    /// 固定容量的输出 payload。
    pub output: [u8; LANGUAGE_FRAME_PAYLOAD_LEN],
    /// 尾部保留字段，V1 必须为零。
    pub reserved2: u64,
}

impl LanguageKernelCallResponseV1 {
    /// 当前结构的 wire 尺寸。
    pub const SIZE: usize = size_of::<Self>();

    /// 从调用身份和输出 payload 构造回复。
    pub fn new(
        owner: crate::LanguageOwnerV1,
        operation_id: u64,
        call_id: u64,
        status: LanguageRuntimeStatus,
        output: &[u8],
    ) -> Result<Self, LanguageValidationError> {
        if output.len() > LANGUAGE_FRAME_PAYLOAD_LEN {
            return Err(LanguageValidationError::PayloadLength);
        }
        let mut result = Self {
            abi_version: LANGUAGE_RESOURCE_ABI_VERSION,
            struct_size: Self::SIZE as u16,
            flags: 0,
            status: status.raw(),
            reserved0: 0,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
            operation_id,
            call_id,
            output_len: output.len() as u16,
            reserved1: 0,
            output: [0; LANGUAGE_FRAME_PAYLOAD_LEN],
            reserved2: 0,
        };
        result.output[..output.len()].copy_from_slice(output);
        result.validate()?;
        Ok(result)
    }

    /// 返回回复 owner。
    pub const fn owner(&self) -> crate::LanguageOwnerV1 {
        crate::LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 返回有效输出 payload。
    pub fn output(&self) -> Result<&[u8], LanguageValidationError> {
        validate_payload_length(self.output_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        Ok(&self.output[..self.output_len as usize])
    }

    /// 验证回复帧。
    pub fn validate(&self) -> ValidationResult {
        validate_header(
            self.abi_version,
            self.struct_size,
            LANGUAGE_RESOURCE_ABI_VERSION,
            Self::SIZE,
        )?;
        validate_flags(self.flags, 0)?;
        validate_owner(self.owner_cell_id, self.owner_generation)?;
        validate_identifier(self.operation_id)?;
        validate_identifier(self.call_id)?;
        validate_payload_length(self.output_len, LANGUAGE_FRAME_PAYLOAD_LEN)?;
        validate_reserved(self.reserved0 as u64)?;
        validate_reserved(self.reserved1 as u64)?;
        validate_reserved(self.reserved2)
    }

    /// 在结构校验后确认回复 owner 与受信任调用上下文一致。
    pub fn validate_for_owner(&self, expected: crate::LanguageOwnerV1) -> ValidationResult {
        self.validate()?;
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected.cell_id,
            expected.generation,
        )
    }
}

const _: () = assert!(size_of::<LanguageKernelCallRequestV1>() == LANGUAGE_MANAGED_FRAME_LEN);
const _: () = assert!(size_of::<LanguageKernelCallResponseV1>() == LANGUAGE_MANAGED_FRAME_LEN);

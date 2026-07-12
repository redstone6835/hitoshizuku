//! EBI 来源管理所需的固定布局线类型。
//!
//! ELM Core 只消费 EBI 投影结果，不识别 EKI、SOYO 或其他容器。装载请求通过
//! [`ElmEbiSourceRequest`] 选择 `Projection`、`Builtin` 或测试用 `Memory` 来源；projection
//! 请求再用 [`ElmProjectionSourceRequest`] 指向负责解释某种容器的 provider。这样新增文件
//! 格式只需实现 projection source，不需要修改 Core。
//!
//! 大镜像通过 image session 上传并封口，装载请求只传递 [`ElmImageSessionReferenceV1`]，
//! 不把任意用户指针或网络位置塞入 EBI。所有结构均为小端固定布局，flags 与保留字段必须
//! 严格验证。

use crate::ids::{ELM_MGR_BUILTIN_ID, ElmId};
use crate::resource::ElmResourceBudget;

/// `ELM_EBI_SOURCE_ABI_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_EBI_SOURCE_ABI_VERSION: u16 = 1;
/// `ELM_EBI_SOURCE_REQUEST_SIZE` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_EBI_SOURCE_REQUEST_SIZE: usize = core::mem::size_of::<ElmEbiSourceRequest>();
/// `ELM_EBI_SOURCE_FLAG_NONE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_SOURCE_FLAG_NONE: u32 = 0;
/// `ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT: u32 = 1 << 0;
/// 请求运行时根据镜像声明、信任证明和调用主体授予 Kernel API capability。
pub const ELM_EBI_SOURCE_FLAG_GRANT_KERNEL_API: u32 = 1 << 1;
/// `ELM_EBI_SOURCE_FLAGS_MASK` 定义当前版本认可的全部标志位；输入包含掩码外位时必须拒绝或按调用契约报错。
pub const ELM_EBI_SOURCE_FLAGS_MASK: u32 =
    ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT | ELM_EBI_SOURCE_FLAG_GRANT_KERNEL_API;
/// `ELM_EBI_PROJECTION_SOURCE_ABI_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_EBI_PROJECTION_SOURCE_ABI_VERSION: u16 = 1;
/// `ELM_EBI_PROJECTION_SOURCE_REQUEST_SIZE` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_EBI_PROJECTION_SOURCE_REQUEST_SIZE: usize =
    core::mem::size_of::<ElmProjectionSourceRequest>();
/// `ELM_EBI_PROJECTION_SOURCE_FLAG_IMAGE_SESSION` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_EBI_PROJECTION_SOURCE_FLAG_IMAGE_SESSION: u16 = 1 << 0;
/// `ELM_EBI_PROJECTION_SOURCE_FLAGS_MASK` 定义当前版本认可的全部标志位；输入包含掩码外位时必须拒绝或按调用契约报错。
pub const ELM_EBI_PROJECTION_SOURCE_FLAGS_MASK: u16 = ELM_EBI_PROJECTION_SOURCE_FLAG_IMAGE_SESSION;
/// `ELM_IMAGE_SESSION_REFERENCE_ABI_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_IMAGE_SESSION_REFERENCE_ABI_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
/// `ElmEbiSourceKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmEbiSourceKind {
    /// `Projection` 表示 `ElmEbiSourceKind` 的对象类别：`projection`。
    Projection = 2,
    /// `Builtin` 表示 `ElmEbiSourceKind` 的对象类别：`builtin`。
    Builtin = 3,
    /// `Memory` 表示 `ElmEbiSourceKind` 的对象类别：`memory`。
    Memory = 4,
}

impl ElmEbiSourceKind {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
    pub const fn from_raw(raw: u16) -> Option<Self> {
        match raw {
            2 => Some(Self::Projection),
            3 => Some(Self::Builtin),
            4 => Some(Self::Memory),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmEbiSourceRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmEbiSourceRequest {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `source_kind` 是所属枚举的稳定判别值；未知值必须拒绝。
    pub source_kind: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `parent_cell_id` 所指对象的稳定运行时标识符。
    pub parent_cell_id: u64,
    /// 该 cell 当前生效的资源预算。
    pub budget: ElmResourceBudget,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u16,
    /// 有效载荷的实际字节数；不得超过相邻载荷缓冲区容量。
    pub payload_len: u32,
    /// 保留字段；生产者必须写零，消费者在当前 ABI 必须验证为零。
    pub reserved2: u32,
    /// 保留字段；生产者必须写零，消费者在当前 ABI 必须验证为零。
    pub reserved3: u32,
}

impl ElmEbiSourceRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(kind: ElmEbiSourceKind, payload_len: u32) -> Self {
        Self::new_under_parent(
            kind,
            ELM_MGR_BUILTIN_ID,
            ElmResourceBudget::DEFAULT,
            payload_len,
        )
    }

    /// 执行 `new_under_parent` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn new_under_parent(
        kind: ElmEbiSourceKind,
        parent: ElmId,
        budget: ElmResourceBudget,
        payload_len: u32,
    ) -> Self {
        Self {
            abi_version: ELM_EBI_SOURCE_ABI_VERSION,
            source_kind: kind as u16,
            flags: ELM_EBI_SOURCE_FLAG_NONE,
            parent_cell_id: parent.0,
            budget,
            reserved0: 0,
            reserved1: 0,
            payload_len,
            reserved2: 0,
            reserved3: 0,
        }
    }

    /// 设置 `management_grant` 并返回更新后的值，便于构建器式初始化。
    pub const fn with_management_grant(mut self) -> Self {
        self.flags |= ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT;
        self
    }

    /// 执行 `grants_management` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn grants_management(self) -> bool {
        self.flags & ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT != 0
    }

    /// 显式请求为镜像声明的 Kernel API requirements 建立按代授权。
    pub const fn with_kernel_api_grant(mut self) -> Self {
        self.flags |= ELM_EBI_SOURCE_FLAG_GRANT_KERNEL_API;
        self
    }

    /// 返回调用方是否显式请求了 Kernel API 授权事务。
    pub const fn grants_kernel_api(self) -> bool {
        self.flags & ELM_EBI_SOURCE_FLAG_GRANT_KERNEL_API != 0
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmProjectionSourceRequest` 是发送给 ELM 运行时的固定布局请求；保留字段必须为零，长度和标识符必须在调用前校验。
pub struct ElmProjectionSourceRequest {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u32,
    /// `provider_id` 所指对象的稳定运行时标识符。
    pub provider_id: u64,
    /// 有效载荷的实际字节数；不得超过相邻载荷缓冲区容量。
    pub payload_len: u32,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u32,
}

impl ElmProjectionSourceRequest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(provider_id: u64, payload_len: u32) -> Self {
        Self {
            abi_version: ELM_EBI_PROJECTION_SOURCE_ABI_VERSION,
            flags: 0,
            reserved0: 0,
            provider_id,
            payload_len,
            reserved1: 0,
        }
    }

    /// 执行 `from_image_session` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn from_image_session(provider_id: u64) -> Self {
        Self {
            abi_version: ELM_EBI_PROJECTION_SOURCE_ABI_VERSION,
            flags: ELM_EBI_PROJECTION_SOURCE_FLAG_IMAGE_SESSION,
            reserved0: 0,
            provider_id,
            payload_len: core::mem::size_of::<ElmImageSessionReferenceV1>() as u32,
            reserved1: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// projection source 请求引用一个已上传 image session 的固定布局句柄。
pub struct ElmImageSessionReferenceV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
    /// `session_id` 所指对象的稳定运行时标识符。
    pub session_id: u64,
}

impl ElmImageSessionReferenceV1 {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(session_id: u64) -> Self {
        Self {
            abi_version: ELM_IMAGE_SESSION_REFERENCE_ABI_VERSION,
            flags: 0,
            reserved: 0,
            session_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
/// `ElmEbiLoadStatus` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmEbiLoadStatus {
    /// `Ok` 表示 `ElmEbiLoadStatus` 的结果状态：`ok`。
    Ok = 0,
    /// `InvalidUnit` 表示 `ElmEbiLoadStatus` 的结果状态：`invalid unit`。
    InvalidUnit = -1,
    /// `UnsupportedAbi` 表示 `ElmEbiLoadStatus` 的结果状态：`unsupported abi`。
    UnsupportedAbi = -2,
    /// `InvalidTarget` 表示 `ElmEbiLoadStatus` 的结果状态：`invalid target`。
    InvalidTarget = -3,
    /// `InvalidSegment` 表示 `ElmEbiLoadStatus` 的结果状态：`invalid segment`。
    InvalidSegment = -4,
    /// `ArchMismatch` 表示 `ElmEbiLoadStatus` 的结果状态：`arch mismatch`。
    ArchMismatch = -5,
    /// `InvalidManifest` 表示 `ElmEbiLoadStatus` 的结果状态：`invalid manifest`。
    InvalidManifest = -6,
    /// `InvalidMenu` 表示 `ElmEbiLoadStatus` 的结果状态：`invalid menu`。
    InvalidMenu = -7,
    /// `NativeCodeTodo` 表示 `ElmEbiLoadStatus` 的结果状态：`native code todo`。
    NativeCodeTodo = -4096,
    /// `RuntimeRejected` 表示 `ElmEbiLoadStatus` 的结果状态：`runtime rejected`。
    RuntimeRejected = -4097,
    /// `UntrustedImage` 表示 `ElmEbiLoadStatus` 的结果状态：`untrusted image`。
    UntrustedImage = -4098,
    /// `AbiFingerprintRejected` 表示 `ElmEbiLoadStatus` 的结果状态：`abi fingerprint rejected`。
    AbiFingerprintRejected = -4099,
    /// `RollbackRejected` 表示 `ElmEbiLoadStatus` 的结果状态：`rollback rejected`。
    RollbackRejected = -4100,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmLoadCellResponse` 是 ELM 运行时返回的固定布局回复；调用方必须先检查状态和版本，再读取其余字段。
pub struct ElmLoadCellResponse {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 操作成功或失败收口后预期/实际到达的 cell 状态。
    pub final_state: u32,
    /// `reason` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub reason: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmLoadCellResponse {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(
        status: ElmEbiLoadStatus,
        cell_id: u64,
        final_state: u32,
        reason: u32,
    ) -> Self {
        Self {
            cell_id,
            status: status as i32,
            final_state,
            reason,
            reserved: 0,
        }
    }

    /// 执行 `failed` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn failed(status: ElmEbiLoadStatus) -> Self {
        Self::new(status, 0, 0, 0)
    }
}

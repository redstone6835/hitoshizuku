//! ELM 单元资源预算、用量和 owned resource 收口协议。
//!
//! [`ElmResourceBudget`] 限制 provider 端口、队列、订阅、镜像、故障、审计、并发调用、内存和
//! CPU 时间；[`ElmResourceUsage`] 是运行时核算快照。预算不是建议值，装载、注册、调用和
//! 动态调整都必须在提交前检查，子单元不能通过更新策略超过父级允许范围。
//!
//! owned resource 协议用于登记模块创建但需要运行时协助排空的 timer、task、work item、
//! callback、IRQ callback 和异步请求。运行时按 quiesce、cancel、drain、release 顺序调用
//! 操作表，避免卸载代码仍被异步回调访问。

use crate::ids::{ElmId, Generation};

/// `ELM_OWNED_RESOURCE_ABI_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_OWNED_RESOURCE_ABI_VERSION: u16 = 1;
/// `ELM_OWNED_RESOURCE_FLAG_NONE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_OWNED_RESOURCE_FLAG_NONE: u32 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// `ElmOwnedResourceKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmOwnedResourceKind {
    /// `Task` 表示 `ElmOwnedResourceKind` 的对象类别：`task`。
    Task = 1,
    /// `Timer` 表示 `ElmOwnedResourceKind` 的对象类别：`timer`。
    Timer = 2,
    /// `WorkItem` 表示 `ElmOwnedResourceKind` 的对象类别：`work item`。
    WorkItem = 3,
    /// `Callback` 表示 `ElmOwnedResourceKind` 的对象类别：`callback`。
    Callback = 4,
    /// `IrqCallback` 表示 `ElmOwnedResourceKind` 的对象类别：`irq callback`。
    IrqCallback = 5,
    /// `AsyncRequest` 表示 `ElmOwnedResourceKind` 的对象类别：`async request`。
    AsyncRequest = 6,
    /// `Custom` 表示 `ElmOwnedResourceKind` 的对象类别：`custom`。
    Custom = 7,
}

impl ElmOwnedResourceKind {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Task),
            2 => Some(Self::Timer),
            3 => Some(Self::WorkItem),
            4 => Some(Self::Callback),
            5 => Some(Self::IrqCallback),
            6 => Some(Self::AsyncRequest),
            7 => Some(Self::Custom),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// `ElmOwnedResourceState` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmOwnedResourceState {
    /// `Active` 表示 `ElmOwnedResourceState` 的生命周期状态：`active`。
    Active = 1,
    /// `Quiescing` 表示 `ElmOwnedResourceState` 的生命周期状态：`quiescing`。
    Quiescing = 2,
    /// `Canceling` 表示 `ElmOwnedResourceState` 的生命周期状态：`canceling`。
    Canceling = 3,
    /// `Draining` 表示 `ElmOwnedResourceState` 的生命周期状态：`draining`。
    Draining = 4,
    /// `Releasing` 表示 `ElmOwnedResourceState` 的生命周期状态：`releasing`。
    Releasing = 5,
    /// `Failed` 表示 `ElmOwnedResourceState` 的生命周期状态：`failed`。
    Failed = 6,
}

/// `ElmOwnedResourceOp` 为该调用路径使用的规范类型别名，统一公开签名并避免重复表达底层布局。
pub type ElmOwnedResourceOp =
    fn(owner: ElmId, generation: Generation, handle: u64) -> Result<(), i32>;

/// 子系统资源的完整退役操作表。
///
/// 四个阶段均为必需项。子系统没有对应动作时必须显式提供成功的空操作，不能用
/// 缺失函数指针隐式跳过安全边界。所有函数返回前必须完成对应阶段，不得把回调
/// 或执行现场留在待卸载 ELM 镜像中。操作表和函数入口必须由常驻内核子系统提供，
/// 不得存放在可卸载 ELM 镜像内；ELM 只持有子系统分配的资源句柄。
#[derive(Clone, Copy)]
pub struct ElmOwnedResourceOpsV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 执行 `quiesce` 操作的受控回调；调用方必须遵守所属表的生命周期和故障边界。
    pub quiesce: ElmOwnedResourceOp,
    /// 执行 `cancel` 操作的受控回调；调用方必须遵守所属表的生命周期和故障边界。
    pub cancel: ElmOwnedResourceOp,
    /// 执行 `drain` 操作的受控回调；调用方必须遵守所属表的生命周期和故障边界。
    pub drain: ElmOwnedResourceOp,
    /// 执行 `release` 操作的受控回调；调用方必须遵守所属表的生命周期和故障边界。
    pub release: ElmOwnedResourceOp,
}

impl ElmOwnedResourceOpsV1 {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(
        quiesce: ElmOwnedResourceOp,
        cancel: ElmOwnedResourceOp,
        drain: ElmOwnedResourceOp,
        release: ElmOwnedResourceOp,
    ) -> Self {
        Self {
            abi_version: ELM_OWNED_RESOURCE_ABI_VERSION,
            reserved: 0,
            flags: ELM_OWNED_RESOURCE_FLAG_NONE,
            quiesce,
            cancel,
            drain,
            release,
        }
    }

    /// 检查版本、保留字段、标志位和必需入口是否满足当前 ABI 约束。
    pub const fn valid(&self) -> bool {
        self.abi_version == ELM_OWNED_RESOURCE_ABI_VERSION
            && self.reserved == 0
            && self.flags == ELM_OWNED_RESOURCE_FLAG_NONE
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmOwnedResourceSnapshotV1` 是某一时刻的只读快照表示，不授予对原对象的所有权或长期引用。
pub struct ElmOwnedResourceSnapshotV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 生产者写入的完整结构字节数，用于向前兼容地判断可读取字段范围。
    pub struct_size: u16,
    /// 对象或单元的当前状态编码。
    pub state: u32,
    /// `resource_id` 所指对象的稳定运行时标识符。
    pub resource_id: u64,
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// 资源所有者的代际；必须与单元当前代际同时匹配。
    pub owner_generation: u64,
    /// `handle` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub handle: u64,
    /// 该记录、资源或关系的类别编码。
    pub kind: u32,
    /// 该对象最近一次受控操作返回的状态码。
    pub last_status: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmResourceBudget` 描述 ELM 资源限制或用量；所有计数和字节值均受运行时配额核算。
pub struct ElmResourceBudget {
    /// `max_provider_ports` 对应资源预算的硬上限；零值语义由所属预算结构定义。
    pub max_provider_ports: u16,
    /// `max_provider_queue` 对应资源预算的硬上限；零值语义由所属预算结构定义。
    pub max_provider_queue: u16,
    /// `max_event_subscriptions` 对应资源预算的硬上限；零值语义由所属预算结构定义。
    pub max_event_subscriptions: u16,
    /// `max_pending_loads` 对应资源预算的硬上限；零值语义由所属预算结构定义。
    pub max_pending_loads: u16,
    /// `max_native_images` 对应资源预算的硬上限；零值语义由所属预算结构定义。
    pub max_native_images: u16,
    /// `max_native_faults` 对应资源预算的硬上限；零值语义由所属预算结构定义。
    pub max_native_faults: u16,
    /// `max_audit_records` 对应资源预算的硬上限；零值语义由所属预算结构定义。
    pub max_audit_records: u16,
    /// `max_concurrent_calls` 对应资源预算的硬上限；零值语义由所属预算结构定义。
    pub max_concurrent_calls: u16,
    /// `max_native_image_bytes` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub max_native_image_bytes: u64,
    /// `max_native_stack_bytes` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub max_native_stack_bytes: u64,
    /// `max_dynamic_alloc_bytes` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub max_dynamic_alloc_bytes: u64,
    /// `max_cpu_time_ns_per_call` 对应资源预算的硬上限；零值语义由所属预算结构定义。
    pub max_cpu_time_ns_per_call: u64,
    /// `cpu_budget_ns_per_period` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub cpu_budget_ns_per_period: u64,
    /// `cpu_period_ns` 使用纳秒单位；具体时钟域由所属记录定义。
    pub cpu_period_ns: u64,
}

impl ElmResourceBudget {
    /// `ROOT` 是内建根 elm-mgr 使用的资源预算，覆盖运行时允许的最大管理容量。
    pub const ROOT: Self = Self {
        max_provider_ports: 256,
        max_provider_queue: 256,
        max_event_subscriptions: 256,
        max_pending_loads: 64,
        max_native_images: 128,
        max_native_faults: 16,
        max_audit_records: 1024,
        max_concurrent_calls: 256,
        max_native_image_bytes: 256 * 1024 * 1024,
        max_native_stack_bytes: 64 * 1024 * 1024,
        max_dynamic_alloc_bytes: 1024 * 1024 * 1024,
        max_cpu_time_ns_per_call: 5_000_000_000,
        cpu_budget_ns_per_period: 60_000_000_000,
        cpu_period_ns: 60_000_000_000,
    };

    /// `DEFAULT` 是普通动态 ELM 在未显式声明预算时使用的保守资源上限。
    pub const DEFAULT: Self = Self {
        max_provider_ports: 16,
        max_provider_queue: 64,
        max_event_subscriptions: 16,
        max_pending_loads: 4,
        max_native_images: 8,
        max_native_faults: 3,
        max_audit_records: 128,
        max_concurrent_calls: 16,
        max_native_image_bytes: 16 * 1024 * 1024,
        max_native_stack_bytes: 4 * 1024 * 1024,
        max_dynamic_alloc_bytes: 64 * 1024 * 1024,
        max_cpu_time_ns_per_call: 1_000_000_000,
        cpu_budget_ns_per_period: 2_500_000_000,
        cpu_period_ns: 10_000_000_000,
    };
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
/// `ElmResourceUsage` 描述 ELM 资源限制或用量；所有计数和字节值均受运行时配额核算。
pub struct ElmResourceUsage {
    /// `provider_ports` 保存所属对象声明或快照中的有序记录集合。
    pub provider_ports: u16,
    /// `provider_queue` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub provider_queue: u16,
    /// `event_subscriptions` 保存所属对象声明或快照中的有序记录集合。
    pub event_subscriptions: u16,
    /// `pending_loads` 保存所属对象声明或快照中的有序记录集合。
    pub pending_loads: u16,
    /// `native_images` 保存所属对象声明或快照中的有序记录集合。
    pub native_images: u16,
    /// `native_faults` 保存所属对象声明或快照中的有序记录集合。
    pub native_faults: u16,
    /// `audit_records` 保存所属对象声明或快照中的有序记录集合。
    pub audit_records: u16,
    /// `active_calls` 是对应对象、调用或引用的数量。
    pub active_calls: u16,
    /// `native_image_bytes` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub native_image_bytes: u64,
    /// `native_stack_bytes` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub native_stack_bytes: u64,
    /// `dynamic_alloc_bytes` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub dynamic_alloc_bytes: u64,
    /// `peak_dynamic_alloc_bytes` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub peak_dynamic_alloc_bytes: u64,
    /// `cpu_time_ns_total` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub cpu_time_ns_total: u64,
    /// `cpu_time_ns_period` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub cpu_time_ns_period: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmResourceKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmResourceKind {
    /// `ProviderPort` 表示 `ElmResourceKind` 的对象类别：`provider port`。
    ProviderPort,
    /// `ProviderQueue` 表示 `ElmResourceKind` 的对象类别：`provider queue`。
    ProviderQueue,
    /// `EventSubscription` 表示 `ElmResourceKind` 的对象类别：`event subscription`。
    EventSubscription,
    /// `PendingLoad` 表示 `ElmResourceKind` 的对象类别：`pending load`。
    PendingLoad,
    /// `NativeImage` 表示 `ElmResourceKind` 的对象类别：`native image`。
    NativeImage,
    /// `NativeFault` 表示 `ElmResourceKind` 的对象类别：`native fault`。
    NativeFault,
    /// `AuditRecord` 表示 `ElmResourceKind` 的对象类别：`audit record`。
    AuditRecord,
    /// `ConcurrentCall` 表示 `ElmResourceKind` 的对象类别：`concurrent call`。
    ConcurrentCall,
    /// `NativeImageBytes` 表示 `ElmResourceKind` 的对象类别：`native image bytes`。
    NativeImageBytes,
    /// `NativeStackBytes` 表示 `ElmResourceKind` 的对象类别：`native stack bytes`。
    NativeStackBytes,
    /// `DynamicAllocBytes` 表示 `ElmResourceKind` 的对象类别：`dynamic alloc bytes`。
    DynamicAllocBytes,
    /// `CpuTimePerCall` 表示 `ElmResourceKind` 的对象类别：`cpu time per call`。
    CpuTimePerCall,
    /// `CpuTimePerPeriod` 表示 `ElmResourceKind` 的对象类别：`cpu time per period`。
    CpuTimePerPeriod,
}

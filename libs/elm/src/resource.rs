//! ELM 单元资源预算模型。

use crate::ids::{ElmId, Generation};

pub const ELM_OWNED_RESOURCE_ABI_VERSION: u16 = 1;
pub const ELM_OWNED_RESOURCE_FLAG_NONE: u32 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmOwnedResourceKind {
    Task = 1,
    Timer = 2,
    WorkItem = 3,
    Callback = 4,
    IrqCallback = 5,
    AsyncRequest = 6,
    Custom = 7,
}

impl ElmOwnedResourceKind {
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
pub enum ElmOwnedResourceState {
    Active = 1,
    Quiescing = 2,
    Canceling = 3,
    Draining = 4,
    Releasing = 5,
    Failed = 6,
}

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
    pub abi_version: u16,
    pub reserved: u16,
    pub flags: u32,
    pub quiesce: ElmOwnedResourceOp,
    pub cancel: ElmOwnedResourceOp,
    pub drain: ElmOwnedResourceOp,
    pub release: ElmOwnedResourceOp,
}

impl ElmOwnedResourceOpsV1 {
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

    pub const fn valid(&self) -> bool {
        self.abi_version == ELM_OWNED_RESOURCE_ABI_VERSION
            && self.reserved == 0
            && self.flags == ELM_OWNED_RESOURCE_FLAG_NONE
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmOwnedResourceSnapshotV1 {
    pub abi_version: u16,
    pub struct_size: u16,
    pub state: u32,
    pub resource_id: u64,
    pub owner_cell_id: u64,
    pub owner_generation: u64,
    pub handle: u64,
    pub kind: u32,
    pub last_status: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmResourceBudget {
    pub max_provider_ports: u16,
    pub max_provider_queue: u16,
    pub max_event_subscriptions: u16,
    pub max_pending_loads: u16,
    pub max_native_images: u16,
    pub max_native_faults: u16,
    pub max_audit_records: u16,
    pub max_concurrent_calls: u16,
    pub max_native_image_bytes: u64,
    pub max_native_stack_bytes: u64,
    pub max_dynamic_alloc_bytes: u64,
    pub max_cpu_time_ns_per_call: u64,
    pub cpu_budget_ns_per_period: u64,
    pub cpu_period_ns: u64,
}

impl ElmResourceBudget {
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
pub struct ElmResourceUsage {
    pub provider_ports: u16,
    pub provider_queue: u16,
    pub event_subscriptions: u16,
    pub pending_loads: u16,
    pub native_images: u16,
    pub native_faults: u16,
    pub audit_records: u16,
    pub active_calls: u16,
    pub native_image_bytes: u64,
    pub native_stack_bytes: u64,
    pub dynamic_alloc_bytes: u64,
    pub peak_dynamic_alloc_bytes: u64,
    pub cpu_time_ns_total: u64,
    pub cpu_time_ns_period: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElmResourceKind {
    ProviderPort,
    ProviderQueue,
    EventSubscription,
    PendingLoad,
    NativeImage,
    NativeFault,
    AuditRecord,
    ConcurrentCall,
    NativeImageBytes,
    NativeStackBytes,
    DynamicAllocBytes,
    CpuTimePerCall,
    CpuTimePerPeriod,
}

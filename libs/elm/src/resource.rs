//! ELM 单元资源预算模型。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmResourceBudget {
    pub max_provider_ports: u16,
    pub max_provider_queue: u16,
    pub max_event_subscriptions: u16,
    pub max_pending_loads: u16,
    pub max_native_images: u16,
    pub max_native_faults: u16,
    pub max_audit_records: u16,
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
    };

    pub const DEFAULT: Self = Self {
        max_provider_ports: 16,
        max_provider_queue: 64,
        max_event_subscriptions: 16,
        max_pending_loads: 4,
        max_native_images: 8,
        max_native_faults: 3,
        max_audit_records: 128,
    };
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ElmResourceUsage {
    pub provider_ports: u16,
    pub provider_queue: u16,
    pub event_subscriptions: u16,
    pub pending_loads: u16,
    pub native_images: u16,
    pub native_faults: u16,
    pub audit_records: u16,
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
}

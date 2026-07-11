//! `elm-mgr` 对外 API 与事件订阅协议。
//!
//! 本模块只定义稳定的 Rust ABI 固定布局。新代码优先使用
//! `elm::elmmgr::api::*`；`elm::mgr::api::*` 保留为兼容路径。这里描述的是
//! ELM 运行时自身能力，不是访问 VFS、调度、内存分配等子系统的唯一入口。

use crate::ctl::ELM_CTL_ABI_VERSION;
use crate::event::ElmEventRecord;

pub const ELM_MGR_API_NAMESPACE_LEN: usize = 32;
pub const ELM_MGR_API_NAME_LEN: usize = 48;
pub const ELM_MGR_API_CONTRACT_LEN: usize = 48;

pub const ELM_MGR_API_KIND_CONTROL: u32 = 1;
pub const ELM_MGR_API_KIND_SNAPSHOT: u32 = 2;
pub const ELM_MGR_API_KIND_EVENT: u32 = 3;
pub const ELM_MGR_API_KIND_PROVIDER: u32 = 4;
pub const ELM_MGR_API_KIND_SUBSYSTEM: u32 = 5;

pub const ELM_MGR_API_FLAG_STABLE: u32 = 1 << 0;
pub const ELM_MGR_API_FLAG_TODO: u32 = 1 << 1;
pub const ELM_MGR_API_FLAG_SYSCALL: u32 = 1 << 2;
pub const ELM_MGR_API_FLAG_SYSFS: u32 = 1 << 3;
pub const ELM_MGR_API_FLAG_PROVIDER_OPS: u32 = 1 << 4;

pub const ELM_RUNTIME_LOG_EXPORT_NAME: &str = "elm.runtime.log";
pub const ELM_RUNTIME_LOG_EXPORT_CONTRACT: &str = "elm.runtime.log@1";
pub const ELM_RUNTIME_LOG_EXPORT_VERSION: u32 = 1;

pub const ELM_MGR_EVENT_FILTER_ANY: u32 = 0;
pub const ELM_MGR_EVENT_SUBSCRIPTION_FLAG_ACTIVE: u32 = 1 << 0;
pub const ELM_MGR_EVENT_READ_FLAG_ADVANCE: u32 = 1 << 0;
pub const ELM_MGR_EVENT_READ_DEFAULT_MAX_RECORDS: u32 = 32;
pub const ELM_MGR_EVENT_READ_ABSOLUTE_MAX_RECORDS: u32 = 64;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrApiRegistryHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub flags: u32,
    pub reserved: u32,
    pub generation: u64,
}

impl ElmMgrApiRegistryHeader {
    pub const fn new(record_count: u32, generation: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmMgrApiDescriptor>() as u16,
            record_count,
            flags: 0,
            reserved: 0,
            generation,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrApiDescriptor {
    pub id: u64,
    pub owner_cell_id: u64,
    pub kind: u32,
    pub flags: u32,
    pub call_kind: u32,
    pub min_abi_version: u16,
    pub current_abi_version: u16,
    pub namespace_len: u16,
    pub name_len: u16,
    pub contract_len: u16,
    pub reserved0: u16,
    pub capabilities: u64,
    pub namespace: [u8; ELM_MGR_API_NAMESPACE_LEN],
    pub name: [u8; ELM_MGR_API_NAME_LEN],
    pub contract: [u8; ELM_MGR_API_CONTRACT_LEN],
}

impl ElmMgrApiDescriptor {
    pub fn new(
        id: u64,
        owner_cell_id: u64,
        kind: u32,
        flags: u32,
        call_kind: u32,
        namespace: &str,
        name: &str,
        contract: &str,
    ) -> Self {
        let mut out = Self {
            id,
            owner_cell_id,
            kind,
            flags,
            call_kind,
            min_abi_version: ELM_CTL_ABI_VERSION,
            current_abi_version: ELM_CTL_ABI_VERSION,
            namespace_len: 0,
            name_len: 0,
            contract_len: 0,
            reserved0: 0,
            capabilities: 0,
            namespace: [0; ELM_MGR_API_NAMESPACE_LEN],
            name: [0; ELM_MGR_API_NAME_LEN],
            contract: [0; ELM_MGR_API_CONTRACT_LEN],
        };
        out.namespace_len = copy_str(namespace, &mut out.namespace) as u16;
        out.name_len = copy_str(name, &mut out.name) as u16;
        out.contract_len = copy_str(contract, &mut out.contract) as u16;
        out
    }

    pub const fn with_capabilities(mut self, capabilities: u64) -> Self {
        self.capabilities = capabilities;
        self
    }
}

pub type ElmMgrApiDescriptorRecord = ElmMgrApiDescriptor;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrEventSubscribeRequest {
    pub owner_cell_id: u64,
    pub kind_filter: u32,
    pub flags: u32,
    pub cell_filter: u64,
    pub port_filter: u64,
    pub binding_filter: u64,
    pub lease_filter: u64,
}

impl ElmMgrEventSubscribeRequest {
    pub const fn new(owner_cell_id: u64) -> Self {
        Self {
            owner_cell_id,
            kind_filter: ELM_MGR_EVENT_FILTER_ANY,
            flags: 0,
            cell_filter: 0,
            port_filter: 0,
            binding_filter: 0,
            lease_filter: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrEventSubscribeResponse {
    pub subscription_id: u64,
    pub lease_id: u64,
    pub owner_cell_id: u64,
    pub cursor: u64,
    pub status: i32,
    pub flags: u32,
    pub dropped_events: u64,
}

impl ElmMgrEventSubscribeResponse {
    pub const fn new(
        subscription_id: u64,
        lease_id: u64,
        owner_cell_id: u64,
        cursor: u64,
        status: i32,
        dropped_events: u64,
    ) -> Self {
        Self {
            subscription_id,
            lease_id,
            owner_cell_id,
            cursor,
            status,
            flags: if status == 0 {
                ELM_MGR_EVENT_SUBSCRIPTION_FLAG_ACTIVE
            } else {
                0
            },
            dropped_events,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrEventUnsubscribeRequest {
    pub subscription_id: u64,
    pub owner_cell_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl ElmMgrEventUnsubscribeRequest {
    pub const fn new(subscription_id: u64, owner_cell_id: u64) -> Self {
        Self {
            subscription_id,
            owner_cell_id,
            flags: 0,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrEventUnsubscribeResponse {
    pub subscription_id: u64,
    pub lease_id: u64,
    pub owner_cell_id: u64,
    pub status: i32,
    pub revoked: u32,
    pub delivered_events: u64,
    pub dropped_events: u64,
}

impl ElmMgrEventUnsubscribeResponse {
    pub const fn new(
        subscription_id: u64,
        lease_id: u64,
        owner_cell_id: u64,
        status: i32,
        revoked: bool,
        delivered_events: u64,
        dropped_events: u64,
    ) -> Self {
        Self {
            subscription_id,
            lease_id,
            owner_cell_id,
            status,
            revoked: if revoked { 1 } else { 0 },
            delivered_events,
            dropped_events,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrEventSubscriptionHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub event_sequence: u64,
}

impl ElmMgrEventSubscriptionHeader {
    pub const fn new(record_count: u32, event_sequence: u64) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmMgrEventSubscriptionRecord>() as u16,
            record_count,
            event_sequence,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrEventSubscriptionRecord {
    pub subscription_id: u64,
    pub owner_cell_id: u64,
    pub lease_id: u64,
    pub cursor: u64,
    pub kind_filter: u32,
    pub flags: u32,
    pub cell_filter: u64,
    pub port_filter: u64,
    pub binding_filter: u64,
    pub lease_filter: u64,
    pub delivered_events: u64,
    pub dropped_events: u64,
}

impl ElmMgrEventSubscriptionRecord {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        subscription_id: u64,
        owner_cell_id: u64,
        lease_id: u64,
        cursor: u64,
        kind_filter: u32,
        active: bool,
        cell_filter: u64,
        port_filter: u64,
        binding_filter: u64,
        lease_filter: u64,
        delivered_events: u64,
        dropped_events: u64,
    ) -> Self {
        Self {
            subscription_id,
            owner_cell_id,
            lease_id,
            cursor,
            kind_filter,
            flags: if active {
                ELM_MGR_EVENT_SUBSCRIPTION_FLAG_ACTIVE
            } else {
                0
            },
            cell_filter,
            port_filter,
            binding_filter,
            lease_filter,
            delivered_events,
            dropped_events,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrSubscribedEventReadRequest {
    pub subscription_id: u64,
    pub cursor: u64,
    pub max_records: u32,
    pub flags: u32,
}

impl ElmMgrSubscribedEventReadRequest {
    pub const fn new(subscription_id: u64, cursor: u64, max_records: u32) -> Self {
        Self {
            subscription_id,
            cursor,
            max_records,
            flags: ELM_MGR_EVENT_READ_FLAG_ADVANCE,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmMgrSubscribedEventReadHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub status: i32,
    pub flags: u32,
    pub subscription_id: u64,
    pub cursor: u64,
    pub next_cursor: u64,
    pub dropped_events: u64,
}

impl ElmMgrSubscribedEventReadHeader {
    pub const fn new(
        record_count: u32,
        status: i32,
        flags: u32,
        subscription_id: u64,
        cursor: u64,
        next_cursor: u64,
        dropped_events: u64,
    ) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmEventRecord>() as u16,
            record_count,
            status,
            flags,
            subscription_id,
            cursor,
            next_cursor,
            dropped_events,
        }
    }
}

fn copy_str(value: &str, out: &mut [u8]) -> usize {
    let bytes = value.as_bytes();
    let len = core::cmp::min(bytes.len(), out.len());
    out[..len].copy_from_slice(&bytes[..len]);
    len
}

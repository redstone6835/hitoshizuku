//! 设备层导出的 ELM provider 规格。
//!
//! 这里声明设备、IRQ、DMA、MMIO 和块 I/O 暴露给 `elm-mgr` 的稳定入口。
//! 具体协议和真实执行逻辑属于设备层，ELM Core 只读取这些规格并转发调用。

use alloc::vec::Vec;
use core::str;

use elm_model::{
    BindingId, ELM_CALL_STATUS_BUSY, ELM_CALL_STATUS_INVALID, ELM_CALL_STATUS_NOT_FOUND,
    ELM_CALL_STATUS_OK, ELM_CALL_STATUS_UNSUPPORTED, ELM_FRAME_PAYLOAD_LEN,
    ELM_KERNEL_PROVIDER_FLAG_NONE, ELM_MGR_API_KIND_SUBSYSTEM, ELM_MGR_STATUS_BUSY,
    ELM_MGR_STATUS_INVALID, ElmCallFrame, ElmKernelProviderSpec, ElmPortAccessPolicy,
    ElmReplyFrame, FlowDirection, FlowMode, LeaseId,
};
use vfs::sync::Spinlock;

use crate::dev::enumerate::DEVICES;

pub const ELM_DEV_DISCOVERY_OPCODE_QUERY: u32 = 1;
pub const ELM_DEV_DISCOVERY_CLASS_LEN: usize = 16;
pub const ELM_DEV_DISCOVERY_NAME_LEN: usize = 64;
pub const ELM_DEV_DISCOVERY_FLAG_TRUNCATED: u32 = 1 << 0;
pub const ELM_DEV_DISCOVERY_RECORD_FLAG_CLASS_TRUNCATED: u32 = 1 << 0;
pub const ELM_DEV_DISCOVERY_RECORD_FLAG_NAME_TRUNCATED: u32 = 1 << 1;
pub const ELM_DEV_DISCOVERY_CAP_SNAPSHOT: u64 = 1 << 0;
pub const ELM_DEV_DISCOVERY_CAP_INVOKE_QUERY: u64 = 1 << 1;
pub const ELM_DEV_CLAIM_OPCODE_ACQUIRE: u32 = 1;
pub const ELM_DEV_CLAIM_OPCODE_RELEASE: u32 = 2;
pub const ELM_DEV_CLAIM_OPCODE_QUERY: u32 = 3;
pub const ELM_DEV_CLAIM_CLASS_LEN: usize = ELM_DEV_DISCOVERY_CLASS_LEN;
pub const ELM_DEV_CLAIM_NAME_LEN: usize = ELM_DEV_DISCOVERY_NAME_LEN;
pub const ELM_DEV_CLAIM_FLAG_NONE: u16 = 0;
pub const ELM_DEV_CLAIM_SNAPSHOT_FLAG_TRUNCATED: u32 = 1 << 0;
pub const ELM_DEV_CLAIM_REPLY_FLAG_HELD: u32 = 1 << 0;
pub const ELM_DEV_CLAIM_CAP_ACQUIRE: u64 = 1 << 0;
pub const ELM_DEV_CLAIM_CAP_RELEASE: u64 = 1 << 1;
pub const ELM_DEV_CLAIM_CAP_QUERY: u64 = 1 << 2;
pub const ELM_DEV_CLAIM_CAP_SNAPSHOT: u64 = 1 << 3;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmDeviceClaimRequest {
    pub abi_version: u16,
    pub flags: u16,
    pub class_len: u16,
    pub name_len: u16,
    pub class_name: [u8; ELM_DEV_CLAIM_CLASS_LEN],
    pub dev_name: [u8; ELM_DEV_CLAIM_NAME_LEN],
}

impl ElmDeviceClaimRequest {
    pub fn new(class_name: &str, dev_name: &str) -> Self {
        let mut out = Self {
            abi_version: elm_model::ELM_CTL_ABI_VERSION,
            flags: ELM_DEV_CLAIM_FLAG_NONE,
            class_len: 0,
            name_len: 0,
            class_name: [0; ELM_DEV_CLAIM_CLASS_LEN],
            dev_name: [0; ELM_DEV_CLAIM_NAME_LEN],
        };
        out.class_len = copy_str(class_name, &mut out.class_name) as u16;
        out.name_len = copy_str(dev_name, &mut out.dev_name) as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmDeviceClaimReply {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub flags: u32,
    pub owner_binding_id: u64,
    pub owner_lease_id: u64,
    pub class_len: u16,
    pub name_len: u16,
    pub reserved: u32,
    pub class_name: [u8; ELM_DEV_CLAIM_CLASS_LEN],
    pub dev_name: [u8; ELM_DEV_CLAIM_NAME_LEN],
}

impl ElmDeviceClaimReply {
    fn from_claim(claim: &DeviceClaimRecord, flags: u32) -> Self {
        Self {
            abi_version: elm_model::ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmDeviceClaimRecord>() as u16,
            flags,
            owner_binding_id: claim.binding_id,
            owner_lease_id: 0,
            class_len: claim.class_len,
            name_len: claim.name_len,
            reserved: 0,
            class_name: claim.class_name,
            dev_name: claim.dev_name,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmDeviceClaimSnapshotHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub total_count: u32,
    pub flags: u32,
    pub generation: u64,
}

impl ElmDeviceClaimSnapshotHeader {
    fn new(record_count: u32, total_count: u32, flags: u32, generation: u64) -> Self {
        Self {
            abi_version: elm_model::ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmDeviceClaimRecord>() as u16,
            record_count,
            total_count,
            flags,
            generation,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmDeviceClaimRecord {
    pub owner_binding_id: u64,
    pub owner_lease_id: u64,
    pub class_len: u16,
    pub name_len: u16,
    pub flags: u32,
    pub class_name: [u8; ELM_DEV_CLAIM_CLASS_LEN],
    pub dev_name: [u8; ELM_DEV_CLAIM_NAME_LEN],
}

#[derive(Debug, Clone, Copy)]
struct DeviceClaimRecord {
    binding_id: u64,
    class_len: u16,
    name_len: u16,
    class_name: [u8; ELM_DEV_CLAIM_CLASS_LEN],
    dev_name: [u8; ELM_DEV_CLAIM_NAME_LEN],
}

impl DeviceClaimRecord {
    fn new(binding_id: u64, request: &ElmDeviceClaimRequest) -> Self {
        Self {
            binding_id,
            class_len: request.class_len,
            name_len: request.name_len,
            class_name: request.class_name,
            dev_name: request.dev_name,
        }
    }

    fn class_bytes(&self) -> &[u8] {
        &self.class_name[..self.class_len as usize]
    }

    fn name_bytes(&self) -> &[u8] {
        &self.dev_name[..self.name_len as usize]
    }

    fn matches_request(&self, request: &ElmDeviceClaimRequest) -> bool {
        self.class_len == request.class_len
            && self.name_len == request.name_len
            && self.class_bytes() == request.class_bytes()
            && self.name_bytes() == request.name_bytes()
    }
}

impl ElmDeviceClaimRecord {
    fn from_claim(claim: &DeviceClaimRecord) -> Self {
        Self {
            owner_binding_id: claim.binding_id,
            owner_lease_id: 0,
            class_len: claim.class_len,
            name_len: claim.name_len,
            flags: ELM_DEV_CLAIM_REPLY_FLAG_HELD,
            class_name: claim.class_name,
            dev_name: claim.dev_name,
        }
    }
}

#[derive(Debug)]
struct DeviceClaimRegistry {
    generation: u64,
    claims: Vec<DeviceClaimRecord>,
}

impl DeviceClaimRegistry {
    const fn new() -> Self {
        Self {
            generation: 0,
            claims: Vec::new(),
        }
    }

    fn acquire(
        &mut self,
        binding_id: u64,
        request: &ElmDeviceClaimRequest,
    ) -> Result<DeviceClaimRecord, i32> {
        if let Some(existing) = self
            .claims
            .iter()
            .find(|claim| claim.matches_request(request))
            .copied()
        {
            if existing.binding_id == binding_id {
                return Ok(existing);
            }
            return Err(ELM_CALL_STATUS_BUSY);
        }
        self.claims
            .try_reserve(1)
            .map_err(|_| ELM_CALL_STATUS_BUSY)?;
        let claim = DeviceClaimRecord::new(binding_id, request);
        self.claims.push(claim);
        self.generation = self.generation.saturating_add(1);
        Ok(claim)
    }

    fn release(
        &mut self,
        binding_id: u64,
        request: &ElmDeviceClaimRequest,
    ) -> Result<DeviceClaimRecord, i32> {
        let Some(index) = self
            .claims
            .iter()
            .position(|claim| claim.matches_request(request))
        else {
            return Err(ELM_CALL_STATUS_NOT_FOUND);
        };
        let claim = self.claims[index];
        if claim.binding_id != binding_id {
            return Err(ELM_CALL_STATUS_BUSY);
        }
        self.claims.swap_remove(index);
        self.generation = self.generation.saturating_add(1);
        Ok(claim)
    }

    fn query(&self, request: &ElmDeviceClaimRequest) -> Result<DeviceClaimRecord, i32> {
        self.claims
            .iter()
            .find(|claim| claim.matches_request(request))
            .copied()
            .ok_or(ELM_CALL_STATUS_NOT_FOUND)
    }

    fn release_binding(&mut self, binding_id: u64) {
        let old_len = self.claims.len();
        self.claims.retain(|claim| claim.binding_id != binding_id);
        if self.claims.len() != old_len {
            self.generation = self.generation.saturating_add(1);
        }
    }
}

static DEVICE_CLAIMS: Spinlock<DeviceClaimRegistry> = Spinlock::new(DeviceClaimRegistry::new());

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmDeviceDiscoveryHeader {
    pub abi_version: u16,
    pub record_entry_size: u16,
    pub record_count: u32,
    pub total_count: u32,
    pub flags: u32,
    pub generation: u64,
}

impl ElmDeviceDiscoveryHeader {
    pub const fn new(record_count: u32, total_count: u32, flags: u32) -> Self {
        Self {
            abi_version: elm_model::ELM_CTL_ABI_VERSION,
            record_entry_size: core::mem::size_of::<ElmDeviceDiscoveryRecord>() as u16,
            record_count,
            total_count,
            flags,
            // TODO(elm): 设备 registry 接入 generation 计数后这里输出真实世代号。
            generation: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmDeviceDiscoveryRecord {
    pub ordinal: u64,
    pub class_len: u16,
    pub name_len: u16,
    pub flags: u32,
    pub class_name: [u8; ELM_DEV_DISCOVERY_CLASS_LEN],
    pub dev_name: [u8; ELM_DEV_DISCOVERY_NAME_LEN],
}

impl ElmDeviceDiscoveryRecord {
    pub fn new(ordinal: u64, class_name: &str, dev_name: &str) -> Self {
        let mut out = Self {
            ordinal,
            class_len: 0,
            name_len: 0,
            flags: 0,
            class_name: [0; ELM_DEV_DISCOVERY_CLASS_LEN],
            dev_name: [0; ELM_DEV_DISCOVERY_NAME_LEN],
        };
        out.class_len = copy_str(class_name, &mut out.class_name) as u16;
        out.name_len = copy_str(dev_name, &mut out.dev_name) as u16;
        if out.class_len as usize != class_name.len() {
            out.flags |= ELM_DEV_DISCOVERY_RECORD_FLAG_CLASS_TRUNCATED;
        }
        if out.name_len as usize != dev_name.len() {
            out.flags |= ELM_DEV_DISCOVERY_RECORD_FLAG_NAME_TRUNCATED;
        }
        out
    }
}

const DEVICE_PROVIDERS: [ElmKernelProviderSpec; 6] = [
    ElmKernelProviderSpec::new(
        "elm.dev",
        "device.discovered",
        "elm.dev.device.discovered@1",
        ELM_MGR_API_KIND_SUBSYSTEM,
        0,
        ELM_DEV_DISCOVERY_CAP_SNAPSHOT | ELM_DEV_DISCOVERY_CAP_INVOKE_QUERY,
        "device.discovered@1",
        FlowDirection::Source,
        FlowMode::Broadcast,
        ElmPortAccessPolicy::Internal,
        true,
        ELM_KERNEL_PROVIDER_FLAG_NONE,
        device_discovered_invoke,
        Some(device_discovered_snapshot),
        None,
    ),
    ElmKernelProviderSpec::new(
        "elm.dev",
        "device.claim",
        "elm.dev.device.claim@1",
        ELM_MGR_API_KIND_SUBSYSTEM,
        0,
        ELM_DEV_CLAIM_CAP_ACQUIRE
            | ELM_DEV_CLAIM_CAP_RELEASE
            | ELM_DEV_CLAIM_CAP_QUERY
            | ELM_DEV_CLAIM_CAP_SNAPSHOT,
        "device.claim@1",
        FlowDirection::Control,
        FlowMode::Exclusive,
        ElmPortAccessPolicy::Internal,
        true,
        ELM_KERNEL_PROVIDER_FLAG_NONE,
        device_claim_invoke,
        Some(device_claim_snapshot),
        Some(device_claim_revoke),
    ),
    ElmKernelProviderSpec::subsystem_todo(
        "elm.dev",
        "irq.event",
        "elm.dev.irq.event@1",
        "irq.event@1",
        FlowDirection::Source,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        false,
    ),
    ElmKernelProviderSpec::subsystem_todo(
        "elm.dev",
        "dma.buffer",
        "elm.dev.dma.buffer@1",
        "dma.buffer@1",
        FlowDirection::Duplex,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
    ),
    ElmKernelProviderSpec::subsystem_todo(
        "elm.dev",
        "mmio.window",
        "elm.dev.mmio.window@1",
        "mmio.window@1",
        FlowDirection::Duplex,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
    ),
    ElmKernelProviderSpec::subsystem_todo(
        "elm.dev",
        "io.block.submit",
        "elm.dev.io.block.submit@1",
        "io.block.submit@1",
        FlowDirection::Sink,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
    ),
];

pub fn providers() -> &'static [ElmKernelProviderSpec] {
    // TODO(elm): 将 IRQ、DMA、MMIO 和块 I/O 入口接到对应子系统。
    &DEVICE_PROVIDERS
}

fn device_discovered_invoke(frame: ElmCallFrame) -> ElmReplyFrame {
    if frame.opcode != ELM_DEV_DISCOVERY_OPCODE_QUERY {
        return ElmReplyFrame::empty(frame.binding_id, frame.call_id, ELM_CALL_STATUS_UNSUPPORTED);
    }
    if frame.payload_len != 0 || frame.flags != 0 || frame.reserved0 != 0 || frame.reserved1 != 0 {
        return ElmReplyFrame::empty(frame.binding_id, frame.call_id, ELM_CALL_STATUS_INVALID);
    }

    let mut payload = [0u8; ELM_FRAME_PAYLOAD_LEN];
    match write_device_discovery_snapshot(&mut payload) {
        Ok(len) => ElmReplyFrame::new(
            frame.binding_id,
            frame.call_id,
            ELM_CALL_STATUS_OK,
            &payload[..len],
        ),
        Err(status) => ElmReplyFrame::empty(frame.binding_id, frame.call_id, status),
    }
}

fn device_discovered_snapshot(out: &mut [u8]) -> Result<usize, i32> {
    write_device_discovery_snapshot(out)
}

fn device_claim_invoke(frame: ElmCallFrame) -> ElmReplyFrame {
    if frame.flags != 0 || frame.reserved0 != 0 || frame.reserved1 != 0 || frame.binding_id == 0 {
        return ElmReplyFrame::empty(frame.binding_id, frame.call_id, ELM_CALL_STATUS_INVALID);
    }
    let Some(request) = parse_device_claim_request(&frame) else {
        return ElmReplyFrame::empty(frame.binding_id, frame.call_id, ELM_CALL_STATUS_INVALID);
    };
    let result = match frame.opcode {
        ELM_DEV_CLAIM_OPCODE_ACQUIRE => match device_exists(&request) {
            Ok(true) => DEVICE_CLAIMS.lock().acquire(frame.binding_id, &request),
            Ok(false) => Err(ELM_CALL_STATUS_NOT_FOUND),
            Err(status) => Err(status),
        },
        ELM_DEV_CLAIM_OPCODE_RELEASE => DEVICE_CLAIMS.lock().release(frame.binding_id, &request),
        ELM_DEV_CLAIM_OPCODE_QUERY => DEVICE_CLAIMS.lock().query(&request),
        _ => Err(ELM_CALL_STATUS_UNSUPPORTED),
    };

    match result {
        Ok(claim) => {
            let reply = ElmDeviceClaimReply::from_claim(&claim, ELM_DEV_CLAIM_REPLY_FLAG_HELD);
            let mut payload = [0u8; core::mem::size_of::<ElmDeviceClaimReply>()];
            write_device_claim_reply(&mut payload, &reply);
            ElmReplyFrame::new(
                frame.binding_id,
                frame.call_id,
                ELM_CALL_STATUS_OK,
                &payload,
            )
        }
        Err(status) => ElmReplyFrame::empty(frame.binding_id, frame.call_id, status),
    }
}

fn device_claim_snapshot(out: &mut [u8]) -> Result<usize, i32> {
    write_device_claim_snapshot(out)
}

fn device_claim_revoke(binding: Option<BindingId>, _lease: Option<LeaseId>) {
    if let Some(binding) = binding {
        DEVICE_CLAIMS.lock().release_binding(binding.0);
    }
}

fn write_device_discovery_snapshot(out: &mut [u8]) -> Result<usize, i32> {
    let header_size = core::mem::size_of::<ElmDeviceDiscoveryHeader>();
    let record_size = core::mem::size_of::<ElmDeviceDiscoveryRecord>();
    if out.len() < header_size {
        return Err(ELM_MGR_STATUS_INVALID);
    }

    let functions = DEVICES.functions.try_list().ok_or(ELM_MGR_STATUS_BUSY)?;
    let total_count = functions.len();
    let record_capacity = (out.len() - header_size) / record_size;
    let record_count = total_count.min(record_capacity);
    let flags = if record_count < total_count {
        ELM_DEV_DISCOVERY_FLAG_TRUNCATED
    } else {
        0
    };

    let header = ElmDeviceDiscoveryHeader::new(record_count as u32, total_count as u32, flags);
    write_device_discovery_header(out, &header);
    for (index, function) in functions.iter().take(record_count).enumerate() {
        let record = ElmDeviceDiscoveryRecord::new(
            index as u64 + 1,
            function.class_id().as_str(),
            function.dev_name(),
        );
        let offset = header_size + index * record_size;
        write_device_discovery_record(&mut out[offset..offset + record_size], &record);
    }
    Ok(header_size + record_count * record_size)
}

fn write_device_claim_snapshot(out: &mut [u8]) -> Result<usize, i32> {
    let header_size = core::mem::size_of::<ElmDeviceClaimSnapshotHeader>();
    let record_size = core::mem::size_of::<ElmDeviceClaimRecord>();
    if out.len() < header_size {
        return Err(ELM_MGR_STATUS_INVALID);
    }

    let registry = DEVICE_CLAIMS.lock();
    let total_count = registry.claims.len();
    let record_capacity = (out.len() - header_size) / record_size;
    let record_count = total_count.min(record_capacity);
    let flags = if record_count < total_count {
        ELM_DEV_CLAIM_SNAPSHOT_FLAG_TRUNCATED
    } else {
        0
    };
    let header = ElmDeviceClaimSnapshotHeader::new(
        record_count as u32,
        total_count as u32,
        flags,
        registry.generation,
    );
    write_device_claim_snapshot_header(out, &header);
    for (index, claim) in registry.claims.iter().take(record_count).enumerate() {
        let record = ElmDeviceClaimRecord::from_claim(claim);
        let offset = header_size + index * record_size;
        write_device_claim_record(&mut out[offset..offset + record_size], &record);
    }
    Ok(header_size + record_count * record_size)
}

fn parse_device_claim_request(frame: &ElmCallFrame) -> Option<ElmDeviceClaimRequest> {
    if usize::from(frame.payload_len) != core::mem::size_of::<ElmDeviceClaimRequest>() {
        return None;
    }
    let payload = &frame.payload[..usize::from(frame.payload_len)];
    let request = ElmDeviceClaimRequest {
        abi_version: read_u16(payload, 0)?,
        flags: read_u16(payload, 2)?,
        class_len: read_u16(payload, 4)?,
        name_len: read_u16(payload, 6)?,
        class_name: read_fixed::<ELM_DEV_CLAIM_CLASS_LEN>(payload, 8)?,
        dev_name: read_fixed::<ELM_DEV_CLAIM_NAME_LEN>(payload, 8 + ELM_DEV_CLAIM_CLASS_LEN)?,
    };
    if request.abi_version != elm_model::ELM_CTL_ABI_VERSION
        || request.flags != ELM_DEV_CLAIM_FLAG_NONE
        || request.class_len == 0
        || request.name_len == 0
        || request.class_len as usize > ELM_DEV_CLAIM_CLASS_LEN
        || request.name_len as usize > ELM_DEV_CLAIM_NAME_LEN
        || request.class_str().is_none()
        || request.name_str().is_none()
    {
        return None;
    }
    Some(request)
}

fn device_exists(request: &ElmDeviceClaimRequest) -> Result<bool, i32> {
    let class_name = request.class_str().ok_or(ELM_CALL_STATUS_INVALID)?;
    let dev_name = request.name_str().ok_or(ELM_CALL_STATUS_INVALID)?;
    let functions = DEVICES.functions.try_list().ok_or(ELM_CALL_STATUS_BUSY)?;
    Ok(functions.iter().any(|function| {
        function.class_id().as_str() == class_name && function.dev_name() == dev_name
    }))
}

fn write_device_discovery_header(out: &mut [u8], header: &ElmDeviceDiscoveryHeader) {
    out[0..2].copy_from_slice(&header.abi_version.to_le_bytes());
    out[2..4].copy_from_slice(&header.record_entry_size.to_le_bytes());
    out[4..8].copy_from_slice(&header.record_count.to_le_bytes());
    out[8..12].copy_from_slice(&header.total_count.to_le_bytes());
    out[12..16].copy_from_slice(&header.flags.to_le_bytes());
    out[16..24].copy_from_slice(&header.generation.to_le_bytes());
}

fn write_device_discovery_record(out: &mut [u8], record: &ElmDeviceDiscoveryRecord) {
    out[0..8].copy_from_slice(&record.ordinal.to_le_bytes());
    out[8..10].copy_from_slice(&record.class_len.to_le_bytes());
    out[10..12].copy_from_slice(&record.name_len.to_le_bytes());
    out[12..16].copy_from_slice(&record.flags.to_le_bytes());
    out[16..16 + ELM_DEV_DISCOVERY_CLASS_LEN].copy_from_slice(&record.class_name);
    let name_offset = 16 + ELM_DEV_DISCOVERY_CLASS_LEN;
    out[name_offset..name_offset + ELM_DEV_DISCOVERY_NAME_LEN].copy_from_slice(&record.dev_name);
}

fn write_device_claim_snapshot_header(out: &mut [u8], header: &ElmDeviceClaimSnapshotHeader) {
    out[0..2].copy_from_slice(&header.abi_version.to_le_bytes());
    out[2..4].copy_from_slice(&header.record_entry_size.to_le_bytes());
    out[4..8].copy_from_slice(&header.record_count.to_le_bytes());
    out[8..12].copy_from_slice(&header.total_count.to_le_bytes());
    out[12..16].copy_from_slice(&header.flags.to_le_bytes());
    out[16..24].copy_from_slice(&header.generation.to_le_bytes());
}

fn write_device_claim_reply(out: &mut [u8], reply: &ElmDeviceClaimReply) {
    out[0..2].copy_from_slice(&reply.abi_version.to_le_bytes());
    out[2..4].copy_from_slice(&reply.record_entry_size.to_le_bytes());
    out[4..8].copy_from_slice(&reply.flags.to_le_bytes());
    out[8..16].copy_from_slice(&reply.owner_binding_id.to_le_bytes());
    out[16..24].copy_from_slice(&reply.owner_lease_id.to_le_bytes());
    out[24..26].copy_from_slice(&reply.class_len.to_le_bytes());
    out[26..28].copy_from_slice(&reply.name_len.to_le_bytes());
    out[28..32].copy_from_slice(&reply.reserved.to_le_bytes());
    out[32..32 + ELM_DEV_CLAIM_CLASS_LEN].copy_from_slice(&reply.class_name);
    let name_offset = 32 + ELM_DEV_CLAIM_CLASS_LEN;
    out[name_offset..name_offset + ELM_DEV_CLAIM_NAME_LEN].copy_from_slice(&reply.dev_name);
}

fn write_device_claim_record(out: &mut [u8], record: &ElmDeviceClaimRecord) {
    out[0..8].copy_from_slice(&record.owner_binding_id.to_le_bytes());
    out[8..16].copy_from_slice(&record.owner_lease_id.to_le_bytes());
    out[16..18].copy_from_slice(&record.class_len.to_le_bytes());
    out[18..20].copy_from_slice(&record.name_len.to_le_bytes());
    out[20..24].copy_from_slice(&record.flags.to_le_bytes());
    out[24..24 + ELM_DEV_CLAIM_CLASS_LEN].copy_from_slice(&record.class_name);
    let name_offset = 24 + ELM_DEV_CLAIM_CLASS_LEN;
    out[name_offset..name_offset + ELM_DEV_CLAIM_NAME_LEN].copy_from_slice(&record.dev_name);
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_fixed<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset + N)?.try_into().ok()
}

fn copy_str(value: &str, out: &mut [u8]) -> usize {
    let len = value.len().min(out.len());
    out[..len].copy_from_slice(&value.as_bytes()[..len]);
    len
}

impl ElmDeviceClaimRequest {
    fn class_bytes(&self) -> &[u8] {
        &self.class_name[..self.class_len as usize]
    }

    fn name_bytes(&self) -> &[u8] {
        &self.dev_name[..self.name_len as usize]
    }

    fn class_str(&self) -> Option<&str> {
        str::from_utf8(self.class_bytes()).ok()
    }

    fn name_str(&self) -> Option<&str> {
        str::from_utf8(self.name_bytes()).ok()
    }
}

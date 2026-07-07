//! 设备层导出的 ELM provider 规格。
//!
//! 这里声明设备、IRQ、DMA、MMIO 和块 I/O 暴露给 `elm-mgr` 的稳定入口。
//! 具体协议和真实执行逻辑属于设备层，ELM Core 只读取这些规格并转发调用。

use elm_model::{
    ELM_CALL_STATUS_INVALID, ELM_CALL_STATUS_OK, ELM_CALL_STATUS_UNSUPPORTED,
    ELM_FRAME_PAYLOAD_LEN, ELM_KERNEL_PROVIDER_FLAG_NONE, ELM_MGR_API_KIND_SUBSYSTEM,
    ELM_MGR_STATUS_BUSY, ELM_MGR_STATUS_INVALID, ElmCallFrame, ElmKernelProviderSpec,
    ElmPortAccessPolicy, ElmReplyFrame, FlowDirection, FlowMode,
};

use crate::dev::enumerate::DEVICES;

pub const ELM_DEV_DISCOVERY_OPCODE_QUERY: u32 = 1;
pub const ELM_DEV_DISCOVERY_CLASS_LEN: usize = 16;
pub const ELM_DEV_DISCOVERY_NAME_LEN: usize = 64;
pub const ELM_DEV_DISCOVERY_FLAG_TRUNCATED: u32 = 1 << 0;
pub const ELM_DEV_DISCOVERY_RECORD_FLAG_CLASS_TRUNCATED: u32 = 1 << 0;
pub const ELM_DEV_DISCOVERY_RECORD_FLAG_NAME_TRUNCATED: u32 = 1 << 1;
pub const ELM_DEV_DISCOVERY_CAP_SNAPSHOT: u64 = 1 << 0;
pub const ELM_DEV_DISCOVERY_CAP_INVOKE_QUERY: u64 = 1 << 1;

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
    ElmKernelProviderSpec::subsystem_todo(
        "elm.dev",
        "device.claim",
        "elm.dev.device.claim@1",
        "device.claim@1",
        FlowDirection::Control,
        FlowMode::Exclusive,
        ElmPortAccessPolicy::Internal,
        true,
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
    // TODO(elm): 将 claim、IRQ、DMA、MMIO 和块 I/O 入口接到对应子系统。
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

fn copy_str(value: &str, out: &mut [u8]) -> usize {
    let len = value.len().min(out.len());
    out[..len].copy_from_slice(&value.as_bytes()[..len]);
    len
}

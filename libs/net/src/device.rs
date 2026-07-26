//! 网络设备注册、boot key 与 IRQ 唤醒边界。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use spin::Mutex;

use crate::boot::NetDriverBootConfig;
use crate::buf::{
    CompletionBatch, NetBufPoolOwner, PacketBatch, RxRefillBatch, SharedNetBufPool, TxBatch,
};
use crate::queue::{
    NetQueueCaps, NetQueuePair, QueueFatalError, RxBudget, RxPollResult, RxRefillResult,
    TxReclaimResult, TxSubmitResult,
};
use crate::{NetDeviceId, QueuePairId};

static NEXT_DEVICE_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_DEVICE_HANDLE: AtomicU64 = AtomicU64::new(1);
static BOOT_CONFIG: Mutex<Option<NetDriverBootConfig>> = Mutex::new(None);
static REGISTRAR: Mutex<Option<&'static dyn NetDeviceRegistrar>> = Mutex::new(None);

/// IRQ handler 安装到 queue 的唤醒目标。
pub trait QueueWakeHandle: Send + Sync {
    fn wake(&self);
}

/// driver 与 IRQ handler 共享的唯一 queue 控制对象。
pub trait QueueIrqControl: Send + Sync {
    fn ack_and_mask(&self) -> bool;
    fn unmask(&self);
    fn set_waker(&self, waker: Arc<dyn QueueWakeHandle>) -> Result<(), QueueIrqError>;

    fn clear_waker(&self);
    fn stats(&self) -> QueueIrqStats;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueIrqError {
    WakerAlreadyInstalled,
    DeviceGone,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueIrqStats {
    pub irq_total: u64,
    pub irq_mask: u64,
    pub irq_unmask: u64,
}

pub const NET_QUEUE_CALL_RUST_ABI: &str = "fn(&mutnet::device::NetQueueCall)->i32";
pub const NET_QUEUE_CALL_STATUS_OK: i32 = 0;
pub const NET_QUEUE_CALL_STATUS_INVALID: i32 = -22;

pub const NET_QUEUE_OP_REFILL_RX: u32 = 1;
pub const NET_QUEUE_OP_POLL_RX: u32 = 2;
pub const NET_QUEUE_OP_RECLAIM_TX: u32 = 3;
pub const NET_QUEUE_OP_SUBMIT_TX: u32 = 4;
pub const NET_QUEUE_OP_HAS_PENDING: u32 = 5;
pub const NET_QUEUE_OP_QUIESCE: u32 = 6;

/// host 与 driver ELM 间一次同步 queue batch 调用的固定帧。
#[repr(C)]
pub struct NetQueueCall {
    pub struct_size: u16,
    pub opcode: u32,
    pub queue_id: QueuePairId,
    pub reserved0: u16,
    pub budget: RxBudget,
    pub refill_batch: *mut RxRefillBatch,
    pub packet_batch: *mut PacketBatch,
    pub completion_batch: *mut CompletionBatch,
    pub tx_batch: *mut TxBatch,
    pub tx_header_pool: *mut NetBufPoolOwner,
    pub rx_refill_result: RxRefillResult,
    pub rx_poll_result: RxPollResult,
    pub tx_reclaim_result: TxReclaimResult,
    pub tx_submit_result: TxSubmitResult,
    pub pending: bool,
    pub quiesce_result: Option<QueueFatalError>,
    pub reserved1: [u64; 2],
}

#[kernel_symbols::export]
impl NetQueueCall {
    pub fn new(opcode: u32, queue_id: QueuePairId) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u16,
            opcode,
            queue_id,
            reserved0: 0,
            budget: RxBudget {
                packets: 1,
                bytes: 1,
            },
            refill_batch: core::ptr::null_mut(),
            packet_batch: core::ptr::null_mut(),
            completion_batch: core::ptr::null_mut(),
            tx_batch: core::ptr::null_mut(),
            tx_header_pool: core::ptr::null_mut(),
            rx_refill_result: RxRefillResult {
                posted: 0,
                descriptor_starved: false,
                fatal: None,
            },
            rx_poll_result: RxPollResult {
                packets: 0,
                bytes: 0,
                ring_empty: true,
                descriptor_starved: false,
                fatal: None,
            },
            tx_reclaim_result: TxReclaimResult {
                completions: 0,
                descriptors: 0,
                ring_empty: true,
                fatal: None,
            },
            tx_submit_result: TxSubmitResult {
                packets: 0,
                descriptors: 0,
                bytes: 0,
                queue_full: false,
                fatal: None,
            },
            pending: false,
            quiesce_result: None,
            reserved1: [0; 2],
        }
    }

    #[kernel_symbols::export(
        name = "net.device.NetQueueCall.valid",
        contract = "kernel.net.queue-call-frame@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
    pub fn valid(&self, opcode: u32, queue_id: QueuePairId) -> bool {
        self.struct_size as usize == core::mem::size_of::<Self>()
            && self.opcode == opcode
            && self.queue_id == queue_id
            && self.reserved0 == 0
            && self.reserved1 == [0; 2]
    }
}

/// 固定到特定 ELM generation 的 queue export 描述；字符串已复制到常驻分配中。
pub struct PinnedNetQueueEndpoint {
    owner_cell: u64,
    owner_generation: u64,
    export_name: Box<str>,
    export_contract: Box<str>,
    export_version: u32,
    id: QueuePairId,
    caps: NetQueueCaps,
    tx_produces_rx_synchronously: bool,
}

#[kernel_symbols::export]
impl PinnedNetQueueEndpoint {
    #[kernel_symbols::export(
        name = "net.device.PinnedNetQueueEndpoint.current",
        contract = "kernel.net.queue-endpoint@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER
    )]
    pub fn current(
        export_name: &str,
        export_contract: &str,
        export_version: u32,
        id: QueuePairId,
        caps: NetQueueCaps,
        tx_produces_rx_synchronously: bool,
    ) -> Option<Self> {
        let context = elm_model::current_context()?;
        if export_name.is_empty()
            || export_contract.is_empty()
            || export_version == 0
            || elm_model::FlowContract::new(export_contract).is_err()
        {
            return None;
        }
        Some(Self {
            owner_cell: context.cell_id.0,
            owner_generation: context.generation.0,
            export_name: export_name.into(),
            export_contract: export_contract.into(),
            export_version,
            id,
            caps,
            tx_produces_rx_synchronously,
        })
    }

    pub const fn owner_cell(&self) -> u64 {
        self.owner_cell
    }

    pub const fn owner_generation(&self) -> u64 {
        self.owner_generation
    }

    pub fn export_name(&self) -> &str {
        &self.export_name
    }

    pub fn export_contract(&self) -> &str {
        &self.export_contract
    }

    pub const fn export_version(&self) -> u32 {
        self.export_version
    }

    pub const fn id(&self) -> QueuePairId {
        self.id
    }

    pub const fn caps(&self) -> NetQueueCaps {
        self.caps
    }

    pub const fn tx_produces_rx_synchronously(&self) -> bool {
        self.tx_produces_rx_synchronously
    }
}

pub enum NetQueueEndpoint {
    Integrated(Box<dyn NetQueuePair>),
    Pinned(PinnedNetQueueEndpoint),
}

impl NetQueueEndpoint {
    pub fn id(&self) -> QueuePairId {
        match self {
            Self::Integrated(queue) => queue.id(),
            Self::Pinned(queue) => queue.id(),
        }
    }

    pub fn caps(&self) -> NetQueueCaps {
        match self {
            Self::Integrated(queue) => queue.caps(),
            Self::Pinned(queue) => queue.caps(),
        }
    }
}

/// 一个 queue pair 连同其三个独立 pool owner 的原子注册单元。
pub struct NetQueueRegistration {
    pub id: QueuePairId,
    pub queue: NetQueueEndpoint,
    pub rx_pool: NetBufPoolOwner,
    pub tx_header_pool: NetBufPoolOwner,
    pub tx_payload_pool: SharedNetBufPool,
    pub socket_tx_pool: SharedNetBufPool,
    pub irq: Arc<dyn QueueIrqControl>,
}

#[kernel_symbols::export]
impl NetQueueRegistration {
    pub fn integrated_heap(
        queue: Box<dyn NetQueuePair>,
        rx_pool_count: usize,
        rx_buffer_size: usize,
        tx_header_pool_count: usize,
        tx_header_size: usize,
        tx_payload_pool_count: usize,
        tx_payload_size: usize,
    ) -> Result<Self, crate::buf::NetBufPoolError> {
        let id = queue.id();
        Ok(Self {
            id,
            queue: NetQueueEndpoint::Integrated(queue),
            rx_pool: crate::buf::NetBufPool::new_heap(rx_pool_count, rx_buffer_size)?,
            tx_header_pool: crate::buf::NetBufPool::new_heap(tx_header_pool_count, tx_header_size)?,
            tx_payload_pool: Arc::new(Mutex::new(crate::buf::NetBufPool::new_heap(
                tx_payload_pool_count,
                tx_payload_size,
            )?)),
            socket_tx_pool: Arc::new(Mutex::new(crate::buf::NetBufPool::new_heap(
                tx_payload_pool_count
                    .saturating_mul(crate::tuning::SOCKET_TX_POOL_DEPTH_MULTIPLIER),
                tx_payload_size,
            )?)),
            irq: Arc::new(SoftwareQueueIrq::new()),
        })
    }

    #[kernel_symbols::export(
        name = "net.device.NetQueueRegistration.pinned_heap",
        contract = "kernel.net.queue-endpoint@1",
        version = 1,
        capabilities = kernel_symbols::capability::ALLOCATOR_MEMORY | kernel_symbols::capability::DEVICE_DRIVER | kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn pinned_heap(
        endpoint: PinnedNetQueueEndpoint,
        rx_pool_count: usize,
        rx_buffer_size: usize,
        tx_header_pool_count: usize,
        tx_header_size: usize,
        tx_payload_pool_count: usize,
        tx_payload_size: usize,
    ) -> Result<Self, crate::buf::NetBufPoolError> {
        let id = endpoint.id();
        Ok(Self {
            id,
            queue: NetQueueEndpoint::Pinned(endpoint),
            rx_pool: crate::buf::NetBufPool::new_heap(rx_pool_count, rx_buffer_size)?,
            tx_header_pool: crate::buf::NetBufPool::new_heap(tx_header_pool_count, tx_header_size)?,
            tx_payload_pool: Arc::new(Mutex::new(crate::buf::NetBufPool::new_heap(
                tx_payload_pool_count,
                tx_payload_size,
            )?)),
            socket_tx_pool: Arc::new(Mutex::new(crate::buf::NetBufPool::new_heap(
                tx_payload_pool_count
                    .saturating_mul(crate::tuning::SOCKET_TX_POOL_DEPTH_MULTIPLIER),
                tx_payload_size,
            )?)),
            irq: Arc::new(SoftwareQueueIrq::new()),
        })
    }
}

struct SoftwareQueueIrq {
    pending: core::sync::atomic::AtomicBool,
    masked: core::sync::atomic::AtomicBool,
    waker: Mutex<Option<Arc<dyn QueueWakeHandle>>>,
    irq_total: AtomicU64,
    irq_mask: AtomicU64,
    irq_unmask: AtomicU64,
}

impl SoftwareQueueIrq {
    fn new() -> Self {
        Self {
            pending: core::sync::atomic::AtomicBool::new(false),
            masked: core::sync::atomic::AtomicBool::new(false),
            waker: Mutex::new(None),
            irq_total: AtomicU64::new(0),
            irq_mask: AtomicU64::new(0),
            irq_unmask: AtomicU64::new(0),
        }
    }
}

impl QueueIrqControl for SoftwareQueueIrq {
    fn ack_and_mask(&self) -> bool {
        if !self.pending.swap(false, Ordering::AcqRel) {
            return false;
        }
        self.masked.store(true, Ordering::Release);
        self.irq_mask.fetch_add(1, Ordering::Relaxed);
        true
    }

    fn unmask(&self) {
        self.pending.store(false, Ordering::Release);
        self.masked.store(false, Ordering::Release);
        self.irq_unmask.fetch_add(1, Ordering::Relaxed);
        if self.pending.load(Ordering::Acquire)
            && let Some(waker) = self.waker.lock().as_ref()
        {
            waker.wake();
        }
    }

    fn set_waker(&self, waker: Arc<dyn QueueWakeHandle>) -> Result<(), QueueIrqError> {
        let mut slot = self.waker.lock();
        if slot.is_some() {
            return Err(QueueIrqError::WakerAlreadyInstalled);
        }
        *slot = Some(waker);
        Ok(())
    }

    fn clear_waker(&self) {
        *self.waker.lock() = None;
    }

    fn stats(&self) -> QueueIrqStats {
        QueueIrqStats {
            irq_total: self.irq_total.load(Ordering::Relaxed),
            irq_mask: self.irq_mask.load(Ordering::Relaxed),
            irq_unmask: self.irq_unmask.load(Ordering::Relaxed),
        }
    }
}

/// driver 完成协商后一次性交给 kernel 的设备资源。
pub struct NetDeviceRegistration {
    handle: NetDeviceHandle,
    pub id: NetDeviceId,
    pub name: Box<str>,
    pub mac_address: [u8; 6],
    pub mtu: u32,
    pub running: bool,
    pub queues: Box<[NetQueueRegistration]>,
}

#[kernel_symbols::export]
impl NetDeviceRegistration {
    #[kernel_symbols::export(
        name = "net.device.NetDeviceRegistration.new",
        contract = "kernel.net.device-registration@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER
    )]
    pub fn new(
        name: Box<str>,
        mac_address: [u8; 6],
        mtu: u32,
        running: bool,
        queues: Box<[NetQueueRegistration]>,
    ) -> Self {
        let raw = NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed);
        assert!(raw != 0, "NetDeviceId 已耗尽");
        let raw_handle = NEXT_DEVICE_HANDLE.fetch_add(1, Ordering::Relaxed);
        assert!(raw_handle != 0, "NetDeviceHandle 已耗尽");
        Self {
            handle: NetDeviceHandle(raw_handle),
            id: NetDeviceId(raw),
            name,
            mac_address,
            mtu,
            running,
            queues,
        }
    }

    pub const fn handle(&self) -> NetDeviceHandle {
        self.handle
    }
}

/// kernel 成功接管设备后的不透明句柄。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct NetDeviceHandle(pub u64);

/// 同步 remove 完成后交还给 driver 的令牌。
pub struct NetDeviceTeardown {
    pub handle: NetDeviceHandle,
}

/// 控制面按需生成的只读设备快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetDeviceSnapshot {
    pub id: NetDeviceId,
    pub name: Box<str>,
    pub mac_address: [u8; 6],
    pub mtu: u32,
    pub queue_pairs: u16,
    pub running: bool,
    pub stats: NetDeviceStats,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetDeviceStats {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errors: u64,
    pub rx_dropped: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errors: u64,
    pub tx_dropped: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetStat {
    pub device: NetDeviceId,
    pub queue: QueuePairId,
    pub key: &'static str,
    pub value: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetDeviceRegisterErrorKind {
    RegistrarNotReady,
    InvalidRegistration,
    ResourceExhausted,
}

/// 注册失败必须原样返还全部 ownership。
pub struct NetDeviceRegisterError {
    pub kind: NetDeviceRegisterErrorKind,
    pub registration: NetDeviceRegistration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetDeviceRemoveError {
    NoDevice,
    Busy,
    AlreadyRemoving,
}

/// 由 kernel 实现的设备接管入口。
pub trait NetDeviceRegistrar: Send + Sync {
    fn register_device(
        &self,
        registration: NetDeviceRegistration,
    ) -> Result<NetDeviceHandle, NetDeviceRegisterError>;

    fn begin_remove(
        &self,
        handle: NetDeviceHandle,
    ) -> Result<NetDeviceTeardown, NetDeviceRemoveError>;

    fn snapshot_devices(&self) -> Vec<NetDeviceSnapshot>;

    fn snapshot_stats(&self) -> Vec<NetStat>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallNetRuntimeError {
    AlreadyInstalled,
}

/// 在任何网络 PnP probe 前一次性安装 boot key 与 kernel registrar。
pub fn install_net_runtime(
    config: NetDriverBootConfig,
    registrar: &'static dyn NetDeviceRegistrar,
) -> Result<(), InstallNetRuntimeError> {
    let mut config_slot = BOOT_CONFIG.lock();
    let mut registrar_slot = REGISTRAR.lock();
    if config_slot.is_some() || registrar_slot.is_some() {
        return Err(InstallNetRuntimeError::AlreadyInstalled);
    }
    *config_slot = Some(config);
    *registrar_slot = Some(registrar);
    Ok(())
}

#[kernel_symbols::export(
    name = "net.device.boot_config",
    contract = "kernel.net.driver-boot-config@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER
)]
pub fn boot_config() -> Option<NetDriverBootConfig> {
    *BOOT_CONFIG.lock()
}

#[kernel_symbols::export(
    name = "net.device.register_device",
    contract = "kernel.net.device-registration@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER | kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn register_device(
    registration: NetDeviceRegistration,
) -> Result<NetDeviceHandle, NetDeviceRegisterError> {
    let registrar = *REGISTRAR.lock();
    let Some(registrar) = registrar else {
        return Err(NetDeviceRegisterError {
            kind: NetDeviceRegisterErrorKind::RegistrarNotReady,
            registration,
        });
    };
    let handle = registration.handle();
    let tracked = kernel_symbols::track_owned_resource(
        kernel_symbols::KERNEL_SYMBOL_RESOURCE_KIND_DEVICE,
        handle.0,
        NET_DEVICE_RESOURCE_OPS,
    );
    if tracked == kernel_symbols::KERNEL_SYMBOL_RESOURCE_STATUS_FAILED {
        return Err(NetDeviceRegisterError {
            kind: NetDeviceRegisterErrorKind::ResourceExhausted,
            registration,
        });
    }
    match registrar.register_device(registration) {
        Ok(handle) => Ok(handle),
        Err(error) => {
            if tracked == kernel_symbols::KERNEL_SYMBOL_RESOURCE_STATUS_TRACKED {
                let _ = kernel_symbols::untrack_owned_resource(
                    kernel_symbols::KERNEL_SYMBOL_RESOURCE_KIND_DEVICE,
                    handle.0,
                );
            }
            Err(error)
        }
    }
}

#[kernel_symbols::export(
    name = "net.device.begin_remove",
    contract = "kernel.net.device-registration@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER | kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn begin_remove(handle: NetDeviceHandle) -> Result<NetDeviceTeardown, NetDeviceRemoveError> {
    let registrar = *REGISTRAR.lock();
    let Some(registrar) = registrar else {
        return Err(NetDeviceRemoveError::NoDevice);
    };
    let result = registrar.begin_remove(handle);
    if result.is_ok() {
        let _ = kernel_symbols::untrack_owned_resource(
            kernel_symbols::KERNEL_SYMBOL_RESOURCE_KIND_DEVICE,
            handle.0,
        );
    }
    result
}

const NET_DEVICE_RESOURCE_OPS: kernel_symbols::KernelSymbolOwnedResourceOpsV1 =
    kernel_symbols::KernelSymbolOwnedResourceOpsV1::new(
        suspend_net_device_resource,
        resume_net_device_resource,
        quiesce_net_device_resource,
        cancel_net_device_resource,
        drain_net_device_resource,
        release_net_device_resource,
    );

fn suspend_net_device_resource(_owner: u64, _generation: u64, _handle: u64) -> Result<(), i32> {
    // v1 不承诺暂停后保留 queue 状态；拒绝 pause 比让 worker 继续进入暂停镜像更安全。
    Err(-16)
}

fn resume_net_device_resource(_owner: u64, _generation: u64, _handle: u64) -> Result<(), i32> {
    Ok(())
}

fn quiesce_net_device_resource(_owner: u64, _generation: u64, handle: u64) -> Result<(), i32> {
    let _ = handle;
    Ok(())
}

fn cancel_net_device_resource(_owner: u64, _generation: u64, _handle: u64) -> Result<(), i32> {
    Ok(())
}

fn drain_net_device_resource(_owner: u64, _generation: u64, handle: u64) -> Result<(), i32> {
    let _ = handle;
    Ok(())
}

fn release_net_device_resource(_owner: u64, _generation: u64, handle: u64) -> Result<(), i32> {
    let _ = handle;
    Ok(())
}

/// registrar 尚未安装时返回空列表，供早期 procfs/sysfs/netlink 安全读取。
pub fn snapshot_devices() -> Vec<NetDeviceSnapshot> {
    let registrar = *REGISTRAR.lock();
    registrar
        .map(NetDeviceRegistrar::snapshot_devices)
        .unwrap_or_default()
}

pub fn snapshot_stats() -> Vec<NetStat> {
    let registrar = *REGISTRAR.lock();
    registrar
        .map(NetDeviceRegistrar::snapshot_stats)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_call_frame_rejects_stale_or_corrupt_prefix() {
        let queue = QueuePairId(3);
        let mut frame = NetQueueCall::new(NET_QUEUE_OP_POLL_RX, queue);
        assert!(frame.valid(NET_QUEUE_OP_POLL_RX, queue));
        frame.struct_size = frame.struct_size.saturating_add(1);
        assert!(!frame.valid(NET_QUEUE_OP_POLL_RX, queue));
        frame.struct_size = core::mem::size_of::<NetQueueCall>() as u16;
        frame.reserved1[0] = 1;
        assert!(!frame.valid(NET_QUEUE_OP_POLL_RX, queue));
    }

    #[test]
    fn pinned_queue_endpoint_requires_elm_context() {
        assert!(
            PinnedNetQueueEndpoint::current(
                "test.queue",
                "test.queue@1",
                1,
                QueuePairId(0),
                NetQueueCaps {
                    queue_size: 16,
                    scatter_gather: false,
                    max_tx_descriptors: 1,
                    max_rx_batch: 32,
                    max_tx_batch: 32,
                    tx_checksum: false,
                    udp_segmentation: false,
                    max_udp_segments: 0,
                },
                false,
            )
            .is_none()
        );
    }
}

//! VirtIO-Net 批量队列驱动。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering, fence};

use spin::Mutex;

use net::QueuePairId;
use net::buf::{
    CompletionBatch, NetBufLease, NetBufPool, NetBufPoolOwner, NetBufStorage, PacketBatch,
    PacketChain, PacketMetadata, RxRefillBatch, TxBatch, TxPacket,
};
use net::device::{
    NetDeviceHandle, NetDeviceRegistration, NetQueueRegistration, QueueIrqControl, QueueIrqError,
    QueueIrqStats, QueueWakeHandle,
};
use net::queue::{
    NetQueueCaps, NetQueuePair, QueueFatalError, RxBudget, RxPollResult, RxRefillResult,
    TxReclaimResult, TxSubmitResult,
};

use crate::dev::dma::{DmaBuffer, DmaContext, DmaDirection, DmaSyncHandle};
use crate::dev::irq::{self, IrqError, IrqHandler, IrqLine, IrqStatus};
use crate::dev::naming::StableNameAllocator;
use crate::dev::net::NetFunction;
use crate::dev::pci::{
    PciDevice, PciInfo, PciMsiPnpResource, PciMsixError, PciMsixPnpResource, PciMsixSet,
};
use crate::dev::platform::{PlatformDeviceInfo, PlatformIrqRegistrationError};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDependency, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use crate::dev::virtio::{
    VIRTIO_F_VERSION_1, VIRTIO_PCI_FUNCTION_NETWORK, VIRTIO_PCI_RESET_SPIN_LIMIT,
    VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER, VIRTIO_STATUS_DRIVER_OK, VIRTIO_STATUS_FAILED,
    VIRTIO_STATUS_FEATURES_OK, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE, VirtioPciTransport,
    choose_split_queue_size, parse_virtio_pci_caps,
};
use crate::dev::virtio_mmio::{self, VirtioMmioTransport};

static NET_IFACE_NAMES: StableNameAllocator = StableNameAllocator::new("eth");

const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const VIRTIO_NET_F_CTRL_VQ: u64 = 1 << 17;
const VIRTIO_NET_F_MQ: u64 = 1 << 22;
const VIRTIO_NET_F_RSS: u64 = 1 << 60;
const NET_CFG_MAC: usize = 0;
const NET_CFG_STATUS: usize = 6;
const NET_CFG_MAX_VQ_PAIRS: usize = 8;
const NET_CFG_RSS_MAX_KEY_SIZE: usize = 17;
const NET_CFG_RSS_MAX_TABLE_LEN: usize = 18;
const NET_CFG_RSS_SUPPORTED_HASH_TYPES: usize = 20;
const VIRTIO_NET_STATUS_LINK_UP: u16 = 1;
const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;
const QUEUE_LIMIT: u16 = 256;
const QUEUE_LIMIT_USIZE: usize = QUEUE_LIMIT as usize;
const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;
const RX_DESCRIPTOR_OFFSET: u16 = 116;
const RX_FRAME_OFFSET: u16 = 128;
const RX_DESCRIPTOR_LEN: u32 = 4096 - 116;
const VIRTIO_HEADER_LEN: u16 = 12;
const VIRTIO_MMIO_DEVICE_ID_NETWORK: u32 = 1;
const VIRTIO_NET_CTRL_ACK_OK: u8 = 0;
const VIRTIO_NET_CTRL_ACK_ERR: u8 = 1;
const VIRTIO_NET_CTRL_MQ: u8 = 4;
const VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET: u8 = 0;
const VIRTIO_NET_CTRL_MQ_RSS_CONFIG: u8 = 1;
const VIRTIO_NET_RSS_HASH_TYPES: u32 = 0x3f;
const VIRTIO_NET_RSS_TABLE_LEN: u16 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MqRssFallback {
    MissingFeatures,
    TooFewQueuePairs,
    MsixUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MqRssPlan {
    Single(MqRssFallback),
    Multi { pairs: u16 },
}

fn plan_mq_rss(
    offered: u64,
    active_cpus: u8,
    device_pairs: u16,
    available_msix_vectors: u16,
) -> MqRssPlan {
    let required = VIRTIO_NET_F_CTRL_VQ | VIRTIO_NET_F_MQ | VIRTIO_NET_F_RSS;
    if offered & required != required {
        return MqRssPlan::Single(MqRssFallback::MissingFeatures);
    }
    let pairs = device_pairs.min(u16::from(active_cpus)).min(8);
    if pairs < 2 {
        return MqRssPlan::Single(MqRssFallback::TooFewQueuePairs);
    }
    if available_msix_vectors < pairs + 1 {
        return MqRssPlan::Single(MqRssFallback::MsixUnavailable);
    }
    MqRssPlan::Multi { pairs }
}

trait NetTransport: Send + Sync {
    fn reset(&self) -> bool;
    fn status(&self) -> u32;
    fn set_status(&self, status: u32);
    fn add_status(&self, status: u32) {
        self.set_status(self.status() | status);
    }
    fn device_features(&self) -> u64;
    fn set_driver_features(&self, features: u64);
    fn select_queue(&self, index: u16);
    fn selected_queue_size(&self) -> u16;
    fn set_selected_queue_size(&self, size: u16);
    fn set_config_msix_vector(&self, vector: u16) -> Result<(), &'static str>;
    fn set_selected_queue_msix_vector(&self, vector: u16) -> Result<(), &'static str>;
    fn set_selected_queue_addresses(&self, desc: u64, avail: u64, used: u64);
    fn enable_selected_queue(&self);
    fn selected_queue_notify_token(&self, index: u16) -> Result<usize, &'static str>;
    fn notify_queue(&self, token: usize, index: u16);
    fn ack_interrupt(&self) -> bool;
    fn read_config_u8(&self, offset: usize) -> Option<u8>;
    fn read_config_u16(&self, offset: usize) -> Option<u16>;
    fn read_config_u32(&self, offset: usize) -> Option<u32>;
}

struct PciNetTransport(VirtioPciTransport);

impl NetTransport for PciNetTransport {
    fn reset(&self) -> bool {
        self.0.reset_wait(VIRTIO_PCI_RESET_SPIN_LIMIT)
    }
    fn status(&self) -> u32 {
        u32::from(self.0.status())
    }
    fn set_status(&self, status: u32) {
        self.0.set_status(status as u8);
    }
    fn device_features(&self) -> u64 {
        self.0.device_features()
    }
    fn set_driver_features(&self, features: u64) {
        self.0.set_driver_features(features);
    }
    fn select_queue(&self, index: u16) {
        self.0.select_queue(index);
    }
    fn selected_queue_size(&self) -> u16 {
        self.0.selected_queue_size()
    }
    fn set_selected_queue_size(&self, size: u16) {
        self.0.set_selected_queue_size(size);
    }
    fn set_config_msix_vector(&self, vector: u16) -> Result<(), &'static str> {
        self.0
            .set_config_msix_vector(vector)
            .map_err(|_| "virtio-net config MSI-X vector 被拒绝")
    }
    fn set_selected_queue_msix_vector(&self, vector: u16) -> Result<(), &'static str> {
        self.0
            .set_selected_queue_msix_vector(vector)
            .map_err(|_| "virtio-net queue MSI-X vector 被拒绝")
    }
    fn set_selected_queue_addresses(&self, desc: u64, avail: u64, used: u64) {
        self.0.set_selected_queue_addresses(desc, avail, used);
    }
    fn enable_selected_queue(&self) {
        self.0.enable_selected_queue();
    }
    fn selected_queue_notify_token(&self, _index: u16) -> Result<usize, &'static str> {
        self.0
            .selected_queue_notify_addr()
            .map_err(|_| "virtio-net PCI notify 地址非法")
    }
    fn notify_queue(&self, token: usize, index: u16) {
        self.0.notify_queue(token, index);
    }
    fn ack_interrupt(&self) -> bool {
        self.0.isr_status() != 0
    }
    fn read_config_u8(&self, offset: usize) -> Option<u8> {
        let config = self.0.caps().device?;
        config
            .covers(offset, 1)
            .then(|| unsafe { read_volatile((config.vaddr + offset) as *const u8) })
    }
    fn read_config_u16(&self, offset: usize) -> Option<u16> {
        let config = self.0.caps().device?;
        config
            .covers(offset, 2)
            .then(|| unsafe { read_volatile((config.vaddr + offset) as *const u16) })
    }
    fn read_config_u32(&self, offset: usize) -> Option<u32> {
        let config = self.0.caps().device?;
        config
            .covers(offset, 4)
            .then(|| unsafe { read_volatile((config.vaddr + offset) as *const u32) })
    }
}

struct MmioNetTransport {
    inner: Box<dyn VirtioMmioTransport>,
}

impl NetTransport for MmioNetTransport {
    fn reset(&self) -> bool {
        self.inner.write_status(0);
        for _ in 0..VIRTIO_PCI_RESET_SPIN_LIMIT {
            if self.inner.read_status() == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }
    fn status(&self) -> u32 {
        self.inner.read_status()
    }
    fn set_status(&self, status: u32) {
        self.inner.write_status(status);
    }
    fn device_features(&self) -> u64 {
        self.inner.read_device_features()
    }
    fn set_driver_features(&self, features: u64) {
        self.inner.write_driver_features(features);
    }
    fn select_queue(&self, index: u16) {
        self.inner.select_queue(index);
    }
    fn selected_queue_size(&self) -> u16 {
        self.inner.read_queue_max_size().min(u32::from(u16::MAX)) as u16
    }
    fn set_selected_queue_size(&self, size: u16) {
        self.inner.write_queue_size(u32::from(size));
    }
    fn set_config_msix_vector(&self, _vector: u16) -> Result<(), &'static str> {
        Err("virtio-mmio 不支持 MSI-X")
    }
    fn set_selected_queue_msix_vector(&self, _vector: u16) -> Result<(), &'static str> {
        Err("virtio-mmio 不支持 MSI-X")
    }
    fn set_selected_queue_addresses(&self, desc: u64, avail: u64, used: u64) {
        self.inner.configure_queue_addresses(desc, avail, used);
    }
    fn enable_selected_queue(&self) {
        self.inner.enable_queue();
    }
    fn selected_queue_notify_token(&self, index: u16) -> Result<usize, &'static str> {
        Ok(index as usize)
    }
    fn notify_queue(&self, _token: usize, index: u16) {
        self.inner.notify_queue(u32::from(index));
    }
    fn ack_interrupt(&self) -> bool {
        let status = self.inner.read_interrupt_status();
        if status == 0 {
            return false;
        }
        self.inner.acknowledge_interrupt(status);
        true
    }
    fn read_config_u8(&self, offset: usize) -> Option<u8> {
        Some(unsafe { read_volatile((self.inner.base() + 0x100 + offset) as *const u8) })
    }
    fn read_config_u16(&self, offset: usize) -> Option<u16> {
        Some(unsafe { read_volatile((self.inner.base() + 0x100 + offset) as *const u16) })
    }
    fn read_config_u32(&self, offset: usize) -> Option<u32> {
        Some(unsafe { read_volatile((self.inner.base() + 0x100 + offset) as *const u32) })
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct VirtqAvail {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_LIMIT_USIZE],
    used_event: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElem; QUEUE_LIMIT_USIZE],
    avail_event: u16,
}

struct SplitQueue {
    desc_dma: DmaBuffer,
    avail_dma: DmaBuffer,
    used_dma: DmaBuffer,
    desc: *mut VirtqDesc,
    avail: *mut VirtqAvail,
    used: *mut VirtqUsed,
    size: u16,
    last_used: u16,
    notify_addr: usize,
}

unsafe impl Send for SplitQueue {}

impl SplitQueue {
    fn used_index(&self) -> u16 {
        self.used_dma.sync_for_cpu();
        fence(Ordering::Acquire);
        // SAFETY: used ring 是设备拥有的稳定 DMA 映射。
        unsafe { read_volatile(&(*self.used).idx) }
    }

    fn push_available(&mut self, descriptor: u16) {
        // SAFETY: queue 只由唯一 NetWorker 修改 avail idx/ring。
        unsafe {
            let index = read_volatile(&(*self.avail).idx);
            write_volatile(
                &mut (*self.avail).ring[index as usize % self.size as usize],
                descriptor,
            );
            fence(Ordering::Release);
            write_volatile(&mut (*self.avail).idx, index.wrapping_add(1));
        }
    }
}

fn setup_queue_with_minimum(
    transport: &dyn NetTransport,
    dma_context: DmaContext,
    index: u16,
    msix_vector: Option<u16>,
    minimum: u16,
) -> Result<SplitQueue, &'static str> {
    transport.select_queue(index);
    let maximum = transport.selected_queue_size();
    if maximum < minimum {
        return Err("virtio-net queue 小于最小容量");
    }
    let size = choose_split_queue_size(maximum, Some(QUEUE_LIMIT))
        .map_err(|_| "virtio-net queue size 非法")?;
    if !(minimum..=QUEUE_LIMIT).contains(&size) {
        return Err("virtio-net queue size 超出支持范围");
    }
    transport.set_selected_queue_size(size);
    if let Some(vector) = msix_vector {
        transport.set_selected_queue_msix_vector(vector)?;
    }
    let desc_dma = DmaBuffer::page_in(dma_context, DmaDirection::ToDevice)?;
    let avail_dma = DmaBuffer::page_in(dma_context, DmaDirection::ToDevice)?;
    let used_dma = DmaBuffer::page_in(dma_context, DmaDirection::FromDevice)?;
    let desc = desc_dma.vaddr() as *mut VirtqDesc;
    let avail = avail_dma.vaddr() as *mut VirtqAvail;
    let used = used_dma.vaddr() as *mut VirtqUsed;
    // worker 安装并首次 refill 前保持 completion IRQ 关闭。
    unsafe { write_volatile(&mut (*avail).flags, VIRTQ_AVAIL_F_NO_INTERRUPT) };
    desc_dma.sync_for_device();
    avail_dma.sync_for_device();
    transport.set_selected_queue_addresses(
        desc_dma.dma_addr() as u64,
        avail_dma.dma_addr() as u64,
        used_dma.dma_addr() as u64,
    );
    let notify_addr = transport.selected_queue_notify_token(index)?;
    transport.enable_selected_queue();
    Ok(SplitQueue {
        desc_dma,
        avail_dma,
        used_dma,
        desc,
        avail,
        used,
        size,
        last_used: 0,
        notify_addr,
    })
}

fn setup_data_queue(
    transport: &dyn NetTransport,
    dma_context: DmaContext,
    index: u16,
    msix_vector: Option<u16>,
) -> Result<SplitQueue, &'static str> {
    setup_queue_with_minimum(transport, dma_context, index, msix_vector, 16)
}

fn setup_control_queue(
    transport: &dyn NetTransport,
    dma_context: DmaContext,
    index: u16,
    msix_vector: u16,
) -> Result<SplitQueue, &'static str> {
    setup_queue_with_minimum(transport, dma_context, index, Some(msix_vector), 4)
}

fn submit_control_command(
    transport: &dyn NetTransport,
    queue: &mut SplitQueue,
    dma_context: DmaContext,
    queue_index: u16,
    class: u8,
    command: u8,
    payload: &[u8],
) -> Result<(), &'static str> {
    if queue.size < 3 || payload.len() + 3 > 4096 {
        return Err("virtio-net control command 过大");
    }
    let mut buffer = DmaBuffer::page_in(dma_context, DmaDirection::Bidirectional)?;
    let payload_offset = 2usize;
    let ack_offset = payload_offset + payload.len();
    let bytes = buffer.as_mut_slice();
    bytes[0] = class;
    bytes[1] = command;
    bytes[payload_offset..ack_offset].copy_from_slice(payload);
    bytes[ack_offset] = VIRTIO_NET_CTRL_ACK_ERR;

    let base = buffer.dma_addr() as u64;
    // SAFETY: control queue 在初始化期独占，descriptor 0..2 已由 queue size 校验。
    unsafe {
        write_volatile(
            queue.desc.add(0),
            VirtqDesc {
                addr: base,
                len: 2,
                flags: VIRTQ_DESC_F_NEXT,
                next: 1,
            },
        );
        write_volatile(
            queue.desc.add(1),
            VirtqDesc {
                addr: base + payload_offset as u64,
                len: payload.len() as u32,
                flags: VIRTQ_DESC_F_NEXT,
                next: 2,
            },
        );
        write_volatile(
            queue.desc.add(2),
            VirtqDesc {
                addr: base + ack_offset as u64,
                len: 1,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        );
    }
    buffer.sync_for_device();
    queue.desc_dma.sync_for_device();
    queue.push_available(0);
    queue.avail_dma.sync_for_device();
    transport.notify_queue(queue.notify_addr, queue_index);

    for _ in 0..VIRTIO_PCI_RESET_SPIN_LIMIT {
        if queue.used_index() != queue.last_used {
            queue.last_used = queue.last_used.wrapping_add(1);
            buffer.sync_for_cpu();
            return (buffer.as_slice()[ack_offset] == VIRTIO_NET_CTRL_ACK_OK)
                .then_some(())
                .ok_or("virtio-net control command 被设备拒绝");
        }
        core::hint::spin_loop();
    }
    Err("virtio-net control command 超时")
}

struct TxPending {
    _header: NetBufLease,
    packet: TxPacket,
    descriptors: [u16; 8],
    descriptor_count: u8,
}

fn tx_descriptor_count(payload_fragments: usize) -> Option<usize> {
    let total = payload_fragments.checked_add(1)?;
    (payload_fragments != 0 && total <= 8).then_some(total)
}

struct VirtioNetQueuePair {
    id: QueuePairId,
    transport: Arc<dyn NetTransport>,
    rx: SplitQueue,
    tx: SplitQueue,
    rx_pending: Box<[Option<NetBufLease>]>,
    rx_free: Vec<u16>,
    tx_pending: Box<[Option<TxPending>]>,
    tx_free: Vec<u16>,
    quiesced: bool,
}

unsafe impl Send for VirtioNetQueuePair {}

impl VirtioNetQueuePair {
    fn rx_queue_index(&self) -> u16 {
        self.id.0 * 2
    }

    fn tx_queue_index(&self) -> u16 {
        self.rx_queue_index() + 1
    }

    fn new(
        id: QueuePairId,
        transport: Arc<dyn NetTransport>,
        rx: SplitQueue,
        tx: SplitQueue,
    ) -> Self {
        let rx_size = rx.size as usize;
        let tx_size = tx.size as usize;
        let mut rx_free = Vec::with_capacity(rx_size);
        let mut tx_free = Vec::with_capacity(tx_size);
        for id in (0..rx.size).rev() {
            rx_free.push(id);
        }
        for id in (0..tx.size).rev() {
            tx_free.push(id);
        }
        Self {
            id,
            transport,
            rx,
            tx,
            rx_pending: (0..rx_size)
                .map(|_| None)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            rx_free,
            tx_pending: (0..tx_size)
                .map(|_| None)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            tx_free,
            quiesced: false,
        }
    }

    fn write_descriptor(
        queue: &mut SplitQueue,
        id: u16,
        address: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) {
        // SAFETY: descriptor id 来自该 queue 的唯一 free list。
        unsafe {
            write_volatile(
                queue.desc.add(id as usize),
                VirtqDesc {
                    addr: address,
                    len,
                    flags,
                    next,
                },
            );
        }
    }
}

impl NetQueuePair for VirtioNetQueuePair {
    fn id(&self) -> QueuePairId {
        self.id
    }

    fn caps(&self) -> NetQueueCaps {
        NetQueueCaps {
            queue_size: self.rx.size.min(self.tx.size),
            scatter_gather: true,
            max_tx_descriptors: 8,
            max_rx_batch: 32,
            max_tx_batch: 32,
        }
    }

    fn refill_rx_batch(&mut self, batch: &mut RxRefillBatch) -> RxRefillResult {
        if self.quiesced {
            return RxRefillResult {
                posted: 0,
                descriptor_starved: false,
                fatal: Some(QueueFatalError::DeviceGone),
            };
        }
        let original_len = batch.len();
        let mut posted = 0u16;
        for index in 0..original_len {
            let Some(descriptor) = self.rx_free.pop() else {
                break;
            };
            let Some(lease) = batch.take(index) else {
                self.rx_free.push(descriptor);
                continue;
            };
            let address = match lease.dma_addr() {
                Ok(Some(address)) if lease.data_offset() == RX_DESCRIPTOR_OFFSET => address,
                _ => {
                    self.rx_free.push(descriptor);
                    batch.put(index, lease).unwrap_or_else(|_| unreachable!());
                    return RxRefillResult {
                        posted,
                        descriptor_starved: false,
                        fatal: Some(QueueFatalError::DmaFault),
                    };
                }
            };
            if lease.sync_for_device().is_err() {
                self.rx_free.push(descriptor);
                batch.put(index, lease).unwrap_or_else(|_| unreachable!());
                return RxRefillResult {
                    posted,
                    descriptor_starved: false,
                    fatal: Some(QueueFatalError::DmaFault),
                };
            }
            Self::write_descriptor(
                &mut self.rx,
                descriptor,
                address,
                RX_DESCRIPTOR_LEN,
                VIRTQ_DESC_F_WRITE,
                0,
            );
            self.rx_pending[descriptor as usize] = Some(lease);
            self.rx.push_available(descriptor);
            posted += 1;
        }
        if posted != 0 {
            self.rx.desc_dma.sync_for_device();
            self.rx.avail_dma.sync_for_device();
            self.transport
                .notify_queue(self.rx.notify_addr, self.rx_queue_index());
        }
        RxRefillResult {
            posted,
            descriptor_starved: self.rx_free.is_empty() && !batch.is_empty(),
            fatal: None,
        }
    }

    fn poll_rx_batch(&mut self, budget: RxBudget, out: &mut PacketBatch) -> RxPollResult {
        if self.quiesced {
            return RxPollResult {
                packets: 0,
                bytes: 0,
                ring_empty: true,
                descriptor_starved: false,
                fatal: Some(QueueFatalError::DeviceGone),
            };
        }
        let used_index = self.rx.used_index();
        let mut packets = 0u16;
        let mut bytes = 0u32;
        let mut fatal = None;
        while self.rx.last_used != used_index && packets < budget.packets && packets < 32 {
            let element = unsafe {
                read_volatile(
                    &(*self.rx.used).ring[self.rx.last_used as usize % self.rx.size as usize],
                )
            };
            let descriptor = element.id as usize;
            let frame_len = element.len.saturating_sub(u32::from(VIRTIO_HEADER_LEN));
            if packets != 0 && bytes.saturating_add(frame_len) > budget.bytes {
                break;
            }
            self.rx.last_used = self.rx.last_used.wrapping_add(1);
            if descriptor >= self.rx_pending.len() {
                fatal = Some(QueueFatalError::RingCorrupt);
                break;
            }
            let Some(mut lease) = self.rx_pending[descriptor].take() else {
                fatal = Some(QueueFatalError::RingCorrupt);
                break;
            };
            self.rx_free.push(descriptor as u16);
            if !(u32::from(VIRTIO_HEADER_LEN)..=RX_DESCRIPTOR_LEN).contains(&element.len)
                || lease
                    .set_data_range(RX_FRAME_OFFSET, frame_len as u16)
                    .is_err()
            {
                drop(lease);
                continue;
            }
            *lease.metadata_mut() = PacketMetadata {
                queue_pair: self.id,
                frame_len,
                ..PacketMetadata::default()
            };
            let metadata = *lease.metadata();
            out.push(PacketChain::from_lease(lease), metadata)
                .unwrap_or_else(|_| unreachable!());
            packets += 1;
            bytes = bytes.saturating_add(frame_len);
        }
        RxPollResult {
            packets,
            bytes,
            ring_empty: self.rx.last_used == used_index,
            descriptor_starved: self.rx_free.len() == self.rx_pending.len(),
            fatal,
        }
    }

    fn reclaim_tx_batch(&mut self, out: &mut CompletionBatch) -> TxReclaimResult {
        let used_index = self.tx.used_index();
        let mut completions = 0u16;
        let mut descriptors = 0u16;
        let mut fatal = None;
        while self.tx.last_used != used_index && completions < 32 {
            let element = unsafe {
                read_volatile(
                    &(*self.tx.used).ring[self.tx.last_used as usize % self.tx.size as usize],
                )
            };
            self.tx.last_used = self.tx.last_used.wrapping_add(1);
            let head = element.id as usize;
            if head >= self.tx_pending.len() {
                fatal = Some(QueueFatalError::RingCorrupt);
                break;
            }
            let Some(pending) = self.tx_pending[head].take() else {
                fatal = Some(QueueFatalError::RingCorrupt);
                break;
            };
            for descriptor in pending.descriptors[..pending.descriptor_count as usize]
                .iter()
                .copied()
            {
                self.tx_free.push(descriptor);
                descriptors += 1;
            }
            out.push(pending.packet.completion)
                .unwrap_or_else(|_| unreachable!());
            completions += 1;
        }
        TxReclaimResult {
            completions,
            descriptors,
            ring_empty: self.tx.last_used == used_index,
            fatal,
        }
    }

    fn submit_tx_batch(
        &mut self,
        batch: &mut TxBatch,
        header_pool: &mut NetBufPoolOwner,
    ) -> TxSubmitResult {
        if self.quiesced {
            return TxSubmitResult {
                packets: 0,
                descriptors: 0,
                bytes: 0,
                queue_full: false,
                fatal: Some(QueueFatalError::DeviceGone),
            };
        }
        let original_len = batch.len();
        let mut submitted = 0u16;
        let mut descriptor_total = 0u16;
        let mut bytes = 0u32;
        let mut fatal = None;
        for index in 0..original_len {
            let Some(packet_ref) = batch.packet(index) else {
                continue;
            };
            let fragment_count = packet_ref.chain.fragment_count();
            let Some(descriptor_count) = tx_descriptor_count(fragment_count) else {
                break;
            };
            if self.tx_free.len() < descriptor_count {
                break;
            }
            let Some(packet) = batch.take(index) else {
                continue;
            };
            let payload_bytes = packet.chain.total_len() as u32;
            let Ok(mut header) = header_pool.lease(0, VIRTIO_HEADER_LEN, PacketMetadata::default())
            else {
                batch.put(index, packet).unwrap_or_else(|_| unreachable!());
                break;
            };
            let header_address = match header.dma_addr() {
                Ok(Some(address)) => address,
                _ => {
                    batch.put(index, packet).unwrap_or_else(|_| unreachable!());
                    fatal = Some(QueueFatalError::DmaFault);
                    break;
                }
            };
            header
                .as_mut_slice()
                .unwrap_or_else(|_| unreachable!("TX header lease 范围已校验"))
                .fill(0);
            if header.sync_for_device().is_err() {
                batch.put(index, packet).unwrap_or_else(|_| unreachable!());
                fatal = Some(QueueFatalError::DmaFault);
                break;
            }
            let mut ids = [0u16; 8];
            for id in &mut ids[..descriptor_count] {
                *id = self.tx_free.pop().expect("TX free descriptor 数量失配");
            }
            Self::write_descriptor(
                &mut self.tx,
                ids[0],
                header_address,
                u32::from(VIRTIO_HEADER_LEN),
                VIRTQ_DESC_F_NEXT,
                ids[1],
            );
            let mut valid = true;
            for fragment_index in 0..fragment_count {
                let fragment = packet.chain.fragment(fragment_index).unwrap();
                let Ok(Some(address)) = fragment.dma_addr() else {
                    valid = false;
                    break;
                };
                if fragment.sync_for_device().is_err() {
                    valid = false;
                    break;
                }
                let has_next = fragment_index + 1 < fragment_count;
                Self::write_descriptor(
                    &mut self.tx,
                    ids[fragment_index + 1],
                    address,
                    fragment.len() as u32,
                    if has_next { VIRTQ_DESC_F_NEXT } else { 0 },
                    if has_next { ids[fragment_index + 2] } else { 0 },
                );
            }
            if !valid {
                for id in ids[..descriptor_count].iter().copied() {
                    self.tx_free.push(id);
                }
                batch.put(index, packet).unwrap_or_else(|_| unreachable!());
                fatal = Some(QueueFatalError::DmaFault);
                break;
            }
            let head = ids[0];
            self.tx_pending[head as usize] = Some(TxPending {
                _header: header,
                packet,
                descriptors: ids,
                descriptor_count: descriptor_count as u8,
            });
            self.tx.push_available(head);
            submitted += 1;
            descriptor_total += descriptor_count as u16;
            bytes = bytes.saturating_add(payload_bytes);
        }
        if submitted != 0 {
            self.tx.desc_dma.sync_for_device();
            self.tx.avail_dma.sync_for_device();
            self.transport
                .notify_queue(self.tx.notify_addr, self.tx_queue_index());
        }
        TxSubmitResult {
            packets: submitted,
            descriptors: descriptor_total,
            bytes,
            queue_full: submitted as usize != original_len && fatal.is_none(),
            fatal,
        }
    }

    fn has_pending_work(&mut self) -> bool {
        self.rx.last_used != self.rx.used_index() || self.tx.last_used != self.tx.used_index()
    }

    fn quiesce(&mut self) -> Result<(), QueueFatalError> {
        self.quiesced = true;
        unsafe {
            atomic_flags(self.rx.avail).fetch_or(VIRTQ_AVAIL_F_NO_INTERRUPT, Ordering::AcqRel);
            atomic_flags(self.tx.avail).fetch_or(VIRTQ_AVAIL_F_NO_INTERRUPT, Ordering::AcqRel);
        }
        self.rx.avail_dma.sync_for_device();
        self.tx.avail_dma.sync_for_device();
        Ok(())
    }
}

unsafe fn atomic_flags(avail: *mut VirtqAvail) -> &'static AtomicU16 {
    // SAFETY: flags 是 2 字节对齐的 DMA 字段，queue 生命周期内地址稳定。
    unsafe { &*(core::ptr::addr_of!((*avail).flags).cast::<AtomicU16>()) }
}

struct VirtioQueueIrq {
    transport: Arc<dyn NetTransport>,
    rx_avail: *mut VirtqAvail,
    tx_avail: *mut VirtqAvail,
    rx_avail_sync: DmaSyncHandle,
    tx_avail_sync: DmaSyncHandle,
    uses_isr_status: bool,
    waker: Mutex<Option<Arc<dyn QueueWakeHandle>>>,
    masked: AtomicBool,
    pending: AtomicBool,
    irq_total: AtomicU64,
    irq_mask: AtomicU64,
    irq_unmask: AtomicU64,
}

unsafe impl Send for VirtioQueueIrq {}
unsafe impl Sync for VirtioQueueIrq {}

impl VirtioQueueIrq {
    fn notify_irq(&self) -> bool {
        if !self.ack_and_mask() {
            return false;
        }
        if !self.pending.swap(true, Ordering::AcqRel)
            && let Some(waker) = self.waker.lock().as_ref()
        {
            waker.wake();
        }
        true
    }
}

impl QueueIrqControl for VirtioQueueIrq {
    fn ack_and_mask(&self) -> bool {
        // VirtIO PCI 在 MSI-X 启用时规定 ISR status 未使用；MSI、INTx 和 MMIO
        // 仍需通过 transport 的 ISR/ack 寄存器确认中断来源。
        if self.uses_isr_status && !self.transport.ack_interrupt() {
            return false;
        }
        unsafe {
            atomic_flags(self.rx_avail).fetch_or(VIRTQ_AVAIL_F_NO_INTERRUPT, Ordering::AcqRel);
            atomic_flags(self.tx_avail).fetch_or(VIRTQ_AVAIL_F_NO_INTERRUPT, Ordering::AcqRel);
        }
        self.rx_avail_sync.sync_for_device();
        self.tx_avail_sync.sync_for_device();
        self.masked.store(true, Ordering::Release);
        self.irq_total.fetch_add(1, Ordering::Relaxed);
        self.irq_mask.fetch_add(1, Ordering::Relaxed);
        true
    }

    fn unmask(&self) {
        self.pending.store(false, Ordering::Release);
        unsafe {
            atomic_flags(self.rx_avail).fetch_and(!VIRTQ_AVAIL_F_NO_INTERRUPT, Ordering::Release);
            atomic_flags(self.tx_avail).fetch_and(!VIRTQ_AVAIL_F_NO_INTERRUPT, Ordering::Release);
        }
        self.rx_avail_sync.sync_for_device();
        self.tx_avail_sync.sync_for_device();
        fence(Ordering::SeqCst);
        self.masked.store(false, Ordering::Release);
        self.irq_unmask.fetch_add(1, Ordering::Relaxed);
    }

    fn set_waker(&self, waker: Arc<dyn QueueWakeHandle>) -> Result<(), QueueIrqError> {
        let mut slot = self.waker.lock();
        if slot.is_some() {
            return Err(QueueIrqError::WakerAlreadyInstalled);
        }
        *slot = Some(waker);
        Ok(())
    }

    fn stats(&self) -> QueueIrqStats {
        QueueIrqStats {
            irq_total: self.irq_total.load(Ordering::Relaxed),
            irq_mask: self.irq_mask.load(Ordering::Relaxed),
            irq_unmask: self.irq_unmask.load(Ordering::Relaxed),
        }
    }
}

fn make_dma_pool(
    count: usize,
    size: usize,
    align: usize,
    direction: DmaDirection,
    context: DmaContext,
) -> Result<NetBufPoolOwner, &'static str> {
    let mut storages = Vec::with_capacity(count);
    for _ in 0..count {
        let buffer = DmaBuffer::new_in(context, size, align, direction)?;
        storages.push(Box::new(buffer) as Box<dyn NetBufStorage>);
    }
    NetBufPool::new(storages.into_boxed_slice()).map_err(|_| "virtio-net pool 构造失败")
}

fn derive_mac(seed: &[u8; 16], identity: u64) -> [u8; 6] {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for byte in seed {
        state ^= u64::from(*byte);
        state = state.rotate_left(9).wrapping_mul(0x1000_0000_01b3);
    }
    state ^= identity;
    let mut mac = [0; 6];
    mac.copy_from_slice(&state.to_le_bytes()[..6]);
    mac[0] = (mac[0] | 0x02) & !0x01;
    mac
}

struct PreparedQueue {
    pair: VirtioNetQueuePair,
    irq: Arc<VirtioQueueIrq>,
    rx_pool: NetBufPoolOwner,
    tx_header_pool: NetBufPoolOwner,
    tx_payload_pool: NetBufPoolOwner,
}

struct PreparedDevice {
    queues: Vec<PreparedQueue>,
    control_queue: Option<SplitQueue>,
    transport: Arc<dyn NetTransport>,
    mac: [u8; 6],
    running: bool,
}

fn prepare_device(
    transport: Arc<dyn NetTransport>,
    context: DmaContext,
    identity: u64,
    use_msix: bool,
) -> Result<PreparedDevice, &'static str> {
    let boot = net::device::boot_config().ok_or("NetBootConfig 尚未安装")?;
    if !transport.reset() {
        return Err("VirtIO reset 超时");
    }
    transport.add_status(u32::from(VIRTIO_STATUS_ACKNOWLEDGE));
    transport.add_status(u32::from(VIRTIO_STATUS_DRIVER));
    let offered = transport.device_features();
    if offered & VIRTIO_F_VERSION_1 == 0 {
        transport.set_status(u32::from(VIRTIO_STATUS_FAILED));
        return Err("设备不支持 VERSION_1");
    }
    let device_pairs = if offered & VIRTIO_NET_F_MQ != 0 {
        transport
            .read_config_u16(NET_CFG_MAX_VQ_PAIRS)
            .filter(|pairs| *pairs != 0)
            .unwrap_or(1)
    } else {
        1
    };
    // 单队列仍可使用 MSI-X，但不会因此伪报 MQ/RSS 已启用。
    let available_vectors = if use_msix { 2 } else { 0 };
    let mq_plan = plan_mq_rss(
        offered,
        boot.active_cpu_count(),
        device_pairs,
        available_vectors,
    );
    if let MqRssPlan::Single(reason) = mq_plan {
        log::printk!(
            "[virtio-net] single-queue fallback: {:?} offered={:#x} pairs={}",
            reason,
            offered,
            device_pairs
        );
    }
    let mut accepted = VIRTIO_F_VERSION_1;
    if offered & VIRTIO_NET_F_MAC != 0 {
        accepted |= VIRTIO_NET_F_MAC;
    }
    if offered & VIRTIO_NET_F_STATUS != 0 {
        accepted |= VIRTIO_NET_F_STATUS;
    }
    transport.set_driver_features(accepted);
    transport.add_status(u32::from(VIRTIO_STATUS_FEATURES_OK));
    if transport.status() & u32::from(VIRTIO_STATUS_FEATURES_OK) == 0 {
        transport.set_status(u32::from(VIRTIO_STATUS_FAILED));
        return Err("设备拒绝 FEATURES_OK");
    }
    if use_msix {
        transport.set_config_msix_vector(1)?;
    }
    let queue_vector = use_msix.then_some(0);
    let rx = setup_data_queue(transport.as_ref(), context, RX_QUEUE, queue_vector)?;
    let tx = setup_data_queue(transport.as_ref(), context, TX_QUEUE, queue_vector)?;
    let queue_size = rx.size.min(tx.size) as usize;
    let irq = Arc::new(VirtioQueueIrq {
        transport: Arc::clone(&transport),
        rx_avail: rx.avail,
        tx_avail: tx.avail,
        rx_avail_sync: rx.avail_dma.sync_handle(),
        tx_avail_sync: tx.avail_dma.sync_handle(),
        uses_isr_status: !use_msix,
        waker: Mutex::new(None),
        masked: AtomicBool::new(true),
        pending: AtomicBool::new(false),
        irq_total: AtomicU64::new(0),
        irq_mask: AtomicU64::new(0),
        irq_unmask: AtomicU64::new(0),
    });
    let mut mac = [0; 6];
    if accepted & VIRTIO_NET_F_MAC != 0 {
        for (index, byte) in mac.iter_mut().enumerate() {
            *byte = transport
                .read_config_u8(NET_CFG_MAC + index)
                .ok_or("VirtIO MAC config 不可读")?;
        }
    } else {
        mac = derive_mac(boot.mac_seed(), identity);
    }
    let running = if accepted & VIRTIO_NET_F_STATUS != 0 {
        transport
            .read_config_u16(NET_CFG_STATUS)
            .is_some_and(|status| status & VIRTIO_NET_STATUS_LINK_UP != 0)
    } else {
        true
    };
    let rx_pool = make_dma_pool(
        queue_size * 2 + 16,
        4096,
        4096,
        DmaDirection::FromDevice,
        context,
    )?;
    let tx_header_pool = make_dma_pool(queue_size, 256, 256, DmaDirection::ToDevice, context)?;
    let tx_payload_pool = make_dma_pool(256, 4096, 4096, DmaDirection::ToDevice, context)?;
    transport.add_status(u32::from(VIRTIO_STATUS_DRIVER_OK));
    Ok(PreparedDevice {
        queues: vec![PreparedQueue {
            pair: VirtioNetQueuePair::new(QueuePairId(0), Arc::clone(&transport), rx, tx),
            irq,
            rx_pool,
            tx_header_pool,
            tx_payload_pool,
        }],
        control_queue: None,
        transport,
        mac,
        running,
    })
}

fn inspect_mq_plan(transport: &dyn NetTransport, available_msix_vectors: u16) -> MqRssPlan {
    let Some(boot) = net::device::boot_config() else {
        return MqRssPlan::Single(MqRssFallback::TooFewQueuePairs);
    };
    if !transport.reset() {
        return MqRssPlan::Single(MqRssFallback::MissingFeatures);
    }
    transport.add_status(u32::from(VIRTIO_STATUS_ACKNOWLEDGE));
    transport.add_status(u32::from(VIRTIO_STATUS_DRIVER));
    let offered = transport.device_features();
    let pairs = if offered & VIRTIO_NET_F_MQ != 0 {
        transport
            .read_config_u16(NET_CFG_MAX_VQ_PAIRS)
            .filter(|pairs| *pairs != 0)
            .unwrap_or(1)
    } else {
        1
    };
    let plan = plan_mq_rss(
        offered,
        boot.active_cpu_count(),
        pairs,
        available_msix_vectors,
    );
    let _ = transport.reset();
    plan
}

fn build_rss_config(
    pairs: u16,
    device_key_size: u8,
    device_table_len: u16,
    supported_hash: u32,
    key: &[u8; 40],
) -> Result<Vec<u8>, &'static str> {
    if pairs < 2
        || device_key_size < key.len() as u8
        || device_table_len < VIRTIO_NET_RSS_TABLE_LEN
        || supported_hash & VIRTIO_NET_RSS_HASH_TYPES != VIRTIO_NET_RSS_HASH_TYPES
    {
        return Err("VirtIO RSS 参数能力不足");
    }
    let mut rss = Vec::new();
    rss.try_reserve_exact(4 + 2 + 2 + VIRTIO_NET_RSS_TABLE_LEN as usize * 2 + 2 + 1 + key.len())
        .map_err(|_| "VirtIO RSS payload 分配失败")?;
    rss.extend_from_slice(&VIRTIO_NET_RSS_HASH_TYPES.to_le_bytes());
    rss.extend_from_slice(&(VIRTIO_NET_RSS_TABLE_LEN - 1).to_le_bytes());
    rss.extend_from_slice(&0u16.to_le_bytes());
    for bucket in 0..VIRTIO_NET_RSS_TABLE_LEN {
        rss.extend_from_slice(&(bucket % pairs).to_le_bytes());
    }
    rss.extend_from_slice(&pairs.to_le_bytes());
    rss.push(key.len() as u8);
    rss.extend_from_slice(key);
    Ok(rss)
}

fn prepare_multi_device(
    transport: Arc<dyn NetTransport>,
    context: DmaContext,
    identity: u64,
    pairs: u16,
) -> Result<PreparedDevice, &'static str> {
    let boot = net::device::boot_config().ok_or("NetBootConfig 尚未安装")?;
    if pairs < 2 || pairs > 8 || !transport.reset() {
        return Err("VirtIO MQ 参数或 reset 非法");
    }
    transport.add_status(u32::from(VIRTIO_STATUS_ACKNOWLEDGE));
    transport.add_status(u32::from(VIRTIO_STATUS_DRIVER));
    let offered = transport.device_features();
    let required = VIRTIO_F_VERSION_1 | VIRTIO_NET_F_CTRL_VQ | VIRTIO_NET_F_MQ | VIRTIO_NET_F_RSS;
    if offered & required != required {
        return Err("VirtIO MQ/RSS feature 不完整");
    }
    let mut accepted = required;
    if offered & VIRTIO_NET_F_MAC != 0 {
        accepted |= VIRTIO_NET_F_MAC;
    }
    if offered & VIRTIO_NET_F_STATUS != 0 {
        accepted |= VIRTIO_NET_F_STATUS;
    }
    transport.set_driver_features(accepted);
    transport.add_status(u32::from(VIRTIO_STATUS_FEATURES_OK));
    if transport.status() & u32::from(VIRTIO_STATUS_FEATURES_OK) == 0 {
        return Err("设备拒绝 MQ/RSS FEATURES_OK");
    }
    transport.set_config_msix_vector(pairs)?;

    let mut queues = Vec::new();
    queues
        .try_reserve_exact(pairs as usize)
        .map_err(|_| "virtio-net MQ queue 分配失败")?;
    for pair_index in 0..pairs {
        let rx_index = pair_index * 2;
        let tx_index = rx_index + 1;
        let rx = setup_data_queue(transport.as_ref(), context, rx_index, Some(pair_index))?;
        let tx = setup_data_queue(transport.as_ref(), context, tx_index, Some(pair_index))?;
        let queue_size = rx.size.min(tx.size) as usize;
        let irq = Arc::new(VirtioQueueIrq {
            transport: Arc::clone(&transport),
            rx_avail: rx.avail,
            tx_avail: tx.avail,
            rx_avail_sync: rx.avail_dma.sync_handle(),
            tx_avail_sync: tx.avail_dma.sync_handle(),
            uses_isr_status: false,
            waker: Mutex::new(None),
            masked: AtomicBool::new(true),
            pending: AtomicBool::new(false),
            irq_total: AtomicU64::new(0),
            irq_mask: AtomicU64::new(0),
            irq_unmask: AtomicU64::new(0),
        });
        queues.push(PreparedQueue {
            pair: VirtioNetQueuePair::new(QueuePairId(pair_index), Arc::clone(&transport), rx, tx),
            irq,
            rx_pool: make_dma_pool(
                queue_size * 2 + 16,
                4096,
                4096,
                DmaDirection::FromDevice,
                context,
            )?,
            tx_header_pool: make_dma_pool(queue_size, 256, 256, DmaDirection::ToDevice, context)?,
            tx_payload_pool: make_dma_pool(256, 4096, 4096, DmaDirection::ToDevice, context)?,
        });
    }

    let control_index = pairs * 2;
    let mut control_queue = setup_control_queue(transport.as_ref(), context, control_index, pairs)?;

    // VirtIO 1.2 禁止驱动在 DRIVER_OK 前发送 available notification。数据队列
    // 此时尚未 refill，因此设备进入 live 状态后只会消费下面的 control command。
    transport.add_status(u32::from(VIRTIO_STATUS_DRIVER_OK));
    submit_control_command(
        transport.as_ref(),
        &mut control_queue,
        context,
        control_index,
        VIRTIO_NET_CTRL_MQ,
        VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET,
        &pairs.to_le_bytes(),
    )?;

    let key_size = transport
        .read_config_u8(NET_CFG_RSS_MAX_KEY_SIZE)
        .ok_or("VirtIO RSS key size 不可读")?;
    let device_table_len = transport
        .read_config_u16(NET_CFG_RSS_MAX_TABLE_LEN)
        .ok_or("VirtIO RSS table size 不可读")?;
    let supported_hash = transport
        .read_config_u32(NET_CFG_RSS_SUPPORTED_HASH_TYPES)
        .ok_or("VirtIO RSS hash types 不可读")?;
    let rss = build_rss_config(
        pairs,
        key_size,
        device_table_len,
        supported_hash,
        boot.rss_key(),
    )?;
    submit_control_command(
        transport.as_ref(),
        &mut control_queue,
        context,
        control_index,
        VIRTIO_NET_CTRL_MQ,
        VIRTIO_NET_CTRL_MQ_RSS_CONFIG,
        &rss,
    )?;

    let mut mac = [0; 6];
    if accepted & VIRTIO_NET_F_MAC != 0 {
        for (index, byte) in mac.iter_mut().enumerate() {
            *byte = transport
                .read_config_u8(NET_CFG_MAC + index)
                .ok_or("VirtIO MAC config 不可读")?;
        }
    } else {
        mac = derive_mac(boot.mac_seed(), identity);
    }
    let running = if accepted & VIRTIO_NET_F_STATUS != 0 {
        transport
            .read_config_u16(NET_CFG_STATUS)
            .is_some_and(|status| status & VIRTIO_NET_STATUS_LINK_UP != 0)
    } else {
        true
    };
    Ok(PreparedDevice {
        queues,
        control_queue: Some(control_queue),
        transport,
        mac,
        running,
    })
}

trait MqSetupTransaction {
    type Prepared;
    type Msix;
    type Activation;
    type AttemptError;
    type SingleError;

    fn allocate_msix(&mut self, count: u16) -> Result<Self::Msix, Self::AttemptError>;
    fn prepare_multi(&mut self, pairs: u16) -> Result<Self::Prepared, Self::AttemptError>;
    fn release_msix(&mut self, set: Self::Msix);
    /// 消费 MSI-X set；失败时实现方必须释放 set 和已登记的外部 handle。
    fn activate_multi(
        &mut self,
        prepared: &Self::Prepared,
        set: Self::Msix,
    ) -> Result<Self::Activation, Self::AttemptError>;
    fn reset_for_fallback(&mut self);
    fn prepare_single(&mut self) -> Result<Self::Prepared, Self::SingleError>;
}

enum MqSetupOutcome<Prepared, Activation, AttemptError> {
    Multi {
        prepared: Prepared,
        activation: Activation,
    },
    Single {
        prepared: Prepared,
        reason: AttemptError,
    },
}

fn run_mq_setup_transaction<T: MqSetupTransaction>(
    transaction: &mut T,
    pairs: u16,
) -> Result<MqSetupOutcome<T::Prepared, T::Activation, T::AttemptError>, T::SingleError> {
    let set = match transaction.allocate_msix(pairs + 1) {
        Ok(set) => set,
        Err(reason) => {
            transaction.reset_for_fallback();
            let prepared = transaction.prepare_single()?;
            return Ok(MqSetupOutcome::Single { prepared, reason });
        }
    };
    match transaction.prepare_multi(pairs) {
        Ok(prepared) => match transaction.activate_multi(&prepared, set) {
            Ok(activation) => Ok(MqSetupOutcome::Multi {
                prepared,
                activation,
            }),
            Err(reason) => {
                drop(prepared);
                transaction.reset_for_fallback();
                let prepared = transaction.prepare_single()?;
                Ok(MqSetupOutcome::Single { prepared, reason })
            }
        },
        Err(reason) => {
            transaction.reset_for_fallback();
            transaction.release_msix(set);
            let prepared = transaction.prepare_single()?;
            Ok(MqSetupOutcome::Single { prepared, reason })
        }
    }
}

#[derive(Debug)]
enum VirtioMqAttemptError {
    Msix(PciMsixError),
    Prepare(&'static str),
    Activate,
}

impl VirtioMqAttemptError {
    fn describe(&self) -> alloc::string::String {
        match self {
            Self::Msix(error) => alloc::format!("MSI-X: {:?}", error),
            Self::Prepare(message) => alloc::format!("MQ/RSS: {}", message),
            Self::Activate => alloc::string::String::from("MSI-X IRQ 激活失败"),
        }
    }
}

struct VirtioMqSetupTransaction<'a> {
    device: &'a Arc<PnpDevice>,
    pci: &'a PciDevice,
    transport: Arc<dyn NetTransport>,
    context: DmaContext,
    identity: u64,
}

impl MqSetupTransaction for VirtioMqSetupTransaction<'_> {
    type Prepared = PreparedDevice;
    type Msix = PciMsixSet;
    type Activation = IrqRegistration;
    type AttemptError = VirtioMqAttemptError;
    type SingleError = &'static str;

    fn allocate_msix(&mut self, count: u16) -> Result<Self::Msix, Self::AttemptError> {
        self.pci
            .try_configure_msix(count)
            .map_err(VirtioMqAttemptError::Msix)
    }

    fn prepare_multi(&mut self, pairs: u16) -> Result<Self::Prepared, Self::AttemptError> {
        prepare_multi_device(
            Arc::clone(&self.transport),
            self.context,
            self.identity,
            pairs,
        )
        .map_err(VirtioMqAttemptError::Prepare)
    }

    fn release_msix(&mut self, set: Self::Msix) {
        self.pci.release_configured_msix(set);
    }

    fn activate_multi(
        &mut self,
        prepared: &Self::Prepared,
        set: Self::Msix,
    ) -> Result<Self::Activation, Self::AttemptError> {
        register_msix_irqs(self.device, self.pci, set, &prepared.queues)
            .map_err(|_| VirtioMqAttemptError::Activate)
    }

    fn reset_for_fallback(&mut self) {
        let _ = self.transport.reset();
    }

    fn prepare_single(&mut self) -> Result<Self::Prepared, Self::SingleError> {
        prepare_device(
            Arc::clone(&self.transport),
            self.context,
            self.identity,
            false,
        )
    }
}

#[derive(Clone, Copy)]
struct IrqRegistration {
    using_msi: bool,
}

struct VirtioIrqHandler {
    control: Arc<VirtioQueueIrq>,
}

impl IrqHandler for VirtioIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        if self.control.notify_irq() {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }
}

struct VirtioMsixConfigIrqHandler;

impl IrqHandler for VirtioMsixConfigIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        // 中断上下文不修改 link/control 状态。MSI-X message 本身已经
        // 标识 config/control vector，且 VirtIO 规定此模式下 ISR status 未使用。
        IrqStatus::Handled
    }
}

fn map_irq_error(error: IrqError) -> &'static str {
    match error {
        IrqError::OutOfMemory => "out of memory",
        IrqError::NotFound => "not found",
        IrqError::AlreadyRegistered => "already registered",
    }
}

fn register_irq(
    device: &Arc<PnpDevice>,
    pci: &PciDevice,
    control: Arc<VirtioQueueIrq>,
) -> Result<IrqRegistration, PnpError> {
    device.reserve_owned_resources(2)?;
    let handler: Arc<dyn IrqHandler> = Arc::new(VirtioIrqHandler { control });
    if let Ok(msi) = pci.try_configure_single_msi() {
        match irq::register_irq_handler(msi.line(), Arc::clone(&handler)) {
            Ok(irq_handle) if pci.try_enable_configured_msi(msi).is_ok() => {
                pci.disable_interrupts();
                device.own_resource(PciMsiPnpResource::new(pci.clone(), msi, "virtio-net-msi"))?;
                device.own_resource(irq::irq_handler_pnp_resource(
                    irq_handle,
                    "virtio-net-msi-irq",
                ))?;
                return Ok(IrqRegistration { using_msi: true });
            }
            Ok(irq_handle) => {
                let _ = irq::unregister_irq_handler(irq_handle);
                pci.release_configured_msi(msi);
            }
            Err(error) => {
                log::printk!("[virtio-net] MSI 注册失败: {}", map_irq_error(error));
                pci.release_configured_msi(msi);
            }
        }
    }
    let Some(line) = pci.routed_irq_line() else {
        return Err(PnpError::missing(
            PnpResourceKind::Irq,
            "virtio-net PCI IRQ route missing",
        ));
    };
    match irq::register_irq_handler(line, handler) {
        Ok(handle) => {
            pci.enable_interrupts();
            device.own_resource(irq::irq_handler_pnp_resource(handle, "virtio-net-intx"))?;
            Ok(IrqRegistration { using_msi: false })
        }
        Err(error) => {
            log::printk!("[virtio-net] INTx 注册失败: {}", map_irq_error(error));
            Err(PnpError::hardware_failure(
                "virtio-net INTx registration failed",
            ))
        }
    }
}

fn register_msix_irqs(
    device: &Arc<PnpDevice>,
    pci: &PciDevice,
    set: PciMsixSet,
    queues: &[PreparedQueue],
) -> Result<IrqRegistration, PnpError> {
    if set.len() != queues.len() + 1 {
        pci.release_configured_msix(set);
        return Err(PnpError::InvalidState);
    }
    if let Err(error) = device.reserve_owned_resources(queues.len() + 2) {
        pci.release_configured_msix(set);
        return Err(error);
    }
    let mut handles = Vec::new();
    if handles.try_reserve_exact(queues.len() + 1).is_err() {
        pci.release_configured_msix(set);
        return Err(PnpError::OutOfMemory);
    }
    for (index, queue) in queues.iter().enumerate() {
        let vector = set
            .vector(index)
            .unwrap_or_else(|| unreachable!("MSI-X vector 数量已校验"));
        let handler: Arc<dyn IrqHandler> = Arc::new(VirtioIrqHandler {
            control: Arc::clone(&queue.irq),
        });
        match irq::register_irq_handler(vector.line(), handler) {
            Ok(handle) => handles.push(handle),
            Err(error) => {
                for handle in handles.drain(..) {
                    let _ = irq::unregister_irq_handler(handle);
                }
                pci.release_configured_msix(set);
                log::printk!(
                    "[virtio-net] MSI-X queue IRQ 注册失败: {}",
                    map_irq_error(error)
                );
                return Err(PnpError::InvalidState);
            }
        }
    }
    let config_vector = set
        .vector(queues.len())
        .unwrap_or_else(|| unreachable!("MSI-X config vector 已校验"));
    let config_handler: Arc<dyn IrqHandler> = Arc::new(VirtioMsixConfigIrqHandler);
    match irq::register_irq_handler(config_vector.line(), config_handler) {
        Ok(handle) => handles.push(handle),
        Err(error) => {
            for handle in handles.drain(..) {
                let _ = irq::unregister_irq_handler(handle);
            }
            pci.release_configured_msix(set);
            log::printk!(
                "[virtio-net] MSI-X config IRQ 注册失败: {}",
                map_irq_error(error)
            );
            return Err(PnpError::InvalidState);
        }
    }
    if pci.try_enable_configured_msix(&set).is_err() {
        for handle in handles.drain(..) {
            let _ = irq::unregister_irq_handler(handle);
        }
        pci.release_configured_msix(set);
        return Err(PnpError::InvalidState);
    }
    pci.disable_interrupts();
    device.own_resource(PciMsixPnpResource::new(pci.clone(), set, "virtio-net-msix"))?;
    for handle in handles {
        device.own_resource(irq::irq_handler_pnp_resource(handle, "virtio-net-msix-irq"))?;
    }
    Ok(IrqRegistration { using_msi: true })
}

struct VirtioNetBinding {
    handle: NetDeviceHandle,
    transport: Arc<dyn NetTransport>,
    irq: Option<IrqRegistration>,
    _control_queue: Option<Mutex<SplitQueue>>,
}

pub struct VirtioNetPciDriver;

impl PnpDriver for VirtioNetPciDriver {
    fn name(&self) -> &'static str {
        "virtio-pci-net"
    }

    fn bus_type(&self) -> BusType {
        BusType::PCI
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        let PnpId::Pci { .. } = id else {
            return false;
        };
        let Some(info) = info.as_any().downcast_ref::<PciInfo>() else {
            return false;
        };
        VIRTIO_PCI_FUNCTION_NETWORK.matches_pci_ids(info.vendor, info.device_id)
    }

    fn probe(&self, device: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let pci = PciDevice::from_pnp(device).ok_or(PnpError::InvalidState)?;
        pci.try_enable_mmio().map_err(|_| PnpError::InvalidState)?;
        pci.try_enable_bus_master()
            .map_err(|_| PnpError::InvalidState)?;
        // 候选初始化期间尚未安装 handler；禁止 MSI-X 失败窗口回落到 INTx。
        pci.disable_interrupts();
        let caps = parse_virtio_pci_caps(&pci).ok_or(PnpError::InvalidState)?;
        let pci_transport = VirtioPciTransport::new(caps).map_err(|_| PnpError::InvalidState)?;
        let identity = pci
            .info()
            .map(|info| u64::from(info.vendor) << 32 | u64::from(info.device_id))
            .unwrap_or(0);
        let transport: Arc<dyn NetTransport> = Arc::new(PciNetTransport(pci_transport));
        let mq_plan = inspect_mq_plan(transport.as_ref(), pci.msix_table_size().unwrap_or(0));
        let (prepared, multi_irq, single_msix) = match mq_plan {
            MqRssPlan::Multi { pairs } => {
                let mut transaction = VirtioMqSetupTransaction {
                    device,
                    pci: &pci,
                    context: pci.dma_context(),
                    identity,
                    transport: Arc::clone(&transport),
                };
                match run_mq_setup_transaction(&mut transaction, pairs).map_err(|message| {
                    log::printk!("[virtio-net] reset-to-single 失败: {}", message);
                    PnpError::hardware_failure("virtio-net fallback failed")
                })? {
                    MqSetupOutcome::Multi {
                        prepared,
                        activation,
                    } => (prepared, Some(activation), None),
                    MqSetupOutcome::Single { prepared, reason } => {
                        log::printk!(
                            "[virtio-net] MQ/RSS 候选失败，reset-to-single: {}",
                            reason.describe()
                        );
                        (prepared, None, None)
                    }
                }
            }
            MqRssPlan::Single(_) => {
                let single_msix = pci.try_configure_msix(2).ok();
                let prepared = match prepare_device(
                    Arc::clone(&transport),
                    pci.dma_context(),
                    identity,
                    single_msix.is_some(),
                ) {
                    Ok(prepared) => prepared,
                    Err(message) => {
                        if let Some(set) = single_msix {
                            pci.release_configured_msix(set);
                        }
                        log::printk!("[virtio-net] probe 失败: {}", message);
                        return Err(PnpError::hardware_failure("virtio-net init failed"));
                    }
                };
                (prepared, None, single_msix)
            }
        };
        let name = NET_IFACE_NAMES
            .try_alloc_stable(&device.name)?
            .into_string();
        let irq_registration = if let Some(registration) = multi_irq {
            Some(registration)
        } else if let Some(set) = single_msix {
            Some(register_msix_irqs(device, &pci, set, &prepared.queues)?)
        } else {
            Some(register_irq(
                device,
                &pci,
                Arc::clone(&prepared.queues[0].irq),
            )?)
        };
        let queues = prepared
            .queues
            .into_iter()
            .enumerate()
            .map(|(index, prepared)| NetQueueRegistration {
                id: QueuePairId(index as u16),
                queue: Box::new(prepared.pair),
                rx_pool: prepared.rx_pool,
                tx_header_pool: prepared.tx_header_pool,
                tx_payload_pool: prepared.tx_payload_pool,
                irq: prepared.irq,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let registration = NetDeviceRegistration::new(
            name.clone().into_boxed_str(),
            prepared.mac,
            1500,
            prepared.running,
            queues,
        );
        let handle = match net::device::register_device(registration) {
            Ok(handle) => handle,
            Err(error) => {
                prepared.transport.set_status(0);
                return Err(PnpError::registration_failed(
                    PnpResourceKind::Function,
                    match error.kind {
                        net::device::NetDeviceRegisterErrorKind::RegistrarNotReady => {
                            "net registrar not ready"
                        }
                        _ => "net registration rejected",
                    },
                ));
            }
        };
        if let Err(error) = device.register_function(Arc::new(NetFunction::new(&name))) {
            let _ = net::device::begin_remove(handle);
            prepared.transport.set_status(0);
            return Err(error);
        }
        device.set_driver_data(Arc::new(VirtioNetBinding {
            handle,
            transport: prepared.transport,
            irq: irq_registration,
            _control_queue: prepared.control_queue.map(Mutex::new),
        }));
        log::printk!(
            "[virtio-net] attached {} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} link={}",
            name,
            prepared.mac[0],
            prepared.mac[1],
            prepared.mac[2],
            prepared.mac[3],
            prepared.mac[4],
            prepared.mac[5],
            prepared.running as u8,
        );
        Ok(())
    }

    fn remove(&self, device: &Arc<PnpDevice>) {
        if let Some(data) = device.take_driver_data()
            && let Ok(binding) = data.downcast::<VirtioNetBinding>()
        {
            if net::device::begin_remove(binding.handle).is_ok() {
                if !binding.irq.is_some_and(|irq| irq.using_msi)
                    && let Some(pci) = PciDevice::from_pnp(device)
                {
                    pci.disable_interrupts();
                }
                binding.transport.set_status(0);
            }
        }
        log::printk!("[virtio-net] remove {}", device.id);
    }
}

pub struct VirtioNetMmioDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl VirtioNetMmioDriver {
    fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_network(&self, info: &PlatformDeviceInfo) -> bool {
        if !(info.has_id("virtio,mmio") || info.has_id("LNRO0005")) {
            return false;
        }
        let Some((physical, _)) = info.first_mmio() else {
            return false;
        };
        let base = (self.device_mmio_to_virt)(physical);
        let magic = unsafe { read_volatile(base as *const u32) };
        let version = unsafe { read_volatile((base + 4) as *const u32) };
        let device_id = unsafe { read_volatile((base + 8) as *const u32) };
        if magic == 0x7472_6976 && device_id != 0 {
            log::printk!(
                "[virtio-mmio-net] candidate phys={:#x} version={} device_id={}",
                physical,
                version,
                device_id,
            );
        }
        magic == 0x7472_6976 && version == 2 && device_id == VIRTIO_MMIO_DEVICE_ID_NETWORK
    }
}

struct VirtioMmioIrqHandler {
    control: Arc<VirtioQueueIrq>,
}

impl IrqHandler for VirtioMmioIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        if self.control.notify_irq() {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }
}

fn first_irq_dependency(info: &PlatformDeviceInfo) -> PnpDependency {
    info.irq_resources()
        .find_map(|irq| irq.controller())
        .map(PnpDependency::IrqController)
        .unwrap_or(PnpDependency::DefaultIrqDomain)
}

fn register_mmio_irq(
    device: &Arc<PnpDevice>,
    info: &PlatformDeviceInfo,
    control: Arc<VirtioQueueIrq>,
) -> Result<(), PnpError> {
    device.reserve_owned_resources(1)?;
    let handler: Arc<dyn IrqHandler> = Arc::new(VirtioMmioIrqHandler { control });
    let handle = match info.register_first_irq_handler(handler) {
        Ok(handle) => handle,
        Err(PlatformIrqRegistrationError::NoResource) => {
            return Err(PnpError::missing(
                PnpResourceKind::Irq,
                "virtio-mmio net irq missing",
            ));
        }
        Err(PlatformIrqRegistrationError::Unresolved) => {
            return Err(PnpError::dependency(first_irq_dependency(info)));
        }
        Err(PlatformIrqRegistrationError::RegistrationFailed { err, .. }) => {
            return Err(match err {
                IrqError::OutOfMemory => PnpError::OutOfMemory,
                IrqError::AlreadyRegistered => PnpError::registration_failed(
                    PnpResourceKind::Irq,
                    "virtio-mmio net irq already registered",
                ),
                IrqError::NotFound => PnpError::registration_failed(
                    PnpResourceKind::Irq,
                    "virtio-mmio net irq not found",
                ),
            });
        }
    };
    if let Err(error) =
        device.own_resource(irq::irq_handler_pnp_resource(handle, "virtio-mmio-net-irq"))
    {
        let _ = irq::unregister_irq_handler(handle);
        return Err(error);
    }
    Ok(())
}

impl PnpDriver for VirtioNetMmioDriver {
    fn name(&self) -> &'static str {
        "platform-virtio-mmio-net"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .is_some_and(|info| self.matches_network(info))
    }

    fn probe(&self, device: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = device
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let (physical, _) = info.first_mmio().ok_or(PnpError::missing(
            PnpResourceKind::Mmio,
            "virtio-mmio net reg missing",
        ))?;
        let base = (self.device_mmio_to_virt)(physical);
        let inner = virtio_mmio::detect(base).map_err(|_| PnpError::InvalidState)?;
        if inner.is_legacy() {
            return Err(PnpError::hardware_failure(
                "legacy virtio-mmio net is unsupported",
            ));
        }
        let transport: Arc<dyn NetTransport> = Arc::new(MmioNetTransport { inner });
        let prepared = prepare_device(transport, info.dma_context(), physical as u64, false)
            .map_err(|message| {
                log::printk!("[virtio-mmio-net] probe 失败: {}", message);
                PnpError::hardware_failure("virtio-mmio net init failed")
            })?;
        register_mmio_irq(device, info, Arc::clone(&prepared.queues[0].irq))?;
        let name = NET_IFACE_NAMES
            .try_alloc_stable(&device.name)?
            .into_string();
        let queues = prepared
            .queues
            .into_iter()
            .enumerate()
            .map(|(index, prepared)| NetQueueRegistration {
                id: QueuePairId(index as u16),
                queue: Box::new(prepared.pair),
                rx_pool: prepared.rx_pool,
                tx_header_pool: prepared.tx_header_pool,
                tx_payload_pool: prepared.tx_payload_pool,
                irq: prepared.irq,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let registration = NetDeviceRegistration::new(
            name.clone().into_boxed_str(),
            prepared.mac,
            1500,
            prepared.running,
            queues,
        );
        let handle = match net::device::register_device(registration) {
            Ok(handle) => handle,
            Err(error) => {
                prepared.transport.set_status(0);
                return Err(PnpError::registration_failed(
                    PnpResourceKind::Function,
                    match error.kind {
                        net::device::NetDeviceRegisterErrorKind::RegistrarNotReady => {
                            "net registrar not ready"
                        }
                        _ => "net registration rejected",
                    },
                ));
            }
        };
        if let Err(error) = device.register_function(Arc::new(NetFunction::new(&name))) {
            let _ = net::device::begin_remove(handle);
            prepared.transport.set_status(0);
            return Err(error);
        }
        device.set_driver_data(Arc::new(VirtioNetBinding {
            handle,
            transport: prepared.transport,
            irq: None,
            _control_queue: prepared.control_queue.map(Mutex::new),
        }));
        log::printk!(
            "[virtio-mmio-net] attached {} phys={:#x} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            name,
            physical,
            prepared.mac[0],
            prepared.mac[1],
            prepared.mac[2],
            prepared.mac[3],
            prepared.mac[4],
            prepared.mac[5],
        );
        Ok(())
    }

    fn remove(&self, device: &Arc<PnpDevice>) {
        if let Some(data) = device.take_driver_data()
            && let Ok(binding) = data.downcast::<VirtioNetBinding>()
            && net::device::begin_remove(binding.handle).is_ok()
        {
            binding.transport.set_status(0);
        }
        log::printk!("[virtio-mmio-net] remove {}", device.id);
    }
}

struct VirtioNetPciFactory;

impl DriverFactory for VirtioNetPciFactory {
    fn name(&self) -> &'static str {
        "virtio-pci-net"
    }

    fn create(&self, _ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(VirtioNetPciDriver))
    }
}

struct VirtioNetMmioFactory;

impl DriverFactory for VirtioNetMmioFactory {
    fn name(&self) -> &'static str {
        "platform-virtio-mmio-net"
    }

    fn create(&self, context: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(VirtioNetMmioDriver::new(
            context.device_mmio_to_virt,
        )))
    }
}

pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(VirtioNetPciFactory))?;
    register_driver_factory(Arc::new(VirtioNetMmioFactory)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use crate::dev::dma::{DmaConstraints, DmaMapper, DmaSyncRegion};

    struct CountingMapper {
        device_syncs: AtomicUsize,
    }

    impl DmaMapper for CountingMapper {
        fn sync_for_device(&self, _region: DmaSyncRegion) {
            self.device_syncs.fetch_add(1, Ordering::Relaxed);
        }

        fn sync_for_cpu(&self, _region: DmaSyncRegion) {}

        fn phys_to_dma(
            &self,
            region: DmaSyncRegion,
            _constraints: DmaConstraints,
        ) -> Option<usize> {
            Some(region.paddr)
        }
    }

    static COUNTING_MAPPER: CountingMapper = CountingMapper {
        device_syncs: AtomicUsize::new(0),
    };

    struct IrqTestTransport;

    impl NetTransport for IrqTestTransport {
        fn reset(&self) -> bool {
            true
        }

        fn status(&self) -> u32 {
            0
        }

        fn set_status(&self, _status: u32) {}

        fn device_features(&self) -> u64 {
            0
        }

        fn set_driver_features(&self, _features: u64) {}

        fn select_queue(&self, _index: u16) {}

        fn selected_queue_size(&self) -> u16 {
            0
        }

        fn set_selected_queue_size(&self, _size: u16) {}

        fn set_config_msix_vector(&self, _vector: u16) -> Result<(), &'static str> {
            Ok(())
        }

        fn set_selected_queue_msix_vector(&self, _vector: u16) -> Result<(), &'static str> {
            Ok(())
        }

        fn set_selected_queue_addresses(&self, _desc: u64, _avail: u64, _used: u64) {}

        fn enable_selected_queue(&self) {}

        fn selected_queue_notify_token(&self, _index: u16) -> Result<usize, &'static str> {
            Ok(0)
        }

        fn notify_queue(&self, _token: usize, _index: u16) {}

        fn ack_interrupt(&self) -> bool {
            true
        }

        fn read_config_u8(&self, _offset: usize) -> Option<u8> {
            None
        }

        fn read_config_u16(&self, _offset: usize) -> Option<u16> {
            None
        }

        fn read_config_u32(&self, _offset: usize) -> Option<u32> {
            None
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SetupStage {
        AllocateMsix,
        AcceptFeatures,
        SetupQueues,
        SetDriverOk,
        SetPairs,
        SetRss,
        ActivateMsix,
    }

    struct FakeMqBackend {
        fail: Option<SetupStage>,
        reset_count: usize,
        single_count: usize,
        release_count: usize,
        visited: Vec<SetupStage>,
    }

    impl FakeMqBackend {
        fn run_stage(&mut self, stage: SetupStage) -> Result<(), SetupStage> {
            self.visited.push(stage);
            (self.fail != Some(stage)).then_some(()).ok_or(stage)
        }
    }

    impl MqSetupTransaction for FakeMqBackend {
        type Prepared = ();
        type Msix = ();
        type Activation = ();
        type AttemptError = SetupStage;
        type SingleError = ();

        fn allocate_msix(&mut self, _count: u16) -> Result<Self::Msix, Self::AttemptError> {
            self.run_stage(SetupStage::AllocateMsix)
        }

        fn prepare_multi(&mut self, _pairs: u16) -> Result<Self::Prepared, Self::AttemptError> {
            for stage in [
                SetupStage::AcceptFeatures,
                SetupStage::SetupQueues,
                SetupStage::SetDriverOk,
                SetupStage::SetPairs,
                SetupStage::SetRss,
            ] {
                self.run_stage(stage)?;
            }
            Ok(())
        }

        fn release_msix(&mut self, _set: Self::Msix) {
            self.release_count += 1;
        }

        fn activate_multi(
            &mut self,
            _prepared: &Self::Prepared,
            _set: Self::Msix,
        ) -> Result<Self::Activation, Self::AttemptError> {
            match self.run_stage(SetupStage::ActivateMsix) {
                Ok(()) => Ok(()),
                Err(error) => {
                    self.release_count += 1;
                    Err(error)
                }
            }
        }

        fn reset_for_fallback(&mut self) {
            self.reset_count += 1;
        }

        fn prepare_single(&mut self) -> Result<Self::Prepared, Self::SingleError> {
            self.single_count += 1;
            Ok(())
        }
    }

    #[test]
    fn mq_rss_plan_requires_complete_capability_set() {
        let required = VIRTIO_NET_F_CTRL_VQ | VIRTIO_NET_F_MQ | VIRTIO_NET_F_RSS;
        assert_eq!(
            plan_mq_rss(required, 8, 8, 9),
            MqRssPlan::Multi { pairs: 8 }
        );
        assert_eq!(
            plan_mq_rss(required & !VIRTIO_NET_F_RSS, 8, 8, 9),
            MqRssPlan::Single(MqRssFallback::MissingFeatures)
        );
        assert_eq!(
            plan_mq_rss(required, 8, 1, 9),
            MqRssPlan::Single(MqRssFallback::TooFewQueuePairs)
        );
        assert_eq!(
            plan_mq_rss(required, 8, 8, 8),
            MqRssPlan::Single(MqRssFallback::MsixUnavailable)
        );
    }

    #[test]
    fn tx_layout_reserves_one_descriptor_for_device_header() {
        assert_eq!(tx_descriptor_count(1), Some(2));
        assert_eq!(tx_descriptor_count(7), Some(8));
        assert_eq!(tx_descriptor_count(0), None);
        assert_eq!(tx_descriptor_count(8), None);
    }

    #[test]
    fn irq_mask_changes_are_synced_for_noncoherent_dma() {
        let mut rx_avail: VirtqAvail = unsafe { core::mem::zeroed() };
        let mut tx_avail: VirtqAvail = unsafe { core::mem::zeroed() };
        let context = DmaContext::new(DmaConstraints::coherent_identity(), &COUNTING_MAPPER);
        let before = COUNTING_MAPPER.device_syncs.load(Ordering::Relaxed);
        let control = VirtioQueueIrq {
            transport: Arc::new(IrqTestTransport),
            rx_avail: &mut rx_avail,
            tx_avail: &mut tx_avail,
            rx_avail_sync: context.sync_handle(DmaSyncRegion {
                paddr: 1,
                vaddr: core::ptr::addr_of_mut!(rx_avail) as usize,
                len: core::mem::size_of::<VirtqAvail>(),
                direction: DmaDirection::ToDevice,
            }),
            tx_avail_sync: context.sync_handle(DmaSyncRegion {
                paddr: 2,
                vaddr: core::ptr::addr_of_mut!(tx_avail) as usize,
                len: core::mem::size_of::<VirtqAvail>(),
                direction: DmaDirection::ToDevice,
            }),
            uses_isr_status: true,
            waker: Mutex::new(None),
            masked: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            irq_total: AtomicU64::new(0),
            irq_mask: AtomicU64::new(0),
            irq_unmask: AtomicU64::new(0),
        };

        assert!(control.ack_and_mask());
        assert_eq!(rx_avail.flags & VIRTQ_AVAIL_F_NO_INTERRUPT, 1);
        assert_eq!(tx_avail.flags & VIRTQ_AVAIL_F_NO_INTERRUPT, 1);
        control.unmask();
        assert_eq!(rx_avail.flags & VIRTQ_AVAIL_F_NO_INTERRUPT, 0);
        assert_eq!(tx_avail.flags & VIRTQ_AVAIL_F_NO_INTERRUPT, 0);
        assert_eq!(
            COUNTING_MAPPER.device_syncs.load(Ordering::Relaxed) - before,
            4
        );
    }

    #[test]
    fn rss_config_matches_virtio_12_layout() {
        let key = core::array::from_fn(|index| index as u8);
        let config = build_rss_config(
            4,
            40,
            VIRTIO_NET_RSS_TABLE_LEN,
            VIRTIO_NET_RSS_HASH_TYPES,
            &key,
        )
        .unwrap();
        assert_eq!(config.len(), 307);
        assert_eq!(u32::from_le_bytes(config[0..4].try_into().unwrap()), 0x3f);
        assert_eq!(u16::from_le_bytes(config[4..6].try_into().unwrap()), 127);
        assert_eq!(u16::from_le_bytes(config[6..8].try_into().unwrap()), 0);
        for bucket in 0..VIRTIO_NET_RSS_TABLE_LEN as usize {
            let offset = 8 + bucket * 2;
            assert_eq!(
                u16::from_le_bytes(config[offset..offset + 2].try_into().unwrap()),
                bucket as u16 % 4
            );
        }
        assert_eq!(u16::from_le_bytes(config[264..266].try_into().unwrap()), 4);
        assert_eq!(config[266], 40);
        assert_eq!(&config[267..], &key);
    }

    #[test]
    fn rss_config_rejects_incomplete_device_limits() {
        let key = [0u8; 40];
        assert!(build_rss_config(1, 40, 128, 0x3f, &key).is_err());
        assert!(build_rss_config(2, 39, 128, 0x3f, &key).is_err());
        assert!(build_rss_config(2, 40, 127, 0x3f, &key).is_err());
        assert!(build_rss_config(2, 40, 128, 0x1f, &key).is_err());
    }

    #[test]
    fn every_mq_setup_failure_resets_before_single_queue() {
        for failure in [
            SetupStage::AllocateMsix,
            SetupStage::AcceptFeatures,
            SetupStage::SetupQueues,
            SetupStage::SetDriverOk,
            SetupStage::SetPairs,
            SetupStage::SetRss,
            SetupStage::ActivateMsix,
        ] {
            let mut backend = FakeMqBackend {
                fail: Some(failure),
                reset_count: 0,
                single_count: 0,
                release_count: 0,
                visited: Vec::new(),
            };
            let outcome = run_mq_setup_transaction(&mut backend, 4).unwrap();
            assert!(matches!(outcome, MqSetupOutcome::Single { .. }));
            assert_eq!(backend.reset_count, 1);
            assert_eq!(backend.single_count, 1);
            assert_eq!(
                backend.release_count,
                usize::from(failure != SetupStage::AllocateMsix)
            );
            assert_eq!(backend.visited.last(), Some(&failure));
        }
    }

    #[test]
    fn complete_mq_setup_does_not_enter_fallback() {
        let mut backend = FakeMqBackend {
            fail: None,
            reset_count: 0,
            single_count: 0,
            release_count: 0,
            visited: Vec::new(),
        };
        let outcome = run_mq_setup_transaction(&mut backend, 4).unwrap();
        assert!(matches!(outcome, MqSetupOutcome::Multi { .. }));
        assert_eq!(backend.reset_count, 0);
        assert_eq!(backend.single_count, 0);
        assert_eq!(backend.release_count, 0);
        assert_eq!(backend.visited.len(), 7);
    }
}

//! VirtIO-net 批量 queue、DMA pool 与 ELM 生命周期公共逻辑。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use spin::mutex::Mutex;

use general::dev::dma::{DmaContext, DmaDirection, new_netbuf_pool, new_shared_netbuf_pool};
use net::QueuePairId;
use net::buf::{
    CompletionBatch, NetBufLease, NetBufPoolOwner, PacketBatch, PacketChain, PacketLayout,
    PacketMetadata, RxRefillBatch, TxBatch, TxChecksum, TxPacket,
};
#[cfg(not(feature = "elm-integrated"))]
use net::device::PinnedNetQueueEndpoint;
#[cfg(not(feature = "elm-integrated"))]
use net::device::{
    NET_QUEUE_CALL_STATUS_INVALID, NET_QUEUE_CALL_STATUS_OK, NET_QUEUE_OP_HAS_PENDING,
    NET_QUEUE_OP_POLL_RX, NET_QUEUE_OP_QUIESCE, NET_QUEUE_OP_RECLAIM_TX, NET_QUEUE_OP_REFILL_RX,
    NET_QUEUE_OP_SUBMIT_TX,
};
use net::device::{
    NetDeviceHandle, NetDeviceRegisterErrorKind, NetDeviceRegistration, NetDeviceRemoveError,
    NetQueueEndpoint, NetQueueRegistration, QueueIrqControl,
};
use net::queue::{
    NetQueueCaps, NetQueuePair, QueueFatalError, RxBudget, RxPollResult, RxRefillResult,
    TxReclaimResult, TxSubmitResult,
};
use virtio::virtio_mmio::VirtioMmioTransport;
use virtio::{
    SplitVirtQueue, VIRTIO_PCI_RESET_SPIN_LIMIT, VIRTQ_AVAIL_F_NO_INTERRUPT, VIRTQ_DESC_F_WRITE,
    VIRTQ_USED_F_NO_NOTIFY, VirtioPciTransport, VirtqDescUpdate, virtq_need_event,
};

use crate::VIRTIO_NET_DEVICE_NAME;

const VIRTIO_NET_HEADER_LEN: u16 = 12;
const RX_DESCRIPTOR_OFFSET: u16 = 116;
const RX_FRAME_OFFSET: u16 = RX_DESCRIPTOR_OFFSET + VIRTIO_NET_HEADER_LEN;
const DMA_PAGE_SIZE: usize = 4096;
const TX_HEADER_SIZE: usize = 256;
const MAX_BATCH: usize = 32;
const MAX_RX_REFILL_PER_CALL: usize = 4;
const MAX_TX_DESCRIPTORS: usize = 18;
const VIRTIO_NET_HDR_F_NEEDS_CSUM: u8 = 1;

const fn virtio_tx_header(checksum: TxChecksum) -> [u8; VIRTIO_NET_HEADER_LEN as usize] {
    let mut header = [0; VIRTIO_NET_HEADER_LEN as usize];
    if let TxChecksum::Partial { start, offset } = checksum {
        let start = start.to_le_bytes();
        let offset = offset.to_le_bytes();
        header[0] = VIRTIO_NET_HDR_F_NEEDS_CSUM;
        header[6] = start[0];
        header[7] = start[1];
        header[8] = offset[0];
        header[9] = offset[1];
    }
    header
}

const _: () = {
    let header = virtio_tx_header(TxChecksum::Partial {
        start: 0x1234,
        offset: 0x5678,
    });
    assert!(header[0] == VIRTIO_NET_HDR_F_NEEDS_CSUM);
    assert!(header[6] == 0x34 && header[7] == 0x12);
    assert!(header[8] == 0x78 && header[9] == 0x56);
};

pub(crate) enum VirtioNetTransport {
    Mmio(Box<dyn VirtioMmioTransport>),
    Pci {
        transport: VirtioPciTransport,
        rx_queue: u16,
        tx_queue: u16,
        rx_notify: usize,
        tx_notify: usize,
    },
}

impl VirtioNetTransport {
    fn notify(&self, queue: u16) {
        match self {
            Self::Mmio(transport) => transport.notify_queue(u32::from(queue)),
            Self::Pci {
                transport,
                rx_queue,
                tx_queue,
                rx_notify,
                tx_notify,
            } => {
                let address = if queue == *rx_queue {
                    *rx_notify
                } else {
                    debug_assert_eq!(queue, *tx_queue);
                    *tx_notify
                };
                transport.notify_queue(address, queue);
            }
        }
    }

    fn reset_wait(&self) -> bool {
        match self {
            Self::Mmio(transport) => {
                transport.write_status(0);
                for _ in 0..VIRTIO_PCI_RESET_SPIN_LIMIT {
                    if transport.read_status() == 0 {
                        return true;
                    }
                    core::hint::spin_loop();
                }
                transport.read_status() == 0
            }
            Self::Pci { transport, .. } => transport.reset_wait(VIRTIO_PCI_RESET_SPIN_LIMIT),
        }
    }
}

struct PendingTx {
    packet: TxPacket,
    _header: NetBufLease,
    descriptors: u16,
}

pub(crate) struct VirtioNetQueue {
    id: QueuePairId,
    transport: VirtioNetTransport,
    rx: SplitVirtQueue,
    tx: SplitVirtQueue,
    rx_pending: Box<[Option<NetBufLease>]>,
    tx_pending: Box<[Option<PendingTx>]>,
    event_idx: bool,
    tx_checksum: bool,
    quiesced: bool,
}

impl VirtioNetQueue {
    pub(crate) fn new(
        id: QueuePairId,
        transport: VirtioNetTransport,
        rx: SplitVirtQueue,
        tx: SplitVirtQueue,
        event_idx: bool,
        tx_checksum: bool,
    ) -> Self {
        let rx_size = usize::from(rx.queue_size());
        let tx_size = usize::from(tx.queue_size());
        Self {
            id,
            transport,
            rx,
            tx,
            rx_pending: (0..rx_size)
                .map(|_| None)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            tx_pending: (0..tx_size)
                .map(|_| None)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            event_idx,
            tx_checksum,
            quiesced: false,
        }
    }

    fn notification_required(event_idx: bool, queue: &SplitVirtQueue, old: u16) -> bool {
        if event_idx {
            return queue
                .avail_event()
                .map(|event| virtq_need_event(event, queue.avail_idx(), old))
                .unwrap_or(true);
        }
        queue.used_flags() & VIRTQ_USED_F_NO_NOTIFY == 0
    }

    pub(crate) const fn caps_value(queue_size: u16, tx_checksum: bool) -> NetQueueCaps {
        NetQueueCaps {
            queue_size,
            scatter_gather: true,
            max_tx_descriptors: MAX_TX_DESCRIPTORS as u8,
            max_rx_batch: MAX_BATCH as u8,
            max_tx_batch: MAX_BATCH as u8,
            tx_checksum,
            udp_segmentation: false,
            max_udp_segments: 0,
        }
    }

    fn clear_pending(&mut self) {
        for pending in self.rx_pending.iter_mut() {
            let _ = pending.take();
        }
        for pending in self.tx_pending.iter_mut() {
            let _ = pending.take();
        }
    }
}

impl Drop for VirtioNetQueue {
    fn drop(&mut self) {
        if !self.transport.reset_wait() {
            panic!("virtio-net: device reset timed out before queue DMA teardown");
        }
        self.clear_pending();
    }
}

impl NetQueuePair for VirtioNetQueue {
    fn id(&self) -> QueuePairId {
        self.id
    }

    fn caps(&self) -> NetQueueCaps {
        Self::caps_value(self.rx.queue_size(), self.tx_checksum)
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
        let mut heads = [0u16; MAX_BATCH];
        let mut slots = [0usize; MAX_BATCH];
        let mut posted = 0usize;
        for index in 0..original_len.min(MAX_RX_REFILL_PER_CALL) {
            let Some(lease) = batch.take(index) else {
                continue;
            };
            if lease.sync_for_device().is_err() {
                let _ = batch.put(index, lease);
                return RxRefillResult {
                    posted: posted as u16,
                    descriptor_starved: false,
                    fatal: Some(QueueFatalError::DmaFault),
                };
            }
            let dma = match lease.dma_addr() {
                Ok(Some(address)) => address,
                _ => {
                    let _ = batch.put(index, lease);
                    break;
                }
            };
            let chain = match self.rx.alloc_chain(1) {
                Ok(chain) => chain,
                Err(virtio::VirtQueueError::QueueFull) => {
                    let _ = batch.put(index, lease);
                    break;
                }
                Err(_) => {
                    let _ = batch.put(index, lease);
                    return RxRefillResult {
                        posted: posted as u16,
                        descriptor_starved: false,
                        fatal: Some(QueueFatalError::RingCorrupt),
                    };
                }
            };
            let head = chain.head();
            if self.rx_pending[usize::from(head)].is_some()
                || self
                    .rx
                    .write_desc(head, dma, lease.len() as u32, VIRTQ_DESC_F_WRITE, None)
                    .is_err()
            {
                let _ = self.rx.free_chain(chain);
                let _ = batch.put(index, lease);
                return RxRefillResult {
                    posted: posted as u16,
                    descriptor_starved: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            }
            self.rx_pending[usize::from(head)] = Some(lease);
            heads[posted] = head;
            slots[posted] = index;
            posted += 1;
        }
        let old_avail = self.rx.avail_idx();
        if posted != 0 && self.rx.push_avail_many(&heads[..posted]).is_err() {
            for position in 0..posted {
                let head = heads[position];
                if let Some(lease) = self.rx_pending[usize::from(head)].take() {
                    let _ = batch.put(slots[position], lease);
                }
                let _ = self.rx.free_chain_from_head(head);
            }
            return RxRefillResult {
                posted: 0,
                descriptor_starved: false,
                fatal: Some(QueueFatalError::RingCorrupt),
            };
        }
        if posted != 0 && Self::notification_required(self.event_idx, &self.rx, old_avail) {
            self.transport.notify(self.id.0.saturating_mul(2));
        }
        RxRefillResult {
            posted: posted as u16,
            descriptor_starved: self.rx.free_descriptor_count() == 0 && !batch.is_empty(),
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
        let mut packets = 0u16;
        let mut bytes = 0u32;
        while packets < budget.packets && usize::from(packets) < MAX_BATCH {
            let used = match self.rx.pop_used() {
                Ok(Some(used)) => used,
                Ok(None) => break,
                Err(_) => {
                    return RxPollResult {
                        packets,
                        bytes,
                        ring_empty: false,
                        descriptor_starved: false,
                        fatal: Some(QueueFatalError::RingCorrupt),
                    };
                }
            };
            let Some(mut lease) = self.rx_pending[usize::from(used.head)].take() else {
                return RxPollResult {
                    packets,
                    bytes,
                    ring_empty: false,
                    descriptor_starved: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            };
            if self.rx.free_chain_from_head(used.head).is_err() {
                return RxPollResult {
                    packets,
                    bytes,
                    ring_empty: false,
                    descriptor_starved: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            }
            if used.len < u32::from(VIRTIO_NET_HEADER_LEN)
                || used.len > (DMA_PAGE_SIZE - usize::from(RX_DESCRIPTOR_OFFSET)) as u32
            {
                drop(lease);
                continue;
            }
            let frame_len = used.len - u32::from(VIRTIO_NET_HEADER_LEN);
            if lease
                .set_data_range(RX_FRAME_OFFSET, frame_len as u16)
                .is_err()
            {
                drop(lease);
                continue;
            }
            let metadata = PacketMetadata {
                frame_len,
                checksums_validated: false,
                layout: PacketLayout::Plain,
                ..PacketMetadata::default()
            };
            let chain = PacketChain::from_lease(lease);
            if out.push(chain, metadata).is_err() {
                return RxPollResult {
                    packets,
                    bytes,
                    ring_empty: false,
                    descriptor_starved: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            }
            packets += 1;
            bytes = bytes.saturating_add(frame_len);
            if bytes >= budget.bytes {
                break;
            }
        }
        let ring_empty = match self.rx.has_used() {
            Ok(pending) => !pending,
            Err(_) => {
                return RxPollResult {
                    packets,
                    bytes,
                    ring_empty: false,
                    descriptor_starved: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            }
        };
        RxPollResult {
            packets,
            bytes,
            ring_empty,
            descriptor_starved: self.rx.free_descriptor_count() != 0
                && self.rx_pending.iter().all(Option::is_none),
            fatal: None,
        }
    }

    fn reclaim_tx_batch(&mut self, out: &mut CompletionBatch) -> TxReclaimResult {
        let mut completions = 0u16;
        let mut descriptors = 0u16;
        while usize::from(completions) < MAX_BATCH {
            let used = match self.tx.pop_used() {
                Ok(Some(used)) => used,
                Ok(None) => break,
                Err(_) => {
                    return TxReclaimResult {
                        completions,
                        descriptors,
                        ring_empty: false,
                        fatal: Some(QueueFatalError::RingCorrupt),
                    };
                }
            };
            let Some(pending) = self.tx_pending[usize::from(used.head)].take() else {
                return TxReclaimResult {
                    completions,
                    descriptors,
                    ring_empty: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            };
            if self.tx.free_chain_from_head(used.head).is_err() {
                return TxReclaimResult {
                    completions,
                    descriptors,
                    ring_empty: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            }
            let token = pending.packet.completion;
            descriptors = descriptors.saturating_add(pending.descriptors);
            drop(pending);
            if out.push(token).is_err() {
                return TxReclaimResult {
                    completions,
                    descriptors,
                    ring_empty: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            }
            completions += 1;
        }
        let ring_empty = match self.tx.has_used() {
            Ok(pending) => !pending,
            Err(_) => {
                return TxReclaimResult {
                    completions,
                    descriptors,
                    ring_empty: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            }
        };
        TxReclaimResult {
            completions,
            descriptors,
            ring_empty,
            fatal: None,
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
        let mut heads = [0u16; MAX_BATCH];
        let mut slots = [0usize; MAX_BATCH];
        let mut submitted = 0usize;
        let mut descriptor_total = 0u16;
        let mut byte_total = 0u32;
        for index in 0..original_len.min(MAX_BATCH) {
            let Some(candidate) = batch.packet(index) else {
                continue;
            };
            let fragment_count = candidate.chain.fragment_count();
            let descriptor_count = fragment_count + 1;
            let checksum_valid = candidate
                .checksum
                .valid_for(self.tx_checksum, candidate.chain.total_len());
            if descriptor_count > MAX_TX_DESCRIPTORS
                || !checksum_valid
                || (0..fragment_count).any(|fragment| {
                    candidate
                        .chain
                        .fragment(fragment)
                        .and_then(|fragment| fragment.dma_addr().ok().flatten())
                        .is_none()
                })
            {
                break;
            }
            let Ok(mut header) =
                header_pool.lease(0, VIRTIO_NET_HEADER_LEN, PacketMetadata::default())
            else {
                break;
            };
            let header_bytes = header
                .as_mut_slice()
                .expect("VirtIO-net TX header lease 范围有效");
            header_bytes.copy_from_slice(&virtio_tx_header(candidate.checksum));
            if header.sync_for_device().is_err() {
                break;
            }
            let Some(packet) = batch.take(index) else {
                break;
            };
            let chain = match self.tx.alloc_chain(descriptor_count) {
                Ok(chain) => chain,
                Err(virtio::VirtQueueError::QueueFull) => {
                    let _ = batch.put(index, packet);
                    break;
                }
                Err(_) => {
                    let _ = batch.put(index, packet);
                    return TxSubmitResult {
                        packets: submitted as u16,
                        descriptors: descriptor_total,
                        bytes: byte_total,
                        queue_full: false,
                        fatal: Some(QueueFatalError::RingCorrupt),
                    };
                }
            };
            let descriptors = chain.as_slice();
            let mut updates = [VirtqDescUpdate::new(0, 0, 0, 0, None); MAX_TX_DESCRIPTORS];
            let header_dma = header
                .dma_addr()
                .expect("VirtIO-net TX header DMA 身份有效")
                .expect("VirtIO-net TX header 必须来自 DMA pool");
            updates[0] = VirtqDescUpdate::new(
                descriptors[0],
                header_dma,
                u32::from(VIRTIO_NET_HEADER_LEN),
                0,
                descriptors.get(1).copied(),
            );
            let mut update_failed = false;
            for fragment_index in 0..fragment_count {
                let fragment = packet
                    .chain
                    .fragment(fragment_index)
                    .expect("TX fragment 索引有效");
                if fragment.sync_for_device().is_err() {
                    update_failed = true;
                    break;
                }
                let Some(dma) = fragment.dma_addr().ok().flatten() else {
                    update_failed = true;
                    break;
                };
                updates[fragment_index + 1] = VirtqDescUpdate::new(
                    descriptors[fragment_index + 1],
                    dma,
                    fragment.len() as u32,
                    0,
                    descriptors.get(fragment_index + 2).copied(),
                );
            }
            if update_failed
                || self.tx.write_descs(&updates[..descriptor_count]).is_err()
                || self.tx_pending[usize::from(chain.head())].is_some()
            {
                let _ = self.tx.free_chain(chain);
                let _ = batch.put(index, packet);
                return TxSubmitResult {
                    packets: submitted as u16,
                    descriptors: descriptor_total,
                    bytes: byte_total,
                    queue_full: false,
                    fatal: Some(QueueFatalError::RingCorrupt),
                };
            }
            let head = chain.head();
            let frame_len = packet.chain.total_len() as u32;
            self.tx_pending[usize::from(head)] = Some(PendingTx {
                packet,
                _header: header,
                descriptors: descriptor_count as u16,
            });
            heads[submitted] = head;
            slots[submitted] = index;
            submitted += 1;
            descriptor_total = descriptor_total.saturating_add(descriptor_count as u16);
            byte_total = byte_total.saturating_add(frame_len);
        }
        let old_avail = self.tx.avail_idx();
        if submitted != 0 && self.tx.push_avail_many(&heads[..submitted]).is_err() {
            for position in 0..submitted {
                let head = heads[position];
                if let Some(pending) = self.tx_pending[usize::from(head)].take() {
                    let _ = batch.put(slots[position], pending.packet);
                }
                let _ = self.tx.free_chain_from_head(head);
            }
            return TxSubmitResult {
                packets: 0,
                descriptors: 0,
                bytes: 0,
                queue_full: false,
                fatal: Some(QueueFatalError::RingCorrupt),
            };
        }
        if submitted != 0 && Self::notification_required(self.event_idx, &self.tx, old_avail) {
            self.transport
                .notify(self.id.0.saturating_mul(2).saturating_add(1));
        }
        TxSubmitResult {
            packets: submitted as u16,
            descriptors: descriptor_total,
            bytes: byte_total,
            queue_full: submitted != original_len,
            fatal: None,
        }
    }

    fn has_pending_work(&mut self) -> bool {
        self.rx.has_used().unwrap_or(true) || self.tx.has_used().unwrap_or(true)
    }

    fn quiesce(&mut self) -> Result<(), QueueFatalError> {
        self.quiesced = true;
        self.rx.set_avail_flags(VIRTQ_AVAIL_F_NO_INTERRUPT);
        self.tx.set_avail_flags(VIRTQ_AVAIL_F_NO_INTERRUPT);
        if !self.transport.reset_wait() {
            return Err(QueueFatalError::DeviceReset);
        }
        // reset 后设备不再访问 descriptor，必须在常驻 pool 释放前归还全部 lease。
        self.clear_pending();
        Ok(())
    }
}

#[cfg(feature = "elm-integrated")]
struct SharedVirtioNetQueue {
    inner: Arc<Mutex<VirtioNetQueue>>,
}

#[cfg(feature = "elm-integrated")]
impl NetQueuePair for SharedVirtioNetQueue {
    fn id(&self) -> QueuePairId {
        self.inner.lock().id()
    }

    fn caps(&self) -> NetQueueCaps {
        self.inner.lock().caps()
    }

    fn refill_rx_batch(&mut self, batch: &mut RxRefillBatch) -> RxRefillResult {
        self.inner.lock().refill_rx_batch(batch)
    }

    fn poll_rx_batch(&mut self, budget: RxBudget, out: &mut PacketBatch) -> RxPollResult {
        self.inner.lock().poll_rx_batch(budget, out)
    }

    fn reclaim_tx_batch(&mut self, out: &mut CompletionBatch) -> TxReclaimResult {
        self.inner.lock().reclaim_tx_batch(out)
    }

    fn submit_tx_batch(
        &mut self,
        batch: &mut TxBatch,
        header_pool: &mut NetBufPoolOwner,
    ) -> TxSubmitResult {
        self.inner.lock().submit_tx_batch(batch, header_pool)
    }

    fn has_pending_work(&mut self) -> bool {
        self.inner.lock().has_pending_work()
    }

    fn quiesce(&mut self) -> Result<(), QueueFatalError> {
        self.inner.lock().quiesce()
    }
}

struct ActiveDevice {
    queues: Box<[Arc<Mutex<VirtioNetQueue>>]>,
    _control_queue: Option<SplitVirtQueue>,
    handle: Option<NetDeviceHandle>,
}

static ACTIVE_DEVICE: Mutex<Option<ActiveDevice>> = Mutex::new(None);

fn dma_pool(
    context: &DmaContext,
    count: usize,
    size: usize,
    align: usize,
    direction: DmaDirection,
) -> Result<NetBufPoolOwner, NetDeviceRegisterErrorKind> {
    new_netbuf_pool(context.clone(), count, size, align, direction)
        .map_err(|_| NetDeviceRegisterErrorKind::ResourceExhausted)
}

pub(crate) fn install_active(
    queue: VirtioNetQueue,
    context: DmaContext,
    irq: Arc<dyn QueueIrqControl>,
    mac_address: [u8; 6],
    mtu: u32,
) -> Result<NetDeviceHandle, NetDeviceRegisterErrorKind> {
    install_active_queues(alloc::vec![(queue, irq)], None, context, mac_address, mtu)
}

pub(crate) fn install_active_queues(
    queues: Vec<(VirtioNetQueue, Arc<dyn QueueIrqControl>)>,
    control_queue: Option<SplitVirtQueue>,
    context: DmaContext,
    mac_address: [u8; 6],
    mtu: u32,
) -> Result<NetDeviceHandle, NetDeviceRegisterErrorKind> {
    if queues.is_empty()
        || queues
            .iter()
            .enumerate()
            .any(|(index, (queue, _))| queue.id() != QueuePairId(index as u16))
    {
        return Err(NetDeviceRegisterErrorKind::InvalidRegistration);
    }
    let mut active_queues = Vec::new();
    let mut registrations = Vec::new();
    active_queues
        .try_reserve_exact(queues.len())
        .map_err(|_| NetDeviceRegisterErrorKind::ResourceExhausted)?;
    registrations
        .try_reserve_exact(queues.len())
        .map_err(|_| NetDeviceRegisterErrorKind::ResourceExhausted)?;
    for (queue, irq) in queues {
        let id = queue.id();
        let queue_size = usize::from(queue.rx.queue_size());
        let queue = Arc::new(Mutex::new(queue));
        let rx_pool = dma_pool(
            &context,
            queue_size,
            DMA_PAGE_SIZE,
            DMA_PAGE_SIZE,
            DmaDirection::FromDevice,
        )?;
        let tx_header_pool = dma_pool(
            &context,
            queue_size,
            TX_HEADER_SIZE,
            64,
            DmaDirection::ToDevice,
        )?;
        let tx_payload_pool = new_shared_netbuf_pool(
            context.clone(),
            queue_size,
            DMA_PAGE_SIZE,
            DMA_PAGE_SIZE,
            DmaDirection::ToDevice,
        )
        .map_err(|_| NetDeviceRegisterErrorKind::ResourceExhausted)?;
        let socket_tx_pool = new_shared_netbuf_pool(
            context.clone(),
            queue_size.saturating_mul(net::tuning::SOCKET_TX_POOL_DEPTH_MULTIPLIER),
            DMA_PAGE_SIZE,
            DMA_PAGE_SIZE,
            DmaDirection::ToDevice,
        )
        .map_err(|_| NetDeviceRegisterErrorKind::ResourceExhausted)?;
        #[cfg(not(feature = "elm-integrated"))]
        let endpoint = {
            let caps = queue.lock().caps();
            NetQueueEndpoint::Pinned(
                PinnedNetQueueEndpoint::current(
                    "net.virtio.queue-call",
                    "mygo.net.queue-call@1",
                    1,
                    id,
                    caps,
                    false,
                )
                .ok_or(NetDeviceRegisterErrorKind::InvalidRegistration)?,
            )
        };
        #[cfg(feature = "elm-integrated")]
        let endpoint = NetQueueEndpoint::Integrated(Box::new(SharedVirtioNetQueue {
            inner: Arc::clone(&queue),
        }));
        registrations.push(NetQueueRegistration {
            id,
            queue: endpoint,
            rx_pool,
            tx_header_pool,
            tx_payload_pool,
            socket_tx_pool,
            irq,
        });
        active_queues.push(queue);
    }
    {
        let mut active = ACTIVE_DEVICE.lock();
        if active.is_some() {
            return Err(NetDeviceRegisterErrorKind::ResourceExhausted);
        }
        *active = Some(ActiveDevice {
            queues: active_queues.into_boxed_slice(),
            _control_queue: control_queue,
            handle: None,
        });
    }
    let registration = NetDeviceRegistration::new(
        VIRTIO_NET_DEVICE_NAME.into(),
        mac_address,
        mtu,
        true,
        registrations.into_boxed_slice(),
    );
    match net::device::register_device(registration) {
        Ok(handle) => {
            ACTIVE_DEVICE
                .lock()
                .as_mut()
                .expect("VirtIO-net active slot 必须存在")
                .handle = Some(handle);
            Ok(handle)
        }
        Err(error) => {
            let _ = ACTIVE_DEVICE.lock().take();
            Err(error.kind)
        }
    }
}

pub(crate) fn quiesce_active() -> Result<(), NetDeviceRemoveError> {
    let active = ACTIVE_DEVICE.lock();
    if let Some(active) = active.as_ref() {
        for queue in active.queues.iter() {
            queue
                .lock()
                .quiesce()
                .map_err(|_| NetDeviceRemoveError::Busy)?;
        }
    }
    Ok(())
}

pub(crate) fn detach_active() -> Result<(), NetDeviceRemoveError> {
    let handle = ACTIVE_DEVICE
        .lock()
        .as_ref()
        .and_then(|active| active.handle);
    let Some(handle) = handle else {
        return Ok(());
    };
    match net::device::begin_remove(handle) {
        Ok(_) | Err(NetDeviceRemoveError::NoDevice) => {
            if let Some(active) = ACTIVE_DEVICE.lock().as_mut() {
                active.handle = None;
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn destroy_active() {
    let _ = ACTIVE_DEVICE.lock().take();
}

pub(crate) fn remove_active_from_pnp() -> Result<(), NetDeviceRemoveError> {
    quiesce_active()?;
    detach_active()?;
    destroy_active();
    Ok(())
}

#[cfg(not(feature = "elm-integrated"))]
#[elm::export(
    name = "net.virtio.queue-call",
    contract = "mygo.net.queue-call@1",
    version = 1,
    mode = "direct-pinned",
    visibility = "private"
)]
fn virtio_net_queue_call(frame: &mut net::device::NetQueueCall) -> i32 {
    if !frame.valid(frame.opcode, frame.queue_id) {
        return NET_QUEUE_CALL_STATUS_INVALID;
    }
    let active = ACTIVE_DEVICE.lock();
    let Some(active) = active.as_ref() else {
        return NET_QUEUE_CALL_STATUS_INVALID;
    };
    let Some(queue) = active.queues.get(frame.queue_id.0 as usize) else {
        return NET_QUEUE_CALL_STATUS_INVALID;
    };
    let mut queue = queue.lock();
    if queue.id() != frame.queue_id {
        return NET_QUEUE_CALL_STATUS_INVALID;
    }
    match frame.opcode {
        NET_QUEUE_OP_REFILL_RX => {
            let Some(batch) = (unsafe { frame.refill_batch.as_mut() }) else {
                return NET_QUEUE_CALL_STATUS_INVALID;
            };
            frame.rx_refill_result = queue.refill_rx_batch(batch);
        }
        NET_QUEUE_OP_POLL_RX => {
            let Some(batch) = (unsafe { frame.packet_batch.as_mut() }) else {
                return NET_QUEUE_CALL_STATUS_INVALID;
            };
            frame.rx_poll_result = queue.poll_rx_batch(frame.budget, batch);
        }
        NET_QUEUE_OP_RECLAIM_TX => {
            let Some(batch) = (unsafe { frame.completion_batch.as_mut() }) else {
                return NET_QUEUE_CALL_STATUS_INVALID;
            };
            frame.tx_reclaim_result = queue.reclaim_tx_batch(batch);
        }
        NET_QUEUE_OP_SUBMIT_TX => {
            let Some(batch) = (unsafe { frame.tx_batch.as_mut() }) else {
                return NET_QUEUE_CALL_STATUS_INVALID;
            };
            let Some(header_pool) = (unsafe { frame.tx_header_pool.as_mut() }) else {
                return NET_QUEUE_CALL_STATUS_INVALID;
            };
            frame.tx_submit_result = queue.submit_tx_batch(batch, header_pool);
        }
        NET_QUEUE_OP_HAS_PENDING => frame.pending = queue.has_pending_work(),
        NET_QUEUE_OP_QUIESCE => frame.quiesce_result = queue.quiesce().err(),
        _ => return NET_QUEUE_CALL_STATUS_INVALID,
    }
    NET_QUEUE_CALL_STATUS_OK
}

//! 批量队列的 loopback ELM 设备。

use alloc::boxed::Box;
use alloc::vec;

use spin::Mutex;

use net::QueuePairId;
use net::buf::{
    CompletionBatch, CompletionToken, NetBufPoolOwner, PacketBatch, PacketLayout, PacketMetadata,
    RxRefillBatch, TxBatch,
};
use net::device::{
    NET_QUEUE_CALL_STATUS_INVALID, NET_QUEUE_CALL_STATUS_OK, NET_QUEUE_OP_HAS_PENDING,
    NET_QUEUE_OP_POLL_RX, NET_QUEUE_OP_QUIESCE, NET_QUEUE_OP_RECLAIM_TX, NET_QUEUE_OP_REFILL_RX,
    NET_QUEUE_OP_SUBMIT_TX, NetDeviceHandle, NetDeviceRegisterErrorKind, NetDeviceRegistration,
    NetDeviceRemoveError, NetQueueRegistration,
};
#[cfg(not(feature = "elm-integrated"))]
use net::device::PinnedNetQueueEndpoint;
use net::queue::{
    NetQueueCaps, NetQueuePair, QueueFatalError, RxBudget, RxPollResult, RxRefillResult,
    TxReclaimResult, TxSubmitResult,
};

const LOOPBACK_RING_SIZE: usize = 1024;
const LOOPBACK_MTU: u32 = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopbackError {
    Pool,
    Context,
    Register(NetDeviceRegisterErrorKind),
    Remove(NetDeviceRemoveError),
}

struct LoopbackQueue {
    packets: Box<[Option<net::buf::PacketChain>]>,
    metadata: Box<[Option<PacketMetadata>]>,
    completions: Box<[Option<CompletionToken>]>,
    rx_reserve: Box<[Option<net::buf::NetBufLease>]>,
    rx_reserve_len: usize,
    packet_head: usize,
    packet_tail: usize,
    packet_count: usize,
    completion_head: usize,
    completion_tail: usize,
    completion_count: usize,
    quiesced: bool,
}

static QUEUE: Mutex<Option<LoopbackQueue>> = Mutex::new(None);

impl LoopbackQueue {
    const fn caps_value() -> NetQueueCaps {
        NetQueueCaps {
            queue_size: 256,
            scatter_gather: true,
            max_tx_descriptors: 18,
            max_rx_batch: 32,
            max_tx_batch: 32,
            udp_segmentation: true,
            max_udp_segments: 16,
        }
    }

    fn new() -> Self {
        Self {
            packets: (0..LOOPBACK_RING_SIZE)
                .map(|_| None)
                .collect::<alloc::vec::Vec<_>>()
                .into_boxed_slice(),
            metadata: vec![None; LOOPBACK_RING_SIZE].into_boxed_slice(),
            completions: vec![None; LOOPBACK_RING_SIZE].into_boxed_slice(),
            rx_reserve: (0..256)
                .map(|_| None)
                .collect::<alloc::vec::Vec<_>>()
                .into_boxed_slice(),
            rx_reserve_len: 0,
            packet_head: 0,
            packet_tail: 0,
            packet_count: 0,
            completion_head: 0,
            completion_tail: 0,
            completion_count: 0,
            quiesced: false,
        }
    }
}

impl NetQueuePair for LoopbackQueue {
    fn id(&self) -> QueuePairId {
        QueuePairId(0)
    }

    fn caps(&self) -> NetQueueCaps {
        Self::caps_value()
    }

    fn tx_produces_rx_synchronously(&self) -> bool {
        true
    }

    fn refill_rx_batch(&mut self, batch: &mut RxRefillBatch) -> RxRefillResult {
        let original_len = batch.len();
        let mut posted = 0u16;
        for index in 0..original_len {
            if self.rx_reserve_len == self.rx_reserve.len() {
                break;
            }
            let Some(lease) = batch.take(index) else {
                continue;
            };
            self.rx_reserve[self.rx_reserve_len] = Some(lease);
            self.rx_reserve_len += 1;
            posted += 1;
        }
        RxRefillResult {
            posted,
            descriptor_starved: self.rx_reserve_len == self.rx_reserve.len() && !batch.is_empty(),
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
        while self.packet_count != 0 && packets < budget.packets && packets < 32 {
            let stored_len = self.packets[self.packet_head]
                .as_ref()
                .map(|packet| packet.total_len() as u32)
                .unwrap_or(0);
            let metadata = self.metadata[self.packet_head].unwrap_or_default();
            let (logical_packets, logical_bytes) = match metadata.layout {
                PacketLayout::Plain => (1u16, stored_len),
                PacketLayout::UdpSegments(layout) => {
                    let logical_bytes = (usize::from(layout.header_len)
                        + usize::from(layout.payload_len))
                    .saturating_mul(usize::from(layout.segment_count));
                    (
                        u16::from(layout.segment_count),
                        logical_bytes.min(u32::MAX as usize) as u32,
                    )
                }
            };
            if packets != 0
                && (packets.saturating_add(logical_packets) > budget.packets
                    || bytes.saturating_add(logical_bytes) > budget.bytes)
            {
                break;
            }
            let packet = self.packets[self.packet_head]
                .take()
                .expect("loopback ring 出现空洞");
            let metadata = self.metadata[self.packet_head].take().unwrap_or_default();
            self.packet_head = (self.packet_head + 1) % LOOPBACK_RING_SIZE;
            self.packet_count -= 1;
            out.push(packet, metadata)
                .unwrap_or_else(|_| unreachable!());
            packets = packets.saturating_add(logical_packets);
            bytes = bytes.saturating_add(logical_bytes);
        }
        RxPollResult {
            packets,
            bytes,
            ring_empty: self.packet_count == 0,
            descriptor_starved: false,
            fatal: None,
        }
    }

    fn reclaim_tx_batch(&mut self, out: &mut CompletionBatch) -> TxReclaimResult {
        let mut completions = 0u16;
        while self.completion_count != 0 && completions < 32 {
            let token = self.completions[self.completion_head]
                .take()
                .expect("loopback completion ring 出现空洞");
            self.completion_head = (self.completion_head + 1) % LOOPBACK_RING_SIZE;
            self.completion_count -= 1;
            out.push(token).unwrap_or_else(|_| unreachable!());
            completions += 1;
        }
        TxReclaimResult {
            completions,
            descriptors: completions,
            ring_empty: self.completion_count == 0,
            fatal: None,
        }
    }

    fn submit_tx_batch(
        &mut self,
        batch: &mut TxBatch,
        _header_pool: &mut NetBufPoolOwner,
    ) -> TxSubmitResult {
        let mut packets = 0u16;
        let mut descriptors = 0u16;
        let mut bytes = 0u32;
        let original_len = batch.len();
        for index in 0..original_len {
            if self.packet_count == LOOPBACK_RING_SIZE
                || self.completion_count == LOOPBACK_RING_SIZE
            {
                break;
            }
            let Some(packet) = batch.take(index) else {
                continue;
            };
            let stored_len = packet.chain.total_len() as u32;
            let logical_bytes = match packet.layout {
                PacketLayout::Plain => stored_len,
                PacketLayout::UdpSegments(layout) => (usize::from(layout.header_len)
                    + usize::from(layout.payload_len))
                .saturating_mul(usize::from(layout.segment_count))
                .min(u32::MAX as usize) as u32,
            };
            descriptors = descriptors.saturating_add(packet.chain.fragment_count() as u16);
            bytes = bytes.saturating_add(logical_bytes);
            self.packets[self.packet_tail] = Some(packet.chain);
            self.metadata[self.packet_tail] = Some(PacketMetadata {
                frame_len: logical_bytes,
                checksums_validated: packet.checksums_validated,
                layout: packet.layout,
                ..PacketMetadata::default()
            });
            self.packet_tail = (self.packet_tail + 1) % LOOPBACK_RING_SIZE;
            self.packet_count += 1;
            self.completions[self.completion_tail] = Some(packet.completion);
            self.completion_tail = (self.completion_tail + 1) % LOOPBACK_RING_SIZE;
            self.completion_count += 1;
            packets += 1;
        }
        TxSubmitResult {
            packets,
            descriptors,
            bytes,
            queue_full: packets as usize != original_len,
            fatal: None,
        }
    }

    fn has_pending_work(&mut self) -> bool {
        self.packet_count != 0 || self.completion_count != 0
    }

    fn quiesce(&mut self) -> Result<(), QueueFatalError> {
        self.quiesced = true;
        Ok(())
    }
}

pub(crate) struct LoopbackHandle {
    handle: NetDeviceHandle,
}

pub(crate) fn register() -> Result<LoopbackHandle, LoopbackError> {
    #[cfg(not(feature = "elm-integrated"))]
    let queue = {
        let caps = LoopbackQueue::caps_value();
        let endpoint = PinnedNetQueueEndpoint::current(
            "net.loopback.queue-call",
            "mygo.net.queue-call@1",
            1,
            QueuePairId(0),
            caps,
            true,
        )
        .ok_or(LoopbackError::Context)?;
        NetQueueRegistration::pinned_heap(endpoint, 48, 4096, 256, 256, 256, 4096)
            .map_err(|_| LoopbackError::Pool)?
    };
    #[cfg(feature = "elm-integrated")]
    let queue = {
        NetQueueRegistration::integrated_heap(
            Box::new(LoopbackQueue::new()),
            48,
            4096,
            256,
            256,
            256,
            4096,
        )
        .map_err(|_| LoopbackError::Pool)?
    };
    let registration = NetDeviceRegistration::new(
        "lo".into(),
        [0; 6],
        LOOPBACK_MTU,
        true,
        vec![queue].into_boxed_slice(),
    );
    let handle = net::device::register_device(registration)
        .map_err(|error| LoopbackError::Register(error.kind))?;
    log::printk!("[loopback] registered batch queue");
    Ok(LoopbackHandle { handle })
}

impl LoopbackHandle {
    pub(crate) fn unregister(&self) -> Result<(), LoopbackError> {
        match net::device::begin_remove(self.handle) {
            Ok(_) | Err(NetDeviceRemoveError::NoDevice) => Ok(()),
            Err(error) => Err(LoopbackError::Remove(error)),
        }
    }
}

#[cfg(feature = "elm-integrated")]
pub(crate) fn create_queue() -> Result<(), LoopbackError> {
    Ok(())
}

#[cfg(not(feature = "elm-integrated"))]
pub(crate) fn create_queue() -> Result<(), LoopbackError> {
    let mut queue = QUEUE.lock();
    if queue.is_some() {
        return Err(LoopbackError::Context);
    }
    *queue = Some(LoopbackQueue::new());
    Ok(())
}

pub(crate) fn quiesce_queue() {
    if let Some(queue) = QUEUE.lock().as_mut() {
        let _ = queue.quiesce();
    }
}

pub(crate) fn destroy_queue() {
    *QUEUE.lock() = None;
}

#[elm::export(
    name = "net.loopback.queue-call",
    contract = "mygo.net.queue-call@1",
    version = 1,
    mode = "direct-pinned",
    visibility = "private"
)]
fn loopback_queue_call(frame: &mut net::device::NetQueueCallV1) -> i32 {
    if !frame.valid(frame.opcode, QueuePairId(0)) {
        return NET_QUEUE_CALL_STATUS_INVALID;
    }
    let mut slot = QUEUE.lock();
    let Some(queue) = slot.as_mut() else {
        return NET_QUEUE_CALL_STATUS_INVALID;
    };
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

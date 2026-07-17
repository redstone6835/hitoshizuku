//! 批量队列的 loopback 设备。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Mutex;

use net::QueuePairId;
use net::buf::{
    CompletionBatch, CompletionToken, NetBufPool, NetBufPoolOwner, NetBufStorage, PacketBatch,
    PacketMetadata, RxRefillBatch, TxBatch,
};
use net::device::{
    NetDeviceRegistration, NetQueueRegistration, QueueIrqControl, QueueIrqError, QueueIrqStats,
    QueueWakeHandle,
};
use net::queue::{
    NetQueueCaps, NetQueuePair, QueueFatalError, RxBudget, RxPollResult, RxRefillResult,
    TxReclaimResult, TxSubmitResult,
};

use crate::dev::pnp::{PnpError, PnpResourceKind};

const LOOPBACK_RING_SIZE: usize = 1024;
const LOOPBACK_MTU: u32 = 65_536;

struct HeapStorage {
    bytes: Box<[u8]>,
}

impl HeapStorage {
    fn new(size: usize) -> Self {
        Self {
            bytes: vec![0; size].into_boxed_slice(),
        }
    }
}

impl NetBufStorage for HeapStorage {
    fn capacity(&self) -> usize {
        self.bytes.len()
    }

    fn base_ptr(&self) -> NonNull<u8> {
        NonNull::new(self.bytes.as_ptr() as *mut u8).expect("loopback storage 地址为空")
    }

    fn dma_addr(&self) -> Option<u64> {
        None
    }

    fn sync_for_cpu(&self, _offset: usize, _len: usize) {}
    fn sync_for_device(&self, _offset: usize, _len: usize) {}
}

fn make_pool(count: usize, size: usize) -> Result<NetBufPoolOwner, PnpError> {
    let storages = (0..count)
        .map(|_| Box::new(HeapStorage::new(size)) as Box<dyn NetBufStorage>)
        .collect::<alloc::vec::Vec<_>>()
        .into_boxed_slice();
    NetBufPool::new(storages)
        .map_err(|_| PnpError::registration_failed(PnpResourceKind::Function, "loopback pool"))
}

struct LoopbackIrq {
    pending: AtomicBool,
    masked: AtomicBool,
    waker: Mutex<Option<Arc<dyn QueueWakeHandle>>>,
    irq_total: AtomicU64,
    irq_mask: AtomicU64,
    irq_unmask: AtomicU64,
}

impl LoopbackIrq {
    fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
            masked: AtomicBool::new(false),
            waker: Mutex::new(None),
            irq_total: AtomicU64::new(0),
            irq_mask: AtomicU64::new(0),
            irq_unmask: AtomicU64::new(0),
        }
    }
}

impl QueueIrqControl for LoopbackIrq {
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

    fn stats(&self) -> QueueIrqStats {
        QueueIrqStats {
            irq_total: self.irq_total.load(Ordering::Relaxed),
            irq_mask: self.irq_mask.load(Ordering::Relaxed),
            irq_unmask: self.irq_unmask.load(Ordering::Relaxed),
        }
    }
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

impl LoopbackQueue {
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
        NetQueueCaps {
            queue_size: 256,
            scatter_gather: true,
            max_tx_descriptors: 18,
            max_rx_batch: 32,
            max_tx_batch: 32,
        }
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
            let packet_len = self.packets[self.packet_head]
                .as_ref()
                .map(|packet| packet.total_len() as u32)
                .unwrap_or(0);
            if packets != 0 && bytes.saturating_add(packet_len) > budget.bytes {
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
            packets += 1;
            bytes = bytes.saturating_add(packet_len);
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
            let packet_len = packet.chain.total_len() as u32;
            descriptors = descriptors.saturating_add(packet.chain.fragment_count() as u16);
            bytes = bytes.saturating_add(packet_len);
            self.packets[self.packet_tail] = Some(packet.chain);
            self.metadata[self.packet_tail] = Some(PacketMetadata {
                frame_len: packet_len,
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

pub fn register_builtin_driver() -> Result<(), PnpError> {
    let irq = Arc::new(LoopbackIrq::new());
    let queue = NetQueueRegistration {
        id: QueuePairId(0),
        queue: Box::new(LoopbackQueue::new()),
        rx_pool: make_pool(48, 4096)?,
        tx_header_pool: make_pool(256, 256)?,
        tx_payload_pool: make_pool(256, 4096)?,
        irq,
    };
    let registration = NetDeviceRegistration::new(
        "lo".into(),
        [0; 6],
        LOOPBACK_MTU,
        true,
        vec![queue].into_boxed_slice(),
    );
    net::device::register_device(registration).map_err(|_| {
        PnpError::registration_failed(PnpResourceKind::Function, "loopback registration")
    })?;
    log::printk!("[loopback] registered batch queue");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use net::buf::{PacketChain, TxPacket};

    #[test]
    fn tx_submission_exposes_rx_and_completion_synchronously() {
        let mut queue = LoopbackQueue::new();
        assert!(queue.tx_produces_rx_synchronously());

        let mut payload_pool = make_pool(1, 256).unwrap();
        let lease = payload_pool
            .lease(32, 64, PacketMetadata::default())
            .unwrap();
        let mut tx = TxBatch::new();
        tx.push(TxPacket {
            chain: PacketChain::from_lease(lease),
            completion: CompletionToken(7),
            low_latency: false,
        })
        .unwrap_or_else(|_| unreachable!());
        let mut header_pool = make_pool(1, 256).unwrap();

        let submitted = queue.submit_tx_batch(&mut tx, &mut header_pool);
        assert_eq!(submitted.packets, 1);
        assert_eq!(submitted.bytes, 64);
        assert!(tx.is_empty());

        let mut rx = PacketBatch::new();
        let polled = queue.poll_rx_batch(
            RxBudget {
                packets: 32,
                bytes: 256 * 1024,
            },
            &mut rx,
        );
        assert_eq!(polled.packets, 1);
        assert_eq!(polled.bytes, 64);
        assert!(polled.ring_empty);

        let mut completions = CompletionBatch::new();
        let reclaimed = queue.reclaim_tx_batch(&mut completions);
        assert_eq!(reclaimed.completions, 1);
        assert_eq!(completions.token(0), Some(CompletionToken(7)));
        assert!(reclaimed.ring_empty);
    }
}

//! 批量 queue 契约和 NAPI 风格调度状态。

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::QueuePairId;
use crate::buf::{CompletionBatch, NetBufPoolOwner, PacketBatch, RxRefillBatch, TxBatch};

/// 单次 RX batch 的剩余预算。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxBudget {
    pub packets: u16,
    pub bytes: u32,
}

impl RxBudget {
    pub fn validate(self) -> bool {
        (1..=32).contains(&self.packets) && self.bytes != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxPollResult {
    pub packets: u16,
    pub bytes: u32,
    pub ring_empty: bool,
    pub descriptor_starved: bool,
    pub fatal: Option<QueueFatalError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxRefillResult {
    pub posted: u16,
    pub descriptor_starved: bool,
    pub fatal: Option<QueueFatalError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxSubmitResult {
    pub packets: u16,
    pub descriptors: u16,
    pub bytes: u32,
    pub queue_full: bool,
    pub fatal: Option<QueueFatalError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxReclaimResult {
    pub completions: u16,
    pub descriptors: u16,
    pub ring_empty: bool,
    pub fatal: Option<QueueFatalError>,
}

/// queue 在设备生命周期内冻结的能力。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetQueueCaps {
    pub queue_size: u16,
    pub scatter_gather: bool,
    pub max_tx_descriptors: u8,
    pub max_rx_batch: u8,
    pub max_tx_batch: u8,
    pub udp_segmentation: bool,
    pub max_udp_segments: u8,
}

impl NetQueueCaps {
    pub fn validate_data_queue(self) -> bool {
        self.queue_size.is_power_of_two()
            && (16..=256).contains(&self.queue_size)
            && self.max_rx_batch == 32
            && self.max_tx_batch == 32
            && self.max_tx_descriptors != 0
            && if self.udp_segmentation {
                (2..=32).contains(&self.max_udp_segments)
            } else {
                self.max_udp_segments == 0
            }
    }
}

/// 只有无法继续保证 ring/ownership 正确性时才能返回 fatal。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueFatalError {
    DeviceGone,
    DeviceReset,
    DmaFault,
    RingCorrupt,
}

/// 物理与虚拟网络设备统一使用的批量数据面接口。
pub trait NetQueuePair: Send {
    fn id(&self) -> QueuePairId;
    fn caps(&self) -> NetQueueCaps;
    /// TX 提交成功后，是否保证同一 queue 的 RX 立即可见。
    fn tx_produces_rx_synchronously(&self) -> bool {
        false
    }
    fn refill_rx_batch(&mut self, batch: &mut RxRefillBatch) -> RxRefillResult;
    fn poll_rx_batch(&mut self, budget: RxBudget, out: &mut PacketBatch) -> RxPollResult;
    fn reclaim_tx_batch(&mut self, out: &mut CompletionBatch) -> TxReclaimResult;
    fn submit_tx_batch(
        &mut self,
        batch: &mut TxBatch,
        header_pool: &mut NetBufPoolOwner,
    ) -> TxSubmitResult;
    fn has_pending_work(&mut self) -> bool;
    fn quiesce(&mut self) -> Result<(), QueueFatalError>;
}

/// NAPI 状态的可观测阶段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum QueuePollState {
    IdleIrqEnabled = 0,
    IrqObserved = 1,
    PollScheduledIrqMasked = 2,
    PollRunning = 3,
    PollPending = 4,
    RecheckBeforeArm = 5,
}

impl QueuePollState {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::IdleIrqEnabled,
            1 => Self::IrqObserved,
            2 => Self::PollScheduledIrqMasked,
            3 => Self::PollRunning,
            4 => Self::PollPending,
            5 => Self::RecheckBeforeArm,
            _ => panic!("非法 NetQueue poll 状态"),
        }
    }
}

/// IRQ 与唯一 worker 共享的最小调度状态。
pub struct QueuePollMachine {
    state: AtomicU8,
    pending: AtomicBool,
}

impl QueuePollMachine {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(QueuePollState::IdleIrqEnabled as u8),
            pending: AtomicBool::new(false),
        }
    }

    pub fn state(&self) -> QueuePollState {
        QueuePollState::from_raw(self.state.load(Ordering::Acquire))
    }

    pub fn pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    /// IRQ 已由设备确认并屏蔽。只有 clean-to-pending 转换需要 wake worker。
    pub fn observe_irq(&self) -> bool {
        let wake = !self.pending.swap(true, Ordering::AcqRel);
        let _ = self.state.compare_exchange(
            QueuePollState::IdleIrqEnabled as u8,
            QueuePollState::IrqObserved as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        wake
    }

    /// worker 接受一次调度并进入 poll。
    pub fn begin_poll(&self) {
        self.state
            .store(QueuePollState::PollRunning as u8, Ordering::Release);
        self.pending.store(false, Ordering::Release);
    }

    /// 本 turn 预算耗尽或 queue 仍有工作。
    pub fn keep_polling(&self) {
        self.pending.store(true, Ordering::Release);
        self.state
            .store(QueuePollState::PollPending as u8, Ordering::Release);
    }

    /// ring 观察为空，worker 将先 arm IRQ 再复查 used index。
    pub fn begin_recheck_before_arm(&self) {
        self.state
            .store(QueuePollState::RecheckBeforeArm as u8, Ordering::Release);
    }

    /// arm 后复查结果。返回 true 表示必须立即保持 masked 并继续 poll。
    pub fn finish_arm_recheck(&self, ring_changed: bool) -> bool {
        if ring_changed || self.pending.load(Ordering::Acquire) {
            self.pending.store(true, Ordering::Release);
            self.state.store(
                QueuePollState::PollScheduledIrqMasked as u8,
                Ordering::Release,
            );
            true
        } else {
            self.state
                .store(QueuePollState::IdleIrqEnabled as u8, Ordering::Release);
            false
        }
    }
}

impl Default for QueuePollMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::ptr::NonNull;

    use crate::buf::{
        CompletionToken, NetBufPool, NetBufStorage, PacketChain, PacketFragment, PacketMetadata,
        TxPacket,
    };

    struct TestStorage {
        bytes: Box<[u8]>,
    }

    impl TestStorage {
        fn new(size: usize) -> Self {
            Self {
                bytes: vec![0; size].into_boxed_slice(),
            }
        }
    }

    impl NetBufStorage for TestStorage {
        fn capacity(&self) -> usize {
            self.bytes.len()
        }

        fn base_ptr(&self) -> NonNull<u8> {
            NonNull::new(self.bytes.as_ptr() as *mut u8).expect("测试 buffer 地址为空")
        }

        fn dma_addr(&self) -> Option<u64> {
            None
        }

        fn sync_for_cpu(&self, _offset: usize, _len: usize) {}
        fn sync_for_device(&self, _offset: usize, _len: usize) {}
    }

    /// 只模拟 ownership 和 batch 前缀，不模拟任何硬件寄存器。
    struct FakeQueue {
        rx_posted: Vec<crate::buf::NetBufLease>,
        tx_pending: Vec<TxPacket>,
        max_rx: usize,
        max_tx: usize,
        doorbells: usize,
    }

    impl FakeQueue {
        fn new(max_rx: usize, max_tx: usize) -> Self {
            Self {
                rx_posted: Vec::new(),
                tx_pending: Vec::new(),
                max_rx,
                max_tx,
                doorbells: 0,
            }
        }
    }

    impl NetQueuePair for FakeQueue {
        fn id(&self) -> QueuePairId {
            QueuePairId(0)
        }

        fn caps(&self) -> NetQueueCaps {
            NetQueueCaps {
                queue_size: 16,
                scatter_gather: true,
                max_tx_descriptors: 2,
                max_rx_batch: 32,
                max_tx_batch: 32,
                udp_segmentation: false,
                max_udp_segments: 0,
            }
        }

        fn refill_rx_batch(&mut self, batch: &mut RxRefillBatch) -> RxRefillResult {
            let original_len = batch.len();
            let mut posted = 0u16;
            for index in 0..original_len {
                if self.rx_posted.len() == self.max_rx {
                    break;
                }
                if let Some(lease) = batch.take(index) {
                    self.rx_posted.push(lease);
                    posted += 1;
                }
            }
            RxRefillResult {
                posted,
                descriptor_starved: false,
                fatal: None,
            }
        }

        fn poll_rx_batch(&mut self, _budget: RxBudget, out: &mut PacketBatch) -> RxPollResult {
            let Some(mut lease) = self.rx_posted.pop() else {
                return RxPollResult {
                    packets: 0,
                    bytes: 0,
                    ring_empty: true,
                    descriptor_starved: false,
                    fatal: None,
                };
            };
            lease.set_data_range(128, 4).unwrap();
            let metadata = *lease.metadata();
            assert!(out.push(PacketChain::from_lease(lease), metadata).is_ok());
            RxPollResult {
                packets: 1,
                bytes: 4,
                ring_empty: self.rx_posted.is_empty(),
                descriptor_starved: false,
                fatal: None,
            }
        }

        fn reclaim_tx_batch(&mut self, out: &mut CompletionBatch) -> TxReclaimResult {
            let Some(packet) = self.tx_pending.pop() else {
                return TxReclaimResult {
                    completions: 0,
                    descriptors: 0,
                    ring_empty: true,
                    fatal: None,
                };
            };
            out.push(packet.completion).unwrap();
            drop(packet);
            TxReclaimResult {
                completions: 1,
                descriptors: 1,
                ring_empty: self.tx_pending.is_empty(),
                fatal: None,
            }
        }

        fn submit_tx_batch(
            &mut self,
            batch: &mut TxBatch,
            _header_pool: &mut NetBufPoolOwner,
        ) -> TxSubmitResult {
            let original_len = batch.len();
            let mut submitted = 0u16;
            if self.tx_pending.len() < self.max_tx
                && let Some(packet) = batch.take(0)
            {
                self.tx_pending.push(packet);
                submitted = 1;
                self.doorbells += 1;
            }
            TxSubmitResult {
                packets: submitted,
                descriptors: submitted,
                bytes: if submitted == 1 { 4 } else { 0 },
                queue_full: submitted as usize != original_len,
                fatal: None,
            }
        }

        fn has_pending_work(&mut self) -> bool {
            !self.rx_posted.is_empty() || !self.tx_pending.is_empty()
        }

        fn quiesce(&mut self) -> Result<(), QueueFatalError> {
            Ok(())
        }
    }

    fn make_pool(count: usize) -> crate::buf::NetBufPoolOwner {
        let storage = (0..count)
            .map(|_| Box::new(TestStorage::new(512)) as Box<dyn NetBufStorage>)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        NetBufPool::new(storage).unwrap()
    }

    #[test]
    fn irq_between_empty_and_arm_is_not_lost() {
        let machine = QueuePollMachine::new();
        assert!(machine.observe_irq());
        machine.begin_poll();
        machine.begin_recheck_before_arm();
        assert!(machine.observe_irq());
        assert!(machine.finish_arm_recheck(false));
        assert!(machine.pending());
        assert_eq!(machine.state(), QueuePollState::PollScheduledIrqMasked);
    }

    #[test]
    fn ring_change_after_arm_keeps_poll_scheduled() {
        let machine = QueuePollMachine::new();
        machine.begin_poll();
        machine.begin_recheck_before_arm();
        assert!(machine.finish_arm_recheck(true));
        assert!(machine.pending());
    }

    #[test]
    fn clean_recheck_returns_to_irq_idle() {
        let machine = QueuePollMachine::new();
        machine.begin_poll();
        machine.begin_recheck_before_arm();
        assert!(!machine.finish_arm_recheck(false));
        assert_eq!(machine.state(), QueuePollState::IdleIrqEnabled);
    }

    #[test]
    fn fake_queue_preserves_partial_prefix_and_releases_on_completion() {
        let mut rx_owner = make_pool(3);
        let mut refill = RxRefillBatch::new();
        for _ in 0..3 {
            assert!(
                refill
                    .push(rx_owner.lease(0, 64, PacketMetadata::default()).unwrap())
                    .is_ok()
            );
        }
        let mut queue = FakeQueue::new(2, 1);
        let result = queue.refill_rx_batch(&mut refill);
        assert_eq!(result.posted, 2);
        assert_eq!(refill.len(), 3);
        assert!(refill.take(0).is_none());
        assert!(refill.take(1).is_none());
        let tail = refill.take(2).unwrap();
        // 重新放回未发布后缀，模拟 worker 的回收路径。
        assert!(refill.put(2, tail).is_ok());
        assert_eq!(rx_owner.outstanding(), 3);

        let mut rx = PacketBatch::new();
        let poll = queue.poll_rx_batch(
            RxBudget {
                packets: 32,
                bytes: 1024,
            },
            &mut rx,
        );
        assert_eq!(poll.packets, 1);
        let (mut packet, _) = rx.take(0).unwrap();
        let Some(PacketFragment::Exclusive(lease)) = packet.take_fragment(0) else {
            panic!("fake RX 必须交付独占 lease");
        };
        rx_owner.recycle_local(lease).unwrap();

        let mut rx_second = PacketBatch::new();
        assert_eq!(
            queue
                .poll_rx_batch(
                    RxBudget {
                        packets: 32,
                        bytes: 1024,
                    },
                    &mut rx_second,
                )
                .packets,
            1
        );
        let (mut packet, _) = rx_second.take(0).unwrap();
        let Some(PacketFragment::Exclusive(lease)) = packet.take_fragment(0) else {
            panic!("fake RX 必须交付独占 lease");
        };
        rx_owner.recycle_local(lease).unwrap();

        // 未发布的后缀仍归 worker，不能由 queue 代为释放。
        let lease = refill.take(2).unwrap();
        rx_owner.recycle_local(lease).unwrap();
        assert_eq!(rx_owner.outstanding(), 0);

        let mut tx_owner = make_pool(2);
        let lease = tx_owner.lease(64, 4, PacketMetadata::default()).unwrap();
        let mut tx = TxBatch::new();
        assert!(
            tx.push(TxPacket {
                chain: PacketChain::from_lease(lease),
                completion: CompletionToken(7),
                low_latency: true,
                checksums_validated: false,
                layout: crate::buf::PacketLayout::Plain,
            })
            .is_ok()
        );
        let mut header_owner = make_pool(2);
        let submitted = queue.submit_tx_batch(&mut tx, &mut header_owner);
        assert_eq!(submitted.packets, 1);
        assert_eq!(queue.doorbells, 1);
        assert!(tx.is_empty());
        assert_eq!(tx_owner.outstanding(), 1);

        let mut completions = CompletionBatch::new();
        let reclaimed = queue.reclaim_tx_batch(&mut completions);
        assert_eq!(reclaimed.completions, 1);
        assert_eq!(completions.token(0), Some(CompletionToken(7)));
        tx_owner.drain_remote();
        assert_eq!(tx_owner.outstanding(), 0);
    }
}

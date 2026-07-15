//! 固定容量网络 buffer 与 packet 所有权。

mod packet;
mod pool;

pub use packet::{
    CompletionBatch, CompletionToken, PacketBatch, PacketChain, PacketFragment, RxRefillBatch,
    TxBatch, TxPacket,
};
pub use pool::{
    ChunkRef, NetBufGeneration, NetBufId, NetBufLease, NetBufPool, NetBufPoolError, NetBufPoolId,
    NetBufPoolOwner, NetBufPoolStats, NetBufStorage, PoolGeneration,
};

use crate::{NetDeviceId, QueuePairId};

/// 数据面丢包原因。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum DropReason {
    #[default]
    None = 0,
    NoConsumer,
    MalformedDescriptor,
    LoopbackRingFull,
    DeviceGone,
}

/// packet 随所有权移动的最小元数据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketMetadata {
    pub ingress_device: NetDeviceId,
    pub queue_pair: QueuePairId,
    pub rx_timestamp_ns: u64,
    pub rss_hash: Option<u32>,
    pub rss_generation: u32,
    pub frame_len: u32,
    pub drop_reason: DropReason,
}

impl Default for PacketMetadata {
    fn default() -> Self {
        Self {
            ingress_device: NetDeviceId(0),
            queue_pair: QueuePairId(0),
            rx_timestamp_ns: 0,
            rss_hash: None,
            rss_generation: 0,
            frame_len: 0,
            drop_reason: DropReason::None,
        }
    }
}

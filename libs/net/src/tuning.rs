//! 网络数据面的固定参数。

/// 一个 packet batch 的最大 packet 数。
pub const PACKET_BATCH_CAPACITY: usize = 32;
/// 一个 packet chain 的最大 fragment 数。
pub const PACKET_FRAGMENT_CAPACITY: usize = 18;
/// RX page 大小。
pub const RX_PAGE_SIZE: usize = 4096;
/// TCP RX payload 达到该长度且 RX reserve 健康时保留原 DMA page。
pub const TCP_RX_PIN_MIN_BYTES: usize = 512;
/// 每个 RX queue 只供 descriptor refill 使用的紧急 page 数。
pub const RX_POOL_EMERGENCY_RESERVE: usize = 16;
/// VirtIO RX descriptor 在 page 内的起始偏移。
pub const VIRTIO_RX_DESCRIPTOR_OFFSET: usize = 116;
/// VirtIO-net header 长度。
pub const VIRTIO_NET_HEADER_LEN: usize = 12;
/// 每个硬件 queue depth 对应的 socket TX DMA chunk 数。
///
/// socket pool 与设备提交 pool 分离；该倍率允许多个拥塞窗口并行，又让内存随
/// 实际 queue 能力缩放，而不是按 CPU 或固定连接数预留。
pub const SOCKET_TX_POOL_DEPTH_MULTIPLIER: usize = 4;
/// Ethernet frame 在 RX page 内的起始偏移。
pub const VIRTIO_RX_FRAME_OFFSET: usize = 128;

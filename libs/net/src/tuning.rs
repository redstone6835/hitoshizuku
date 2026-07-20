//! 网络数据面的固定参数。

/// 一个 packet batch 的最大 packet 数。
pub const PACKET_BATCH_CAPACITY: usize = 32;
/// 一个 packet chain 的最大 fragment 数。
pub const PACKET_FRAGMENT_CAPACITY: usize = 18;
/// RX page 大小。
pub const RX_PAGE_SIZE: usize = 4096;
/// VirtIO RX descriptor 在 page 内的起始偏移。
pub const VIRTIO_RX_DESCRIPTOR_OFFSET: usize = 116;
/// VirtIO-net header 长度。
pub const VIRTIO_NET_HEADER_LEN: usize = 12;
/// Ethernet frame 在 RX page 内的起始偏移。
pub const VIRTIO_RX_FRAME_OFFSET: usize = 128;

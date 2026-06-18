//! 网络协议栈调优参数。
//!
//! 这里集中保存协议栈内部的容量、端口范围和主动轮询预算，避免在收发路径
//! 里散落魔数。所有值只描述内核网络层自身的资源策略；用户态兼容语义应当
//! 留在上层兼容模块中转换。

/// TCP socket 缓冲区配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpBufferTuning {
    /// 接收缓冲区字节数。
    pub rx_bytes: usize,
    /// 发送缓冲区字节数。
    pub tx_bytes: usize,
}

/// TCP 监听 socket 的内部队列配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpListenTuning {
    /// 每个接口最多缓存的已完成握手连接数量。
    pub accept_backlog: usize,
}

/// 数据报类 socket 缓冲区配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketBufferTuning {
    /// 接收数据区字节数。
    pub rx_bytes: usize,
    /// 发送数据区字节数。
    pub tx_bytes: usize,
    /// 接收队列可容纳的数据包元数据数量。
    pub rx_meta: usize,
    /// 发送队列可容纳的数据包元数据数量。
    pub tx_meta: usize,
}

/// 主动推进协议栈状态机的预算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivePollTuning {
    /// 单次主动推进最多执行的轮数。
    pub max_rounds: usize,
}

/// 自动分配本地端口的范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EphemeralPortRange {
    /// 端口范围下界，包含。
    pub start: u16,
    /// 端口范围上界，包含。
    pub end: u16,
}

impl EphemeralPortRange {
    /// 端口范围内的端口数量。
    pub const fn len(self) -> u32 {
        self.end as u32 - self.start as u32 + 1
    }

    /// 检查端口是否落在当前范围内。
    pub const fn contains(self, port: u16) -> bool {
        port >= self.start && port <= self.end
    }
}

/// 全局网络栈调优参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetTuning {
    /// 默认 TCP 缓冲配置。
    pub tcp: TcpBufferTuning,
    /// TCP 监听队列配置。
    pub tcp_listen: TcpListenTuning,
    /// 默认 UDP 缓冲配置。
    pub udp: PacketBufferTuning,
    /// 默认 raw IP 缓冲配置。
    pub raw: PacketBufferTuning,
    /// 默认 ICMP 缓冲配置。
    pub icmp: PacketBufferTuning,
    /// 主动轮询预算。
    pub active_poll: ActivePollTuning,
    /// 自动分配端口范围。
    pub ephemeral_ports: EphemeralPortRange,
}

/// 默认 TCP 缓冲区大小。
///
// 默认 TCP 缓冲区大小
pub const DEFAULT_TCP_BUFFER_BYTES: usize = 256 * 1024;

/// 默认 UDP 数据区大小。
pub const DEFAULT_UDP_BUFFER_BYTES: usize = 128 * 1024;

/// 默认 raw/ICMP 数据区大小。
pub const DEFAULT_CONTROL_BUFFER_BYTES: usize = 8 * 1024;

/// 默认数据包元数据队列深度。
pub const DEFAULT_PACKET_META_COUNT: usize = 32;

/// 默认主动 poll 预算。
///
/// 主动 poll 只用于刚产生出站事件后的快速推进；主定时路径仍负责长期重传和
/// 超时。该预算允许 loopback 在一次系统调用内完成握手，同时避免热路径固定
/// 空转过多轮。
pub const DEFAULT_ACTIVE_POLL_ROUNDS: usize = 128;

/// 默认 TCP accept 待连接队列长度。
pub const DEFAULT_TCP_ACCEPT_BACKLOG: usize = 128;

/// 默认临时端口范围。
pub const DEFAULT_EPHEMERAL_PORTS: EphemeralPortRange = EphemeralPortRange {
    start: 49152,
    end: 65535,
};

impl NetTuning {
    /// 构造网络栈默认调优参数。
    pub const fn defaults() -> Self {
        Self {
            tcp: TcpBufferTuning {
                rx_bytes: DEFAULT_TCP_BUFFER_BYTES,
                tx_bytes: DEFAULT_TCP_BUFFER_BYTES,
            },
            tcp_listen: TcpListenTuning {
                accept_backlog: DEFAULT_TCP_ACCEPT_BACKLOG,
            },
            udp: PacketBufferTuning {
                rx_bytes: DEFAULT_UDP_BUFFER_BYTES,
                tx_bytes: DEFAULT_UDP_BUFFER_BYTES,
                rx_meta: DEFAULT_PACKET_META_COUNT,
                tx_meta: DEFAULT_PACKET_META_COUNT,
            },
            raw: PacketBufferTuning {
                rx_bytes: DEFAULT_CONTROL_BUFFER_BYTES,
                tx_bytes: DEFAULT_CONTROL_BUFFER_BYTES,
                rx_meta: DEFAULT_PACKET_META_COUNT,
                tx_meta: DEFAULT_PACKET_META_COUNT,
            },
            icmp: PacketBufferTuning {
                rx_bytes: DEFAULT_CONTROL_BUFFER_BYTES,
                tx_bytes: DEFAULT_CONTROL_BUFFER_BYTES,
                rx_meta: DEFAULT_PACKET_META_COUNT,
                tx_meta: DEFAULT_PACKET_META_COUNT,
            },
            active_poll: ActivePollTuning {
                max_rounds: DEFAULT_ACTIVE_POLL_ROUNDS,
            },
            ephemeral_ports: DEFAULT_EPHEMERAL_PORTS,
        }
    }
}

impl Default for NetTuning {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tuning_has_valid_resource_bounds() {
        let tuning = NetTuning::defaults();

        assert!(tuning.tcp.rx_bytes >= DEFAULT_TCP_BUFFER_BYTES);
        assert!(tuning.tcp.tx_bytes >= DEFAULT_TCP_BUFFER_BYTES);
        assert!(tuning.tcp_listen.accept_backlog > 0);
        assert!(tuning.udp.rx_meta > 0);
        assert!(tuning.udp.tx_meta > 0);
        assert!(tuning.active_poll.max_rounds >= 2);
        assert!(
            tuning
                .ephemeral_ports
                .contains(tuning.ephemeral_ports.start)
        );
        assert!(tuning.ephemeral_ports.contains(tuning.ephemeral_ports.end));
        assert_eq!(tuning.ephemeral_ports.len(), 16_384);
    }
}

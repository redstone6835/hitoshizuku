//! 网络接口配置类型。
//!
//! 定义接口上线时需要的网络参数：IP 地址、网关、配置模式等。
//! 保持类型简单且独立于 smoltcp——smoltcp 特定类型的转换在
//! [`interface`](crate::interface) 模块中完成。
//!
//! 同时支持 IPv4 和 IPv6 双栈。

use alloc::vec::Vec;

// ── IPv4 地址 ─────────────────────────────────────────────────────────────────

/// IPv4 地址（4 字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    pub const UNSPECIFIED: Self = Self([0, 0, 0, 0]);
    pub const LOCALHOST: Self = Self([127, 0, 0, 1]);
    pub const BROADCAST: Self = Self([255, 255, 255, 255]);

    pub fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self([a, b, c, d])
    }

    pub fn octets(&self) -> [u8; 4] {
        self.0
    }
}

// ── IPv6 地址 ─────────────────────────────────────────────────────────────────

/// IPv6 地址（16 字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv6Addr(pub [u8; 16]);

impl Ipv6Addr {
    pub const UNSPECIFIED: Self = Self([0; 16]);
    pub const LOCALHOST: Self = Self([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

    pub fn new(segments: [u16; 8]) -> Self {
        let mut octets = [0u8; 16];
        for (i, seg) in segments.iter().enumerate() {
            octets[i * 2] = (seg >> 8) as u8;
            octets[i * 2 + 1] = *seg as u8;
        }
        Self(octets)
    }

    pub fn octets(&self) -> [u8; 16] {
        self.0
    }
}

// ── 通用 IP 地址 ─────────────────────────────────────────────────────────────

/// 协议无关的 IP 地址。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpAddr {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
}

/// 带前缀长度的 IP 地址（CIDR 表示法）。
///
/// 例如 `192.168.1.10/24` 或 `fe80::1/64`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CidrAddress {
    pub addr: IpAddr,
    pub prefix_len: u8,
}

impl CidrAddress {
    pub fn new_v4(addr: Ipv4Addr, prefix_len: u8) -> Self {
        Self {
            addr: IpAddr::V4(addr),
            prefix_len,
        }
    }

    pub fn new_v6(addr: Ipv6Addr, prefix_len: u8) -> Self {
        Self {
            addr: IpAddr::V6(addr),
            prefix_len,
        }
    }
}

// ── 端点 ─────────────────────────────────────────────────────────────────────

/// 网络端点（IP 地址 + 端口）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub addr: IpAddr,
    pub port: u16,
}

// ── 网关 ─────────────────────────────────────────────────────────────────────

/// 默认网关配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gateway {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    DualStack { v4: Ipv4Addr, v6: Ipv6Addr },
}

// ── 接口配置 ─────────────────────────────────────────────────────────────────

/// 接口配置模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfMode {
    /// 静态 IP。
    Static,
    /// DHCP/SLAAC 自动获取。
    ///
    /// 当前 Ethernet 接口会由内部 DHCPv4 socket 填充 IPv4 配置；IPv6
    /// SLAAC 状态机后续接入时复用同一模式。
    Auto,
}

/// 网络接口配置。
#[derive(Debug, Clone)]
pub struct IfConfig {
    /// 接口上的 IP 地址列表（支持 IPv4 + IPv6 混合绑定）。
    pub addresses: Vec<CidrAddress>,
    /// 默认网关。
    pub gateway: Option<Gateway>,
    /// 配置模式。
    pub mode: IfMode,
}

impl IfConfig {
    /// 纯 IPv4 静态配置。
    pub fn static_v4(addr: Ipv4Addr, prefix_len: u8, gateway: Option<Ipv4Addr>) -> Self {
        Self {
            addresses: alloc::vec![CidrAddress::new_v4(addr, prefix_len)],
            gateway: gateway.map(Gateway::V4),
            mode: IfMode::Static,
        }
    }

    /// 纯 IPv6 静态配置。
    pub fn static_v6(addr: Ipv6Addr, prefix_len: u8, gateway: Option<Ipv6Addr>) -> Self {
        Self {
            addresses: alloc::vec![CidrAddress::new_v6(addr, prefix_len)],
            gateway: gateway.map(Gateway::V6),
            mode: IfMode::Static,
        }
    }

    /// 自动配置（DHCP + SLAAC）。
    pub fn auto() -> Self {
        // Auto 初始态没有租约；接口 poll 中的自动配置状态机会在获得租约后
        // 原子提交地址和默认网关。
        Self {
            addresses: Vec::new(),
            gateway: None,
            mode: IfMode::Auto,
        }
    }
}

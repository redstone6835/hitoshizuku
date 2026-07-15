//! 协议层以后会使用的架构无关地址值类型。
//!
//! 这些类型只表达地址，不携带旧协议引擎或全局 stack 状态。

/// IPv4 地址，按网络字节序保存。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    pub const UNSPECIFIED: Self = Self([0; 4]);
    pub const LOCALHOST: Self = Self([127, 0, 0, 1]);

    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self([a, b, c, d])
    }
}

/// IPv6 地址，按网络字节序保存。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ipv6Addr(pub [u8; 16]);

impl Ipv6Addr {
    pub const UNSPECIFIED: Self = Self([0; 16]);
    pub const LOCALHOST: Self = Self([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

    pub const fn new(segments: [u16; 8]) -> Self {
        let mut octets = [0; 16];
        let mut index = 0;
        while index < segments.len() {
            let bytes = segments[index].to_be_bytes();
            octets[index * 2] = bytes[0];
            octets[index * 2 + 1] = bytes[1];
            index += 1;
        }
        Self(octets)
    }
}

/// IP 地址联合。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IpAddr {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
}

/// IP endpoint。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub addr: IpAddr,
    pub port: u16,
}

//! 协议层以后会使用的架构无关地址值类型。
//!
//! 这些类型只表达地址，不携带旧协议引擎或全局 stack 状态。

/// IPv4 地址，按网络字节序保存。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    pub const UNSPECIFIED: Self = Self([0; 4]);
    pub const LOCALHOST: Self = Self([127, 0, 0, 1]);
    pub const BROADCAST: Self = Self([255; 4]);

    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self([a, b, c, d])
    }

    pub fn is_unspecified(self) -> bool {
        self.0 == [0; 4]
    }

    pub const fn is_loopback(self) -> bool {
        self.0[0] == 127
    }

    pub const fn is_multicast(self) -> bool {
        self.0[0] >= 224 && self.0[0] <= 239
    }

    pub fn is_broadcast(self) -> bool {
        self.0 == [255; 4]
    }

    pub const fn as_u32(self) -> u32 {
        u32::from_be_bytes(self.0)
    }
}

/// IPv6 地址，按网络字节序保存。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

    pub fn is_unspecified(self) -> bool {
        self.0 == [0; 16]
    }

    pub fn is_loopback(self) -> bool {
        self.0 == Self::LOCALHOST.0
    }

    pub const fn is_multicast(self) -> bool {
        self.0[0] == 0xff
    }

    pub const fn is_link_local(self) -> bool {
        self.0[0] == 0xfe && self.0[1] & 0xc0 == 0x80
    }
}

/// IP 地址联合。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IpAddr {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AddressFamily {
    Ipv4,
    Ipv6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum TransportProtocol {
    Tcp = 6,
    Udp = 17,
}

impl IpAddr {
    pub fn is_unspecified(self) -> bool {
        match self {
            Self::V4(address) => address.is_unspecified(),
            Self::V6(address) => address.is_unspecified(),
        }
    }

    pub fn is_multicast(self) -> bool {
        match self {
            Self::V4(address) => address.is_multicast(),
            Self::V6(address) => address.is_multicast(),
        }
    }

    pub const fn octet_len(self) -> usize {
        match self {
            Self::V4(_) => 4,
            Self::V6(_) => 16,
        }
    }
}

/// IP endpoint。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Endpoint {
    pub addr: IpAddr,
    pub port: u16,
}

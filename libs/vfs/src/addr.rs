//! IPv4/IPv6 sockaddr 解析与序列化。
//!
//! 本模块把用户态 `struct sockaddr_in` / `struct sockaddr_in6` 二进制格式
//! 与 `net::Endpoint` 之间互转。

use errno::Errno;
use net::{Endpoint, IpAddr, Ipv4Addr, Ipv6Addr};

pub const AF_INET: u16 = 2;
pub const AF_INET6: u16 = 10;

const SOCKADDR_IN_SIZE: usize = 16;
const SOCKADDR_IN6_SIZE: usize = 28;

/// 解析用户态 sockaddr 为 `net::Endpoint`。
///
/// 支持 `sockaddr_in`（16 字节）和 `sockaddr_in6`（28 字节）。
pub fn parse_inet_sockaddr(data: &[u8]) -> Result<Endpoint, Errno> {
    let family = sockaddr_family(data)?;
    parse_inet_sockaddr_with_family(data, family)
}

/// 解析 sockaddr，并要求 sockaddr family 与当前 socket family 一致。
pub fn parse_inet_sockaddr_for_socket(data: &[u8], socket_family: u16) -> Result<Endpoint, Errno> {
    let family = sockaddr_family(data)?;
    if family != socket_family {
        return Err(Errno::EAFNOSUPPORT);
    }
    parse_inet_sockaddr_with_family(data, family)
}

pub fn sockaddr_family(data: &[u8]) -> Result<u16, Errno> {
    if data.len() < 2 {
        return Err(Errno::EINVAL);
    }
    Ok(u16::from_ne_bytes([data[0], data[1]]))
}

fn parse_inet_sockaddr_with_family(data: &[u8], family: u16) -> Result<Endpoint, Errno> {
    match family {
        AF_INET => parse_sockaddr_in(data),
        AF_INET6 => parse_sockaddr_in6(data),
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

/// 序列化 `net::Endpoint` 为 sockaddr 二进制格式。
///
/// 返回实际写入的字节数。
pub fn encode_inet_sockaddr(ep: &Endpoint, family: u16, buf: &mut [u8]) -> Result<usize, Errno> {
    match family {
        AF_INET => encode_sockaddr_in(ep, buf),
        AF_INET6 => encode_sockaddr_in6(ep, buf),
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

fn parse_sockaddr_in(data: &[u8]) -> Result<Endpoint, Errno> {
    if data.len() < SOCKADDR_IN_SIZE {
        return Err(Errno::EINVAL);
    }
    let port = u16::from_be_bytes([data[2], data[3]]);
    let addr = Ipv4Addr([data[4], data[5], data[6], data[7]]);
    Ok(Endpoint {
        addr: IpAddr::V4(addr),
        port,
    })
}

fn parse_sockaddr_in6(data: &[u8]) -> Result<Endpoint, Errno> {
    if data.len() < SOCKADDR_IN6_SIZE {
        return Err(Errno::EINVAL);
    }
    let port = u16::from_be_bytes([data[2], data[3]]);
    let mut octets = [0u8; 16];
    octets.copy_from_slice(&data[8..24]);
    let addr = Ipv6Addr(octets);
    Ok(Endpoint {
        addr: IpAddr::V6(addr),
        port,
    })
}

fn encode_sockaddr_in(ep: &Endpoint, buf: &mut [u8]) -> Result<usize, Errno> {
    if buf.len() < SOCKADDR_IN_SIZE {
        return Err(Errno::EINVAL);
    }
    let IpAddr::V4(v4) = ep.addr else {
        return Err(Errno::EINVAL);
    };
    buf[..SOCKADDR_IN_SIZE].fill(0);
    buf[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
    buf[2..4].copy_from_slice(&ep.port.to_be_bytes());
    buf[4..8].copy_from_slice(&v4.0);
    Ok(SOCKADDR_IN_SIZE)
}

fn encode_sockaddr_in6(ep: &Endpoint, buf: &mut [u8]) -> Result<usize, Errno> {
    if buf.len() < SOCKADDR_IN6_SIZE {
        return Err(Errno::EINVAL);
    }
    let IpAddr::V6(v6) = ep.addr else {
        return Err(Errno::EINVAL);
    };
    buf[..SOCKADDR_IN6_SIZE].fill(0);
    buf[0..2].copy_from_slice(&AF_INET6.to_ne_bytes());
    buf[2..4].copy_from_slice(&ep.port.to_be_bytes());
    // flowinfo = 0 (bytes 4..8 already zeroed)
    buf[8..24].copy_from_slice(&v6.0);
    // scope_id = 0 (bytes 24..28 already zeroed)
    Ok(SOCKADDR_IN6_SIZE)
}

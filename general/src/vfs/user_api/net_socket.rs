//! 网络 socket ioctl 兼容层。
//!
//! 本模块位于 VFS/ABI 边界，负责把用户态 `SIOC*`、`ifreq`、`arpreq`
//! 布局和用户指针拷贝翻译为 `net` crate 的 typed 接口。底层 `dev::net`
//! 只暴露网络设备 function 和 typed control，不承载这些用户 ABI 细节。

use errno::Errno;
use net::config::IpAddr;

use crate::mm::{copy_from_user, copy_to_user};

const SIOCADDRT: u32 = 0x890B;
const SIOCDELRT: u32 = 0x890C;
const SIOCGIFCONF: u32 = 0x8912;
const SIOCGIFFLAGS: u32 = 0x8913;
const SIOCSIFFLAGS: u32 = 0x8914;
const SIOCGIFADDR: u32 = 0x8915;
const SIOCSIFADDR: u32 = 0x8916;
const SIOCGIFBRDADDR: u32 = 0x8919;
const SIOCGIFNETMASK: u32 = 0x891b;
const SIOCSIFNETMASK: u32 = 0x891c;
const SIOCGIFMTU: u32 = 0x8921;
const SIOCSIFMTU: u32 = 0x8922;
const SIOCGIFHWADDR: u32 = 0x8927;
const SIOCGIFINDEX: u32 = 0x8933;
const SIOCGIFTXQLEN: u32 = 0x8942;
const SIOCSIFTXQLEN: u32 = 0x8943;
const SIOCGARP: u32 = 0x8954;
const SIOCSARP: u32 = 0x8955;
const SIOCDARP: u32 = 0x8953;

/// 用户态 ifreq 兼容布局：ifr_name[16] + union[24]。
const IFREQ_SIZE: usize = 40;
const IFNAMSIZ: usize = 16;
const IFCONF_LEN_OFFSET: usize = 0;
const IFCONF_BUF_PTR_OFFSET: usize = 8;
const IFREQ_DATA_OFFSET: usize = IFNAMSIZ;
const SOCKADDR_SIZE: usize = 16;
const SOCKADDR_FAMILY_OFFSET: usize = 0;
const SOCKADDR_INET_ADDR_OFFSET: usize = 4;
const SOCKADDR_HWADDR_OFFSET: usize = 2;
const ARPREQ_HA_OFFSET: usize = 16;
const ARPREQ_FLAGS_OFFSET: usize = 32;
const AF_INET: u8 = 2;
const ARPHRD_ETHER: u8 = 1;
const ATF_COM: u32 = 0x02;
/// 当前 net driver 没有独立 txqlen 状态，兼容层返回传统默认值。
const DEFAULT_TX_QUEUE_LEN: i32 = 1000;

/// 安装网络 socket ioctl 适配器。
pub fn install_net_socket_ioctl_adapter() {
    ::vfs::net_socket::install_net_ioctl_handler(net_ioctl);
}

/// 处理网络 socket 上的 ioctl。由 VFS socket 层调用。
pub fn net_ioctl(cmd: u32, arg: usize) -> Result<usize, Errno> {
    match cmd {
        SIOCGIFCONF => ioctl_gifconf(arg),
        SIOCGIFFLAGS => ioctl_gifflags(arg),
        SIOCGIFADDR => ioctl_gifaddr(arg),
        SIOCGIFNETMASK => ioctl_gifnetmask(arg),
        SIOCGIFHWADDR => ioctl_gifhwaddr(arg),
        SIOCGIFMTU => ioctl_gifmtu(arg),
        SIOCGIFINDEX => ioctl_gifindex(arg),
        SIOCGIFTXQLEN => ioctl_giftxqlen(arg),
        SIOCGIFBRDADDR => ioctl_gifbrdaddr(arg),
        SIOCSIFFLAGS => ioctl_sifflags(arg),
        SIOCSIFADDR => ioctl_sifaddr(arg),
        SIOCSIFNETMASK => ioctl_sifnetmask(arg),
        SIOCSIFMTU => ioctl_sifmtu(arg),
        SIOCSIFTXQLEN => Err(Errno::EPERM),
        SIOCADDRT => ioctl_addrt(arg),
        SIOCDELRT => ioctl_delrt(arg),
        SIOCGARP => ioctl_get_arp(arg),
        SIOCSARP | SIOCDARP => Err(Errno::EPERM),
        _ => Err(Errno::ENOTTY),
    }
}

fn map_net_errno(err: net::NetError) -> Errno {
    match err {
        net::NetError::InterfaceNotFound => Errno::ENODEV,
        net::NetError::InvalidArgument => Errno::EINVAL,
        net::NetError::ResourceExhausted => Errno::ENOMEM,
        net::NetError::WouldBlock => Errno::EAGAIN,
        net::NetError::AddressInUse => Errno::EADDRINUSE,
        net::NetError::TimedOut => Errno::ETIMEDOUT,
        net::NetError::ConnectionRefused => Errno::ECONNREFUSED,
        net::NetError::ConnectionReset => Errno::ECONNRESET,
        net::NetError::Closed => Errno::EPIPE,
        net::NetError::LinkDown | net::NetError::Unreachable | net::NetError::BufferTooSmall => {
            Errno::EINVAL
        }
        net::NetError::InterfaceExists => Errno::EEXIST,
    }
}

/// 把 CIDR 前缀长度转换成 IPv4 子网掩码（network-order 字节）。
fn prefix_to_netmask(prefix: u8) -> [u8; 4] {
    let prefix = prefix.min(32);
    let mask: u32 = if prefix == 0 {
        0
    } else {
        !0u32 << (32 - prefix)
    };
    mask.to_be_bytes()
}

fn ioctl_get_arp(arg: usize) -> Result<usize, Errno> {
    let mut pa_buf = [0u8; SOCKADDR_SIZE];
    copy_from_user(arg, &mut pa_buf).map_err(|_| Errno::EFAULT)?;
    let target_ip = [
        pa_buf[SOCKADDR_INET_ADDR_OFFSET],
        pa_buf[SOCKADDR_INET_ADDR_OFFSET + 1],
        pa_buf[SOCKADDR_INET_ADDR_OFFSET + 2],
        pa_buf[SOCKADDR_INET_ADDR_OFFSET + 3],
    ];
    let target = net::IpAddr::V4(net::Ipv4Addr(target_ip));
    let neighbors = net::stack().all_neighbors();
    for (_iface_id, entries) in &neighbors {
        for entry in entries {
            if entry.ip_addr == target {
                let mut ha = [0u8; SOCKADDR_SIZE];
                ha[SOCKADDR_FAMILY_OFFSET] = ARPHRD_ETHER;
                ha[SOCKADDR_HWADDR_OFFSET..SOCKADDR_HWADDR_OFFSET + 6]
                    .copy_from_slice(&entry.hw_addr);
                copy_to_user(arg + ARPREQ_HA_OFFSET, &ha).map_err(|_| Errno::EFAULT)?;
                let flags = ATF_COM.to_ne_bytes();
                copy_to_user(arg + ARPREQ_FLAGS_OFFSET, &flags).map_err(|_| Errno::EFAULT)?;
                return Ok(0);
            }
        }
    }
    Err(Errno::ENOENT)
}

fn find_iface_by_name(name: &[u8]) -> Option<net::InterfaceSnapshot> {
    let name_str = core::str::from_utf8(name).ok()?;
    let name_trimmed = name_str.trim_end_matches('\0');
    net::stack()
        .snapshot_interfaces()
        .into_iter()
        .find(|i| i.name == name_trimmed)
}

fn read_ifreq_name(arg: usize) -> Result<[u8; IFNAMSIZ], Errno> {
    let mut name = [0u8; IFNAMSIZ];
    copy_from_user(arg, &mut name).map_err(|_| Errno::EFAULT)?;
    Ok(name)
}

fn write_ifreq_data(arg: usize, offset: usize, data: &[u8]) -> Result<(), Errno> {
    copy_to_user(arg + offset, data).map_err(|_| Errno::EFAULT)
}

fn ioctl_gifconf(arg: usize) -> Result<usize, Errno> {
    let mut len_buf = [0u8; 4];
    copy_from_user(arg + IFCONF_LEN_OFFSET, &mut len_buf).map_err(|_| Errno::EFAULT)?;
    let buf_len = i32::from_ne_bytes(len_buf) as usize;

    let mut ptr_buf = [0u8; 8];
    copy_from_user(arg + IFCONF_BUF_PTR_OFFSET, &mut ptr_buf).map_err(|_| Errno::EFAULT)?;
    let buf_ptr = usize::from_ne_bytes(ptr_buf);

    let ifaces = net::stack().snapshot_interfaces();
    let mut offset = 0usize;
    for iface in &ifaces {
        if offset + IFREQ_SIZE > buf_len {
            break;
        }
        let mut ifreq = [0u8; IFREQ_SIZE];
        let name_bytes = iface.name.as_bytes();
        let copy_len = name_bytes.len().min(IFNAMSIZ - 1);
        ifreq[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
        ifreq[IFREQ_DATA_OFFSET] = AF_INET;
        copy_to_user(buf_ptr + offset, &ifreq).map_err(|_| Errno::EFAULT)?;
        offset += IFREQ_SIZE;
    }
    let actual_len = (offset as i32).to_ne_bytes();
    copy_to_user(arg + IFCONF_LEN_OFFSET, &actual_len).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

fn ioctl_gifflags(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let flags = (iface.flags as i16).to_ne_bytes();
    write_ifreq_data(arg, IFREQ_DATA_OFFSET, &[flags[0], flags[1]])?;
    Ok(0)
}

fn ioctl_gifaddr(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mut sa = [0u8; SOCKADDR_SIZE];
    sa[SOCKADDR_FAMILY_OFFSET] = AF_INET;
    if let Some(cidr) = iface
        .addresses
        .iter()
        .find(|c| matches!(c.addr, IpAddr::V4(_)))
        && let IpAddr::V4(v4) = cidr.addr
    {
        sa[SOCKADDR_INET_ADDR_OFFSET..SOCKADDR_INET_ADDR_OFFSET + 4].copy_from_slice(&v4.0);
    }
    write_ifreq_data(arg, IFREQ_DATA_OFFSET, &sa)?;
    Ok(0)
}

fn ioctl_gifnetmask(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mut sa = [0u8; SOCKADDR_SIZE];
    sa[SOCKADDR_FAMILY_OFFSET] = AF_INET;
    if let Some(cidr) = iface
        .addresses
        .iter()
        .find(|c| matches!(c.addr, IpAddr::V4(_)))
    {
        sa[SOCKADDR_INET_ADDR_OFFSET..SOCKADDR_INET_ADDR_OFFSET + 4]
            .copy_from_slice(&prefix_to_netmask(cidr.prefix_len));
    }
    write_ifreq_data(arg, IFREQ_DATA_OFFSET, &sa)?;
    Ok(0)
}

fn ioctl_gifhwaddr(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mut sa = [0u8; SOCKADDR_SIZE];
    sa[SOCKADDR_FAMILY_OFFSET] = ARPHRD_ETHER;
    sa[SOCKADDR_HWADDR_OFFSET..SOCKADDR_HWADDR_OFFSET + 6].copy_from_slice(&iface.mac);
    write_ifreq_data(arg, IFREQ_DATA_OFFSET, &sa)?;
    Ok(0)
}

fn ioctl_gifmtu(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mtu = (iface.mtu as i32).to_ne_bytes();
    write_ifreq_data(arg, IFREQ_DATA_OFFSET, &mtu)?;
    Ok(0)
}

fn ioctl_gifindex(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let index = (iface.id.raw() as i32 + 1).to_ne_bytes();
    write_ifreq_data(arg, IFREQ_DATA_OFFSET, &index)?;
    Ok(0)
}

fn ioctl_giftxqlen(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let _iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let txqlen = DEFAULT_TX_QUEUE_LEN.to_ne_bytes();
    write_ifreq_data(arg, IFREQ_DATA_OFFSET, &txqlen)?;
    Ok(0)
}

fn ioctl_gifbrdaddr(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mut sa = [0u8; SOCKADDR_SIZE];
    sa[SOCKADDR_FAMILY_OFFSET] = AF_INET;
    if let Some(cidr) = iface
        .addresses
        .iter()
        .find(|c| matches!(c.addr, IpAddr::V4(_)))
        && let IpAddr::V4(v4) = cidr.addr
    {
        let mask = prefix_to_netmask(cidr.prefix_len);
        for i in 0..4 {
            sa[SOCKADDR_INET_ADDR_OFFSET + i] = v4.0[i] | !mask[i];
        }
    }
    write_ifreq_data(arg, IFREQ_DATA_OFFSET, &sa)?;
    Ok(0)
}

fn ioctl_sifflags(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mut flags = [0u8; 2];
    copy_from_user(arg + IFREQ_DATA_OFFSET, &mut flags).map_err(|_| Errno::EFAULT)?;
    let flags = u16::from_ne_bytes(flags) as u32;
    net::stack()
        .set_iface_admin_up(iface.id, flags & net::IFF_UP != 0)
        .map_err(|_| Errno::ENODEV)?;
    Ok(0)
}

fn ioctl_sifmtu(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mut raw = [0u8; 4];
    copy_from_user(arg + IFREQ_DATA_OFFSET, &mut raw).map_err(|_| Errno::EFAULT)?;
    let mtu = i32::from_ne_bytes(raw);
    if mtu < 0 {
        return Err(Errno::EINVAL);
    }
    net::stack()
        .set_iface_mtu(iface.id, mtu as usize)
        .map_err(map_net_errno)?;
    Ok(0)
}

fn ioctl_sifaddr(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mut sa = [0u8; 16];
    copy_from_user(arg + IFNAMSIZ, &mut sa).map_err(|_| Errno::EFAULT)?;
    if sa[0] != 2 {
        return Err(Errno::EAFNOSUPPORT);
    }
    let addr = net::Ipv4Addr([sa[4], sa[5], sa[6], sa[7]]);
    let prefix = iface
        .addresses
        .iter()
        .find(|c| matches!(c.addr, net::IpAddr::V4(_)))
        .map(|c| c.prefix_len)
        .unwrap_or(24);
    net::stack()
        .set_iface_ipv4_addr(iface.id, addr, prefix)
        .map_err(|_| Errno::ENODEV)?;
    Ok(0)
}

fn ioctl_sifnetmask(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mut sa = [0u8; 16];
    copy_from_user(arg + IFNAMSIZ, &mut sa).map_err(|_| Errno::EFAULT)?;
    if sa[0] != 2 {
        return Err(Errno::EAFNOSUPPORT);
    }
    let mask = net::Ipv4Addr([sa[4], sa[5], sa[6], sa[7]]);
    let prefix = mask_to_prefix_len_bytes(&mask.0);
    let cidr = iface
        .addresses
        .iter()
        .find(|c| matches!(c.addr, net::IpAddr::V4(_)))
        .ok_or(Errno::EADDRNOTAVAIL)?;
    if let net::IpAddr::V4(v4) = cidr.addr {
        net::stack()
            .set_iface_ipv4_addr(iface.id, v4, prefix)
            .map_err(|_| Errno::ENODEV)?;
    }
    Ok(0)
}

fn ioctl_addrt(arg: usize) -> Result<usize, Errno> {
    let mut buf = [0u8; 64];
    copy_from_user(arg, &mut buf).map_err(|_| Errno::EFAULT)?;
    let dest = extract_ipv4(&buf, 8);
    let gw = extract_ipv4(&buf, 24);
    let mask = extract_ipv4(&buf, 40);
    let ifaces = net::stack().snapshot_interfaces();
    let target = ifaces.iter().find(|i| i.name != "lo").or(ifaces.first());
    if let Some(iface) = target {
        net::stack()
            .add_route(iface.id, dest, mask, gw)
            .map_err(|_| Errno::EINVAL)?;
    }
    Ok(0)
}

fn ioctl_delrt(arg: usize) -> Result<usize, Errno> {
    let mut buf = [0u8; 64];
    copy_from_user(arg, &mut buf).map_err(|_| Errno::EFAULT)?;
    let dest = extract_ipv4(&buf, 8);
    let mask = extract_ipv4(&buf, 40);
    let ifaces = net::stack().snapshot_interfaces();
    let target = ifaces.iter().find(|i| i.name != "lo").or(ifaces.first());
    if let Some(iface) = target {
        net::stack()
            .remove_route(iface.id, dest, mask)
            .map_err(|_| Errno::EINVAL)?;
    }
    Ok(0)
}

fn extract_ipv4(buf: &[u8], offset: usize) -> net::Ipv4Addr {
    net::Ipv4Addr([
        buf[offset + 4],
        buf[offset + 5],
        buf[offset + 6],
        buf[offset + 7],
    ])
}

fn mask_to_prefix_len_bytes(mask: &[u8; 4]) -> u8 {
    u32::from_be_bytes(*mask).leading_ones() as u8
}

//! 网络 ioctl 的链路信息边界。
//!
//! 只读链路信息来自网络设备快照；尚未实现的地址、路由、邻居和写操作明确失败。

use errno::Errno;

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
const IFREQ_SIZE: usize = 40;
const IFNAMSIZ: usize = 16;
const IFCONF_BUF_PTR_OFFSET: usize = 8;
const IFREQ_DATA_OFFSET: usize = IFNAMSIZ;
const SOCKADDR_SIZE: usize = 16;
const ARPHRD_ETHER: u8 = 1;
const IFF_UP: u32 = 1;
const IFF_RUNNING: u32 = 0x40;
const DEFAULT_TX_QUEUE_LEN: i32 = 1000;

pub fn install_net_socket_ioctl_adapter() {
    ::vfs::net_socket::install_net_ioctl_handler(net_ioctl);
}

pub fn net_ioctl(cmd: u32, arg: usize) -> Result<usize, Errno> {
    match cmd {
        SIOCGIFCONF => ioctl_gifconf(arg),
        SIOCGIFFLAGS => ioctl_read(arg, ReadKind::Flags),
        SIOCGIFADDR | SIOCGIFNETMASK | SIOCGIFBRDADDR => ioctl_empty_inet_addr(arg),
        SIOCGIFHWADDR => ioctl_read(arg, ReadKind::HardwareAddress),
        SIOCGIFMTU => ioctl_read(arg, ReadKind::Mtu),
        SIOCGIFINDEX => ioctl_read(arg, ReadKind::Index),
        SIOCGIFTXQLEN => ioctl_read(arg, ReadKind::TxQueueLen),
        SIOCGARP => Err(Errno::ENOENT),
        SIOCSIFFLAGS | SIOCSIFADDR | SIOCSIFNETMASK | SIOCSIFMTU | SIOCSIFTXQLEN | SIOCADDRT
        | SIOCDELRT | SIOCSARP | SIOCDARP => Err(Errno::EOPNOTSUPP),
        _ => Err(Errno::ENOTTY),
    }
}

enum ReadKind {
    Flags,
    HardwareAddress,
    Mtu,
    Index,
    TxQueueLen,
}

fn snapshot_by_name(name: &[u8]) -> Option<net::device::NetDeviceSnapshot> {
    let name = core::str::from_utf8(name).ok()?.trim_end_matches('\0');
    net::device::snapshot_devices()
        .into_iter()
        .find(|device| device.name.as_ref() == name)
}

fn read_name(arg: usize) -> Result<[u8; IFNAMSIZ], Errno> {
    let mut name = [0; IFNAMSIZ];
    copy_from_user(arg, &mut name).map_err(|_| Errno::EFAULT)?;
    Ok(name)
}

fn ioctl_read(arg: usize, kind: ReadKind) -> Result<usize, Errno> {
    let device = snapshot_by_name(&read_name(arg)?).ok_or(Errno::ENODEV)?;
    let mut data = [0; SOCKADDR_SIZE];
    let bytes: &[u8] = match kind {
        ReadKind::Flags => {
            let flags = if device.running {
                IFF_UP | IFF_RUNNING
            } else {
                0
            };
            data[..2].copy_from_slice(&(flags as i16).to_ne_bytes());
            &data[..2]
        }
        ReadKind::HardwareAddress => {
            data[0] = ARPHRD_ETHER;
            data[2..8].copy_from_slice(&device.mac_address);
            &data
        }
        ReadKind::Mtu => {
            data[..4].copy_from_slice(&(device.mtu as i32).to_ne_bytes());
            &data[..4]
        }
        ReadKind::Index => {
            data[..4].copy_from_slice(&(device.id.raw() as i32).to_ne_bytes());
            &data[..4]
        }
        ReadKind::TxQueueLen => {
            data[..4].copy_from_slice(&DEFAULT_TX_QUEUE_LEN.to_ne_bytes());
            &data[..4]
        }
    };
    copy_to_user(arg + IFREQ_DATA_OFFSET, bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

fn ioctl_empty_inet_addr(arg: usize) -> Result<usize, Errno> {
    let _ = snapshot_by_name(&read_name(arg)?).ok_or(Errno::ENODEV)?;
    let mut address = [0; SOCKADDR_SIZE];
    address[0] = 2;
    copy_to_user(arg + IFREQ_DATA_OFFSET, &address).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

fn ioctl_gifconf(arg: usize) -> Result<usize, Errno> {
    let mut len_bytes = [0; 4];
    copy_from_user(arg, &mut len_bytes).map_err(|_| Errno::EFAULT)?;
    let buffer_len = i32::from_ne_bytes(len_bytes).max(0) as usize;
    let mut pointer_bytes = [0; 8];
    copy_from_user(arg + IFCONF_BUF_PTR_OFFSET, &mut pointer_bytes).map_err(|_| Errno::EFAULT)?;
    let buffer = usize::from_ne_bytes(pointer_bytes);
    let mut written = 0usize;
    for device in net::device::snapshot_devices() {
        if written + IFREQ_SIZE > buffer_len {
            break;
        }
        let mut request = [0; IFREQ_SIZE];
        let name = device.name.as_bytes();
        let len = name.len().min(IFNAMSIZ - 1);
        request[..len].copy_from_slice(&name[..len]);
        request[IFREQ_DATA_OFFSET] = 2;
        copy_to_user(buffer + written, &request).map_err(|_| Errno::EFAULT)?;
        written += IFREQ_SIZE;
    }
    copy_to_user(arg, &(written as i32).to_ne_bytes()).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

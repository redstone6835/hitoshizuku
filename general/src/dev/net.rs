//! 网络设备 PnP 集成 + 网络 ioctl。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;

use errno::Errno;
use net::config::IpAddr;

use crate::dev::function::{DevNodeSpec, DeviceClassId, DeviceFunction};
use crate::mm::{copy_from_user, copy_to_user};

/// 网络设备 function 类别 ID。
pub const NET_CLASS: DeviceClassId = DeviceClassId::new("net");

/// 把 [`net::NetDevice`] 适配为通用 [`DeviceFunction`]。
pub struct NetFunction {
    dev_name: Box<str>,
    dev: Arc<net::NetDevice>,
}

impl NetFunction {
    pub fn new(dev_name: &str, dev: Arc<net::NetDevice>) -> Self {
        Self {
            dev_name: dev_name.into(),
            dev,
        }
    }

    pub fn net_device(&self) -> &Arc<net::NetDevice> {
        &self.dev
    }
}

impl DeviceFunction for NetFunction {
    fn class_id(&self) -> DeviceClassId {
        NET_CLASS
    }

    fn dev_name(&self) -> &str {
        &self.dev_name
    }

    fn mark_gone(&self) {
        self.dev.mark_gone();
    }

    fn devnode(&self) -> Option<DevNodeSpec> {
        None
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ── 网络 ioctl ──────────────────────────────────────────────────────────────

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

/// 处理网络 socket 上的 ioctl。由 syscall 层调用。
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
        // 设置类 ioctl：当前只读栈，返回 EPERM（与 Linux 非 root 行为一致）
        // TODO: 在 ManagedInterface 中引入可变 runtime config 以支持运行时修改
        SIOCSIFFLAGS | SIOCSIFADDR | SIOCSIFNETMASK | SIOCSIFMTU | SIOCSIFTXQLEN => {
            Err(Errno::EPERM)
        }
        // 路由管理：尚未实现路由表修改
        // TODO: 阶段 3 实现路由表管理
        SIOCADDRT | SIOCDELRT => Err(Errno::EPERM),
        // ARP 邻居表查询：通过 mygo-smoltcp 暴露的 neighbor_cache 获取真实数据
        SIOCGARP => ioctl_get_arp(arg),
        SIOCSARP | SIOCDARP => Err(Errno::EPERM),
        _ => Err(Errno::ENOTTY),
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

/// SIOCGARP — 查询 ARP 邻居表中的 MAC 地址。
/// struct arpreq 布局（简化）：sockaddr arp_pa(16) + sockaddr arp_ha(16) + flags(4) + ...
fn ioctl_get_arp(arg: usize) -> Result<usize, Errno> {
    // 读取 arpreq 前 16 字节 (arp_pa: struct sockaddr)
    let mut pa_buf = [0u8; 16];
    copy_from_user(arg, &mut pa_buf).map_err(|_| Errno::EFAULT)?;
    // 提取目标 IPv4 地址 (sockaddr_in: family[2] + port[2] + addr[4])
    let target_ip = [pa_buf[4], pa_buf[5], pa_buf[6], pa_buf[7]];
    let target = net::IpAddr::V4(net::Ipv4Addr(target_ip));
    // 遍历所有接口邻居表查找匹配
    let neighbors = net::stack().all_neighbors();
    for (_iface_id, entries) in &neighbors {
        for entry in entries {
            if entry.ip_addr == target {
                // 写入 arp_ha (offset 16, struct sockaddr: family[2] + data[14])
                let mut ha = [0u8; 16];
                ha[0] = 1; // ARPHRD_ETHER
                ha[2..8].copy_from_slice(&entry.hw_addr);
                copy_to_user(arg + 16, &ha).map_err(|_| Errno::EFAULT)?;
                // 写入 arp_flags (offset 32): ATF_COM (0x02) 表示 complete
                let flags = 0x02u32.to_ne_bytes();
                copy_to_user(arg + 32, &flags).map_err(|_| Errno::EFAULT)?;
                return Ok(0);
            }
        }
    }
    Err(Errno::ENOENT)
}

/// 检查是否为网络 ioctl 命令。
pub fn is_net_ioctl(cmd: u32) -> bool {
    matches!(
        cmd,
        SIOCADDRT
            | SIOCDELRT
            | SIOCGIFCONF
            | SIOCGIFFLAGS
            | SIOCSIFFLAGS
            | SIOCGIFADDR
            | SIOCSIFADDR
            | SIOCGIFBRDADDR
            | SIOCGIFNETMASK
            | SIOCSIFNETMASK
            | SIOCGIFMTU
            | SIOCSIFMTU
            | SIOCGIFHWADDR
            | SIOCGIFINDEX
            | SIOCGIFTXQLEN
            | SIOCSIFTXQLEN
            | SIOCGARP
            | SIOCSARP
            | SIOCDARP
    )
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
    copy_from_user(arg, &mut len_buf).map_err(|_| Errno::EFAULT)?;
    let buf_len = i32::from_ne_bytes(len_buf) as usize;

    let mut ptr_buf = [0u8; 8];
    copy_from_user(arg + 8, &mut ptr_buf).map_err(|_| Errno::EFAULT)?;
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
        ifreq[IFNAMSIZ] = 2; // AF_INET
        copy_to_user(buf_ptr + offset, &ifreq).map_err(|_| Errno::EFAULT)?;
        offset += IFREQ_SIZE;
    }
    let actual_len = (offset as i32).to_ne_bytes();
    copy_to_user(arg, &actual_len).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

fn ioctl_gifflags(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let flags = (iface.flags as i16).to_ne_bytes();
    write_ifreq_data(arg, IFNAMSIZ, &[flags[0], flags[1]])?;
    Ok(0)
}

fn ioctl_gifaddr(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mut sa = [0u8; 16];
    sa[0] = 2; // AF_INET
    if let Some(cidr) = iface
        .addresses
        .iter()
        .find(|c| matches!(c.addr, IpAddr::V4(_)))
    {
        if let IpAddr::V4(v4) = cidr.addr {
            sa[4..8].copy_from_slice(&v4.0);
        }
    }
    write_ifreq_data(arg, IFNAMSIZ, &sa)?;
    Ok(0)
}

fn ioctl_gifnetmask(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mut sa = [0u8; 16];
    sa[0] = 2; // AF_INET
    if let Some(cidr) = iface
        .addresses
        .iter()
        .find(|c| matches!(c.addr, IpAddr::V4(_)))
    {
        sa[4..8].copy_from_slice(&prefix_to_netmask(cidr.prefix_len));
    }
    write_ifreq_data(arg, IFNAMSIZ, &sa)?;
    Ok(0)
}

fn ioctl_gifhwaddr(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mut sa = [0u8; 16];
    sa[0] = 1; // ARPHRD_ETHER
    sa[2..8].copy_from_slice(&iface.mac);
    write_ifreq_data(arg, IFNAMSIZ, &sa)?;
    Ok(0)
}

fn ioctl_gifmtu(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mtu = (iface.mtu as i32).to_ne_bytes();
    write_ifreq_data(arg, IFNAMSIZ, &mtu)?;
    Ok(0)
}

fn ioctl_gifindex(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let index = (iface.id.raw() as i32 + 1).to_ne_bytes();
    write_ifreq_data(arg, IFNAMSIZ, &index)?;
    Ok(0)
}

fn ioctl_giftxqlen(arg: usize) -> Result<usize, Errno> {
    // Linux 默认 txqlen 为 1000，virtio-net 驱动无单独 txqlen 暴露
    let name = read_ifreq_name(arg)?;
    let _iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let txqlen = 1000i32.to_ne_bytes();
    write_ifreq_data(arg, IFNAMSIZ, &txqlen)?;
    Ok(0)
}

fn ioctl_gifbrdaddr(arg: usize) -> Result<usize, Errno> {
    let name = read_ifreq_name(arg)?;
    let iface = find_iface_by_name(&name).ok_or(Errno::ENODEV)?;
    let mut sa = [0u8; 16];
    sa[0] = 2; // AF_INET
    if let Some(cidr) = iface
        .addresses
        .iter()
        .find(|c| matches!(c.addr, IpAddr::V4(_)))
    {
        if let IpAddr::V4(v4) = cidr.addr {
            let mask = prefix_to_netmask(cidr.prefix_len);
            for i in 0..4 {
                sa[4 + i] = v4.0[i] | !mask[i];
            }
        }
    }
    write_ifreq_data(arg, IFNAMSIZ, &sa)?;
    Ok(0)
}

//! 经典内存字符设备(/dev/mem、/dev/kmem、/dev/port)与 /dev/kmsg 的 VFS 适配。
//!
//! 这些设备没有 PnP backing，也不是 typed 硬件对象；它们直接依赖内核的物理内存
//! 直映与日志环能力。因此和 RTC/loop 一样，这里用 custom devnode 走 devtmpfs 的
//! 适配器注册路径，把用户态偏移/写语义翻译成物理内存访问与 printk 环写入。
//!
//! 取舍(无法完整复刻 Linux 语义的部分)：
//! - `/dev/mem` 只读；仅允许读取 buddy 管理的物理内存(`physical_numa_node` 命中),
//!   等价于 Linux `CONFIG_STRICT_DEVMEM` 的只读退化。设备 MMIO 空洞返回短读。
//! - `/dev/kmem` 只读；偏移按物理地址解释(本内核内核地址即直映物理地址,近似 Linux
//!   "内核虚拟内存"语义)。
//! - `/dev/port` 在 LoongArch64/RISC-V64 上没有 x86 风格 IO 端口空间,读写返回
//!   ENXIO(无可行实现,在任务书中注明)。
//! - `/dev/kmsg` 读按字节偏移切分日志环快照,写解析可选 `<N>` 优先级前缀后写入
//!   printk 环。Linux 的按 seq 定位与 continuation 标志未完整复刻。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::any::Any;
use core::ops::ControlFlow;

use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeOps};

use crate::vfs::device_files::spec::{CustomDevNodeKind, CustomDevNodeSpec, DevNodeSpec};
use crate::vfs::devtmpfs::{
    DevTmpfsCustomNodeAdapter, DevTmpfsCustomNodeAdapterRegistration, DevTmpfsStaticNode,
    register_custom_devnode_adapter, register_static_dev_node,
};

const MEM_DEVNODE_OWNER: &str = "mem-devnode";
const MEM_ADAPTER_NAME: &str = "mem";

/// 静态节点名与 Linux devtmpfs 布局一致。
const MEM_NODE_NAME: &str = "mem";
const KMEM_NODE_NAME: &str = "kmem";
const PORT_NODE_NAME: &str = "port";
const KMSG_NODE_NAME: &str = "kmsg";

/// 本适配层解释的字符设备类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemKind {
    Mem,
    Kmem,
    Port,
    Kmsg,
}

/// devtmpfs 自定义节点中的 typed endpoint。
pub struct MemCharEndpoint {
    kind: MemKind,
}

impl MemCharEndpoint {
    pub fn mem() -> Self {
        Self { kind: MemKind::Mem }
    }

    pub fn kmem() -> Self {
        Self {
            kind: MemKind::Kmem,
        }
    }

    pub fn port() -> Self {
        Self {
            kind: MemKind::Port,
        }
    }

    pub fn kmsg() -> Self {
        Self {
            kind: MemKind::Kmsg,
        }
    }
}

/// 注册 mem/kmem/port/kmsg 的 custom devnode 适配器。
pub fn register_devtmpfs_adapter() -> VfsResult<DevTmpfsCustomNodeAdapterRegistration> {
    register_custom_devnode_adapter(DevTmpfsCustomNodeAdapter::new(
        MEM_DEVNODE_OWNER,
        MEM_ADAPTER_NAME,
        build_mem_inode_ops,
    ))
}

fn build_mem_inode_ops(
    spec: &CustomDevNodeSpec,
) -> VfsResult<Option<Arc<dyn InodeOps + Send + Sync>>> {
    let payload = spec.payload();
    let Some(endpoint) = payload.as_ref().downcast_ref::<MemCharEndpoint>() else {
        return Ok(None);
    };
    Ok(Some(Arc::new(MemInodeOps {
        kind: endpoint.kind,
    })))
}

struct MemInodeOps {
    kind: MemKind,
}

impl InodeOps for MemInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &vfs::cred::Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(MemFileOps { kind: self.kind }))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct MemFileOps {
    kind: MemKind,
}

impl FileOps for MemFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        match self.kind {
            MemKind::Mem | MemKind::Kmem => Ok(read_physical_memory(offset, buf)),
            MemKind::Port => Err(VfsError::NoSuchDeviceOrAddress),
            MemKind::Kmsg => kmsg_read(buf, offset),
        }
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        match self.kind {
            // 只读退化(等价 Linux STRICT_DEVMEM 只读)。
            MemKind::Mem | MemKind::Kmem => Err(VfsError::OperationNotPermitted),
            MemKind::Port => Err(VfsError::NoSuchDeviceOrAddress),
            MemKind::Kmsg => kmsg_write(buf),
        }
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, interest: PollEvents) -> PollEvents {
        let ready = match self.kind {
            MemKind::Kmsg => PollEvents::POLLIN.with(PollEvents::POLLOUT),
            MemKind::Port => PollEvents(0),
            MemKind::Mem | MemKind::Kmem => PollEvents::POLLIN.with(PollEvents::POLLOUT),
        };
        ready.intersect(interest)
    }

    fn ioctl(&self, _cmd: vfs::file::IoctlCmd, _arg: usize) -> Result<usize, errno::Errno> {
        Err(errno::Errno::ENOTTY)
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 读取物理内存窗口(只读,按页校验是否为 buddy 管理内存)。
fn read_physical_memory(offset: u64, buf: &mut [u8]) -> usize {
    let page_size = allocator::PAGE_SIZE;
    let mut done = 0usize;
    while done < buf.len() {
        let paddr = offset.saturating_add(done as u64);
        let page = paddr & !(page_size as u64 - 1);
        // 只允许读取 buddy 管理的物理内存,避免把 MMIO 空洞翻译成内核 fault。
        if allocator::KERNEL_ALLOCATOR
            .physical_numa_node(page as usize)
            .is_none()
        {
            break;
        }
        let in_page = (paddr & (page_size as u64 - 1)) as usize;
        let take = (page_size - in_page).min(buf.len() - done);
        let Some(vaddr) = allocator::KERNEL_ALLOCATOR.physical_to_virtual(paddr as usize) else {
            break;
        };
        // Safety: vaddr 是受管物理页在直映中的虚拟地址,[vaddr, vaddr+take) 不跨页,
        // 且该页已经通过 physical_numa_node 校验为 buddy 管理内存,读取是安全的。
        let src = unsafe { core::slice::from_raw_parts(vaddr as *const u8, take) };
        buf[done..done + take].copy_from_slice(src);
        done += take;
    }
    done
}

/// 把 /dev/kmsg 的读缓冲按字节偏移切分日志环格式化快照。
fn kmsg_read(buf: &mut [u8], offset: u64) -> VfsResult<usize> {
    let text = kmsg_ring_text();
    let bytes = text.as_bytes();
    let start = (offset as usize).min(bytes.len());
    let available = &bytes[start..];
    let n = buf.len().min(available.len());
    buf[..n].copy_from_slice(&available[..n]);
    Ok(n)
}

/// 生成 /dev/kmsg 的 dmesg 风格格式化快照。
fn kmsg_ring_text() -> String {
    let mut text = String::new();
    for record in log::LOGGER.read_all() {
        let (secs, nanos) = log::format_timestamp(record.timestamp);
        let _ = alloc::fmt::write(
            &mut text,
            format_args!("[{:6}.{:06}] {}\n", secs, nanos / 1000, record.message),
        );
    }
    text
}

/// 把 /dev/kmsg 的写缓冲解析为 printk 环记录。
///
/// 支持 Linux 的 `<N>` 优先级前缀;未带前缀时按 Info 级别记录。
fn kmsg_write(buf: &[u8]) -> VfsResult<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    // 拒绝包含内嵌 NUL 的输入(printk 环记录的是字符串)。
    let mut end = buf.len();
    if buf.contains(&0) {
        end = buf.iter().position(|byte| *byte == 0).unwrap();
    }
    let message = core::str::from_utf8(&buf[..end]).map_err(|_| VfsError::InvalidArgument)?;
    let (level, body) = parse_kmsg_priority(message);
    // 去掉一条记录末尾的换行(printk 环内记录本身不含换行)。
    let body = body.strip_suffix('\n').unwrap_or(body);
    log::logger_entry(level, log::get_timestamp_ns(), body);
    Ok(buf.len())
}

/// 解析 `/dev/kmsg` 写路径的可选 `<N>` 优先级前缀。
fn parse_kmsg_priority(line: &str) -> (log::LogLevel, &str) {
    let Some(rest) = line.strip_prefix('<') else {
        return (log::LogLevel::Info, line);
    };
    let Some(close) = rest.find('>') else {
        return (log::LogLevel::Info, line);
    };
    let digits = &rest[..close];
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return (log::LogLevel::Info, line);
    }
    let Ok(prio) = digits.parse::<u8>() else {
        return (log::LogLevel::Info, line);
    };
    let level = match prio.min(7) {
        0 => log::LogLevel::Emergency,
        1 => log::LogLevel::Alert,
        2 => log::LogLevel::Critical,
        3 => log::LogLevel::Error,
        4 => log::LogLevel::Warning,
        5 => log::LogLevel::Notice,
        6 => log::LogLevel::Info,
        _ => log::LogLevel::Debug,
    };
    (level, &rest[close + 1..])
}

fn mem_dev_node() -> VfsResult<DevNodeSpec> {
    Ok(DevNodeSpec::custom(CustomDevNodeSpec::try_new(
        MEM_NODE_NAME,
        CustomDevNodeKind::CharDevice,
        Arc::new(MemCharEndpoint::mem()),
    )?))
}

fn kmem_dev_node() -> VfsResult<DevNodeSpec> {
    Ok(DevNodeSpec::custom(CustomDevNodeSpec::try_new(
        KMEM_NODE_NAME,
        CustomDevNodeKind::CharDevice,
        Arc::new(MemCharEndpoint::kmem()),
    )?))
}

fn port_dev_node() -> VfsResult<DevNodeSpec> {
    Ok(DevNodeSpec::custom(CustomDevNodeSpec::try_new(
        PORT_NODE_NAME,
        CustomDevNodeKind::CharDevice,
        Arc::new(MemCharEndpoint::port()),
    )?))
}

fn kmsg_dev_node() -> VfsResult<DevNodeSpec> {
    Ok(DevNodeSpec::custom(CustomDevNodeSpec::try_new(
        KMSG_NODE_NAME,
        CustomDevNodeKind::CharDevice,
        Arc::new(MemCharEndpoint::kmsg()),
    )?))
}

/// 注册本适配层的全部内容(adapter + 静态节点 + 节点权限策略)。
///
/// 幂等:同一 owner/name 重复注册由 devtmpfs 内部去重。启动期在 devtmpfs
/// mount 之前调用,保证 static node 首次绑定时 custom adapter 已经就绪。
pub fn register_all() -> VfsResult<()> {
    register_devtmpfs_adapter()?;
    // 先登记权限策略再注册静态节点:若 devtmpfs 已经挂载,节点会立即绑定,
    // 此时策略必须已就绪(当前这些节点均为 0600,与默认一致,但保持顺序正确)。
    register_node_policies()?;
    register_static_nodes()
}

/// 注册 mem/kmem/port/kmsg 四个静态节点。
///
/// 使用批量注册入口获得事务语义;任一节点失败都会回滚本轮已经发布的节点。
pub fn register_static_nodes() -> VfsResult<()> {
    register_static_dev_node(DevTmpfsStaticNode::new(
        MEM_DEVNODE_OWNER,
        MEM_NODE_NAME,
        mem_dev_node,
    ))?;
    register_static_dev_node(DevTmpfsStaticNode::new(
        MEM_DEVNODE_OWNER,
        KMEM_NODE_NAME,
        kmem_dev_node,
    ))?;
    register_static_dev_node(DevTmpfsStaticNode::new(
        MEM_DEVNODE_OWNER,
        PORT_NODE_NAME,
        port_dev_node,
    ))?;
    register_static_dev_node(DevTmpfsStaticNode::new(
        MEM_DEVNODE_OWNER,
        KMSG_NODE_NAME,
        kmsg_dev_node,
    ))?;
    Ok(())
}

/// 注册本适配层的节点权限策略(与 Linux devtmpfs 的 root:root 默认对齐)。
pub fn register_node_policies() -> VfsResult<()> {
    let register = |name: &'static str, mode: u16| {
        crate::vfs::devtmpfs::register_node_policy(
            name,
            crate::vfs::devtmpfs::DevNodePolicy::new(mode),
        )
    };
    register(MEM_NODE_NAME, 0o600)?;
    register(KMEM_NODE_NAME, 0o600)?;
    register(PORT_NODE_NAME, 0o600)?;
    register(KMSG_NODE_NAME, 0o600)?;
    Ok(())
}

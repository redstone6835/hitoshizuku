//! devtmpfs — 设备临时文件系统
//!
//! # 设计要点
//!
//! 设备节点的 inode 直接持有设备对象引用（`CharDevice` 或 `Arc<BlockDevice>`），
//! 而非设备名称字符串。`open()` 时零查找：已在绑定时解析，运行时直接调用。
//!
//! ```text
//! bind_char("uart0", dev: CharDevice)
//!   └─ 创建 Inode，InodeOps = DevCharOps { dev, tty }
//!         └─ open() → 直接访问 dev              // 无查找，直接构造
//!
//! bind_block("disk/root", dev: Arc<BlockDevice>)
//!   └─ 创建 Inode，InodeOps = DevBlockOps { dev: Arc<BlockDevice> }
//!         └─ open() → 直接访问 dev              // 无查找，直接构造
//!
//! bind_symlink("disk/by-name/root", "../root")
//!   └─ 创建 Symlink Inode，InodeOps = DevSymlinkOps { target: "../root" }
//!         └─ path lookup → readlink() → 按标准相对链接规则继续解析
//! ```
//!
//! # 文件系统结构
//!
//! devtmpfs 是一棵普通目录树。每个目录 inode 维护本级
//! `name → Arc<Inode>` 的 `BTreeMap`，作为 `lookup` 和 `readdir` 的数据源。
//! 设备驱动可以声明主节点、目录化节点或符号链接节点；devtmpfs 不内建任何固定
//! 设备别名。
//!
//! 整个文件系统通过 `mount -t devtmpfs` 挂载到 `/dev`，之后通过
//! [`DevTmpfsSuperblockOps::bind_char`] / [`DevTmpfsSuperblockOps::bind_block`] /
//! [`DevTmpfsSuperblockOps::bind_symlink`] / [`DevTmpfsSuperblockOps::bind_node`]
//! 动态增删节点。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use errno::Errno;
use sched::operation;
use vfs::cred::{Credentials, Gid, Uid};
use vfs::dentry::{Dentry, SmallStr};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::mount::MountFlags;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{FsDriver, FsDriverFlags, Superblock, SuperblockOps};
use vfs::sync::Spinlock;

use crate::dev::bio::{BioBuffer, BioError, BioOp, BlockRange};
use crate::dev::block::{BlockDevice, BlockFeatures};
use crate::dev::char::{CharControlRequest, CharControlResponse, CharDevice, CharIoError};
use crate::dev::control::{BlockControlRequest, BlockControlResponse, BlockIoHints, ControlError};
use crate::dev::enumerate::DEVICES;
use crate::dev::function::{CustomDevNodeSpec, DevNodeSet, DevNodeSpec};
use crate::dev::pnp::{PnpDevtmpfsCallbacks, PnpError, set_devtmpfs_callbacks};
use crate::mm::{copy_from_user, copy_to_user};

// ───────── 全局实例计数器 ─────────

static DEVTMPFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);

static DEVTMPFS_SINGLETON_SB: Spinlock<Option<&'static Arc<Superblock>>> = Spinlock::new(None);
static TTY_SHARED_STATES: Spinlock<BTreeMap<String, Weak<TtySharedState>>> =
    Spinlock::new(BTreeMap::new());
static STATIC_DEV_NODES: Spinlock<Vec<DevTmpfsStaticNode>> = Spinlock::new(Vec::new());
static BUILTIN_STATIC_NODES_REGISTERED: AtomicBool = AtomicBool::new(false);

/// devtmpfs 静态节点声明。
///
/// 静态节点用于 random/null/zero 这类没有 PnP backing device、但又必须出现在
/// `/dev` 的基础设备。声明只保存构造器，真正的 inode 仍通过 [`DevNodeSpec`]
/// 进入统一绑定路径，避免 devtmpfs 为每种特殊设备增加分支。
#[derive(Clone, Copy)]
pub struct DevTmpfsStaticNode {
    owner: &'static str,
    name: &'static str,
    build: fn() -> DevNodeSpec,
}

impl DevTmpfsStaticNode {
    /// 构造一个静态节点声明。
    ///
    /// `owner` 是声明来源的稳定名字，用于让同一组件重复初始化时幂等返回，同时
    /// 仍能发现两个不同组件抢占同一个 `/dev` 名称的真实冲突。
    pub const fn new(owner: &'static str, name: &'static str, build: fn() -> DevNodeSpec) -> Self {
        Self { owner, name, build }
    }

    pub const fn name(self) -> &'static str {
        self.name
    }
}

fn null_static_node() -> DevNodeSpec {
    DevNodeSpec::Char {
        name: "null".into(),
        dev: CharDevice::null(),
    }
}

fn zero_static_node() -> DevNodeSpec {
    DevNodeSpec::Char {
        name: "zero".into(),
        dev: CharDevice::zero(),
    }
}

fn bind_static_node(ops: &DevTmpfsSuperblockOps, node: DevTmpfsStaticNode) -> VfsResult<()> {
    ops.bind_node(&(node.build)())
}

fn remove_static_dev_node_record(owner: &'static str, name: &str) -> Option<DevTmpfsStaticNode> {
    let mut nodes = STATIC_DEV_NODES.lock();
    let Some(index) = nodes
        .iter()
        .position(|existing| existing.owner == owner && existing.name == name)
    else {
        return None;
    };
    Some(nodes.remove(index))
}

fn restore_static_dev_node_record(node: DevTmpfsStaticNode) -> VfsResult<()> {
    let mut nodes = STATIC_DEV_NODES.lock();
    if let Some(existing) = nodes.iter().find(|existing| existing.name == node.name) {
        if existing.owner == node.owner {
            return Ok(());
        }
        return Err(VfsError::AlreadyExists);
    }
    nodes.try_reserve(1).map_err(|_| VfsError::NoSpace)?;
    nodes.push(node);
    Ok(())
}

fn ensure_builtin_static_nodes_registered() -> VfsResult<()> {
    if BUILTIN_STATIC_NODES_REGISTERED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(());
    }

    if let Err(err) = register_static_dev_node(DevTmpfsStaticNode::new(
        "devtmpfs-core",
        "null",
        null_static_node,
    )) {
        BUILTIN_STATIC_NODES_REGISTERED.store(false, Ordering::Release);
        return Err(err);
    }
    if let Err(err) = register_static_dev_node(DevTmpfsStaticNode::new(
        "devtmpfs-core",
        "zero",
        zero_static_node,
    )) {
        let _ = unregister_static_dev_node("devtmpfs-core", "null");
        BUILTIN_STATIC_NODES_REGISTERED.store(false, Ordering::Release);
        return Err(err);
    }
    Ok(())
}

/// 注册一个非 PnP 静态 devtmpfs 节点。
///
/// 如果 devtmpfs 已经安装到 PnP bridge，注册会立即把节点补进现有 superblock；
/// 否则节点会留在注册表中，首次 mount devtmpfs 时批量绑定。调用方不需要关心
/// DTB/ACPI 等启动路径的先后顺序。
pub fn register_static_dev_node(node: DevTmpfsStaticNode) -> VfsResult<()> {
    split_devtmpfs_path(node.name)?;
    {
        let mut nodes = STATIC_DEV_NODES.lock();
        if let Some(existing) = nodes.iter().find(|existing| existing.name == node.name) {
            if existing.owner == node.owner {
                return Ok(());
            }
            return Err(VfsError::AlreadyExists);
        }
        nodes.try_reserve(1).map_err(|_| VfsError::NoSpace)?;
        nodes.push(node);
    }

    if let Some(sb) = mounted_devtmpfs_sb() {
        let ops = sb
            .downcast_ops::<DevTmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
        if let Err(err) = bind_static_node(ops, node) {
            STATIC_DEV_NODES
                .lock()
                .retain(|existing| existing.name != node.name);
            return Err(err);
        }
    }

    Ok(())
}

/// 注销一个非 PnP 静态 devtmpfs 节点。
///
/// 这给未来可卸载的内核内建服务提供对称生命周期：注册表先移除声明，再从当前
/// devtmpfs 单例中删除节点。不存在的节点按 `NotFound` 返回，避免调用方误以为
/// 已完成解绑。
pub fn unregister_static_dev_node(owner: &'static str, name: &str) -> VfsResult<()> {
    split_devtmpfs_path(name)?;
    let Some(node) = remove_static_dev_node_record(owner, name) else {
        return Err(VfsError::NotFound);
    };

    if let Some(sb) = mounted_devtmpfs_sb() {
        let ops = sb
            .downcast_ops::<DevTmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
        match ops.unbind(name) {
            Ok(()) | Err(VfsError::NotFound) => {}
            Err(err) => {
                let _ = restore_static_dev_node_record(node);
                return Err(err);
            }
        }
    }

    Ok(())
}

const DEVTMPFS_NAME_MAX: usize = 255;

fn devtmpfs_compat_default_dir_mode() -> FileMode {
    FileMode::new(0o755)
}

fn devtmpfs_compat_default_symlink_mode() -> FileMode {
    FileMode::new(0o777)
}

fn devtmpfs_compat_default_device_mode() -> FileMode {
    FileMode::new(0o660)
}

fn devtmpfs_compat_default_uid() -> Uid {
    Uid::ROOT
}

fn devtmpfs_compat_default_gid() -> Gid {
    Gid::ROOT
}

fn devtmpfs_compat_block_size() -> u32 {
    512
}

fn devtmpfs_compat_char_poll_default() -> PollEvents {
    PollEvents::POLLIN.with(PollEvents::POLLOUT)
}

fn devtmpfs_compat_max_blocks_per_io() -> u32 {
    1024
}

fn validate_devtmpfs_component(name: &str) -> VfsResult<()> {
    if name.is_empty() || name.contains('/') || name.contains('\0') || name == "." || name == ".." {
        return Err(VfsError::InvalidArgument);
    }
    if name.len() > DEVTMPFS_NAME_MAX {
        return Err(VfsError::NameTooLong);
    }
    Ok(())
}

fn split_devtmpfs_path(path: &str) -> VfsResult<Vec<&str>> {
    if path.is_empty() || path.starts_with('/') || path.ends_with('/') || path.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }

    let mut components = Vec::new();
    for component in path.split('/') {
        validate_devtmpfs_component(component)?;
        components.push(component);
    }
    if components.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    Ok(components)
}

fn validate_symlink_target(target: &str) -> VfsResult<()> {
    if target.is_empty() || target.contains('\0') {
        return Err(VfsError::InvalidArgument);
    }
    Ok(())
}

/// 安装 PnP 到 devtmpfs 的桥接。
///
/// 安装后，PnpDevice 注册带 [`DevNodeSpec`] 的 function 时，会自动在这个
/// devtmpfs superblock 中创建或删除对应 `/dev` 节点。桥接只消费 `DevNodeSpec`
/// 携带的设备对象，不 downcast 具体 function 类型。
pub fn install_pnp_bridge(dev_sb: Arc<Superblock>) -> Result<(), PnpError> {
    let (dev_sb, first_publish) = publish_devtmpfs_sb(dev_sb);
    dev_sb
        .downcast_ops::<DevTmpfsSuperblockOps>()
        .ok_or(PnpError::NoDevtmpfs)?;

    set_devtmpfs_callbacks(PnpDevtmpfsCallbacks {
        bind: pnp_bind_cb,
        unbind: pnp_unbind_cb,
    });
    if first_publish {
        for func in DEVICES.functions.list() {
            if let Some(nodes) = func.devnodes() {
                // 允许 PnP core 在 devtmpfs 之前完成底层注册；bridge 首次安装后补齐
                // POSIX 节点投影。重复安装 bridge 时不能再次 bind 同一批节点，否则
                // 幂等初始化路径会被已有节点误判为失败。
                pnp_bind_cb(&nodes)?;
            }
        }
    }
    Ok(())
}

fn pnp_devtmpfs_sb() -> Result<&'static Arc<Superblock>, PnpError> {
    DEVTMPFS_SINGLETON_SB.lock().ok_or(PnpError::NoDevtmpfs)
}

fn mounted_devtmpfs_sb() -> Option<Arc<Superblock>> {
    let guard = DEVTMPFS_SINGLETON_SB.lock();
    guard.as_ref().map(|sb| Arc::clone(*sb))
}

fn publish_devtmpfs_sb(dev_sb: Arc<Superblock>) -> (Arc<Superblock>, bool) {
    let mut guard = DEVTMPFS_SINGLETON_SB.lock();
    if let Some(existing) = guard.as_ref() {
        return (Arc::clone(*existing), false);
    }

    // devtmpfs 是全局设备名字空间的投影，superblock 生命周期等同内核生命周期。
    // 这里泄露一个 Arc 作为单例锚点，后续 mount/bridge/static node 注册都只克隆它。
    let leaked: &'static Arc<Superblock> = Box::leak(Box::new(dev_sb));
    *guard = Some(leaked);
    (Arc::clone(leaked), true)
}

fn pnp_bind_cb(nodes: &DevNodeSet) -> Result<(), PnpError> {
    let sb = pnp_devtmpfs_sb()?;
    let ops = sb
        .downcast_ops::<DevTmpfsSuperblockOps>()
        .ok_or(PnpError::NoDevtmpfs)?;
    ops.bind_nodes(nodes).map_err(|_| PnpError::DevtmpfsError)
}

fn pnp_unbind_cb(nodes: &DevNodeSet) -> Result<(), PnpError> {
    let sb = pnp_devtmpfs_sb()?;
    let ops = sb
        .downcast_ops::<DevTmpfsSuperblockOps>()
        .ok_or(PnpError::NoDevtmpfs)?;
    ops.unbind_nodes(nodes).map_err(|_| PnpError::DevtmpfsError)
}

// ───────── 字符设备 FileOps（内联适配器） ─────────

fn map_char_err(e: CharIoError) -> VfsError {
    match e {
        CharIoError::HardwareError => VfsError::Io,
        CharIoError::Unavailable => VfsError::NoDevice,
        CharIoError::Timeout => VfsError::TimedOut,
    }
}

fn map_control_errno(e: ControlError) -> Errno {
    e.to_errno()
}

const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TCSETSW: usize = 0x5403;
const TCSETSF: usize = 0x5404;
const TCSBRK: usize = 0x5409;
const TCXONC: usize = 0x540a;
const TCFLSH: usize = 0x540b;
const TIOCEXCL: usize = 0x540c;
const TIOCNXCL: usize = 0x540d;
const TIOCSCTTY: usize = 0x540e;
const TIOCGPGRP: usize = 0x540f;
const TIOCSPGRP: usize = 0x5410;
const TIOCOUTQ: usize = 0x5411;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSWINSZ: usize = 0x5414;
const FIONREAD: usize = 0x541b;
const TIOCNOTTY: usize = 0x5422;
const TIOCSETD: usize = 0x5423;
const TIOCGETD: usize = 0x5424;
const TCSBRKP: usize = 0x5425;
const TIOCGSID: usize = 0x5429;
const TCGETS2: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, b'T' as usize, 0x2a, LINUX_TERMIOS2_LEN).raw();
const TCSETS2: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_WRITE, b'T' as usize, 0x2b, LINUX_TERMIOS2_LEN).raw();
const TCSETSW2: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_WRITE, b'T' as usize, 0x2c, LINUX_TERMIOS2_LEN).raw();
const TCSETSF2: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_WRITE, b'T' as usize, 0x2d, LINUX_TERMIOS2_LEN).raw();
const TCIFLUSH: usize = 0;
const TCOFLUSH: usize = 1;
const TCIOFLUSH: usize = 2;
const TTY_DEFAULT_BREAK_MS: u32 = 250;
const TTY_BREAK_UNIT_MS: u32 = 100;

const BLKROGET: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 94, 0).raw();
const BLKGETSIZE: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 96, 0).raw();
const BLKFLSBUF: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 97, 0).raw();
const BLKSSZGET: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 104, 0).raw();
const BLKBSZGET: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, 0x12, 112, core::mem::size_of::<usize>()).raw();
const BLKGETSIZE64: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, 0x12, 114, core::mem::size_of::<usize>()).raw();
const BLKIOMIN: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 120, 0).raw();
const BLKIOOPT: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 121, 0).raw();
const BLKALIGNOFF: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 122, 0).raw();
const BLKPBSZGET: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 123, 0).raw();
const BLKDISCARDZEROES: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 124, 0).raw();
const BLKROTATIONAL: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 126, 0).raw();
const BLKGETDISKSEQ: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, 0x12, 128, core::mem::size_of::<u64>()).raw();

const LINUX_TERMIOS_LEN: usize = 36;
const LINUX_TERMIOS2_LEN: usize = 44;
const LINUX_WINSIZE_LEN: usize = 8;
const NCCS_OFFSET: usize = 17;
const NCCS_VINTR: usize = 0;
const NCCS_VQUIT: usize = 1;
const NCCS_VERASE: usize = 2;
const NCCS_VKILL: usize = 3;
const NCCS_VEOF: usize = 4;
const NCCS_VTIME: usize = 5;
const NCCS_VMIN: usize = 6;
const NCCS_VSUSP: usize = 10;

const ICRNL: u32 = 0x0100;
const IXON: u32 = 0x0400;
const OPOST: u32 = 0x0001;
const ONLCR: u32 = 0x0004;
const ISIG: u32 = 0x0001;
const ICANON: u32 = 0x0002;
const ECHO: u32 = 0x0008;
const ECHOE: u32 = 0x0010;
const ECHOK: u32 = 0x0020;

#[derive(Clone, Copy)]
struct LinuxTermios {
    raw: [u8; LINUX_TERMIOS_LEN],
}

impl LinuxTermios {
    fn new_default() -> Self {
        let mut raw = [0u8; LINUX_TERMIOS_LEN];
        put_u32(&mut raw, 0, 0x0500); // ICRNL | IXON
        put_u32(&mut raw, 4, 0x0005); // OPOST | ONLCR
        put_u32(&mut raw, 8, 0x04bf); // B38400 | CS8 | CREAD | HUPCL
        put_u32(&mut raw, 12, 0x803b); // ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN
        raw[17] = 3; // VINTR
        raw[18] = 28; // VQUIT
        raw[19] = 127; // VERASE
        raw[20] = 21; // VKILL
        raw[21] = 4; // VEOF
        raw[22] = 0; // VTIME
        raw[23] = 1; // VMIN
        raw[25] = 17; // VSTART
        raw[26] = 19; // VSTOP
        raw[27] = 26; // VSUSP
        Self { raw }
    }

    fn as_termios2_bytes(&self) -> [u8; LINUX_TERMIOS2_LEN] {
        let mut out = [0u8; LINUX_TERMIOS2_LEN];
        out[..LINUX_TERMIOS_LEN].copy_from_slice(&self.raw);
        put_u32(&mut out, 36, 38400);
        put_u32(&mut out, 40, 38400);
        out
    }

    fn iflag(&self) -> u32 {
        u32::from_le_bytes(self.raw[0..4].try_into().unwrap())
    }

    fn oflag(&self) -> u32 {
        u32::from_le_bytes(self.raw[4..8].try_into().unwrap())
    }

    fn lflag(&self) -> u32 {
        u32::from_le_bytes(self.raw[12..16].try_into().unwrap())
    }

    fn cc(&self, index: usize) -> u8 {
        self.raw[NCCS_OFFSET + index]
    }

    fn canonical(&self) -> bool {
        (self.lflag() & ICANON) != 0
    }

    fn echo(&self) -> bool {
        (self.lflag() & ECHO) != 0
    }

    fn echoe(&self) -> bool {
        (self.lflag() & ECHOE) != 0
    }

    fn echok(&self) -> bool {
        (self.lflag() & ECHOK) != 0
    }

    fn isig(&self) -> bool {
        (self.lflag() & ISIG) != 0
    }

    fn icrnl(&self) -> bool {
        (self.iflag() & ICRNL) != 0
    }

    fn ixon(&self) -> bool {
        (self.iflag() & IXON) != 0
    }

    fn opost_onlcr(&self) -> bool {
        (self.oflag() & (OPOST | ONLCR)) == (OPOST | ONLCR)
    }

    fn vintr(&self) -> u8 {
        self.cc(NCCS_VINTR)
    }

    fn vquit(&self) -> u8 {
        self.cc(NCCS_VQUIT)
    }

    fn verase(&self) -> u8 {
        self.cc(NCCS_VERASE)
    }

    fn vkill(&self) -> u8 {
        self.cc(NCCS_VKILL)
    }

    fn veof(&self) -> u8 {
        self.cc(NCCS_VEOF)
    }

    fn vtime(&self) -> u8 {
        self.cc(NCCS_VTIME)
    }

    fn vmin(&self) -> u8 {
        self.cc(NCCS_VMIN)
    }

    fn vsusp(&self) -> u8 {
        self.cc(NCCS_VSUSP)
    }

    fn signal_for_input(&self, ch: u8) -> Option<sched::SignalNumber> {
        if !self.isig() || ch == 0 {
            return None;
        }
        if ch == self.vintr() {
            Some(sched::SignalNumber::SIGINT)
        } else if ch == self.vquit() {
            Some(sched::SignalNumber::SIGQUIT)
        } else if ch == self.vsusp() {
            Some(sched::SignalNumber::SIGTSTP)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy)]
struct LinuxWinSize {
    rows: u16,
    cols: u16,
    xpixel: u16,
    ypixel: u16,
}

impl LinuxWinSize {
    const fn default_console() -> Self {
        Self {
            rows: 25,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        }
    }

    fn from_bytes(raw: [u8; LINUX_WINSIZE_LEN]) -> Self {
        Self {
            rows: u16::from_le_bytes([raw[0], raw[1]]),
            cols: u16::from_le_bytes([raw[2], raw[3]]),
            xpixel: u16::from_le_bytes([raw[4], raw[5]]),
            ypixel: u16::from_le_bytes([raw[6], raw[7]]),
        }
    }

    fn to_bytes(self) -> [u8; LINUX_WINSIZE_LEN] {
        let mut out = [0u8; LINUX_WINSIZE_LEN];
        out[0..2].copy_from_slice(&self.rows.to_le_bytes());
        out[2..4].copy_from_slice(&self.cols.to_le_bytes());
        out[4..6].copy_from_slice(&self.xpixel.to_le_bytes());
        out[6..8].copy_from_slice(&self.ypixel.to_le_bytes());
        out
    }
}

fn read_bytes_from_user(user: usize, dst: &mut [u8]) -> Result<(), Errno> {
    copy_from_user(user, dst).map_err(|e| e.as_errno())
}

fn write_bytes_to_user(user: usize, src: &[u8]) -> Result<(), Errno> {
    copy_to_user(user, src).map_err(|e| e.as_errno())
}

fn read_i32_from_user(user: usize) -> Result<i32, Errno> {
    let mut raw = [0u8; 4];
    read_bytes_from_user(user, &mut raw)?;
    Ok(i32::from_le_bytes(raw))
}

fn write_i32_to_user(user: usize, value: i32) -> Result<(), Errno> {
    write_bytes_to_user(user, &value.to_le_bytes())
}

fn write_u32_to_user(user: usize, value: u32) -> Result<(), Errno> {
    write_bytes_to_user(user, &value.to_le_bytes())
}

fn write_u64_to_user(user: usize, value: u64) -> Result<(), Errno> {
    write_bytes_to_user(user, &value.to_le_bytes())
}

fn write_usize_to_user(user: usize, value: usize) -> Result<(), Errno> {
    write_bytes_to_user(user, &value.to_le_bytes())
}

fn put_u32(out: &mut [u8], off: usize, value: u32) {
    out[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

#[derive(Default)]
struct TtyLineState {
    line: Vec<u8>,
    ready: VecDeque<u8>,
    eof_pending: bool,
}

impl TtyLineState {
    fn clear(&mut self) {
        self.line.clear();
        self.ready.clear();
        self.eof_pending = false;
    }
}

/// 一个底层 TTY 设备的共享行规程状态。
///
/// devtmpfs 可能把同一个串口同时投影成 `/dev/console` 和 `/dev/uart0`。
/// termios、窗口大小、前台进程组和规范模式行缓冲都属于控制终端本身，
/// 必须在这些节点和所有 open fd 之间共享；每个 fd 只保留自己的状态标志。
struct TtySharedState {
    termios: Spinlock<LinuxTermios>,
    winsize: Spinlock<LinuxWinSize>,
    foreground_pgrp: Spinlock<i32>,
    line_state: Spinlock<TtyLineState>,
}

impl TtySharedState {
    fn new() -> Self {
        Self {
            termios: Spinlock::new(LinuxTermios::new_default()),
            winsize: Spinlock::new(LinuxWinSize::default_console()),
            foreground_pgrp: Spinlock::new(0),
            line_state: Spinlock::new(TtyLineState::default()),
        }
    }
}

fn shared_tty_state(dev: &CharDevice) -> Option<Arc<TtySharedState>> {
    if !dev.is_tty() {
        return None;
    }

    let mut states = TTY_SHARED_STATES.lock();
    if let Some(state) = states.get(dev.fw_name()).and_then(Weak::upgrade) {
        return Some(state);
    }

    // 同一个底层 TTY 可能被投影成多个 `/dev` 节点，例如稳定的 console 别名
    // 和驱动自己的串口节点。行规程状态必须按设备共享，不能按 open fd 分裂。
    let state = Arc::new(TtySharedState::new());
    states.insert(String::from(dev.fw_name()), Arc::downgrade(&state));
    Some(state)
}

struct CharDevFileOps {
    dev: CharDevice,
    nonblock: AtomicBool,
    tty: Option<Arc<TtySharedState>>,
}

impl CharDevFileOps {
    fn new(dev: CharDevice, nonblock: bool, tty: Option<Arc<TtySharedState>>) -> Self {
        Self {
            dev,
            nonblock: AtomicBool::new(nonblock),
            tty,
        }
    }

    fn is_tty(&self) -> bool {
        self.tty.is_some() && self.dev.is_tty()
    }

    fn current_or_stored_pgrp(&self) -> Result<i32, Errno> {
        let stored = self
            .tty
            .as_deref()
            .map(|tty| *tty.foreground_pgrp.lock())
            .unwrap_or(0);
        if stored > 0 {
            Ok(stored)
        } else {
            operation::getpgid(0)
        }
    }

    fn control_done_ignore_unsupported(&self, req: CharControlRequest) -> Result<(), Errno> {
        match self.dev.control(req) {
            Ok(CharControlResponse::Done) | Err(ControlError::Unsupported) => Ok(()),
            Ok(_) => Err(Errno::EINVAL),
            Err(err) => Err(map_control_errno(err)),
        }
    }

    fn control_u32_or_zero(&self, req: CharControlRequest) -> Result<u32, Errno> {
        match self.dev.control(req) {
            Ok(CharControlResponse::U32(value)) => Ok(value),
            Ok(CharControlResponse::Done) => Err(Errno::EINVAL),
            Err(ControlError::Unsupported) => Ok(0),
            Err(err) => Err(map_control_errno(err)),
        }
    }

    fn sync_termios2_hardware(&self, raw: &[u8; LINUX_TERMIOS2_LEN]) -> Result<(), Errno> {
        let ospeed = u32::from_le_bytes(raw[40..44].try_into().unwrap());
        if ospeed == 0 {
            return Ok(());
        }
        self.control_done_ignore_unsupported(CharControlRequest::SetSerialConfig {
            baud: Some(ospeed),
        })
    }

    fn write_tty_bytes(&self, buf: &[u8], termios: LinuxTermios) -> VfsResult<()> {
        if buf.is_empty() {
            return Ok(());
        }
        if !termios.opost_onlcr() {
            return self.dev.write_all(buf).map_err(map_char_err);
        }

        let mut cooked = Vec::with_capacity(buf.len());
        for &byte in buf {
            if byte == b'\n' {
                cooked.push(b'\r');
                cooked.push(b'\n');
            } else {
                cooked.push(byte);
            }
        }
        self.dev.write_all(&cooked).map_err(map_char_err)
    }

    fn dequeue_ready(&self, tty: &TtySharedState, buf: &mut [u8]) -> Option<usize> {
        let mut state = tty.line_state.lock();
        if state.eof_pending {
            state.eof_pending = false;
            return Some(0);
        }
        if state.ready.is_empty() {
            return None;
        }
        let mut n = 0usize;
        while n < buf.len() {
            let Some(byte) = state.ready.pop_front() else {
                break;
            };
            buf[n] = byte;
            n += 1;
        }
        Some(n)
    }

    fn send_fg_signal(&self, sig: sched::SignalNumber) {
        let stored = self
            .tty
            .as_deref()
            .map(|tty| *tty.foreground_pgrp.lock())
            .unwrap_or(0);
        let current = operation::getpgid(0).ok().filter(|pgrp| *pgrp > 0);
        let primary = if stored > 0 { Some(stored) } else { current };

        if let Some(pgrp) = primary {
            // 前台进程组是 TTY 的内部对象关系，不应通过 kill(-PGID) 的
            // 用户态 pid 编码间接表达；PGID==1 会与特殊广播形式冲突。
            let _ = operation::kill_process_group(pgrp, Some(sig));
        }

        if stored > 0 {
            if let Some(current_pgrp) = current {
                if current_pgrp != stored {
                    // 当前作业控制还不完整：某些 shell 会把 TTY 前台组留在 shell 自己，
                    // 但前台程序已经在这个读路径里消费到 VINTR/VQUIT/VSUSP。补发给当前
                    // 读者进程组，避免 Ctrl-C 只打到 shell，真正阻塞的程序继续睡眠。
                    let _ = operation::kill_process_group(current_pgrp, Some(sig));
                }
            }
        }
    }

    fn echo_signal_char(&self, sig: sched::SignalNumber, termios: LinuxTermios) {
        if !termios.echo() {
            return;
        }
        let bytes = if sig == sched::SignalNumber::SIGINT {
            &b"^C\n"[..]
        } else if sig == sched::SignalNumber::SIGQUIT {
            &b"^\\\n"[..]
        } else if sig == sched::SignalNumber::SIGTSTP {
            &b"^Z\n"[..]
        } else {
            &b"\n"[..]
        };
        let _ = self.write_tty_bytes(bytes, termios);
    }

    fn handle_input_signal(
        &self,
        tty: &TtySharedState,
        ch: u8,
        termios: LinuxTermios,
    ) -> VfsResult<()> {
        let Some(sig) = termios.signal_for_input(ch) else {
            return Ok(());
        };
        self.send_fg_signal(sig);
        tty.line_state.lock().clear();
        self.echo_signal_char(sig, termios);
        Err(VfsError::Interrupted)
    }

    fn read_tty_canonical(
        &self,
        tty: &TtySharedState,
        buf: &mut [u8],
        termios: LinuxTermios,
    ) -> VfsResult<usize> {
        loop {
            if let Some(n) = self.dequeue_ready(tty, buf) {
                return Ok(n);
            }

            let mut byte = [0u8; 1];
            let n = self.dev.read(&mut byte).map_err(map_char_err)?;
            if n == 0 {
                return Err(VfsError::WouldBlock);
            }

            let mut ch = byte[0];
            if termios.icrnl() && ch == b'\r' {
                ch = b'\n';
            }
            if termios.ixon() && (ch == 17 || ch == 19) {
                continue;
            }
            self.handle_input_signal(tty, ch, termios)?;

            let mut echo_bytes: Option<Vec<u8>> = None;
            {
                let mut state = tty.line_state.lock();
                if ch == termios.verase() && ch != 0 {
                    if state.line.pop().is_some() && termios.echo() {
                        echo_bytes = Some(if termios.echoe() {
                            Vec::from(&b"\x08 \x08"[..])
                        } else {
                            Vec::from(&[ch][..])
                        });
                    }
                } else if ch == termios.vkill() && ch != 0 {
                    let erased = state.line.len();
                    state.line.clear();
                    if erased != 0 && termios.echo() {
                        let mut out = Vec::new();
                        if termios.echoe() {
                            out.reserve(erased * 3);
                            for _ in 0..erased {
                                out.extend_from_slice(b"\x08 \x08");
                            }
                        }
                        if termios.echok() {
                            out.push(b'\n');
                        }
                        if !out.is_empty() {
                            echo_bytes = Some(out);
                        }
                    }
                } else if ch == termios.veof() && ch != 0 {
                    if state.line.is_empty() {
                        state.eof_pending = true;
                    } else {
                        while let Some(byte) = state.line.first().copied() {
                            state.ready.push_back(byte);
                            state.line.remove(0);
                        }
                    }
                } else {
                    state.line.push(ch);
                    if termios.echo() {
                        echo_bytes = Some(Vec::from(&[ch][..]));
                    }
                    if ch == b'\n' {
                        while let Some(byte) = state.line.first().copied() {
                            state.ready.push_back(byte);
                            state.line.remove(0);
                        }
                    }
                }
            }

            if let Some(bytes) = echo_bytes.as_deref() {
                let _ = self.write_tty_bytes(bytes, termios);
            }
        }
    }

    fn read_tty_raw(
        &self,
        tty: &TtySharedState,
        buf: &mut [u8],
        termios: LinuxTermios,
    ) -> VfsResult<usize> {
        let want = termios.vmin().max(1) as usize;
        let mut filled = 0usize;
        loop {
            let n = self.dev.read(&mut buf[filled..]).map_err(map_char_err)?;
            if n != 0 {
                let start = filled;
                filled += n;
                if termios.icrnl() {
                    for byte in &mut buf[start..filled] {
                        if *byte == b'\r' {
                            *byte = b'\n';
                        }
                    }
                }
                for idx in start..filled {
                    self.handle_input_signal(tty, buf[idx], termios)?;
                }
                if termios.echo() {
                    let _ = self.write_tty_bytes(&buf[start..filled], termios);
                }
                if filled >= want || filled == buf.len() {
                    return Ok(filled);
                }
            } else {
                if filled != 0 && termios.vtime() == 0 {
                    return Ok(filled);
                }
                return Err(VfsError::WouldBlock);
            }
        }
    }
}

impl FileOps for CharDevFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if buf.is_empty() || self.nonblock.load(Ordering::Acquire) || !self.is_tty() {
            return self.dev.read(buf).map_err(map_char_err);
        }
        let Some(tty) = self.tty.as_deref() else {
            return self.dev.read(buf).map_err(map_char_err);
        };
        let termios = *tty.termios.lock();
        if termios.canonical() {
            self.read_tty_canonical(tty, buf, termios)
        } else {
            self.read_tty_raw(tty, buf, termios)
        }
    }
    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        if !self.is_tty() || self.nonblock.load(Ordering::Acquire) {
            return self.dev.write(buf).map_err(map_char_err);
        }
        let Some(tty) = self.tty.as_deref() else {
            return self.dev.write(buf).map_err(map_char_err);
        };
        let termios = *tty.termios.lock();
        self.write_tty_bytes(buf, termios)?;
        Ok(buf.len())
    }
    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> {
        self.dev.flush().map_err(map_char_err)
    }
    fn poll(&self, _interest: PollEvents) -> PollEvents {
        if !self.dev.is_active() {
            return PollEvents::POLLERR.with(PollEvents::POLLHUP);
        }
        if let Some(tty) = self.tty.as_deref() {
            let line_readable = {
                let state = tty.line_state.lock();
                state.eof_pending || !state.ready.is_empty()
            };
            // 规范模式同样要暴露底层 FIFO 的“有字节可取”状态。阻塞 read()
            // 在无完整行时会先返回 WouldBlock，再由 syscall 层按 poll() 等待；
            // 若这里只看行缓冲，UART 字节永远不会被重新拉进行规程，shell 会像
            // 串口输入失效一样卡住。
            let dev_readable = self.dev.poll_read();
            let readable = line_readable || dev_readable;
            return if readable {
                PollEvents::POLLIN.with(PollEvents::POLLOUT)
            } else {
                PollEvents::POLLOUT
            };
        }
        devtmpfs_compat_char_poll_default()
    }

    fn poll_add_waiter(&self, task: &Arc<sched::Task>, interest: PollEvents) -> bool {
        let want_read = interest.has(PollEvents::POLLIN) || interest.has(PollEvents::POLLPRI);
        let want_write = interest.has(PollEvents::POLLOUT);
        if !want_read && !want_write {
            return false;
        }
        self.dev.poll_add_waiter(task, want_read, want_write)
    }

    fn poll_remove_waiter(&self, task: &Arc<sched::Task>) {
        self.dev.poll_remove_waiter(task);
    }

    fn set_status_flags(&self, flags: OpenOptions) {
        self.nonblock.store(flags.nonblock, Ordering::Release);
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        if !self.dev.is_active() {
            return Err(Errno::ENODEV);
        }
        let Some(tty) = self.tty.as_deref() else {
            return Err(Errno::ENOTTY);
        };

        match cmd.raw() {
            TCGETS => {
                let termios = *tty.termios.lock();
                write_bytes_to_user(arg, &termios.raw)?;
                Ok(0)
            }
            TCGETS2 => {
                let termios = *tty.termios.lock();
                write_bytes_to_user(arg, &termios.as_termios2_bytes())?;
                Ok(0)
            }
            TCSETS | TCSETSW | TCSETSF => {
                let mut raw = [0u8; LINUX_TERMIOS_LEN];
                read_bytes_from_user(arg, &mut raw)?;
                *tty.termios.lock() = LinuxTermios { raw };
                if matches!(cmd.raw(), TCSETSW | TCSETSF) {
                    self.control_done_ignore_unsupported(CharControlRequest::DrainTx)?;
                }
                if matches!(cmd.raw(), TCSETSF) {
                    tty.line_state.lock().clear();
                    self.control_done_ignore_unsupported(CharControlRequest::FlushRx)?;
                }
                Ok(0)
            }
            TCSETS2 | TCSETSW2 | TCSETSF2 => {
                let mut raw = [0u8; LINUX_TERMIOS2_LEN];
                read_bytes_from_user(arg, &mut raw)?;
                let mut termios = [0u8; LINUX_TERMIOS_LEN];
                termios.copy_from_slice(&raw[..LINUX_TERMIOS_LEN]);
                *tty.termios.lock() = LinuxTermios { raw: termios };
                self.sync_termios2_hardware(&raw)?;
                if matches!(cmd.raw(), TCSETSW2 | TCSETSF2) {
                    self.control_done_ignore_unsupported(CharControlRequest::DrainTx)?;
                }
                if matches!(cmd.raw(), TCSETSF2) {
                    tty.line_state.lock().clear();
                    self.control_done_ignore_unsupported(CharControlRequest::FlushRx)?;
                }
                Ok(0)
            }
            TIOCGWINSZ => {
                let winsize = *tty.winsize.lock();
                write_bytes_to_user(arg, &winsize.to_bytes())?;
                Ok(0)
            }
            TIOCSWINSZ => {
                let mut raw = [0u8; LINUX_WINSIZE_LEN];
                read_bytes_from_user(arg, &mut raw)?;
                *tty.winsize.lock() = LinuxWinSize::from_bytes(raw);
                Ok(0)
            }
            FIONREAD => {
                let queued = self.control_u32_or_zero(CharControlRequest::GetInputQueueLen)?;
                write_u32_to_user(arg, queued)?;
                Ok(0)
            }
            TIOCOUTQ => {
                let queued = self.control_u32_or_zero(CharControlRequest::GetOutputQueueLen)?;
                write_u32_to_user(arg, queued)?;
                Ok(0)
            }
            TIOCGPGRP => {
                write_i32_to_user(arg, self.current_or_stored_pgrp()?)?;
                Ok(0)
            }
            TIOCSPGRP => {
                let pgid = read_i32_from_user(arg)?;
                if pgid <= 0 {
                    return Err(Errno::EINVAL);
                }
                *tty.foreground_pgrp.lock() = pgid;
                Ok(0)
            }
            TIOCGSID => {
                write_i32_to_user(arg, operation::getsid(0)?)?;
                Ok(0)
            }
            TIOCGETD => {
                write_i32_to_user(arg, 0)?;
                Ok(0)
            }
            TIOCSETD => {
                let discipline = read_i32_from_user(arg)?;
                if discipline == 0 {
                    Ok(0)
                } else {
                    Err(Errno::EINVAL)
                }
            }
            TCSBRK => {
                self.control_done_ignore_unsupported(CharControlRequest::DrainTx)?;
                if arg == 0 {
                    self.control_done_ignore_unsupported(CharControlRequest::SendBreak {
                        duration_ms: TTY_DEFAULT_BREAK_MS,
                    })?;
                }
                Ok(0)
            }
            TCSBRKP => {
                self.control_done_ignore_unsupported(CharControlRequest::DrainTx)?;
                let units = u32::try_from(arg).unwrap_or(u32::MAX);
                let duration_ms = if units == 0 {
                    TTY_DEFAULT_BREAK_MS
                } else {
                    units.saturating_mul(TTY_BREAK_UNIT_MS)
                };
                self.control_done_ignore_unsupported(CharControlRequest::SendBreak {
                    duration_ms,
                })?;
                Ok(0)
            }
            TCFLSH => match arg {
                TCIFLUSH => {
                    tty.line_state.lock().clear();
                    self.control_done_ignore_unsupported(CharControlRequest::FlushRx)?;
                    Ok(0)
                }
                TCOFLUSH => {
                    self.control_done_ignore_unsupported(CharControlRequest::FlushTx)?;
                    Ok(0)
                }
                TCIOFLUSH => {
                    tty.line_state.lock().clear();
                    self.control_done_ignore_unsupported(CharControlRequest::FlushBoth)?;
                    Ok(0)
                }
                _ => Err(Errno::EINVAL),
            },
            TCXONC | TIOCEXCL | TIOCNXCL | TIOCSCTTY | TIOCNOTTY => Ok(0),
            _ => Err(Errno::ENOTTY),
        }
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── 字符设备 InodeOps ─────────

/// 字符设备节点的操作对象。
///
/// 直接持有 `CharDev` 句柄。句柄内部共享 active/gone 状态，设备解绑后旧 inode
/// 和已打开 fd 都会通过同一状态停止访问底层驱动。
struct DevCharOps {
    dev: CharDevice,
    tty: Option<Arc<TtySharedState>>,
}

impl DevCharOps {
    fn dev(&self) -> CharDevice {
        self.dev.clone()
    }
}

impl InodeOps for DevCharOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        if !self.dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        Ok(Box::new(CharDevFileOps::new(
            self.dev.clone(),
            opts.nonblock,
            self.tty.clone(),
        )))
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── 块设备 InodeOps ─────────

/// 块设备节点的操作对象。
///
/// 持有 `Arc<BlockDev>`，`open()` 时同样无需查找。
struct DevBlockOps {
    dev: Arc<BlockDevice>,
}

struct BlockDevFileOps {
    dev: Arc<BlockDevice>,
    sync_writes: bool,
    direct: bool,
}

fn map_bio_err(err: BioError) -> VfsError {
    match err {
        BioError::Submit(s) => match s {
            crate::dev::bio::SubmitError::Unsupported => VfsError::NotSupported,
            crate::dev::bio::SubmitError::ReadOnly => VfsError::ReadOnlyFilesystem,
            crate::dev::bio::SubmitError::QueueFull => VfsError::WouldBlock,
            crate::dev::bio::SubmitError::DeviceGone => VfsError::NoDevice,
            crate::dev::bio::SubmitError::OutOfMemory => VfsError::OutOfMemory,
            crate::dev::bio::SubmitError::InvalidRequest(_) => VfsError::InvalidArgument,
        },
        BioError::Io(i) => match i {
            crate::dev::bio::BioIoError::MediaError => VfsError::Io,
            crate::dev::bio::BioIoError::Unavailable => VfsError::NoDevice,
            crate::dev::bio::BioIoError::Timeout => VfsError::TimedOut,
            crate::dev::bio::BioIoError::ReadOnly => VfsError::ReadOnlyFilesystem,
            crate::dev::bio::BioIoError::Unsupported => VfsError::NotSupported,
        },
    }
}

fn boxed_zeroed(len: usize) -> VfsResult<Box<[u8]>> {
    let mut data = Vec::new();
    data.try_reserve(len).map_err(|_| VfsError::OutOfMemory)?;
    data.resize(len, 0);
    Ok(data.into_boxed_slice())
}

fn boxed_copy(buf: &[u8]) -> VfsResult<Box<[u8]>> {
    let mut data = Vec::new();
    data.try_reserve(buf.len())
        .map_err(|_| VfsError::OutOfMemory)?;
    data.extend_from_slice(buf);
    Ok(data.into_boxed_slice())
}

fn block_range_for_io(dev: &BlockDevice, offset: u64, len: usize) -> VfsResult<Option<BlockRange>> {
    if len == 0 {
        return Ok(None);
    }
    if offset == u64::MAX {
        return Err(VfsError::InvalidArgument);
    }
    let block_size = dev.geometry().logical_block_size().get() as u64;
    let len_u64 = u64::try_from(len).map_err(|_| VfsError::InvalidArgument)?;
    if !offset.is_multiple_of(block_size) || !len_u64.is_multiple_of(block_size) {
        return Err(VfsError::InvalidArgument);
    }
    let blocks = len_u64 / block_size;
    let blocks = u32::try_from(blocks).map_err(|_| VfsError::InvalidArgument)?;
    Ok(Some(BlockRange {
        lba: offset / block_size,
        blocks,
    }))
}

fn block_capacity_remaining(dev: &BlockDevice, offset: u64, len: usize) -> usize {
    let Some(capacity) = dev.geometry().capacity_bytes() else {
        return len;
    };
    if offset >= capacity {
        return 0;
    }
    let remaining = capacity - offset;
    len.min(remaining as usize)
}

fn max_blocks_per_io(dev: &BlockDevice) -> u32 {
    dev.limits()
        .max_blocks_per_io()
        .map(|n| n.get())
        .unwrap_or_else(devtmpfs_compat_max_blocks_per_io)
        .max(1)
}

fn block_io_hints(dev: &Arc<BlockDevice>) -> Result<BlockIoHints, Errno> {
    match dev
        .control(BlockControlRequest::GetIoHints)
        .map_err(map_control_errno)?
    {
        BlockControlResponse::IoHints(hints) => Ok(hints),
        _ => Err(Errno::EINVAL),
    }
}

fn block_read_exact(dev: &Arc<BlockDevice>, lba: u64, blocks: u32) -> VfsResult<Box<[u8]>> {
    let block_size = dev.geometry().logical_block_size().get() as usize;
    let len = (blocks as usize)
        .checked_mul(block_size)
        .ok_or(VfsError::InvalidArgument)?;
    let owned = boxed_zeroed(len)?;
    let bio = dev
        .submit_bio_wait(
            BioOp::Read,
            BlockRange { lba, blocks },
            BioBuffer::Owned(owned),
        )
        .map_err(map_bio_err)?;
    match bio.buffer {
        BioBuffer::Owned(buf) => Ok(buf),
        BioBuffer::None => Err(VfsError::Io),
    }
}

fn block_write_exact(dev: &Arc<BlockDevice>, lba: u64, data: Box<[u8]>) -> VfsResult<()> {
    let block_size = dev.geometry().logical_block_size().get() as usize;
    if data.len() == 0 || !data.len().is_multiple_of(block_size) {
        return Err(VfsError::InvalidArgument);
    }
    let blocks = u32::try_from(data.len() / block_size).map_err(|_| VfsError::InvalidArgument)?;
    dev.submit_bio_wait(
        BioOp::Write,
        BlockRange { lba, blocks },
        BioBuffer::Owned(data),
    )
    .map_err(map_bio_err)?;
    Ok(())
}

fn flush_if_supported(dev: &Arc<BlockDevice>) -> VfsResult<()> {
    if !dev.features().contains(BlockFeatures::FLUSH) {
        return Ok(());
    }
    dev.submit_bio_wait(
        BioOp::Flush,
        BlockRange { lba: 0, blocks: 0 },
        BioBuffer::None,
    )
    .map_err(map_bio_err)?;
    Ok(())
}

impl FileOps for BlockDevFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let len = block_capacity_remaining(&self.dev, offset, buf.len());
        if len == 0 {
            return Ok(0);
        }
        let block_size = self.dev.geometry().logical_block_size().get() as usize;
        let mut done = 0usize;

        if self.direct {
            let Some(range) = block_range_for_io(&self.dev, offset, len)? else {
                return Ok(0);
            };
            let mut lba = range.lba;
            let mut remaining_blocks = range.blocks as usize;
            let max_blocks = max_blocks_per_io(&self.dev) as usize;
            while remaining_blocks != 0 {
                let blocks = remaining_blocks.min(max_blocks).min(u32::MAX as usize) as u32;
                let data = block_read_exact(&self.dev, lba, blocks)?;
                let bytes = data.len();
                buf[done..done + bytes].copy_from_slice(&data);
                done += bytes;
                lba += blocks as u64;
                remaining_blocks -= blocks as usize;
            }
            return Ok(done);
        }

        if offset.is_multiple_of(block_size as u64) && len.is_multiple_of(block_size) {
            let mut lba = offset / block_size as u64;
            let mut remaining_blocks = len / block_size;
            let max_blocks = max_blocks_per_io(&self.dev) as usize;
            while remaining_blocks != 0 {
                let blocks = remaining_blocks.min(max_blocks).min(u32::MAX as usize) as u32;
                let data = block_read_exact(&self.dev, lba, blocks)?;
                let bytes = data.len();
                buf[done..done + bytes].copy_from_slice(&data);
                done += bytes;
                lba += blocks as u64;
                remaining_blocks -= blocks as usize;
            }
            return Ok(done);
        }

        while done < len {
            let abs = offset.saturating_add(done as u64);
            let lba = abs / block_size as u64;
            let in_block = (abs % block_size as u64) as usize;
            let take = (block_size - in_block).min(len - done);
            let data = block_read_exact(&self.dev, lba, 1)?;
            buf[done..done + take].copy_from_slice(&data[in_block..in_block + take]);
            done += take;
        }
        Ok(done)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let len = block_capacity_remaining(&self.dev, offset, buf.len());
        if len == 0 {
            return Ok(0);
        }
        let block_size = self.dev.geometry().logical_block_size().get() as usize;
        let mut done = 0usize;

        if self.direct {
            let Some(range) = block_range_for_io(&self.dev, offset, len)? else {
                return Ok(0);
            };
            let mut lba = range.lba;
            let mut remaining_blocks = range.blocks as usize;
            let max_blocks = max_blocks_per_io(&self.dev) as usize;
            while remaining_blocks != 0 {
                let blocks = remaining_blocks.min(max_blocks).min(u32::MAX as usize) as u32;
                let bytes = blocks as usize * block_size;
                let owned = boxed_copy(&buf[done..done + bytes])?;
                block_write_exact(&self.dev, lba, owned)?;
                done += bytes;
                lba += blocks as u64;
                remaining_blocks -= blocks as usize;
            }
        } else if offset.is_multiple_of(block_size as u64) && len.is_multiple_of(block_size) {
            let mut lba = offset / block_size as u64;
            let mut remaining_blocks = len / block_size;
            let max_blocks = max_blocks_per_io(&self.dev) as usize;
            while remaining_blocks != 0 {
                let blocks = remaining_blocks.min(max_blocks).min(u32::MAX as usize) as u32;
                let bytes = blocks as usize * block_size;
                let owned = boxed_copy(&buf[done..done + bytes])?;
                block_write_exact(&self.dev, lba, owned)?;
                done += bytes;
                lba += blocks as u64;
                remaining_blocks -= blocks as usize;
            }
        } else {
            while done < len {
                let abs = offset.saturating_add(done as u64);
                let lba = abs / block_size as u64;
                let in_block = (abs % block_size as u64) as usize;
                let take = (block_size - in_block).min(len - done);
                let mut data = block_read_exact(&self.dev, lba, 1)?;
                data[in_block..in_block + take].copy_from_slice(&buf[done..done + take]);
                block_write_exact(&self.dev, lba, data)?;
                done += take;
            }
        }
        if self.sync_writes {
            flush_if_supported(&self.dev)?;
        }
        Ok(done)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        if !self.dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        flush_if_supported(&self.dev)?;
        Ok(())
    }

    fn poll(&self, _interest: PollEvents) -> PollEvents {
        if !self.dev.is_active() {
            return PollEvents::POLLERR.with(PollEvents::POLLHUP);
        }
        PollEvents::POLLIN.with(PollEvents::POLLOUT)
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        if !self.dev.is_active() {
            return Err(Errno::ENODEV);
        }

        match cmd.raw() {
            BLKROGET => {
                let readonly = match self
                    .dev
                    .control(BlockControlRequest::GetReadOnly)
                    .map_err(map_control_errno)?
                {
                    BlockControlResponse::Bool(value) => i32::from(value),
                    _ => return Err(Errno::EINVAL),
                };
                write_i32_to_user(arg, readonly)?;
                Ok(0)
            }
            BLKGETSIZE => {
                let bytes = match self
                    .dev
                    .control(BlockControlRequest::GetCapacityBytes)
                    .map_err(map_control_errno)?
                {
                    BlockControlResponse::U64(value) => value,
                    _ => return Err(Errno::EINVAL),
                };
                let sectors = usize::try_from(bytes / 512).map_err(|_| Errno::EINVAL)?;
                write_usize_to_user(arg, sectors)?;
                Ok(0)
            }
            BLKGETSIZE64 => {
                let bytes = match self
                    .dev
                    .control(BlockControlRequest::GetCapacityBytes)
                    .map_err(map_control_errno)?
                {
                    BlockControlResponse::U64(value) => value,
                    _ => return Err(Errno::EINVAL),
                };
                write_u64_to_user(arg, bytes)?;
                Ok(0)
            }
            BLKSSZGET => {
                let block_size = match self
                    .dev
                    .control(BlockControlRequest::GetLogicalBlockSize)
                    .map_err(map_control_errno)?
                {
                    BlockControlResponse::U32(value) => value,
                    _ => return Err(Errno::EINVAL),
                };
                write_u32_to_user(arg, block_size)?;
                Ok(0)
            }
            BLKBSZGET => {
                let block_size = match self
                    .dev
                    .control(BlockControlRequest::GetLogicalBlockSize)
                    .map_err(map_control_errno)?
                {
                    BlockControlResponse::U32(value) => value,
                    _ => return Err(Errno::EINVAL),
                };
                write_usize_to_user(arg, block_size as usize)?;
                Ok(0)
            }
            BLKPBSZGET => {
                let block_size = match self
                    .dev
                    .control(BlockControlRequest::GetPhysicalBlockSize)
                    .map_err(map_control_errno)?
                {
                    BlockControlResponse::U32(value) => value,
                    _ => return Err(Errno::EINVAL),
                };
                write_u32_to_user(arg, block_size)?;
                Ok(0)
            }
            BLKIOMIN => {
                let hints = block_io_hints(&self.dev)?;
                write_u32_to_user(arg, hints.min_io_size)?;
                Ok(0)
            }
            BLKIOOPT => {
                let hints = block_io_hints(&self.dev)?;
                write_u32_to_user(arg, hints.optimal_io_size)?;
                Ok(0)
            }
            BLKALIGNOFF => {
                let hints = block_io_hints(&self.dev)?;
                write_i32_to_user(arg, hints.alignment_offset)?;
                Ok(0)
            }
            BLKDISCARDZEROES => {
                let hints = block_io_hints(&self.dev)?;
                write_i32_to_user(arg, i32::from(hints.discard_zeroes))?;
                Ok(0)
            }
            BLKROTATIONAL => {
                let hints = block_io_hints(&self.dev)?;
                write_i32_to_user(arg, i32::from(hints.rotational))?;
                Ok(0)
            }
            BLKGETDISKSEQ => {
                let diskseq = match self
                    .dev
                    .control(BlockControlRequest::GetDiskSeq)
                    .map_err(map_control_errno)?
                {
                    BlockControlResponse::U64(value) => value,
                    _ => return Err(Errno::EINVAL),
                };
                write_u64_to_user(arg, diskseq)?;
                Ok(0)
            }
            BLKFLSBUF => {
                self.dev
                    .control(BlockControlRequest::Flush)
                    .map_err(map_control_errno)?;
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl InodeOps for DevBlockOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        if !self.dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        Ok(Box::new(BlockDevFileOps {
            dev: Arc::clone(&self.dev),
            sync_writes: opts.sync,
            direct: opts.direct,
        }))
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// 从 devtmpfs 块设备 inode 中恢复底层块设备对象。
///
/// 这是给 blockfs 挂载源解析使用的窄接口：调用方仍然通过 VFS 解析路径和符号链接，
/// 只有最终确认 inode 属于 devtmpfs 块设备节点后，才取出其内联保存的设备对象。
pub fn block_device_from_inode(inode: &Inode) -> Option<Arc<BlockDevice>> {
    if inode.kind() != FileType::BlockDevice {
        return None;
    }
    let ops = inode.downcast_ops::<DevBlockOps>()?;
    let dev = Arc::clone(&ops.dev);
    dev.is_active().then_some(dev)
}

// ───────── 符号链接 InodeOps ─────────

/// devtmpfs 符号链接节点的操作对象。
///
/// 只保存链接目标文本。相对目标由 VFS path walker 按“链接所在目录”继续解析。
struct DevSymlinkOps {
    target: String,
}

impl InodeOps for DevSymlinkOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn readlink(&self, inode: &Inode) -> VfsResult<String> {
        if inode.kind() != FileType::Symlink {
            return Err(VfsError::InvalidArgument);
        }
        Ok(self.target.clone())
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Err(VfsError::InvalidArgument)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── 目录 InodeOps ─────────

/// devtmpfs 目录操作对象。
///
/// 每个目录只维护本级 `name → Arc<Inode>` 映射。设备节点的批量增删通过
/// [`DevTmpfsSuperblockOps`] 对外暴露，普通符号链接和目录创建也走 VFS 标准入口。
pub struct DevDirOps {
    pub(crate) children: Spinlock<BTreeMap<String, Arc<Inode>>>,
}

impl DevDirOps {
    fn new() -> Self {
        Self {
            children: Spinlock::new(BTreeMap::new()),
        }
    }

    /// 返回当前子节点的快照：`(user_name, Arc<Inode>)` 列表。
    pub fn children_snapshot(&self) -> alloc::vec::Vec<(String, Arc<Inode>)> {
        self.children
            .lock()
            .iter()
            .map(|(name, inode)| (name.clone(), Arc::clone(inode)))
            .collect()
    }
}

impl InodeOps for DevDirOps {
    fn lookup(&self, _inode: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        self.children
            .lock()
            .get(name)
            .cloned()
            .ok_or(VfsError::NotFound)
    }

    fn mkdir(
        &self,
        dir: &Inode,
        name: &str,
        mode: FileMode,
        cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        validate_devtmpfs_component(name)?;

        let sb = dir.superblock().ok_or(VfsError::InvalidArgument)?;
        let sb_ops = sb
            .downcast_ops::<DevTmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
        let inode = sb_ops.new_dir_inode(mode, cred.euid, cred.egid)?;

        let mut children = self.children.lock();
        if children.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        children.insert(String::from(name), Arc::clone(&inode));
        drop(children);

        dir.inc_nlink();
        dir.touch_mtime();
        dir.touch_ctime();
        Ok(inode)
    }

    fn symlink(
        &self,
        dir: &Inode,
        name: &str,
        target: &str,
        cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        validate_devtmpfs_component(name)?;
        validate_symlink_target(target)?;

        let sb = dir.superblock().ok_or(VfsError::InvalidArgument)?;
        let sb_ops = sb
            .downcast_ops::<DevTmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
        let inode = sb_ops.new_symlink_inode(target, cred.euid, cred.egid)?;

        let mut children = self.children.lock();
        if children.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        children.insert(String::from(name), Arc::clone(&inode));
        drop(children);

        dir.touch_mtime();
        dir.touch_ctime();
        Ok(inode)
    }

    fn rmdir(&self, dir: &Inode, name: &str, child: &Inode) -> VfsResult<()> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        if child.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }

        let child_ops = child
            .downcast_ops::<DevDirOps>()
            .ok_or(VfsError::InvalidArgument)?;
        if !child_ops.children.lock().is_empty() {
            return Err(VfsError::DirectoryNotEmpty);
        }

        let mut children = self.children.lock();
        let existing = children.get(name).ok_or(VfsError::NotFound)?;
        if existing.fs_id() != child.fs_id() || existing.ino() != child.ino() {
            return Err(VfsError::NotFound);
        }
        let removed = children.remove(name).ok_or(VfsError::NotFound)?;
        drop(children);

        dir.dec_nlink();
        dir.touch_mtime();
        dir.touch_ctime();
        removed.set_nlink(0);
        removed.touch_ctime();
        Ok(())
    }

    fn unlink(&self, dir: &Inode, name: &str, child: &Inode) -> VfsResult<()> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        if child.kind() == FileType::Directory {
            return Err(VfsError::IsADirectory);
        }
        if child.kind() != FileType::Symlink {
            return Err(VfsError::OperationNotPermitted);
        }

        let mut children = self.children.lock();
        let existing = children.get(name).ok_or(VfsError::NotFound)?;
        if existing.fs_id() != child.fs_id() || existing.ino() != child.ino() {
            return Err(VfsError::NotFound);
        }
        let removed = children.remove(name).ok_or(VfsError::NotFound)?;
        drop(children);

        dir.touch_mtime();
        dir.touch_ctime();
        removed.set_nlink(0);
        removed.touch_ctime();
        Ok(())
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(DevRootFile {
            snapshot: self
                .children
                .lock()
                .iter()
                .map(|(name, inode)| DirEntry {
                    ino: inode.ino(),
                    name: SmallStr::new(name),
                    kind: inode.kind(),
                })
                .collect(),
        }))
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── 根目录 FileOps ─────────

struct DevRootFile {
    snapshot: alloc::vec::Vec<DirEntry>,
}

impl FileOps for DevRootFile {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }

    fn readdir(
        &self,
        pos: u64,
        sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        let start = pos as usize;
        for (i, entry) in self.snapshot.iter().enumerate().skip(start) {
            if sink(entry.clone()).is_break() {
                return Ok(i as u64);
            }
        }
        Ok(self.snapshot.len() as u64)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, _interest: PollEvents) -> PollEvents {
        PollEvents(0)
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── SuperblockOps ─────────

/// devtmpfs 超级块操作对象。
///
/// 同时提供公开的 `bind_char` / `bind_block` / `bind_symlink` / `unbind` API，
/// 让设备驱动或兼容层在设备注册/注销时同步更新 `/dev` 下的节点。
pub struct DevTmpfsSuperblockOps {
    next_ino: AtomicU64,
    /// 超级块弱引用，创建 Inode 时需要
    sb: vfs::sync::Spinlock<Option<alloc::sync::Weak<Superblock>>>,
}

impl DevTmpfsSuperblockOps {
    fn alloc_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    fn fs_id(&self) -> Option<FsId> {
        self.sb.lock().as_ref()?.upgrade().map(|sb| sb.fs_id)
    }

    fn sb_weak(&self) -> Option<alloc::sync::Weak<Superblock>> {
        self.sb.lock().clone()
    }

    fn root_inode(&self) -> VfsResult<Arc<Inode>> {
        self.sb
            .lock()
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .map(|sb| Arc::clone(&sb.root_inode))
            .ok_or(VfsError::InvalidArgument)
    }

    fn invalidate_path_dcache(&self, path: &str) {
        let Some(sb) = self.sb.lock().as_ref().and_then(|weak| weak.upgrade()) else {
            return;
        };

        let mut parent = Arc::clone(&sb.root_dentry);
        let mut components = path.split('/').peekable();
        while let Some(component) = components.next() {
            let Some(dentry) = vfs::DCACHE.get(&parent, component) else {
                return;
            };
            if components.peek().is_none() {
                vfs::DCACHE.invalidate_dentry(&dentry);
                dentry.invalidate();
                return;
            }
            if !dentry.is_positive() {
                return;
            }
            parent = dentry;
        }
    }

    fn new_dir_inode(&self, mode: FileMode, uid: Uid, gid: Gid) -> VfsResult<Arc<Inode>> {
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;

        let now = Timespec::now();
        let meta = InodeMeta {
            size: 0,
            nlink: 2,
            mode,
            uid,
            gid,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        Ok(Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            FileType::Directory,
            DevId::new(0, 0),
            devtmpfs_compat_block_size(),
            None,
            meta,
            Arc::new(DevDirOps::new()),
            sb_weak,
        ))
    }

    fn new_symlink_inode(&self, target: &str, uid: Uid, gid: Gid) -> VfsResult<Arc<Inode>> {
        validate_symlink_target(target)?;
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;

        let now = Timespec::now();
        let meta = InodeMeta {
            size: target.len() as u64,
            nlink: 1,
            mode: devtmpfs_compat_default_symlink_mode(),
            uid,
            gid,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        Ok(Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            FileType::Symlink,
            DevId::new(0, 0),
            devtmpfs_compat_block_size(),
            None,
            meta,
            Arc::new(DevSymlinkOps {
                target: String::from(target),
            }),
            sb_weak,
        ))
    }

    fn new_custom_inode(&self, spec: &CustomDevNodeSpec, rdev: DevId) -> VfsResult<Arc<Inode>> {
        split_devtmpfs_path(spec.name())?;
        if spec.block_size() == 0 || spec.nlink() == 0 {
            return Err(VfsError::InvalidArgument);
        }
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;

        let now = Timespec::now();
        let meta = InodeMeta {
            size: spec.size(),
            nlink: spec.nlink(),
            mode: spec.mode(),
            uid: spec.uid(),
            gid: spec.gid(),
            atime: now,
            mtime: now,
            ctime: now,
            blocks: spec.blocks(),
        };

        Ok(Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            spec.kind(),
            rdev,
            spec.block_size(),
            None,
            meta,
            spec.ops(),
            sb_weak,
        ))
    }

    fn ensure_parent_dir(&self, components: &[&str]) -> VfsResult<Arc<Inode>> {
        let mut dir_inode = self.root_inode()?;
        let mut current_path = String::new();

        for component in &components[..components.len().saturating_sub(1)] {
            if !current_path.is_empty() {
                current_path.push('/');
            }
            current_path.push_str(component);

            let dir_ops = dir_inode
                .downcast_ops::<DevDirOps>()
                .ok_or(VfsError::NotADirectory)?;
            let mut created = false;
            let next = {
                let mut children = dir_ops.children.lock();
                if let Some(existing) = children.get(*component).cloned() {
                    existing
                } else {
                    let child = self.new_dir_inode(
                        devtmpfs_compat_default_dir_mode(),
                        devtmpfs_compat_default_uid(),
                        devtmpfs_compat_default_gid(),
                    )?;
                    children.insert(String::from(*component), Arc::clone(&child));
                    created = true;
                    child
                }
            };

            if next.kind() != FileType::Directory {
                return Err(VfsError::NotADirectory);
            }
            if created {
                dir_inode.inc_nlink();
                dir_inode.touch_mtime();
                dir_inode.touch_ctime();
                self.invalidate_path_dcache(&current_path);
            }
            dir_inode = next;
        }

        Ok(dir_inode)
    }

    fn lookup_parent_dir(&self, components: &[&str]) -> VfsResult<Arc<Inode>> {
        let mut dir_inode = self.root_inode()?;
        for component in &components[..components.len().saturating_sub(1)] {
            let dir_ops = dir_inode
                .downcast_ops::<DevDirOps>()
                .ok_or(VfsError::NotADirectory)?;
            let next = dir_ops
                .children
                .lock()
                .get(*component)
                .cloned()
                .ok_or(VfsError::NotFound)?;
            if next.kind() != FileType::Directory {
                return Err(VfsError::NotADirectory);
            }
            dir_inode = next;
        }
        Ok(dir_inode)
    }

    fn insert_node_at(&self, path: &str, inode: Arc<Inode>) -> VfsResult<()> {
        let components = split_devtmpfs_path(path)?;
        let name = components
            .last()
            .copied()
            .ok_or(VfsError::InvalidArgument)?;
        let parent = self.ensure_parent_dir(&components)?;
        let parent_ops = parent
            .downcast_ops::<DevDirOps>()
            .ok_or(VfsError::NotADirectory)?;

        let mut children = parent_ops.children.lock();
        if children.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        children.insert(String::from(name), inode);
        drop(children);

        parent.touch_mtime();
        parent.touch_ctime();
        self.invalidate_path_dcache(path);
        Ok(())
    }

    fn remove_node_at(&self, path: &str) -> VfsResult<Arc<Inode>> {
        let components = split_devtmpfs_path(path)?;
        let name = components
            .last()
            .copied()
            .ok_or(VfsError::InvalidArgument)?;
        let parent = self.lookup_parent_dir(&components)?;
        let parent_ops = parent
            .downcast_ops::<DevDirOps>()
            .ok_or(VfsError::NotADirectory)?;

        let mut children = parent_ops.children.lock();
        let inode = children.remove(name).ok_or(VfsError::NotFound)?;
        drop(children);

        if inode.kind() == FileType::Directory {
            let dir_ops = inode
                .downcast_ops::<DevDirOps>()
                .ok_or(VfsError::InvalidArgument)?;
            if !dir_ops.children.lock().is_empty() {
                let mut children = parent_ops.children.lock();
                children.insert(String::from(name), Arc::clone(&inode));
                return Err(VfsError::DirectoryNotEmpty);
            }
            parent.dec_nlink();
        }

        parent.touch_mtime();
        parent.touch_ctime();
        inode.set_nlink(0);
        inode.touch_ctime();
        if let Some(sb) = inode.superblock() {
            sb.remove_inode(inode.ino());
        }
        self.invalidate_path_dcache(path);
        Ok(inode)
    }

    fn lookup_node_at(&self, path: &str) -> VfsResult<Arc<Inode>> {
        let components = split_devtmpfs_path(path)?;
        let name = components
            .last()
            .copied()
            .ok_or(VfsError::InvalidArgument)?;
        let parent = self.lookup_parent_dir(&components)?;
        let parent_ops = parent
            .downcast_ops::<DevDirOps>()
            .ok_or(VfsError::NotADirectory)?;
        parent_ops
            .children
            .lock()
            .get(name)
            .cloned()
            .ok_or(VfsError::NotFound)
    }

    /// 将字符设备绑定到 devtmpfs 相对路径。
    ///
    /// - `user_name`：用户空间可见的相对路径（如 `"console"` 或 `"tty/serial0"`）
    /// - `dev`：已注册的字符设备对象（直接存入 inode，不再保存名称）
    pub fn bind_char(&self, user_name: &str, dev: CharDevice) -> VfsResult<()> {
        split_devtmpfs_path(user_name)?;
        if !dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        if self.lookup_node_at(user_name).is_ok() {
            return Err(VfsError::AlreadyExists);
        }
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;
        let rdev = super::device_numbers::register_char(user_name, dev.fw_name())
            .ok_or(VfsError::NoSpace)?;

        let now = Timespec::now();
        let meta = InodeMeta {
            size: 0,
            nlink: 1,
            mode: devtmpfs_compat_default_device_mode(),
            uid: devtmpfs_compat_default_uid(),
            gid: devtmpfs_compat_default_gid(),
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        let tty = shared_tty_state(&dev);
        let ops = Arc::new(DevCharOps { dev, tty });
        let inode = Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            FileType::CharDevice,
            rdev,
            devtmpfs_compat_block_size(),
            None,
            meta,
            ops,
            sb_weak,
        );

        if let Err(err) = self.insert_node_at(user_name, inode) {
            super::device_numbers::unregister_node(user_name);
            return Err(err);
        }
        Ok(())
    }

    /// 绑定已经注册的静态节点。
    ///
    /// mount 时调用一次，用于把早于 devtmpfs 出现的非 PnP 节点批量投影到
    /// 当前 superblock。运行期后注册的静态节点由 [`register_static_dev_node`]
    /// 直接补绑。
    fn bind_registered_static_nodes(&self) -> VfsResult<()> {
        let mut bound: Vec<&'static str> = Vec::new();
        for node in STATIC_DEV_NODES.lock().iter().copied() {
            if let Err(err) = bind_static_node(self, node) {
                for name in bound.iter().rev() {
                    let _ = self.unbind(name);
                }
                return Err(err);
            }
            bound.push(node.name());
        }
        Ok(())
    }

    /// 将块设备绑定到 devtmpfs 相对路径。
    ///
    /// - `user_name`：用户空间可见的相对路径（如 `"block/root"`）
    /// - `dev`：已注册的块设备对象（`Arc` 直接存入 inode）
    pub fn bind_block(&self, user_name: &str, dev: Arc<BlockDevice>) -> VfsResult<()> {
        split_devtmpfs_path(user_name)?;
        if !dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        if self.lookup_node_at(user_name).is_ok() {
            return Err(VfsError::AlreadyExists);
        }
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;
        let rdev = super::device_numbers::register_block(user_name, dev.name())
            .ok_or(VfsError::NoSpace)?;

        let now = Timespec::now();
        let meta = InodeMeta {
            size: 0,
            nlink: 1,
            mode: devtmpfs_compat_default_device_mode(),
            uid: devtmpfs_compat_default_uid(),
            gid: devtmpfs_compat_default_gid(),
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        let ops = Arc::new(DevBlockOps { dev });
        let inode = Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            FileType::BlockDevice,
            rdev,
            devtmpfs_compat_block_size(),
            None,
            meta,
            ops,
            sb_weak,
        );

        if let Err(err) = self.insert_node_at(user_name, inode) {
            super::device_numbers::unregister_node(user_name);
            return Err(err);
        }
        Ok(())
    }

    /// 在 devtmpfs 相对路径上创建一个符号链接节点。
    ///
    /// `target` 按标准符号链接文本保存，不在创建时验证目标是否存在。相对目标会按
    /// VFS path walker 的规则以链接所在目录为基准继续解析。
    pub fn bind_symlink(&self, user_name: &str, target: &str) -> VfsResult<()> {
        split_devtmpfs_path(user_name)?;
        let inode = self.new_symlink_inode(
            target,
            devtmpfs_compat_default_uid(),
            devtmpfs_compat_default_gid(),
        )?;
        self.insert_node_at(user_name, inode)
    }

    /// 绑定一个自定义 devtmpfs 节点。
    ///
    /// 自定义节点用于未来 LKM 或新设备类别直接提供自己的 `InodeOps`，devtmpfs
    /// 不需要知道该节点背后的设备类型。无法完整实现的设备语义应在对应 `InodeOps`
    /// 或 `FileOps` 内部明确返回不支持，而不是在 devtmpfs 中增加临时类型分支。
    pub fn bind_custom(&self, spec: &CustomDevNodeSpec) -> VfsResult<()> {
        split_devtmpfs_path(spec.name())?;
        if self.lookup_node_at(spec.name()).is_ok() {
            return Err(VfsError::AlreadyExists);
        }
        let mut registered_rdev = false;
        let rdev = if spec.rdev() != DevId::new(0, 0) {
            spec.rdev()
        } else {
            match spec.kind() {
                FileType::CharDevice => {
                    registered_rdev = true;
                    super::device_numbers::register_char(spec.name(), spec.name())
                        .ok_or(VfsError::NoSpace)?
                }
                FileType::BlockDevice => {
                    registered_rdev = true;
                    super::device_numbers::register_block(spec.name(), spec.name())
                        .ok_or(VfsError::NoSpace)?
                }
                _ => spec.rdev(),
            }
        };
        let inode = self.new_custom_inode(spec, rdev)?;
        if let Err(err) = self.insert_node_at(spec.name(), inode) {
            if registered_rdev {
                super::device_numbers::unregister_node(spec.name());
            }
            return Err(err);
        }
        Ok(())
    }

    /// 绑定一个通用 devtmpfs 节点规格。
    pub fn bind_node(&self, node: &DevNodeSpec) -> VfsResult<()> {
        match node {
            DevNodeSpec::Char { name, dev } => self.bind_char(name, dev.clone()),
            DevNodeSpec::Block { name, dev } => self.bind_block(name, Arc::clone(dev)),
            DevNodeSpec::Symlink { name, target } => self.bind_symlink(name, target),
            DevNodeSpec::Custom(spec) => self.bind_custom(spec),
        }
    }

    /// 批量绑定一个 function 声明的 devtmpfs 节点集合。
    ///
    /// 任一节点创建失败时，已经创建的节点会按逆序回滚。这样 PnP 注册要么完整暴露
    /// 一个 function 的全部节点，要么不留下半完成名字空间状态。
    pub fn bind_nodes(&self, nodes: &DevNodeSet) -> VfsResult<()> {
        let mut bound: Vec<&str> = Vec::new();
        for node in nodes.nodes() {
            if let Err(err) = self.bind_node(node) {
                for name in bound.iter().rev() {
                    let _ = self.unbind(name);
                }
                return Err(err);
            }
            bound.push(node.name());
        }
        Ok(())
    }

    /// 解除设备绑定，删除 devtmpfs 中的相对路径节点。
    pub fn unbind(&self, user_name: &str) -> VfsResult<()> {
        self.remove_node_at(user_name)?;
        super::device_numbers::unregister_node(user_name);
        Ok(())
    }

    /// 批量解绑一个 function 声明的 devtmpfs 节点集合。
    pub fn unbind_nodes(&self, nodes: &DevNodeSet) -> VfsResult<()> {
        let mut last_error = None;
        for node in nodes.nodes().iter().rev() {
            match self.unbind(node.name()) {
                Ok(()) | Err(VfsError::NotFound) => {}
                Err(err) => last_error = Some(err),
            }
        }
        if let Some(err) = last_error {
            Err(err)
        } else {
            Ok(())
        }
    }

    /// 根据 devtmpfs 相对路径恢复其绑定的字符设备对象。
    pub fn char_dev(&self, user_name: &str) -> Option<CharDevice> {
        let inode = self.lookup_node_at(user_name).ok()?;
        let ops = inode.downcast_ops::<DevCharOps>()?;
        let dev = ops.dev();
        dev.is_active().then_some(dev)
    }

    /// 根据 devtmpfs 相对路径恢复其绑定的块设备对象。
    pub fn block_dev(&self, user_name: &str) -> Option<Arc<BlockDevice>> {
        let inode = self.lookup_node_at(user_name).ok()?;
        block_device_from_inode(&inode)
    }
}

impl SuperblockOps for DevTmpfsSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Ok(FsStat {
            fs_type: 0x444f4445, // "devt" 魔数
            block_size: devtmpfs_compat_block_size() as u64,
            total_blocks: 0,
            free_blocks: 0,
            avail_blocks: 0,
            total_inodes: self.next_ino.load(Ordering::Relaxed),
            free_inodes: 0,
            fs_id: sb.fs_id.raw(),
            name_max: DEVTMPFS_NAME_MAX as u32,
        })
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }

    fn remount(&self, _sb: &Arc<Superblock>, _flags: MountFlags) -> VfsResult<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── FsDriver ─────────

/// devtmpfs 文件系统驱动。
///
/// 通过 `mount` 方法创建超级块，返回的 `Arc<Superblock>` 的
/// `ops` 字段可通过 `downcast_ops::<DevTmpfsSuperblockOps>()` 取回，
/// 供驱动调用 `bind_char` / `bind_block` / `bind_symlink` 或批量节点 API。
///
/// # 典型初始化流程
///
/// ```rust,ignore
/// // 1. 挂载 devtmpfs 到 /dev
/// let sb = FS_REGISTRY.find("devtmpfs").unwrap().mount(None, "")?;
/// mount_ns.mount(&dev_dentry, &dev_mount, sb.clone(), MountFlags::empty())?;
///
/// // 2. 驱动注册后绑定设备
/// let ops = sb.downcast_ops::<DevTmpfsSuperblockOps>().unwrap();
/// ops.bind_char("console", char_dev)?;       // 直接绑定对象引用
/// ops.bind_block("block/root", block_dev)?;  // 目录化块设备节点
/// ops.bind_symlink("disk/root", "../block/root")?; // 可选符号链接投影
/// ```
pub struct DevTmpfsDriver;

impl FsDriver for DevTmpfsDriver {
    fn name(&self) -> &'static str {
        "devtmpfs"
    }

    fn flags(&self) -> FsDriverFlags {
        FsDriverFlags::NODEV.with(FsDriverFlags::SINGLE)
    }

    fn mount(&self, _dev: Option<&str>, _data: &str) -> VfsResult<Arc<Superblock>> {
        // devtmpfs 是内核设备树的 POSIX 投影，不能像 tmpfs 一样每次 mount
        // 都创建空实例。启动期 PnP bridge 安装后，用户态再次挂载 devtmpfs
        // 应复用同一个 superblock，否则会覆盖掉已经绑定的 console/uart/vd0 等节点。
        if let Some(sb) = mounted_devtmpfs_sb() {
            return Ok(sb);
        }

        let fs_id = FsId::new(DEVTMPFS_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));

        let root_ops = Arc::new(DevDirOps::new());

        // 只构造一个 DevTmpfsSuperblockOps 实例，move 进 new_cyclic 闭包，
        // 写入 weak ref 后再 Box 化存入 Superblock。外层不再持有任何引用，
        // 后续通过 sb.downcast_ops::<DevTmpfsSuperblockOps>() 访问。
        let sb_ops = DevTmpfsSuperblockOps {
            next_ino: AtomicU64::new(2),
            sb: vfs::sync::Spinlock::new(None),
        };

        let sb = Superblock::new(move |weak_sb| {
            sb_ops.sb.lock().replace(weak_sb.clone());

            let now = Timespec::now();
            let root_meta = InodeMeta {
                size: 0,
                nlink: 2,
                mode: devtmpfs_compat_default_dir_mode(),
                uid: devtmpfs_compat_default_uid(),
                gid: devtmpfs_compat_default_gid(),
                atime: now,
                mtime: now,
                ctime: now,
                blocks: 0,
            };

            let root_inode = Inode::new(
                InodeId { fs_id, ino: 1 },
                FileType::Directory,
                DevId::new(0, 0),
                devtmpfs_compat_block_size(),
                None,
                root_meta,
                Arc::clone(&root_ops) as Arc<dyn InodeOps + Send + Sync>,
                weak_sb.clone(),
            );

            let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));

            Superblock {
                fs_type: "devtmpfs",
                fs_id,
                dev_id: None,
                block_size: devtmpfs_compat_block_size(),
                name_max: DEVTMPFS_NAME_MAX as u32,
                root_inode,
                root_dentry,
                inode_cache: vfs::superblock::InodeCache::new(),
                ops: Box::new(sb_ops),
                self_weak: weak_sb,
            }
        });

        ensure_builtin_static_nodes_registered()?;

        let ops = sb
            .downcast_ops::<DevTmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
        ops.bind_registered_static_nodes()?;

        let (sb, _) = publish_devtmpfs_sb(sb);
        Ok(sb)
    }

    fn kill_sb(&self, _sb: Arc<Superblock>) {}

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

//! sysfs：`/sys` 虚拟文件系统。
//!
//! 当前实现按访问入口呈现内核对象快照，设备视图通过 function 注册表的兼容层
//! helper 收集字符/块设备，不向 sysfs 泄露具体 function 类型。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, Ordering};

use errno::Errno;
use sched::{online_cpu_mask, supported_cpu_mask};
use vfs::cred::{Credentials, Gid, Uid};
use vfs::dentry::{Dentry, SmallStr};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::mount::MountFlags;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{FsDriver, FsDriverFlags, Superblock, SuperblockOps};
use vfs::sync::Spinlock;

use crate::dev::block::BlockDevice;
use crate::dev::cpu;
use crate::dev::enumerate::{DEVICES, PNP_DEVICES};
use crate::dev::function::DeviceFunction;
use crate::dev::net::NET_CLASS;
use crate::dev::pnp::{PnpDependency, PnpId, PnpOwnedResourceSnapshot, PnpResourceKind, PnpState};
use crate::vfs::device_files::projection::{
    PublishedDevNodeClass, append_function_projection_diagnostics, published_block_devnodes,
    published_char_devnodes, published_devnode_classes,
};
use crate::vfs::user_api::device_numbers;

// ─── 静态 ino 编号 ──────────────────────────────────────────
const ROOT_INO: u64 = 1;
const BLOCK_DIR_INO: u64 = 2;
const DEVICES_DIR_INO: u64 = 3;
const DEV_DIR_INO: u64 = 4;
const KERNEL_DIR_INO: u64 = 5;
const FS_DIR_INO: u64 = 6;
const BUS_DIR_INO: u64 = 7;
const CLASS_DIR_INO: u64 = 8;
const MODULE_DIR_INO: u64 = 9;
const POWER_DIR_INO: u64 = 10;
const FIRMWARE_DIR_INO: u64 = 11;
const DEVICES_SYSTEM_INO: u64 = 12;
const DEVICES_SYSTEM_CPU_INO: u64 = 13;
const DEVICES_SYSTEM_CPU_ONLINE_INO: u64 = 14;
const DEVICES_SYSTEM_CPU_POSSIBLE_INO: u64 = 15;
const DEVICES_SYSTEM_CPU_PRESENT_INO: u64 = 16;
const DEVICES_VIRTUAL_INO: u64 = 17;
const DEVICES_PNP_INO: u64 = 18;
const KERNEL_HOSTNAME_INO: u64 = 19;
const KERNEL_OSTYPE_INO: u64 = 20;
const KERNEL_OSRELEASE_INO: u64 = 21;
const KERNEL_VERSION_INO: u64 = 22;
const KERNEL_CMDLINE_INO: u64 = 23;
const KERNEL_DEVICE_FUNCTIONS_INO: u64 = 24;
const DEV_BLOCK_DIR_INO: u64 = 30;
const DEV_CHAR_DIR_INO: u64 = 31;
const FS_CGROUP_INO: u64 = 40;

const CPU_BASE: u64 = 10_000_000;
const CPU_SLOTS: u64 = 4;
const CPU_TOPOLOGY_BASE: u64 = 20_000_000;
const CPU_TOPOLOGY_SLOTS: u64 = 8;

static SYSFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);
static SYSFS_INO_REGISTRY: Spinlock<Option<SysfsInoRegistry>> = Spinlock::new(None);

const SYSFS_MAGIC: u64 = 0x6265_6572;
const SYSFS_DYNAMIC_INO_START: u64 = 1_000_000_000;
const SYSFS_BLOCK_CLASS: &str = "block";
const SYSFS_CHAR_CLASS: &str = "char";
const SYSFS_NET_CLASS: &str = NET_CLASS.as_str();

/// sysfs 用户视图默认策略。
///
/// sysfs 优先渲染 typed device snapshot；当底层设备暂未暴露队列深度、链路类型
/// 或电源管理状态时，统一从这里取用户 ABI 默认值，避免魔数散落在各个属性文件。
#[derive(Clone, Copy)]
struct SysfsUserViewPolicy {
    block_nr_requests: u32,
    net_link_type_ether: u32,
    net_tx_queue_len: u32,
    power_runtime_status: &'static str,
    power_control: &'static str,
    power_wakeup: &'static str,
}

impl SysfsUserViewPolicy {
    const fn standard() -> Self {
        Self {
            block_nr_requests: 64,
            net_link_type_ether: 1,
            net_tx_queue_len: 1000,
            power_runtime_status: "active",
            power_control: "on",
            power_wakeup: "disabled",
        }
    }

    fn net_ifindex(self, iface_id: u32) -> u32 {
        // 当前网络栈 snapshot 尚未携带独立 ifindex。把编号策略集中在用户视图
        // policy 内，后续接入 stable interface index 时无需改属性渲染路径。
        iface_id.saturating_add(1)
    }
}

const SYSFS_USER_VIEW_POLICY: SysfsUserViewPolicy = SysfsUserViewPolicy::standard();

// ─── 渲染辅助 ──────────────────────────────────────────────

fn timespec_now() -> Timespec {
    Timespec::now()
}

struct SysfsInoRegistry {
    next: u64,
    by_key: BTreeMap<String, u64>,
}

impl SysfsInoRegistry {
    fn new() -> Self {
        Self {
            next: SYSFS_DYNAMIC_INO_START,
            by_key: BTreeMap::new(),
        }
    }

    fn get_or_alloc(&mut self, key: String) -> u64 {
        if let Some(ino) = self.by_key.get(&key).copied() {
            return ino;
        }
        let ino = self.alloc_unused_ino();
        self.by_key.insert(key, ino);
        ino
    }

    fn alloc_unused_ino(&mut self) -> u64 {
        let start = self.next;
        loop {
            let ino = self.next;
            self.next = self.next.checked_add(1).unwrap_or(SYSFS_DYNAMIC_INO_START);
            if !self.by_key.values().any(|allocated| *allocated == ino) {
                return ino;
            }
            // 动态 inode 空间理论上不可能被 sysfs 用尽；这里仍显式处理完整回绕，
            // 避免计数器重复扫描时陷入无限循环。
            if self.next == start {
                return SYSFS_DYNAMIC_INO_START;
            }
        }
    }
}

/// 动态 sysfs inode 的稳定 key。
///
/// key 集中由专用 helper 生成；inode 编号只承担 VFS 标识职责，不反向成为设备
/// 身份，也不依赖枚举顺序。后续如需改成 enum/typed key，只需要替换本结构的
/// 构造器，不必修改目录渲染逻辑。
struct SysfsKey(String);

impl SysfsKey {
    fn raw(value: String) -> Self {
        Self(value)
    }

    fn rdev(rdev: DevId) -> String {
        format!("{}:{}", rdev.major, rdev.minor)
    }

    fn block_device(name: &str) -> Self {
        Self::raw(format!("block/{name}"))
    }

    fn block_device_slot(name: &str, slot: u64) -> Self {
        Self::raw(format!("block/{name}/slot/{slot}"))
    }

    fn block_queue_slot(name: &str, slot: u64) -> Self {
        Self::raw(format!("block/{name}/queue/{slot}"))
    }

    fn device(class_name: &str, rdev: DevId) -> Self {
        Self::raw(format!("devices/{class_name}/{}", Self::rdev(rdev)))
    }

    fn device_slot(class_name: &str, rdev: DevId, slot: u64) -> Self {
        Self::raw(format!(
            "devices/{class_name}/{}/slot/{slot}",
            Self::rdev(rdev)
        ))
    }

    fn device_power_slot(class_name: &str, rdev: DevId, slot: u64) -> Self {
        Self::raw(format!(
            "devices/{class_name}/{}/power/{slot}",
            Self::rdev(rdev)
        ))
    }

    fn virtual_class(class_name: &str) -> Self {
        Self::raw(format!("devices/virtual/{class_name}"))
    }

    fn virtual_device(class_name: &str, rdev: DevId) -> Self {
        Self::raw(format!("devices/virtual/{class_name}/{}", Self::rdev(rdev)))
    }

    fn virtual_device_slot(class_name: &str, rdev: DevId, slot: u64) -> Self {
        Self::raw(format!(
            "devices/virtual/{class_name}/{}/slot/{slot}",
            Self::rdev(rdev)
        ))
    }

    fn virtual_device_power_slot(class_name: &str, rdev: DevId, slot: u64) -> Self {
        Self::raw(format!(
            "devices/virtual/{class_name}/{}/power/{slot}",
            Self::rdev(rdev)
        ))
    }

    fn pnp_bus(bus: &str) -> Self {
        Self::raw(format!("devices/pnp/{bus}"))
    }

    fn pnp_device(bus: &str, name: &str) -> Self {
        Self::raw(format!("devices/pnp/{bus}/{name}"))
    }

    fn pnp_device_slot(bus: &str, name: &str, slot: u64) -> Self {
        Self::raw(format!("devices/pnp/{bus}/{name}/slot/{slot}"))
    }

    fn bus_class(bus: &str) -> Self {
        Self::raw(format!("bus/{bus}"))
    }

    fn bus_class_devices(bus: &str) -> Self {
        Self::raw(format!("bus/{bus}/devices"))
    }

    fn bus_class_device_link(bus: &str, name: &str) -> Self {
        Self::raw(format!("bus/{bus}/devices/{name}"))
    }

    fn class_dir(class_name: &str) -> Self {
        Self::raw(format!("class/{class_name}"))
    }

    fn class_node(class_name: &str, name: &str) -> Self {
        Self::raw(format!("class/{class_name}/{name}"))
    }

    fn dev_block_link(rdev: DevId) -> Self {
        Self::raw(format!("dev/block/{}", Self::rdev(rdev)))
    }

    fn dev_char_dir(rdev: DevId) -> Self {
        Self::raw(format!("dev/char/{}", Self::rdev(rdev)))
    }

    fn dev_char_inner(rdev: DevId, slot: u64) -> Self {
        Self::raw(format!("dev/char/{}/slot/{slot}", Self::rdev(rdev)))
    }

    fn net_iface(iface_id: u32) -> Self {
        Self::raw(format!("class/net/iface/{iface_id}"))
    }

    fn net_iface_slot(iface_id: u32, slot: u64) -> Self {
        Self::raw(format!("class/net/iface/{iface_id}/slot/{slot}"))
    }

    fn net_stats(iface_id: u32) -> Self {
        Self::raw(format!("class/net/iface/{iface_id}/statistics"))
    }

    fn net_stats_slot(iface_id: u32, slot: u64) -> Self {
        Self::raw(format!("class/net/iface/{iface_id}/statistics/slot/{slot}"))
    }
}

fn sysfs_dynamic_ino(key: SysfsKey) -> u64 {
    let mut registry = SYSFS_INO_REGISTRY.lock();
    registry
        .get_or_insert_with(SysfsInoRegistry::new)
        .get_or_alloc(key.0)
}

fn inode_meta(mode: u16, nlink: u32, now: Timespec) -> InodeMeta {
    InodeMeta {
        size: 0,
        nlink,
        mode: FileMode::new(mode),
        uid: Uid::ROOT,
        gid: Gid::ROOT,
        atime: now,
        mtime: now,
        ctime: now,
        blocks: 0,
    }
}

fn mk_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    ino: u64,
    kind: FileType,
    mode: u16,
    nlink: u32,
    ops: Arc<dyn InodeOps + Send + Sync>,
) -> Arc<Inode> {
    Inode::new(
        InodeId { fs_id, ino },
        kind,
        DevId::new(0, 0),
        4096,
        None,
        inode_meta(mode, nlink, timespec_now()),
        ops,
        weak_sb.clone(),
    )
}

// ─── 快照：dev core → /sys 树 ────────────────────────────────

#[derive(Clone)]
struct CharDevSnapshot {
    /// /sys/devices/ 下的目录名 = `fw_name`（如 "serial@9000000"、"null"）。
    sysfs_name: String,
    rdev: DevId,
    class_name: &'static str,
}

#[derive(Clone)]
struct BlockDevSnapshot {
    /// /sys/block/ 与 /sys/dev/block/ 下的目录名 = `dev.name()`（如 "vd0"）。
    sysfs_name: String,
    rdev: DevId,
    dev: Arc<BlockDevice>,
    class_name: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SysDeviceKind {
    Char,
    Block,
}

#[derive(Clone)]
struct SysDeviceSnapshot {
    /// /sys/devices/ 下的目录名。
    sysfs_name: String,
    rdev: DevId,
    class_name: &'static str,
    _kind: SysDeviceKind,
}

#[derive(Clone)]
struct SysVirtualDeviceSnapshot {
    /// /sys/devices/virtual/<class>/ 下的目录名，来自兼容层展示名。
    sysfs_name: String,
    rdev: DevId,
    class_name: &'static str,
}

#[derive(Clone)]
struct SysPnpDeviceSnapshot {
    /// /sys/devices/pnp/<bus>/ 下的目录名，来自 PnP 设备名的 sysfs 安全形式。
    sysfs_name: String,
    /// PnP core 内部登记名，保留原始固件/总线语义用于展示。
    name: String,
    bus_type: &'static str,
    id: PnpId,
    state: &'static str,
    driver: Option<&'static str>,
    parent: Option<String>,
    child_count: usize,
    functions: Vec<Arc<dyn DeviceFunction>>,
    resources: Vec<PnpOwnedResourceSnapshot>,
    deferred_dependency: Option<PnpDependency>,
}

#[derive(Clone)]
struct CharDevNodeSnapshot {
    /// /sys/devices/ 下的目标目录名。
    sysfs_name: String,
    /// devtmpfs 相对路径，用于 uevent DEVNAME。
    devtmpfs_name: String,
    rdev: DevId,
    class_name: &'static str,
}

#[derive(Clone)]
struct BlockDevNodeSnapshot {
    /// /sys/block/ 下的目标目录名。
    sysfs_name: String,
    rdev: DevId,
    class_name: &'static str,
}

#[derive(Clone)]
struct SysClassSnapshot {
    /// /sys/class/ 下的 class 目录名，来自 dev core 的 function class 或兼容投影。
    name: &'static str,
}

#[derive(Clone)]
struct SysClassNodeSnapshot {
    class_name: &'static str,
    sysfs_name: String,
    kind: SysClassNodeKind,
}

#[derive(Clone)]
enum SysClassNodeKind {
    /// 兼容 class 节点以 symlink 指向 `/sys/devices` 或 `/sys/block` 中的真实对象。
    Symlink { target: String },
    /// 网络接口没有 `/dev` 节点，class 节点直接作为目录暴露 typed net 属性。
    NetInterface { iface_id: u32 },
}

fn sysfs_fallible_string(value: &str) -> Option<String> {
    let mut out = String::new();
    out.try_reserve(value.len()).ok()?;
    out.push_str(value);
    Some(out)
}

fn sysfs_fallible_smallstr(value: &str) -> Option<SmallStr> {
    let bytes = value.as_bytes();
    if bytes.len() <= 23 {
        let mut buf = [0u8; 23];
        buf[..bytes.len()].copy_from_slice(bytes);
        return Some(SmallStr::Inline {
            len: bytes.len() as u8,
            buf,
        });
    }
    Some(SmallStr::Heap(sysfs_fallible_string(value)?))
}

fn sysfs_smallstr_lossy(value: &str) -> SmallStr {
    sysfs_fallible_smallstr(value).unwrap_or_else(|| {
        let mut buf = [0u8; 23];
        buf[0] = b'?';
        SmallStr::Inline { len: 1, buf }
    })
}

fn push_sysfs_dir_entry(out: &mut Vec<DirEntry>, ino: u64, name: &str, kind: FileType) -> bool {
    // sysfs 是诊断视图：目录快照分配失败时返回已收集的前缀，避免 readdir
    // 因长设备名或瞬时低内存 panic。下一次 readdir 会重新构造快照。
    if out.try_reserve(1).is_err() {
        return false;
    }
    let Some(name) = sysfs_fallible_smallstr(name) else {
        return false;
    };
    out.push(DirEntry { ino, name, kind });
    true
}

/// sysfs 单次访问从 dev core 拷贝出的不可变快照。
///
/// 目录 inode 保存稳定 key，lookup/readdir 可以在访问入口重新收集快照；属性
/// 文件 inode 保存创建它时的快照，保证一次 read 看到的是同一组字段。
#[derive(Clone, Default)]
struct SysSnapshot {
    devices: Vec<SysDeviceSnapshot>,
    pnp_devices: Vec<SysPnpDeviceSnapshot>,
    pnp_buses: Vec<&'static str>,
    virtual_devices: Vec<SysVirtualDeviceSnapshot>,
    virtual_classes: Vec<&'static str>,
    classes: Vec<SysClassSnapshot>,
    class_nodes: Vec<SysClassNodeSnapshot>,
    chars: Vec<CharDevSnapshot>,
    blocks: Vec<BlockDevSnapshot>,
    char_nodes: Vec<CharDevNodeSnapshot>,
    block_nodes: Vec<BlockDevNodeSnapshot>,
}

fn sysfs_component_name(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch == '/' || ch == '\0' {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() { "device".into() } else { out }
}

fn sysfs_unique_name_with_rdev<F>(base: &str, rdev: DevId, mut exists: F) -> String
where
    F: FnMut(&str) -> bool,
{
    let primary = sysfs_component_name(base);
    if !exists(&primary) {
        return primary;
    }

    // sysfs 目录名是兼容层投影，不是底层设备身份。同名设备出现时用已分配的
    // rdev 做稳定消歧；这只影响用户态可见路径，不反向改变 dev core。
    let mut suffix = 0usize;
    loop {
        let raw = if suffix == 0 {
            format!("{}-{}:{}", base, rdev.major, rdev.minor)
        } else {
            format!("{}-{}:{}-{suffix}", base, rdev.major, rdev.minor)
        };
        let candidate = sysfs_component_name(&raw);
        if !exists(&candidate) {
            return candidate;
        }
        if suffix == usize::MAX {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
}

fn pnp_state_name(state: PnpState) -> &'static str {
    match state {
        PnpState::Discovered => "discovered",
        PnpState::Probing => "probing",
        PnpState::Bound => "bound",
        PnpState::Removing => "removing",
        PnpState::Gone => "gone",
    }
}

fn pnp_resource_kind_name(kind: PnpResourceKind) -> &'static str {
    match kind {
        PnpResourceKind::Mmio => "mmio",
        PnpResourceKind::Irq => "irq",
        PnpResourceKind::IrqDomain => "irq-domain",
        PnpResourceKind::Msi => "msi",
        PnpResourceKind::MsiController => "msi-controller",
        PnpResourceKind::Syscon => "syscon",
        PnpResourceKind::Flash => "flash",
        PnpResourceKind::FwCfg => "fwcfg",
        PnpResourceKind::FirmwareBus => "firmware-bus",
        PnpResourceKind::PciHostBridge => "pci-host-bridge",
        PnpResourceKind::Dma => "dma",
        PnpResourceKind::Function => "function",
        PnpResourceKind::Other(name) => name,
    }
}

fn pnp_dependency_name(dependency: PnpDependency) -> String {
    match dependency {
        PnpDependency::IrqController(id) => {
            format!("irq-controller:{id}")
        }
        PnpDependency::DefaultIrqDomain => "default-irq-domain".into(),
        PnpDependency::MsiController(id) => {
            format!("msi-controller:{id}")
        }
        PnpDependency::Syscon(id) => format!("syscon:{id}"),
        PnpDependency::FwCfg => "fwcfg".into(),
        PnpDependency::FirmwareBus => "firmware-bus".into(),
        PnpDependency::PciHostBridge(domain) => {
            format!("pci-host-bridge:{domain}")
        }
        PnpDependency::Dma => "dma".into(),
        PnpDependency::Other(name) => name.into(),
    }
}

fn class_for_devnode(
    devnode_classes: &[PublishedDevNodeClass],
    node_name: &str,
    fallback: &'static str,
) -> &'static str {
    devnode_classes
        .iter()
        .find(|entry| entry.node_name() == node_name)
        .map(|entry| entry.class_name())
        .unwrap_or(fallback)
}

fn push_sys_class(snap: &mut SysSnapshot, name: &'static str) {
    if !snap.classes.iter().any(|class| class.name == name) {
        snap.classes.push(SysClassSnapshot { name });
    }
}

fn push_class_node(
    snap: &mut SysSnapshot,
    class_name: &'static str,
    sysfs_name: String,
    kind: SysClassNodeKind,
) {
    push_sys_class(snap, class_name);
    if snap
        .class_nodes
        .iter()
        .any(|node| node.class_name == class_name && node.sysfs_name == sysfs_name)
    {
        return;
    }
    snap.class_nodes.push(SysClassNodeSnapshot {
        class_name,
        sysfs_name,
        kind,
    });
}

impl SysSnapshot {
    fn collect() -> Self {
        let mut snap = SysSnapshot::default();
        let records = device_numbers::try_records().unwrap_or_default();
        let devnode_classes = published_devnode_classes();

        // PnP 设备是 dev core 的硬件身份与 driver 绑定视图。这里先把它们放入
        // sysfs 快照，即便设备没有 `/dev` 投影，也能在 `/sys/devices/pnp` 中诊断。
        for dev in PNP_DEVICES.try_list().unwrap_or_default() {
            let bus_type = dev.info.bus_type().as_str();
            let mut sysfs_name = sysfs_component_name(&dev.name);
            if snap
                .pnp_devices
                .iter()
                .any(|existing| existing.bus_type == bus_type && existing.sysfs_name == sysfs_name)
            {
                sysfs_name = sysfs_component_name(&format!("{}-{}", dev.name, dev.id));
            }
            let mut suffix = 1usize;
            while snap
                .pnp_devices
                .iter()
                .any(|existing| existing.bus_type == bus_type && existing.sysfs_name == sysfs_name)
            {
                sysfs_name = sysfs_component_name(&format!("{}-{}-{suffix}", dev.name, dev.id));
                suffix = suffix.saturating_add(1);
            }
            let functions = dev.try_functions().unwrap_or_default();
            let resources = dev.try_owned_resources().unwrap_or_default();
            let parent = dev.parent().map(|parent| parent.name.to_string());
            let child_count = dev
                .try_children()
                .map(|children| children.len())
                .unwrap_or(0);
            snap.pnp_devices.push(SysPnpDeviceSnapshot {
                sysfs_name,
                name: dev.name.to_string(),
                bus_type,
                id: dev.id.clone(),
                state: pnp_state_name(dev.state()),
                driver: dev.bound_driver_name(),
                parent,
                child_count,
                functions,
                resources,
                deferred_dependency: dev.deferred_dependency(),
            });
            if !snap.pnp_buses.contains(&bus_type) {
                snap.pnp_buses.push(bus_type);
            }
        }

        // `/sys/block` 和 `/sys/devices` 展示 typed device object；`rdev` 来自
        // projection 层已经确认发布的 devtmpfs+device_numbers 联合快照，sysfs
        // 不再重复解释 `/dev` 节点和设备号表的关联规则。
        for projection in published_block_devnodes(&DEVICES.functions) {
            let dev = Arc::clone(projection.dev());
            let sysfs_name = sysfs_unique_name_with_rdev(dev.name(), projection.rdev(), |name| {
                snap.blocks.iter().any(|block| block.sysfs_name == name)
            });
            snap.blocks.push(BlockDevSnapshot {
                sysfs_name,
                rdev: projection.rdev(),
                dev,
                class_name: projection.class_id().as_str(),
            });
        }

        for projection in published_char_devnodes(&DEVICES.functions) {
            let sysfs_name = sysfs_unique_name_with_rdev(
                projection.dev().fw_name(),
                projection.rdev(),
                |name| snap.chars.iter().any(|ch| ch.sysfs_name == name),
            );
            snap.chars.push(CharDevSnapshot {
                sysfs_name,
                rdev: projection.rdev(),
                class_name: projection.class_id().as_str(),
            });
        }

        // `/sys/devices` 使用统一设备视图渲染公共属性和 symlink，避免目录逻辑
        // 在 char/block 两套索引之间反复分支。当前已有 function 只暴露 char/block；
        // 后续新增 class 时只需要扩展快照构造，不需要改公共设备属性渲染。
        snap.devices
            .extend(snap.chars.iter().map(|dev| SysDeviceSnapshot {
                sysfs_name: dev.sysfs_name.clone(),
                rdev: dev.rdev,
                class_name: dev.class_name,
                _kind: SysDeviceKind::Char,
            }));
        snap.devices
            .extend(snap.blocks.iter().map(|dev| SysDeviceSnapshot {
                sysfs_name: dev.sysfs_name.clone(),
                rdev: dev.rdev,
                class_name: dev.class_name,
                _kind: SysDeviceKind::Block,
            }));

        // `/sys/dev/{char,block}` 是 `dev_t` 的用户 ABI 视图，来源只能是
        // device_numbers registry；这里不向底层设备模型反向写入任何信息。
        for record in records {
            match record.kind {
                device_numbers::DeviceNumberKind::Char => {
                    let class_name =
                        class_for_devnode(&devnode_classes, &record.node_name, SYSFS_CHAR_CLASS);
                    let backing_name = snap
                        .devices
                        .iter()
                        .find(|dev| dev.class_name == class_name && dev.rdev == record.rdev)
                        .map(|dev| dev.sysfs_name.clone());
                    let sysfs_name = backing_name.unwrap_or_else(|| {
                        sysfs_unique_name_with_rdev(&record.display_name, record.rdev, |name| {
                            snap.virtual_devices
                                .iter()
                                .any(|dev| dev.class_name == class_name && dev.sysfs_name == name)
                                || snap.char_nodes.iter().any(|node| {
                                    node.class_name == class_name && node.sysfs_name == name
                                })
                        })
                    });
                    if !snap
                        .devices
                        .iter()
                        .any(|dev| dev.class_name == class_name && dev.rdev == record.rdev)
                        && !snap
                            .virtual_devices
                            .iter()
                            .any(|dev| dev.class_name == class_name && dev.rdev == record.rdev)
                    {
                        snap.virtual_devices.push(SysVirtualDeviceSnapshot {
                            sysfs_name: sysfs_name.clone(),
                            rdev: record.rdev,
                            class_name,
                        });
                    }
                    snap.char_nodes.push(CharDevNodeSnapshot {
                        sysfs_name,
                        devtmpfs_name: record.node_name,
                        rdev: record.rdev,
                        class_name,
                    })
                }
                device_numbers::DeviceNumberKind::Block => {
                    let class_name =
                        class_for_devnode(&devnode_classes, &record.node_name, SYSFS_BLOCK_CLASS);
                    let backing_name = snap
                        .devices
                        .iter()
                        .find(|dev| dev.class_name == class_name && dev.rdev == record.rdev)
                        .map(|dev| dev.sysfs_name.clone());
                    let sysfs_name = backing_name.unwrap_or_else(|| {
                        sysfs_unique_name_with_rdev(&record.display_name, record.rdev, |name| {
                            snap.virtual_devices
                                .iter()
                                .any(|dev| dev.class_name == class_name && dev.sysfs_name == name)
                                || snap.block_nodes.iter().any(|node| {
                                    node.class_name == class_name && node.sysfs_name == name
                                })
                        })
                    });
                    if !snap
                        .devices
                        .iter()
                        .any(|dev| dev.class_name == class_name && dev.rdev == record.rdev)
                        && !snap
                            .virtual_devices
                            .iter()
                            .any(|dev| dev.class_name == class_name && dev.rdev == record.rdev)
                    {
                        snap.virtual_devices.push(SysVirtualDeviceSnapshot {
                            sysfs_name: sysfs_name.clone(),
                            rdev: record.rdev,
                            class_name,
                        });
                    }
                    snap.block_nodes.push(BlockDevNodeSnapshot {
                        sysfs_name,
                        rdev: record.rdev,
                        class_name,
                    })
                }
            }
        }
        for dev in &snap.virtual_devices {
            if !snap.virtual_classes.contains(&dev.class_name) {
                snap.virtual_classes.push(dev.class_name);
            }
        }

        // `/sys/class` 是 dev core function class 与 VFS 兼容投影的汇合点。
        // 节点名和设备号来自用户 ABI registry；class 语义优先来自 function
        // registry。这样 RTC 等 custom devnode 能自然进入 `rtc` class，而不
        // 被压回底层并不关心的 `char` 类别。
        let char_node_count = snap.char_nodes.len();
        for idx in 0..char_node_count {
            let node = &snap.char_nodes[idx];
            let class_name = node.class_name;
            let sysfs_name = node.sysfs_name.clone();
            let target = char_device_link_target(&snap, idx, "../../");
            push_class_node(
                &mut snap,
                class_name,
                sysfs_name,
                SysClassNodeKind::Symlink { target },
            );
        }
        let block_node_count = snap.block_nodes.len();
        for idx in 0..block_node_count {
            let node = &snap.block_nodes[idx];
            let class_name = node.class_name;
            let sysfs_name = node.sysfs_name.clone();
            let target = block_device_link_target(&snap, idx, "../../");
            push_class_node(
                &mut snap,
                class_name,
                sysfs_name,
                SysClassNodeKind::Symlink { target },
            );
        }
        for iface in net::stack().snapshot_interfaces() {
            push_class_node(
                &mut snap,
                SYSFS_NET_CLASS,
                iface.name,
                SysClassNodeKind::NetInterface {
                    iface_id: iface.id.raw(),
                },
            );
        }
        snap
    }
}

// ─── 动态 inode 辅助 ─────────────────────────────────────────

fn block_dev_ino(name: &str) -> u64 {
    sysfs_dynamic_ino(SysfsKey::block_device(name))
}
fn block_dev_slot_ino(name: &str, slot: u64) -> u64 {
    sysfs_dynamic_ino(SysfsKey::block_device_slot(name, slot))
}
fn block_queue_slot_ino(name: &str, slot: u64) -> u64 {
    sysfs_dynamic_ino(SysfsKey::block_queue_slot(name, slot))
}
fn device_ino(class_name: &str, rdev: DevId) -> u64 {
    sysfs_dynamic_ino(SysfsKey::device(class_name, rdev))
}
fn device_slot_ino(class_name: &str, rdev: DevId, slot: u64) -> u64 {
    sysfs_dynamic_ino(SysfsKey::device_slot(class_name, rdev, slot))
}
fn device_power_ino(class_name: &str, rdev: DevId, slot: u64) -> u64 {
    sysfs_dynamic_ino(SysfsKey::device_power_slot(class_name, rdev, slot))
}
fn virtual_class_ino(class_name: &str) -> u64 {
    sysfs_dynamic_ino(SysfsKey::virtual_class(class_name))
}
fn virtual_device_ino(class_name: &str, rdev: DevId) -> u64 {
    sysfs_dynamic_ino(SysfsKey::virtual_device(class_name, rdev))
}
fn virtual_device_slot_ino(class_name: &str, rdev: DevId, slot: u64) -> u64 {
    sysfs_dynamic_ino(SysfsKey::virtual_device_slot(class_name, rdev, slot))
}
fn virtual_device_power_ino(class_name: &str, rdev: DevId, slot: u64) -> u64 {
    sysfs_dynamic_ino(SysfsKey::virtual_device_power_slot(class_name, rdev, slot))
}
fn pnp_bus_ino(bus: &str) -> u64 {
    sysfs_dynamic_ino(SysfsKey::pnp_bus(bus))
}
fn pnp_device_ino(bus: &str, name: &str) -> u64 {
    sysfs_dynamic_ino(SysfsKey::pnp_device(bus, name))
}
fn pnp_device_slot_ino(bus: &str, name: &str, slot: u64) -> u64 {
    sysfs_dynamic_ino(SysfsKey::pnp_device_slot(bus, name, slot))
}
fn bus_class_ino(bus: &str) -> u64 {
    sysfs_dynamic_ino(SysfsKey::bus_class(bus))
}
fn bus_class_devices_ino(bus: &str) -> u64 {
    sysfs_dynamic_ino(SysfsKey::bus_class_devices(bus))
}
fn bus_class_device_link_ino(bus: &str, name: &str) -> u64 {
    sysfs_dynamic_ino(SysfsKey::bus_class_device_link(bus, name))
}
fn class_dir_ino(class_name: &str) -> u64 {
    sysfs_dynamic_ino(SysfsKey::class_dir(class_name))
}
fn class_node_ino(class_name: &str, name: &str) -> u64 {
    sysfs_dynamic_ino(SysfsKey::class_node(class_name, name))
}
fn dev_block_link_ino(rdev: DevId) -> u64 {
    sysfs_dynamic_ino(SysfsKey::dev_block_link(rdev))
}
fn dev_char_dir_ino(rdev: DevId) -> u64 {
    sysfs_dynamic_ino(SysfsKey::dev_char_dir(rdev))
}
fn dev_char_inner_ino(rdev: DevId, slot: u64) -> u64 {
    sysfs_dynamic_ino(SysfsKey::dev_char_inner(rdev, slot))
}
fn cpu_ino(cpu_id: usize) -> u64 {
    CPU_BASE + (cpu_id as u64) * CPU_SLOTS
}
fn cpu_slot_ino(cpu_id: usize, slot: u64) -> u64 {
    cpu_ino(cpu_id) + slot
}
fn cpu_topology_slot_ino(cpu_id: usize, slot: u64) -> u64 {
    CPU_TOPOLOGY_BASE + (cpu_id as u64) * CPU_TOPOLOGY_SLOTS + slot
}

// ─── 文件 kind 枚举 ──────────────────────────────────────────

#[derive(Clone, Copy)]
enum BlockDevSlot {
    Size,
    Ro,
    Removable,
    Dev,
    Range,
    QueueDir,
    Holders,
    Stat,
    Inflight,
    Periodic,
}

impl BlockDevSlot {
    fn to_u64(self) -> u64 {
        match self {
            Self::Size => 0,
            Self::Ro => 1,
            Self::Removable => 2,
            Self::Dev => 3,
            Self::Range => 4,
            Self::QueueDir => 5,
            Self::Holders => 6,
            Self::Stat => 7,
            Self::Inflight => 8,
            Self::Periodic => 9,
        }
    }
}

#[derive(Clone, Copy)]
enum BlockQueueSlot {
    Lbs,
    Pbs,
    Rotational,
    NrRequests,
    HwSectorSize,
    DiscardZeroes,
}

impl BlockQueueSlot {
    fn to_u64(self) -> u64 {
        match self {
            Self::Lbs => 0,
            Self::Pbs => 1,
            Self::Rotational => 2,
            Self::NrRequests => 3,
            Self::HwSectorSize => 4,
            Self::DiscardZeroes => 5,
        }
    }
}

// ── /sys/class/net/<name>/ 属性槽位 ──────────────────────────────────

#[derive(Clone, Copy)]
enum NetDevSlot {
    Type,
    Address,
    Mtu,
    Flags,
    IfIndex,
    TxQueueLen,
    Carrier,
    Operstate,
    StatisticsRxBytes,
    StatisticsTxBytes,
    StatisticsRxPackets,
    StatisticsTxPackets,
    StatisticsRxDropped,
    StatisticsTxDropped,
    StatisticsRxErrors,
    StatisticsTxErrors,
}

impl NetDevSlot {
    fn to_u64(self) -> u64 {
        match self {
            Self::Type => 0,
            Self::Address => 1,
            Self::Mtu => 2,
            Self::Flags => 3,
            Self::IfIndex => 4,
            Self::TxQueueLen => 5,
            Self::Carrier => 6,
            Self::Operstate => 7,
            Self::StatisticsRxBytes => 8,
            Self::StatisticsTxBytes => 9,
            Self::StatisticsRxPackets => 10,
            Self::StatisticsTxPackets => 11,
            Self::StatisticsRxDropped => 12,
            Self::StatisticsTxDropped => 13,
            Self::StatisticsRxErrors => 14,
            Self::StatisticsTxErrors => 15,
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Address => "address",
            Self::Mtu => "mtu",
            Self::Flags => "flags",
            Self::IfIndex => "ifindex",
            Self::TxQueueLen => "tx_queue_len",
            Self::Carrier => "carrier",
            Self::Operstate => "operstate",
            Self::StatisticsRxBytes => "rx_bytes",
            Self::StatisticsTxBytes => "tx_bytes",
            Self::StatisticsRxPackets => "rx_packets",
            Self::StatisticsTxPackets => "tx_packets",
            Self::StatisticsRxDropped => "rx_dropped",
            Self::StatisticsTxDropped => "tx_dropped",
            Self::StatisticsRxErrors => "rx_errors",
            Self::StatisticsTxErrors => "tx_errors",
        }
    }

    const ALL: &'static [Self] = &[
        Self::Type,
        Self::Address,
        Self::Mtu,
        Self::Flags,
        Self::IfIndex,
        Self::TxQueueLen,
        Self::Carrier,
        Self::Operstate,
        Self::StatisticsRxBytes,
        Self::StatisticsTxBytes,
        Self::StatisticsRxPackets,
        Self::StatisticsTxPackets,
        Self::StatisticsRxDropped,
        Self::StatisticsTxDropped,
        Self::StatisticsRxErrors,
        Self::StatisticsTxErrors,
    ];
}

fn netdev_slot_by_name(name: &str) -> Option<NetDevSlot> {
    NetDevSlot::ALL
        .iter()
        .find(|s| s.file_name() == name)
        .copied()
}

#[derive(Clone, Copy)]
enum NetDevStatsSlot {
    RxBytes,
    TxBytes,
    RxPackets,
    TxPackets,
    RxDropped,
    TxDropped,
    RxErrors,
    TxErrors,
}

impl NetDevStatsSlot {
    fn to_u64(self) -> u64 {
        match self {
            Self::RxBytes => 0,
            Self::TxBytes => 1,
            Self::RxPackets => 2,
            Self::TxPackets => 3,
            Self::RxDropped => 4,
            Self::TxDropped => 5,
            Self::RxErrors => 6,
            Self::TxErrors => 7,
        }
    }
    fn to_netdev_slot(self) -> NetDevSlot {
        match self {
            Self::RxBytes => NetDevSlot::StatisticsRxBytes,
            Self::TxBytes => NetDevSlot::StatisticsTxBytes,
            Self::RxPackets => NetDevSlot::StatisticsRxPackets,
            Self::TxPackets => NetDevSlot::StatisticsTxPackets,
            Self::RxDropped => NetDevSlot::StatisticsRxDropped,
            Self::TxDropped => NetDevSlot::StatisticsTxDropped,
            Self::RxErrors => NetDevSlot::StatisticsRxErrors,
            Self::TxErrors => NetDevSlot::StatisticsTxErrors,
        }
    }
    fn file_name(self) -> &'static str {
        self.to_netdev_slot().file_name()
    }
    const ALL: &'static [Self] = &[
        Self::RxBytes,
        Self::TxBytes,
        Self::RxPackets,
        Self::TxPackets,
        Self::RxDropped,
        Self::TxDropped,
        Self::RxErrors,
        Self::TxErrors,
    ];
}

fn netdev_stats_slot_by_name(name: &str) -> Option<NetDevStatsSlot> {
    NetDevStatsSlot::ALL
        .iter()
        .find(|s| s.file_name() == name)
        .copied()
}

fn class_net_iface_ino(iface_id: u32) -> u64 {
    sysfs_dynamic_ino(SysfsKey::net_iface(iface_id))
}

fn class_net_iface_slot_ino(iface_id: u32, slot: u64) -> u64 {
    sysfs_dynamic_ino(SysfsKey::net_iface_slot(iface_id, slot))
}

fn class_net_stats_ino(iface_id: u32) -> u64 {
    sysfs_dynamic_ino(SysfsKey::net_stats(iface_id))
}

fn class_net_stats_slot_ino(iface_id: u32, slot: u64) -> u64 {
    sysfs_dynamic_ino(SysfsKey::net_stats_slot(iface_id, slot))
}

fn render_netdev_file(iface: &net::stack::InterfaceSnapshot, slot: NetDevSlot) -> String {
    use alloc::fmt::Write;
    let mut s = String::new();
    match slot {
        NetDevSlot::Type => {
            // 当前网络设备 snapshot 尚未携带链路层类型；用户视图策略给出以太网
            // 默认值，后续 typed capability 可覆盖该字段。
            let _ = writeln!(s, "{}", SYSFS_USER_VIEW_POLICY.net_link_type_ether);
        }
        NetDevSlot::Address => {
            let mac = iface.mac;
            let _ = writeln!(
                s,
                "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
            );
        }
        NetDevSlot::Mtu => {
            let _ = writeln!(s, "{}", iface.mtu);
        }
        NetDevSlot::Flags => {
            let _ = writeln!(s, "0x{:x}", iface.flags);
        }
        NetDevSlot::IfIndex => {
            let _ = writeln!(s, "{}", SYSFS_USER_VIEW_POLICY.net_ifindex(iface.id.raw()));
        }
        NetDevSlot::TxQueueLen => {
            let _ = writeln!(s, "{}", SYSFS_USER_VIEW_POLICY.net_tx_queue_len);
        }
        NetDevSlot::Carrier => {
            let carrier = if iface.flags & net::stack::IFF_RUNNING != 0 {
                "1"
            } else {
                "0"
            };
            let _ = writeln!(s, "{}", carrier);
        }
        NetDevSlot::Operstate => {
            let state = if iface.flags & net::stack::IFF_UP != 0 {
                "up"
            } else {
                "down"
            };
            let _ = writeln!(s, "{}", state);
        }
        NetDevSlot::StatisticsRxBytes => {
            let _ = writeln!(s, "{}", iface.stats.rx_bytes);
        }
        NetDevSlot::StatisticsTxBytes => {
            let _ = writeln!(s, "{}", iface.stats.tx_bytes);
        }
        NetDevSlot::StatisticsRxPackets => {
            let _ = writeln!(s, "{}", iface.stats.rx_packets);
        }
        NetDevSlot::StatisticsTxPackets => {
            let _ = writeln!(s, "{}", iface.stats.tx_packets);
        }
        NetDevSlot::StatisticsRxDropped => {
            let _ = writeln!(s, "{}", iface.stats.rx_dropped);
        }
        NetDevSlot::StatisticsTxDropped => {
            let _ = writeln!(s, "{}", iface.stats.tx_dropped);
        }
        NetDevSlot::StatisticsRxErrors => {
            let _ = writeln!(s, "{}", iface.stats.rx_errors);
        }
        NetDevSlot::StatisticsTxErrors => {
            let _ = writeln!(s, "{}", iface.stats.tx_errors);
        }
    }
    s
}

#[derive(Clone, Copy)]
enum DeviceSlot {
    Name,
    Dev,
    Subsystem,
    PwrDir,
}

impl DeviceSlot {
    fn to_u64(self) -> u64 {
        match self {
            Self::Name => 0,
            Self::Dev => 1,
            Self::Subsystem => 2,
            Self::PwrDir => 3,
        }
    }
}

#[derive(Clone, Copy)]
enum PnpDeviceSlot {
    Name,
    Id,
    Bus,
    State,
    Driver,
    Parent,
    Children,
    Functions,
    Resources,
    DeferredDependency,
}

impl PnpDeviceSlot {
    const ALL: &'static [Self] = &[
        Self::Name,
        Self::Id,
        Self::Bus,
        Self::State,
        Self::Driver,
        Self::Parent,
        Self::Children,
        Self::Functions,
        Self::Resources,
        Self::DeferredDependency,
    ];

    fn to_u64(self) -> u64 {
        match self {
            Self::Name => 0,
            Self::Id => 1,
            Self::Bus => 2,
            Self::State => 3,
            Self::Driver => 4,
            Self::Parent => 5,
            Self::Children => 6,
            Self::Functions => 7,
            Self::Resources => 8,
            Self::DeferredDependency => 9,
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Id => "id",
            Self::Bus => "bus",
            Self::State => "state",
            Self::Driver => "driver",
            Self::Parent => "parent",
            Self::Children => "children",
            Self::Functions => "functions",
            Self::Resources => "resources",
            Self::DeferredDependency => "deferred_dependency",
        }
    }
}

fn pnp_device_slot_by_name(name: &str) -> Option<PnpDeviceSlot> {
    PnpDeviceSlot::ALL
        .iter()
        .find(|slot| slot.file_name() == name)
        .copied()
}

/// `/sys/devices/<dev>/power` 的通用兼容属性。
///
/// 当前 dev core 还没有 per-device runtime PM 控制器，也没有 wakeup capability
/// 模型。sysfs 只能暴露设备注册表能证明的事实：进入 `/sys/devices` 的设备已
/// 完成 probe 且处于可访问状态；runtime policy 由内核固定保持打开；未声明
/// wakeup 能力。后续如果 dev core 增加电源管理 trait，只需要替换这里的快照
/// 渲染，不需要改目录/slot 分配。
#[derive(Clone, Copy)]
enum DevicePowerSlot {
    RuntimeStatus,
    Control,
    Wakeup,
}

impl DevicePowerSlot {
    const ALL: &'static [Self] = &[Self::RuntimeStatus, Self::Control, Self::Wakeup];

    fn to_u64(self) -> u64 {
        match self {
            Self::RuntimeStatus => 0,
            Self::Control => 1,
            Self::Wakeup => 2,
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::RuntimeStatus => "runtime_status",
            Self::Control => "control",
            Self::Wakeup => "wakeup",
        }
    }
}

fn device_power_slot_by_name(name: &str) -> Option<DevicePowerSlot> {
    DevicePowerSlot::ALL
        .iter()
        .find(|slot| slot.file_name() == name)
        .copied()
}

#[derive(Clone, Copy)]
enum DevCharInnerSlot {
    Dev,
    DeviceLink,
    SubsystemLink,
    Uevent,
}

impl DevCharInnerSlot {
    fn to_u64(self) -> u64 {
        match self {
            Self::Dev => 0,
            Self::DeviceLink => 1,
            Self::SubsystemLink => 2,
            Self::Uevent => 3,
        }
    }
}

#[derive(Clone, Copy)]
enum CpuSlot {
    TopoDir,
    Online,
    Possible,
    Present,
}

impl CpuSlot {
    fn to_u64(self) -> u64 {
        match self {
            Self::TopoDir => 0,
            Self::Online => 1,
            Self::Possible => 2,
            Self::Present => 3,
        }
    }
}

#[derive(Clone, Copy)]
enum CpuTopologySlot {
    PhysicalPackageId,
    CoreId,
    ThreadId,
    CoreSiblingsList,
    ThreadSiblingsList,
}

impl CpuTopologySlot {
    const ALL: &'static [Self] = &[
        Self::PhysicalPackageId,
        Self::CoreId,
        Self::ThreadId,
        Self::CoreSiblingsList,
        Self::ThreadSiblingsList,
    ];

    fn to_u64(self) -> u64 {
        match self {
            Self::PhysicalPackageId => 0,
            Self::CoreId => 1,
            Self::ThreadId => 2,
            Self::CoreSiblingsList => 3,
            Self::ThreadSiblingsList => 4,
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::PhysicalPackageId => "physical_package_id",
            Self::CoreId => "core_id",
            Self::ThreadId => "thread_id",
            Self::CoreSiblingsList => "core_siblings_list",
            Self::ThreadSiblingsList => "thread_siblings_list",
        }
    }
}

fn cpu_topology_slot_by_name(name: &str) -> Option<CpuTopologySlot> {
    CpuTopologySlot::ALL
        .iter()
        .find(|slot| slot.file_name() == name)
        .copied()
}

#[derive(Clone, Copy)]
enum SysRegFile {
    BlockDev {
        idx: usize,
        slot: BlockDevSlot,
    },
    BlockQueue {
        idx: usize,
        slot: BlockQueueSlot,
    },
    Device {
        idx: usize,
        slot: DeviceSlot,
    },
    DevicePower {
        idx: usize,
        slot: DevicePowerSlot,
    },
    VirtualDevice {
        idx: usize,
        slot: DeviceSlot,
    },
    VirtualDevicePower {
        idx: usize,
        slot: DevicePowerSlot,
    },
    PnpDevice {
        idx: usize,
        slot: PnpDeviceSlot,
    },
    DevCharInner {
        idx: usize,
        slot: DevCharInnerSlot,
    },
    Cpu {
        cpu_id: usize,
        slot: CpuSlot,
    },
    CpuTopology {
        cpu_id: usize,
        slot: CpuTopologySlot,
    },
    CpuOnline,
    CpuPossible,
    CpuPresent,
    Hostname,
    Ostype,
    Osrelease,
    Version,
    Cmdline,
    DeviceFunctions,
    NetDev {
        iface_id: u32,
        slot: NetDevSlot,
    },
}

// ─── 内容渲染 ────────────────────────────────────────────────

fn render_block_dev_file(snap: &SysSnapshot, idx: usize, slot: BlockDevSlot) -> String {
    let dev = &snap.blocks[idx].dev;
    let geom = dev.geometry();
    let features = dev.features();
    match slot {
        BlockDevSlot::Size => {
            let sectors = geom
                .block_count()
                .map(|c| c * (geom.logical_block_size().get() as u64) / 512)
                .unwrap_or(0);
            format!("{}\n", sectors)
        }
        BlockDevSlot::Ro => {
            if features.contains(crate::dev::block::BlockFeatures::READ_ONLY) {
                "1\n".into()
            } else {
                "0\n".into()
            }
        }
        BlockDevSlot::Removable => {
            if dev.attributes().removable() {
                "1\n".into()
            } else {
                "0\n".into()
            }
        }
        BlockDevSlot::Dev => format_rdev(snap.blocks[idx].rdev),
        BlockDevSlot::Range => "1\n".into(),
        BlockDevSlot::Holders => String::new(),
        BlockDevSlot::Stat => {
            let stats = dev.io_stats();
            // 这里输出通用块层维护的兼容 diskstats 字段。合并计数和队列总耗时
            // 当前没有独立数据源，保持为 0；完成数、扇区数、inflight 和操作耗时
            // 均来自 BlockDevice 的 BIO 统计。
            format!(
                "{} 0 {} {} {} 0 {} {} {} 0 0 {} 0 {} {} {} {}\n",
                stats.read_ios,
                stats.read_sectors,
                ns_to_ms(stats.read_time_ns),
                stats.write_ios,
                stats.write_sectors,
                ns_to_ms(stats.write_time_ns),
                stats.read_inflight.saturating_add(stats.write_inflight),
                stats.discard_ios,
                stats.discard_sectors,
                ns_to_ms(stats.discard_time_ns),
                stats.flush_ios,
                ns_to_ms(stats.flush_time_ns),
            )
        }
        BlockDevSlot::Inflight => {
            let stats = dev.io_stats();
            format!("{} {}\n", stats.read_inflight, stats.write_inflight)
        }
        BlockDevSlot::Periodic => String::new(),
        BlockDevSlot::QueueDir => String::new(),
    }
}

fn render_block_queue_file(snap: &SysSnapshot, idx: usize, slot: BlockQueueSlot) -> String {
    let dev = &snap.blocks[idx].dev;
    let geom = dev.geometry();
    let features = dev.features();
    match slot {
        BlockQueueSlot::Lbs => format!("{}\n", geom.logical_block_size().get()),
        BlockQueueSlot::Pbs => format!("{}\n", geom.physical_block_size().get()),
        BlockQueueSlot::Rotational => {
            if dev.attributes().rotational() {
                "1\n".into()
            } else {
                "0\n".into()
            }
        }
        BlockQueueSlot::NrRequests => {
            // 没有真实队列深度的设备使用 sysfs 用户视图默认值；VirtIO 等驱动会填实际协商值。
            let depth = dev
                .attributes()
                .queue_depth()
                .map(|n| n.get())
                .unwrap_or(SYSFS_USER_VIEW_POLICY.block_nr_requests);
            format!("{}\n", depth)
        }
        BlockQueueSlot::HwSectorSize => format!("{}\n", geom.logical_block_size().get()),
        BlockQueueSlot::DiscardZeroes => {
            if features.contains(crate::dev::block::BlockFeatures::WRITE_ZEROES) {
                "1\n".into()
            } else {
                "0\n".into()
            }
        }
    }
}

fn render_device_file(snap: &SysSnapshot, idx: usize, slot: DeviceSlot) -> String {
    let dev = &snap.devices[idx];
    match slot {
        DeviceSlot::Name => format!("{}\n", dev.sysfs_name),
        DeviceSlot::Dev => format_rdev(dev.rdev),
        DeviceSlot::Subsystem | DeviceSlot::PwrDir => String::new(),
    }
}

fn render_device_power_file(_snap: &SysSnapshot, _idx: usize, slot: DevicePowerSlot) -> String {
    match slot {
        // 通用设备模型暂未暴露 runtime PM typed state 时，统一从用户视图策略渲染。
        DevicePowerSlot::RuntimeStatus => {
            format!("{}\n", SYSFS_USER_VIEW_POLICY.power_runtime_status)
        }
        DevicePowerSlot::Control => format!("{}\n", SYSFS_USER_VIEW_POLICY.power_control),
        DevicePowerSlot::Wakeup => format!("{}\n", SYSFS_USER_VIEW_POLICY.power_wakeup),
    }
}

fn render_virtual_device_file(snap: &SysSnapshot, idx: usize, slot: DeviceSlot) -> String {
    let dev = &snap.virtual_devices[idx];
    match slot {
        DeviceSlot::Name => format!("{}\n", dev.sysfs_name),
        DeviceSlot::Dev => format_rdev(dev.rdev),
        DeviceSlot::Subsystem | DeviceSlot::PwrDir => String::new(),
    }
}

fn render_pnp_device_file(snap: &SysSnapshot, idx: usize, slot: PnpDeviceSlot) -> String {
    let dev = &snap.pnp_devices[idx];
    match slot {
        PnpDeviceSlot::Name => format!("{}\n", dev.name),
        PnpDeviceSlot::Id => format!("{}\n", dev.id),
        PnpDeviceSlot::Bus => format!("{}\n", dev.bus_type),
        PnpDeviceSlot::State => format!("{}\n", dev.state),
        PnpDeviceSlot::Driver => dev
            .driver
            .map(|name| format!("{name}\n"))
            .unwrap_or_default(),
        PnpDeviceSlot::Parent => dev
            .parent
            .as_ref()
            .map(|name| format!("{name}\n"))
            .unwrap_or_default(),
        PnpDeviceSlot::Children => format!("{}\n", dev.child_count),
        PnpDeviceSlot::Functions => {
            let mut out = String::new();
            for function in &dev.functions {
                let _ = core::fmt::Write::write_fmt(
                    &mut out,
                    format_args!("{}:{}\n", function.class_id().as_str(), function.dev_name()),
                );
            }
            out
        }
        PnpDeviceSlot::Resources => {
            let mut out = String::new();
            for resource in &dev.resources {
                let _ = core::fmt::Write::write_fmt(
                    &mut out,
                    format_args!(
                        "{}:{}\n",
                        pnp_resource_kind_name(resource.kind),
                        resource.label
                    ),
                );
            }
            out
        }
        PnpDeviceSlot::DeferredDependency => dev
            .deferred_dependency
            .map(pnp_dependency_name)
            .map(|dependency| format!("{dependency}\n"))
            .unwrap_or_default(),
    }
}

fn render_dev_char_inner(snap: &SysSnapshot, idx: usize, slot: DevCharInnerSlot) -> String {
    match slot {
        DevCharInnerSlot::Dev => format_rdev(snap.char_nodes[idx].rdev),
        DevCharInnerSlot::DeviceLink => String::new(), // symlink，不渲染
        DevCharInnerSlot::SubsystemLink => String::new(),
        DevCharInnerSlot::Uevent => {
            let c = &snap.char_nodes[idx];
            format!(
                "MAJOR={}\nMINOR={}\nDEVNAME={}\n",
                c.rdev.major, c.rdev.minor, c.devtmpfs_name
            )
        }
    }
}

fn format_rdev(rdev: DevId) -> String {
    format!("{}:{}\n", rdev.major, rdev.minor)
}

fn ns_to_ms(ns: u64) -> u64 {
    ns / 1_000_000
}

fn rdev_name(rdev: DevId) -> String {
    format!("{}:{}", rdev.major, rdev.minor)
}

fn has_sysfs_backing_device(snap: &SysSnapshot, class_name: &'static str, rdev: DevId) -> bool {
    snap.devices
        .iter()
        .any(|dev| dev.class_name == class_name && dev.rdev == rdev)
}

fn char_device_link_target(snap: &SysSnapshot, idx: usize, root_prefix: &str) -> String {
    let node = &snap.char_nodes[idx];
    if has_sysfs_backing_device(snap, node.class_name, node.rdev) {
        format!("{}devices/{}", root_prefix, node.sysfs_name)
    } else {
        format!(
            "{}devices/virtual/{}/{}",
            root_prefix, node.class_name, node.sysfs_name
        )
    }
}

fn block_device_link_target(snap: &SysSnapshot, idx: usize, root_prefix: &str) -> String {
    let node = &snap.block_nodes[idx];
    if has_sysfs_backing_device(snap, node.class_name, node.rdev) {
        format!("{}block/{}", root_prefix, node.sysfs_name)
    } else {
        format!(
            "{}devices/virtual/{}/{}",
            root_prefix, node.class_name, node.sysfs_name
        )
    }
}

fn parse_rdev_name(name: &str) -> Option<DevId> {
    let (major, minor) = name.split_once(':')?;
    Some(DevId::new(major.parse().ok()?, minor.parse().ok()?))
}

fn push_cpu_range(out: &mut String, start: usize, end: usize) {
    if !out.is_empty() {
        out.push(',');
    }
    if start == end {
        out.push_str(&format!("{}", start));
    } else {
        out.push_str(&format!("{}-{}", start, end));
    }
}

fn format_cpu_mask_range(mask: u64) -> String {
    let mut out = String::new();
    let mut iter = CpuMaskIter::new(mask);
    while let Some(start) = iter.next() {
        let mut end = start;
        while let Some(next) = iter.peek() {
            if next != end.saturating_add(1) {
                break;
            }
            end = iter.next().unwrap_or(end);
        }
        push_cpu_range(&mut out, start, end);
    }
    out.push('\n');
    out
}

struct CpuMaskIter {
    mask: u64,
    next: usize,
}

impl CpuMaskIter {
    const fn new(mask: u64) -> Self {
        Self { mask, next: 0 }
    }

    fn peek(&self) -> Option<usize> {
        let mut cpu = self.next;
        while cpu < u64::BITS as usize {
            if self.mask & (1u64 << cpu) != 0 {
                return Some(cpu);
            }
            cpu += 1;
        }
        None
    }
}

impl Iterator for CpuMaskIter {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        let cpu = self.peek()?;
        self.next = cpu.saturating_add(1);
        Some(cpu)
    }
}

#[derive(Clone, Copy)]
struct CpuTopologyView {
    package_id: u32,
    core_id: u32,
    thread_id: u32,
}

fn cpu_topology_view(cpu_id: usize, entries: &[cpu::CpuTopologyEntry]) -> Option<CpuTopologyView> {
    let logical_id = u32::try_from(cpu_id).ok()?;
    let entry = entries.iter().find(|entry| entry.logical_id == logical_id);

    // 固件可能只描述部分层级。这里做的是通用拓扑归一化：没有 socket 层级时
    // 说明当前快照无法区分 package，统一归入 0；没有 core/thread 层级时，
    // 使用 logical CPU 自身作为 core，thread 使用 0，保持 sibling 计算稳定。
    Some(CpuTopologyView {
        package_id: entry.and_then(|entry| entry.socket_id).unwrap_or(0),
        core_id: entry.and_then(|entry| entry.core_id).unwrap_or(logical_id),
        thread_id: entry.and_then(|entry| entry.thread_id).unwrap_or(0),
    })
}

fn cpu_topology_sibling_mask(
    cpu_id: usize,
    entries: &[cpu::CpuTopologyEntry],
    same_group: fn(CpuTopologyView, CpuTopologyView) -> bool,
) -> u64 {
    let Some(base) = cpu_topology_view(cpu_id, entries) else {
        return 0;
    };
    let mut mask = 0u64;
    let online = online_cpu_mask();
    let mut candidate = 0usize;
    while candidate < 64 {
        let bit = 1u64 << candidate;
        if online & bit != 0
            && let Some(view) = cpu_topology_view(candidate, entries)
            && same_group(base, view)
        {
            mask |= bit;
        }
        candidate += 1;
    }
    mask
}

fn render_cpu_file(_snap: &SysSnapshot, _cpu_id: usize, slot: CpuSlot) -> String {
    match slot {
        CpuSlot::TopoDir => String::new(),
        CpuSlot::Online => "1\n".into(),
        CpuSlot::Possible => "1\n".into(),
        CpuSlot::Present => "1\n".into(),
    }
}

fn render_cpu_topology_file(_snap: &SysSnapshot, cpu_id: usize, slot: CpuTopologySlot) -> String {
    let entries = cpu::snapshot_topology();
    let Some(view) = cpu_topology_view(cpu_id, &entries) else {
        return String::new();
    };
    match slot {
        CpuTopologySlot::PhysicalPackageId => format!("{}\n", view.package_id),
        CpuTopologySlot::CoreId => format!("{}\n", view.core_id),
        CpuTopologySlot::ThreadId => format!("{}\n", view.thread_id),
        CpuTopologySlot::CoreSiblingsList => {
            let mask =
                cpu_topology_sibling_mask(cpu_id, &entries, |a, b| a.package_id == b.package_id);
            format_cpu_mask_range(mask)
        }
        CpuTopologySlot::ThreadSiblingsList => {
            let mask = cpu_topology_sibling_mask(cpu_id, &entries, |a, b| {
                a.package_id == b.package_id && a.core_id == b.core_id
            });
            format_cpu_mask_range(mask)
        }
    }
}

fn render_kernel_cmdline() -> String {
    let Some(bytes) = crate::start::start_cmdline() else {
        return String::new();
    };
    let text = crate::cmdline::Cmdline::new(bytes).as_str();
    if text.is_empty() {
        String::new()
    } else {
        // 启动命令行来自架构加载器保存的稳定快照；sysfs 只负责展示，不重新解析策略。
        format!("{text}\n")
    }
}

fn render_reg_file(snap: &SysSnapshot, kind: SysRegFile) -> String {
    match kind {
        SysRegFile::BlockDev { idx, slot } => render_block_dev_file(snap, idx, slot),
        SysRegFile::BlockQueue { idx, slot } => render_block_queue_file(snap, idx, slot),
        SysRegFile::Device { idx, slot } => render_device_file(snap, idx, slot),
        SysRegFile::DevicePower { idx, slot } => render_device_power_file(snap, idx, slot),
        SysRegFile::VirtualDevice { idx, slot } => render_virtual_device_file(snap, idx, slot),
        SysRegFile::VirtualDevicePower { idx, slot } => render_device_power_file(snap, idx, slot),
        SysRegFile::PnpDevice { idx, slot } => render_pnp_device_file(snap, idx, slot),
        SysRegFile::DevCharInner { idx, slot } => render_dev_char_inner(snap, idx, slot),
        SysRegFile::Cpu { cpu_id, slot } => render_cpu_file(snap, cpu_id, slot),
        SysRegFile::CpuTopology { cpu_id, slot } => render_cpu_topology_file(snap, cpu_id, slot),
        SysRegFile::CpuOnline => format_cpu_mask_range(online_cpu_mask()),
        SysRegFile::CpuPossible => format_cpu_mask_range(supported_cpu_mask()),
        // 当前内核尚未区分“已发现但离线”的 CPU；present 先反映在线 CPU 集合。
        SysRegFile::CpuPresent => format_cpu_mask_range(online_cpu_mask()),
        SysRegFile::Hostname => "mygo\n".into(),
        SysRegFile::Ostype => "MyGO\n".into(),
        SysRegFile::Osrelease => env!("CARGO_PKG_VERSION").to_string() + "\n",
        SysRegFile::Version => format!("mygo {} (mygo-build)\n", env!("CARGO_PKG_VERSION")),
        SysRegFile::Cmdline => render_kernel_cmdline(),
        SysRegFile::DeviceFunctions => {
            let mut out = String::new();
            append_function_projection_diagnostics(&mut out);
            out
        }
        SysRegFile::NetDev { iface_id, slot } => {
            if let Some(iface) = net::stack()
                .snapshot_interfaces()
                .into_iter()
                .find(|i| i.id.raw() == iface_id)
            {
                render_netdev_file(&iface, slot)
            } else {
                String::new()
            }
        }
    }
}

// ─── Driver / Superblock ─────────────────────────────────────

pub struct SysFsDriver;

impl FsDriver for SysFsDriver {
    fn name(&self) -> &'static str {
        "sysfs"
    }
    fn flags(&self) -> FsDriverFlags {
        FsDriverFlags::NODEV
            .with(FsDriverFlags::SINGLE)
            .with(FsDriverFlags::RDONLY)
    }

    fn mount(&self, _dev: Option<&str>, _data: &str) -> VfsResult<Arc<Superblock>> {
        let fs_id = FsId::new(SYSFS_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));
        Ok(Superblock::new(|weak_sb| {
            let snap = Arc::new(SysSnapshot::collect());
            let root_inode = build_root_inode(fs_id, &weak_sb, Arc::clone(&snap));
            let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));
            Superblock {
                fs_type: "sysfs",
                fs_id,
                dev_id: None,
                block_size: 4096,
                name_max: 255,
                root_inode,
                root_dentry,
                inode_cache: vfs::superblock::InodeCache::new(),
                ops: Box::new(SysSuperblockOps),
                self_weak: weak_sb,
            }
        }))
    }

    fn kill_sb(&self, _sb: Arc<Superblock>) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct SysSuperblockOps;
impl SuperblockOps for SysSuperblockOps {
    fn alloc_inode(&self, _: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::ReadOnlyFilesystem)
    }
    fn write_inode(&self, _: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }
    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Ok(FsStat {
            fs_type: SYSFS_MAGIC,
            block_size: sb.block_size as u64,
            total_blocks: 0,
            free_blocks: 0,
            avail_blocks: 0,
            total_inodes: 0,
            free_inodes: 0,
            fs_id: sb.fs_id.raw(),
            name_max: sb.name_max,
        })
    }
    fn sync_fs(&self, _: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }
    fn remount(&self, _: &Arc<Superblock>, _: MountFlags) -> VfsResult<()> {
        Ok(())
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ─── File / Dir FileOps ─────────────────────────────────────

struct SysDirFile {
    snapshot: Vec<DirEntry>,
}
struct SysRegFileOps {
    kind: SysRegFile,
    snap: Arc<SysSnapshot>,
}

fn feed_dir_entries(
    snapshot: &[DirEntry],
    pos: u64,
    sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
) -> VfsResult<u64> {
    let start = core::cmp::min(pos as usize, snapshot.len());
    for (i, entry) in snapshot.iter().enumerate().skip(start) {
        if sink(entry.clone()).is_break() {
            // sink 返回 Break 表示当前条目未被用户缓冲区接收，下一次 getdents
            // 必须从同一个游标重试，不能提前跳过该目录项。
            return Ok(i as u64);
        }
    }
    Ok(snapshot.len() as u64)
}

impl FileOps for SysDirFile {
    fn read_at(&self, _: &mut [u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }
    fn readdir(
        &self,
        pos: u64,
        sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        feed_dir_entries(&self.snapshot, pos, sink)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, interest: PollEvents) -> PollEvents {
        // 目录枚举基于内存快照，可立即尝试读取目录项。
        PollEvents::READ_WRITE_READY.intersect(interest)
    }
    fn ioctl(&self, _: IoctlCmd, _: usize) -> Result<usize, Errno> {
        Err(errno::Errno::ENOTTY)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl FileOps for SysRegFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let s = render_reg_file(&self.snap, self.kind);
        let bytes = s.as_bytes();
        let total = bytes.len();
        let off = offset as usize;
        if off >= total {
            return Ok(0);
        }
        let n = core::cmp::min(buf.len(), total - off);
        buf[..n].copy_from_slice(&bytes[off..off + n]);
        Ok(n)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::ReadOnlyFilesystem)
    }
    fn readdir(&self, _: u64, _: &mut dyn FnMut(DirEntry) -> ControlFlow<()>) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, interest: PollEvents) -> PollEvents {
        // 属性内容由内核内存即时渲染，不依赖外部事件到达。
        PollEvents::READ_WRITE_READY.intersect(interest)
    }
    fn ioctl(&self, _: IoctlCmd, _: usize) -> Result<usize, Errno> {
        Err(errno::Errno::ENOTTY)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ─── 目录/文件 InodeOps 统一工厂宏 ───────────────────────────

/// 给定子 ino 与子文件 kind，构造对应文件/目录的 Inode（供目录 lookup 共用）。
fn build_child_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    snap: &Arc<SysSnapshot>,
    ino: u64,
    kind: SysRegFile,
) -> Option<Arc<Inode>> {
    let ops: Arc<dyn InodeOps + Send + Sync> = Arc::new(SysRegInodeOps {
        kind,
        snap: Arc::clone(snap),
    });
    Some(mk_inode(
        fs_id,
        weak_sb,
        ino,
        FileType::Regular,
        0o444,
        1,
        ops,
    ))
}

fn build_dir_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    snap: &Arc<SysSnapshot>,
    ino: u64,
    dir_kind: SysDirKind,
) -> Arc<Inode> {
    let ops: Arc<dyn InodeOps + Send + Sync> = Arc::new(SysDirInodeOps {
        kind: dir_kind,
        fs_id,
        weak_sb: weak_sb.clone(),
        snap: Arc::clone(snap),
    });
    mk_inode(fs_id, weak_sb, ino, FileType::Directory, 0o555, 2, ops)
}

fn build_link_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    ino: u64,
    target: String,
) -> Arc<Inode> {
    let ops: Arc<dyn InodeOps + Send + Sync> = Arc::new(SysLinkInodeOps { target });
    mk_inode(fs_id, weak_sb, ino, FileType::Symlink, 0o777, 1, ops)
}

// ─── 目录类型枚举 ───────────────────────────────────────────

#[derive(Clone)]
enum SysDirKind {
    Root,
    Block,
    BlockDev {
        name: String,
    },
    BlockQueue {
        name: String,
    },
    Devices,
    Device {
        class_name: &'static str,
        rdev: DevId,
    },
    DevicePower {
        class_name: &'static str,
        rdev: DevId,
    },
    DevicesVirtual,
    DevicesVirtualClass {
        class_name: &'static str,
    },
    VirtualDevice {
        class_name: &'static str,
        rdev: DevId,
    },
    VirtualDevicePower {
        class_name: &'static str,
        rdev: DevId,
    },
    DevicesPnp,
    DevicesPnpBus {
        bus: &'static str,
    },
    PnpDevice {
        bus: &'static str,
        name: String,
    },
    Dev,
    DevBlock,
    DevChar,
    DevCharInner {
        rdev: DevId,
    },
    Kernel,
    Fs,
    FsCgroup,
    Bus,
    BusClass {
        bus: &'static str,
    },
    BusClassDevices {
        bus: &'static str,
    },
    Class,
    ClassDir {
        class_name: &'static str,
    },
    ClassNetIface {
        iface_id: u32,
    },
    ClassNetStats {
        iface_id: u32,
    },
    Module,
    Power,
    Firmware,
    DevicesSystem,
    DevicesSystemCpu,
    Cpu {
        cpu_id: usize,
    },
    CpuTopology {
        cpu_id: usize,
    },
}

// ─── InodeOps ────────────────────────────────────────────────

struct SysRegInodeOps {
    kind: SysRegFile,
    snap: Arc<SysSnapshot>,
}
struct SysLinkInodeOps {
    target: String,
}
struct SysDirInodeOps {
    kind: SysDirKind,
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
    snap: Arc<SysSnapshot>,
}

impl InodeOps for SysRegInodeOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }
    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(SysRegFileOps {
            kind: self.kind,
            snap: Arc::clone(&self.snap),
        }))
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl InodeOps for SysLinkInodeOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }
    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Err(VfsError::NotFound)
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Ok(self.target.clone())
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl InodeOps for SysDirInodeOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        self.lookup_child(name)
    }
    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let current = SysDirInodeOps {
            kind: self.kind.clone(),
            fs_id: self.fs_id,
            weak_sb: self.weak_sb.clone(),
            snap: Arc::new(SysSnapshot::collect()),
        };
        Ok(Box::new(SysDirFile {
            snapshot: current.readdir_entries(),
        }))
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
impl SysDirInodeOps {
    fn lookup_child(&self, name: &str) -> VfsResult<Arc<Inode>> {
        let fs_id = self.fs_id;
        let weak_sb = &self.weak_sb;
        let current = Arc::new(SysSnapshot::collect());
        let snap = &current;
        let mk_reg = |ino: u64, kind: SysRegFile| -> VfsResult<Arc<Inode>> {
            build_child_inode(fs_id, weak_sb, snap, ino, kind).ok_or(VfsError::OutOfMemory)
        };
        let mk_dir = |ino: u64, k: SysDirKind| -> Arc<Inode> {
            build_dir_inode(fs_id, weak_sb, snap, ino, k)
        };
        let mk_link = |ino: u64, target: String| -> Arc<Inode> {
            build_link_inode(fs_id, weak_sb, ino, target)
        };

        match self.kind.clone() {
            SysDirKind::Root => match name {
                "block" => Ok(mk_dir(BLOCK_DIR_INO, SysDirKind::Block)),
                "devices" => Ok(mk_dir(DEVICES_DIR_INO, SysDirKind::Devices)),
                "dev" => Ok(mk_dir(DEV_DIR_INO, SysDirKind::Dev)),
                "kernel" => Ok(mk_dir(KERNEL_DIR_INO, SysDirKind::Kernel)),
                "fs" => Ok(mk_dir(FS_DIR_INO, SysDirKind::Fs)),
                "bus" => Ok(mk_dir(BUS_DIR_INO, SysDirKind::Bus)),
                "class" => Ok(mk_dir(CLASS_DIR_INO, SysDirKind::Class)),
                "module" => Ok(mk_dir(MODULE_DIR_INO, SysDirKind::Module)),
                "power" => Ok(mk_dir(POWER_DIR_INO, SysDirKind::Power)),
                "firmware" => Ok(mk_dir(FIRMWARE_DIR_INO, SysDirKind::Firmware)),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::Block => {
                if !snap.blocks.iter().any(|b| b.sysfs_name == name) {
                    return Err(VfsError::NotFound);
                }
                Ok(mk_dir(
                    block_dev_ino(name),
                    SysDirKind::BlockDev {
                        name: name.to_string(),
                    },
                ))
            }
            SysDirKind::BlockDev { name: block_name } => {
                let idx = snap
                    .blocks
                    .iter()
                    .position(|block| block.sysfs_name == block_name)
                    .ok_or(VfsError::NotFound)?;
                let slot = block_slot_by_name(name).ok_or(VfsError::NotFound)?;
                let ino = block_dev_slot_ino(&block_name, slot.to_u64());
                if matches!(slot, BlockDevSlot::QueueDir) {
                    Ok(mk_dir(ino, SysDirKind::BlockQueue { name: block_name }))
                } else {
                    mk_reg(ino, SysRegFile::BlockDev { idx, slot })
                }
            }
            SysDirKind::BlockQueue { name: block_name } => {
                let idx = snap
                    .blocks
                    .iter()
                    .position(|block| block.sysfs_name == block_name)
                    .ok_or(VfsError::NotFound)?;
                let slot = block_queue_slot_by_name(name).ok_or(VfsError::NotFound)?;
                let ino = block_queue_slot_ino(&block_name, slot.to_u64());
                mk_reg(ino, SysRegFile::BlockQueue { idx, slot })
            }
            SysDirKind::Devices => {
                if let Some(idx) = snap.devices.iter().position(|dev| dev.sysfs_name == name) {
                    let dev = &snap.devices[idx];
                    Ok(mk_dir(
                        device_ino(dev.class_name, dev.rdev),
                        SysDirKind::Device {
                            class_name: dev.class_name,
                            rdev: dev.rdev,
                        },
                    ))
                } else if name == "system" {
                    Ok(mk_dir(DEVICES_SYSTEM_INO, SysDirKind::DevicesSystem))
                } else if name == "virtual" {
                    Ok(mk_dir(DEVICES_VIRTUAL_INO, SysDirKind::DevicesVirtual))
                } else if name == "pnp" {
                    Ok(mk_dir(DEVICES_PNP_INO, SysDirKind::DevicesPnp))
                } else {
                    Err(VfsError::NotFound)
                }
            }
            SysDirKind::Device { class_name, rdev } => {
                let idx = snap
                    .devices
                    .iter()
                    .position(|dev| dev.class_name == class_name && dev.rdev == rdev)
                    .ok_or(VfsError::NotFound)?;
                let slot = device_slot_by_name(name).ok_or(VfsError::NotFound)?;
                let ino = device_slot_ino(class_name, rdev, slot.to_u64());
                if matches!(slot, DeviceSlot::PwrDir) {
                    Ok(mk_dir(ino, SysDirKind::DevicePower { class_name, rdev }))
                } else if matches!(slot, DeviceSlot::Subsystem) {
                    let target = format!("../class/{}", snap.devices[idx].class_name);
                    Ok(mk_link(ino, target))
                } else {
                    mk_reg(ino, SysRegFile::Device { idx, slot })
                }
            }
            SysDirKind::DevicePower { class_name, rdev } => {
                let idx = snap
                    .devices
                    .iter()
                    .position(|dev| dev.class_name == class_name && dev.rdev == rdev)
                    .ok_or(VfsError::NotFound)?;
                let slot = device_power_slot_by_name(name).ok_or(VfsError::NotFound)?;
                mk_reg(
                    device_power_ino(class_name, rdev, slot.to_u64()),
                    SysRegFile::DevicePower { idx, slot },
                )
            }
            SysDirKind::DevicesVirtual => {
                let class_idx = snap
                    .virtual_classes
                    .iter()
                    .position(|class_name| *class_name == name)
                    .ok_or(VfsError::NotFound)?;
                Ok(mk_dir(
                    virtual_class_ino(snap.virtual_classes[class_idx]),
                    SysDirKind::DevicesVirtualClass {
                        class_name: snap.virtual_classes[class_idx],
                    },
                ))
            }
            SysDirKind::DevicesVirtualClass { class_name } => {
                let idx = snap
                    .virtual_devices
                    .iter()
                    .position(|dev| dev.class_name == class_name && dev.sysfs_name == name)
                    .ok_or(VfsError::NotFound)?;
                Ok(mk_dir(
                    virtual_device_ino(class_name, snap.virtual_devices[idx].rdev),
                    SysDirKind::VirtualDevice {
                        class_name,
                        rdev: snap.virtual_devices[idx].rdev,
                    },
                ))
            }
            SysDirKind::VirtualDevice { class_name, rdev } => {
                let idx = snap
                    .virtual_devices
                    .iter()
                    .position(|dev| dev.class_name == class_name && dev.rdev == rdev)
                    .ok_or(VfsError::NotFound)?;
                let slot = device_slot_by_name(name).ok_or(VfsError::NotFound)?;
                let ino = virtual_device_slot_ino(class_name, rdev, slot.to_u64());
                if matches!(slot, DeviceSlot::PwrDir) {
                    Ok(mk_dir(
                        ino,
                        SysDirKind::VirtualDevicePower { class_name, rdev },
                    ))
                } else if matches!(slot, DeviceSlot::Subsystem) {
                    let target = format!("../../../class/{}", snap.virtual_devices[idx].class_name);
                    Ok(mk_link(ino, target))
                } else {
                    mk_reg(ino, SysRegFile::VirtualDevice { idx, slot })
                }
            }
            SysDirKind::VirtualDevicePower { class_name, rdev } => {
                let idx = snap
                    .virtual_devices
                    .iter()
                    .position(|dev| dev.class_name == class_name && dev.rdev == rdev)
                    .ok_or(VfsError::NotFound)?;
                let slot = device_power_slot_by_name(name).ok_or(VfsError::NotFound)?;
                mk_reg(
                    virtual_device_power_ino(class_name, rdev, slot.to_u64()),
                    SysRegFile::VirtualDevicePower { idx, slot },
                )
            }
            SysDirKind::DevicesPnp => {
                let bus_idx = snap
                    .pnp_buses
                    .iter()
                    .position(|bus| *bus == name)
                    .ok_or(VfsError::NotFound)?;
                Ok(mk_dir(
                    pnp_bus_ino(snap.pnp_buses[bus_idx]),
                    SysDirKind::DevicesPnpBus {
                        bus: snap.pnp_buses[bus_idx],
                    },
                ))
            }
            SysDirKind::DevicesPnpBus { bus } => {
                if !snap
                    .pnp_devices
                    .iter()
                    .any(|dev| dev.bus_type == bus && dev.sysfs_name == name)
                {
                    return Err(VfsError::NotFound);
                }
                Ok(mk_dir(
                    pnp_device_ino(bus, name),
                    SysDirKind::PnpDevice {
                        bus,
                        name: name.to_string(),
                    },
                ))
            }
            SysDirKind::PnpDevice {
                bus,
                name: dev_name,
            } => {
                let idx = snap
                    .pnp_devices
                    .iter()
                    .position(|dev| dev.bus_type == bus && dev.sysfs_name == dev_name)
                    .ok_or(VfsError::NotFound)?;
                let slot = pnp_device_slot_by_name(name).ok_or(VfsError::NotFound)?;
                mk_reg(
                    pnp_device_slot_ino(bus, &dev_name, slot.to_u64()),
                    SysRegFile::PnpDevice { idx, slot },
                )
            }
            SysDirKind::Dev => match name {
                "block" => Ok(mk_dir(DEV_BLOCK_DIR_INO, SysDirKind::DevBlock)),
                "char" => Ok(mk_dir(DEV_CHAR_DIR_INO, SysDirKind::DevChar)),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::DevBlock => {
                let rdev = parse_rdev_name(name).ok_or(VfsError::NotFound)?;
                let idx = snap
                    .block_nodes
                    .iter()
                    .position(|b| b.rdev == rdev)
                    .ok_or(VfsError::NotFound)?;
                Ok(mk_link(
                    dev_block_link_ino(rdev),
                    block_device_link_target(snap, idx, "../../"),
                ))
            }
            SysDirKind::DevChar => {
                let rdev = parse_rdev_name(name).ok_or(VfsError::NotFound)?;
                if !snap.char_nodes.iter().any(|c| c.rdev == rdev) {
                    return Err(VfsError::NotFound);
                }
                Ok(mk_dir(
                    dev_char_dir_ino(rdev),
                    SysDirKind::DevCharInner { rdev },
                ))
            }
            SysDirKind::DevCharInner { rdev } => {
                let idx = snap
                    .char_nodes
                    .iter()
                    .position(|c| c.rdev == rdev)
                    .ok_or(VfsError::NotFound)?;
                let slot = dev_char_inner_slot_by_name(name).ok_or(VfsError::NotFound)?;
                let ino = dev_char_inner_ino(rdev, slot.to_u64());
                match slot {
                    DevCharInnerSlot::DeviceLink => Ok(mk_link(
                        ino,
                        char_device_link_target(snap, idx, "../../../"),
                    )),
                    DevCharInnerSlot::SubsystemLink => Ok(mk_link(
                        ino,
                        format!("../../../class/{}", snap.char_nodes[idx].class_name),
                    )),
                    _ => mk_reg(ino, SysRegFile::DevCharInner { idx, slot }),
                }
            }
            SysDirKind::Kernel => match name {
                "hostname" => mk_reg(KERNEL_HOSTNAME_INO, SysRegFile::Hostname),
                "ostype" => mk_reg(KERNEL_OSTYPE_INO, SysRegFile::Ostype),
                "osrelease" => mk_reg(KERNEL_OSRELEASE_INO, SysRegFile::Osrelease),
                "version" => mk_reg(KERNEL_VERSION_INO, SysRegFile::Version),
                "cmdline" => mk_reg(KERNEL_CMDLINE_INO, SysRegFile::Cmdline),
                // 该文件是 devtmpfs/sysfs/procfs 共享的 function 投影诊断入口。
                // 它只展示 VFS 用户态命名空间发布状态，不参与底层设备生命周期。
                "device_functions" => {
                    mk_reg(KERNEL_DEVICE_FUNCTIONS_INO, SysRegFile::DeviceFunctions)
                }
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::Fs => match name {
                // 当前内核尚未提供 cgroup controller registry；这里暴露稳定的空根目录，
                // 等 controller 子系统接入后再由 registry 驱动目录内容。
                "cgroup" => Ok(mk_dir(FS_CGROUP_INO, SysDirKind::FsCgroup)),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::FsCgroup => Err(VfsError::NotFound),
            SysDirKind::Class => {
                let class = snap
                    .classes
                    .iter()
                    .find(|class| class.name == name)
                    .ok_or(VfsError::NotFound)?;
                Ok(mk_dir(
                    class_dir_ino(class.name),
                    SysDirKind::ClassDir {
                        class_name: class.name,
                    },
                ))
            }
            SysDirKind::ClassDir { class_name } => {
                let node = snap
                    .class_nodes
                    .iter()
                    .find(|node| node.class_name == class_name && node.sysfs_name == name)
                    .ok_or(VfsError::NotFound)?;
                let ino = class_node_ino(class_name, &node.sysfs_name);
                match node.kind.clone() {
                    SysClassNodeKind::Symlink { target } => Ok(mk_link(ino, target)),
                    SysClassNodeKind::NetInterface { iface_id } => Ok(mk_dir(
                        class_net_iface_ino(iface_id),
                        SysDirKind::ClassNetIface { iface_id },
                    )),
                }
            }
            SysDirKind::ClassNetIface { iface_id } => {
                if let Some(slot) = netdev_slot_by_name(name) {
                    mk_reg(
                        class_net_iface_slot_ino(iface_id, slot.to_u64()),
                        SysRegFile::NetDev { iface_id, slot },
                    )
                } else if name == "statistics" {
                    Ok(mk_dir(
                        class_net_stats_ino(iface_id),
                        SysDirKind::ClassNetStats { iface_id },
                    ))
                } else {
                    Err(VfsError::NotFound)
                }
            }
            SysDirKind::ClassNetStats { iface_id } => {
                if let Some(slot) = netdev_stats_slot_by_name(name) {
                    mk_reg(
                        class_net_stats_slot_ino(iface_id, slot.to_u64()),
                        SysRegFile::NetDev {
                            iface_id,
                            slot: slot.to_netdev_slot(),
                        },
                    )
                } else {
                    Err(VfsError::NotFound)
                }
            }
            SysDirKind::Bus => {
                let bus_idx = snap
                    .pnp_buses
                    .iter()
                    .position(|bus| *bus == name)
                    .ok_or(VfsError::NotFound)?;
                Ok(mk_dir(
                    bus_class_ino(snap.pnp_buses[bus_idx]),
                    SysDirKind::BusClass {
                        bus: snap.pnp_buses[bus_idx],
                    },
                ))
            }
            SysDirKind::BusClass { bus } => match name {
                "devices" => Ok(mk_dir(
                    bus_class_devices_ino(bus),
                    SysDirKind::BusClassDevices { bus },
                )),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::BusClassDevices { bus } => {
                if !snap.pnp_buses.iter().any(|entry| *entry == bus) {
                    return Err(VfsError::NotFound);
                }
                let idx = snap
                    .pnp_devices
                    .iter()
                    .position(|dev| dev.bus_type == bus && dev.sysfs_name == name)
                    .ok_or(VfsError::NotFound)?;
                Ok(mk_link(
                    bus_class_device_link_ino(bus, name),
                    format!(
                        "../../../devices/pnp/{}/{}",
                        bus, snap.pnp_devices[idx].sysfs_name
                    ),
                ))
            }
            SysDirKind::Module | SysDirKind::Power | SysDirKind::Firmware => {
                Err(VfsError::NotFound)
            }
            SysDirKind::DevicesSystem => match name {
                "cpu" => Ok(mk_dir(DEVICES_SYSTEM_CPU_INO, SysDirKind::DevicesSystemCpu)),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::DevicesSystemCpu => {
                if name == "online" {
                    mk_reg(DEVICES_SYSTEM_CPU_ONLINE_INO, SysRegFile::CpuOnline)
                } else if name == "possible" {
                    mk_reg(DEVICES_SYSTEM_CPU_POSSIBLE_INO, SysRegFile::CpuPossible)
                } else if name == "present" {
                    mk_reg(DEVICES_SYSTEM_CPU_PRESENT_INO, SysRegFile::CpuPresent)
                } else if let Some(rest) = name.strip_prefix("cpu") {
                    let cpu_id: usize = rest.parse().map_err(|_| VfsError::NotFound)?;
                    let mask = online_cpu_mask();
                    if mask & (1u64 << cpu_id) == 0 {
                        return Err(VfsError::NotFound);
                    }
                    Ok(mk_dir(cpu_ino(cpu_id), SysDirKind::Cpu { cpu_id }))
                } else {
                    Err(VfsError::NotFound)
                }
            }
            SysDirKind::Cpu { cpu_id } => match name {
                "online" => mk_reg(
                    cpu_slot_ino(cpu_id, CpuSlot::Online.to_u64()),
                    SysRegFile::Cpu {
                        cpu_id,
                        slot: CpuSlot::Online,
                    },
                ),
                "possible" => mk_reg(
                    cpu_slot_ino(cpu_id, CpuSlot::Possible.to_u64()),
                    SysRegFile::Cpu {
                        cpu_id,
                        slot: CpuSlot::Possible,
                    },
                ),
                "present" => mk_reg(
                    cpu_slot_ino(cpu_id, CpuSlot::Present.to_u64()),
                    SysRegFile::Cpu {
                        cpu_id,
                        slot: CpuSlot::Present,
                    },
                ),
                "topology" => Ok(mk_dir(
                    cpu_slot_ino(cpu_id, CpuSlot::TopoDir.to_u64()),
                    SysDirKind::CpuTopology { cpu_id },
                )),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::CpuTopology { cpu_id } => {
                let slot = cpu_topology_slot_by_name(name).ok_or(VfsError::NotFound)?;
                mk_reg(
                    cpu_topology_slot_ino(cpu_id, slot.to_u64()),
                    SysRegFile::CpuTopology { cpu_id, slot },
                )
            }
        }
    }

    fn readdir_entries(&self) -> Vec<DirEntry> {
        let snap = &self.snap;
        let mk_dir_entry = |ino: u64, name: &str, kind: FileType| DirEntry {
            ino,
            name: sysfs_smallstr_lossy(name),
            kind,
        };
        match self.kind.clone() {
            SysDirKind::Root => vec![
                mk_dir_entry(BLOCK_DIR_INO, "block", FileType::Directory),
                mk_dir_entry(DEVICES_DIR_INO, "devices", FileType::Directory),
                mk_dir_entry(DEV_DIR_INO, "dev", FileType::Directory),
                mk_dir_entry(KERNEL_DIR_INO, "kernel", FileType::Directory),
                mk_dir_entry(FS_DIR_INO, "fs", FileType::Directory),
                mk_dir_entry(BUS_DIR_INO, "bus", FileType::Directory),
                mk_dir_entry(CLASS_DIR_INO, "class", FileType::Directory),
                mk_dir_entry(MODULE_DIR_INO, "module", FileType::Directory),
                mk_dir_entry(POWER_DIR_INO, "power", FileType::Directory),
                mk_dir_entry(FIRMWARE_DIR_INO, "firmware", FileType::Directory),
            ],
            SysDirKind::Block => {
                let mut entries = Vec::new();
                for b in &snap.blocks {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        block_dev_ino(&b.sysfs_name),
                        &b.sysfs_name,
                        FileType::Directory,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::BlockDev { name } => {
                let Some(_) = snap
                    .blocks
                    .iter()
                    .position(|block| block.sysfs_name == name)
                else {
                    return Vec::new();
                };
                vec![
                    mk_dir_entry(
                        block_dev_slot_ino(&name, BlockDevSlot::Size.to_u64()),
                        "size",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_dev_slot_ino(&name, BlockDevSlot::Ro.to_u64()),
                        "ro",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_dev_slot_ino(&name, BlockDevSlot::Removable.to_u64()),
                        "removable",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_dev_slot_ino(&name, BlockDevSlot::Dev.to_u64()),
                        "dev",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_dev_slot_ino(&name, BlockDevSlot::Range.to_u64()),
                        "range",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_dev_slot_ino(&name, BlockDevSlot::QueueDir.to_u64()),
                        "queue",
                        FileType::Directory,
                    ),
                    mk_dir_entry(
                        block_dev_slot_ino(&name, BlockDevSlot::Holders.to_u64()),
                        "holders",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_dev_slot_ino(&name, BlockDevSlot::Stat.to_u64()),
                        "stat",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_dev_slot_ino(&name, BlockDevSlot::Inflight.to_u64()),
                        "inflight",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_dev_slot_ino(&name, BlockDevSlot::Periodic.to_u64()),
                        "periodic",
                        FileType::Regular,
                    ),
                ]
            }
            SysDirKind::BlockQueue { name } => {
                let Some(_) = snap
                    .blocks
                    .iter()
                    .position(|block| block.sysfs_name == name)
                else {
                    return Vec::new();
                };
                vec![
                    mk_dir_entry(
                        block_queue_slot_ino(&name, BlockQueueSlot::Lbs.to_u64()),
                        "logical_block_size",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_queue_slot_ino(&name, BlockQueueSlot::Pbs.to_u64()),
                        "physical_block_size",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_queue_slot_ino(&name, BlockQueueSlot::Rotational.to_u64()),
                        "rotational",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_queue_slot_ino(&name, BlockQueueSlot::NrRequests.to_u64()),
                        "nr_requests",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_queue_slot_ino(&name, BlockQueueSlot::HwSectorSize.to_u64()),
                        "hw_sector_size",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_queue_slot_ino(&name, BlockQueueSlot::DiscardZeroes.to_u64()),
                        "discard_zeroes_data",
                        FileType::Regular,
                    ),
                ]
            }
            SysDirKind::Devices => {
                let mut entries = Vec::new();
                for dev in &snap.devices {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        device_ino(dev.class_name, dev.rdev),
                        &dev.sysfs_name,
                        FileType::Directory,
                    ) {
                        return entries;
                    }
                }
                for (ino, name) in [
                    (DEVICES_SYSTEM_INO, "system"),
                    (DEVICES_VIRTUAL_INO, "virtual"),
                    (DEVICES_PNP_INO, "pnp"),
                ] {
                    if !push_sysfs_dir_entry(&mut entries, ino, name, FileType::Directory) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::Device { class_name, rdev } => {
                let Some(_) = snap
                    .devices
                    .iter()
                    .position(|dev| dev.class_name == class_name && dev.rdev == rdev)
                else {
                    return Vec::new();
                };
                vec![
                    mk_dir_entry(
                        device_slot_ino(class_name, rdev, DeviceSlot::Name.to_u64()),
                        "name",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        device_slot_ino(class_name, rdev, DeviceSlot::Dev.to_u64()),
                        "dev",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        device_slot_ino(class_name, rdev, DeviceSlot::Subsystem.to_u64()),
                        "subsystem",
                        FileType::Symlink,
                    ),
                    mk_dir_entry(
                        device_slot_ino(class_name, rdev, DeviceSlot::PwrDir.to_u64()),
                        "power",
                        FileType::Directory,
                    ),
                ]
            }
            SysDirKind::DevicePower { class_name, rdev } => {
                let Some(_) = snap
                    .devices
                    .iter()
                    .position(|dev| dev.class_name == class_name && dev.rdev == rdev)
                else {
                    return Vec::new();
                };
                let mut entries = Vec::new();
                for slot in DevicePowerSlot::ALL {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        device_power_ino(class_name, rdev, slot.to_u64()),
                        slot.file_name(),
                        FileType::Regular,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::DevicesVirtual => {
                let mut entries = Vec::new();
                for class_name in &snap.virtual_classes {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        virtual_class_ino(class_name),
                        class_name,
                        FileType::Directory,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::DevicesVirtualClass { class_name } => {
                let mut entries = Vec::new();
                for dev in snap
                    .virtual_devices
                    .iter()
                    .filter(|dev| dev.class_name == class_name)
                {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        virtual_device_ino(dev.class_name, dev.rdev),
                        &dev.sysfs_name,
                        FileType::Directory,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::VirtualDevice { class_name, rdev } => {
                let Some(_) = snap
                    .virtual_devices
                    .iter()
                    .position(|dev| dev.class_name == class_name && dev.rdev == rdev)
                else {
                    return Vec::new();
                };
                vec![
                    mk_dir_entry(
                        virtual_device_slot_ino(class_name, rdev, DeviceSlot::Name.to_u64()),
                        "name",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        virtual_device_slot_ino(class_name, rdev, DeviceSlot::Dev.to_u64()),
                        "dev",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        virtual_device_slot_ino(class_name, rdev, DeviceSlot::Subsystem.to_u64()),
                        "subsystem",
                        FileType::Symlink,
                    ),
                    mk_dir_entry(
                        virtual_device_slot_ino(class_name, rdev, DeviceSlot::PwrDir.to_u64()),
                        "power",
                        FileType::Directory,
                    ),
                ]
            }
            SysDirKind::VirtualDevicePower { class_name, rdev } => {
                let Some(_) = snap
                    .virtual_devices
                    .iter()
                    .position(|dev| dev.class_name == class_name && dev.rdev == rdev)
                else {
                    return Vec::new();
                };
                let mut entries = Vec::new();
                for slot in DevicePowerSlot::ALL {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        virtual_device_power_ino(class_name, rdev, slot.to_u64()),
                        slot.file_name(),
                        FileType::Regular,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::DevicesPnp => {
                let mut entries = Vec::new();
                for bus in &snap.pnp_buses {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        pnp_bus_ino(bus),
                        bus,
                        FileType::Directory,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::DevicesPnpBus { bus } => {
                let mut entries = Vec::new();
                for dev in snap.pnp_devices.iter().filter(|dev| dev.bus_type == bus) {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        pnp_device_ino(dev.bus_type, &dev.sysfs_name),
                        &dev.sysfs_name,
                        FileType::Directory,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::PnpDevice { bus, name } => {
                let Some(_) = snap
                    .pnp_devices
                    .iter()
                    .position(|dev| dev.bus_type == bus && dev.sysfs_name == name)
                else {
                    return Vec::new();
                };
                let mut entries = Vec::new();
                for slot in PnpDeviceSlot::ALL {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        pnp_device_slot_ino(bus, &name, slot.to_u64()),
                        slot.file_name(),
                        FileType::Regular,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::Dev => vec![
                mk_dir_entry(DEV_BLOCK_DIR_INO, "block", FileType::Directory),
                mk_dir_entry(DEV_CHAR_DIR_INO, "char", FileType::Directory),
            ],
            SysDirKind::DevBlock => {
                let mut entries = Vec::new();
                for node in &snap.block_nodes {
                    let name = rdev_name(node.rdev);
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        dev_block_link_ino(node.rdev),
                        &name,
                        FileType::Symlink,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::DevChar => {
                let mut entries = Vec::new();
                for node in &snap.char_nodes {
                    let name = rdev_name(node.rdev);
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        dev_char_dir_ino(node.rdev),
                        &name,
                        FileType::Directory,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::DevCharInner { rdev } => {
                let Some(_) = snap.char_nodes.iter().position(|node| node.rdev == rdev) else {
                    return Vec::new();
                };
                vec![
                    mk_dir_entry(
                        dev_char_inner_ino(rdev, DevCharInnerSlot::Dev.to_u64()),
                        "dev",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        dev_char_inner_ino(rdev, DevCharInnerSlot::DeviceLink.to_u64()),
                        "device",
                        FileType::Symlink,
                    ),
                    mk_dir_entry(
                        dev_char_inner_ino(rdev, DevCharInnerSlot::SubsystemLink.to_u64()),
                        "subsystem",
                        FileType::Symlink,
                    ),
                    mk_dir_entry(
                        dev_char_inner_ino(rdev, DevCharInnerSlot::Uevent.to_u64()),
                        "uevent",
                        FileType::Regular,
                    ),
                ]
            }
            SysDirKind::Kernel => vec![
                mk_dir_entry(KERNEL_HOSTNAME_INO, "hostname", FileType::Regular),
                mk_dir_entry(KERNEL_OSTYPE_INO, "ostype", FileType::Regular),
                mk_dir_entry(KERNEL_OSRELEASE_INO, "osrelease", FileType::Regular),
                mk_dir_entry(KERNEL_VERSION_INO, "version", FileType::Regular),
                mk_dir_entry(KERNEL_CMDLINE_INO, "cmdline", FileType::Regular),
                mk_dir_entry(
                    KERNEL_DEVICE_FUNCTIONS_INO,
                    "device_functions",
                    FileType::Regular,
                ),
            ],
            SysDirKind::Fs => vec![mk_dir_entry(FS_CGROUP_INO, "cgroup", FileType::Directory)],
            SysDirKind::FsCgroup => Vec::new(),
            SysDirKind::Class => {
                let mut entries = Vec::new();
                for class in &snap.classes {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        class_dir_ino(class.name),
                        class.name,
                        FileType::Directory,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::ClassDir { class_name } => {
                let mut entries = Vec::new();
                for node in snap
                    .class_nodes
                    .iter()
                    .filter(|node| node.class_name == class_name)
                {
                    let kind = match &node.kind {
                        SysClassNodeKind::Symlink { .. } => FileType::Symlink,
                        SysClassNodeKind::NetInterface { .. } => FileType::Directory,
                    };
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        class_node_ino(class_name, &node.sysfs_name),
                        &node.sysfs_name,
                        kind,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::ClassNetIface { iface_id } => {
                let mut entries = Vec::new();
                if !push_sysfs_dir_entry(
                    &mut entries,
                    class_net_stats_ino(iface_id),
                    "statistics",
                    FileType::Directory,
                ) {
                    return entries;
                }
                for slot in NetDevSlot::ALL {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        class_net_iface_slot_ino(iface_id, slot.to_u64()),
                        slot.file_name(),
                        FileType::Regular,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::ClassNetStats { iface_id } => {
                let mut entries = Vec::new();
                for slot in NetDevStatsSlot::ALL {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        class_net_stats_slot_ino(iface_id, slot.to_u64()),
                        slot.file_name(),
                        FileType::Regular,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::Bus => {
                let mut entries = Vec::new();
                for bus in &snap.pnp_buses {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        bus_class_ino(bus),
                        bus,
                        FileType::Directory,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::BusClass { bus } => {
                let Some(_) = snap.pnp_buses.iter().position(|entry| *entry == bus) else {
                    return Vec::new();
                };
                vec![mk_dir_entry(
                    bus_class_devices_ino(bus),
                    "devices",
                    FileType::Directory,
                )]
            }
            SysDirKind::BusClassDevices { bus } => {
                let Some(_) = snap.pnp_buses.iter().position(|entry| *entry == bus) else {
                    return Vec::new();
                };
                let mut entries = Vec::new();
                for dev in snap.pnp_devices.iter().filter(|dev| dev.bus_type == bus) {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        bus_class_device_link_ino(bus, &dev.sysfs_name),
                        &dev.sysfs_name,
                        FileType::Symlink,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::Module | SysDirKind::Power | SysDirKind::Firmware => Vec::new(),
            SysDirKind::DevicesSystem => vec![mk_dir_entry(
                DEVICES_SYSTEM_CPU_INO,
                "cpu",
                FileType::Directory,
            )],
            SysDirKind::DevicesSystemCpu => {
                let mask = online_cpu_mask();
                let mut entries = Vec::new();
                for (ino, name) in [
                    (DEVICES_SYSTEM_CPU_ONLINE_INO, "online"),
                    (DEVICES_SYSTEM_CPU_POSSIBLE_INO, "possible"),
                    (DEVICES_SYSTEM_CPU_PRESENT_INO, "present"),
                ] {
                    if !push_sysfs_dir_entry(&mut entries, ino, name, FileType::Regular) {
                        return entries;
                    }
                }
                for cpu in CpuMaskIter::new(mask) {
                    let name = {
                        let mut out = String::new();
                        use core::fmt::Write;
                        if write!(&mut out, "cpu{}", cpu).is_err() {
                            return entries;
                        }
                        out
                    };
                    if !push_sysfs_dir_entry(&mut entries, cpu_ino(cpu), &name, FileType::Directory)
                    {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::Cpu { cpu_id } => vec![
                mk_dir_entry(
                    cpu_slot_ino(cpu_id, CpuSlot::Online.to_u64()),
                    "online",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    cpu_slot_ino(cpu_id, CpuSlot::Possible.to_u64()),
                    "possible",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    cpu_slot_ino(cpu_id, CpuSlot::Present.to_u64()),
                    "present",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    cpu_slot_ino(cpu_id, CpuSlot::TopoDir.to_u64()),
                    "topology",
                    FileType::Directory,
                ),
            ],
            SysDirKind::CpuTopology { cpu_id } => {
                let mut entries = Vec::new();
                for slot in CpuTopologySlot::ALL {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        cpu_topology_slot_ino(cpu_id, slot.to_u64()),
                        slot.file_name(),
                        FileType::Regular,
                    ) {
                        return entries;
                    }
                }
                entries
            }
        }
    }
}
// ─── 名字 → slot 查表 ────────────────────────────────────────

fn block_slot_by_name(name: &str) -> Option<BlockDevSlot> {
    Some(match name {
        "size" => BlockDevSlot::Size,
        "ro" => BlockDevSlot::Ro,
        "removable" => BlockDevSlot::Removable,
        "dev" => BlockDevSlot::Dev,
        "range" => BlockDevSlot::Range,
        "queue" => BlockDevSlot::QueueDir,
        "holders" => BlockDevSlot::Holders,
        "stat" => BlockDevSlot::Stat,
        "inflight" => BlockDevSlot::Inflight,
        "periodic" => BlockDevSlot::Periodic,
        _ => return None,
    })
}

fn block_queue_slot_by_name(name: &str) -> Option<BlockQueueSlot> {
    Some(match name {
        "logical_block_size" => BlockQueueSlot::Lbs,
        "physical_block_size" => BlockQueueSlot::Pbs,
        "rotational" => BlockQueueSlot::Rotational,
        "nr_requests" => BlockQueueSlot::NrRequests,
        "hw_sector_size" => BlockQueueSlot::HwSectorSize,
        "discard_zeroes_data" => BlockQueueSlot::DiscardZeroes,
        _ => return None,
    })
}

fn device_slot_by_name(name: &str) -> Option<DeviceSlot> {
    Some(match name {
        "name" => DeviceSlot::Name,
        "dev" => DeviceSlot::Dev,
        "subsystem" => DeviceSlot::Subsystem,
        "power" => DeviceSlot::PwrDir,
        _ => return None,
    })
}

fn dev_char_inner_slot_by_name(name: &str) -> Option<DevCharInnerSlot> {
    Some(match name {
        "dev" => DevCharInnerSlot::Dev,
        "device" => DevCharInnerSlot::DeviceLink,
        "subsystem" => DevCharInnerSlot::SubsystemLink,
        "uevent" => DevCharInnerSlot::Uevent,
        _ => return None,
    })
}

// ─── 根 inode 工厂 ───────────────────────────────────────────

fn build_root_inode(fs_id: FsId, weak_sb: &Weak<Superblock>, snap: Arc<SysSnapshot>) -> Arc<Inode> {
    let ops: Arc<dyn InodeOps + Send + Sync> = Arc::new(SysDirInodeOps {
        kind: SysDirKind::Root,
        fs_id,
        weak_sb: weak_sb.clone(),
        snap,
    });
    Inode::new(
        InodeId {
            fs_id,
            ino: ROOT_INO,
        },
        FileType::Directory,
        DevId::new(0, 0),
        4096,
        None,
        inode_meta(0o555, 2, timespec_now()),
        ops,
        weak_sb.clone(),
    )
}

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
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

use crate::dev::block::{
    BlockAttributes, BlockFeatures, BlockGeometry, BlockIoStatsSnapshot, BlockLimits,
};
use crate::dev::enumerate::{DEVICES, PNP_DEVICES};
use crate::dev::net::NET_CLASS;
use crate::dev::pnp::{PnpDependency, PnpId, PnpOwnedResourceSnapshot, PnpResourceKind, PnpState};
use crate::dev::{cpu, numa};
use crate::vfs::device_files::projection::{
    PublishedDevNodeClass, append_function_projection_diagnostics, published_block_devnodes,
    published_char_devnodes, published_devnode_classes,
};
use crate::vfs::user_api::device_numbers;

/// 安装 Device Tree sysfs 投影时可能返回的错误。
#[derive(Debug)]
pub enum DeviceTreeSysfsInstallError {
    /// 输入不是一份符合 FDT 结构规范的扁平设备树。
    InvalidFdt(fdt::Error),
    /// 已经安装了内容不同的启动设备树。
    AlreadyInstalled,
}

/// 向 sysfs live Device Tree 应用 overlay 时可能返回的错误。
#[derive(Debug)]
pub enum DeviceTreeSysfsOverlayError {
    /// 尚未安装启动设备树。
    NotInstalled,
    /// 当前 live tree 无法建立规范 owned 表示。
    InvalidLiveTree(fdt::OwnedTreeError),
    /// overlay 本身、fixup 或 fragment 不合法。
    InvalidOverlay(fdt::OverlayError),
    /// 合并结果无法重新序列化为规范 DTB。
    InvalidOutput(fdt::OwnedTreeError),
    /// 另一个 overlay 事务正在校验或切换设备模型。
    UpdateInProgress,
    /// 内核固件语义层拒绝提交候选 live tree。
    RuntimeRejected(DeviceTreeOverlayRuntimeError),
}

/// live Device Tree 进入内核设备模型时的拒绝原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceTreeOverlayRuntimeError {
    /// 候选树虽然是合法 FDT，但不能建立完整的规范固件抽象。
    InvalidFirmware,
    /// overlay 试图修改本内核不支持热插拔的启动对象。
    UnsupportedChange,
    /// platform PnP 设备集合无法完成事务式切换。
    PlatformPnp,
    /// 规范化节点图无法与 live tree 一同提交。
    NodeGraph,
}

/// 安装 live Device Tree 提交钩子时可能返回的错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceTreeOverlayHookInstallError {
    /// 已经安装了另一个提交钩子。
    AlreadyInstalled,
}

/// 在 sysfs 发布候选树前同步内核固件抽象和设备模型。
pub type DeviceTreeOverlayCommitHook =
    fn(base: &[u8], candidate: &[u8]) -> Result<(), DeviceTreeOverlayRuntimeError>;

/// sysfs 持有的启动设备树。
///
/// 启动 blob 在安装时复制并清除不可重新公开的启动秘密，之后永久不可变；live blob
/// 使用独立分配，并可在 overlay 完整校验后原子替换。目录访问每次先固定 live `Arc`
/// 快照，再从中取得借用视图，因此不需要自引用结构，也不会把解析器内部类型泄露给
/// VFS。
struct DeviceTreeFirmware {
    boot_blob: Arc<[u8]>,
    live_blob: Spinlock<Arc<[u8]>>,
    overlay_in_progress: AtomicBool,
}

impl DeviceTreeFirmware {
    fn from_fdt(tree: &fdt::Fdt<'_>) -> Result<Self, fdt::Error> {
        const FDT_NOP_BYTES: [u8; 4] = 4u32.to_be_bytes();

        let mut blob = tree.as_bytes().to_vec();
        let seed_records = tree
            .root()
            .children()
            .filter(|node| matches!(node.name(), "chosen" | "chosen@0"))
            .flat_map(|chosen| {
                chosen
                    .properties()
                    .filter(|property| matches!(property.name(), "rng-seed" | "kaslr-seed"))
                    .map(|property| property.encoded_structure_range())
            })
            .collect::<Vec<_>>();
        let structure_start = tree.header().off_dt_struct as usize;
        for encoded in seed_records {
            let start = structure_start + encoded.start;
            let end = structure_start + encoded.end;
            debug_assert!((end - start).is_multiple_of(4));
            for token in blob[start..end].chunks_exact_mut(4) {
                token.copy_from_slice(&FDT_NOP_BYTES);
            }
        }

        // 完整属性记录被替换为 NOP 后再次校验，确保 raw FDT 与目录投影始终
        // 来自同一份仍符合 FDT token 规则的不可变副本。
        fdt::Fdt::parse(&blob)?;
        let boot_blob: Arc<[u8]> = blob.into();
        // 两个 ABI 文件必须具有彼此独立的生命周期：后续 live tree 交换绝不能改变
        // `/sys/firmware/fdt` 保存的启动快照。
        let live_blob: Arc<[u8]> = Arc::from(boot_blob.as_ref());
        Ok(Self {
            boot_blob,
            live_blob: Spinlock::new(live_blob),
            overlay_in_progress: AtomicBool::new(false),
        })
    }

    fn live_blob(&self) -> Arc<[u8]> {
        Arc::clone(&self.live_blob.lock())
    }

    fn begin_overlay_update(&self) -> Result<DeviceTreeOverlayUpdateGuard<'_>, ()> {
        self.overlay_in_progress
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| ())?;
        Ok(DeviceTreeOverlayUpdateGuard {
            active: &self.overlay_in_progress,
        })
    }
}

struct DeviceTreeOverlayUpdateGuard<'a> {
    active: &'a AtomicBool,
}

impl Drop for DeviceTreeOverlayUpdateGuard<'_> {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

/// live tree 中不依赖 structure block offset 的节点身份。
///
/// 对规范 DTB，`path` 就是唯一绝对路径，`sibling_occurrences` 全为零。后者只用于让
/// sysfs 继续安全投影旧固件中不合规范的同名兄弟节点，不参与规范路径的 inode key。
#[derive(Clone, Debug, PartialEq, Eq)]
struct DeviceTreeNodeId {
    path: String,
    sibling_occurrences: Vec<usize>,
}

impl DeviceTreeNodeId {
    fn root() -> Self {
        Self {
            path: "/".to_string(),
            sibling_occurrences: Vec::new(),
        }
    }

    fn child(&self, name: &str, sibling_occurrence: usize) -> Self {
        let path = if self.path == "/" {
            format!("/{name}")
        } else {
            format!("{}/{name}", self.path)
        };
        let mut sibling_occurrences = self.sibling_occurrences.clone();
        sibling_occurrences.push(sibling_occurrence);
        Self {
            path,
            sibling_occurrences,
        }
    }

    fn node<'a>(&self, blob: &'a [u8]) -> Option<fdt::Node<'a>> {
        let tree = fdt::Fdt::parse(blob).ok()?;
        if self.path == "/" {
            return Some(tree.root());
        }

        let mut node = tree.root();
        let mut occurrences = self.sibling_occurrences.iter().copied();
        for component in self.path.strip_prefix('/')?.split('/') {
            let occurrence = occurrences.next()?;
            node = node
                .children()
                .filter(|child| child.name() == component)
                .nth(occurrence)?;
        }
        occurrences.next().is_none().then_some(node)
    }

    fn append_key_suffix(&self, key: &mut String) {
        key.push_str(&self.path);
        if self
            .sibling_occurrences
            .iter()
            .any(|occurrence| *occurrence != 0)
        {
            key.push('\0');
            for occurrence in &self.sibling_occurrences {
                key.push_str(&format!("{occurrence},"));
            }
        }
    }
}

const DEVICE_TREE_SAFE_NAME_RETRIES: usize = 16;

fn device_tree_has_synthetic_name(node: fdt::Node<'_>) -> bool {
    node.property("name").is_none()
}

#[derive(Clone, Copy)]
enum DeviceTreePropertySource<'a> {
    Encoded(fdt::Property<'a>),
    SyntheticName,
}

fn device_tree_property_mode(name: &str) -> u16 {
    if name.starts_with("security-") {
        0o400
    } else {
        0o444
    }
}

/// 复现 Linux `drivers/of/kobj.c::safe_name` 的成功发布结果。
///
/// Linux 在原名冲突后最多构造 `#1` 到 `#16`；若最后一个候选仍冲突，
/// 后续 `kobject_add` 会以 `EEXIST` 失败，因此这里返回 `None`。
fn device_tree_safe_name(original: &str, mut occupied: impl FnMut(&str) -> bool) -> Option<String> {
    if !occupied(original) {
        return Some(original.to_string());
    }
    for suffix in 1..=DEVICE_TREE_SAFE_NAME_RETRIES {
        let candidate = format!("{original}#{suffix}");
        if !occupied(&candidate) {
            return Some(candidate);
        }
    }
    None
}

struct DeviceTreeChildProjection<'a> {
    sysfs_name: String,
    node: fdt::Node<'a>,
    sibling_occurrence: usize,
}

struct DeviceTreePropertyProjection<'a> {
    sysfs_name: String,
    source: DeviceTreePropertySource<'a>,
}

impl DeviceTreePropertyProjection<'_> {
    fn original_name(&self) -> &str {
        match self.source {
            DeviceTreePropertySource::Encoded(property) => property.name(),
            DeviceTreePropertySource::SyntheticName => "name",
        }
    }

    fn data(&self, node: fdt::Node<'_>) -> Arc<[u8]> {
        match self.source {
            DeviceTreePropertySource::Encoded(property) => Arc::from(property.value()),
            DeviceTreePropertySource::SyntheticName => {
                let base_name = node.base_name_bytes();
                let mut value = Vec::with_capacity(base_name.len() + 1);
                value.extend_from_slice(base_name);
                value.push(0);
                Arc::from(value)
            }
        }
    }
}

/// 属性按 unflatten 后的链表顺序挂入 sysfs；同名属性同样使用 Linux `safe_name`。
fn device_tree_property_projections(node: fdt::Node<'_>) -> Vec<DeviceTreePropertyProjection<'_>> {
    let mut projected: Vec<DeviceTreePropertyProjection<'_>> = Vec::new();
    for property in node.properties() {
        let Some(sysfs_name) = device_tree_safe_name(property.name(), |candidate| {
            projected.iter().any(|entry| entry.sysfs_name == candidate)
        }) else {
            continue;
        };
        projected.push(DeviceTreePropertyProjection {
            sysfs_name,
            source: DeviceTreePropertySource::Encoded(property),
        });
    }

    if device_tree_has_synthetic_name(node) {
        let sysfs_name = device_tree_safe_name("name", |candidate| {
            projected.iter().any(|entry| entry.sysfs_name == candidate)
        });
        if let Some(sysfs_name) = sysfs_name {
            projected.push(DeviceTreePropertyProjection {
                sysfs_name,
                source: DeviceTreePropertySource::SyntheticName,
            });
        }
    }
    projected
}

/// 按 Linux 的挂接顺序计算当前节点的子目录显示名。
///
/// 当前节点的属性（包括合成 `name`）先占用名称，随后子节点按 DT 顺序逐个发布。
/// `node` 保留原始身份，`sysfs_name` 只用于用户可见目录项。
fn device_tree_child_projections<'a>(node: fdt::Node<'a>) -> Vec<DeviceTreeChildProjection<'a>> {
    let properties = device_tree_property_projections(node);
    let mut projected: Vec<DeviceTreeChildProjection<'a>> = Vec::new();
    let mut sibling_occurrences: BTreeMap<String, usize> = BTreeMap::new();
    for child in node.children() {
        let occurrence = sibling_occurrences
            .entry(child.name().to_string())
            .or_insert(0);
        let sibling_occurrence = *occurrence;
        *occurrence += 1;
        let sysfs_name = device_tree_safe_name(child.name(), |candidate| {
            properties.iter().any(|entry| entry.sysfs_name == candidate)
                || projected.iter().any(|entry| entry.sysfs_name == candidate)
        });
        let Some(sysfs_name) = sysfs_name else {
            continue;
        };
        projected.push(DeviceTreeChildProjection {
            sysfs_name,
            node: child,
            sibling_occurrence,
        });
    }
    projected
}

static DEVICE_TREE_FIRMWARE: Spinlock<Option<Arc<DeviceTreeFirmware>>> = Spinlock::new(None);
static DEVICE_TREE_OVERLAY_COMMIT_HOOK: Spinlock<Option<DeviceTreeOverlayCommitHook>> =
    Spinlock::new(None);

/// 安装 live Device Tree 的内核提交钩子。
///
/// sysfs 在持有 live tree 交换锁且确认基线仍然有效后调用该钩子。钩子返回错误时，
/// live blob 和 dentry 缓存均保持不变；返回成功后不再执行任何可能失败的步骤。
pub fn install_device_tree_overlay_commit_hook(
    hook: DeviceTreeOverlayCommitHook,
) -> Result<(), DeviceTreeOverlayHookInstallError> {
    let mut installed = DEVICE_TREE_OVERLAY_COMMIT_HOOK.lock();
    if let Some(current) = *installed {
        return if core::ptr::fn_addr_eq(current, hook) {
            Ok(())
        } else {
            Err(DeviceTreeOverlayHookInstallError::AlreadyInstalled)
        };
    }
    *installed = Some(hook);
    Ok(())
}

/// 安装 Linux ABI 兼容的启动 Device Tree sysfs 视图。
///
/// 安装成功后，所有 sysfs 实例都会暴露原始 blob `/sys/firmware/fdt`，以及
/// `/sys/firmware/devicetree/base` 下的节点和属性层次。除 Linux 同样会在消费后
/// 擦除的 `/chosen/{rng,kaslr}-seed` 外，属性内容保持原始二进制字节，不执行字符串、
/// 整数或端序转换。重复安装同一份投影是幂等操作；启动期间若已安装另一份投影，
/// 则拒绝替换，保证已打开 inode 的视图稳定。
pub fn install_device_tree(tree: &fdt::Fdt<'_>) -> Result<(), DeviceTreeSysfsInstallError> {
    let candidate = Arc::new(
        DeviceTreeFirmware::from_fdt(tree).map_err(DeviceTreeSysfsInstallError::InvalidFdt)?,
    );
    let mut installed = DEVICE_TREE_FIRMWARE.lock();
    if let Some(current) = installed.as_ref() {
        return if current.boot_blob.as_ref() == candidate.boot_blob.as_ref() {
            Ok(())
        } else {
            Err(DeviceTreeSysfsInstallError::AlreadyInstalled)
        };
    }
    *installed = Some(candidate);
    drop(installed);
    // sysfs 可能早于固件安装完成；清除此前 lookup 产生的正/负缓存，使首次发布
    // 对所有已挂载 sysfs 实例立即可见。
    invalidate_firmware_children(&["fdt", "devicetree"]);
    Ok(())
}

/// 校验并安装一份原始 FDT blob。
///
/// 调用方的切片无需具有 `'static` 生命周期；sysfs 会持有自己的精确副本。
pub fn install_device_tree_blob(blob: &[u8]) -> Result<(), DeviceTreeSysfsInstallError> {
    let tree = fdt::Fdt::parse(blob).map_err(DeviceTreeSysfsInstallError::InvalidFdt)?;
    install_device_tree(&tree)
}

/// 返回启动 Device Tree 是否已经发布到 sysfs。
pub fn device_tree_installed() -> bool {
    DEVICE_TREE_FIRMWARE.lock().is_some()
}

fn installed_device_tree() -> Option<Arc<DeviceTreeFirmware>> {
    DEVICE_TREE_FIRMWARE.lock().clone()
}

fn remove_live_device_tree_seeds(tree: &mut fdt::OwnedTree) {
    for chosen in tree
        .root
        .children
        .iter_mut()
        .filter(|node| matches!(node.name.as_str(), "chosen" | "chosen@0"))
    {
        chosen
            .properties
            .retain(|property| !matches!(property.name.as_str(), "rng-seed" | "kaslr-seed"));
    }
}

/// 原子地向 `/sys/firmware/devicetree/base` 应用一份标准 dtc/Linux overlay。
///
/// overlay 的解析、fixup、合并和规范 v17 序列化都在当前 live blob 的私有副本上
/// 完成。只有结果完整通过校验且基线在构建期间未被其他 overlay 更新时，才交换 live
/// `Arc`；任何错误都不会改变已发布目录。`/sys/firmware/fdt` 始终保持安装时清理过
/// seed 的启动 blob。同一时刻只允许一个 overlay 事务进入语义提交；并发或重入更新
/// 返回 UpdateInProgress，调用方可在稍后重试。
pub fn apply_device_tree_overlay(blob: &[u8]) -> Result<(), DeviceTreeSysfsOverlayError> {
    let firmware = installed_device_tree().ok_or(DeviceTreeSysfsOverlayError::NotInstalled)?;
    let _update = firmware
        .begin_overlay_update()
        .map_err(|()| DeviceTreeSysfsOverlayError::UpdateInProgress)?;
    let base_blob = firmware.live_blob();
    let mut tree = fdt::OwnedTree::parse(base_blob.as_ref())
        .map_err(DeviceTreeSysfsOverlayError::InvalidLiveTree)?;
    tree.apply_overlay_blob(blob)
        .map_err(DeviceTreeSysfsOverlayError::InvalidOverlay)?;
    // 启动 seed 一旦消费便不得通过后续 live tree 更新重新公开。
    remove_live_device_tree_seeds(&mut tree);
    let candidate: Arc<[u8]> = tree
        .to_dtb()
        .map_err(DeviceTreeSysfsOverlayError::InvalidOutput)?
        .into();

    let commit = *DEVICE_TREE_OVERLAY_COMMIT_HOOK.lock();
    if let Some(commit) = commit {
        commit(base_blob.as_ref(), candidate.as_ref())
            .map_err(DeviceTreeSysfsOverlayError::RuntimeRejected)?;
    }
    let mut live_blob = firmware.live_blob.lock();
    debug_assert!(Arc::ptr_eq(&base_blob, &live_blob));
    *live_blob = candidate;
    drop(live_blob);
    invalidate_device_tree_dentries();
    Ok(())
}

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
const DEVICES_SYSTEM_CLOCKEVENTS_INO: u64 = 32;
const KERNEL_UEVENT_SEQNUM_INO: u64 = 33;
const KERNEL_UEVENT_HELPER_INO: u64 = 34;
const KERNEL_HOTPLUG_INO: u64 = 35;
const POWER_STATE_INO: u64 = 41;
const POWER_WAKEUP_COUNT_INO: u64 = 42;
const KERNEL_HOSTNAME_INO: u64 = 19;
const KERNEL_OSTYPE_INO: u64 = 20;
const KERNEL_OSRELEASE_INO: u64 = 21;
const KERNEL_VERSION_INO: u64 = 22;
const KERNEL_CMDLINE_INO: u64 = 23;
const KERNEL_DEVICE_FUNCTIONS_INO: u64 = 24;
const KERNEL_NET_STATS_INO: u64 = 25;
#[cfg(feature = "performance-profile")]
const KERNEL_PROFILE_STATS_INO: u64 = 26;
#[cfg(feature = "performance-profile")]
const KERNEL_PROFILE_CONTROL_INO: u64 = 27;
#[cfg(feature = "performance-profile")]
const KERNEL_PROFILE_SAMPLES_INO: u64 = 28;
#[cfg(feature = "performance-profile")]
const KERNEL_PROFILE_CATALOG_INO: u64 = 29;
#[cfg(feature = "performance-profile")]
const KERNEL_PROFILE_TRACE_INO: u64 = 73;
#[cfg(feature = "performance-profile")]
const KERNEL_PROFILE_SNAPSHOT_INO: u64 = 74;
#[cfg(feature = "performance-profile")]
const KERNEL_PROFILE_HEALTH_INO: u64 = 75;
const KERNEL_ELM_DIR_INO: u64 = 80;
const KERNEL_ELM_FILE_BASE_INO: u64 = 81;
const KERNEL_MM_DIR_INO: u64 = 90;
const KERNEL_MM_THP_DIR_INO: u64 = 91;
const KERNEL_MM_THP_KHUGEPAGED_DIR_INO: u64 = 92;
const KERNEL_MM_KSM_DIR_INO: u64 = 93;
const KERNEL_MM_HUGEPAGES_DIR_INO: u64 = 94;
const KERNEL_MM_THP_ENABLED_INO: u64 = 95;
const KERNEL_MM_THP_DEFRAG_INO: u64 = 96;
const KERNEL_MM_THP_SHMEM_ENABLED_INO: u64 = 97;
const KERNEL_MM_THP_USE_ZERO_PAGE_INO: u64 = 98;
const KERNEL_MM_KHP_SCAN_SLEEP_INO: u64 = 99;
const KERNEL_MM_KHP_ALLOC_SLEEP_INO: u64 = 100;
const KERNEL_MM_KHP_MAX_PTES_NONE_INO: u64 = 101;
const KERNEL_MM_KHP_PAGES_COLLAPSED_INO: u64 = 102;
const KERNEL_MM_KSM_RUN_INO: u64 = 110;
const KERNEL_MM_KSM_MERGE_ACROSS_NODES_INO: u64 = 111;
const KERNEL_MM_KSM_PAGES_SHARED_INO: u64 = 112;
const KERNEL_MM_KSM_PAGES_SHARING_INO: u64 = 113;
const KERNEL_MM_KSM_PAGES_UNSHARED_INO: u64 = 114;
const KERNEL_MM_KSM_PAGES_VOLATILE_INO: u64 = 115;
const KERNEL_MM_KSM_FULL_SCANS_INO: u64 = 116;
const KERNEL_MM_KSM_MAX_PAGE_SHARING_INO: u64 = 117;
const KERNEL_MM_HUGEPAGES_SUBDIR_INO: u64 = 120;
const KERNEL_MM_HP_NR_INO: u64 = 121;
const KERNEL_MM_HP_NR_OVERCOMMIT_INO: u64 = 122;
const KERNEL_MM_HP_FREE_INO: u64 = 123;
const KERNEL_MM_HP_RESV_INO: u64 = 124;
const KERNEL_MM_HP_SURPLUS_INO: u64 = 125;
const DEV_BLOCK_DIR_INO: u64 = 30;
const DEV_CHAR_DIR_INO: u64 = 31;
const FS_CGROUP_INO: u64 = 40;

const CPU_BASE: u64 = 10_000_000;
const CPU_SLOTS: u64 = 4;
const CPU_TOPOLOGY_BASE: u64 = 20_000_000;
const CPU_TOPOLOGY_SLOTS: u64 = 8;

static SYSFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);
static SYSFS_INO_REGISTRY: Spinlock<Option<SysfsInoRegistry>> = Spinlock::new(None);
static ELM_SYSFS_RENDERER: Spinlock<Option<ElmSysfsRenderer>> = Spinlock::new(None);
static SYSFS_ROOT_DENTRIES: Spinlock<Vec<Weak<Dentry>>> = Spinlock::new(Vec::new());

const SYSFS_MAGIC: u64 = 0x6265_6572;
const SYSFS_DYNAMIC_INO_START: u64 = 1_000_000_000;
const SYSFS_BLOCK_CLASS: &str = "block";
const SYSFS_CHAR_CLASS: &str = "char";
const SYSFS_NET_CLASS: &str = NET_CLASS.as_str();

/// Linux 常驻 class 目录:即使内核当前没有对应设备,也保持稳定空目录。
///
/// `mem`/`tty`/`misc` 还会被 device_numbers 的 `major_name` 投影真实设备;
/// `thermal`/`wdt` 由 PnP function 投影;input/led/gpio/power_supply 目前没有
/// 设备模型,先提供稳定空目录供用户空间探测布局。
const STATIC_SYSFS_CLASSES: &[&str] = &[
    "mem",
    "tty",
    "thermal",
    "wdt",
    "input",
    "misc",
    "gpio",
    "power_supply",
    "leds",
];

/// Linux 常驻总线目录:dev core 没有 cpu/memory/clockevents/clocksource/virtio
/// 的 PnP 设备模型,这里提供稳定空 `devices` 子目录,与 Linux 布局保持一致。
const STATIC_SYSFS_BUSES: &[&str] = &["cpu", "memory", "clockevents", "clocksource", "virtio"];

fn is_static_sysfs_bus(name: &str) -> bool {
    STATIC_SYSFS_BUSES.contains(&name)
}

/// uevent 序号:Linux 中 `kobject_uevent` 每投递一次事件就 +1,udev 据此检测漏事件。
///
/// 本内核尚未接入 netlink uevent 通道,这里只维护内核内计数器;`/sys/devices/*/uevent`
/// 写一个合法动作词时递增,使 `/sys/kernel/uevent_seqnum` 具备可观测语义。
static UEVENT_SEQNUM: AtomicU64 = AtomicU64::new(0);

/// 用户态 uevent helper 路径(对应 Linux `/sys/kernel/uevent_helper`)。
static UEVENT_HELPER_PATH: Spinlock<String> = Spinlock::new(String::new());

pub type ElmSysfsRenderer = fn(&str) -> String;

fn register_sysfs_root_dentry(root: &Arc<Dentry>) {
    let mut roots = SYSFS_ROOT_DENTRIES.lock();
    roots.retain(|root| root.strong_count() != 0);
    roots.push(Arc::downgrade(root));
}

/// live tree 交换后逐出各 sysfs 实例中旧的 Device Tree dentry 子树。
///
/// 这同时清除正向和负向缓存，使 API 返回后重新进行的路径 lookup 必定进入新的
/// `InodeOps` 投影；已经打开的文件仍持有自己的 inode/FileOps `Arc` 快照。
fn invalidate_device_tree_dentries() {
    invalidate_firmware_children(&["devicetree"]);
}

fn invalidate_firmware_children(names: &[&str]) {
    let roots = {
        let mut tracked = SYSFS_ROOT_DENTRIES.lock();
        let mut roots = Vec::new();
        tracked.retain(|root| {
            let Some(root) = root.upgrade() else {
                return false;
            };
            roots.push(root);
            true
        });
        roots
    };

    for root in roots {
        let Some(firmware) = vfs::DCACHE.get(&root, "firmware") else {
            continue;
        };
        for name in names {
            if let Some(child) = vfs::DCACHE.get(&firmware, name) {
                vfs::DCACHE.invalidate_subtree(&child);
            }
        }
    }
}

pub fn register_elm_renderer(renderer: ElmSysfsRenderer) {
    *ELM_SYSFS_RENDERER.lock() = Some(renderer);
}

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
        // `NetDeviceId` 是网络栈在本次启动内稳定且不复用的编号,起始值为 1,
        // 语义与 Linux 的 interface index 一致,直接作为 ifindex 暴露。此前这里
        // 额外 +1 造成 off-by-one,现移除。
        iface_id
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

    fn firmware_fdt() -> Self {
        Self::raw("firmware/fdt".into())
    }

    fn firmware_device_tree() -> Self {
        Self::raw("firmware/devicetree".into())
    }

    fn device_tree_node(node: &DeviceTreeNodeId) -> Self {
        let mut key = "firmware/devicetree/node".to_string();
        node.append_key_suffix(&mut key);
        Self::raw(key)
    }

    fn device_tree_property(node: &DeviceTreeNodeId, name: &str) -> Self {
        let mut key = "firmware/devicetree/property".to_string();
        node.append_key_suffix(&mut key);
        // FDT 规范不允许节点名或属性名包含 NUL；分隔符因此不会造成 key 歧义。
        key.push('\0');
        key.push_str(name);
        Self::raw(key)
    }

    fn numa_root() -> Self {
        Self::raw("devices/system/node".into())
    }

    fn numa_root_slot(slot: u64) -> Self {
        Self::raw(format!("devices/system/node/slot/{slot}"))
    }

    fn numa_node(node_id: u32) -> Self {
        Self::raw(format!("devices/system/node/node{node_id}"))
    }

    fn numa_node_slot(node_id: u32, slot: u64) -> Self {
        Self::raw(format!("devices/system/node/node{node_id}/slot/{slot}"))
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
    geometry: BlockGeometry,
    features: BlockFeatures,
    attributes: BlockAttributes,
    limits: BlockLimits,
    io_stats: BlockIoStatsSnapshot,
    class_name: &'static str,
    /// 父块设备目录名(分区→整盘);整盘无块级父设备时为 `None`。
    parent_name: Option<String>,
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
    bus_type: String,
    id: PnpId,
    state: &'static str,
    driver: Option<String>,
    parent: Option<String>,
    child_count: usize,
    functions: Vec<SysPnpFunctionSnapshot>,
    resources: Vec<PnpOwnedResourceSnapshot>,
    deferred_dependency: Option<PnpDependency>,
}

#[derive(Clone)]
struct SysPnpFunctionSnapshot {
    class_name: String,
    dev_name: String,
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
    pnp_buses: Vec<String>,
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
        PnpDependency::DtbProvider { kind, phandle } => {
            format!("dt-provider:{kind}:{phandle}")
        }
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

/// 把设备号表的传统主设备名映射到 Linux sysfs class。
///
/// `null/zero/full/kmsg/random/urandom` 的 major_name 是 `mem`,`console/tty/ptmx`
/// 是 `console`,`tty0..7` 是 `tty`;Linux 把它们分别归入 `/sys/class/mem` 和
/// `/sys/class/tty`(console 与 tty 同属 tty class)。misc 主设备名归入
/// `/sys/class/misc`。其余(私有动态 major 或未声明主设备名的)返回 `None`,
/// 调用方回退到 function class。
fn sysfs_class_for_major_name(major_name: &str) -> Option<&'static str> {
    match major_name {
        "mem" => Some("mem"),
        "console" | "tty" => Some("tty"),
        "misc" => Some("misc"),
        _ => None,
    }
}

/// 由 PnP function 推导 sysfs class(仅覆盖没有 devtmpfs 投影的设备类型)。
///
/// RTC 已经有 rtc projector 并走 device_numbers 投影路径,这里不再重复发布。
/// WDT 的 class_id 是内建 `wdt`;温度传感器是动态 class,只能通过 operation
/// contract `mygo.device.thermal@1` 识别。
fn pnp_function_sysfs_class(
    function: &dyn crate::dev::function::DeviceFunction,
) -> Option<&'static str> {
    match function.class_name() {
        "wdt" => Some("wdt"),
        "dynamic" => function.operation_contract().and_then(|contract| {
            if contract.starts_with("mygo.device.thermal@1") {
                Some("thermal")
            } else {
                None
            }
        }),
        _ => None,
    }
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

        // Linux 常驻 class 目录先稳定发布,后续真实设备以 class node 挂入。
        for class in STATIC_SYSFS_CLASSES {
            push_sys_class(&mut snap, class);
        }

        // PnP 设备是 dev core 的硬件身份与 driver 绑定视图。这里先把它们放入
        // sysfs 快照，即便设备没有 `/dev` 投影，也能在 `/sys/devices/pnp` 中诊断。
        for dev in PNP_DEVICES.try_list().unwrap_or_default() {
            let bus_type = dev.info.bus_name().to_string();
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
            // 没有 devtmpfs 投影的 function(thermal/wdt)也要进入 `/sys/class`;
            // class node 直接链接到 PnP 设备目录,保持 class 视图的硬件反向关联。
            let raw_functions = dev.try_functions().unwrap_or_default();
            let mut functions = Vec::new();
            for function in raw_functions {
                let class_name = function.class_name().to_string();
                let dev_name = function.dev_name().to_string();
                if let Some(sysfs_class) = pnp_function_sysfs_class(function.as_ref()) {
                    push_class_node(
                        &mut snap,
                        sysfs_class,
                        sysfs_component_name(&dev_name),
                        SysClassNodeKind::Symlink {
                            target: format!("../../devices/pnp/{}/{}", bus_type, sysfs_name),
                        },
                    );
                }
                functions.push(SysPnpFunctionSnapshot {
                    class_name,
                    dev_name,
                });
            }
            let resources = dev.try_owned_resources().unwrap_or_default();
            let parent = dev.parent().map(|parent| parent.name.to_string());
            let child_count = dev
                .try_children()
                .map(|children| children.len())
                .unwrap_or(0);
            snap.pnp_devices.push(SysPnpDeviceSnapshot {
                sysfs_name,
                name: dev.name.to_string(),
                bus_type: bus_type.clone(),
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
            let dev = projection.dev();
            let sysfs_name = sysfs_unique_name_with_rdev(dev.name(), projection.rdev(), |name| {
                snap.blocks.iter().any(|block| block.sysfs_name == name)
            });
            snap.blocks.push(BlockDevSnapshot {
                sysfs_name,
                rdev: projection.rdev(),
                geometry: *dev.geometry(),
                features: dev.features(),
                attributes: dev.attributes(),
                limits: *dev.limits(),
                io_stats: dev.io_stats(),
                class_name: projection.class_id().as_str(),
                parent_name: dev.parent().map(|parent| parent.name().to_string()),
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
                    // 传统主设备名(mem/console/tty/misc)优先决定 Linux class,
                    // 否则回退到 function class(rtc/wdt/char 等)。
                    let class_name =
                        sysfs_class_for_major_name(&record.major_name).unwrap_or_else(|| {
                            class_for_devnode(&devnode_classes, &record.node_name, SYSFS_CHAR_CLASS)
                        });
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
        for iface in net::device::snapshot_devices() {
            push_class_node(
                &mut snap,
                SYSFS_NET_CLASS,
                iface.name.into_string(),
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
fn firmware_fdt_ino() -> u64 {
    sysfs_dynamic_ino(SysfsKey::firmware_fdt())
}
fn firmware_device_tree_ino() -> u64 {
    sysfs_dynamic_ino(SysfsKey::firmware_device_tree())
}
fn device_tree_node_ino(node: &DeviceTreeNodeId) -> u64 {
    sysfs_dynamic_ino(SysfsKey::device_tree_node(node))
}
fn device_tree_property_ino(node: &DeviceTreeNodeId, name: &str) -> u64 {
    sysfs_dynamic_ino(SysfsKey::device_tree_property(node, name))
}
fn numa_root_ino() -> u64 {
    sysfs_dynamic_ino(SysfsKey::numa_root())
}
fn numa_root_slot_ino(slot: u64) -> u64 {
    sysfs_dynamic_ino(SysfsKey::numa_root_slot(slot))
}
fn numa_node_ino(node_id: u32) -> u64 {
    sysfs_dynamic_ino(SysfsKey::numa_node(node_id))
}
fn numa_node_slot_ino(node_id: u32, slot: u64) -> u64 {
    sysfs_dynamic_ino(SysfsKey::numa_node_slot(node_id, slot))
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
    HoldersDir,
    Stat,
    Inflight,
    Periodic,
    Diskseq,
    DeviceLink,
    SubsystemLink,
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
            Self::HoldersDir => 6,
            Self::Stat => 7,
            Self::Inflight => 8,
            Self::Periodic => 9,
            Self::Diskseq => 10,
            Self::DeviceLink => 11,
            Self::SubsystemLink => 12,
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
    DiscardMaxBytes,
    DiscardGranularity,
    WriteZeroesMaxBytes,
    MaxSectorsKb,
    MaxSegments,
    MaxSegmentSize,
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
            Self::DiscardMaxBytes => 6,
            Self::DiscardGranularity => 7,
            Self::WriteZeroesMaxBytes => 8,
            Self::MaxSectorsKb => 9,
            Self::MaxSegments => 10,
            Self::MaxSegmentSize => 11,
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

fn render_netdev_file(iface: &net::device::NetDeviceSnapshot, slot: NetDevSlot) -> String {
    use alloc::fmt::Write;
    let mut s = String::new();
    match slot {
        NetDevSlot::Type => {
            // 当前网络设备 snapshot 尚未携带链路层类型；用户视图策略给出以太网
            // 默认值，后续 typed capability 可覆盖该字段。
            let _ = writeln!(s, "{}", SYSFS_USER_VIEW_POLICY.net_link_type_ether);
        }
        NetDevSlot::Address => {
            let mac = iface.mac_address;
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
            let flags = if iface.running { 0x41u32 } else { 0 };
            let _ = writeln!(s, "0x{:x}", flags);
        }
        NetDevSlot::IfIndex => {
            let _ = writeln!(s, "{}", SYSFS_USER_VIEW_POLICY.net_ifindex(iface.id.raw()));
        }
        NetDevSlot::TxQueueLen => {
            let _ = writeln!(s, "{}", SYSFS_USER_VIEW_POLICY.net_tx_queue_len);
        }
        NetDevSlot::Carrier => {
            let carrier = if iface.running { "1" } else { "0" };
            let _ = writeln!(s, "{}", carrier);
        }
        NetDevSlot::Operstate => {
            let state = if iface.running { "up" } else { "down" };
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
enum NumaRootSlot {
    HasCpu,
    HasMemory,
    Online,
    Possible,
}

impl NumaRootSlot {
    const ALL: &'static [Self] = &[Self::HasCpu, Self::HasMemory, Self::Online, Self::Possible];

    fn to_u64(self) -> u64 {
        match self {
            Self::HasCpu => 0,
            Self::HasMemory => 1,
            Self::Online => 2,
            Self::Possible => 3,
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::HasCpu => "has_cpu",
            Self::HasMemory => "has_memory",
            Self::Online => "online",
            Self::Possible => "possible",
        }
    }
}

fn numa_root_slot_by_name(name: &str) -> Option<NumaRootSlot> {
    NumaRootSlot::ALL
        .iter()
        .find(|slot| slot.file_name() == name)
        .copied()
}

#[derive(Clone, Copy)]
enum NumaNodeSlot {
    CpuList,
    CpuMap,
    Distance,
}

impl NumaNodeSlot {
    const ALL: &'static [Self] = &[Self::CpuList, Self::CpuMap, Self::Distance];

    fn to_u64(self) -> u64 {
        match self {
            Self::CpuList => 0,
            Self::CpuMap => 1,
            Self::Distance => 2,
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::CpuList => "cpulist",
            Self::CpuMap => "cpumap",
            Self::Distance => "distance",
        }
    }
}

fn numa_node_slot_by_name(name: &str) -> Option<NumaNodeSlot> {
    NumaNodeSlot::ALL
        .iter()
        .find(|slot| slot.file_name() == name)
        .copied()
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
    NumaRoot {
        slot: NumaRootSlot,
    },
    NumaNode {
        node_id: u32,
        slot: NumaNodeSlot,
    },
    CpuOnline,
    CpuPossible,
    CpuPresent,
    UeventSeqnum,
    UeventHelper,
    Hotplug,
    PowerState,
    PowerWakeupCount,
    Hostname,
    Ostype,
    Osrelease,
    Version,
    Cmdline,
    DeviceFunctions,
    NetStats,
    #[cfg(feature = "performance-profile")]
    ProfileStats,
    #[cfg(feature = "performance-profile")]
    ProfileControl,
    #[cfg(feature = "performance-profile")]
    ProfileSamples,
    #[cfg(feature = "performance-profile")]
    ProfileCatalog,
    #[cfg(feature = "performance-profile")]
    ProfileTrace,
    #[cfg(feature = "performance-profile")]
    ProfileSnapshot,
    #[cfg(feature = "performance-profile")]
    ProfileHealth,
    Elm {
        slot: ElmSysfsSlot,
    },
    ThpEnabled,
    ThpDefrag,
    ThpShmemEnabled,
    ThpUseZeroPage,
    KhpScanSleepMs,
    KhpAllocSleepMs,
    KhpMaxPtesNone,
    KhpPagesCollapsed,
    KsmRun,
    KsmMergeAcrossNodes,
    KsmPagesShared,
    KsmPagesSharing,
    KsmPagesUnshared,
    KsmPagesVolatile,
    KsmFullScans,
    KsmMaxPageSharing,
    HugepagesNr,
    HugepagesNrOvercommit,
    HugepagesFree,
    HugepagesResv,
    HugepagesSurplus,
    NetDev {
        iface_id: u32,
        slot: NetDevSlot,
    },
}

#[derive(Clone, Copy)]
enum ElmSysfsSlot {
    Core,
    Policy,
    Health,
    Menu,
    Topology,
    Ports,
    Providers,
    Bindings,
    Events,
    Audit,
    Api,
    Trust,
    ProjectionSources,
    Journal,
    Executions,
    OwnedResources,
    ResourceAccounting,
    Workers,
    Diagnostics,
}

impl ElmSysfsSlot {
    const ALL: &'static [Self] = &[
        Self::Core,
        Self::Policy,
        Self::Health,
        Self::Menu,
        Self::Topology,
        Self::Ports,
        Self::Providers,
        Self::Bindings,
        Self::Events,
        Self::Audit,
        Self::Api,
        Self::Trust,
        Self::ProjectionSources,
        Self::Journal,
        Self::Executions,
        Self::OwnedResources,
        Self::ResourceAccounting,
        Self::Workers,
        Self::Diagnostics,
    ];

    fn to_u64(self) -> u64 {
        match self {
            Self::Core => 0,
            Self::Policy => 1,
            Self::Health => 2,
            Self::Menu => 3,
            Self::Topology => 4,
            Self::Ports => 5,
            Self::Providers => 6,
            Self::Bindings => 7,
            Self::Events => 8,
            Self::Audit => 9,
            Self::Api => 10,
            Self::Trust => 11,
            Self::ProjectionSources => 12,
            Self::Journal => 13,
            Self::Executions => 14,
            Self::OwnedResources => 15,
            Self::ResourceAccounting => 16,
            Self::Workers => 18,
            Self::Diagnostics => 17,
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Policy => "policy",
            Self::Health => "health",
            Self::Menu => "menu",
            Self::Topology => "topology",
            Self::Ports => "ports",
            Self::Providers => "providers",
            Self::Bindings => "bindings",
            Self::Events => "events",
            Self::Audit => "audit",
            Self::Api => "api",
            Self::Trust => "trust",
            Self::ProjectionSources => "projection-sources",
            Self::Journal => "journal",
            Self::Executions => "executions",
            Self::OwnedResources => "owned-resources",
            Self::ResourceAccounting => "resource-accounting",
            Self::Workers => "workers",
            Self::Diagnostics => "diagnostics",
        }
    }
}

fn elm_sysfs_slot_by_name(name: &str) -> Option<ElmSysfsSlot> {
    ElmSysfsSlot::ALL
        .iter()
        .find(|slot| slot.file_name() == name)
        .copied()
}

fn kernel_elm_slot_ino(slot: ElmSysfsSlot) -> u64 {
    KERNEL_ELM_FILE_BASE_INO + slot.to_u64()
}

// ─── 内容渲染 ────────────────────────────────────────────────

fn render_block_dev_file(snap: &SysSnapshot, idx: usize, slot: BlockDevSlot) -> String {
    let dev = &snap.blocks[idx];
    let geom = &dev.geometry;
    let features = dev.features;
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
            if dev.attributes.removable() {
                "1\n".into()
            } else {
                "0\n".into()
            }
        }
        BlockDevSlot::Dev => format_rdev(snap.blocks[idx].rdev),
        BlockDevSlot::Range => "1\n".into(),
        // holders 是目录,symlink(device/subsystem)不渲染内容;这里只覆盖枚举穷尽。
        BlockDevSlot::HoldersDir | BlockDevSlot::DeviceLink | BlockDevSlot::SubsystemLink => {
            String::new()
        }
        BlockDevSlot::Diskseq => format!("{}\n", dev.attributes.diskseq().unwrap_or(0)),
        BlockDevSlot::Stat => {
            let stats = dev.io_stats;
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
            let stats = dev.io_stats;
            format!("{} {}\n", stats.read_inflight, stats.write_inflight)
        }
        BlockDevSlot::Periodic => String::new(),
        BlockDevSlot::QueueDir => String::new(),
    }
}

fn render_block_queue_file(snap: &SysSnapshot, idx: usize, slot: BlockQueueSlot) -> String {
    let dev = &snap.blocks[idx];
    let geom = &dev.geometry;
    let features = dev.features;
    match slot {
        BlockQueueSlot::Lbs => format!("{}\n", geom.logical_block_size().get()),
        BlockQueueSlot::Pbs => format!("{}\n", geom.physical_block_size().get()),
        BlockQueueSlot::Rotational => {
            if dev.attributes.rotational() {
                "1\n".into()
            } else {
                "0\n".into()
            }
        }
        BlockQueueSlot::NrRequests => {
            // 没有真实队列深度的设备使用 sysfs 用户视图默认值；VirtIO 等驱动会填实际协商值。
            let depth = dev
                .attributes
                .queue_depth()
                .map(|n| n.get())
                .unwrap_or(SYSFS_USER_VIEW_POLICY.block_nr_requests);
            format!("{}\n", depth)
        }
        BlockQueueSlot::HwSectorSize => format!("{}\n", geom.logical_block_size().get()),
        BlockQueueSlot::DiscardZeroes => {
            if features.contains(crate::dev::block::BlockFeatures::DISCARD_ZEROES) {
                "1\n".into()
            } else {
                "0\n".into()
            }
        }
        // 范围类命令(trim/write-zeroes)与 scatter/gather 限制在 BlockLimits 中
        // 已有真实数据源;缺失时输出 0,与 Linux「未声明限制即无限制/不支持」一致。
        BlockQueueSlot::DiscardMaxBytes => {
            let bytes = dev
                .limits
                .discard_limits()
                .and_then(|limits| limits.max_blocks_per_io())
                .map(|blocks| blocks.get() as u64 * geom.logical_block_size().get() as u64)
                .unwrap_or(0);
            format!("{}\n", bytes)
        }
        BlockQueueSlot::DiscardGranularity => {
            let bytes = dev
                .limits
                .discard_limits()
                .and_then(|limits| limits.alignment_blocks())
                .map(|blocks| blocks.get() as u64 * geom.logical_block_size().get() as u64)
                .unwrap_or(0);
            format!("{}\n", bytes)
        }
        BlockQueueSlot::WriteZeroesMaxBytes => {
            let bytes = dev
                .limits
                .write_zeroes_limits()
                .and_then(|limits| limits.max_blocks_per_io())
                .map(|blocks| blocks.get() as u64 * geom.logical_block_size().get() as u64)
                .unwrap_or(0);
            format!("{}\n", bytes)
        }
        BlockQueueSlot::MaxSectorsKb => {
            let kb = dev
                .limits
                .max_blocks_per_io()
                .map(|blocks| blocks.get() as u64 * geom.logical_block_size().get() as u64 / 1024)
                .unwrap_or(0);
            format!("{}\n", kb)
        }
        BlockQueueSlot::MaxSegments => {
            let segments = dev.limits.max_data_segments().map(|n| n.get()).unwrap_or(0);
            format!("{}\n", segments)
        }
        BlockQueueSlot::MaxSegmentSize => {
            let bytes = dev
                .limits
                .max_data_segment_size()
                .map(|n| n.get())
                .unwrap_or(0);
            format!("{}\n", bytes)
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
            .as_deref()
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
                    format_args!("{}:{}\n", function.class_name, function.dev_name),
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

fn push_u32_range(out: &mut String, start: u32, end: u32) {
    use core::fmt::Write;

    if !out.is_empty() {
        out.push(',');
    }
    if start == end {
        let _ = write!(out, "{start}");
    } else {
        let _ = write!(out, "{start}-{end}");
    }
}

/// 按 Linux bitmap list ABI 格式化稀疏 node state。
fn format_numa_node_list(nodes: &[u32]) -> String {
    let mut nodes = nodes.to_vec();
    nodes.sort_unstable();
    nodes.dedup();

    let mut out = String::new();
    let mut iter = nodes.into_iter().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while let Some(next) = iter.peek().copied() {
            if next != end.saturating_add(1) {
                break;
            }
            end = iter.next().unwrap_or(end);
        }
        push_u32_range(&mut out, start, end);
    }
    out.push('\n');
    out
}

/// 按 Linux cpumap ABI 输出十六进制 bitmap。
///
/// 每 32 bit 使用逗号分组，低位组固定为八位十六进制；最高组只输出
/// `nr_cpu_ids` 实际需要的位宽，例如 16 CPU 系统输出 `ffff`。
fn format_linux_cpumap(mask: u64, width_bits: usize) -> String {
    use core::fmt::Write;

    let width_bits = width_bits.clamp(1, u64::BITS as usize);
    let groups = width_bits.div_ceil(32);
    let high_bits = width_bits - (groups - 1) * 32;
    let mut out = String::new();
    for group in (0..groups).rev() {
        if !out.is_empty() {
            out.push(',');
        }
        let value = ((mask >> (group * 32)) & u64::from(u32::MAX)) as u32;
        let digits = if group == groups - 1 {
            high_bits.div_ceil(4)
        } else {
            8
        };
        let _ = write!(out, "{value:0digits$x}");
    }
    out.push('\n');
    out
}

const LINUX_DEFAULT_REMOTE_DISTANCE: u32 = 20;

/// 单次访问使用的 NUMA sysfs 只读快照。
///
/// `possible` 保留固件距离矩阵中仅被引用的节点；`online` 则只包含至少拥有一个
/// 可支持 CPU 或非空 RAM 范围的节点，匹配 Linux node device 的发布条件。
#[derive(Clone, Debug)]
struct NumaSysfsView {
    topology: numa::NumaTopology,
    cpu_assignments: Vec<cpu::CpuNumaEntry>,
    possible_nodes: Vec<u32>,
    online_nodes: Vec<u32>,
    cpu_nodes: Vec<u32>,
    memory_nodes: Vec<u32>,
    cpu_bitmap_width: usize,
}

impl NumaSysfsView {
    fn snapshot() -> Self {
        Self::new(
            numa::snapshot_topology(),
            cpu::snapshot_numa_topology(),
            supported_cpu_mask() | online_cpu_mask(),
        )
    }

    fn new(
        topology: numa::NumaTopology,
        mut cpu_assignments: Vec<cpu::CpuNumaEntry>,
        supported_cpus: u64,
    ) -> Self {
        // 当前调度 CPU ABI 使用 u64 mask；忽略无法被内核支持的逻辑编号，避免损坏
        // 固件输入令 cpumap 产生无界输出。
        cpu_assignments.retain(|entry| entry.logical_id < u64::BITS);
        cpu_assignments.sort_unstable_by_key(|entry| (entry.logical_id, entry.node_id));
        cpu_assignments.dedup();

        let mut cpu_nodes = cpu_assignments
            .iter()
            .map(|entry| entry.node_id)
            .collect::<Vec<_>>();
        cpu_nodes.sort_unstable();
        cpu_nodes.dedup();

        let mut memory_nodes = topology
            .memory
            .iter()
            .filter(|range| range.size != 0)
            .map(|range| range.node_id)
            .collect::<Vec<_>>();
        memory_nodes.sort_unstable();
        memory_nodes.dedup();

        let mut online_nodes = cpu_nodes.clone();
        online_nodes.extend_from_slice(&memory_nodes);
        online_nodes.sort_unstable();
        online_nodes.dedup();

        let mut possible_nodes = topology.node_ids.clone();
        possible_nodes.extend_from_slice(&online_nodes);
        possible_nodes.extend(
            topology
                .distances
                .iter()
                .flat_map(|entry| [entry.from, entry.to]),
        );
        possible_nodes.sort_unstable();
        possible_nodes.dedup();

        let mask_width = (u64::BITS - supported_cpus.leading_zeros()) as usize;
        let assigned_width = cpu_assignments
            .iter()
            .map(|entry| entry.logical_id as usize + 1)
            .max()
            .unwrap_or(0);

        Self {
            topology,
            cpu_assignments,
            possible_nodes,
            online_nodes,
            cpu_nodes,
            memory_nodes,
            cpu_bitmap_width: mask_width.max(assigned_width).max(1),
        }
    }

    fn contains_online_node(&self, node_id: u32) -> bool {
        self.online_nodes.binary_search(&node_id).is_ok()
    }

    fn cpu_mask(&self, node_id: u32) -> u64 {
        self.cpu_assignments
            .iter()
            .filter(|entry| entry.node_id == node_id)
            .fold(0u64, |mask, entry| mask | (1u64 << entry.logical_id))
    }

    fn render_root_file(&self, slot: NumaRootSlot) -> String {
        match slot {
            NumaRootSlot::HasCpu => format_numa_node_list(&self.cpu_nodes),
            NumaRootSlot::HasMemory => format_numa_node_list(&self.memory_nodes),
            NumaRootSlot::Online => format_numa_node_list(&self.online_nodes),
            NumaRootSlot::Possible => format_numa_node_list(&self.possible_nodes),
        }
    }

    fn render_node_file(&self, node_id: u32, slot: NumaNodeSlot) -> String {
        match slot {
            NumaNodeSlot::CpuList => format_cpu_mask_range(self.cpu_mask(node_id)),
            NumaNodeSlot::CpuMap => {
                format_linux_cpumap(self.cpu_mask(node_id), self.cpu_bitmap_width)
            }
            NumaNodeSlot::Distance => {
                use core::fmt::Write;

                let mut out = String::new();
                for &target in &self.online_nodes {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    let distance = self.topology.distance(node_id, target).unwrap_or_else(|| {
                        if node_id == target {
                            fdt::NUMA_LOCAL_DISTANCE
                        } else {
                            LINUX_DEFAULT_REMOTE_DISTANCE
                        }
                    });
                    let _ = write!(out, "{distance}");
                }
                out.push('\n');
                out
            }
        }
    }
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
struct CpuTopologyView<'a> {
    package_id: u32,
    cluster_path: &'a [u32],
    core_id: u32,
    thread_id: u32,
}

fn cpu_topology_view<'a>(
    cpu_id: usize,
    entries: &'a [cpu::CpuTopologyEntry],
) -> Option<CpuTopologyView<'a>> {
    let logical_id = u32::try_from(cpu_id).ok()?;
    let entry = entries.iter().find(|entry| entry.logical_id == logical_id);

    // 固件可能只描述部分层级。这里做的是通用拓扑归一化：没有 socket 层级时
    // 说明当前快照无法区分 package，统一归入 0；没有 core/thread 层级时，
    // 使用 logical CPU 自身作为 core，thread 使用 0，保持 sibling 计算稳定。
    Some(CpuTopologyView {
        package_id: entry.and_then(|entry| entry.socket_id).unwrap_or(0),
        cluster_path: entry.map_or(&[], |entry| entry.cluster_path.as_ref()),
        core_id: entry.and_then(|entry| entry.core_id).unwrap_or(logical_id),
        thread_id: entry.and_then(|entry| entry.thread_id).unwrap_or(0),
    })
}

fn cpu_topology_sibling_mask<F>(
    cpu_id: usize,
    entries: &[cpu::CpuTopologyEntry],
    same_group: F,
) -> u64
where
    F: Fn(CpuTopologyView<'_>, CpuTopologyView<'_>) -> bool,
{
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

fn same_thread_sibling(left: CpuTopologyView<'_>, right: CpuTopologyView<'_>) -> bool {
    left.package_id == right.package_id
        && left.cluster_path == right.cluster_path
        && left.core_id == right.core_id
}

fn cpu_in_mask(cpu_id: usize, mask: u64) -> bool {
    cpu_id < u64::BITS as usize && mask & (1u64 << cpu_id) != 0
}

fn render_cpu_file(_snap: &SysSnapshot, cpu_id: usize, slot: CpuSlot) -> String {
    match slot {
        CpuSlot::TopoDir => String::new(),
        // per-CPU 目录只对 online CPU 发布,online 恒为 1;possible/present 按
        // 支持掩码求值,避免与顶层 mask 语义脱节。
        CpuSlot::Online => {
            format!("{}\n", u8::from(cpu_in_mask(cpu_id, online_cpu_mask())))
        }
        CpuSlot::Possible => {
            format!("{}\n", u8::from(cpu_in_mask(cpu_id, supported_cpu_mask())))
        }
        CpuSlot::Present => {
            format!("{}\n", u8::from(cpu_in_mask(cpu_id, supported_cpu_mask())))
        }
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
            let mask = cpu_topology_sibling_mask(cpu_id, &entries, same_thread_sibling);
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

fn render_net_stats() -> String {
    use alloc::fmt::Write;
    let mut output = String::new();
    for stat in net::device::snapshot_stats() {
        let _ = writeln!(
            output,
            "device={} queue={} key={} value={}",
            stat.device.0, stat.queue.0, stat.key, stat.value,
        );
    }
    output
}

#[cfg(feature = "performance-profile")]
fn render_profile_stats() -> String {
    use alloc::fmt::Write;
    let mut output = String::new();
    let session = profiling::session_info();
    let _ = writeln!(
        output,
        "state={} enabled={} session={} generation={} phase={} active_writers={} counter_hz={} event_mask={:#x} event_mask_high={:#x} sampling={} sample_hz={} trace={} timing_shift={} effective_timing_shift={} timing_sampler={} cpu_slots={} histogram_buckets={} syscall_slots={} errno_slots={}",
        session.state.name(),
        u8::from(profiling::enabled()),
        session.session_id,
        session.generation,
        session.phase,
        session.active_writers,
        session.counter_hz,
        session.event_mask,
        session.event_mask_high,
        u8::from(session.sampling_enabled),
        session.sample_hz,
        u8::from(session.trace_enabled),
        session.timing_shift,
        profiling::effective_timing_shift(),
        session.timing_sampler,
        profiling::CPU_SLOTS,
        profiling::HISTOGRAM_BUCKETS,
        profiling::SYSCALL_SLOTS,
        profiling::ERRNO_SLOTS,
    );
    for cpu in 0..profiling::CPU_SLOTS {
        for event in profiling::Event::ALL {
            let value = profiling::snapshot(cpu, event);
            if value.calls == 0 {
                continue;
            }
            let _ = writeln!(
                output,
                "cpu={} event={} event_id={} category={} calls={} timed_samples={} sample_ratio={}/{} cycles={} bytes={} packets={} sampled_max_cycles={} sampled_wall_ns={} estimated_wall_ns={} mean_ns={} sampled_on_cpu_ns={} estimated_on_cpu_ns={} sampled_off_cpu_ns={} estimated_off_cpu_ns={} sampled_max_latency_ns={} migrations={} p50_ns={} p95_ns={} p99_ns={} hist={}",
                ProfileCpuDisplay(cpu),
                event.name(),
                event as usize,
                event.category().name(),
                value.calls,
                value.timed_samples,
                value.timed_samples,
                value.calls,
                value.cycles,
                value.bytes,
                value.packets,
                value.max_cycles,
                value.wall_ns,
                profiling::estimate_total(value.wall_ns, value.calls, value.timed_samples),
                value.wall_ns.checked_div(value.timed_samples).unwrap_or(0),
                value.on_cpu_ns,
                profiling::estimate_total(value.on_cpu_ns, value.calls, value.timed_samples),
                value.off_cpu_ns,
                profiling::estimate_total(value.off_cpu_ns, value.calls, value.timed_samples),
                value.max_latency_ns,
                value.migrations,
                profiling::histogram_percentile(&value.latency, 50),
                profiling::histogram_percentile(&value.latency, 95),
                profiling::histogram_percentile(&value.latency, 99),
                HistogramDisplay(&value.latency),
            );
        }
        for metric in profiling::Metric::ALL {
            let value = profiling::metric_snapshot(cpu, metric);
            if value.observations == 0 {
                continue;
            }
            let _ = writeln!(
                output,
                "cpu={} metric={} observations={} sum={} max={} p50={} p95={} p99={} hist={}",
                ProfileCpuDisplay(cpu),
                metric.name(),
                value.observations,
                value.sum,
                value.max,
                profiling::histogram_percentile(&value.values, 50),
                profiling::histogram_percentile(&value.values, 95),
                profiling::histogram_percentile(&value.values, 99),
                HistogramDisplay(&value.values),
            );
        }
    }
    for phase in 0..profiling::MAX_PHASES {
        for nr in 0..profiling::SYSCALL_SLOTS {
            let Some(value) = profiling::syscall_snapshot(phase, nr) else {
                continue;
            };
            let timing = value.timing;
            let completed = value.success.saturating_add(value.errors);
            let inflight = timing.calls.saturating_sub(completed);
            let _ = writeln!(
                output,
                "phase={} syscall={} calls={} completed={} inflight={} success={} errors={} cycles={} max_cycles={} wall_ns={} on_cpu_ns={} off_cpu_ns={} max_latency_ns={} migrations={} p50_ns={} p95_ns={} p99_ns={} hist={}",
                phase,
                nr,
                timing.calls,
                completed,
                inflight,
                value.success,
                value.errors,
                timing.cycles,
                timing.max_cycles,
                timing.wall_ns,
                timing.on_cpu_ns,
                timing.off_cpu_ns,
                timing.max_latency_ns,
                timing.migrations,
                profiling::histogram_percentile(&timing.latency, 50),
                profiling::histogram_percentile(&timing.latency, 95),
                profiling::histogram_percentile(&timing.latency, 99),
                HistogramDisplay(&timing.latency),
            );
        }
    }
    for slot in 0..profiling::ERRNO_SLOTS {
        let Some(value) = profiling::errno_snapshot(slot) else {
            continue;
        };
        let _ = writeln!(
            output,
            "phase={} syscall={} errno={} count={}",
            value.phase, value.nr, value.errno, value.count,
        );
    }
    for slot in 0..profiling::TASK_SLOTS {
        let Some(value) = profiling::task_snapshot(slot) else {
            continue;
        };
        let _ = writeln!(
            output,
            "task session={} pid={} tgid={} ppid={} runtime_ns={} voluntary_switches={} involuntary_switches={} migrations={} exited={} exit_code={}",
            value.session,
            value.pid,
            value.tgid,
            value.ppid,
            value.runtime_ns,
            value.voluntary_switches,
            value.involuntary_switches,
            value.migrations,
            u8::from(value.exited),
            value.exit_code,
        );
    }
    let _ = writeln!(
        output,
        "health dropped_errno_records={} dropped_task_records={}",
        profiling::dropped_errno_records(),
        profiling::dropped_task_records(),
    );
    output
}

#[cfg(feature = "performance-profile")]
struct HistogramDisplay<'a>(&'a [u64; profiling::HISTOGRAM_BUCKETS]);

#[cfg(feature = "performance-profile")]
struct ProfileCpuDisplay(usize);

#[cfg(feature = "performance-profile")]
impl core::fmt::Display for ProfileCpuDisplay {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 == profiling::MIXED_CPU {
            formatter.write_str("mixed")
        } else {
            write!(formatter, "{}", self.0)
        }
    }
}

#[cfg(feature = "performance-profile")]
impl core::fmt::Display for HistogramDisplay<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (index, value) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str(",")?;
            }
            write!(formatter, "{value}")?;
        }
        Ok(())
    }
}

#[cfg(feature = "performance-profile")]
fn render_profile_samples() -> String {
    use alloc::fmt::Write;
    let mut output = String::new();
    let session = profiling::session_info();
    let _ = writeln!(
        output,
        "state={} enabled={} session={} generation={} sampling={} sample_hz={} aggregation=image_pc slots_per_cpu={}",
        session.state.name(),
        u8::from(profiling::enabled()),
        session.session_id,
        session.generation,
        u8::from(session.sampling_enabled),
        session.sample_hz,
        profiling::SAMPLE_SLOTS,
    );
    for cpu in 0..profiling::MAX_CPUS {
        let _ = writeln!(
            output,
            "cpu={} dropped_samples={}",
            cpu,
            profiling::dropped_samples(cpu),
        );
        for slot in 0..profiling::SAMPLE_SLOTS {
            let Some(sample) = profiling::sample_slot(cpu, slot) else {
                continue;
            };
            let _ = writeln!(
                output,
                "cpu={} task=0 mode={} pc={:#x} image={:#x} load_base={:#x} samples={}",
                cpu,
                if sample.from_user { "user" } else { "kernel" },
                sample.pc,
                sample.image_id,
                sample.load_base,
                sample.samples,
            );
        }
    }
    output
}

#[cfg(feature = "performance-profile")]
fn render_profile_trace() -> String {
    use alloc::fmt::Write;
    let mut output = String::new();
    let session = profiling::session_info();
    let _ = writeln!(
        output,
        "state={} enabled={} session={} generation={} active_writers={} trace={} counter_hz={} slots_per_cpu={} record_bytes={} format_version={}",
        session.state.name(),
        u8::from(profiling::enabled()),
        session.session_id,
        session.generation,
        session.active_writers,
        u8::from(session.trace_enabled),
        session.counter_hz,
        profiling::TRACE_SLOTS_PER_CPU,
        profiling::TRACE_RECORD_BYTES,
        profiling::TRACE_FORMAT_VERSION,
    );
    for cpu in 0..profiling::MAX_CPUS {
        let window = profiling::trace_window(cpu);
        let _ = writeln!(
            output,
            "cpu={} first_sequence={} next_sequence={} retained={} overwritten={} dropped={}",
            cpu,
            window.first_sequence,
            window.next_sequence,
            window.next_sequence.saturating_sub(window.first_sequence),
            window.overwritten,
            window.dropped,
        );
        for sequence in window.first_sequence..window.next_sequence {
            let Some(record) = profiling::trace_record(cpu, sequence) else {
                continue;
            };
            let _ = writeln!(
                output,
                "cpu={} sequence={} session={} generation={} timestamp_cycles={} duration_cycles={} kind={} event={} event_id={} category={} task={} span={} arg0={} arg1={}",
                record.cpu,
                record.sequence,
                record.session_id,
                record.generation,
                record.timestamp_cycles,
                record.duration_cycles,
                record.kind.name(),
                record.event.name(),
                record.event as usize,
                record.event.category().name(),
                record.task_id,
                record.span_id,
                record.arg0,
                record.arg1,
            );
        }
    }
    output
}

#[cfg(feature = "performance-profile")]
fn render_profile_control() -> String {
    let session = profiling::session_info();
    format!(
        "state={} enabled={} session={} generation={} phase={} workload_root={} active_writers={} event_mask={:#x} event_mask_high={:#x} sampling={} sample_hz={} trace={} timing_shift={} effective_timing_shift={} timing_sampler={} commands=start,resume,freeze,stop,reset,preset=<name>,events=<mask>,events_high=<mask>,root=<pid>,phase=<0..31>,samples=0|1,sample_hz=<50..1000>,trace=0|1,timing_shift=0..16\n",
        session.state.name(),
        u8::from(profiling::enabled()),
        session.session_id,
        session.generation,
        session.phase,
        profiling::workload_root(),
        session.active_writers,
        session.event_mask,
        session.event_mask_high,
        u8::from(session.sampling_enabled),
        session.sample_hz,
        u8::from(session.trace_enabled),
        session.timing_shift,
        profiling::effective_timing_shift(),
        session.timing_sampler,
    )
}

#[cfg(feature = "performance-profile")]
fn render_profile_catalog() -> String {
    use alloc::fmt::Write;
    let mut output = String::new();
    for preset in [
        profiling::Preset::Io,
        profiling::Preset::Syscall,
        profiling::Preset::Filesystem,
        profiling::Preset::Memory,
        profiling::Preset::Scheduler,
        profiling::Preset::Block,
        profiling::Preset::Network,
        profiling::Preset::Build,
        profiling::Preset::All,
    ] {
        let _ = writeln!(
            output,
            "kind=preset name={} mask={:#x} mask_high={:#x}",
            preset.name(),
            profiling::preset_event_mask(preset),
            profiling::preset_event_mask_high(preset),
        );
    }
    for event in profiling::Event::ALL {
        let id = event as usize;
        let (word, mask) = if id < u64::BITS as usize {
            (0, 1u64 << id)
        } else {
            (1, 1u64 << (id - u64::BITS as usize))
        };
        let _ = writeln!(
            output,
            "kind=event id={} name={} category={} mask_word={} mask={:#x}",
            id,
            event.name(),
            event.category().name(),
            word,
            mask,
        );
    }
    for metric in profiling::Metric::ALL {
        let _ = writeln!(
            output,
            "kind=metric id={} name={}",
            metric as usize,
            metric.name(),
        );
    }
    for kind in profiling::TraceKind::ALL {
        let _ = writeln!(
            output,
            "kind=trace id={} name={}",
            kind as usize,
            kind.name(),
        );
    }
    output
}

#[cfg(feature = "performance-profile")]
fn render_profile_health() -> String {
    use alloc::fmt::Write;
    let session = profiling::session_info();
    let mut output = String::new();
    let dropped_samples = (0..profiling::MAX_CPUS)
        .map(profiling::dropped_samples)
        .sum::<u64>();
    let dropped_trace = (0..profiling::MAX_CPUS)
        .map(|cpu| profiling::trace_window(cpu).overwritten)
        .sum::<u64>();
    let valid = session.state == profiling::SessionState::Frozen && session.active_writers == 0;
    let samples_complete = dropped_samples == 0;
    let trace_complete = dropped_trace == 0;
    let errno_complete = profiling::dropped_errno_records() == 0;
    let tasks_complete = profiling::dropped_task_records() == 0;
    let complete = samples_complete && trace_complete && errno_complete && tasks_complete;
    let _ = writeln!(
        output,
        "valid={} complete={} samples_complete={} trace_complete={} errno_complete={} tasks_complete={} state={} active_writers={} dropped_samples={} dropped_trace={} dropped_errno_records={} dropped_task_records={} schema_version={} snapshot_bytes={}",
        u8::from(valid),
        u8::from(complete),
        u8::from(samples_complete),
        u8::from(trace_complete),
        u8::from(errno_complete),
        u8::from(tasks_complete),
        session.state.name(),
        session.active_writers,
        dropped_samples,
        dropped_trace,
        profiling::dropped_errno_records(),
        profiling::dropped_task_records(),
        profiling::BINARY_SCHEMA_VERSION,
        profiling::binary_snapshot_len(),
    );
    output
}

fn render_elm_sysfs_file(name: &str) -> String {
    match *ELM_SYSFS_RENDERER.lock() {
        Some(renderer) => renderer(name),
        None => "status=unavailable\n".into(),
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
        SysRegFile::NumaRoot { slot } => NumaSysfsView::snapshot().render_root_file(slot),
        SysRegFile::NumaNode { node_id, slot } => {
            NumaSysfsView::snapshot().render_node_file(node_id, slot)
        }
        SysRegFile::CpuOnline => format_cpu_mask_range(online_cpu_mask()),
        SysRegFile::CpuPossible => format_cpu_mask_range(supported_cpu_mask()),
        // 本内核尚未区分“已发现但离线”的 CPU;present 与 possible 相同,
        // 都取固件/调度暴露的支持 CPU 集合,不再错误地等同于 online。
        SysRegFile::CpuPresent => format_cpu_mask_range(supported_cpu_mask()),
        SysRegFile::UeventSeqnum => format!("{}\n", UEVENT_SEQNUM.load(Ordering::Relaxed)),
        SysRegFile::UeventHelper | SysRegFile::Hotplug => {
            let value = UEVENT_HELPER_PATH.lock();
            if value.is_empty() {
                String::new()
            } else {
                format!("{}\n", &*value)
            }
        }
        // /sys/power/state:本内核没有 suspend 状态机,只支持写 "0" 触发关机的
        // 最小语义;读取返回空(无可用睡眠状态),避免向用户空间宣称未实现的
        // freeze/mem/disk 能力。
        SysRegFile::PowerState => String::new(),
        SysRegFile::PowerWakeupCount => "0\n".into(),
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
        SysRegFile::NetStats => render_net_stats(),
        #[cfg(feature = "performance-profile")]
        SysRegFile::ProfileStats => render_profile_stats(),
        #[cfg(feature = "performance-profile")]
        SysRegFile::ProfileControl => render_profile_control(),
        #[cfg(feature = "performance-profile")]
        SysRegFile::ProfileSamples => render_profile_samples(),
        #[cfg(feature = "performance-profile")]
        SysRegFile::ProfileCatalog => render_profile_catalog(),
        #[cfg(feature = "performance-profile")]
        SysRegFile::ProfileTrace => render_profile_trace(),
        #[cfg(feature = "performance-profile")]
        SysRegFile::ProfileSnapshot => String::new(),
        #[cfg(feature = "performance-profile")]
        SysRegFile::ProfileHealth => render_profile_health(),
        SysRegFile::Elm { slot } => render_elm_sysfs_file(slot.file_name()),
        // THP/khugepaged 视图：本内核无 THP，enabled 恒为 never，其余为默认值。
        SysRegFile::ThpEnabled => "always madvise [never]\n".into(),
        SysRegFile::ThpDefrag => "always defer defer+madvise madvise [never]\n".into(),
        SysRegFile::ThpShmemEnabled => "always within_size advise [never]\n".into(),
        SysRegFile::ThpUseZeroPage => "0\n".into(),
        SysRegFile::KhpScanSleepMs => "10000\n".into(),
        SysRegFile::KhpAllocSleepMs => "60000\n".into(),
        SysRegFile::KhpMaxPtesNone => "511\n".into(),
        SysRegFile::KhpPagesCollapsed => "0\n".into(),
        // KSM 视图：本内核无 KSM，run=0（禁用），其余为 Linux 默认值。
        SysRegFile::KsmRun => "0\n".into(),
        SysRegFile::KsmMergeAcrossNodes => "1\n".into(),
        SysRegFile::KsmPagesShared => "0\n".into(),
        SysRegFile::KsmPagesSharing => "0\n".into(),
        SysRegFile::KsmPagesUnshared => "0\n".into(),
        SysRegFile::KsmPagesVolatile => "0\n".into(),
        SysRegFile::KsmFullScans => "0\n".into(),
        SysRegFile::KsmMaxPageSharing => "256\n".into(),
        // hugetlb 视图：本内核无大页池，全部为 0。
        SysRegFile::HugepagesNr => "0\n".into(),
        SysRegFile::HugepagesNrOvercommit => "0\n".into(),
        SysRegFile::HugepagesFree => "0\n".into(),
        SysRegFile::HugepagesResv => "0\n".into(),
        SysRegFile::HugepagesSurplus => "0\n".into(),
        SysRegFile::NetDev { iface_id, slot } => {
            if let Some(iface) = net::device::snapshot_devices()
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
            register_sysfs_root_dentry(&root_dentry);
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
struct SysBinaryFile {
    data: Arc<[u8]>,
}
struct SysRegFileOps {
    kind: SysRegFile,
    snap: Arc<SysSnapshot>,
    snapshot: Option<Box<[u8]>>,
}

fn read_bytes_at(buf: &mut [u8], offset: u64, bytes: &[u8]) -> VfsResult<usize> {
    let off = offset as usize;
    if off >= bytes.len() {
        return Ok(0);
    }
    let n = core::cmp::min(buf.len(), bytes.len() - off);
    buf[..n].copy_from_slice(&bytes[off..off + n]);
    Ok(n)
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

impl FileOps for SysBinaryFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        read_bytes_at(buf, offset, self.data.as_ref())
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
        #[cfg(feature = "performance-profile")]
        if matches!(self.kind, SysRegFile::ProfileSnapshot) {
            if profiling::state() != profiling::SessionState::Frozen {
                return Err(VfsError::InvalidArgument);
            }
            return Ok(profiling::read_binary_snapshot(buf, offset));
        }
        if let Some(snapshot) = self.snapshot.as_deref() {
            return read_bytes_at(buf, offset, snapshot);
        }
        let s = render_reg_file(&self.snap, self.kind);
        read_bytes_at(buf, offset, s.as_bytes())
    }
    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        // uevent 文件写:接受 Linux 的动作词并递增 uevent 序号。本内核尚未接入
        // netlink uevent 通道,也没有 dev core 重扫描 hook,因此这里以序号递增
        // 作为最小可观测的事件投递信号;真正的 mdev/udev 通知需 dev core 支持。
        if matches!(
            self.kind,
            SysRegFile::DevCharInner {
                slot: DevCharInnerSlot::Uevent,
                ..
            }
        ) {
            let action = core::str::from_utf8(buf)
                .map_err(|_| VfsError::InvalidArgument)?
                .trim();
            return match action {
                "add" | "remove" | "change" | "bind" | "unbind" | "online" | "offline" => {
                    UEVENT_SEQNUM.fetch_add(1, Ordering::Relaxed);
                    Ok(buf.len())
                }
                _ => Err(VfsError::InvalidArgument),
            };
        }
        if matches!(self.kind, SysRegFile::UeventHelper | SysRegFile::Hotplug) {
            if _offset != 0 {
                return Err(VfsError::InvalidArgument);
            }
            let text = core::str::from_utf8(buf).map_err(|_| VfsError::InvalidArgument)?;
            let trimmed = text.trim_end_matches(|ch| ch == '\n' || ch == '\0');
            let mut stored = UEVENT_HELPER_PATH.lock();
            stored.clear();
            stored
                .try_reserve(trimmed.len())
                .map_err(|_| VfsError::OutOfMemory)?;
            stored.push_str(trimmed);
            return Ok(buf.len());
        }
        if matches!(self.kind, SysRegFile::PowerState) {
            let action = core::str::from_utf8(buf)
                .map_err(|_| VfsError::InvalidArgument)?
                .trim();
            // 最小关机语义:写 "0" 触发固件关机;其余状态本内核不支持,返回 EINVAL。
            if action != "0" {
                return Err(VfsError::InvalidArgument);
            }
            return match crate::firmware::power::shutdown() {
                Ok(()) => Ok(buf.len()),
                Err(crate::firmware::power::PowerError::NotInstalled) => Err(VfsError::NoDevice),
                Err(_) => Err(VfsError::Io),
            };
        }
        if matches!(self.kind, SysRegFile::PowerWakeupCount) {
            let value = core::str::from_utf8(buf)
                .map_err(|_| VfsError::InvalidArgument)?
                .trim();
            // 本内核不跟踪 wakeup 事件,当前计数恒为 0;写 0 成功,写其它返回 EINVAL
            // (Linux 语义:写入值必须与当前 wakeup 计数一致,否则拒绝)。
            return match value {
                "0" => Ok(buf.len()),
                _ => Err(VfsError::InvalidArgument),
            };
        }
        #[cfg(feature = "performance-profile")]
        if matches!(self.kind, SysRegFile::ProfileControl) {
            if _offset != 0 {
                // BusyBox ash 的 `printf '%s\n'` 可能把命令和结尾换行拆成
                // 两次 write。第一段已经完成控制操作；后续纯空白片段应像
                // Linux sysfs 的单次文本事务一样被接纳，不能让 shell 误判
                // 整个控制命令失败。
                return if _buf.iter().all(u8::is_ascii_whitespace) {
                    Ok(_buf.len())
                } else {
                    Err(VfsError::InvalidArgument)
                };
            }
            let command = core::str::from_utf8(_buf)
                .map_err(|_| VfsError::InvalidArgument)?
                .trim();
            if let Some(mask) = command.strip_prefix("events=") {
                let mask = mask.strip_prefix("0x").unwrap_or(mask);
                let mask = u64::from_str_radix(mask, 16).map_err(|_| VfsError::InvalidArgument)?;
                profiling::set_event_mask(mask);
                return Ok(_buf.len());
            }
            if let Some(mask) = command.strip_prefix("events_high=") {
                let mask = mask.strip_prefix("0x").unwrap_or(mask);
                let mask = u64::from_str_radix(mask, 16).map_err(|_| VfsError::InvalidArgument)?;
                profiling::set_event_masks(profiling::event_mask(), mask);
                return Ok(_buf.len());
            }
            if let Some(name) = command.strip_prefix("preset=") {
                let preset = profiling::Preset::from_name(name).ok_or(VfsError::InvalidArgument)?;
                profiling::set_event_preset(preset);
                return Ok(_buf.len());
            }
            if let Some(value) = command.strip_prefix("phase=") {
                let phase = value
                    .parse::<usize>()
                    .map_err(|_| VfsError::InvalidArgument)?;
                if !profiling::set_phase(phase) {
                    return Err(VfsError::InvalidArgument);
                }
                return Ok(_buf.len());
            }
            if let Some(value) = command.strip_prefix("root=") {
                let pid = value
                    .parse::<i32>()
                    .map_err(|_| VfsError::InvalidArgument)?;
                let task = sched::root_pid_ns()
                    .registry()
                    .lookup(pid)
                    .and_then(|task| task.upgrade())
                    .ok_or(VfsError::NotFound)?;
                task.set_profile_session_id(profiling::session_id());
                profiling::register_task(
                    profiling::session_id(),
                    pid as u64,
                    task.parent()
                        .and_then(|parent| parent.pid_root_cached())
                        .unwrap_or(0) as u64,
                    task.tgid_cached().unwrap_or(pid) as u64,
                );
                profiling::record_task_images(
                    profiling::session_id(),
                    pid as u64,
                    task.profile_main_image(),
                    task.profile_interpreter_image(),
                );
                profiling::set_workload_root(pid as u64);
                return Ok(_buf.len());
            }
            if let Some(enabled) = command.strip_prefix("samples=") {
                match enabled {
                    "0" | "off" => profiling::set_sampling_enabled(false),
                    "1" | "on" => profiling::set_sampling_enabled(true),
                    _ => return Err(VfsError::InvalidArgument),
                }
                sched::reprogram_current_deadline(None);
                return Ok(_buf.len());
            }
            if let Some(value) = command.strip_prefix("sample_hz=") {
                let hz = value
                    .parse::<u64>()
                    .map_err(|_| VfsError::InvalidArgument)?;
                if !profiling::set_sample_hz(hz) {
                    return Err(VfsError::InvalidArgument);
                }
                sched::reprogram_current_deadline(None);
                return Ok(_buf.len());
            }
            if let Some(enabled) = command.strip_prefix("trace=") {
                match enabled {
                    "0" | "off" => profiling::set_trace_enabled(false),
                    "1" | "on" => profiling::set_trace_enabled(true),
                    _ => return Err(VfsError::InvalidArgument),
                }
                return Ok(_buf.len());
            }
            if let Some(shift) = command.strip_prefix("timing_shift=") {
                let shift = shift
                    .parse::<usize>()
                    .map_err(|_| VfsError::InvalidArgument)?;
                if shift > profiling::MAX_TIMING_SHIFT {
                    return Err(VfsError::InvalidArgument);
                }
                profiling::set_timing_shift(shift);
                return Ok(_buf.len());
            }
            match command {
                "start" => profiling::start(),
                "1" | "enable" | "resume" => profiling::resume(),
                "0" | "freeze" => profiling::freeze(),
                "disable" | "stop" => profiling::stop(),
                "reset" => profiling::reset(),
                _ => return Err(VfsError::InvalidArgument),
            }
            sched::reprogram_current_deadline(None);
            return Ok(_buf.len());
        }
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
    let mode = if sys_reg_file_writable(kind) {
        0o644
    } else {
        0o444
    };
    Some(mk_inode(
        fs_id,
        weak_sb,
        ino,
        FileType::Regular,
        mode,
        1,
        ops,
    ))
}

/// 判断 sysfs 属性文件是否接受写入(用于报告 0644 权限)。
fn sys_reg_file_writable(kind: SysRegFile) -> bool {
    match kind {
        SysRegFile::UeventHelper
        | SysRegFile::Hotplug
        | SysRegFile::PowerState
        | SysRegFile::PowerWakeupCount => true,
        SysRegFile::DevCharInner {
            slot: DevCharInnerSlot::Uevent,
            ..
        } => true,
        #[cfg(feature = "performance-profile")]
        SysRegFile::ProfileControl => true,
        _ => false,
    }
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
    // Linux 的 sysfs 根目录为 0555，kobject 创建的子目录统一报告为 0755；
    // 写位不代表允许任意 VFS 修改，实际操作仍由只读 InodeOps 约束。
    mk_inode(fs_id, weak_sb, ino, FileType::Directory, 0o755, 2, ops)
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

fn build_binary_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    ino: u64,
    mode: u16,
    reported_size: u64,
    data: Arc<[u8]>,
) -> Arc<Inode> {
    let ops: Arc<dyn InodeOps + Send + Sync> = Arc::new(SysBinaryInodeOps { data });
    let mut meta = inode_meta(mode, 1, timespec_now());
    meta.size = reported_size;
    Inode::new(
        InodeId { fs_id, ino },
        FileType::Regular,
        DevId::new(0, 0),
        4096,
        None,
        meta,
        ops,
        weak_sb.clone(),
    )
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
    BlockHolders,
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
        bus: String,
    },
    PnpDevice {
        bus: String,
        name: String,
    },
    Dev,
    DevBlock,
    DevChar,
    DevCharInner {
        rdev: DevId,
    },
    Kernel,
    KernelElm,
    KernelMm,
    KernelMmThp,
    KernelMmThpKhugepaged,
    KernelMmKsm,
    KernelMmHugepages,
    KernelMmHugepagesLeaf,
    Fs,
    FsCgroup,
    Bus,
    BusClass {
        bus: String,
    },
    BusClassDevices {
        bus: String,
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
    FirmwareDeviceTree,
    DeviceTreeNode {
        node_id: DeviceTreeNodeId,
    },
    DevicesSystem,
    DevicesSystemCpu,
    DevicesSystemNode,
    DevicesSystemClockevents,
    NumaNode {
        node_id: u32,
    },
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
struct SysBinaryInodeOps {
    data: Arc<[u8]>,
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

fn truncate_sys_reg(kind: SysRegFile, size: u64) -> VfsResult<()> {
    #[cfg(feature = "performance-profile")]
    if matches!(kind, SysRegFile::ProfileControl) {
        return if size == 0 {
            Ok(())
        } else {
            Err(VfsError::InvalidArgument)
        };
    }
    let _ = kind;
    // 与 Linux kernfs 一致:sysfs 普通文件忽略截断到 0 的请求
    // (shell 的 `echo x > file` 带 O_TRUNC),非零截断拒绝。
    if size == 0 {
        Ok(())
    } else {
        Err(VfsError::ReadOnlyFilesystem)
    }
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
        // trace 是有界环，打开时固定窗口，避免 read_at 重渲染或读取期间窗口漂移。
        #[cfg(feature = "performance-profile")]
        let snapshot = if matches!(self.kind, SysRegFile::ProfileTrace) {
            Some(render_profile_trace().into_bytes().into_boxed_slice())
        } else {
            None
        };
        #[cfg(not(feature = "performance-profile"))]
        let snapshot = None;
        Ok(Box::new(SysRegFileOps {
            kind: self.kind,
            snap: Arc::clone(&self.snap),
            snapshot,
        }))
    }
    fn truncate(&self, _: &Inode, size: u64) -> VfsResult<()> {
        truncate_sys_reg(self.kind, size)
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl InodeOps for SysBinaryInodeOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }
    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(SysBinaryFile {
            data: Arc::clone(&self.data),
        }))
    }
    fn truncate(&self, _: &Inode, _: u64) -> VfsResult<()> {
        Err(VfsError::ReadOnlyFilesystem)
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
        let mk_binary = |ino: u64, mode: u16, reported_size: u64, data: Arc<[u8]>| -> Arc<Inode> {
            build_binary_inode(fs_id, weak_sb, ino, mode, reported_size, data)
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
                match slot {
                    BlockDevSlot::QueueDir => {
                        Ok(mk_dir(ino, SysDirKind::BlockQueue { name: block_name }))
                    }
                    BlockDevSlot::HoldersDir => Ok(mk_dir(ino, SysDirKind::BlockHolders)),
                    BlockDevSlot::DeviceLink => {
                        let parent = snap.blocks[idx]
                            .parent_name
                            .as_ref()
                            .ok_or(VfsError::NotFound)?;
                        Ok(mk_link(ino, format!("../{}", parent)))
                    }
                    BlockDevSlot::SubsystemLink => {
                        Ok(mk_link(ino, "../../class/block".to_string()))
                    }
                    _ => mk_reg(ino, SysRegFile::BlockDev { idx, slot }),
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
            // holders 当前没有 dm/md holder 模型,保持稳定空目录。
            SysDirKind::BlockHolders => Err(VfsError::NotFound),
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
                    pnp_bus_ino(&snap.pnp_buses[bus_idx]),
                    SysDirKind::DevicesPnpBus {
                        bus: snap.pnp_buses[bus_idx].clone(),
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
                    pnp_device_ino(&bus, name),
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
                    pnp_device_slot_ino(&bus, &dev_name, slot.to_u64()),
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
                "net_stats" => mk_reg(KERNEL_NET_STATS_INO, SysRegFile::NetStats),
                // Linux uevent ABI:uevent_seqnum 只读,uevent_helper 可写。
                // hotplug 是 /proc/sys/kernel/hotplug 的 /sys 别名,便于旧工具迁移。
                "uevent_seqnum" => mk_reg(KERNEL_UEVENT_SEQNUM_INO, SysRegFile::UeventSeqnum),
                "uevent_helper" => mk_reg(KERNEL_UEVENT_HELPER_INO, SysRegFile::UeventHelper),
                "hotplug" => mk_reg(KERNEL_HOTPLUG_INO, SysRegFile::Hotplug),
                #[cfg(feature = "performance-profile")]
                "profile_stats" => mk_reg(KERNEL_PROFILE_STATS_INO, SysRegFile::ProfileStats),
                #[cfg(feature = "performance-profile")]
                "profile_control" => mk_reg(KERNEL_PROFILE_CONTROL_INO, SysRegFile::ProfileControl),
                #[cfg(feature = "performance-profile")]
                "profile_samples" => mk_reg(KERNEL_PROFILE_SAMPLES_INO, SysRegFile::ProfileSamples),
                #[cfg(feature = "performance-profile")]
                "profile_catalog" => mk_reg(KERNEL_PROFILE_CATALOG_INO, SysRegFile::ProfileCatalog),
                #[cfg(feature = "performance-profile")]
                "profile_trace" => mk_reg(KERNEL_PROFILE_TRACE_INO, SysRegFile::ProfileTrace),
                #[cfg(feature = "performance-profile")]
                "profile_snapshot" => {
                    mk_reg(KERNEL_PROFILE_SNAPSHOT_INO, SysRegFile::ProfileSnapshot)
                }
                #[cfg(feature = "performance-profile")]
                "profile_health" => mk_reg(KERNEL_PROFILE_HEALTH_INO, SysRegFile::ProfileHealth),
                "elm" => Ok(mk_dir(KERNEL_ELM_DIR_INO, SysDirKind::KernelElm)),
                "mm" => Ok(mk_dir(KERNEL_MM_DIR_INO, SysDirKind::KernelMm)),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::KernelMm => match name {
                "transparent_hugepage" => {
                    Ok(mk_dir(KERNEL_MM_THP_DIR_INO, SysDirKind::KernelMmThp))
                }
                "ksm" => Ok(mk_dir(KERNEL_MM_KSM_DIR_INO, SysDirKind::KernelMmKsm)),
                "hugepages" => Ok(mk_dir(
                    KERNEL_MM_HUGEPAGES_DIR_INO,
                    SysDirKind::KernelMmHugepages,
                )),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::KernelMmThp => match name {
                "enabled" => mk_reg(KERNEL_MM_THP_ENABLED_INO, SysRegFile::ThpEnabled),
                "defrag" => mk_reg(KERNEL_MM_THP_DEFRAG_INO, SysRegFile::ThpDefrag),
                "shmem_enabled" => {
                    mk_reg(KERNEL_MM_THP_SHMEM_ENABLED_INO, SysRegFile::ThpShmemEnabled)
                }
                "use_zero_page" => {
                    mk_reg(KERNEL_MM_THP_USE_ZERO_PAGE_INO, SysRegFile::ThpUseZeroPage)
                }
                "khugepaged" => Ok(mk_dir(
                    KERNEL_MM_THP_KHUGEPAGED_DIR_INO,
                    SysDirKind::KernelMmThpKhugepaged,
                )),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::KernelMmThpKhugepaged => match name {
                "scan_sleep_millisecs" => {
                    mk_reg(KERNEL_MM_KHP_SCAN_SLEEP_INO, SysRegFile::KhpScanSleepMs)
                }
                "alloc_sleep_millisecs" => {
                    mk_reg(KERNEL_MM_KHP_ALLOC_SLEEP_INO, SysRegFile::KhpAllocSleepMs)
                }
                "max_ptes_none" => {
                    mk_reg(KERNEL_MM_KHP_MAX_PTES_NONE_INO, SysRegFile::KhpMaxPtesNone)
                }
                "pages_collapsed" => mk_reg(
                    KERNEL_MM_KHP_PAGES_COLLAPSED_INO,
                    SysRegFile::KhpPagesCollapsed,
                ),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::KernelMmKsm => match name {
                "run" => mk_reg(KERNEL_MM_KSM_RUN_INO, SysRegFile::KsmRun),
                "merge_across_nodes" => mk_reg(
                    KERNEL_MM_KSM_MERGE_ACROSS_NODES_INO,
                    SysRegFile::KsmMergeAcrossNodes,
                ),
                "pages_shared" => {
                    mk_reg(KERNEL_MM_KSM_PAGES_SHARED_INO, SysRegFile::KsmPagesShared)
                }
                "pages_sharing" => {
                    mk_reg(KERNEL_MM_KSM_PAGES_SHARING_INO, SysRegFile::KsmPagesSharing)
                }
                "pages_unshared" => mk_reg(
                    KERNEL_MM_KSM_PAGES_UNSHARED_INO,
                    SysRegFile::KsmPagesUnshared,
                ),
                "pages_volatile" => mk_reg(
                    KERNEL_MM_KSM_PAGES_VOLATILE_INO,
                    SysRegFile::KsmPagesVolatile,
                ),
                "full_scans" => mk_reg(KERNEL_MM_KSM_FULL_SCANS_INO, SysRegFile::KsmFullScans),
                "max_page_sharing" => mk_reg(
                    KERNEL_MM_KSM_MAX_PAGE_SHARING_INO,
                    SysRegFile::KsmMaxPageSharing,
                ),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::KernelMmHugepages => match name {
                "hugepages-2048kB" => Ok(mk_dir(
                    KERNEL_MM_HUGEPAGES_SUBDIR_INO,
                    SysDirKind::KernelMmHugepagesLeaf,
                )),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::KernelMmHugepagesLeaf => match name {
                "nr_hugepages" => mk_reg(KERNEL_MM_HP_NR_INO, SysRegFile::HugepagesNr),
                "nr_overcommit_hugepages" => mk_reg(
                    KERNEL_MM_HP_NR_OVERCOMMIT_INO,
                    SysRegFile::HugepagesNrOvercommit,
                ),
                "free_hugepages" => mk_reg(KERNEL_MM_HP_FREE_INO, SysRegFile::HugepagesFree),
                "resv_hugepages" => mk_reg(KERNEL_MM_HP_RESV_INO, SysRegFile::HugepagesResv),
                "surplus_hugepages" => {
                    mk_reg(KERNEL_MM_HP_SURPLUS_INO, SysRegFile::HugepagesSurplus)
                }
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::KernelElm => {
                let slot = elm_sysfs_slot_by_name(name).ok_or(VfsError::NotFound)?;
                mk_reg(kernel_elm_slot_ino(slot), SysRegFile::Elm { slot })
            }
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
                // 常驻总线(cpu/memory/clockevents/clocksource/virtio)与 PnP 派生的
                // 总线(pci/usb/platform)统一提供 `devices` 子目录。
                if !is_static_sysfs_bus(name) && !snap.pnp_buses.iter().any(|bus| *bus == name) {
                    return Err(VfsError::NotFound);
                }
                Ok(mk_dir(
                    bus_class_ino(name),
                    SysDirKind::BusClass {
                        bus: name.to_string(),
                    },
                ))
            }
            SysDirKind::BusClass { bus } => match name {
                "devices" => Ok(mk_dir(
                    bus_class_devices_ino(&bus),
                    SysDirKind::BusClassDevices { bus },
                )),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::BusClassDevices { bus } => {
                // 常驻总线没有 PnP 设备模型,devices 保持稳定空目录。
                if is_static_sysfs_bus(&bus) {
                    return Err(VfsError::NotFound);
                }
                if !snap.pnp_buses.iter().any(|entry| *entry == bus) {
                    return Err(VfsError::NotFound);
                }
                let idx = snap
                    .pnp_devices
                    .iter()
                    .position(|dev| dev.bus_type == bus && dev.sysfs_name == name)
                    .ok_or(VfsError::NotFound)?;
                Ok(mk_link(
                    bus_class_device_link_ino(&bus, name),
                    format!(
                        "../../../devices/pnp/{}/{}",
                        bus, snap.pnp_devices[idx].sysfs_name
                    ),
                ))
            }
            SysDirKind::Module => Err(VfsError::NotFound),
            SysDirKind::Power => match name {
                "state" => mk_reg(POWER_STATE_INO, SysRegFile::PowerState),
                "wakeup_count" => mk_reg(POWER_WAKEUP_COUNT_INO, SysRegFile::PowerWakeupCount),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::Firmware => {
                let firmware = installed_device_tree().ok_or(VfsError::NotFound)?;
                match name {
                    "fdt" => {
                        let size = firmware.boot_blob.len() as u64;
                        Ok(mk_binary(
                            firmware_fdt_ino(),
                            0o400,
                            size,
                            Arc::clone(&firmware.boot_blob),
                        ))
                    }
                    "devicetree" => Ok(mk_dir(
                        firmware_device_tree_ino(),
                        SysDirKind::FirmwareDeviceTree,
                    )),
                    _ => Err(VfsError::NotFound),
                }
            }
            SysDirKind::FirmwareDeviceTree => {
                if name != "base" {
                    return Err(VfsError::NotFound);
                }
                installed_device_tree().ok_or(VfsError::NotFound)?;
                let node_id = DeviceTreeNodeId::root();
                Ok(mk_dir(
                    device_tree_node_ino(&node_id),
                    SysDirKind::DeviceTreeNode { node_id },
                ))
            }
            SysDirKind::DeviceTreeNode { node_id } => {
                let firmware = installed_device_tree().ok_or(VfsError::NotFound)?;
                let live_blob = firmware.live_blob();
                let node = node_id.node(live_blob.as_ref()).ok_or(VfsError::NotFound)?;

                if let Some(property) = device_tree_property_projections(node)
                    .into_iter()
                    .find(|property| property.sysfs_name == name)
                {
                    let mode = device_tree_property_mode(property.original_name());
                    let value = property.data(node);
                    let reported_size = if mode == 0o400 { 0 } else { value.len() as u64 };
                    return Ok(mk_binary(
                        device_tree_property_ino(&node_id, name),
                        mode,
                        reported_size,
                        value,
                    ));
                }

                if let Some(child) = device_tree_child_projections(node)
                    .into_iter()
                    .find(|child| child.sysfs_name == name)
                {
                    let child_id = node_id.child(child.node.name(), child.sibling_occurrence);
                    return Ok(mk_dir(
                        device_tree_node_ino(&child_id),
                        SysDirKind::DeviceTreeNode { node_id: child_id },
                    ));
                }
                Err(VfsError::NotFound)
            }
            SysDirKind::DevicesSystem => match name {
                "cpu" => Ok(mk_dir(DEVICES_SYSTEM_CPU_INO, SysDirKind::DevicesSystemCpu)),
                "node" => Ok(mk_dir(numa_root_ino(), SysDirKind::DevicesSystemNode)),
                "clockevents" => Ok(mk_dir(
                    DEVICES_SYSTEM_CLOCKEVENTS_INO,
                    SysDirKind::DevicesSystemClockevents,
                )),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::DevicesSystemClockevents => Err(VfsError::NotFound),
            SysDirKind::DevicesSystemNode => {
                if let Some(slot) = numa_root_slot_by_name(name) {
                    return mk_reg(
                        numa_root_slot_ino(slot.to_u64()),
                        SysRegFile::NumaRoot { slot },
                    );
                }
                let node_id = name
                    .strip_prefix("node")
                    .and_then(|value| value.parse::<u32>().ok())
                    .ok_or(VfsError::NotFound)?;
                if !NumaSysfsView::snapshot().contains_online_node(node_id) {
                    return Err(VfsError::NotFound);
                }
                Ok(mk_dir(
                    numa_node_ino(node_id),
                    SysDirKind::NumaNode { node_id },
                ))
            }
            SysDirKind::NumaNode { node_id } => {
                if !NumaSysfsView::snapshot().contains_online_node(node_id) {
                    return Err(VfsError::NotFound);
                }
                let slot = numa_node_slot_by_name(name).ok_or(VfsError::NotFound)?;
                mk_reg(
                    numa_node_slot_ino(node_id, slot.to_u64()),
                    SysRegFile::NumaNode { node_id, slot },
                )
            }
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
                let Some(idx) = snap
                    .blocks
                    .iter()
                    .position(|block| block.sysfs_name == name)
                else {
                    return Vec::new();
                };
                let mut entries = vec![
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
                        block_dev_slot_ino(&name, BlockDevSlot::HoldersDir.to_u64()),
                        "holders",
                        FileType::Directory,
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
                    mk_dir_entry(
                        block_dev_slot_ino(&name, BlockDevSlot::Diskseq.to_u64()),
                        "diskseq",
                        FileType::Regular,
                    ),
                ];
                // 整盘没有块级父设备时按 Linux 省略 device 链接;分区链到父整盘。
                if snap.blocks[idx].parent_name.is_some() {
                    entries.push(mk_dir_entry(
                        block_dev_slot_ino(&name, BlockDevSlot::DeviceLink.to_u64()),
                        "device",
                        FileType::Symlink,
                    ));
                }
                entries.push(mk_dir_entry(
                    block_dev_slot_ino(&name, BlockDevSlot::SubsystemLink.to_u64()),
                    "subsystem",
                    FileType::Symlink,
                ));
                entries
            }
            SysDirKind::BlockHolders => Vec::new(),
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
                    mk_dir_entry(
                        block_queue_slot_ino(&name, BlockQueueSlot::DiscardMaxBytes.to_u64()),
                        "discard_max_bytes",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_queue_slot_ino(&name, BlockQueueSlot::DiscardGranularity.to_u64()),
                        "discard_granularity",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_queue_slot_ino(&name, BlockQueueSlot::WriteZeroesMaxBytes.to_u64()),
                        "write_zeroes_max_bytes",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_queue_slot_ino(&name, BlockQueueSlot::MaxSectorsKb.to_u64()),
                        "max_sectors_kb",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_queue_slot_ino(&name, BlockQueueSlot::MaxSegments.to_u64()),
                        "max_segments",
                        FileType::Regular,
                    ),
                    mk_dir_entry(
                        block_queue_slot_ino(&name, BlockQueueSlot::MaxSegmentSize.to_u64()),
                        "max_segment_size",
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
                        pnp_device_ino(&dev.bus_type, &dev.sysfs_name),
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
                        pnp_device_slot_ino(&bus, &name, slot.to_u64()),
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
                mk_dir_entry(KERNEL_NET_STATS_INO, "net_stats", FileType::Regular),
                mk_dir_entry(KERNEL_UEVENT_SEQNUM_INO, "uevent_seqnum", FileType::Regular),
                mk_dir_entry(KERNEL_UEVENT_HELPER_INO, "uevent_helper", FileType::Regular),
                mk_dir_entry(KERNEL_HOTPLUG_INO, "hotplug", FileType::Regular),
                #[cfg(feature = "performance-profile")]
                mk_dir_entry(KERNEL_PROFILE_STATS_INO, "profile_stats", FileType::Regular),
                #[cfg(feature = "performance-profile")]
                mk_dir_entry(
                    KERNEL_PROFILE_CONTROL_INO,
                    "profile_control",
                    FileType::Regular,
                ),
                #[cfg(feature = "performance-profile")]
                mk_dir_entry(
                    KERNEL_PROFILE_SAMPLES_INO,
                    "profile_samples",
                    FileType::Regular,
                ),
                #[cfg(feature = "performance-profile")]
                mk_dir_entry(
                    KERNEL_PROFILE_CATALOG_INO,
                    "profile_catalog",
                    FileType::Regular,
                ),
                #[cfg(feature = "performance-profile")]
                mk_dir_entry(KERNEL_PROFILE_TRACE_INO, "profile_trace", FileType::Regular),
                #[cfg(feature = "performance-profile")]
                mk_dir_entry(
                    KERNEL_PROFILE_SNAPSHOT_INO,
                    "profile_snapshot",
                    FileType::Regular,
                ),
                #[cfg(feature = "performance-profile")]
                mk_dir_entry(
                    KERNEL_PROFILE_HEALTH_INO,
                    "profile_health",
                    FileType::Regular,
                ),
                mk_dir_entry(KERNEL_ELM_DIR_INO, "elm", FileType::Directory),
                mk_dir_entry(KERNEL_MM_DIR_INO, "mm", FileType::Directory),
            ],
            SysDirKind::KernelMm => vec![
                mk_dir_entry(
                    KERNEL_MM_THP_DIR_INO,
                    "transparent_hugepage",
                    FileType::Directory,
                ),
                mk_dir_entry(KERNEL_MM_KSM_DIR_INO, "ksm", FileType::Directory),
                mk_dir_entry(
                    KERNEL_MM_HUGEPAGES_DIR_INO,
                    "hugepages",
                    FileType::Directory,
                ),
            ],
            SysDirKind::KernelMmThp => vec![
                mk_dir_entry(KERNEL_MM_THP_ENABLED_INO, "enabled", FileType::Regular),
                mk_dir_entry(KERNEL_MM_THP_DEFRAG_INO, "defrag", FileType::Regular),
                mk_dir_entry(
                    KERNEL_MM_THP_SHMEM_ENABLED_INO,
                    "shmem_enabled",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    KERNEL_MM_THP_USE_ZERO_PAGE_INO,
                    "use_zero_page",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    KERNEL_MM_THP_KHUGEPAGED_DIR_INO,
                    "khugepaged",
                    FileType::Directory,
                ),
            ],
            SysDirKind::KernelMmThpKhugepaged => vec![
                mk_dir_entry(
                    KERNEL_MM_KHP_SCAN_SLEEP_INO,
                    "scan_sleep_millisecs",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    KERNEL_MM_KHP_ALLOC_SLEEP_INO,
                    "alloc_sleep_millisecs",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    KERNEL_MM_KHP_MAX_PTES_NONE_INO,
                    "max_ptes_none",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    KERNEL_MM_KHP_PAGES_COLLAPSED_INO,
                    "pages_collapsed",
                    FileType::Regular,
                ),
            ],
            SysDirKind::KernelMmKsm => vec![
                mk_dir_entry(KERNEL_MM_KSM_RUN_INO, "run", FileType::Regular),
                mk_dir_entry(
                    KERNEL_MM_KSM_MERGE_ACROSS_NODES_INO,
                    "merge_across_nodes",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    KERNEL_MM_KSM_PAGES_SHARED_INO,
                    "pages_shared",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    KERNEL_MM_KSM_PAGES_SHARING_INO,
                    "pages_sharing",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    KERNEL_MM_KSM_PAGES_UNSHARED_INO,
                    "pages_unshared",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    KERNEL_MM_KSM_PAGES_VOLATILE_INO,
                    "pages_volatile",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    KERNEL_MM_KSM_FULL_SCANS_INO,
                    "full_scans",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    KERNEL_MM_KSM_MAX_PAGE_SHARING_INO,
                    "max_page_sharing",
                    FileType::Regular,
                ),
            ],
            SysDirKind::KernelMmHugepages => vec![mk_dir_entry(
                KERNEL_MM_HUGEPAGES_SUBDIR_INO,
                "hugepages-2048kB",
                FileType::Directory,
            )],
            SysDirKind::KernelMmHugepagesLeaf => vec![
                mk_dir_entry(KERNEL_MM_HP_NR_INO, "nr_hugepages", FileType::Regular),
                mk_dir_entry(
                    KERNEL_MM_HP_NR_OVERCOMMIT_INO,
                    "nr_overcommit_hugepages",
                    FileType::Regular,
                ),
                mk_dir_entry(KERNEL_MM_HP_FREE_INO, "free_hugepages", FileType::Regular),
                mk_dir_entry(KERNEL_MM_HP_RESV_INO, "resv_hugepages", FileType::Regular),
                mk_dir_entry(
                    KERNEL_MM_HP_SURPLUS_INO,
                    "surplus_hugepages",
                    FileType::Regular,
                ),
            ],
            SysDirKind::KernelElm => {
                let mut entries = Vec::new();
                for slot in ElmSysfsSlot::ALL {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        kernel_elm_slot_ino(*slot),
                        slot.file_name(),
                        FileType::Regular,
                    ) {
                        return entries;
                    }
                }
                entries
            }
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
                for bus in STATIC_SYSFS_BUSES {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        bus_class_ino(bus),
                        bus,
                        FileType::Directory,
                    ) {
                        return entries;
                    }
                }
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
                if !is_static_sysfs_bus(&bus) && !snap.pnp_buses.iter().any(|entry| *entry == bus) {
                    return Vec::new();
                }
                vec![mk_dir_entry(
                    bus_class_devices_ino(&bus),
                    "devices",
                    FileType::Directory,
                )]
            }
            SysDirKind::BusClassDevices { bus } => {
                if is_static_sysfs_bus(&bus) {
                    return Vec::new();
                }
                let Some(_) = snap.pnp_buses.iter().position(|entry| *entry == bus) else {
                    return Vec::new();
                };
                let mut entries = Vec::new();
                for dev in snap.pnp_devices.iter().filter(|dev| dev.bus_type == bus) {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        bus_class_device_link_ino(&bus, &dev.sysfs_name),
                        &dev.sysfs_name,
                        FileType::Symlink,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::Module => Vec::new(),
            SysDirKind::Power => vec![
                mk_dir_entry(POWER_STATE_INO, "state", FileType::Regular),
                mk_dir_entry(POWER_WAKEUP_COUNT_INO, "wakeup_count", FileType::Regular),
            ],
            SysDirKind::Firmware => {
                if installed_device_tree().is_none() {
                    Vec::new()
                } else {
                    vec![
                        mk_dir_entry(firmware_fdt_ino(), "fdt", FileType::Regular),
                        mk_dir_entry(
                            firmware_device_tree_ino(),
                            "devicetree",
                            FileType::Directory,
                        ),
                    ]
                }
            }
            SysDirKind::FirmwareDeviceTree => {
                if installed_device_tree().is_none() {
                    return Vec::new();
                }
                let node_id = DeviceTreeNodeId::root();
                vec![mk_dir_entry(
                    device_tree_node_ino(&node_id),
                    "base",
                    FileType::Directory,
                )]
            }
            SysDirKind::DeviceTreeNode { node_id } => {
                let Some(firmware) = installed_device_tree() else {
                    return Vec::new();
                };
                let live_blob = firmware.live_blob();
                let Some(node) = node_id.node(live_blob.as_ref()) else {
                    return Vec::new();
                };
                let mut entries = Vec::new();

                for property in device_tree_property_projections(node) {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        device_tree_property_ino(&node_id, &property.sysfs_name),
                        &property.sysfs_name,
                        FileType::Regular,
                    ) {
                        return entries;
                    }
                }
                for child in device_tree_child_projections(node) {
                    let child_id = node_id.child(child.node.name(), child.sibling_occurrence);
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        device_tree_node_ino(&child_id),
                        &child.sysfs_name,
                        FileType::Directory,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::DevicesSystem => vec![
                mk_dir_entry(DEVICES_SYSTEM_CPU_INO, "cpu", FileType::Directory),
                mk_dir_entry(numa_root_ino(), "node", FileType::Directory),
                mk_dir_entry(
                    DEVICES_SYSTEM_CLOCKEVENTS_INO,
                    "clockevents",
                    FileType::Directory,
                ),
            ],
            // dev core 没有 clockevent 设备模型,只提供稳定空目录。
            SysDirKind::DevicesSystemClockevents => Vec::new(),
            SysDirKind::DevicesSystemNode => {
                let view = NumaSysfsView::snapshot();
                let mut entries = Vec::new();
                for slot in NumaRootSlot::ALL {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        numa_root_slot_ino(slot.to_u64()),
                        slot.file_name(),
                        FileType::Regular,
                    ) {
                        return entries;
                    }
                }
                for node_id in view.online_nodes {
                    let name = format!("node{node_id}");
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        numa_node_ino(node_id),
                        &name,
                        FileType::Directory,
                    ) {
                        return entries;
                    }
                }
                entries
            }
            SysDirKind::NumaNode { node_id } => {
                if !NumaSysfsView::snapshot().contains_online_node(node_id) {
                    return Vec::new();
                }
                let mut entries = Vec::new();
                for slot in NumaNodeSlot::ALL {
                    if !push_sysfs_dir_entry(
                        &mut entries,
                        numa_node_slot_ino(node_id, slot.to_u64()),
                        slot.file_name(),
                        FileType::Regular,
                    ) {
                        return entries;
                    }
                }
                entries
            }
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
        "holders" => BlockDevSlot::HoldersDir,
        "stat" => BlockDevSlot::Stat,
        "inflight" => BlockDevSlot::Inflight,
        "periodic" => BlockDevSlot::Periodic,
        "diskseq" => BlockDevSlot::Diskseq,
        "device" => BlockDevSlot::DeviceLink,
        "subsystem" => BlockDevSlot::SubsystemLink,
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
        "discard_max_bytes" => BlockQueueSlot::DiscardMaxBytes,
        "discard_granularity" => BlockQueueSlot::DiscardGranularity,
        "write_zeroes_max_bytes" => BlockQueueSlot::WriteZeroesMaxBytes,
        "max_sectors_kb" => BlockQueueSlot::MaxSectorsKb,
        "max_segments" => BlockQueueSlot::MaxSegments,
        "max_segment_size" => BlockQueueSlot::MaxSegmentSize,
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

#[cfg(test)]
mod tests {
    use super::*;

    static DEVICE_TREE_SYSFS_TEST_LOCK: Spinlock<()> = Spinlock::new(());
    static DEVICE_TREE_OVERLAY_TEST_REJECTION: Spinlock<Option<DeviceTreeOverlayRuntimeError>> =
        Spinlock::new(None);
    static DEVICE_TREE_OVERLAY_TEST_CALLS: AtomicU64 = AtomicU64::new(0);
    const TEST_RNG_SEED: &[u8] = &[0xde, 0xad, 0xbe, 0xef, 0x13, 0x37, 0xc0, 0xde];
    const TEST_KASLR_SEED: &[u8] = &[0x91, 0x82, 0x73, 0x64, 0x55, 0x46, 0x37, 0x28];

    #[test]
    fn thread_siblings_keep_cluster_scoped_core_ids_separate() {
        let entry = |logical_id, cluster: &[u32], thread_id| cpu::CpuTopologyEntry {
            logical_id,
            reg: u64::from(logical_id),
            phandle: Some(logical_id + 1),
            interrupt_controller_phandles: Vec::new().into_boxed_slice(),
            compatible: Vec::new(),
            socket_id: Some(0),
            cluster_path: cluster.to_vec().into_boxed_slice(),
            core_id: Some(0),
            thread_id: Some(thread_id),
            capacity_dmips_mhz: None,
        };
        let entries = [entry(0, &[0], 0), entry(1, &[0], 1), entry(2, &[1], 0)];
        let first = cpu_topology_view(0, &entries).unwrap();
        let sibling = cpu_topology_view(1, &entries).unwrap();
        let other_cluster = cpu_topology_view(2, &entries).unwrap();

        assert!(same_thread_sibling(first, sibling));
        assert!(!same_thread_sibling(first, other_cluster));
    }

    #[test]
    fn numa_sysfs_view_matches_linux_node_list_bitmap_and_distance_formats() {
        let topology = numa::NumaTopology {
            node_ids: vec![7, 4, 2, 0],
            distances: vec![numa::NumaDistance {
                from: 0,
                to: 2,
                distance: 21,
            }],
            memory: vec![
                numa::NumaMemoryRange {
                    start: 0x1000,
                    size: 0x1000,
                    node_id: 2,
                },
                numa::NumaMemoryRange {
                    start: 0x2000,
                    size: 0x1000,
                    node_id: 4,
                },
            ],
        };
        let view = NumaSysfsView::new(
            topology,
            vec![
                cpu::CpuNumaEntry {
                    logical_id: 1,
                    node_id: 0,
                },
                cpu::CpuNumaEntry {
                    logical_id: 0,
                    node_id: 0,
                },
                cpu::CpuNumaEntry {
                    logical_id: 33,
                    node_id: 2,
                },
            ],
            1u64 << 33,
        );

        assert_eq!(view.render_root_file(NumaRootSlot::HasCpu), "0,2\n");
        assert_eq!(view.render_root_file(NumaRootSlot::HasMemory), "2,4\n");
        assert_eq!(view.render_root_file(NumaRootSlot::Online), "0,2,4\n");
        assert_eq!(view.render_root_file(NumaRootSlot::Possible), "0,2,4,7\n");
        assert!(view.contains_online_node(4));
        assert!(!view.contains_online_node(7));

        assert_eq!(view.render_node_file(0, NumaNodeSlot::CpuList), "0-1\n");
        assert_eq!(
            view.render_node_file(0, NumaNodeSlot::CpuMap),
            "0,00000003\n"
        );
        assert_eq!(
            view.render_node_file(2, NumaNodeSlot::CpuMap),
            "2,00000000\n"
        );
        assert_eq!(view.render_node_file(4, NumaNodeSlot::CpuList), "\n");
        assert_eq!(
            view.render_node_file(4, NumaNodeSlot::CpuMap),
            "0,00000000\n"
        );
        assert_eq!(
            view.render_node_file(0, NumaNodeSlot::Distance),
            "10 21 20\n"
        );
        assert_eq!(
            view.render_node_file(4, NumaNodeSlot::Distance),
            "20 20 10\n"
        );
    }

    #[test]
    fn empty_numa_sysfs_view_is_stable() {
        let view = NumaSysfsView::new(numa::NumaTopology::default(), Vec::new(), 0);
        for slot in NumaRootSlot::ALL {
            assert_eq!(view.render_root_file(*slot), "\n");
        }
        assert!(view.online_nodes.is_empty());
        assert_eq!(view.render_node_file(0, NumaNodeSlot::CpuList), "\n");
        assert_eq!(view.render_node_file(0, NumaNodeSlot::CpuMap), "0\n");
        assert_eq!(view.render_node_file(0, NumaNodeSlot::Distance), "\n");
        assert_eq!(format_linux_cpumap(0xffff, 16), "ffff\n");
    }

    #[test]
    fn devices_system_publishes_empty_numa_subsystem() {
        let system = SysDirInodeOps {
            kind: SysDirKind::DevicesSystem,
            fs_id: FsId::new(0x4e55),
            weak_sb: Weak::new(),
            snap: Arc::new(SysSnapshot::default()),
        };
        assert_eq!(
            system
                .readdir_entries()
                .into_iter()
                .map(|entry| (entry.name.as_str().to_string(), entry.kind))
                .collect::<Vec<_>>(),
            vec![
                ("cpu".to_string(), FileType::Directory),
                ("node".to_string(), FileType::Directory),
                ("clockevents".to_string(), FileType::Directory),
            ]
        );

        assert!(matches!(system.lookup_child("clockevents"), Ok(_)));

        let node = system.lookup_child("node").unwrap();
        assert_eq!(
            directory_entries(&node),
            vec![
                ("has_cpu".to_string(), FileType::Regular),
                ("has_memory".to_string(), FileType::Regular),
                ("online".to_string(), FileType::Regular),
                ("possible".to_string(), FileType::Regular),
            ]
        );
        assert!(matches!(node.lookup("node0"), Err(VfsError::NotFound)));
    }

    #[test]
    fn sysfs_class_for_major_name_maps_linux_traditional_classes() {
        assert_eq!(sysfs_class_for_major_name("mem"), Some("mem"));
        assert_eq!(sysfs_class_for_major_name("console"), Some("tty"));
        assert_eq!(sysfs_class_for_major_name("tty"), Some("tty"));
        assert_eq!(sysfs_class_for_major_name("misc"), Some("misc"));
        assert_eq!(sysfs_class_for_major_name("uart0"), None);
        assert_eq!(sysfs_class_for_major_name(""), None);
    }

    #[test]
    fn cpu_in_mask_bounds_shifts_and_matches_supported_mask() {
        assert!(cpu_in_mask(0, 0b11));
        assert!(cpu_in_mask(1, 0b11));
        assert!(!cpu_in_mask(2, 0b11));
        // cpu_id 超出 u64 掩码宽度时视为不在掩码内。
        assert!(!cpu_in_mask(64, u64::MAX));
        assert!(!cpu_in_mask(usize::MAX, u64::MAX));
    }

    struct InstalledDeviceTreeReset {
        firmware: Option<Arc<DeviceTreeFirmware>>,
        overlay_hook: Option<DeviceTreeOverlayCommitHook>,
    }

    impl InstalledDeviceTreeReset {
        fn take() -> Self {
            DEVICE_TREE_OVERLAY_TEST_CALLS.store(0, Ordering::Relaxed);
            *DEVICE_TREE_OVERLAY_TEST_REJECTION.lock() = None;
            Self {
                firmware: DEVICE_TREE_FIRMWARE.lock().take(),
                overlay_hook: DEVICE_TREE_OVERLAY_COMMIT_HOOK.lock().take(),
            }
        }
    }

    impl Drop for InstalledDeviceTreeReset {
        fn drop(&mut self) {
            *DEVICE_TREE_FIRMWARE.lock() = self.firmware.take();
            *DEVICE_TREE_OVERLAY_COMMIT_HOOK.lock() = self.overlay_hook.take();
            *DEVICE_TREE_OVERLAY_TEST_REJECTION.lock() = None;
        }
    }

    fn device_tree_overlay_test_commit_hook(
        base: &[u8],
        candidate: &[u8],
    ) -> Result<(), DeviceTreeOverlayRuntimeError> {
        DEVICE_TREE_OVERLAY_TEST_CALLS.fetch_add(1, Ordering::Relaxed);
        assert_eq!(
            fdt::Fdt::parse(base)
                .unwrap()
                .find_node("/soc@0")
                .unwrap()
                .property("state")
                .unwrap()
                .value(),
            b"old\0"
        );
        assert_eq!(
            fdt::Fdt::parse(candidate)
                .unwrap()
                .find_node("/soc@0")
                .unwrap()
                .property("state")
                .unwrap()
                .value(),
            b"new\0"
        );
        match *DEVICE_TREE_OVERLAY_TEST_REJECTION.lock() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    struct CachedDentrySubtreeReset(Arc<Dentry>);

    impl Drop for CachedDentrySubtreeReset {
        fn drop(&mut self) {
            vfs::DCACHE.invalidate_subtree(&self.0);
        }
    }

    fn cached_positive_child(parent: &Arc<Dentry>, name: &str) -> Arc<Dentry> {
        if let Some(cached) = vfs::DCACHE.get(parent, name) {
            assert!(cached.is_positive());
            return cached;
        }
        let parent_inode = parent.inode().unwrap();
        let child_inode = parent_inode.lookup(name).unwrap();
        vfs::DCACHE.insert(Dentry::new_positive(
            name,
            Some(Arc::clone(parent)),
            child_inode,
        ))
    }

    fn cached_negative_child(parent: &Arc<Dentry>, name: &str) -> Arc<Dentry> {
        assert!(matches!(
            parent.inode().unwrap().lookup(name),
            Err(VfsError::NotFound)
        ));
        vfs::DCACHE.insert(Dentry::new_negative(name, Some(Arc::clone(parent))))
    }

    fn push_be32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_be_bytes());
    }

    fn pad_to_u32(out: &mut Vec<u8>) {
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }

    fn add_fdt_string(strings: &mut Vec<u8>, name: &[u8]) -> u32 {
        let offset = strings.len() as u32;
        strings.extend_from_slice(name);
        strings.push(0);
        offset
    }

    fn push_fdt_property(structure: &mut Vec<u8>, name_offset: u32, value: &[u8]) {
        const FDT_PROP: u32 = 3;
        push_be32(structure, FDT_PROP);
        push_be32(structure, value.len() as u32);
        push_be32(structure, name_offset);
        structure.extend_from_slice(value);
        pad_to_u32(structure);
    }

    fn test_dtb() -> Vec<u8> {
        const FDT_BEGIN_NODE: u32 = 1;
        const FDT_END_NODE: u32 = 2;
        const FDT_END: u32 = 9;

        let mut strings = Vec::new();
        let compatible = add_fdt_string(&mut strings, b"compatible");
        let clash = add_fdt_string(&mut strings, b"clash");
        let clash_1 = add_fdt_string(&mut strings, b"clash#1");
        let address_cells = add_fdt_string(&mut strings, b"#address-cells");
        let binary = add_fdt_string(&mut strings, b"binary");
        let empty = add_fdt_string(&mut strings, b"empty");
        let security_password = add_fdt_string(&mut strings, b"security-password");
        let marker = add_fdt_string(&mut strings, b"marker");
        let name = add_fdt_string(&mut strings, b"name");
        let status = add_fdt_string(&mut strings, b"status");
        let rng_seed = add_fdt_string(&mut strings, b"rng-seed");
        let kaslr_seed = add_fdt_string(&mut strings, b"kaslr-seed");

        let mut structure = Vec::new();
        push_be32(&mut structure, FDT_BEGIN_NODE);
        structure.push(0);
        pad_to_u32(&mut structure);
        push_fdt_property(&mut structure, compatible, b"test,board\0");
        push_fdt_property(&mut structure, clash, &[]);
        push_fdt_property(&mut structure, clash_1, &[]);
        push_fdt_property(&mut structure, compatible, b"test,duplicate\0");

        push_be32(&mut structure, FDT_BEGIN_NODE);
        structure.extend_from_slice(b"chosen\0");
        pad_to_u32(&mut structure);
        push_fdt_property(&mut structure, rng_seed, TEST_RNG_SEED);
        push_fdt_property(&mut structure, kaslr_seed, TEST_KASLR_SEED);
        push_be32(&mut structure, FDT_END_NODE);

        push_be32(&mut structure, FDT_BEGIN_NODE);
        structure.extend_from_slice(b"soc@0\0");
        pad_to_u32(&mut structure);
        push_fdt_property(&mut structure, address_cells, &[0, 0, 0, 2]);
        push_fdt_property(&mut structure, binary, &[0, 0xff, 1, 0x80, 0]);
        push_fdt_property(&mut structure, empty, &[]);
        push_fdt_property(&mut structure, security_password, b"s3cr3t\0");
        push_be32(&mut structure, FDT_END_NODE);

        push_be32(&mut structure, FDT_BEGIN_NODE);
        structure.extend_from_slice(b"disabled@0\0");
        pad_to_u32(&mut structure);
        push_fdt_property(&mut structure, status, b"disabled\0");
        push_be32(&mut structure, FDT_END_NODE);

        push_be32(&mut structure, FDT_BEGIN_NODE);
        structure.extend_from_slice(b"clash\0");
        pad_to_u32(&mut structure);
        push_fdt_property(&mut structure, marker, &[0x42]);
        push_be32(&mut structure, FDT_END_NODE);

        push_be32(&mut structure, FDT_BEGIN_NODE);
        structure.extend_from_slice(b"clash\0");
        pad_to_u32(&mut structure);
        push_fdt_property(&mut structure, marker, &[0x44]);
        push_be32(&mut structure, FDT_END_NODE);

        push_be32(&mut structure, FDT_BEGIN_NODE);
        structure.extend_from_slice(b"name\0");
        pad_to_u32(&mut structure);
        push_fdt_property(&mut structure, marker, &[0x43]);
        push_fdt_property(&mut structure, name, b"explicit\0");
        push_be32(&mut structure, FDT_END_NODE);
        push_be32(&mut structure, FDT_END_NODE);
        push_be32(&mut structure, FDT_END);

        finish_test_dtb(structure, strings)
    }

    fn finish_test_dtb(structure: Vec<u8>, strings: Vec<u8>) -> Vec<u8> {
        const HEADER_SIZE: u32 = 40;
        const RESERVATION_SIZE: u32 = 16;
        let structure_offset = HEADER_SIZE + RESERVATION_SIZE;
        let strings_offset = structure_offset + structure.len() as u32;
        let total_size = strings_offset + strings.len() as u32;

        let mut blob = Vec::with_capacity(total_size as usize);
        for value in [
            fdt::DTB_MAGIC,
            total_size,
            structure_offset,
            strings_offset,
            HEADER_SIZE,
            17,
            16,
            0,
            strings.len() as u32,
            structure.len() as u32,
        ] {
            push_be32(&mut blob, value);
        }
        blob.resize((HEADER_SIZE + RESERVATION_SIZE) as usize, 0);
        blob.extend_from_slice(&structure);
        blob.extend_from_slice(&strings);
        blob
    }

    fn duplicate_seed_test_dtb() -> Vec<u8> {
        const FDT_BEGIN_NODE: u32 = 1;
        const FDT_END_NODE: u32 = 2;
        const FDT_END: u32 = 9;

        let mut strings = Vec::new();
        let rng_seed = add_fdt_string(&mut strings, b"rng-seed");
        let kaslr_seed = add_fdt_string(&mut strings, b"kaslr-seed");
        let mut structure = Vec::new();
        push_be32(&mut structure, FDT_BEGIN_NODE);
        structure.push(0);
        pad_to_u32(&mut structure);

        push_be32(&mut structure, FDT_BEGIN_NODE);
        structure.extend_from_slice(b"chosen\0");
        pad_to_u32(&mut structure);
        push_fdt_property(&mut structure, rng_seed, &[]);
        push_fdt_property(&mut structure, rng_seed, b"duplicate-rng-secret");
        push_fdt_property(&mut structure, kaslr_seed, b"duplicate-kaslr-secret");
        push_be32(&mut structure, FDT_END_NODE);

        push_be32(&mut structure, FDT_BEGIN_NODE);
        structure.extend_from_slice(b"chosen@0\0");
        pad_to_u32(&mut structure);
        push_fdt_property(&mut structure, rng_seed, b"legacy-rng-secret");
        push_fdt_property(&mut structure, kaslr_seed, b"legacy-kaslr-secret");
        push_be32(&mut structure, FDT_END_NODE);
        push_be32(&mut structure, FDT_END_NODE);
        push_be32(&mut structure, FDT_END);
        finish_test_dtb(structure, strings)
    }

    fn owned_property(name: &str, value: &[u8]) -> fdt::OwnedProperty {
        fdt::OwnedProperty {
            name: name.to_string(),
            value: value.to_vec(),
        }
    }

    fn valid_live_test_dtb() -> Vec<u8> {
        let mut root = fdt::OwnedNode::new("");
        root.properties
            .push(owned_property("compatible", b"test,live-board\0"));

        let mut chosen = fdt::OwnedNode::new("chosen");
        chosen
            .properties
            .push(owned_property("rng-seed", TEST_RNG_SEED));
        chosen
            .properties
            .push(owned_property("kaslr-seed", TEST_KASLR_SEED));
        root.children.push(chosen);

        let mut soc = fdt::OwnedNode::new("soc@0");
        soc.properties.push(owned_property("state", b"old\0"));
        root.children.push(soc);

        fdt::OwnedTree {
            root,
            reservations: Vec::new(),
            boot_cpuid_phys: None,
        }
        .to_dtb()
        .unwrap()
    }

    fn overlay_fragment(name: &str, target_path: &str, contents: fdt::OwnedNode) -> fdt::OwnedNode {
        let mut fragment = fdt::OwnedNode::new(name);
        let mut encoded_path = target_path.as_bytes().to_vec();
        encoded_path.push(0);
        fragment.properties.push(fdt::OwnedProperty {
            name: "target-path".to_string(),
            value: encoded_path,
        });
        fragment.children.push(contents);
        fragment
    }

    fn valid_live_test_overlay() -> Vec<u8> {
        let mut root = fdt::OwnedNode::new("");

        let mut root_contents = fdt::OwnedNode::new("__overlay__");
        root_contents.properties.push(owned_property(
            "overlay-root-padding",
            b"force every following structure offset to move\0",
        ));
        root.children
            .push(overlay_fragment("fragment@0", "/", root_contents));

        let mut soc_contents = fdt::OwnedNode::new("__overlay__");
        soc_contents
            .properties
            .push(owned_property("state", b"new\0"));
        let mut device = fdt::OwnedNode::new("device@10");
        device
            .properties
            .push(owned_property("compatible", b"test,overlay-device\0"));
        soc_contents.children.push(device);
        root.children
            .push(overlay_fragment("fragment@1", "/soc@0", soc_contents));

        let mut chosen_contents = fdt::OwnedNode::new("__overlay__");
        chosen_contents
            .properties
            .push(owned_property("rng-seed", b"must-not-leak"));
        root.children
            .push(overlay_fragment("fragment@2", "/chosen", chosen_contents));

        fdt::OwnedTree {
            root,
            reservations: Vec::new(),
            boot_cpuid_phys: None,
        }
        .to_dtb()
        .unwrap()
    }

    fn missing_target_test_overlay() -> Vec<u8> {
        let mut root = fdt::OwnedNode::new("");
        let mut contents = fdt::OwnedNode::new("__overlay__");
        contents
            .properties
            .push(owned_property("state", b"broken\0"));
        root.children
            .push(overlay_fragment("fragment@0", "/does-not-exist", contents));
        fdt::OwnedTree {
            root,
            reservations: Vec::new(),
            boot_cpuid_phys: None,
        }
        .to_dtb()
        .unwrap()
    }

    fn read_binary_inode(inode: &Inode, expected_len: usize) -> Vec<u8> {
        let file = inode
            .open_ops(&OpenOptions::default(), &Credentials::root())
            .unwrap();
        let mut bytes = vec![0; expected_len];
        let read = file.read_at(&mut bytes, 0).unwrap();
        assert_eq!(read, bytes.len());
        let mut eof = [0u8; 1];
        assert_eq!(file.read_at(&mut eof, read as u64), Ok(0));
        bytes
    }

    fn directory_entries(inode: &Inode) -> Vec<(String, FileType)> {
        let file = inode
            .open_ops(&OpenOptions::default(), &Credentials::root())
            .unwrap();
        let mut entries = Vec::new();
        file.readdir(0, &mut |entry| {
            entries.push((entry.name.as_str().to_string(), entry.kind));
            ControlFlow::Continue(())
        })
        .unwrap();
        entries
    }

    #[test]
    fn regular_file_reads_stable_open_snapshot() {
        let file = SysRegFileOps {
            kind: SysRegFile::Hostname,
            snap: Arc::new(SysSnapshot::default()),
            snapshot: Some(b"trace-snapshot\n".to_vec().into_boxed_slice()),
        };
        let mut first = [0u8; 6];
        let first_len = file.read_at(&mut first, 0).unwrap();
        assert_eq!(&first[..first_len], b"trace-");

        let mut second = [0u8; 16];
        let second_len = file.read_at(&mut second, first_len as u64).unwrap();
        assert_eq!(&second[..second_len], b"snapshot\n");
        assert_eq!(
            file.read_at(&mut second, (first_len + second_len) as u64),
            Ok(0)
        );
    }

    #[test]
    fn device_tree_safe_name_matches_linux_retry_limit() {
        assert_eq!(
            device_tree_safe_name("node", |candidate| candidate != "node#16"),
            Some("node#16".to_string())
        );
        assert_eq!(device_tree_safe_name("node", |_| true), None);
        assert_eq!(
            device_tree_safe_name("node", |_| false),
            Some("node".to_string())
        );
    }

    #[test]
    fn device_tree_seed_scrubbing_covers_all_legacy_nodes_and_duplicate_properties() {
        let blob = duplicate_seed_test_dtb();
        let input = fdt::Fdt::parse(&blob).unwrap();
        let firmware = DeviceTreeFirmware::from_fdt(&input).unwrap();
        for secret in [
            b"duplicate-rng-secret".as_slice(),
            b"duplicate-kaslr-secret".as_slice(),
            b"legacy-rng-secret".as_slice(),
            b"legacy-kaslr-secret".as_slice(),
        ] {
            assert!(
                !firmware
                    .boot_blob
                    .windows(secret.len())
                    .any(|window| window == secret)
            );
        }

        let sanitized = fdt::Fdt::parse(&firmware.boot_blob).unwrap();
        let chosen_nodes = sanitized
            .root()
            .children()
            .filter(|node| matches!(node.name(), "chosen" | "chosen@0"))
            .collect::<Vec<_>>();
        assert_eq!(chosen_nodes.len(), 2);
        assert!(chosen_nodes.iter().all(|chosen| {
            chosen
                .properties()
                .all(|property| !matches!(property.name(), "rng-seed" | "kaslr-seed"))
        }));
    }

    #[test]
    fn first_device_tree_install_invalidates_preexisting_negative_dentries() {
        let _test_lock = DEVICE_TREE_SYSFS_TEST_LOCK.lock();
        let _reset = InstalledDeviceTreeReset::take();
        let sysfs = SysFsDriver.mount(None, "").unwrap();
        let root = Arc::clone(&sysfs.root_dentry);
        let firmware = cached_positive_child(&root, "firmware");
        let _cached_reset = CachedDentrySubtreeReset(Arc::clone(&firmware));
        let negative_fdt = cached_negative_child(&firmware, "fdt");
        let negative_tree = cached_negative_child(&firmware, "devicetree");

        install_device_tree_blob(&test_dtb()).unwrap();

        assert!(negative_fdt.is_invalid());
        assert!(negative_tree.is_invalid());
        assert!(vfs::DCACHE.get(&firmware, "fdt").is_none());
        assert!(vfs::DCACHE.get(&firmware, "devicetree").is_none());
        assert!(cached_positive_child(&firmware, "fdt").is_positive());
        assert!(cached_positive_child(&firmware, "devicetree").is_positive());
    }

    #[test]
    fn device_tree_projection_matches_linux_firmware_layout() {
        let _test_lock = DEVICE_TREE_SYSFS_TEST_LOCK.lock();
        let _reset = InstalledDeviceTreeReset::take();
        let fs_id = FsId::new(0x4454);
        let firmware_dir = SysDirInodeOps {
            kind: SysDirKind::Firmware,
            fs_id,
            weak_sb: Weak::new(),
            snap: Arc::new(SysSnapshot::default()),
        };

        assert!(firmware_dir.readdir_entries().is_empty());
        assert!(matches!(
            firmware_dir.lookup_child("fdt"),
            Err(VfsError::NotFound)
        ));

        let blob = test_dtb();
        let input = fdt::Fdt::parse(&blob).unwrap();
        let root_id = DeviceTreeNodeId::root();
        let first_clash_id = root_id.child("clash", 0);
        let second_clash_id = root_id.child("clash", 1);
        let name_id = root_id.child("name", 0);
        assert_eq!(
            input
                .find_node("/chosen")
                .unwrap()
                .property("rng-seed")
                .unwrap()
                .value(),
            TEST_RNG_SEED
        );
        install_device_tree_blob(&blob).unwrap();
        // 重复安装同一份仍含 seed 的输入，会产生相同的清理后投影。
        install_device_tree_blob(&blob).unwrap();
        assert!(device_tree_installed());
        let firmware_inode = build_dir_inode(
            fs_id,
            &Weak::new(),
            &Arc::new(SysSnapshot::default()),
            FIRMWARE_DIR_INO,
            SysDirKind::Firmware,
        );
        assert_eq!(firmware_inode.stat().unwrap().mode & 0o777, 0o755);
        assert_eq!(
            directory_entries(&firmware_inode),
            vec![
                ("fdt".to_string(), FileType::Regular),
                ("devicetree".to_string(), FileType::Directory),
            ]
        );

        let raw_fdt = firmware_dir.lookup_child("fdt").unwrap();
        let raw_stat = raw_fdt.stat().unwrap();
        assert_eq!(raw_stat.mode & 0o777, 0o400);
        assert_eq!(raw_stat.size, blob.len() as i64);
        let raw_file = raw_fdt
            .open_ops(&OpenOptions::default(), &Credentials::root())
            .unwrap();
        assert_eq!(
            raw_file.write_at(&[0], 0),
            Err(VfsError::ReadOnlyFilesystem)
        );
        let sanitized_blob = read_binary_inode(&raw_fdt, blob.len());
        assert_ne!(sanitized_blob, blob);
        assert!(
            !sanitized_blob
                .windows(TEST_RNG_SEED.len())
                .any(|window| window == TEST_RNG_SEED)
        );
        assert!(
            !sanitized_blob
                .windows(TEST_KASLR_SEED.len())
                .any(|window| window == TEST_KASLR_SEED)
        );
        let sanitized = fdt::Fdt::parse(&sanitized_blob).unwrap();
        assert!(
            sanitized
                .find_node("/chosen")
                .unwrap()
                .property("rng-seed")
                .is_none()
        );
        assert!(
            sanitized
                .find_node("/chosen")
                .unwrap()
                .property("kaslr-seed")
                .is_none()
        );
        let encoded = input
            .find_node("/chosen")
            .unwrap()
            .property("rng-seed")
            .unwrap()
            .encoded_structure_range();
        let structure_start = input.header().off_dt_struct as usize;
        assert!(
            sanitized_blob[structure_start + encoded.start..structure_start + encoded.end]
                .chunks_exact(4)
                .all(|token| token == 4u32.to_be_bytes())
        );

        let mut conflicting_blob = blob.clone();
        let compatible_offset = conflicting_blob
            .windows(b"test,board\0".len())
            .position(|window| window == b"test,board\0")
            .unwrap();
        conflicting_blob[compatible_offset] = b'T';
        assert!(matches!(
            install_device_tree_blob(&conflicting_blob),
            Err(DeviceTreeSysfsInstallError::AlreadyInstalled)
        ));
        assert_eq!(
            read_binary_inode(&raw_fdt, sanitized_blob.len()),
            sanitized_blob
        );

        let device_tree = firmware_dir.lookup_child("devicetree").unwrap();
        assert_eq!(device_tree.stat().unwrap().mode & 0o777, 0o755);
        assert_eq!(
            directory_entries(&device_tree),
            vec![("base".to_string(), FileType::Directory)]
        );
        let base = device_tree.lookup("base").unwrap();
        assert_eq!(base.stat().unwrap().mode & 0o777, 0o755);
        assert_eq!(base.ino(), device_tree_node_ino(&root_id));
        assert_eq!(
            directory_entries(&base),
            vec![
                ("compatible".to_string(), FileType::Regular),
                ("clash".to_string(), FileType::Regular),
                ("clash#1".to_string(), FileType::Regular),
                ("compatible#1".to_string(), FileType::Regular),
                ("name".to_string(), FileType::Regular),
                ("chosen".to_string(), FileType::Directory),
                ("soc@0".to_string(), FileType::Directory),
                ("disabled@0".to_string(), FileType::Directory),
                ("clash#2".to_string(), FileType::Directory),
                ("clash#3".to_string(), FileType::Directory),
                ("name#1".to_string(), FileType::Directory),
            ]
        );

        let compatible = base.lookup("compatible").unwrap();
        assert_eq!(compatible.stat().unwrap().mode & 0o777, 0o444);
        assert_eq!(read_binary_inode(&compatible, 11), b"test,board\0");
        let compatible_file = compatible
            .open_ops(&OpenOptions::default(), &Credentials::root())
            .unwrap();
        assert_eq!(
            compatible_file.write_at(b"changed", 0),
            Err(VfsError::ReadOnlyFilesystem)
        );
        assert_eq!(
            read_binary_inode(&base.lookup("compatible#1").unwrap(), 15),
            b"test,duplicate\0"
        );

        let root_name = base.lookup("name").unwrap();
        assert_eq!(root_name.stat().unwrap().mode & 0o777, 0o444);
        assert_eq!(root_name.size(), 1);
        assert_eq!(read_binary_inode(&root_name, 1), b"\0");

        let chosen = base.lookup("chosen").unwrap();
        assert_eq!(
            directory_entries(&chosen),
            vec![("name".to_string(), FileType::Regular)]
        );
        assert!(matches!(chosen.lookup("rng-seed"), Err(VfsError::NotFound)));

        assert_eq!(base.lookup("clash").unwrap().kind(), FileType::Regular);
        assert_eq!(base.lookup("clash#1").unwrap().kind(), FileType::Regular);
        let renamed_clash = base.lookup("clash#2").unwrap();
        assert_eq!(renamed_clash.kind(), FileType::Directory);
        assert_eq!(renamed_clash.ino(), device_tree_node_ino(&first_clash_id));
        assert_eq!(
            directory_entries(&renamed_clash),
            vec![
                ("marker".to_string(), FileType::Regular),
                ("name".to_string(), FileType::Regular),
            ]
        );
        assert_eq!(
            read_binary_inode(&renamed_clash.lookup("name").unwrap(), 6),
            b"clash\0"
        );
        assert_eq!(
            read_binary_inode(&renamed_clash.lookup("marker").unwrap(), 1),
            [0x42]
        );

        let second_clash = base.lookup("clash#3").unwrap();
        assert_ne!(second_clash.ino(), renamed_clash.ino());
        assert_eq!(second_clash.ino(), device_tree_node_ino(&second_clash_id));
        assert_eq!(
            read_binary_inode(&second_clash.lookup("marker").unwrap(), 1),
            [0x44]
        );

        let disabled = base.lookup("disabled@0").unwrap();
        assert_eq!(
            read_binary_inode(&disabled.lookup("status").unwrap(), 9),
            b"disabled\0"
        );

        let renamed_name = base.lookup("name#1").unwrap();
        assert_eq!(renamed_name.kind(), FileType::Directory);
        assert_eq!(renamed_name.ino(), device_tree_node_ino(&name_id));
        assert_eq!(
            directory_entries(&renamed_name),
            vec![
                ("marker".to_string(), FileType::Regular),
                ("name".to_string(), FileType::Regular),
            ]
        );
        assert_eq!(
            read_binary_inode(&renamed_name.lookup("name").unwrap(), 9),
            b"explicit\0"
        );

        let soc = base.lookup("soc@0").unwrap();
        assert_eq!(soc.stat().unwrap().mode & 0o777, 0o755);
        assert_eq!(
            directory_entries(&soc),
            vec![
                ("#address-cells".to_string(), FileType::Regular),
                ("binary".to_string(), FileType::Regular),
                ("empty".to_string(), FileType::Regular),
                ("security-password".to_string(), FileType::Regular),
                ("name".to_string(), FileType::Regular),
            ]
        );
        assert_eq!(
            read_binary_inode(&soc.lookup("#address-cells").unwrap(), 4),
            [0, 0, 0, 2]
        );
        assert_eq!(
            read_binary_inode(&soc.lookup("binary").unwrap(), 5),
            [0, 0xff, 1, 0x80, 0]
        );
        assert_eq!(read_binary_inode(&soc.lookup("empty").unwrap(), 0), []);
        assert_eq!(read_binary_inode(&soc.lookup("name").unwrap(), 4), b"soc\0");

        let security = soc.lookup("security-password").unwrap();
        let security_stat = security.stat().unwrap();
        assert_eq!(security_stat.mode & 0o777, 0o400);
        assert_eq!(security_stat.size, 0);
        assert_eq!(read_binary_inode(&security, 7), b"s3cr3t\0");
        assert!(matches!(soc.lookup("missing"), Err(VfsError::NotFound)));
    }

    #[test]
    fn device_tree_overlay_updates_only_live_tree_atomically() {
        let _test_lock = DEVICE_TREE_SYSFS_TEST_LOCK.lock();
        let _reset = InstalledDeviceTreeReset::take();
        assert!(matches!(
            apply_device_tree_overlay(&valid_live_test_overlay()),
            Err(DeviceTreeSysfsOverlayError::NotInstalled)
        ));

        let blob = valid_live_test_dtb();
        install_device_tree_blob(&blob).unwrap();
        let firmware = installed_device_tree().unwrap();
        let initial_live = firmware.live_blob();
        assert!(!Arc::ptr_eq(&firmware.boot_blob, &initial_live));
        let update = firmware.begin_overlay_update().unwrap();
        assert!(matches!(
            apply_device_tree_overlay(&valid_live_test_overlay()),
            Err(DeviceTreeSysfsOverlayError::UpdateInProgress)
        ));
        drop(update);

        // 同一启动输入重复安装是幂等操作，且不会建立第二套发布状态。
        install_device_tree_blob(&blob).unwrap();
        assert!(Arc::ptr_eq(&firmware, &installed_device_tree().unwrap()));

        let sysfs = SysFsDriver.mount(None, "").unwrap();
        let sysfs_root = Arc::clone(&sysfs.root_dentry);
        let firmware_dentry = cached_positive_child(&sysfs_root, "firmware");
        let _cached_reset = CachedDentrySubtreeReset(Arc::clone(&firmware_dentry));
        let raw_fdt_dentry = cached_positive_child(&firmware_dentry, "fdt");
        let raw_fdt = raw_fdt_dentry.inode().unwrap();
        let startup_blob = read_binary_inode(&raw_fdt, blob.len());
        assert_ne!(startup_blob, blob);

        let device_tree_dentry = cached_positive_child(&firmware_dentry, "devicetree");
        let base_dentry = cached_positive_child(&device_tree_dentry, "base");
        let soc_dentry = cached_positive_child(&base_dentry, "soc@0");
        let old_state_dentry = cached_positive_child(&soc_dentry, "state");
        let negative_device_dentry = vfs::DCACHE.insert(Dentry::new_negative(
            "device@10",
            Some(Arc::clone(&soc_dentry)),
        ));
        let base = base_dentry.inode().unwrap();
        let soc = soc_dentry.inode().unwrap();
        let soc_ino = soc.ino();
        let old_state_inode = old_state_dentry.inode().unwrap();
        let state_ino = old_state_inode.ino();
        let old_state_file = old_state_inode
            .open_ops(&OpenOptions::default(), &Credentials::root())
            .unwrap();
        assert_eq!(read_binary_inode(&old_state_inode, 4), b"old\0");

        install_device_tree_overlay_commit_hook(device_tree_overlay_test_commit_hook).unwrap();
        *DEVICE_TREE_OVERLAY_TEST_REJECTION.lock() =
            Some(DeviceTreeOverlayRuntimeError::UnsupportedChange);
        let live_before_rejection = firmware.live_blob();
        assert!(matches!(
            apply_device_tree_overlay(&valid_live_test_overlay()),
            Err(DeviceTreeSysfsOverlayError::RuntimeRejected(
                DeviceTreeOverlayRuntimeError::UnsupportedChange
            ))
        ));
        assert!(Arc::ptr_eq(&live_before_rejection, &firmware.live_blob()));
        assert!(device_tree_dentry.is_positive());
        assert!(base_dentry.is_positive());
        assert!(soc_dentry.is_positive());
        assert_eq!(read_binary_inode(&old_state_inode, 4), b"old\0");
        assert_eq!(DEVICE_TREE_OVERLAY_TEST_CALLS.load(Ordering::Relaxed), 1);

        *DEVICE_TREE_OVERLAY_TEST_REJECTION.lock() = None;
        apply_device_tree_overlay(&valid_live_test_overlay()).unwrap();
        assert_eq!(DEVICE_TREE_OVERLAY_TEST_CALLS.load(Ordering::Relaxed), 2);

        // raw FDT 永远保持安装时的 seed 清理快照，live 重序列化不会覆盖它。
        assert_eq!(
            read_binary_inode(&raw_fdt, startup_blob.len()),
            startup_blob
        );
        assert!(raw_fdt_dentry.is_positive());
        assert!(Arc::ptr_eq(
            &raw_fdt_dentry,
            &vfs::DCACHE.get(&firmware_dentry, "fdt").unwrap()
        ));
        assert_eq!(
            read_binary_inode(
                &firmware_dentry.inode().unwrap().lookup("fdt").unwrap(),
                startup_blob.len(),
            ),
            startup_blob
        );

        // 成功交换同时失效旧 live dentry 子树（包括负向缓存），但不触碰 raw FDT。
        assert!(device_tree_dentry.is_invalid());
        assert!(base_dentry.is_invalid());
        assert!(soc_dentry.is_invalid());
        assert!(old_state_dentry.is_invalid());
        assert!(negative_device_dentry.is_invalid());
        assert!(vfs::DCACHE.get(&firmware_dentry, "devicetree").is_none());

        let refreshed_device_tree = cached_positive_child(&firmware_dentry, "devicetree");
        let refreshed_base_dentry = cached_positive_child(&refreshed_device_tree, "base");
        let refreshed_soc_dentry = cached_positive_child(&refreshed_base_dentry, "soc@0");
        let refreshed_state_dentry = cached_positive_child(&refreshed_soc_dentry, "state");
        let refreshed_device_dentry = cached_positive_child(&refreshed_soc_dentry, "device@10");
        assert_eq!(
            read_binary_inode(&refreshed_state_dentry.inode().unwrap(), 4),
            b"new\0"
        );
        assert_eq!(
            refreshed_device_dentry.inode().unwrap().kind(),
            FileType::Directory
        );

        // 已有目录 inode 的新 lookup/readdir 使用当前 live Arc；路径 inode 身份不因
        // structure block 重序列化和 offset 整体移动而变化。
        let refreshed_soc = base.lookup("soc@0").unwrap();
        assert_eq!(refreshed_soc.ino(), soc_ino);
        assert!(
            directory_entries(&base)
                .iter()
                .any(|(name, kind)| name == "overlay-root-padding" && *kind == FileType::Regular)
        );
        assert!(
            directory_entries(&soc)
                .iter()
                .any(|(name, kind)| name == "device@10" && *kind == FileType::Directory)
        );
        let new_state_inode = refreshed_soc.lookup("state").unwrap();
        assert_eq!(new_state_inode.ino(), state_ino);
        assert_eq!(read_binary_inode(&new_state_inode, 4), b"new\0");
        assert_eq!(
            read_binary_inode(
                &refreshed_soc
                    .lookup("device@10")
                    .unwrap()
                    .lookup("compatible")
                    .unwrap(),
                20
            ),
            b"test,overlay-device\0"
        );

        // overlay 试图重新注入 seed 时，live sysfs 仍不得公开已消费的启动秘密。
        let chosen = base.lookup("chosen").unwrap();
        assert!(matches!(chosen.lookup("rng-seed"), Err(VfsError::NotFound)));

        // overlay 前已打开的二进制文件持有旧 Arc，不随全局 live tree 交换漂移。
        let mut old_state = [0u8; 4];
        assert_eq!(old_state_file.read_at(&mut old_state, 0), Ok(4));
        assert_eq!(&old_state, b"old\0");
        assert_eq!(read_binary_inode(&old_state_inode, 4), b"old\0");

        // 再次安装相同启动 blob 不会把已经应用的 live overlay 回滚。
        install_device_tree_blob(&blob).unwrap();
        assert_eq!(
            read_binary_inode(&base.lookup("soc@0").unwrap().lookup("state").unwrap(), 4),
            b"new\0"
        );

        let live_before_failure = firmware.live_blob();
        assert!(matches!(
            apply_device_tree_overlay(&missing_target_test_overlay()),
            Err(DeviceTreeSysfsOverlayError::InvalidOverlay(
                fdt::OverlayError::MissingNode(_)
            ))
        ));
        let live_after_failure = firmware.live_blob();
        assert!(Arc::ptr_eq(&live_before_failure, &live_after_failure));
        assert_eq!(
            read_binary_inode(&base.lookup("soc@0").unwrap().lookup("state").unwrap(), 4),
            b"new\0"
        );
        assert_eq!(
            read_binary_inode(&raw_fdt, startup_blob.len()),
            startup_blob
        );
    }

    #[cfg(feature = "performance-profile")]
    #[test]
    fn profile_control_accepts_shell_truncate() {
        assert_eq!(truncate_sys_reg(SysRegFile::ProfileControl, 0), Ok(()));
        assert_eq!(
            truncate_sys_reg(SysRegFile::ProfileControl, 1),
            Err(VfsError::InvalidArgument)
        );
        assert_eq!(
            truncate_sys_reg(SysRegFile::ProfileStats, 0),
            Err(VfsError::ReadOnlyFilesystem)
        );
    }

    #[cfg(feature = "performance-profile")]
    #[test]
    fn profile_control_accepts_split_trailing_newline() {
        let file = SysRegFileOps {
            kind: SysRegFile::ProfileControl,
            snap: Arc::new(SysSnapshot::default()),
            snapshot: None,
        };
        assert_eq!(file.write_at(b"\n", 6), Ok(1));
        assert_eq!(file.write_at(b"x", 6), Err(VfsError::InvalidArgument));
    }
}

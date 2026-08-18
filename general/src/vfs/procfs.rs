//! procfs：`/proc` 虚拟文件系统。
//!
//! 本模块提供进程、挂载、内存和设备等运行时状态的文本视图。设备相关视图通过
//! function 注册表的兼容层 helper 获取字符/块设备快照，不直接依赖具体 function 类型。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::ops::ControlFlow;
use core::ops::Range;
use core::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use mm::VmFlags;
use sched::ids::{Capability as SchedCapability, Credentials as SchedCredentials};
use sched::{PidT, RlimitPair, SchedPolicy, Task, TaskState};
use vfs::FS_REGISTRY;
use vfs::VfsContext;
use vfs::cred::{Credentials, Gid, Uid};
use vfs::dentry::{Dentry, SmallStr};
use vfs::error::{VfsError, VfsResult};
use vfs::fdtable::{Fd, FdTable};
use vfs::file::{DirEntry, File, FileOps, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::mount::MountFlags;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{FsDriver, FsDriverFlags, Superblock, SuperblockOps};
use vfs::sync::Spinlock;

use super::nsfs::ProcNsKind;
use crate::mm::vm_space::dump_vmas;
use crate::mm::{VmSpace, page_size};

use super::{current_vfs_context, namespace_path};
use crate::dev::enumerate::{DEVICES, PNP_DEVICES};
use crate::dev::pnp::{PnpDependency, PnpId, PnpResourceKind, PnpState};
use crate::vfs::device_files::projection::{
    published_block_devnodes, render_function_projection_diagnostics,
};
use crate::vfs::user_api::device_numbers::{self, DeviceNumberKind};

static PROCFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);
static HOTPLUG_PATH: Spinlock<String> = Spinlock::new(String::new());
static FILE_MAX: AtomicU64 = AtomicU64::new(i64::MAX as u64);
static KERNEL_TAINT_FLAGS: AtomicU64 = AtomicU64::new(0);
/// `/proc/sys/kernel/pid_max` 的 procfs 本地可写投影。
///
/// 调度器 pid 注册表在启动时用 `DEFAULT_PID_MAX` 固化，运行期无 setter；
/// 这里保留可写 ABI（Linux 允许写 pid_max）并维护一个独立投影值，供诊断
/// 与工具兼容使用，不改变已建注册表的上限。
static PID_MAX: AtomicI32 = AtomicI32::new(32768);
/// `/proc/[pid]/oom_score_adj` 的 procfs 本地投影（无 OOM killer，仅记账）。
static OOM_SCORE_ADJ: Spinlock<BTreeMap<PidT, i32>> = Spinlock::new(BTreeMap::new());
/// procfs 自有补充 sysctl 的字符串/数值存储。
static EXTRA_SYSCTL_TEXT: Spinlock<BTreeMap<&'static str, String>> = Spinlock::new(BTreeMap::new());
static EXTRA_SYSCTL_NUM: Spinlock<BTreeMap<&'static str, u64>> = Spinlock::new(BTreeMap::new());

// ── /proc/net 数据源（由内核 net_runtime 安装）───────────────────────────────

static ROUTE_SNAPSHOT_PROVIDER: Spinlock<Option<fn() -> Vec<net::control::RouteEntry>>> =
    Spinlock::new(None);
static NEIGHBOR_SNAPSHOT_PROVIDER: Spinlock<
    Option<fn() -> Vec<net::control::NeighborSnapshotEntry>>,
> = Spinlock::new(None);
static DNS_SNAPSHOT_PROVIDER: Spinlock<Option<fn() -> Vec<net::IpAddr>>> = Spinlock::new(None);
static ADDR_SNAPSHOT_PROVIDER: Spinlock<Option<fn() -> Vec<net::control::AddressEntry>>> =
    Spinlock::new(None);

// ── /proc/sysvipc 与 /proc/keys 数据源（由内核 ipc 块安装；缺失时为兼容空视图）─

/// 一条 SysV shm 段快照（procfs 自有布局，避免反向依赖 ipc 内部结构）。
#[derive(Clone, Copy)]
pub struct ProcSysvShmEntry {
    pub id: i32,
    pub key: i32,
    pub size_bytes: u64,
    pub nattch: u32,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    pub mode: u16,
    pub cpid: i32,
    pub lpid: i32,
    pub pages: u64,
    pub locked: bool,
    pub marked_for_removal: bool,
    pub atime: i64,
    pub dtime: i64,
    pub ctime: i64,
}

/// 一条 SysV sem 集合快照。
#[derive(Clone, Copy)]
pub struct ProcSysvSemEntry {
    pub id: i32,
    pub key: i32,
    pub nsems: u32,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    pub mode: u16,
    pub otime: i64,
    pub ctime: i64,
}

/// 一条 SysV 消息队列快照。
#[derive(Clone, Copy)]
pub struct ProcSysvMsgEntry {
    pub id: i32,
    pub key: i32,
    pub qbytes: u64,
    pub qnum: u64,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    pub mode: u16,
    pub lspid: i32,
    pub lrpid: i32,
    pub stime: i64,
    pub rtime: i64,
    pub ctime: i64,
}

/// 一条 POSIX key 快照。
#[derive(Clone)]
pub struct ProcKeyEntry {
    pub id: i32,
    pub type_name: &'static str,
    pub description: String,
    pub uid: u32,
    pub gid: u32,
    pub perm: u32,
    pub state: &'static str,
    pub expiry: Option<u64>,
    pub payload_len: usize,
    pub nkeys: usize,
}

static SYSV_SHM_PROVIDER: Spinlock<Option<fn() -> Vec<ProcSysvShmEntry>>> = Spinlock::new(None);
static SYSV_SEM_PROVIDER: Spinlock<Option<fn() -> Vec<ProcSysvSemEntry>>> = Spinlock::new(None);
static SYSV_MSG_PROVIDER: Spinlock<Option<fn() -> Vec<ProcSysvMsgEntry>>> = Spinlock::new(None);
static KEYS_PROVIDER: Spinlock<Option<fn() -> Vec<ProcKeyEntry>>> = Spinlock::new(None);
static KEY_USERS_PROVIDER: Spinlock<Option<fn() -> Vec<(u32, usize, usize)>>> = Spinlock::new(None);

pub fn install_proc_net_route_provider(provider: fn() -> Vec<net::control::RouteEntry>) {
    *ROUTE_SNAPSHOT_PROVIDER.lock() = Some(provider);
}

pub fn install_proc_net_neighbor_provider(
    provider: fn() -> Vec<net::control::NeighborSnapshotEntry>,
) {
    *NEIGHBOR_SNAPSHOT_PROVIDER.lock() = Some(provider);
}

pub fn install_proc_net_dns_provider(provider: fn() -> Vec<net::IpAddr>) {
    *DNS_SNAPSHOT_PROVIDER.lock() = Some(provider);
}

pub fn install_proc_net_addr_provider(provider: fn() -> Vec<net::control::AddressEntry>) {
    *ADDR_SNAPSHOT_PROVIDER.lock() = Some(provider);
}

pub fn install_proc_sysvipc_shm_provider(provider: fn() -> Vec<ProcSysvShmEntry>) {
    *SYSV_SHM_PROVIDER.lock() = Some(provider);
}

pub fn install_proc_sysvipc_sem_provider(provider: fn() -> Vec<ProcSysvSemEntry>) {
    *SYSV_SEM_PROVIDER.lock() = Some(provider);
}

pub fn install_proc_sysvipc_msg_provider(provider: fn() -> Vec<ProcSysvMsgEntry>) {
    *SYSV_MSG_PROVIDER.lock() = Some(provider);
}

pub fn install_proc_keys_provider(provider: fn() -> Vec<ProcKeyEntry>) {
    *KEYS_PROVIDER.lock() = Some(provider);
}

pub fn install_proc_key_users_provider(provider: fn() -> Vec<(u32, usize, usize)>) {
    *KEY_USERS_PROVIDER.lock() = Some(provider);
}

const ROOT_INO: u64 = 1;
const FILESYSTEMS_INO: u64 = 2;
const MOUNTS_INO: u64 = 3;
const VERSION_INO: u64 = 4;
const CPUINFO_INO: u64 = 5;
const MEMINFO_INO: u64 = 6;
const UPTIME_INO: u64 = 7;
const STAT_INO: u64 = 8;
const DEVICES_INO: u64 = 9;
const SELF_INO: u64 = 10;
const THREAD_SELF_INO: u64 = 11;
const MOUNTINFO_INO: u64 = 12;
const SYS_INO: u64 = 13;
const SYS_KERNEL_INO: u64 = 14;
const SYS_HOTPLUG_INO: u64 = 15;
const NET_DIR_INO: u64 = 16;
const NET_DEV_INO: u64 = 17;
const PNP_INO: u64 = 18;
const DEVICE_FUNCTIONS_INO: u64 = 19;
const SYS_PID_MAX_INO: u64 = 20;
const INTERRUPTS_INO: u64 = 21;
const SYS_FS_INO: u64 = 22;
const SYS_FILE_MAX_INO: u64 = 23;
const SYS_SCHED_RT_PERIOD_INO: u64 = 24;
const SYS_SCHED_RT_RUNTIME_INO: u64 = 25;
const SYS_SCHED_RR_TIMESLICE_INO: u64 = 26;
const SYS_PIPE_MAX_SIZE_INO: u64 = 27;
const SYS_TAINTED_INO: u64 = 28;
const TASK_SNAPSHOT_INO: u64 = 29;
const SYS_VM_INO: u64 = 30;
const SWAPS_INO: u64 = 31;
/// /proc/sys/vm 参数文件的 inode 基址（每个参数一个）。
const SYS_VM_PARAM_BASE: u64 = 100;

const PROC_DYNAMIC_BASE: u64 = 1_000_000;
const PROC_FD_BASE: u64 = 10_000_000_000;
const PROC_NS_BACKING_BASE: u64 = 1 << 61;
const PROC_NS_LINK_BASE: u64 = 1 << 62;

const TASK_SLOT_DIR_PROCESS: u64 = 1;
const TASK_SLOT_DIR_THREAD: u64 = 2;
const TASK_SLOT_EXE: u64 = 3;
const TASK_SLOT_CWD: u64 = 4;
const TASK_SLOT_ROOT: u64 = 5;
const TASK_SLOT_STATUS: u64 = 6;
const TASK_SLOT_STAT: u64 = 7;
const TASK_SLOT_CMDLINE: u64 = 8;
const TASK_SLOT_ENVIRON: u64 = 9;
const TASK_SLOT_COMM: u64 = 10;
const TASK_SLOT_MAPS: u64 = 11;
const TASK_SLOT_FD_DIR: u64 = 12;
const TASK_SLOT_TASK_DIR: u64 = 13;
const TASK_SLOT_MOUNTINFO: u64 = 14;
const TASK_SLOT_MOUNTS: u64 = 15;
const TASK_SLOT_FDINFO_DIR: u64 = 16;
const TASK_SLOT_NS_DIR: u64 = 17;
// 新增顶层文件（loadavg 等）。
const LOADAVG_INO: u64 = 200;
const CMDLINE_INO: u64 = 201;
const PARTITIONS_INO: u64 = 202;
const DISKSTATS_INO: u64 = 203;
const KALLSYMS_INO: u64 = 204;
const VMSTAT_INO: u64 = 205;
const ZONEINFO_INO: u64 = 206;
const BUDDYINFO_INO: u64 = 207;
const IOMEM_INO: u64 = 208;
const SOFTIRQS_INO: u64 = 209;
const SYSV_IPC_DIR_INO: u64 = 210;
const SYSV_SHM_INO: u64 = 211;
const SYSV_SEM_INO: u64 = 212;
const SYSV_MSG_INO: u64 = 213;
const KEYS_INO: u64 = 214;
const KEY_USERS_INO: u64 = 215;
// /proc/sys/net 目录树与 /proc/sys 补充项。
const SYS_NET_DIR_INO: u64 = 300;
const SYS_NET_CORE_INO: u64 = 301;
const SYS_NET_IPV4_INO: u64 = 302;
const SYS_NET_IPV6_INO: u64 = 303;
/// procfs 自有补充 sysctl 文件的 inode 基址。
const SYS_EXTRA_SYSCTL_BASE: u64 = 400;
// 新增 per-pid 文件槽位（沿用 proc_task_base + slot 的布局）。
const TASK_SLOT_SMAPS: u64 = 18;
const TASK_SLOT_NUMA_MAPS: u64 = 19;
const TASK_SLOT_LIMITS: u64 = 20;
const TASK_SLOT_AUXV: u64 = 21;
const TASK_SLOT_IO: u64 = 22;
const TASK_SLOT_OOM_SCORE: u64 = 23;
const TASK_SLOT_OOM_SCORE_ADJ: u64 = 24;
const TASK_SLOT_OOM_ADJ: u64 = 25;
const TASK_SLOT_ATTR_DIR: u64 = 26;
const TASK_SLOT_SCHED: u64 = 27;
const TASK_SLOT_SYSCALL: u64 = 28;
const TASK_SLOT_STACK: u64 = 29;
const TASK_SLOT_CGROUP: u64 = 30;
const TASK_SLOT_CLEAR_REFS: u64 = 31;
const TASK_SLOT_PAGEMAP: u64 = 32;
const TASK_SLOT_SECCOMP: u64 = 33;
const TASK_SLOT_TIMERS: u64 = 34;
const TASK_SLOT_LOGINUID: u64 = 35;
const TASK_SLOT_SESSIONID: u64 = 36;
const TASK_SLOT_UID_MAP: u64 = 37;
const TASK_SLOT_GID_MAP: u64 = 38;
const TASK_SLOT_MEM: u64 = 39;

fn procfs_fallible_string(value: &str) -> VfsResult<String> {
    let mut out = String::new();
    out.try_reserve(value.len())
        .map_err(|_| VfsError::NoSpace)?;
    out.push_str(value);
    Ok(out)
}

fn procfs_decimal_name(value: impl core::fmt::Display) -> VfsResult<String> {
    let mut out = String::new();
    // u64/i64 十进制文本最多 20 字节左右；预留固定上界，避免 write! 过程中
    // 通过 String 自动扩容导致 procfs 目录快照 panic。
    out.try_reserve(20).map_err(|_| VfsError::NoSpace)?;
    write!(&mut out, "{value}").map_err(|_| VfsError::NoSpace)?;
    Ok(out)
}

fn procfs_fallible_smallstr(value: &str) -> VfsResult<SmallStr> {
    let bytes = value.as_bytes();
    if bytes.len() <= 23 {
        let mut buf = [0u8; 23];
        buf[..bytes.len()].copy_from_slice(bytes);
        return Ok(SmallStr::Inline {
            len: bytes.len() as u8,
            buf,
        });
    }
    Ok(SmallStr::Heap(procfs_fallible_string(value)?))
}

fn push_proc_dir_entry(
    out: &mut Vec<DirEntry>,
    ino: u64,
    name: &str,
    kind: FileType,
) -> VfsResult<()> {
    out.try_reserve(1).map_err(|_| VfsError::NoSpace)?;
    out.push(DirEntry {
        ino,
        name: procfs_fallible_smallstr(name)?,
        kind,
    });
    Ok(())
}

pub struct ProcFsDriver;

impl FsDriver for ProcFsDriver {
    fn name(&self) -> &'static str {
        "proc"
    }
    fn flags(&self) -> FsDriverFlags {
        FsDriverFlags::NODEV.with(FsDriverFlags::SINGLE)
    }

    fn mount(&self, _dev: Option<&str>, _data: &str) -> VfsResult<Arc<Superblock>> {
        let fs_id = FsId::new(PROCFS_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));
        Ok(Superblock::new(|weak_sb| {
            let now = Timespec::now();
            let root_inode = root_inode(fs_id, &weak_sb, now);
            let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));
            Superblock {
                fs_type: "proc",
                fs_id,
                dev_id: None,
                block_size: 4096,
                name_max: 255,
                root_inode,
                root_dentry,
                inode_cache: vfs::superblock::InodeCache::new(),
                ops: Box::new(ProcSuperblockOps),
                self_weak: weak_sb,
            }
        }))
    }

    fn kill_sb(&self, _sb: Arc<Superblock>) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcSuperblockOps;
impl SuperblockOps for ProcSuperblockOps {
    fn alloc_inode(&self, _: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::ReadOnlyFilesystem)
    }
    fn write_inode(&self, _: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }
    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Ok(FsStat {
            fs_type: 0x9fa0,
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

#[derive(Clone, Copy)]
enum RootFileKind {
    Filesystems,
    Mounts,
    Mountinfo,
    Version,
    CpuInfo,
    MemInfo,
    Swaps,
    Uptime,
    Stat,
    Interrupts,
    Devices,
    Pnp,
    DeviceFunctions,
    TaskSnapshot,
    Loadavg,
    Cmdline,
    Partitions,
    Diskstats,
    Kallsyms,
    Vmstat,
    Zoneinfo,
    Buddyinfo,
    Iomem,
    Softirqs,
    SysvipcShm,
    SysvipcSem,
    SysvipcMsg,
    Keys,
    KeyUsers,
}

#[derive(Clone, Copy)]
enum TaskFileKind {
    Status,
    Stat,
    Cmdline,
    Environ,
    Comm,
    Maps,
    Mountinfo,
    Mounts,
    Smaps,
    NumaMaps,
    Limits,
    Auxv,
    Io,
    OomScore,
    OomScoreAdj,
    OomAdj,
    Sched,
    Syscall,
    Stack,
    Cgroup,
    ClearRefs,
    Pagemap,
    Seccomp,
    Timers,
    Loginuid,
    Sessionid,
    UidMap,
    GidMap,
    Mem,
}

#[derive(Clone, Copy)]
enum ProcFileKind {
    Root(RootFileKind),
    Task {
        pid: PidT,
        kind: TaskFileKind,
    },
    SysHotplug,
    SysPidMax,
    SysFileMax,
    SysSchedRtPeriod,
    SysSchedRtRuntime,
    SysSchedRrTimeslice,
    SysPipeMaxSize,
    SysTainted,
    SysVm(crate::mm::memstat::VmParam),
    /// procfs 自有补充 sysctl（`/proc/sys/{kernel,fs,vm,net}` 常见项）。
    SysExtra(&'static str),
}

/// 返回当前内核故障污染位图。
///
/// 位值由故障来源自行定义；procfs 只负责提供稳定的十进制诊断视图。
pub fn kernel_taint_flags() -> u64 {
    KERNEL_TAINT_FLAGS.load(Ordering::Acquire)
}

/// 原子地记录内核故障污染标志，并返回更新后的完整位图。
///
/// 污染标志只能累加，不能在运行中清除，确保测试和诊断工具不会丢失已经发生的故障。
pub fn mark_kernel_tainted(flags: u64) -> u64 {
    KERNEL_TAINT_FLAGS.fetch_or(flags, Ordering::AcqRel) | flags
}

#[derive(Clone, Copy)]
enum TaskLinkKind {
    Exe,
    Cwd,
    Root,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskDirView {
    Process,
    Thread,
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
        inode_meta(mode, nlink, Timespec::now()),
        ops,
        weak_sb.clone(),
    )
}

fn root_inode(fs_id: FsId, weak_sb: &Weak<Superblock>, now: Timespec) -> Arc<Inode> {
    let mk_root_file = |ino: u64, kind: RootFileKind| {
        mk_inode(
            fs_id,
            weak_sb,
            ino,
            FileType::Regular,
            0o444,
            1,
            Arc::new(ProcRegularInodeOps {
                kind: ProcFileKind::Root(kind),
            }),
        )
    };
    let self_inode = mk_inode(
        fs_id,
        weak_sb,
        SELF_INO,
        FileType::Symlink,
        0o777,
        1,
        Arc::new(ProcSelfOps),
    );
    let thread_self_inode = mk_inode(
        fs_id,
        weak_sb,
        THREAD_SELF_INO,
        FileType::Symlink,
        0o777,
        1,
        Arc::new(ProcThreadSelfOps),
    );
    let sys_inode = mk_inode(
        fs_id,
        weak_sb,
        SYS_INO,
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcSysDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
        }),
    );
    let static_entries: Vec<(&str, Arc<Inode>)> = vec![
        (
            "filesystems",
            mk_root_file(FILESYSTEMS_INO, RootFileKind::Filesystems),
        ),
        ("mounts", mk_root_file(MOUNTS_INO, RootFileKind::Mounts)),
        (
            "mountinfo",
            mk_root_file(MOUNTINFO_INO, RootFileKind::Mountinfo),
        ),
        ("version", mk_root_file(VERSION_INO, RootFileKind::Version)),
        ("cpuinfo", mk_root_file(CPUINFO_INO, RootFileKind::CpuInfo)),
        ("meminfo", mk_root_file(MEMINFO_INO, RootFileKind::MemInfo)),
        ("swaps", mk_root_file(SWAPS_INO, RootFileKind::Swaps)),
        ("uptime", mk_root_file(UPTIME_INO, RootFileKind::Uptime)),
        ("stat", mk_root_file(STAT_INO, RootFileKind::Stat)),
        (
            "interrupts",
            mk_root_file(INTERRUPTS_INO, RootFileKind::Interrupts),
        ),
        ("devices", mk_root_file(DEVICES_INO, RootFileKind::Devices)),
        ("pnp", mk_root_file(PNP_INO, RootFileKind::Pnp)),
        (
            "device-functions",
            mk_root_file(DEVICE_FUNCTIONS_INO, RootFileKind::DeviceFunctions),
        ),
        (
            "task-snapshot",
            mk_inode(
                fs_id,
                weak_sb,
                TASK_SNAPSHOT_INO,
                FileType::Regular,
                0o400,
                1,
                Arc::new(ProcRegularInodeOps {
                    kind: ProcFileKind::Root(RootFileKind::TaskSnapshot),
                }),
            ),
        ),
        ("self", self_inode),
        ("thread-self", thread_self_inode),
        ("sys", sys_inode),
        (
            "net",
            mk_inode(
                fs_id,
                weak_sb,
                NET_DIR_INO,
                FileType::Directory,
                0o555,
                2,
                Arc::new(ProcNetDirOps {
                    fs_id,
                    weak_sb: weak_sb.clone(),
                }),
            ),
        ),
        ("loadavg", mk_root_file(LOADAVG_INO, RootFileKind::Loadavg)),
        ("cmdline", mk_root_file(CMDLINE_INO, RootFileKind::Cmdline)),
        (
            "partitions",
            mk_root_file(PARTITIONS_INO, RootFileKind::Partitions),
        ),
        (
            "diskstats",
            mk_root_file(DISKSTATS_INO, RootFileKind::Diskstats),
        ),
        (
            "kallsyms",
            mk_root_file(KALLSYMS_INO, RootFileKind::Kallsyms),
        ),
        ("vmstat", mk_root_file(VMSTAT_INO, RootFileKind::Vmstat)),
        (
            "zoneinfo",
            mk_root_file(ZONEINFO_INO, RootFileKind::Zoneinfo),
        ),
        (
            "buddyinfo",
            mk_root_file(BUDDYINFO_INO, RootFileKind::Buddyinfo),
        ),
        ("iomem", mk_root_file(IOMEM_INO, RootFileKind::Iomem)),
        (
            "softirqs",
            mk_root_file(SOFTIRQS_INO, RootFileKind::Softirqs),
        ),
        (
            "sysvipc",
            mk_inode(
                fs_id,
                weak_sb,
                SYSV_IPC_DIR_INO,
                FileType::Directory,
                0o555,
                2,
                Arc::new(ProcSysvipcDirOps {
                    fs_id,
                    weak_sb: weak_sb.clone(),
                }),
            ),
        ),
        ("keys", mk_root_file(KEYS_INO, RootFileKind::Keys)),
        (
            "key-users",
            mk_root_file(KEY_USERS_INO, RootFileKind::KeyUsers),
        ),
    ];
    Inode::new(
        InodeId {
            fs_id,
            ino: ROOT_INO,
        },
        FileType::Directory,
        DevId::new(0, 0),
        4096,
        None,
        inode_meta(0o555, 2, now),
        Arc::new(ProcRootOps {
            entries: static_entries,
            fs_id,
            weak_sb: weak_sb.clone(),
        }),
        weak_sb.clone(),
    )
}

struct ProcRootOps {
    entries: Vec<(&'static str, Arc<Inode>)>,
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
}

impl InodeOps for ProcRootOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        if let Some((_, inode)) = self.entries.iter().find(|(n, _)| *n == name) {
            return Ok(Arc::clone(inode));
        }
        let pid = parse_pid_component(name).ok_or(VfsError::NotFound)?;
        let task = lookup_task(pid).ok_or(VfsError::NotFound)?;
        let view = if task_is_group_leader(&task) {
            TaskDirView::Process
        } else {
            TaskDirView::Thread
        };
        Ok(proc_task_dir_inode(self.fs_id, &self.weak_sb, pid, view))
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve(self.entries.len())
            .map_err(|_| VfsError::NoSpace)?;
        for (name, inode) in &self.entries {
            push_proc_dir_entry(&mut snapshot, inode.ino(), name, inode.kind())?;
        }
        for pid in snapshot_root_processes() {
            let name = procfs_decimal_name(pid)?;
            push_proc_dir_entry(
                &mut snapshot,
                proc_task_dir_ino(pid, TaskDirView::Process),
                &name,
                FileType::Directory,
            )?;
        }
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcSelfOps;
impl InodeOps for ProcSelfOps {
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
        let (tgid, _) = current_tgid_tid()?;
        Ok(format!("{}", tgid))
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcThreadSelfOps;
impl InodeOps for ProcThreadSelfOps {
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
        let (tgid, tid) = current_tgid_tid()?;
        Ok(format!("{}/task/{}", tgid, tid))
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcSysDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
}

// ── /proc/net 目录 ────────────────────────────────────────────────────────────

struct ProcNetDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
}

impl InodeOps for ProcNetDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        if name == "dev" {
            return Ok(mk_inode(
                self.fs_id,
                &self.weak_sb,
                NET_DEV_INO,
                FileType::Regular,
                0o444,
                1,
                Arc::new(ProcNetDevOps),
            ));
        }

        let Some(kind) = ProcNetSnapshotKind::from_name(name) else {
            return Err(VfsError::NotFound);
        };
        Ok(mk_inode(
            self.fs_id,
            &self.weak_sb,
            kind.ino(),
            FileType::Regular,
            0o444,
            1,
            Arc::new(ProcNetSnapshotOps { kind }),
        ))
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let mut snapshot = vec![DirEntry {
            ino: NET_DEV_INO,
            name: SmallStr::new("dev"),
            kind: FileType::Regular,
        }];
        for kind in ProcNetSnapshotKind::ALL {
            snapshot.push(DirEntry {
                ino: kind.ino(),
                name: SmallStr::new(kind.name()),
                kind: FileType::Regular,
            });
        }
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// `/proc/net` 中由内核快照动态渲染的兼容文件。
///
/// 文件名和 inode 偏移集中在这里维护，避免 lookup 与 readdir 各自重复维护一份
/// 列表。渲染逻辑只读取 `net`/`socket` 层公开快照，不把 procfs 的文本 ABI
/// 反向泄入底层设备抽象。
#[derive(Clone, Copy)]
enum ProcNetSnapshotKind {
    Tcp,
    Udp,
    Tcp6,
    Udp6,
    Raw,
    Icmp,
    Snmp,
    IfInet6,
    Route,
    Unix,
    Arp,
    Sockstat,
    Dns,
}

impl ProcNetSnapshotKind {
    const ALL: [Self; 13] = [
        Self::Tcp,
        Self::Udp,
        Self::Tcp6,
        Self::Udp6,
        Self::Raw,
        Self::Icmp,
        Self::Snmp,
        Self::IfInet6,
        Self::Route,
        Self::Unix,
        Self::Arp,
        Self::Sockstat,
        Self::Dns,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Tcp6 => "tcp6",
            Self::Udp6 => "udp6",
            Self::Raw => "raw",
            Self::Icmp => "icmp",
            Self::Snmp => "snmp",
            Self::IfInet6 => "if_inet6",
            Self::Route => "route",
            Self::Unix => "unix",
            Self::Arp => "arp",
            Self::Sockstat => "sockstat",
            Self::Dns => "dns",
        }
    }

    const fn ino(self) -> u64 {
        NET_DEV_INO
            + match self {
                Self::Tcp => 1,
                Self::Udp => 2,
                Self::Route => 3,
                Self::Unix => 4,
                Self::Arp => 5,
                Self::Sockstat => 6,
                Self::Dns => 7,
                Self::Tcp6 => 8,
                Self::Udp6 => 9,
                Self::Raw => 10,
                Self::Icmp => 11,
                Self::Snmp => 12,
                Self::IfInet6 => 13,
            }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.name() == name)
    }

    fn render(self) -> String {
        match self {
            Self::Tcp => render_proc_net_tcp(),
            Self::Udp => render_proc_net_udp(),
            Self::Tcp6 => render_proc_net_tcp6(),
            Self::Udp6 => render_proc_net_udp6(),
            Self::Raw => render_proc_net_raw(),
            Self::Icmp => render_proc_net_icmp(),
            Self::Snmp => render_proc_net_snmp(),
            Self::IfInet6 => render_proc_net_if_inet6(),
            Self::Route => render_proc_net_route(),
            Self::Unix => render_proc_net_unix(),
            Self::Arp => render_proc_net_arp(),
            Self::Sockstat => render_proc_net_sockstat(),
            Self::Dns => render_proc_net_dns(),
        }
    }
}

struct ProcNetSnapshotOps {
    kind: ProcNetSnapshotKind,
}

impl InodeOps for ProcNetSnapshotOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }
    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(ProcNetSnapshotFile { kind: self.kind }))
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcNetSnapshotFile {
    kind: ProcNetSnapshotKind,
}

impl FileOps for ProcNetSnapshotFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let content = self.kind.render();
        let offset = offset as usize;
        if offset >= content.len() {
            return Ok(0);
        }
        let len = buf.len().min(content.len() - offset);
        buf[..len].copy_from_slice(&content.as_bytes()[offset..offset + len]);
        Ok(len)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::PermissionDenied)
    }
    fn readdir(&self, _: u64, _: &mut dyn FnMut(DirEntry) -> ControlFlow<()>) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, interest: PollEvents) -> PollEvents {
        // 快照型伪文件每次读都会重新生成内容，不需要等待异步事件。
        PollEvents::READ_WRITE_READY.intersect(interest)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// IPv4 端点按 Linux /proc/net 格式渲染：地址小端 hex + 端口 hex。
fn proc_ipv4_endpoint(address: net::Ipv4Addr, port: u16) -> alloc::string::String {
    let mut raw = String::new();
    let _ = alloc::fmt::write(
        &mut raw,
        format_args!(
            "{:02X}{:02X}{:02X}{:02X}:{:04X}",
            address.0[3], address.0[2], address.0[1], address.0[0], port
        ),
    );
    raw
}

fn proc_ipv6_endpoint(address: net::Ipv6Addr, port: u16) -> alloc::string::String {
    // Linux 用 32 位小端字序的 4 组 hex。
    let mut raw = String::new();
    for chunk in address.0.chunks_exact(4) {
        let word = u32::from_le_bytes(chunk.try_into().unwrap());
        let _ = alloc::fmt::write(&mut raw, format_args!("{:08X}", word));
    }
    let _ = alloc::fmt::write(&mut raw, format_args!(":{:04X}", port));
    raw
}

fn proc_endpoint(endpoint: net::Endpoint) -> alloc::string::String {
    match endpoint.addr {
        net::IpAddr::V4(address) => proc_ipv4_endpoint(address, endpoint.port),
        net::IpAddr::V6(address) => proc_ipv6_endpoint(address, endpoint.port),
    }
}

/// TCP 状态码（Linux /proc/net/tcp st 字段）。
fn proc_tcp_state_code(state: u8) -> u8 {
    match state {
        1 => 0x01,  // ESTABLISHED
        2 => 0x02,  // SYN_SENT
        3 => 0x03,  // SYN_RECV
        4 => 0x04,  // FIN_WAIT1
        5 => 0x05,  // FIN_WAIT2
        6 => 0x06,  // TIME_WAIT
        7 => 0x07,  // CLOSE
        8 => 0x08,  // CLOSE_WAIT
        9 => 0x09,  // LAST_ACK
        10 => 0x0a, // LISTEN
        11 => 0x0b, // CLOSING
        _ => 0x07,
    }
}

fn render_proc_net_tcp_lines(
    sockets: &[net::InetSocketSnapshot],
    family: net::AddressFamily,
) -> alloc::string::String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode"
    );
    for (index, socket) in sockets.iter().enumerate() {
        if socket.kind != net::SocketKind::Stream || socket.family != family {
            continue;
        }
        let local = socket
            .local
            .map(proc_endpoint)
            .unwrap_or_else(|| "00000000:0000".into());
        let peer = socket
            .peer
            .map(proc_endpoint)
            .unwrap_or_else(|| "00000000:0000".into());
        let state = proc_tcp_state_code(socket.tcp_state);
        let inode = socket.id.counter;
        let _ = writeln!(
            out,
            "{:5}: {:<23} {:<23} {:02X} {:08X}:{:08X} 00:00000000 {:08X}     0        0 {}",
            format_args!("{:X}", index),
            local,
            peer,
            state,
            0u32,
            0u32,
            0u32,
            inode,
        );
    }
    out
}

fn render_proc_net_udp_lines(
    sockets: &[net::InetSocketSnapshot],
    family: net::AddressFamily,
) -> alloc::string::String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode"
    );
    for (index, socket) in sockets.iter().enumerate() {
        if socket.kind != net::SocketKind::Datagram || socket.family != family {
            continue;
        }
        let local = socket
            .local
            .map(proc_endpoint)
            .unwrap_or_else(|| "00000000:0000".into());
        let peer = socket
            .peer
            .map(proc_endpoint)
            .unwrap_or_else(|| "00000000:0000".into());
        let inode = socket.id.counter;
        let _ = writeln!(
            out,
            "{:5}: {:<23} {:<23} 07 {:08X}:{:08X} 00:00000000 {:08X}     0        0 {}",
            format_args!("{:X}", index),
            local,
            peer,
            0u32,
            0u32,
            0u32,
            inode,
        );
    }
    out
}

fn render_proc_net_raw_lines(
    sockets: &[net::InetSocketSnapshot],
    family: net::AddressFamily,
) -> alloc::string::String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode"
    );
    for (index, socket) in sockets.iter().enumerate() {
        if socket.kind != net::SocketKind::Raw || socket.family != family {
            continue;
        }
        let local = socket
            .local
            .map(proc_endpoint)
            .unwrap_or_else(|| "00000000:0000".into());
        let peer = socket
            .peer
            .map(proc_endpoint)
            .unwrap_or_else(|| "00000000:0000".into());
        let inode = socket.id.counter;
        let _ = writeln!(
            out,
            "{:5}: {:<23} {:<23} 00 {:08X}:{:08X} 00:00000000 {:08X}     0        0 {}",
            format_args!("{:X}", index),
            local,
            peer,
            0u32,
            0u32,
            0u32,
            inode,
        );
    }
    out
}

fn render_proc_net_route() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT"
    );
    let routes = ROUTE_SNAPSHOT_PROVIDER
        .lock()
        .map(|provider| provider())
        .unwrap_or_default();
    for route in routes {
        let (destination, gateway, mask) = match route.network {
            net::IpAddr::V4(network) => {
                let mask = if route.prefix_len == 0 {
                    0u32
                } else {
                    u32::MAX << (32 - route.prefix_len)
                };
                let gateway = route.gateway.map(|gw| match gw {
                    net::IpAddr::V4(address) => u32::from_be_bytes(address.0),
                    _ => 0,
                });
                (network, gateway, mask)
            }
            // /proc/net/route 只覆盖 IPv4（Linux 语义）。
            net::IpAddr::V6(_) => continue,
        };
        let iface = proc_route_iface_name(route.interface);
        let mut flags = 1u32; // RTF_UP
        if route.gateway.is_some() {
            flags |= 2; // RTF_GATEWAY
        }
        let destination_raw = u32::from_be_bytes(destination.0);
        let gateway_raw = gateway.unwrap_or(0);
        let _ = writeln!(
            out,
            "{}\t{:08X}\t{:08X}\t{:04X}\t0\t0\t{}\t{:08X}\t0\t0\t0",
            iface, destination_raw, gateway_raw, flags, route.metric, mask,
        );
    }
    out
}

fn proc_route_iface_name(interface: net::InterfaceId) -> alloc::string::String {
    net::device::snapshot_devices()
        .into_iter()
        .find(|device| device.id.raw() == interface.0)
        .map(|device| device.name.as_ref().to_string())
        .unwrap_or_else(|| format!("if{}", interface.0))
}

fn render_proc_net_tcp() -> String {
    render_proc_net_tcp_lines(&net::snapshot_inet_sockets(), net::AddressFamily::Ipv4)
}

fn render_proc_net_udp() -> String {
    render_proc_net_udp_lines(&net::snapshot_inet_sockets(), net::AddressFamily::Ipv4)
}

fn render_proc_net_tcp6() -> String {
    render_proc_net_tcp_lines(&net::snapshot_inet_sockets(), net::AddressFamily::Ipv6)
}

fn render_proc_net_udp6() -> String {
    render_proc_net_udp_lines(&net::snapshot_inet_sockets(), net::AddressFamily::Ipv6)
}

fn render_proc_net_raw() -> String {
    render_proc_net_raw_lines(&net::snapshot_inet_sockets(), net::AddressFamily::Ipv4)
}

fn render_proc_net_icmp() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "       InMsgs InErrors InDestUnreachs InTimeExcds InParmProbs InSrcQuenchs InRedirects InEchos InEchoReps InTimestamps InTimestampReps InAddrMasks InAddrMaskReps"
    );
    let _ = writeln!(out, "Icmp: 0 0 0 0 0 0 0 0 0 0 0 0 0");
    let _ = writeln!(
        out,
        "OutMsgs OutErrors OutDestUnreachs OutTimeExcds OutParmProbs OutSrcQuenchs OutRedirects OutEchos OutEchoReps OutTimestamps OutTimestampReps OutAddrMasks OutAddrMaskReps"
    );
    let _ = writeln!(out, "Icmp: 0 0 0 0 0 0 0 0 0 0 0 0 0");
    out
}

fn render_proc_net_snmp() -> String {
    use alloc::fmt::Write;
    // 无 SNMP MIB 计数器；输出 Linux 兼容的各协议节（全 0）。
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Ip: Forwarding DefaultTTL InReceives InHdrErrors InAddrErrors ForwDatagrams InUnknownProtos InDiscards InDelivers OutRequests OutDiscards OutNoRoutes ReasmTimeout ReasmReqds ReasmOKs ReasmFails FragOKs FragFails FragCreates"
    );
    let _ = writeln!(out, "Ip: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0");
    let _ = writeln!(
        out,
        "Icmp: InMsgs InErrors InDestUnreachs InTimeExcds InParmProbs InSrcQuenchs InRedirects InEchos InEchoReps InTimestamps InTimestampReps InAddrMasks InAddrMaskReps OutMsgs OutErrors OutDestUnreachs OutTimeExcds OutParmProbs OutSrcQuenchs OutRedirects OutEchos OutEchoReps OutTimestamps OutTimestampReps OutAddrMasks OutAddrMaskReps"
    );
    let _ = writeln!(
        out,
        "Icmp: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"
    );
    let _ = writeln!(
        out,
        "Tcp: RtoAlgorithm RtoMin RtoMax MaxConn ActiveOpens PassiveOpens AttemptFails EstabResets CurrEstab InSegs OutSegs RetransSegs InErrs OutRsts InCsumErrors"
    );
    let _ = writeln!(out, "Tcp: 1 200 120000 -1 0 0 0 0 0 0 0 0 0 0 0");
    let _ = writeln!(
        out,
        "Udp: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors InCsumErrors IgnoredMulti"
    );
    let _ = writeln!(out, "Udp: 0 0 0 0 0 0 0 0");
    let _ = writeln!(
        out,
        "UdpLite: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors InCsumErrors IgnoredMulti"
    );
    let _ = writeln!(out, "UdpLite: 0 0 0 0 0 0 0 0");
    out
}

fn render_proc_net_if_inet6() -> String {
    use alloc::fmt::Write;
    let addresses = ADDR_SNAPSHOT_PROVIDER
        .lock()
        .map(|provider| provider())
        .unwrap_or_default();
    let mut out = String::new();
    for entry in addresses {
        let net::IpAddr::V6(address) = entry.address else {
            continue;
        };
        // if_inet6: 32-hex 地址 + ifindex(hex) + prefix_len + scope + flags。
        let mut text = String::new();
        for chunk in address.0.chunks_exact(4) {
            let word = u32::from_be_bytes(chunk.try_into().unwrap());
            let _ = alloc::fmt::write(&mut text, format_args!("{:08x}", word));
        }
        let scope = if address.0[0] == 0xfe && (address.0[1] & 0xc0) == 0x80 {
            0x20 // link-local
        } else {
            0x00 // global
        };
        let _ = writeln!(
            out,
            "{text} {:02x} {:02x} {:02x} {:02x}",
            entry.interface.0,
            entry.prefix_len,
            scope,
            if entry.primary { 0x80 } else { 0x00 },
        );
    }
    out
}

fn render_proc_net_unix() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Num       RefCount Protocol Flags    Type St Inode Path"
    );
    let sockets = socket::snapshot_sockets();
    for s in &sockets {
        let (typ, state) = unix_socket_info(s);
        let path = match s.local_address() {
            socket::UnixAddress::Path { display, .. } => {
                core::str::from_utf8(&display).unwrap_or("").to_string()
            }
            socket::UnixAddress::Abstract(name) => {
                let mut s = String::with_capacity(name.len() + 1);
                s.push('@');
                if let Ok(text) = core::str::from_utf8(&name) {
                    s.push_str(text);
                }
                s
            }
            _ => String::new(),
        };
        let _ = writeln!(
            out,
            "{:016X}: {:08X} {:08X} {:08X} {:04X} {:02X} {:>8} {}",
            s.id(),
            2u32, // RefCount (至少 1: fd 引用 + snapshot 临时引用)
            0u32, // Protocol
            0u32, // Flags
            typ,
            state,
            s.id(),
            path,
        );
    }
    out
}

fn unix_socket_info(s: &socket::Socket) -> (u16, u8) {
    let typ = match s.socket_type() {
        socket::SocketType::Stream => 1u16,
        socket::SocketType::Datagram => 2u16,
        socket::SocketType::Sequenced => 5u16,
        _ => 0u16,
    };
    // 近似状态：通过 socket 是否可读写判断
    let readiness = s.readiness();
    let state = if readiness.bits() == 0 { 1u8 } else { 3u8 }; // SS_UNCONNECTED or SS_CONNECTED
    (typ, state)
}

fn render_proc_net_arp() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "IP address       HW type     Flags       HW address            Mask     Device"
    );
    let neighbors = NEIGHBOR_SNAPSHOT_PROVIDER
        .lock()
        .map(|provider| provider())
        .unwrap_or_default();
    for neighbor in neighbors {
        let (address, _) = match neighbor.address {
            net::IpAddr::V4(address) => (address, 0u32),
            net::IpAddr::V6(_) => continue, // /proc/net/arp 只覆盖 IPv4（Linux 语义）
        };
        let iface = proc_route_iface_name(neighbor.interface);
        // ATF_COM=0x2 表示解析完成（镜像表只保存已解析条目）。
        let flags = 0x2u16;
        let hw = format!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            neighbor.mac[0],
            neighbor.mac[1],
            neighbor.mac[2],
            neighbor.mac[3],
            neighbor.mac[4],
            neighbor.mac[5]
        );
        let address_text = format!(
            "{}.{}.{}.{}",
            address.0[0], address.0[1], address.0[2], address.0[3]
        );
        let _ = writeln!(
            out,
            "{:<16} 0x1         {:<12} {:<19} *        {}",
            address_text,
            format_args!("0x{:x}", flags),
            hw,
            iface,
        );
    }
    out
}

fn render_proc_net_sockstat() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let sockets = net::snapshot_inet_sockets();
    let tcp_total = sockets
        .iter()
        .filter(|socket| socket.kind == net::SocketKind::Stream)
        .count();
    let udp_total = sockets
        .iter()
        .filter(|socket| socket.kind == net::SocketKind::Datagram)
        .count();
    let raw_total = sockets
        .iter()
        .filter(|socket| socket.kind == net::SocketKind::Raw)
        .count();
    let tcp6_total = sockets
        .iter()
        .filter(|s| s.kind == net::SocketKind::Stream && s.family == net::AddressFamily::Ipv6)
        .count();
    let udp6_total = sockets
        .iter()
        .filter(|s| s.kind == net::SocketKind::Datagram && s.family == net::AddressFamily::Ipv6)
        .count();
    let raw6_total = sockets
        .iter()
        .filter(|s| s.kind == net::SocketKind::Raw && s.family == net::AddressFamily::Ipv6)
        .count();
    let unix_total = socket::snapshot_sockets().len();
    let total = tcp_total + udp_total + raw_total + unix_total;
    let _ = writeln!(out, "sockets: used {}", total);
    let _ = writeln!(
        out,
        "TCP: inuse {} orphan 0 tw 0 alloc {} mem 0",
        tcp_total, tcp_total
    );
    let _ = writeln!(out, "UDP: inuse {} mem 0", udp_total);
    let _ = writeln!(out, "UDPLITE: inuse 0");
    let _ = writeln!(out, "RAW: inuse {}", raw_total);
    let _ = writeln!(out, "FRAG: inuse 0 memory 0");
    let _ = writeln!(out, "TCP6: inuse {}", tcp6_total);
    let _ = writeln!(out, "UDP6: inuse {}", udp6_total);
    let _ = writeln!(out, "RAW6: inuse {}", raw6_total);
    let _ = writeln!(out, "FRAG6: inuse 0 memory 0");
    out
}

fn render_proc_net_dns() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let servers = DNS_SNAPSHOT_PROVIDER
        .lock()
        .map(|provider| provider())
        .unwrap_or_default();
    for server in servers {
        let text = match server {
            net::IpAddr::V4(address) => format!(
                "{}.{}.{}.{}",
                address.0[0], address.0[1], address.0[2], address.0[3]
            ),
            net::IpAddr::V6(address) => {
                let mut groups = alloc::string::String::new();
                for chunk in address.0.chunks_exact(2) {
                    if !groups.is_empty() {
                        groups.push(':');
                    }
                    let _ = alloc::fmt::write(
                        &mut groups,
                        format_args!("{:02x}{:02x}", chunk[0], chunk[1]),
                    );
                }
                groups
            }
        };
        let _ = writeln!(out, "{}", text);
    }
    out
}

struct ProcNetDevOps;

impl InodeOps for ProcNetDevOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }
    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(ProcNetDevFile))
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcNetDevFile;

impl FileOps for ProcNetDevFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let content = render_proc_net_dev();
        let offset = offset as usize;
        if offset >= content.len() {
            return Ok(0);
        }
        let len = buf.len().min(content.len() - offset);
        buf[..len].copy_from_slice(&content.as_bytes()[offset..offset + len]);
        Ok(len)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::PermissionDenied)
    }
    fn readdir(&self, _: u64, _: &mut dyn FnMut(DirEntry) -> ControlFlow<()>) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, interest: PollEvents) -> PollEvents {
        // 快照型伪文件每次读都会重新生成内容，不需要等待异步事件。
        PollEvents::READ_WRITE_READY.intersect(interest)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn render_proc_net_dev() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Inter-|   Receive                                                |  Transmit"
    );
    let _ = writeln!(
        out,
        " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed"
    );
    for iface in net::device::snapshot_devices() {
        let s = iface.stats;
        let _ = writeln!(
            out,
            "{:>6}:{:>8} {:>7} {:>4} {:>4} {:>4} {:>5} {:>10} {:>9} {:>8} {:>7} {:>4} {:>4} {:>4} {:>5} {:>7} {:>10}",
            iface.name,
            s.rx_bytes,
            s.rx_packets,
            s.rx_errors,
            s.rx_dropped,
            0,
            0,
            0,
            0,
            s.tx_bytes,
            s.tx_packets,
            s.tx_errors,
            s.tx_dropped,
            0,
            0,
            0,
            0
        );
    }
    out
}

// ── /proc/sys 目录 ────────────────────────────────────────────────────────────

impl InodeOps for ProcSysDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        match name {
            "kernel" => Ok(proc_sys_kernel_dir_inode(self.fs_id, &self.weak_sb)),
            "fs" => Ok(proc_sys_fs_dir_inode(self.fs_id, &self.weak_sb)),
            "vm" => Ok(proc_sys_vm_dir_inode(self.fs_id, &self.weak_sb)),
            "net" => Ok(proc_sys_net_dir_inode(self.fs_id, &self.weak_sb)),
            _ => Err(VfsError::NotFound),
        }
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(ProcDirFile {
            snapshot: vec![
                DirEntry {
                    ino: SYS_KERNEL_INO,
                    name: SmallStr::new("kernel"),
                    kind: FileType::Directory,
                },
                DirEntry {
                    ino: SYS_FS_INO,
                    name: SmallStr::new("fs"),
                    kind: FileType::Directory,
                },
                DirEntry {
                    ino: SYS_VM_INO,
                    name: SmallStr::new("vm"),
                    kind: FileType::Directory,
                },
                DirEntry {
                    ino: SYS_NET_DIR_INO,
                    name: SmallStr::new("net"),
                    kind: FileType::Directory,
                },
            ],
        }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn proc_sys_fs_dir_inode(fs_id: FsId, weak_sb: &Weak<Superblock>) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        SYS_FS_INO,
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcSysFsDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
        }),
    )
}

struct ProcSysFsDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
}

impl InodeOps for ProcSysFsDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        match name {
            "file-max" => Ok(proc_sys_file_max_inode(self.fs_id, &self.weak_sb)),
            "pipe-max-size" => Ok(proc_sys_pipe_max_size_inode(self.fs_id, &self.weak_sb)),
            _ => {
                if let Some(entry) = FS_EXTRA_SYSCTLS.iter().copied().find(|e| *e == name) {
                    Ok(proc_sys_extra_inode(self.fs_id, &self.weak_sb, entry))
                } else {
                    Err(VfsError::NotFound)
                }
            }
        }
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let mut snapshot = vec![
            DirEntry {
                ino: SYS_FILE_MAX_INO,
                name: SmallStr::new("file-max"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: SYS_PIPE_MAX_SIZE_INO,
                name: SmallStr::new("pipe-max-size"),
                kind: FileType::Regular,
            },
        ];
        push_extra_sysctl_entries(&mut snapshot, FS_EXTRA_SYSCTLS)?;
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn proc_sys_pipe_max_size_inode(fs_id: FsId, weak_sb: &Weak<Superblock>) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        SYS_PIPE_MAX_SIZE_INO,
        FileType::Regular,
        0o644,
        1,
        Arc::new(ProcRegularInodeOps {
            kind: ProcFileKind::SysPipeMaxSize,
        }),
    )
}

fn proc_sys_vm_dir_inode(fs_id: FsId, weak_sb: &Weak<Superblock>) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        SYS_VM_INO,
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcSysVmDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
        }),
    )
}

struct ProcSysVmDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
}

impl InodeOps for ProcSysVmDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        if let Some(param) = crate::mm::memstat::VmParam::from_name(name) {
            return Ok(proc_sys_vm_param_inode(self.fs_id, &self.weak_sb, param));
        }
        if let Some(entry) = VM_EXTRA_SYSCTLS.iter().copied().find(|e| *e == name) {
            return Ok(proc_sys_extra_inode(self.fs_id, &self.weak_sb, entry));
        }
        Err(VfsError::NotFound)
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        use crate::mm::memstat::VmParam;
        let params = [
            VmParam::OvercommitMemory,
            VmParam::OvercommitRatio,
            VmParam::OvercommitKbytes,
            VmParam::MaxMapCount,
            VmParam::MinFreeKbytes,
            VmParam::Swappiness,
            VmParam::PanicOnOom,
            VmParam::OomDumpTasks,
            VmParam::OomKillAllocatingTask,
            VmParam::PageCluster,
            VmParam::DirtyRatio,
            VmParam::DirtyBackgroundRatio,
            VmParam::DirtyWritebackCentisecs,
            VmParam::DirtyExpireCentisecs,
            VmParam::VfsCachePressure,
            VmParam::UnprivilegedUserfaultfd,
            VmParam::DropCaches,
        ];
        let mut snapshot: Vec<DirEntry> = params
            .into_iter()
            .enumerate()
            .map(|(index, param)| DirEntry {
                ino: SYS_VM_PARAM_BASE + index as u64,
                name: SmallStr::new(param.name()),
                kind: FileType::Regular,
            })
            .collect();
        push_extra_sysctl_entries(&mut snapshot, VM_EXTRA_SYSCTLS)?;
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn proc_sys_vm_param_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    param: crate::mm::memstat::VmParam,
) -> Arc<Inode> {
    use crate::mm::memstat::VmParam;
    let ino = SYS_VM_PARAM_BASE
        + match param {
            VmParam::OvercommitMemory => 0,
            VmParam::OvercommitRatio => 1,
            VmParam::OvercommitKbytes => 2,
            VmParam::MaxMapCount => 3,
            VmParam::MinFreeKbytes => 4,
            VmParam::Swappiness => 5,
            VmParam::PanicOnOom => 6,
            VmParam::OomDumpTasks => 7,
            VmParam::OomKillAllocatingTask => 8,
            VmParam::PageCluster => 9,
            VmParam::DirtyRatio => 10,
            VmParam::DirtyBackgroundRatio => 11,
            VmParam::DirtyWritebackCentisecs => 12,
            VmParam::DirtyExpireCentisecs => 13,
            VmParam::VfsCachePressure => 14,
            VmParam::UnprivilegedUserfaultfd => 15,
            VmParam::DropCaches => 16,
        };
    mk_inode(
        fs_id,
        weak_sb,
        ino,
        FileType::Regular,
        0o644,
        1,
        Arc::new(ProcRegularInodeOps {
            kind: ProcFileKind::SysVm(param),
        }),
    )
}

fn proc_sys_kernel_dir_inode(fs_id: FsId, weak_sb: &Weak<Superblock>) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        SYS_KERNEL_INO,
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcSysKernelDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
        }),
    )
}

struct ProcSysKernelDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
}

impl InodeOps for ProcSysKernelDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        match name {
            "hotplug" => Ok(proc_sys_hotplug_inode(self.fs_id, &self.weak_sb)),
            "pid_max" => Ok(proc_sys_pid_max_inode(self.fs_id, &self.weak_sb)),
            "sched_rt_period_us" => Ok(proc_sys_sched_inode(
                self.fs_id,
                &self.weak_sb,
                SYS_SCHED_RT_PERIOD_INO,
                ProcFileKind::SysSchedRtPeriod,
            )),
            "sched_rt_runtime_us" => Ok(proc_sys_sched_inode(
                self.fs_id,
                &self.weak_sb,
                SYS_SCHED_RT_RUNTIME_INO,
                ProcFileKind::SysSchedRtRuntime,
            )),
            "sched_rr_timeslice_ms" => Ok(proc_sys_sched_inode(
                self.fs_id,
                &self.weak_sb,
                SYS_SCHED_RR_TIMESLICE_INO,
                ProcFileKind::SysSchedRrTimeslice,
            )),
            "tainted" => Ok(proc_sys_tainted_inode(self.fs_id, &self.weak_sb)),
            "random" => Ok(proc_sys_random_dir_inode(self.fs_id, &self.weak_sb)),
            _ => {
                if let Some(entry) = KERNEL_EXTRA_SYSCTLS.iter().copied().find(|e| *e == name) {
                    Ok(proc_sys_extra_inode(self.fs_id, &self.weak_sb, entry))
                } else {
                    Err(VfsError::NotFound)
                }
            }
        }
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let mut snapshot = vec![
            DirEntry {
                ino: SYS_HOTPLUG_INO,
                name: SmallStr::new("hotplug"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: SYS_PID_MAX_INO,
                name: SmallStr::new("pid_max"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: SYS_SCHED_RT_PERIOD_INO,
                name: SmallStr::new("sched_rt_period_us"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: SYS_SCHED_RT_RUNTIME_INO,
                name: SmallStr::new("sched_rt_runtime_us"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: SYS_SCHED_RR_TIMESLICE_INO,
                name: SmallStr::new("sched_rr_timeslice_ms"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: SYS_TAINTED_INO,
                name: SmallStr::new("tainted"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: SYS_RANDOM_DIR_INO,
                name: SmallStr::new("random"),
                kind: FileType::Directory,
            },
        ];
        push_extra_sysctl_entries(&mut snapshot, KERNEL_EXTRA_SYSCTLS)?;
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn proc_sys_hotplug_inode(fs_id: FsId, weak_sb: &Weak<Superblock>) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        SYS_HOTPLUG_INO,
        FileType::Regular,
        0o644,
        1,
        Arc::new(ProcRegularInodeOps {
            kind: ProcFileKind::SysHotplug,
        }),
    )
}

fn proc_sys_pid_max_inode(fs_id: FsId, weak_sb: &Weak<Superblock>) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        SYS_PID_MAX_INO,
        FileType::Regular,
        0o644,
        1,
        Arc::new(ProcRegularInodeOps {
            kind: ProcFileKind::SysPidMax,
        }),
    )
}

fn proc_sys_file_max_inode(fs_id: FsId, weak_sb: &Weak<Superblock>) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        SYS_FILE_MAX_INO,
        FileType::Regular,
        0o644,
        1,
        Arc::new(ProcRegularInodeOps {
            kind: ProcFileKind::SysFileMax,
        }),
    )
}

fn proc_sys_tainted_inode(fs_id: FsId, weak_sb: &Weak<Superblock>) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        SYS_TAINTED_INO,
        FileType::Regular,
        0o444,
        1,
        Arc::new(ProcRegularInodeOps {
            kind: ProcFileKind::SysTainted,
        }),
    )
}

fn proc_sys_sched_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    ino: u64,
    kind: ProcFileKind,
) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        ino,
        FileType::Regular,
        0o644,
        1,
        Arc::new(ProcRegularInodeOps { kind }),
    )
}

fn proc_task_base(pid: PidT) -> u64 {
    PROC_DYNAMIC_BASE + pid as u64 * 64
}

fn proc_task_dir_ino(pid: PidT, view: TaskDirView) -> u64 {
    proc_task_base(pid)
        + match view {
            TaskDirView::Process => TASK_SLOT_DIR_PROCESS,
            TaskDirView::Thread => TASK_SLOT_DIR_THREAD,
        }
}

fn proc_task_link_ino(pid: PidT, kind: TaskLinkKind) -> u64 {
    proc_task_base(pid)
        + match kind {
            TaskLinkKind::Exe => TASK_SLOT_EXE,
            TaskLinkKind::Cwd => TASK_SLOT_CWD,
            TaskLinkKind::Root => TASK_SLOT_ROOT,
        }
}

fn proc_task_file_ino(pid: PidT, kind: TaskFileKind) -> u64 {
    proc_task_base(pid)
        + match kind {
            TaskFileKind::Status => TASK_SLOT_STATUS,
            TaskFileKind::Stat => TASK_SLOT_STAT,
            TaskFileKind::Cmdline => TASK_SLOT_CMDLINE,
            TaskFileKind::Environ => TASK_SLOT_ENVIRON,
            TaskFileKind::Comm => TASK_SLOT_COMM,
            TaskFileKind::Maps => TASK_SLOT_MAPS,
            TaskFileKind::Mountinfo => TASK_SLOT_MOUNTINFO,
            TaskFileKind::Mounts => TASK_SLOT_MOUNTS,
            TaskFileKind::Smaps => TASK_SLOT_SMAPS,
            TaskFileKind::NumaMaps => TASK_SLOT_NUMA_MAPS,
            TaskFileKind::Limits => TASK_SLOT_LIMITS,
            TaskFileKind::Auxv => TASK_SLOT_AUXV,
            TaskFileKind::Io => TASK_SLOT_IO,
            TaskFileKind::OomScore => TASK_SLOT_OOM_SCORE,
            TaskFileKind::OomScoreAdj => TASK_SLOT_OOM_SCORE_ADJ,
            TaskFileKind::OomAdj => TASK_SLOT_OOM_ADJ,
            TaskFileKind::Sched => TASK_SLOT_SCHED,
            TaskFileKind::Syscall => TASK_SLOT_SYSCALL,
            TaskFileKind::Stack => TASK_SLOT_STACK,
            TaskFileKind::Cgroup => TASK_SLOT_CGROUP,
            TaskFileKind::ClearRefs => TASK_SLOT_CLEAR_REFS,
            TaskFileKind::Pagemap => TASK_SLOT_PAGEMAP,
            TaskFileKind::Seccomp => TASK_SLOT_SECCOMP,
            TaskFileKind::Timers => TASK_SLOT_TIMERS,
            TaskFileKind::Loginuid => TASK_SLOT_LOGINUID,
            TaskFileKind::Sessionid => TASK_SLOT_SESSIONID,
            TaskFileKind::UidMap => TASK_SLOT_UID_MAP,
            TaskFileKind::GidMap => TASK_SLOT_GID_MAP,
            TaskFileKind::Mem => TASK_SLOT_MEM,
        }
}

fn proc_fd_dir_ino(pid: PidT) -> u64 {
    proc_task_base(pid) + TASK_SLOT_FD_DIR
}

fn proc_task_list_ino(pid: PidT) -> u64 {
    proc_task_base(pid) + TASK_SLOT_TASK_DIR
}

fn proc_fdinfo_dir_ino(pid: PidT) -> u64 {
    proc_task_base(pid) + TASK_SLOT_FDINFO_DIR
}

fn proc_ns_dir_ino(pid: PidT) -> u64 {
    proc_task_base(pid) + TASK_SLOT_NS_DIR
}

fn push_proc_task_ns_entry(snapshot: &mut Vec<DirEntry>, pid: PidT) {
    snapshot.push(DirEntry {
        ino: proc_ns_dir_ino(pid),
        name: SmallStr::new("ns"),
        kind: FileType::Directory,
    });
}

fn proc_fd_link_ino(pid: PidT, fd: u32) -> u64 {
    PROC_FD_BASE + pid as u64 * 1_000_000 + fd as u64
}

fn proc_task_dir_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    pid: PidT,
    view: TaskDirView,
) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        proc_task_dir_ino(pid, view),
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcTaskDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
            pid,
            view,
        }),
    )
}

fn proc_task_link_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    pid: PidT,
    kind: TaskLinkKind,
) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        proc_task_link_ino(pid, kind),
        FileType::Symlink,
        0o777,
        1,
        Arc::new(ProcTaskLinkOps { pid, kind }),
    )
}

fn proc_task_file_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    pid: PidT,
    kind: TaskFileKind,
) -> Arc<Inode> {
    // mem 需要跨地址空间读写，走独立 FileOps；其余文本文件复用常规渲染。
    if matches!(kind, TaskFileKind::Mem) {
        return mk_inode(
            fs_id,
            weak_sb,
            proc_task_file_ino(pid, kind),
            FileType::Regular,
            0o600,
            1,
            Arc::new(ProcMemInodeOps { pid }),
        );
    }
    // pagemap 是 offset 相关的稀疏二进制视图，也走独立 FileOps。
    if matches!(kind, TaskFileKind::Pagemap) {
        return mk_inode(
            fs_id,
            weak_sb,
            proc_task_file_ino(pid, kind),
            FileType::Regular,
            0o400,
            1,
            Arc::new(ProcPagemapInodeOps { pid }),
        );
    }
    // oom_score_adj/oom_adj/clear_refs 在 Linux 中可写（procfs 本地投影）。
    let mode = match kind {
        TaskFileKind::OomScoreAdj | TaskFileKind::OomAdj | TaskFileKind::ClearRefs => 0o644,
        _ => 0o444,
    };
    mk_inode(
        fs_id,
        weak_sb,
        proc_task_file_ino(pid, kind),
        FileType::Regular,
        mode,
        1,
        Arc::new(ProcRegularInodeOps {
            kind: ProcFileKind::Task { pid, kind },
        }),
    )
}

fn proc_fd_dir_inode(fs_id: FsId, weak_sb: &Weak<Superblock>, pid: PidT) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        proc_fd_dir_ino(pid),
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcFdDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
            pid,
        }),
    )
}

fn proc_task_list_dir_inode(fs_id: FsId, weak_sb: &Weak<Superblock>, pid: PidT) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        proc_task_list_ino(pid),
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcTaskListDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
            leader_pid: pid,
        }),
    )
}

fn proc_fd_link_inode(fs_id: FsId, weak_sb: &Weak<Superblock>, pid: PidT, fd: u32) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        proc_fd_link_ino(pid, fd),
        FileType::Symlink,
        0o777,
        1,
        Arc::new(ProcFdLinkOps { pid, fd }),
    )
}

struct ProcTaskDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
    pid: PidT,
    view: TaskDirView,
}

impl InodeOps for ProcTaskDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        match name {
            "ns" => Ok(proc_ns_dir_inode(self.fs_id, &self.weak_sb, self.pid)),
            "exe" => Ok(proc_task_link_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskLinkKind::Exe,
            )),
            "cwd" => Ok(proc_task_link_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskLinkKind::Cwd,
            )),
            "root" => Ok(proc_task_link_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskLinkKind::Root,
            )),
            "status" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Status,
            )),
            "stat" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Stat,
            )),
            "cmdline" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Cmdline,
            )),
            "environ" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Environ,
            )),
            "comm" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Comm,
            )),
            "maps" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Maps,
            )),
            "mountinfo" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Mountinfo,
            )),
            "mounts" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Mounts,
            )),
            "smaps" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Smaps,
            )),
            "numa_maps" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::NumaMaps,
            )),
            "limits" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Limits,
            )),
            "auxv" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Auxv,
            )),
            "io" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Io,
            )),
            "oom_score" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::OomScore,
            )),
            "oom_score_adj" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::OomScoreAdj,
            )),
            "oom_adj" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::OomAdj,
            )),
            "attr" => Ok(proc_task_attr_dir_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
            )),
            "sched" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Sched,
            )),
            "syscall" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Syscall,
            )),
            "stack" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Stack,
            )),
            "cgroup" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Cgroup,
            )),
            "clear_refs" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::ClearRefs,
            )),
            "pagemap" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Pagemap,
            )),
            "seccomp" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Seccomp,
            )),
            "timers" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Timers,
            )),
            "loginuid" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Loginuid,
            )),
            "sessionid" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Sessionid,
            )),
            "uid_map" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::UidMap,
            )),
            "gid_map" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::GidMap,
            )),
            "mem" => Ok(proc_task_file_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
                TaskFileKind::Mem,
            )),
            "fd" => Ok(proc_fd_dir_inode(self.fs_id, &self.weak_sb, self.pid)),
            "fdinfo" => Ok(proc_fdinfo_dir_inode(self.fs_id, &self.weak_sb, self.pid)),
            "task" if self.view == TaskDirView::Process => Ok(proc_task_list_dir_inode(
                self.fs_id,
                &self.weak_sb,
                self.pid,
            )),
            _ => Err(VfsError::NotFound),
        }
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        ensure_task_exists(self.pid)?;
        let mut snapshot = vec![
            DirEntry {
                ino: proc_task_link_ino(self.pid, TaskLinkKind::Exe),
                name: SmallStr::new("exe"),
                kind: FileType::Symlink,
            },
            DirEntry {
                ino: proc_task_link_ino(self.pid, TaskLinkKind::Cwd),
                name: SmallStr::new("cwd"),
                kind: FileType::Symlink,
            },
            DirEntry {
                ino: proc_task_link_ino(self.pid, TaskLinkKind::Root),
                name: SmallStr::new("root"),
                kind: FileType::Symlink,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Status),
                name: SmallStr::new("status"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Stat),
                name: SmallStr::new("stat"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Cmdline),
                name: SmallStr::new("cmdline"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Environ),
                name: SmallStr::new("environ"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Comm),
                name: SmallStr::new("comm"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Maps),
                name: SmallStr::new("maps"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Mountinfo),
                name: SmallStr::new("mountinfo"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Mounts),
                name: SmallStr::new("mounts"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Smaps),
                name: SmallStr::new("smaps"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::NumaMaps),
                name: SmallStr::new("numa_maps"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Limits),
                name: SmallStr::new("limits"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Auxv),
                name: SmallStr::new("auxv"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Io),
                name: SmallStr::new("io"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::OomScore),
                name: SmallStr::new("oom_score"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::OomScoreAdj),
                name: SmallStr::new("oom_score_adj"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::OomAdj),
                name: SmallStr::new("oom_adj"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_attr_dir_ino(self.pid),
                name: SmallStr::new("attr"),
                kind: FileType::Directory,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Sched),
                name: SmallStr::new("sched"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Syscall),
                name: SmallStr::new("syscall"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Stack),
                name: SmallStr::new("stack"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Cgroup),
                name: SmallStr::new("cgroup"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::ClearRefs),
                name: SmallStr::new("clear_refs"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Pagemap),
                name: SmallStr::new("pagemap"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Seccomp),
                name: SmallStr::new("seccomp"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Timers),
                name: SmallStr::new("timers"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Loginuid),
                name: SmallStr::new("loginuid"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Sessionid),
                name: SmallStr::new("sessionid"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::UidMap),
                name: SmallStr::new("uid_map"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::GidMap),
                name: SmallStr::new("gid_map"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_task_file_ino(self.pid, TaskFileKind::Mem),
                name: SmallStr::new("mem"),
                kind: FileType::Regular,
            },
            DirEntry {
                ino: proc_fd_dir_ino(self.pid),
                name: SmallStr::new("fd"),
                kind: FileType::Directory,
            },
        ];
        push_proc_task_ns_entry(&mut snapshot, self.pid);
        if self.view == TaskDirView::Process {
            snapshot.push(DirEntry {
                ino: proc_task_list_ino(self.pid),
                name: SmallStr::new("task"),
                kind: FileType::Directory,
            });
        }
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcTaskListDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
    leader_pid: PidT,
}

impl InodeOps for ProcTaskListDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        let tid = parse_pid_component(name).ok_or(VfsError::NotFound)?;
        let task = lookup_task(tid).ok_or(VfsError::NotFound)?;
        if task_leader_pid(&task) != Some(self.leader_pid) {
            return Err(VfsError::NotFound);
        }
        Ok(proc_task_dir_inode(
            self.fs_id,
            &self.weak_sb,
            tid,
            TaskDirView::Thread,
        ))
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let mut tids = snapshot_thread_ids(self.leader_pid)?;
        tids.sort_unstable();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve(tids.len())
            .map_err(|_| VfsError::NoSpace)?;
        for tid in tids {
            let name = procfs_decimal_name(tid)?;
            push_proc_dir_entry(
                &mut snapshot,
                proc_task_dir_ino(tid, TaskDirView::Thread),
                &name,
                FileType::Directory,
            )?;
        }
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcFdDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
    pid: PidT,
}

impl InodeOps for ProcFdDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        let task = lookup_task(self.pid).ok_or(VfsError::NotFound)?;
        ensure_task_access(&task)?;
        let fd = parse_fd_component(name).ok_or(VfsError::NotFound)?;
        let fdt = task_fdtable(&task).ok_or(VfsError::NotFound)?;
        if fdt.get_file(Fd::from_raw(fd)).is_none() {
            return Err(VfsError::NotFound);
        }
        Ok(proc_fd_link_inode(self.fs_id, &self.weak_sb, self.pid, fd))
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let task = lookup_task(self.pid).ok_or(VfsError::NotFound)?;
        ensure_task_access(&task)?;
        let fdt = task_fdtable(&task).ok_or(VfsError::NotFound)?;
        let mut fds = fdt.snapshot_fds();
        fds.sort_unstable_by_key(|(fd, _)| *fd);
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve(fds.len())
            .map_err(|_| VfsError::NoSpace)?;
        for (fd, _) in fds {
            let name = procfs_decimal_name(fd)?;
            push_proc_dir_entry(
                &mut snapshot,
                proc_fd_link_ino(self.pid, fd),
                &name,
                FileType::Symlink,
            )?;
        }
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcTaskLinkOps {
    pid: PidT,
    kind: TaskLinkKind,
}

impl InodeOps for ProcTaskLinkOps {
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
        let task = lookup_task(self.pid).ok_or(VfsError::NotFound)?;
        ensure_task_access(&task)?;
        match self.kind {
            TaskLinkKind::Exe => task_exec_path(&task),
            TaskLinkKind::Cwd => task_cwd_path(&task),
            TaskLinkKind::Root => task_root_path(&task),
        }
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn proc_fdinfo_dir_inode(fs_id: FsId, weak_sb: &Weak<Superblock>, pid: PidT) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        proc_fdinfo_dir_ino(pid),
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcFdInfoDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
            pid,
        }),
    )
}

fn proc_fdinfo_file_ino(pid: PidT, fd: u32) -> u64 {
    // 与 fd 链接 ino 区分：高位翻转一位避免冲突。
    proc_fd_link_ino(pid, fd) | (1 << 40)
}

struct ProcFdInfoDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
    pid: PidT,
}

impl InodeOps for ProcFdInfoDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        let task = lookup_task(self.pid).ok_or(VfsError::NotFound)?;
        ensure_task_access(&task)?;
        let fd = parse_fd_component(name).ok_or(VfsError::NotFound)?;
        let fdt = task_fdtable(&task).ok_or(VfsError::NotFound)?;
        if fdt.get_file(Fd::from_raw(fd)).is_none() {
            return Err(VfsError::NotFound);
        }
        Ok(proc_fdinfo_file_inode(
            self.fs_id,
            &self.weak_sb,
            self.pid,
            fd,
        ))
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let task = lookup_task(self.pid).ok_or(VfsError::NotFound)?;
        ensure_task_access(&task)?;
        let fdt = task_fdtable(&task).ok_or(VfsError::NotFound)?;
        let mut fds = fdt.snapshot_fds();
        fds.sort_unstable_by_key(|(fd, _)| *fd);
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve(fds.len())
            .map_err(|_| VfsError::NoSpace)?;
        for (fd, _) in fds {
            let name = procfs_decimal_name(fd)?;
            push_proc_dir_entry(
                &mut snapshot,
                proc_fdinfo_file_ino(self.pid, fd),
                &name,
                FileType::Regular,
            )?;
        }
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcFdInfoFileInodeOps {
    pid: PidT,
    fd: u32,
}

impl InodeOps for ProcFdInfoFileInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }
    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(ProcFdInfoFileOps {
            pid: self.pid,
            fd: self.fd,
        }))
    }
    fn readlink(&self, _inode: &Inode) -> VfsResult<alloc::string::String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn proc_fdinfo_file_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    pid: PidT,
    fd: u32,
) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        proc_fdinfo_file_ino(pid, fd),
        FileType::Regular,
        0o444,
        1,
        Arc::new(ProcFdInfoFileInodeOps { pid, fd }),
    )
}

/// `/proc/self/fdinfo/<fd>`：pos/flags/mnt_id + 驱动专属行（show_fdinfo）。
struct ProcFdInfoFileOps {
    pid: PidT,
    fd: u32,
}

impl ProcFdInfoFileOps {
    fn render(&self, buf: &mut [u8]) -> VfsResult<usize> {
        use core::fmt::Write;
        let task = lookup_task(self.pid).ok_or(VfsError::NotFound)?;
        ensure_task_access(&task)?;
        let fdt = task_fdtable(&task).ok_or(VfsError::NotFound)?;
        let file = fdt
            .get_file(Fd::from_raw(self.fd))
            .ok_or(VfsError::NotFound)?;
        let mut out = alloc::string::String::new();
        let _ = writeln!(out, "pos:\t{}", file.pos());
        let _ = writeln!(out, "flags:\t{:o}", file.status_flags());
        // Mount 无稳定 id 字段；用挂载对象地址作为 boot 内稳定伪 mnt_id。
        let mnt_id = Arc::as_ptr(file.mount()) as u64;
        let _ = writeln!(out, "mnt_id:\t{mnt_id}");
        file.show_fdinfo(&mut out);
        let bytes = out.as_bytes();
        let take = bytes.len().min(buf.len());
        buf[..take].copy_from_slice(&bytes[..take]);
        Ok(take)
    }
}

impl FileOps for ProcFdInfoFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut full = alloc::vec![0u8; 4096];
        let n = self.render(&mut full)?;
        let start = (offset as usize).min(n);
        let take = (n - start).min(buf.len());
        buf[..take].copy_from_slice(&full[start..start + take]);
        Ok(take)
    }
    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidArgument)
    }
    fn poll(&self, _interest: PollEvents) -> PollEvents {
        PollEvents::default()
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
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcFdLinkOps {
    pid: PidT,
    fd: u32,
}

impl InodeOps for ProcFdLinkOps {
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
        let task = lookup_task(self.pid).ok_or(VfsError::NotFound)?;
        ensure_task_access(&task)?;
        let fdt = task_fdtable(&task).ok_or(VfsError::NotFound)?;
        let file = fdt
            .get_file(Fd::from_raw(self.fd))
            .ok_or(VfsError::NotFound)?;
        Ok(fd_target_path(&task, &file))
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcDirFile {
    snapshot: Vec<DirEntry>,
}

impl FileOps for ProcDirFile {
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
        let start = pos as usize;
        for (idx, entry) in self.snapshot.iter().enumerate().skip(start) {
            if sink(entry.clone()).is_break() {
                return Ok(idx as u64);
            }
        }
        Ok(self.snapshot.len() as u64)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, interest: PollEvents) -> PollEvents {
        // 目录枚举基于打开时快照，可立即尝试读取目录项。
        PollEvents::READ_WRITE_READY.intersect(interest)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcRegularInodeOps {
    kind: ProcFileKind,
}

impl InodeOps for ProcRegularInodeOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        if let ProcFileKind::Task { pid, .. } = self.kind {
            let task = lookup_task(pid).ok_or(VfsError::NotFound)?;
            ensure_task_access(&task)?;
        }
        let snapshot = match self.kind {
            ProcFileKind::Root(RootFileKind::TaskSnapshot) => {
                Some(render_task_snapshot()?.into_boxed_slice())
            }
            _ => None,
        };
        Ok(Box::new(ProcRegularFile {
            kind: self.kind,
            snapshot,
        }))
    }

    fn truncate(&self, _: &Inode, size: u64) -> VfsResult<()> {
        match self.kind {
            ProcFileKind::SysHotplug if size == 0 => {
                HOTPLUG_PATH.lock().clear();
                Ok(())
            }
            ProcFileKind::SysSchedRtPeriod
            | ProcFileKind::SysSchedRtRuntime
            | ProcFileKind::SysSchedRrTimeslice
                if size == 0 =>
            {
                Ok(())
            }
            ProcFileKind::SysHotplug => Err(VfsError::InvalidArgument),
            ProcFileKind::SysFileMax if size == 0 => Ok(()),
            ProcFileKind::SysFileMax => Err(VfsError::InvalidArgument),
            ProcFileKind::SysPipeMaxSize if size == 0 => Ok(()),
            ProcFileKind::SysPipeMaxSize => Err(VfsError::InvalidArgument),
            ProcFileKind::SysVm(_) if size == 0 => Ok(()),
            ProcFileKind::SysVm(_) => Err(VfsError::InvalidArgument),
            ProcFileKind::SysPidMax if size == 0 => Ok(()),
            ProcFileKind::SysPidMax => Err(VfsError::InvalidArgument),
            ProcFileKind::SysExtra(_) if size == 0 => Ok(()),
            ProcFileKind::SysExtra(_) => Err(VfsError::InvalidArgument),
            ProcFileKind::Task {
                kind: TaskFileKind::OomScoreAdj | TaskFileKind::OomAdj | TaskFileKind::ClearRefs,
                ..
            } if size == 0 => Ok(()),
            _ => Err(VfsError::ReadOnlyFilesystem),
        }
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcRegularFile {
    kind: ProcFileKind,
    snapshot: Option<Box<[u8]>>,
}

impl FileOps for ProcRegularFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if let ProcFileKind::Root(RootFileKind::MemInfo) = self.kind {
            return read_meminfo_at(buf, offset);
        }
        if let Some(snapshot) = &self.snapshot {
            return slice_bytes(buf, offset, snapshot);
        }
        let content = render_proc_file(self.kind)?;
        slice_bytes(buf, offset, &content)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        match self.kind {
            ProcFileKind::SysHotplug => {
                if offset != 0 {
                    return Err(VfsError::InvalidArgument);
                }
                let text = core::str::from_utf8(buf).map_err(|_| VfsError::InvalidArgument)?;
                let trimmed = text.trim_end_matches(|ch| ch == '\n' || ch == '\0');
                *HOTPLUG_PATH.lock() = String::from(trimmed);
                Ok(buf.len())
            }
            ProcFileKind::SysFileMax => {
                if offset != 0 {
                    return Err(VfsError::InvalidArgument);
                }
                let value = vfs::sysctl::parse_nonnegative_long(buf)?;
                FILE_MAX.store(value, Ordering::Relaxed);
                Ok(buf.len())
            }
            ProcFileKind::SysSchedRtPeriod => {
                write_sched_sysctl(buf, offset, |value| sched::set_sched_rt_period_us(value))
            }
            ProcFileKind::SysSchedRtRuntime => {
                write_sched_sysctl(buf, offset, |value| sched::set_sched_rt_runtime_us(value))
            }
            ProcFileKind::SysSchedRrTimeslice => {
                write_sched_sysctl(buf, offset, |value| sched::set_sched_rr_timeslice_ms(value))
            }
            ProcFileKind::SysPipeMaxSize => {
                if offset != 0 {
                    return Err(VfsError::InvalidArgument);
                }
                let text = core::str::from_utf8(buf).map_err(|_| VfsError::InvalidArgument)?;
                let value = text
                    .trim_matches(|ch: char| ch.is_ascii_whitespace() || ch == '\0')
                    .parse::<usize>()
                    .map_err(|_| VfsError::InvalidArgument)?;
                vfs::pipe::set_pipe_max_size(value).map_err(|err| match err {
                    errno::Errno::EPERM => VfsError::OperationNotPermitted,
                    _ => VfsError::InvalidArgument,
                })?;
                Ok(buf.len())
            }
            ProcFileKind::SysVm(param) => write_vm_sysctl(param, buf, offset),
            ProcFileKind::SysPidMax => write_pid_max(buf, offset),
            ProcFileKind::SysExtra(name) => write_extra_sysctl(name, buf, offset),
            ProcFileKind::Task {
                pid,
                kind: TaskFileKind::OomScoreAdj,
            } => write_task_oom_score_adj(pid, buf, offset),
            ProcFileKind::Task {
                pid,
                kind: TaskFileKind::OomAdj,
            } => write_task_oom_adj(pid, buf, offset),
            ProcFileKind::Task {
                pid,
                kind: TaskFileKind::ClearRefs,
            } => {
                // 无引用位跟踪；接受写入（语义近似为 no-op），保持 ABI 可写。
                if offset != 0 {
                    return Err(VfsError::InvalidArgument);
                }
                let _ = lookup_task(pid).ok_or(VfsError::NotFound)?;
                Ok(buf.len())
            }
            _ => Err(VfsError::ReadOnlyFilesystem),
        }
    }

    fn readdir(&self, _: u64, _: &mut dyn FnMut(DirEntry) -> ControlFlow<()>) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, interest: PollEvents) -> PollEvents {
        // 只读或少量可写的伪文件不会等待外部 I/O 事件。
        PollEvents::READ_WRITE_READY.intersect(interest)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn slice_bytes(buf: &mut [u8], offset: u64, content: &[u8]) -> VfsResult<usize> {
    if offset > usize::MAX as u64 {
        return Ok(0);
    }
    let start = (offset as usize).min(content.len());
    let end = start.saturating_add(buf.len()).min(content.len());
    let len = end - start;
    buf[..len].copy_from_slice(&content[start..end]);
    Ok(len)
}

fn render_proc_file(kind: ProcFileKind) -> VfsResult<Vec<u8>> {
    match kind {
        ProcFileKind::Root(kind) => Ok(match kind {
            RootFileKind::Filesystems => render_filesystems().into_bytes(),
            RootFileKind::Mounts => render_mounts().into_bytes(),
            RootFileKind::Mountinfo => render_mountinfo_root().into_bytes(),
            RootFileKind::Version => render_version().into_bytes(),
            RootFileKind::CpuInfo => render_cpuinfo().into_bytes(),
            RootFileKind::MemInfo => render_meminfo().into_bytes(),
            RootFileKind::Swaps => render_swaps().into_bytes(),
            RootFileKind::Uptime => render_uptime().into_bytes(),
            RootFileKind::Stat => render_stat().into_bytes(),
            RootFileKind::Interrupts => render_interrupts().into_bytes(),
            RootFileKind::Devices => render_devices().into_bytes(),
            RootFileKind::Pnp => render_pnp().into_bytes(),
            RootFileKind::DeviceFunctions => render_device_functions().into_bytes(),
            RootFileKind::TaskSnapshot => return render_task_snapshot(),
            RootFileKind::Loadavg => render_loadavg().into_bytes(),
            RootFileKind::Cmdline => render_cmdline().into_bytes(),
            RootFileKind::Partitions => render_partitions().into_bytes(),
            RootFileKind::Diskstats => render_diskstats().into_bytes(),
            RootFileKind::Kallsyms => render_kallsyms().into_bytes(),
            RootFileKind::Vmstat => render_vmstat().into_bytes(),
            RootFileKind::Zoneinfo => render_zoneinfo().into_bytes(),
            RootFileKind::Buddyinfo => render_buddyinfo().into_bytes(),
            RootFileKind::Iomem => render_iomem().into_bytes(),
            RootFileKind::Softirqs => render_softirqs().into_bytes(),
            RootFileKind::SysvipcShm => render_sysvipc_shm().into_bytes(),
            RootFileKind::SysvipcSem => render_sysvipc_sem().into_bytes(),
            RootFileKind::SysvipcMsg => render_sysvipc_msg().into_bytes(),
            RootFileKind::Keys => render_proc_keys().into_bytes(),
            RootFileKind::KeyUsers => render_proc_key_users().into_bytes(),
        }),
        ProcFileKind::Task { pid, kind } => {
            let task = lookup_task(pid).ok_or(VfsError::NotFound)?;
            ensure_task_access(&task)?;
            Ok(match kind {
                TaskFileKind::Status => render_task_status(&task).into_bytes(),
                TaskFileKind::Stat => render_task_stat(&task).into_bytes(),
                TaskFileKind::Cmdline => render_task_cmdline(&task),
                TaskFileKind::Environ => render_task_environ(&task),
                TaskFileKind::Comm => render_task_comm(&task).into_bytes(),
                TaskFileKind::Maps => render_task_maps(&task).into_bytes(),
                TaskFileKind::Mountinfo => render_task_mountinfo(&task)?.into_bytes(),
                TaskFileKind::Mounts => render_task_mounts(&task)?.into_bytes(),
                TaskFileKind::Smaps => render_task_smaps(&task).into_bytes(),
                TaskFileKind::NumaMaps => render_task_numa_maps(&task).into_bytes(),
                TaskFileKind::Limits => render_task_limits(&task).into_bytes(),
                TaskFileKind::Auxv => render_task_auxv(&task),
                TaskFileKind::Io => render_task_io(&task).into_bytes(),
                TaskFileKind::OomScore => render_task_oom_score(&task).into_bytes(),
                TaskFileKind::OomScoreAdj => render_task_oom_score_adj(pid).into_bytes(),
                TaskFileKind::OomAdj => render_task_oom_adj(pid).into_bytes(),
                TaskFileKind::Sched => render_task_sched(&task).into_bytes(),
                TaskFileKind::Syscall => render_task_syscall(&task).into_bytes(),
                TaskFileKind::Stack => render_task_stack(&task).into_bytes(),
                TaskFileKind::Cgroup => render_task_cgroup(&task).into_bytes(),
                TaskFileKind::ClearRefs => Vec::new(),
                TaskFileKind::Pagemap => Vec::new(),
                TaskFileKind::Seccomp => render_task_seccomp(&task).into_bytes(),
                TaskFileKind::Timers => render_task_timers(&task).into_bytes(),
                TaskFileKind::Loginuid => render_task_loginuid(&task).into_bytes(),
                TaskFileKind::Sessionid => render_task_sessionid(&task).into_bytes(),
                TaskFileKind::UidMap => render_task_uid_map(&task).into_bytes(),
                TaskFileKind::GidMap => render_task_gid_map(&task).into_bytes(),
                TaskFileKind::Mem => {
                    return Err(VfsError::InvalidArgument);
                }
            })
        }
        ProcFileKind::SysHotplug => Ok(render_hotplug().into_bytes()),
        ProcFileKind::SysPidMax => Ok(render_pid_max().into_bytes()),
        ProcFileKind::SysFileMax => Ok(render_file_max().into_bytes()),
        ProcFileKind::SysSchedRtPeriod => {
            Ok(format!("{}\n", sched::sched_rt_period_us()).into_bytes())
        }
        ProcFileKind::SysSchedRtRuntime => {
            Ok(format!("{}\n", sched::sched_rt_runtime_us()).into_bytes())
        }
        ProcFileKind::SysSchedRrTimeslice => {
            Ok(format!("{}\n", sched::sched_rr_timeslice_ms()).into_bytes())
        }
        ProcFileKind::SysTainted => Ok(format!("{}\n", kernel_taint_flags()).into_bytes()),
        ProcFileKind::SysPipeMaxSize => {
            Ok(format!("{}\n", vfs::pipe::pipe_max_size()).into_bytes())
        }
        ProcFileKind::SysVm(param) => {
            use crate::mm::memstat::VmParam;
            let value = match param {
                VmParam::OomDumpTasks
                | VmParam::OomKillAllocatingTask
                | VmParam::UnprivilegedUserfaultfd => {
                    u64::from(crate::mm::memstat::get_vm_bool(param))
                }
                VmParam::DropCaches => u64::from(crate::mm::memstat::drop_caches_request()),
                _ => crate::mm::memstat::get_vm_u64(param),
            };
            Ok(format!("{value}\n").into_bytes())
        }
        ProcFileKind::SysExtra(name) => Ok(render_extra_sysctl(name).into_bytes()),
    }
}

/// 写入 `/proc/sys/vm/<param>`。取值范围校验与 Linux proc_dointvec 一致：
/// 越界值返回 EINVAL。`drop_caches` 写入后立即执行清缓存动作。
fn write_vm_sysctl(
    param: crate::mm::memstat::VmParam,
    buf: &[u8],
    offset: u64,
) -> VfsResult<usize> {
    use crate::mm::memstat::VmParam;
    if offset != 0 {
        return Err(VfsError::InvalidArgument);
    }
    let text = core::str::from_utf8(buf).map_err(|_| VfsError::InvalidArgument)?;
    let raw = text
        .trim_matches(|ch: char| ch.is_ascii_whitespace() || ch == '\0')
        .parse::<u64>()
        .map_err(|_| VfsError::InvalidArgument)?;
    let valid = match param {
        VmParam::OvercommitMemory => raw <= 2,
        VmParam::OvercommitRatio => raw <= 100,
        VmParam::OvercommitKbytes => true,
        VmParam::MaxMapCount => raw >= 1,
        VmParam::MinFreeKbytes => true,
        VmParam::Swappiness => raw <= 200,
        VmParam::PanicOnOom => raw <= 2,
        VmParam::OomDumpTasks
        | VmParam::OomKillAllocatingTask
        | VmParam::UnprivilegedUserfaultfd => raw <= 1,
        VmParam::PageCluster => true,
        VmParam::DirtyRatio | VmParam::DirtyBackgroundRatio => raw <= 100,
        VmParam::DirtyWritebackCentisecs | VmParam::DirtyExpireCentisecs => true,
        VmParam::VfsCachePressure => raw <= 1000,
        VmParam::DropCaches => raw >= 1 && raw <= 3,
    };
    if !valid {
        return Err(VfsError::InvalidArgument);
    }
    if param == VmParam::DropCaches {
        if crate::mm::memstat::accept_drop_caches(raw as u32) {
            let (drop_page, _drop_dentry) = crate::mm::memstat::perform_drop_caches();
            if drop_page {
                crate::mm::drop_private_file_cache();
            }
        }
        return Ok(buf.len());
    }
    match param {
        VmParam::OomDumpTasks
        | VmParam::OomKillAllocatingTask
        | VmParam::UnprivilegedUserfaultfd => {
            crate::mm::memstat::set_vm_bool(param, raw != 0);
        }
        _ => crate::mm::memstat::set_vm_u64(param, raw),
    }
    Ok(buf.len())
}

fn write_sched_sysctl(
    buf: &[u8],
    offset: u64,
    update: impl FnOnce(i64) -> Result<(), errno::Errno>,
) -> VfsResult<usize> {
    if offset != 0 {
        return Err(VfsError::InvalidArgument);
    }
    let text = core::str::from_utf8(buf).map_err(|_| VfsError::InvalidArgument)?;
    let value = text
        .trim_matches(|ch: char| ch.is_ascii_whitespace() || ch == '\0')
        .parse::<i64>()
        .map_err(|_| VfsError::InvalidArgument)?;
    update(value).map_err(|_| VfsError::InvalidArgument)?;
    Ok(buf.len())
}

fn parse_pid_component(name: &str) -> Option<PidT> {
    if name.is_empty() {
        return None;
    }
    let mut value = 0i32;
    for byte in name.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as i32)?;
    }
    (value > 0).then_some(value)
}

fn parse_fd_component(name: &str) -> Option<u32> {
    if name.is_empty() {
        return None;
    }
    let mut value = 0u32;
    for byte in name.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as u32)?;
    }
    Some(value)
}

fn lookup_task(pid: PidT) -> Option<Arc<Task>> {
    if !sched::is_ready() || pid <= 0 {
        return None;
    }
    sched::root_pid_ns()
        .registry()
        .lookup(pid)
        .and_then(|weak| weak.upgrade())
}

fn ensure_task_exists(pid: PidT) -> VfsResult<Arc<Task>> {
    lookup_task(pid).ok_or(VfsError::NotFound)
}

/// 在一次打开操作中固定任务组状态，供控制面验证 STOP/late-fork 边界。
///
/// 这里刻意不调用 `render_task_stat`：后者还会遍历 VMA，任务 teardown 时
/// 可能长时间等待地址空间锁。快照只读取 PID registry、关系、进程组和原子
/// 状态字段，调用方应把它视为一致性检查而不是完整的 proc ABI。
fn render_task_snapshot() -> VfsResult<Vec<u8>> {
    let mut tasks = Vec::new();
    if sched::is_ready() {
        for (pid, weak) in sched::root_pid_ns().registry().snapshot() {
            if let Some(task) = weak.upgrade() {
                tasks.push((pid, task));
            }
        }
    }
    tasks.sort_unstable_by_key(|(pid, _)| *pid);

    let mut out = String::new();
    out.try_reserve(72usize.saturating_add(tasks.len().saturating_mul(112)))
        .map_err(|_| VfsError::NoSpace)?;
    out.push_str("# mygo.task-snapshot.v1 pid ppid tgid pgrp state start_ticks comm\n");
    for (pid, task) in tasks {
        let ppid = task
            .parent()
            .and_then(|parent| parent.tgid_cached().or_else(|| parent.pid_root_cached()))
            .unwrap_or(0);
        let tgid = task
            .tgid_cached()
            .or_else(|| task.pid_root_cached())
            .unwrap_or(pid);
        let pgrp = task.process_group().pgid();
        let state = task_state_char(task.state());
        let start_ticks = proc_cpu_ticks(task.start_time_ns());
        write!(
            out,
            "{pid}\t{ppid}\t{tgid}\t{pgrp}\t{state}\t{start_ticks}\t"
        )
        .map_err(|_| VfsError::NoSpace)?;
        let raw_comm = task.comm();
        let mut comm_empty = true;
        for byte in raw_comm.iter().copied().take_while(|byte| *byte != 0) {
            if byte.is_ascii_graphic() && !matches!(byte, b'|' | b'=') {
                out.push(byte as char);
            } else {
                out.push('_');
            }
            comm_empty = false;
        }
        if comm_empty {
            out.push_str("unknown");
        }
        out.push('\n');
    }
    Ok(out.into_bytes())
}

fn snapshot_root_processes() -> Vec<PidT> {
    if !sched::is_ready() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (pid, weak) in sched::root_pid_ns().registry().snapshot() {
        let Some(task) = weak.upgrade() else {
            continue;
        };
        if task_is_group_leader(&task) {
            out.push(pid);
        }
    }
    out.sort_unstable();
    out
}

fn snapshot_thread_ids(leader_pid: PidT) -> VfsResult<Vec<PidT>> {
    let leader = lookup_task(leader_pid).ok_or(VfsError::NotFound)?;
    let mut tids = Vec::new();
    for member in leader.thread_group().snapshot() {
        if let Some(pid) = member.pid_root() {
            tids.push(pid);
        }
    }
    Ok(tids)
}

fn current_tgid_tid() -> VfsResult<(PidT, PidT)> {
    if !sched::is_ready() {
        return Err(VfsError::NotFound);
    }
    let me = sched::current_task();
    let tid = me.pid_root().ok_or(VfsError::NotFound)?;
    let tgid = task_leader_pid(&me).unwrap_or(tid);
    Ok((tgid, tid))
}

fn task_leader_pid(task: &Arc<Task>) -> Option<PidT> {
    task.thread_group()
        .leader()
        .and_then(|leader| leader.pid_root())
        .or_else(|| task.pid_root())
}

fn task_is_group_leader(task: &Arc<Task>) -> bool {
    task.pid_root() == task_leader_pid(task)
}

fn task_vfs_context(task: &Arc<Task>) -> Option<Arc<VfsContext>> {
    task.ext_lookup(sched::TASKEXT_VFS_CONTEXT)?
        .downcast::<VfsContext>()
        .ok()
}

fn task_fdtable(task: &Arc<Task>) -> Option<Arc<FdTable>> {
    task.ext_lookup(sched::TASKEXT_VFS_FDTABLE)?
        .downcast::<FdTable>()
        .ok()
}

fn task_vm_space(task: &Arc<Task>) -> Option<Arc<VmSpace>> {
    task.ext_lookup(sched::TASKEXT_VM_SPACE)?
        .downcast::<VmSpace>()
        .ok()
}

fn task_exec_path(task: &Arc<Task>) -> VfsResult<String> {
    task.ext_lookup(sched::TASKEXT_EXEC_PATH)
        .and_then(|payload| payload.downcast::<String>().ok())
        .map(|path| (*path).clone())
        .ok_or(VfsError::NotFound)
}

fn task_exec_args(task: &Arc<Task>) -> Option<Vec<String>> {
    task.ext_lookup(sched::TASKEXT_EXEC_ARGS)
        .and_then(|payload| payload.downcast::<Vec<String>>().ok())
        .map(|args| (*args).clone())
}

fn task_exec_envp(task: &Arc<Task>) -> Option<Vec<String>> {
    task.ext_lookup(sched::TASKEXT_EXEC_ENVP)
        .and_then(|payload| payload.downcast::<Vec<String>>().ok())
        .map(|envp| (*envp).clone())
}

fn task_root_path(task: &Arc<Task>) -> VfsResult<String> {
    let ctx = task_vfs_context(task).ok_or(VfsError::NotFound)?;
    let mut root = namespace_path(&ctx, &ctx.root.root(), &ctx.root.mount())
        .unwrap_or_else(|| String::from("/"));
    if root.is_empty() {
        root.push('/');
    }
    Ok(root)
}

fn task_cwd_path(task: &Arc<Task>) -> VfsResult<String> {
    let ctx = task_vfs_context(task).ok_or(VfsError::NotFound)?;
    let mut cwd =
        namespace_path(&ctx, &ctx.cwd(), &ctx.cwd_mount()).unwrap_or_else(|| String::from("/"));
    if cwd.is_empty() {
        cwd.push('/');
    }
    Ok(cwd)
}

fn fd_target_path(task: &Arc<Task>, file: &Arc<File>) -> String {
    if let Some(ctx) = task_vfs_context(task) {
        if let Some(path) = namespace_path(&ctx, file.dentry(), file.mount()) {
            return path;
        }
    }
    if let Some(path) = file.dentry().full_path(&file.mount().mount_root) {
        return path;
    }
    format!("[inode:{}]", file.inode().ino())
}

fn uid_sets_match(me: &SchedCredentials, target: &SchedCredentials) -> bool {
    let mine = [me.uid.0, me.euid.0, me.suid.0];
    let theirs = [target.uid.0, target.euid.0, target.suid.0];
    mine.iter().any(|uid| theirs.contains(uid))
}

fn can_inspect_task(task: &Arc<Task>) -> bool {
    if !sched::is_ready() {
        return true;
    }
    let me = sched::current_task();
    if Arc::ptr_eq(&me, task) {
        return true;
    }
    if Arc::ptr_eq(&me.thread_group(), &task.thread_group()) {
        return true;
    }
    let me_creds = me.credentials();
    if me_creds.has_cap(SchedCapability::DacReadSearch)
        || me_creds.has_cap(SchedCapability::DacOverride)
    {
        return true;
    }
    let target_creds = task.credentials();
    uid_sets_match(&me_creds, &target_creds)
}

fn ensure_task_access(task: &Arc<Task>) -> VfsResult<()> {
    if can_inspect_task(task) {
        Ok(())
    } else {
        Err(VfsError::PermissionDenied)
    }
}

fn task_state_char(state: TaskState) -> char {
    match state {
        TaskState::New | TaskState::Runnable | TaskState::Running | TaskState::Continued => 'R',
        TaskState::Sleeping => 'S',
        TaskState::Uninterruptible => 'D',
        TaskState::Stopped => 'T',
        TaskState::Zombie => 'Z',
        TaskState::Dead => 'X',
    }
}

fn task_state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::New | TaskState::Runnable | TaskState::Running | TaskState::Continued => {
            "running"
        }
        TaskState::Sleeping => "sleeping",
        TaskState::Uninterruptible => "disk sleep",
        TaskState::Stopped => "stopped",
        TaskState::Zombie => "zombie",
        TaskState::Dead => "dead",
    }
}

fn task_parent_pid(task: &Arc<Task>) -> PidT {
    task.parent()
        .and_then(|parent| parent.thread_group().leader())
        .and_then(|leader| leader.pid_root())
        .unwrap_or(0)
}

fn task_pgrp(task: &Arc<Task>) -> PidT {
    task.process_group()
        .snapshot()
        .iter()
        .find_map(|member| {
            member
                .thread_group()
                .leader()
                .and_then(|leader| leader.pid_root())
        })
        .unwrap_or(0)
}

fn task_session(task: &Arc<Task>) -> PidT {
    task.process_group()
        .session()
        .and_then(|session| session.leader())
        .and_then(|leader| leader.pid_root())
        .unwrap_or(0)
}

fn task_memory_usage(task: &Arc<Task>) -> (u64, u64, u64) {
    let Some(vm) = task_vm_space(task) else {
        return (0, 0, 0);
    };
    let mut vsize = 0u64;
    let mut data = 0u64;
    for (range, flags) in dump_vmas(&vm) {
        let size = (range.end - range.start) as u64;
        vsize = vsize.saturating_add(size);
        if flags.has(VmFlags::WRITE)
            && !flags.has(VmFlags::SHARED)
            && !flags.has(VmFlags::GROWS_DOWN)
        {
            data = data.saturating_add(size);
        }
    }
    let rss = vm.mapped_pages() as u64 * page_size() as u64;
    (vsize, rss, data)
}

fn task_thread_count(task: &Arc<Task>) -> usize {
    task.thread_group().snapshot().len()
}

/// 任务信号视图：(SigPnd, ShdPnd, SigBlk, SigIgn, SigCgt)。
fn task_signal_views(task: &Arc<Task>) -> (u64, u64, u64, u64, u64) {
    let sigpnd = task.signal.pending_snapshot().raw();
    let shdpnd = task.shared_signal().pending_snapshot().raw();
    let sigblk = task.signal.blocked_snapshot().raw();
    let shared = task.shared_signal();
    let mut sigign = 0u64;
    let mut sigcgt = 0u64;
    for n in 1..sched::signal::NSIG {
        let Some(sig) = sched::SignalNumber::from_raw(n as i32) else {
            continue;
        };
        match shared.get_action(sig).handler {
            sched::SigHandler::Ignore => sigign |= sig.bit(),
            sched::SigHandler::Handler(_) => sigcgt |= sig.bit(),
            sched::SigHandler::Default => {}
        }
    }
    (sigpnd, shdpnd, sigblk, sigign, sigcgt)
}

fn online_cpu_list() -> String {
    use alloc::fmt::Write;
    let mask = sched::online_cpu_mask();
    let mut out = String::new();
    for cpu in 0..sched::NR_CPUS {
        if mask & (1u64 << cpu) != 0 {
            if !out.is_empty() {
                out.push(',');
            }
            let _ = write!(out, "{cpu}");
        }
    }
    out
}

/// 统计代码段（VmExe）与库段（VmLib，含 vdso 近似）与栈段（VmStk）字节。
fn task_segment_sizes(task: &Arc<Task>) -> (u64, u64, u64) {
    let Some(vm) = task_vm_space(task) else {
        return (0, 0, 0);
    };
    let mut first_exec = true;
    let mut exe = 0u64;
    let mut lib = 0u64;
    let mut stack = 0u64;
    for (range, flags) in dump_vmas(&vm) {
        let size = (range.end - range.start) as u64;
        if flags.has(VmFlags::GROWS_DOWN) {
            stack = stack.saturating_add(size);
        } else if flags.has(VmFlags::EXEC) {
            if first_exec {
                first_exec = false;
                exe = exe.saturating_add(size);
            } else {
                lib = lib.saturating_add(size);
            }
        }
    }
    (exe, lib, stack)
}

fn render_task_status(task: &Arc<Task>) -> String {
    let name = render_task_comm(task).trim_end().to_string();
    let state = task.state();
    let tgid = task_leader_pid(task).unwrap_or(task.pid_root().unwrap_or(0));
    let pid = task.pid_root().unwrap_or(0);
    let ppid = task_parent_pid(task);
    let creds = task.credentials();
    let fd_count = task_fdtable(task)
        .map(|fdt| fdt.snapshot_fds().len())
        .unwrap_or(0);
    let (vsize, rss, data) = task_memory_usage(task);
    let vm_locked_kb = task_vm_space(task)
        .map(|vm| vm.locked_pages() as u64 * page_size() as u64 / 1024)
        .unwrap_or(0);
    let cap_inh = creds.cap_inheritable.raw() & LINUX_CAP_VALID_MASK;
    let cap_prm = creds.cap_permitted.raw() & LINUX_CAP_VALID_MASK;
    let cap_eff = creds.caps.raw() & LINUX_CAP_VALID_MASK;
    let cap_bnd = creds.cap_bset.raw() & LINUX_CAP_VALID_MASK;
    let seccomp = task
        .ext_lookup(crate::syscall::TASKEXT_SECCOMP)
        .and_then(|payload| payload.downcast::<crate::seccomp::SeccompState>().ok())
        .map(|state| state.mode())
        .unwrap_or(0);
    let (sigpnd, shdpnd, sigblk, sigign, sigcgt) = task_signal_views(task);
    let usage = task.usage_snapshot(sched::now_ns_public());
    let (exe, lib, stack) = task_segment_sizes(task);
    let cpu_mask = sched::online_cpu_mask();
    format!(
        "Name:\t{}\nState:\t{} ({})\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nUid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nFDSize:\t{}\nVmSize:\t{} kB\nVmRSS:\t{} kB\nVmPeak:\t{} kB\nVmExe:\t{} kB\nVmLib:\t{} kB\nVmPTE:\t{} kB\nVmSwap:\t{} kB\nVmData:\t{} kB\nVmStk:\t{} kB\nVmLck:\t{} kB\nVmHWM:\t{} kB\nThreads:\t{}\nSigPnd:\t{:016x}\nShdPnd:\t{:016x}\nSigBlk:\t{:016x}\nSigIgn:\t{:016x}\nSigCgt:\t{:016x}\nCapInh:\t{:016x}\nCapPrm:\t{:016x}\nCapEff:\t{:016x}\nCapBnd:\t{:016x}\nCpus_allowed:\t{:x}\nCpus_allowed_list:\t{}\nMems_allowed:\t1\nMems_allowed_list:\t0\nvoluntary_ctxt_switches:\t{}\nnonvoluntary_ctxt_switches:\t{}\nNoNewPrivs:\t{}\nSeccomp:\t{}\nCoreDumping:\t0\n",
        name,
        task_state_char(state),
        task_state_name(state),
        tgid,
        pid,
        ppid,
        creds.uid.0,
        creds.euid.0,
        creds.suid.0,
        creds.fsuid.0,
        creds.gid.0,
        creds.egid.0,
        creds.sgid.0,
        creds.fsgid.0,
        fd_count,
        vsize / 1024,
        rss / 1024,
        rss / 1024,
        exe / 1024,
        lib / 1024,
        0u64,
        0u64,
        data / 1024,
        stack / 1024,
        vm_locked_kb,
        rss / 1024,
        task_thread_count(task),
        sigpnd,
        shdpnd,
        sigblk,
        sigign,
        sigcgt,
        cap_inh,
        cap_prm,
        cap_eff,
        cap_bnd,
        cpu_mask,
        online_cpu_list(),
        usage.voluntary_ctxt_switches,
        usage.involuntary_ctxt_switches,
        task.no_new_privs() as usize,
        seccomp,
    )
}

const LINUX_CAP_VALID_MASK: u64 = (1u64 << 41) - 1;

fn render_task_stat(task: &Arc<Task>) -> String {
    use alloc::fmt::Write;
    let pid = task.pid_root().unwrap_or(0);
    let comm = render_task_comm(task).trim_end().to_string();
    let state = task_state_char(task.state());
    let ppid = task_parent_pid(task);
    let pgrp = task_pgrp(task);
    let session = task_session(task);
    let num_threads = task_thread_count(task);
    let (vsize, rss_bytes, _) = task_memory_usage(task);
    let rss_pages = rss_bytes / page_size() as u64;
    let usage = task.usage_snapshot(sched::now_ns_public());
    let child_usage = task.child_usage_snapshot();
    let utime = proc_cpu_ticks(usage.user_ns);
    let stime = proc_cpu_ticks(usage.system_ns);
    let cutime = proc_cpu_ticks(child_usage.user_ns);
    let cstime = proc_cpu_ticks(child_usage.system_ns);
    let starttime = proc_cpu_ticks(task.start_time_ns());
    let nice = task.sched.nice();
    let priority = (20 - nice).clamp(0, 39);
    let rt_priority = task.sched.rt_priority();
    let policy = sched_policy_linux_id(task.sched.policy());
    let (sigpnd, _shdpnd, sigblk, sigign, sigcgt) = task_signal_views(task);
    let rsslim = task
        .thread_group()
        .rlimits()
        .lock()
        .get(sched::Resource::Rss)
        .soft
        .raw();
    let mut out = String::new();
    let _ = write!(
        out,
        "{pid} ({comm}) {state} {ppid} {pgrp} {session} 0 0 0 {} {} {} {} {utime} {stime} {cutime} {cstime} {priority} {nice} {num_threads} 0 {starttime} {vsize} {rss_pages} {rsslim} 0 0 0 0 0 {sigpnd} {sigblk} {sigign} {sigcgt} 0 0 0 0 0 {rt_priority} {policy} 0 0 0 0 0 0 0 0 0 0 0 0\n",
        usage.minflt, child_usage.minflt, usage.majflt, child_usage.majflt,
    );
    out
}

fn proc_cpu_ticks(ns: u64) -> u64 {
    const USER_HZ: u64 = 100;
    ns / (1_000_000_000 / USER_HZ)
}

fn render_task_cmdline(task: &Arc<Task>) -> Vec<u8> {
    let args = match task_exec_args(task) {
        Some(args) if !args.is_empty() => args,
        _ => task_exec_path(task)
            .map(|path| vec![path])
            .unwrap_or_default(),
    };
    let mut out = Vec::new();
    for arg in args {
        out.extend_from_slice(arg.as_bytes());
        out.push(0);
    }
    out
}

fn render_task_environ(task: &Arc<Task>) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in task_exec_envp(task).unwrap_or_default() {
        out.extend_from_slice(entry.as_bytes());
        out.push(0);
    }
    out
}

fn render_task_comm(task: &Arc<Task>) -> String {
    let comm = task.comm();
    let len = comm.iter().position(|b| *b == 0).unwrap_or(comm.len());
    let name = core::str::from_utf8(&comm[..len]).unwrap_or("unknown");
    format!("{}\n", name)
}

fn render_task_maps(task: &Arc<Task>) -> String {
    let Some(vm) = task_vm_space(task) else {
        return String::new();
    };
    let exec_path = task_exec_path(task).ok();
    let mut first_exec = true;
    let mut out = String::new();
    for (range, flags) in dump_vmas(&vm) {
        vma_maps_header(
            &mut out,
            exec_path.as_deref(),
            &mut first_exec,
            &range,
            flags,
        );
    }
    out
}

fn vm_flags_to_maps_perms(flags: VmFlags) -> String {
    let mut out = [b'-'; 4];
    if flags.has(VmFlags::READ) {
        out[0] = b'r';
    }
    if flags.has(VmFlags::WRITE) {
        out[1] = b'w';
    }
    if flags.has(VmFlags::EXEC) {
        out[2] = b'x';
    }
    out[3] = if flags.has(VmFlags::SHARED) {
        b's'
    } else {
        b'p'
    };
    String::from_utf8(out.to_vec()).unwrap_or_else(|_| String::from("----"))
}

fn render_task_mountinfo(task: &Arc<Task>) -> VfsResult<String> {
    let ctx = task_vfs_context(task).ok_or(VfsError::NotFound)?;
    Ok(ctx
        .mount_ns
        .dump_mountinfo(&ctx.root.root(), &ctx.root.mount()))
}

fn render_task_mounts(task: &Arc<Task>) -> VfsResult<String> {
    let ctx = task_vfs_context(task).ok_or(VfsError::NotFound)?;
    Ok(ctx.mount_ns.dump_mounts())
}

fn render_hotplug() -> String {
    let value = HOTPLUG_PATH.lock();
    if value.is_empty() {
        String::new()
    } else {
        format!("{}\n", &*value)
    }
}

fn render_file_max() -> String {
    format!("{}\n", FILE_MAX.load(Ordering::Relaxed))
}

fn render_filesystems() -> String {
    let mut out = String::new();
    for entry in FS_REGISTRY.iter() {
        if entry.driver.flags().has(FsDriverFlags::NODEV) {
            out.push_str("nodev\t");
        } else {
            out.push('\t');
        }
        out.push_str(entry.driver.name());
        out.push('\n');
    }
    out
}

fn render_mounts() -> String {
    current_vfs_context()
        .map(|ctx| ctx.mount_ns.dump_mounts())
        .unwrap_or_default()
}

fn render_mountinfo_root() -> String {
    current_vfs_context()
        .map(|ctx| {
            ctx.mount_ns
                .dump_mountinfo(&ctx.root.root(), &ctx.root.mount())
        })
        .unwrap_or_default()
}

fn render_version() -> String {
    // 按目标架构报告，避免 RISC-V 构建也输出 loongarch64。
    let arch = if cfg!(target_arch = "loongarch64") {
        "loongarch64"
    } else if cfg!(target_arch = "riscv64") {
        "riscv64"
    } else {
        "unknown"
    };
    format!("MyGo kernel version 0.1.0 ({arch})\n")
}

fn cpuinfo_model_from_compatible(compatible: &str) -> &str {
    compatible
        .split_once(',')
        .map(|(_, model)| model)
        .unwrap_or(compatible)
}

fn cpuinfo_vendor_from_compatible(compatible: &str) -> &str {
    compatible
        .split_once(',')
        .map(|(vendor, _)| vendor)
        .unwrap_or("unknown")
}

fn cpuinfo_family_from_compatible(compatible: &str) -> &str {
    let vendor = cpuinfo_vendor_from_compatible(compatible);
    if vendor.eq_ignore_ascii_case("loongarch") {
        "LoongArch"
    } else {
        vendor
    }
}

fn render_cpuinfo_entry(out: &mut String, cpu_id: usize, compatible: Option<&str>) {
    let vendor = compatible
        .map(cpuinfo_vendor_from_compatible)
        .unwrap_or("unknown");
    let family = compatible
        .map(cpuinfo_family_from_compatible)
        .unwrap_or("unknown");
    let model = compatible
        .map(cpuinfo_model_from_compatible)
        .unwrap_or("unknown");
    let isa = compatible
        .and_then(|text| text.split_once(',').map(|(isa, _)| isa))
        .unwrap_or("unknown");

    // BogoMIPS 需要架构层提供定标后的循环/延迟校准值。当前公共 CPU 拓扑只包含
    // 固件身份，因此该兼容字段明确报告 0.00，而不是伪造固定性能数字。
    let _ = write!(
        out,
        "processor\t: {cpu_id}\n\
         vendor_id\t: {vendor}\n\
         cpu family\t: {family}\n\
         model name\t: {model}\n\
         CPU architecture\t: {isa}\n\
         isa\t\t: {isa}\n\
         fpu\t\t: unknown\n\
         BogoMIPS\t: 0.00\n\n"
    );
}

fn render_cpuinfo() -> String {
    let mut out = String::new();
    let mut online_mask = sched::online_cpu_mask();
    if online_mask == 0 {
        online_mask = 1;
    }
    let topology = crate::dev::cpu::snapshot_topology();
    for cpu_id in 0..sched::NR_CPUS {
        if (online_mask & (1u64 << cpu_id)) == 0 || !sched::is_cpu_online(cpu_id) {
            continue;
        }
        let compatible = topology
            .iter()
            .find(|entry| entry.logical_id as usize == cpu_id)
            .and_then(|entry| entry.compatible.first())
            .map(|text| text.as_ref());
        render_cpuinfo_entry(&mut out, cpu_id, compatible);
    }
    if out.is_empty() {
        let compatible = topology
            .iter()
            .find(|entry| entry.logical_id == 0)
            .and_then(|entry| entry.compatible.first())
            .map(|text| text.as_ref());
        render_cpuinfo_entry(&mut out, 0, compatible);
    }
    out
}

fn read_meminfo_at(buf: &mut [u8], offset: u64) -> VfsResult<usize> {
    let mut content = [0u8; 8192];
    let len = render_meminfo_into(&mut content);
    slice_bytes(buf, offset, &content[..len])
}

fn render_meminfo_into(buf: &mut [u8]) -> usize {
    let overview = allocator::KERNEL_ALLOCATOR.detailed_stats();
    let layers = allocator::KERNEL_ALLOCATOR.layer_stats();
    let sched_diag = sched::scheduler_diag();
    let task_diag = sched::task_diag();
    let vm_diag = crate::mm::vm_space::vm_space_diag();
    let private_file_cache_diag = crate::mm::vm_space::private_file_page_cache_diag();
    let fault_around_diag = crate::mm::vm_space::fault_around_diag();
    let anon_fault_around_diag = crate::mm::vm_space::anon_fault_around_diag();
    #[cfg(feature = "performance-profile")]
    let hardware_fault_diag = crate::mm::vm_space::hardware_fault_diag();
    let anon_store_shadow_diag = crate::mm::vm_space::anon_store_shadow_diag();
    let file_diag = vfs::file::file_diag();
    let fdtable_diag = vfs::fdtable::fdtable_diag();
    let vfs_context_diag = vfs::vfs_context_diag();
    let slab_classes = allocator::KERNEL_ALLOCATOR.slab_class_stats();
    let dead_comm_len = task_diag
        .dead_ref_sample_comm
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(task_diag.dead_ref_sample_comm.len());
    let dead_comm =
        core::str::from_utf8(&task_diag.dead_ref_sample_comm[..dead_comm_len]).unwrap_or("?");
    let kb = |bytes: usize| -> usize { bytes / 1024 };
    let slab_empty_pages = slab_classes
        .iter()
        .fold(0usize, |sum, class| sum.saturating_add(class.empty_pages));
    let slab_reclaimable_pages = slab_classes.iter().fold(0usize, |sum, class| {
        sum.saturating_add(class.reclaimable_empty_pages)
    });
    let slab_reclaimable_bytes = slab_reclaimable_pages.saturating_mul(page_size());
    let allocator_reclaimable = layers
        .kheap
        .cached_bytes
        .saturating_add(slab_reclaimable_bytes);
    let mem_available = overview.free_physical.saturating_add(allocator_reclaimable);
    let slab_bytes = layers.slab.active_pages.saturating_mul(page_size());
    let (swap_total_pages, swap_free_pages) = crate::mm::swap::swap_totals();
    let page_size_kb = page_size() / 1024;
    let swap_total_kb = (swap_total_pages * page_size_kb as u64) as usize;
    let swap_free_kb = (swap_free_pages * page_size_kb as u64) as usize;
    let anon_pages = crate::mm::memstat::ANON_PAGES.load(core::sync::atomic::Ordering::Relaxed);
    let shared_anon_pages =
        crate::mm::memstat::SHARED_ANON_PAGES.load(core::sync::atomic::Ordering::Relaxed);
    let private_file_pages =
        crate::mm::memstat::PRIVATE_FILE_PAGES.load(core::sync::atomic::Ordering::Relaxed);
    let shared_file_pages =
        crate::mm::memstat::SHARED_FILE_PAGES.load(core::sync::atomic::Ordering::Relaxed);
    let locked_pages = crate::mm::memstat::locked_pages();
    let kb_pages = |pages: u64| (pages * page_size_kb as u64) as usize;
    // Linux 口径: Cached = 文件页缓存(私有文件缓存 + 共享文件驻留页);
    // Shmem = 共享匿名; Mapped = 全部驻留用户页; Mlocked = 锁页总数。
    let cached_kb = kb_pages(private_file_pages.saturating_add(shared_file_pages));
    let shmem_kb = kb_pages(shared_anon_pages);
    let mapped_kb = kb_pages(
        anon_pages
            .saturating_add(shared_anon_pages)
            .saturating_add(private_file_pages)
            .saturating_add(shared_file_pages),
    );
    let mlocked_kb = kb_pages(locked_pages);
    let committed_as_kb = kb_pages(crate::mm::memstat::committed_pages());
    let commit_limit_kb = crate::mm::memstat::commit_limit_kb(
        (overview.total_physical / allocator::PAGE_SIZE) as u64,
        swap_total_pages,
    );
    let mut out = FixedBuf::new(buf);
    let _ = write!(
        out,
        "MemTotal:       {:>8} kB\n\
         MemFree:        {:>8} kB\n\
         MemAvailable:   {:>8} kB\n\
         Buffers:        {:>8} kB\n\
         Cached:         {:>8} kB\n\
         SwapCached:     {:>8} kB\n\
         Slab:           {:>8} kB\n\
         KernelStack:    {:>8} kB\n\
         PageTables:     {:>8} kB\n\
         VmallocTotal:   {:>8} kB\n\
         VmallocUsed:    {:>8} kB\n\
         VmallocChunk:   {:>8} kB\n\
         SwapTotal:      {:>8} kB\n\
         SwapFree:       {:>8} kB\n\
         DirectMapTotal: {:>8} kB\n\
         DirectMapUsed:  {:>8} kB\n\
         DirectMapFree:  {:>8} kB\n\
         MemReserved:    {:>8} kB\n\
         KernelHeap:     {:>8} kB\n\
         BootUsed:       {:>8} kB\n\
         BootFree:       {:>8} kB\n\
         AllocRegLive:   {:>8}\n\
         AllocRegNodes:  {:>8}\n\
         AllocRegFree:   {:>8}\n\
         AllocSmallLive: {:>8}\n\
         AllocLargeLive: {:>8}\n\
         AllocPhysLive:  {:>8}\n\
         SlabObjects:    {:>8}\n\
         SlabActive:     {:>8} kB\n\
         SlabPages:      {:>8}\n\
         SlabFreeNodes:  {:>8}\n\
         KHeapObjects:   {:>8}\n\
         KHeapActive:    {:>8} kB\n\
         KHeapCached:    {:>8} kB\n\
         AllocReclaimable:{:>7} kB\n\
         SlabEmpty:      {:>8} kB\n\
         SlabReclaimable:{:>7} kB\n\
         MetaBacking:    {:>8} kB\n\
         SchedPidCount:  {:>8}\n\
         SchedCurSlots:  {:>8}\n\
         SchedCurDead:   {:>8}\n\
         SchedRqCur:     {:>8}\n\
         SchedRqCurDead: {:>8}\n\
         SchedRqQueued:  {:>8}\n\
         SchedRqQDead:   {:>8}\n\
         SchedRetired:   {:>8}\n\
         InitChildren:   {:>8}\n\
         InitZombies:    {:>8}\n\
         TaskLive:       {:>8}\n\
         TaskCreated:    {:>8}\n\
         TaskDropped:    {:>8}\n\
         TaskTracked:    {:>8}\n\
         TaskZombie:     {:>8}\n\
         TaskDead:       {:>8}\n\
         TaskPidless:    {:>8}\n\
         TaskChildLinks: {:>8}\n\
         TaskDeadChild:  {:>8}\n\
         TaskMaxRefs:    {:>8}\n\
         TaskDeadRefs:   {:>8}\n\
         TaskDeadPid:    {:>8}\n\
         TaskDeadPPid:   {:>8}\n\
         TaskDeadRefMax: {:>8}\n\
         TaskDeadOnRq:   {:>8}\n\
         TaskDeadCtx:    {:>8}\n\
         TaskDeadKStack: {:>8}\n\
         TaskDeadExts:   {:>8}\n\
         TaskDeadComm:   {}\n\
         TaskSigPending: {:>8}\n\
         TaskMaxSigPend: {:>8}\n\
         DcacheEntries:  {:>8}\n\
         FileLive:       {:>8}\n\
         FileCreated:    {:>8}\n\
         FileDropped:    {:>8}\n\
         FdTableLive:    {:>8}\n\
         FdTableCreated: {:>8}\n\
         FdTableDropped: {:>8}\n\
         VfsCtxLive:     {:>8}\n\
         VfsCtxCreated:  {:>8}\n\
         VfsCtxDropped:  {:>8}\n\
         VmSpaceLive:    {:>8}\n\
         VmSpaceCreated: {:>8}\n\
         VmSpaceDropped: {:>8}\n\
         PrivateFileCache:{:>8} kB\n\
         PrivateCachePages:{:>7}\n\
         PrivateCacheLimit:{:>7}\n\
         PrivateCacheHits:  {:>7}\n\
         PrivateCacheMisses:{:>6}\n\
         PrivateCacheEvict:{:>7}\n\
         PrivateCachePressureDrops:{:>3}\n\
         PrivateCacheLoadLeaders:{:>6}\n\
         PrivateCacheLoadWaiters:{:>6}\n\
         PrivateCacheLoadErrors:{:>7}\n\
         FaultAroundWindows:{:>8}\n\
         FaultAroundRequested:{:>6}\n\
         FaultAroundPrepared:{:>7}\n\
         FaultAroundCommits:{:>8}\n\
         FaultAroundInstalled:{:>6}\n\
         FaultAroundRaced:{:>10}\n\
         FaultAroundCollisionWindows:{:>1}\n\
         FaultAroundDuplicatePages:{:>4}\n\
         FaultAroundDiscardedUnmapped:{:>1}\n\
         FaultAroundVmaRetryPages:{:>3}\n\
         FaultAroundRacedPages:{:>7}\n\
         FaultAroundMapFailedPages:{:>3}\n\
         AnonFaultWindows:     {:>8}\n\
         AnonFaultRequested:   {:>8}\n\
         AnonFaultPrepared:    {:>8}\n\
         AnonFaultAllocShort:  {:>8}\n\
         AnonFaultReserveFallback:{:>4}\n\
         AnonFaultVmaRetryPages:{:>6}\n\
         AnonFaultRacedPages:  {:>8}\n\
         AnonFaultInvariantPages:{:>6}\n\
         AnonFaultCollisionDiscard:{:>3}\n\
         AnonFaultMapDiscard:  {:>8}\n\
         AnonFaultInstalled:   {:>8}\n\
         AnonFaultCommits:     {:>8}\n\
         AnonFaultPartial:     {:>8}\n\
         AnonFaultMapFailures: {:>8}\n\
         AnonStoreShadowFaults:{:>6}\n\
         AnonStoreShadowBatches:{:>5}\n\
         AnonStoreShadowWouldSave:{:>3}\n\
         AnonStoreShadowResets:{:>7}\n",
        kb(overview.total_physical),
        kb(overview.free_physical),
        kb(mem_available),
        0usize, // Buffers：无块设备缓冲
        cached_kb,
        0usize, // SwapCached：无换出
        kb(slab_bytes),
        0usize, // KernelStack
        0usize, // PageTables
        kb(overview.kernel_vmem_total),
        kb(overview.kernel_vmem_allocated),
        kb(overview.kernel_vmem_free),
        swap_total_kb,
        swap_free_kb,
        kb(overview.direct_map_total),
        kb(overview.direct_map_allocated),
        kb(overview.direct_map_free),
        kb(overview.reserved_physical),
        kb(overview.kernel_heap_used),
        kb(overview.boot_used),
        kb(overview.boot_free),
        layers.registry.live_records,
        layers.registry.nodes_allocated,
        layers.registry.free_nodes,
        layers.registry.live_small,
        layers.registry.live_large,
        layers.registry.live_physical,
        layers.slab.active_objects,
        kb(layers.slab.active_bytes),
        layers.slab.active_pages,
        layers.slab.free_slab_nodes,
        layers.kheap.active_allocs,
        kb(layers.kheap.active_bytes),
        kb(layers.kheap.cached_bytes),
        kb(allocator_reclaimable),
        kb(slab_empty_pages.saturating_mul(page_size())),
        kb(slab_reclaimable_bytes),
        kb(layers.metadata.backing_pages.saturating_mul(page_size())),
        sched_diag.pid_count,
        sched_diag.current_slots,
        sched_diag.current_zombie_or_dead,
        sched_diag.rq_current_slots,
        sched_diag.rq_current_zombie_or_dead,
        sched_diag.rq_queued_slots,
        sched_diag.rq_queued_zombie_or_dead,
        sched_diag.retired_tasks,
        sched_diag.init_children,
        sched_diag.init_zombies,
        task_diag.live,
        task_diag.created,
        task_diag.dropped,
        task_diag.tracked_alive,
        task_diag.zombie,
        task_diag.dead,
        task_diag.pidless,
        task_diag.child_links,
        task_diag.dead_child_links,
        task_diag.max_external_refs,
        task_diag.dead_external_refs,
        task_diag.dead_ref_sample_pid,
        task_diag.dead_ref_sample_parent_pid,
        task_diag.dead_ref_sample_refs,
        usize::from(task_diag.dead_ref_sample_on_rq),
        usize::from(task_diag.dead_ref_sample_has_ctx),
        usize::from(task_diag.dead_ref_sample_has_kstack),
        task_diag.dead_ref_sample_exts,
        dead_comm,
        task_diag.shared_pending_infos,
        task_diag.max_shared_pending_infos,
        vfs::DCACHE.len(),
        file_diag.live,
        file_diag.created,
        file_diag.dropped,
        fdtable_diag.live,
        fdtable_diag.created,
        fdtable_diag.dropped,
        vfs_context_diag.live,
        vfs_context_diag.created,
        vfs_context_diag.dropped,
        vm_diag.live,
        vm_diag.created,
        vm_diag.dropped,
        kb(private_file_cache_diag.pages.saturating_mul(page_size()),),
        private_file_cache_diag.pages,
        private_file_cache_diag.capacity,
        private_file_cache_diag.hits,
        private_file_cache_diag.misses,
        private_file_cache_diag.evictions,
        private_file_cache_diag.pressure_reclaims,
        private_file_cache_diag.load_leaders,
        private_file_cache_diag.load_waiters,
        private_file_cache_diag.load_errors,
        fault_around_diag.windows,
        fault_around_diag.requested_pages,
        fault_around_diag.prepared_pages,
        fault_around_diag.commits,
        fault_around_diag.installed_pages,
        fault_around_diag.raced_commits,
        fault_around_diag.collision_windows,
        fault_around_diag.duplicate_pages,
        fault_around_diag.discarded_unmapped_pages,
        fault_around_diag.vma_retry_pages,
        fault_around_diag.raced_pages,
        fault_around_diag.map_failed_pages,
        anon_fault_around_diag.windows,
        anon_fault_around_diag.requested_pages,
        anon_fault_around_diag.prepared_pages,
        anon_fault_around_diag.allocation_shortfall_pages,
        anon_fault_around_diag.reserve_fallbacks,
        anon_fault_around_diag.vma_retry_pages,
        anon_fault_around_diag.raced_pages,
        anon_fault_around_diag.invariant_failure_pages,
        anon_fault_around_diag.collision_discarded_pages,
        anon_fault_around_diag.map_discarded_pages,
        anon_fault_around_diag.installed_pages,
        anon_fault_around_diag.commits,
        anon_fault_around_diag.partial_commits,
        anon_fault_around_diag.map_failures,
        anon_store_shadow_diag.faults,
        anon_store_shadow_diag.simulated_batches,
        anon_store_shadow_diag.would_save,
        anon_store_shadow_diag.migration_interleave_resets,
    );
    let _ = write!(
        out,
        "AnonPages:      {:>8} kB\n\
         Mapped:         {:>8} kB\n\
         Shmem:          {:>8} kB\n\
         Active:         {:>8} kB\n\
         Inactive:       {:>8} kB\n\
         Mlocked:        {:>8} kB\n\
         Unevictable:    {:>8} kB\n\
         Dirty:          {:>8} kB\n\
         Writeback:      {:>8} kB\n\
         NFS_Unstable:   {:>8} kB\n\
         Bounce:         {:>8} kB\n\
         WritebackTmp:   {:>8} kB\n\
         CommitLimit:    {:>8} kB\n\
         Committed_AS:   {:>8} kB\n\
         KReclaimable:   {:>8} kB\n\
         SReclaimable:   {:>8} kB\n\
         SUnreclaim:     {:>8} kB\n\
         AnonHugePages:  {:>8} kB\n\
         ShmemHugePages: {:>8} kB\n\
         ShmemPmdMapped: {:>8} kB\n\
         FileHugePages:  {:>8} kB\n\
         FilePmdMapped:  {:>8} kB\n\
         HugePages_Total:{:>8}\n\
         HugePages_Free: {:>8}\n\
         HugePages_Rsvd: {:>8}\n\
         HugePages_Surp: {:>8}\n\
         Hugepagesize:   {:>8} kB\n\
         Hugetlb:        {:>8} kB\n",
        kb_pages(anon_pages),
        mapped_kb,
        shmem_kb,
        0usize, // Active：无 LRU 统计
        0usize, // Inactive
        mlocked_kb,
        mlocked_kb, // Unevictable = Mlocked
        0usize,     // Dirty：无回写统计
        0usize,     // Writeback
        0usize,     // NFS_Unstable
        0usize,     // Bounce
        0usize,     // WritebackTmp
        commit_limit_kb as usize,
        committed_as_kb,
        kb(slab_reclaimable_bytes),
        kb(slab_reclaimable_bytes), // SReclaimable
        kb(slab_bytes.saturating_sub(slab_reclaimable_bytes)), // SUnreclaim
        0usize,                     // AnonHugePages：无 THP
        0usize,                     // ShmemHugePages
        0usize,                     // ShmemPmdMapped
        0usize,                     // FileHugePages
        0usize,                     // FilePmdMapped
        0usize,                     // HugePages_Total
        0usize,                     // HugePages_Free
        0usize,                     // HugePages_Rsvd
        0usize,                     // HugePages_Surp
        2048usize,                  // Hugepagesize（默认 2 MiB，仅呈现）
        0usize,                     // Hugetlb
    );
    #[cfg(feature = "performance-profile")]
    {
        let traps = profiling::loongarch_user_trap_snapshot();
        let _ = write!(
            out,
            "ProfileLaUserSyscalls: {:>8}\n\
             ProfileLaUserOtherTraps:{:>8}\n\
             ProfileLaSysFpuSaved:  {:>8}\n\
             ProfileLaSysLsxSaved:  {:>8}\n\
             ProfileLaOtherFpuSaved:{:>8}\n\
             ProfileLaOtherLsxSaved:{:>8}\n",
            traps.user_syscalls,
            traps.user_other_traps,
            traps.syscall_fpu_saved,
            traps.syscall_lsx_saved,
            traps.other_fpu_saved,
            traps.other_lsx_saved,
        );
        for backing in crate::mm::vm_space::HardwareFaultBacking::ALL {
            for access in crate::mm::vm_space::HardwareFaultAccess::ALL {
                let nonresident = hardware_fault_diag.count(backing, access, false);
                let resident = hardware_fault_diag.count(backing, access, true);
                let _ = writeln!(
                    out,
                    "HwUserFault{}{}: {:>8} resident {:>8}",
                    backing.name(),
                    access.name(),
                    nonresident.saturating_add(resident),
                    resident,
                );
            }
        }
    }
    for class in slab_classes {
        let _ = write!(
            out,
            "Slab{}:         {:>8} objs {:>8} kB {:>8} pages\n",
            class.size_class,
            class.active_objects,
            kb(class.active_bytes),
            class.active_pages,
        );
        let _ = write!(
            out,
            "Slab{}Empty:    {:>8} slabs {:>8} kB {:>8} reclaim_kB\n",
            class.size_class,
            class.empty_slabs,
            kb(class.empty_pages.saturating_mul(page_size())),
            kb(class.reclaimable_empty_pages.saturating_mul(page_size())),
        );
    }
    out.len()
}

fn render_meminfo() -> String {
    let mut buf = [0u8; 8192];
    let len = render_meminfo_into(&mut buf);
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

struct FixedBuf<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> FixedBuf<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl core::fmt::Write for FixedBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let remaining = self.buf.len().saturating_sub(self.len);
        let bytes = s.as_bytes();
        let copy_len = remaining.min(bytes.len());
        if copy_len != 0 {
            self.buf[self.len..self.len + copy_len].copy_from_slice(&bytes[..copy_len]);
            self.len += copy_len;
        }
        Ok(())
    }
}

/// `/proc/swaps`：swap 设备表视图（Size/Used 以 KiB 计，与 Linux 一致）。
fn render_swaps() -> String {
    let mut out = String::from("Filename\t\t\t\tType\t\tSize\tUsed\tPriority\n");
    for entry in crate::mm::swap::swap_entries() {
        let page_size_kb = page_size() / 1024;
        let size_kb = entry.size_pages * page_size_kb as u64;
        let used_kb = entry.used_pages * page_size_kb as u64;
        let _ = write!(
            out,
            "{}\t\t\t\t{}\t\t{}\t{}\t{}\n",
            entry.name,
            entry.kind.as_str(),
            size_kb,
            used_kb,
            entry.priority
        );
    }
    out
}

fn render_uptime() -> String {
    let ns = sched::now_ns_public();
    let secs = ns / 1_000_000_000;
    // 第二个字段是累计 idle 时间。调度器尚未导出 per-CPU idle accounting，
    // 因此只报告可证明的系统运行时间，idle 兼容字段保持 0。
    format!(
        "{}.{:02} {}.{:02}\n",
        secs,
        (ns % 1_000_000_000) / 10_000_000,
        0u64,
        0u64
    )
}

fn render_stat() -> String {
    use alloc::fmt::Write;
    let mut processes = 0usize;
    let mut running = 0usize;
    let mut blocked = 0usize;
    let mut ctxt = 0u64;
    let mut online = sched::online_cpu_mask();
    if online == 0 {
        online = 1;
    }
    if sched::is_ready() {
        for (_, weak) in sched::root_pid_ns().registry().snapshot() {
            let Some(task) = weak.upgrade() else {
                continue;
            };
            processes += 1;
            match task.state() {
                TaskState::New
                | TaskState::Runnable
                | TaskState::Running
                | TaskState::Continued => {
                    running += 1;
                }
                TaskState::Uninterruptible => blocked += 1,
                _ => {}
            }
            let usage = task.usage_snapshot(sched::now_ns_public());
            ctxt = ctxt.saturating_add(usage.voluntary_ctxt_switches);
            ctxt = ctxt.saturating_add(usage.involuntary_ctxt_switches);
        }
    }
    // CPU jiffies / 中断计数 / btime 无公共快照接口，保持 0；ctxt 用每任务切换计数聚合。
    let mut out = String::new();
    let _ = writeln!(out, "cpu  0 0 0 0 0 0 0 0 0 0");
    for cpu in 0..sched::NR_CPUS {
        if online & (1u64 << cpu) != 0 {
            let _ = writeln!(out, "cpu{cpu} 0 0 0 0 0 0 0 0 0 0");
        }
    }
    let _ = writeln!(
        out,
        "intr 0\nctxt {ctxt}\nbtime 0\nprocesses {processes}\nprocs_running {running}\nprocs_blocked {blocked}\n"
    );
    out
}

fn render_interrupts() -> String {
    let mut online_mask = sched::online_cpu_mask();
    if online_mask == 0 {
        online_mask = 1;
    }
    let mut out = String::new();
    out.push_str("           ");
    for cpu in 0..sched::NR_CPUS {
        if online_mask & (1u64 << cpu) != 0 {
            let _ = write!(out, "CPU{cpu:>7}");
        }
    }
    out.push('\n');

    let timer_counts = crate::dev::irq::timer_interrupt_counts();
    let _ = write!(out, "  0:");
    for cpu in 0..sched::NR_CPUS {
        if online_mask & (1u64 << cpu) != 0 {
            let _ = write!(out, " {:>10}", timer_counts[cpu]);
        }
    }
    out.push_str("  timer\n");

    for entry in crate::dev::irq::snapshot_irq_lines() {
        let _ = write!(out, "{:>3}:", entry.proc_irq);
        for cpu in 0..sched::NR_CPUS {
            if online_mask & (1u64 << cpu) != 0 {
                let _ = write!(out, " {:>10}", entry.counts[cpu]);
            }
        }
        if entry.owners.is_empty() {
            let _ = write!(out, "  {:?}", entry.line);
        } else {
            out.push_str("  ");
            for (index, owner) in entry.owners.iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                out.push_str(owner);
            }
        }
        out.push('\n');
    }
    out
}

fn render_devices() -> String {
    // /proc/devices 只导出用户 ABI 设备号投影的 major 汇总，不表示底层设备模型的寻址入口。
    // major 名称来自 VFS 兼容层注册的 device number policy；procfs 只消费汇总快照，
    // 不读取底层设备对象，也不参与设备号分配。
    let mut out = String::new();
    if out.try_reserve("Character devices:\n".len()).is_err() {
        return out;
    }
    out.push_str("Character devices:\n");
    if let Some(summaries) = device_numbers::try_major_summaries(DeviceNumberKind::Char) {
        write_major_summaries(&mut out, &summaries);
    }
    if out.try_reserve("\nBlock devices:\n".len()).is_err() {
        return out;
    }
    out.push_str("\nBlock devices:\n");
    if let Some(summaries) = device_numbers::try_major_summaries(DeviceNumberKind::Block) {
        write_major_summaries(&mut out, &summaries);
    }
    out
}

fn write_major_summaries(out: &mut String, summaries: &[device_numbers::DeviceMajorSummary]) {
    for summary in summaries {
        // 设备诊断文本不能因为临时格式化缓冲分配失败而影响设备注册表本身。
        // 预留本行空间后再写入，避免为每一行创建额外 String。
        let line_reserve = summary.display_name.len().saturating_add(16);
        if out.try_reserve(line_reserve).is_err() {
            return;
        }
        let _ = writeln!(out, "  {} {}", summary.major, summary.display_name);
    }
}

fn render_device_functions() -> String {
    render_function_projection_diagnostics()
}

fn proc_pnp_state_name(state: PnpState) -> &'static str {
    match state {
        PnpState::Discovered => "discovered",
        PnpState::Probing => "probing",
        PnpState::Bound => "bound",
        PnpState::Removing => "removing",
        PnpState::Gone => "gone",
    }
}

fn proc_pnp_resource_kind_name(kind: PnpResourceKind) -> &'static str {
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

fn proc_pnp_dependency_render_len(dependency: PnpDependency) -> usize {
    match dependency {
        PnpDependency::IrqController(_) => "irq-controller:".len() + 10,
        PnpDependency::DefaultIrqDomain => "default-irq-domain".len(),
        PnpDependency::MsiController(_) => "msi-controller:".len() + 10,
        PnpDependency::Syscon(_) => "syscon:".len() + 10,
        PnpDependency::FwCfg => "fwcfg".len(),
        PnpDependency::FirmwareBus => "firmware-bus".len(),
        PnpDependency::PciHostBridge(_) => "pci-host-bridge:".len() + 5,
        PnpDependency::Dma => "dma".len(),
        PnpDependency::DtbProvider { .. } => "dt-provider::".len() + 5 + 10,
        PnpDependency::Other(name) => name.len(),
    }
}

fn proc_pnp_id_render_len(id: &PnpId) -> usize {
    match id {
        PnpId::Pci { .. } => "pci:0000:00:00.".len() + 3,
        PnpId::Usb { interface, .. } => {
            let base = "usb:".len() + 3 + 1 + 3;
            if interface.is_some() {
                base + 1 + 3
            } else {
                base
            }
        }
        PnpId::Platform { name, identity } => {
            if let Some(path) = identity.firmware_path() {
                "platform:".len() + name.len() + 1 + path.len()
            } else {
                // 无固件路径时 Display 只输出 match/resource 计数；这里按最大十进制
                // 位数预留，避免诊断输出为了设备 id 再次扩容。
                "platform:".len() + name.len() + "[ids=,resources=]".len() + 20
            }
        }
        PnpId::Dynamic {
            contract, identity, ..
        } => "dynamic::"
            .len()
            .saturating_add(contract.len())
            .saturating_add(identity.len().saturating_mul(2))
            .saturating_add(32),
    }
}

fn write_proc_pnp_dependency(out: &mut String, dependency: PnpDependency) {
    match dependency {
        PnpDependency::IrqController(id) => {
            let _ = write!(out, "irq-controller:{id}");
        }
        PnpDependency::DefaultIrqDomain => out.push_str("default-irq-domain"),
        PnpDependency::MsiController(id) => {
            let _ = write!(out, "msi-controller:{id}");
        }
        PnpDependency::Syscon(id) => {
            let _ = write!(out, "syscon:{id}");
        }
        PnpDependency::FwCfg => out.push_str("fwcfg"),
        PnpDependency::FirmwareBus => out.push_str("firmware-bus"),
        PnpDependency::PciHostBridge(domain) => {
            let _ = write!(out, "pci-host-bridge:{domain}");
        }
        PnpDependency::Dma => out.push_str("dma"),
        PnpDependency::DtbProvider { kind, phandle } => {
            let _ = write!(out, "dt-provider:{kind}:{phandle}");
        }
        PnpDependency::Other(name) => out.push_str(name),
    }
}

struct ProcPnpSchema;

impl ProcPnpSchema {
    const HEADER: &'static str =
        "bus\tid\tname\tstate\tdriver\tfunctions\tresources\tdeferred_dependency\n";

    fn function_list_len(functions: &[Arc<dyn crate::dev::function::DeviceFunction>]) -> usize {
        functions
            .iter()
            .map(|func| func.class_id().as_str().len() + 1 + func.dev_name().len())
            .sum::<usize>()
            .saturating_add(functions.len().saturating_sub(1))
    }

    fn resource_list_len(resources: &[crate::dev::pnp::PnpOwnedResourceSnapshot]) -> usize {
        resources
            .iter()
            .map(|resource| {
                proc_pnp_resource_kind_name(resource.kind).len() + 1 + resource.label.len()
            })
            .sum::<usize>()
            .saturating_add(resources.len().saturating_sub(1))
    }

    fn write_functions(
        out: &mut String,
        functions: &[Arc<dyn crate::dev::function::DeviceFunction>],
    ) {
        for (idx, func) in functions.iter().enumerate() {
            if idx != 0 {
                out.push(',');
            }
            let _ = write!(out, "{}:{}", func.class_id().as_str(), func.dev_name());
        }
    }

    fn write_resources(out: &mut String, resources: &[crate::dev::pnp::PnpOwnedResourceSnapshot]) {
        for (idx, resource) in resources.iter().enumerate() {
            if idx != 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{}:{}",
                proc_pnp_resource_kind_name(resource.kind),
                resource.label
            );
        }
    }
}

fn render_pnp() -> String {
    // `/proc/pnp` 是面向诊断的 dev core 快照；设备寻址和层级关系以 sysfs 为准。
    let mut out = String::new();
    if out.try_reserve(ProcPnpSchema::HEADER.len()).is_err() {
        return out;
    }
    out.push_str(ProcPnpSchema::HEADER);
    for dev in PNP_DEVICES.try_list().unwrap_or_default() {
        let functions = dev.try_functions().unwrap_or_default();
        let resources = dev.try_owned_resources().unwrap_or_default();
        let deferred = dev.deferred_dependency();
        let driver = dev.bound_driver_name();
        // 诊断输出按设备逐行预留，避免构造 function/resource 的中间字符串列表。
        let functions_len = ProcPnpSchema::function_list_len(&functions);
        let resources_len = ProcPnpSchema::resource_list_len(&resources);
        let deferred_len = deferred.map(proc_pnp_dependency_render_len).unwrap_or(0);
        let line_reserve = dev
            .name
            .len()
            .saturating_add(proc_pnp_id_render_len(&dev.id))
            .saturating_add(dev.info.bus_name().len())
            .saturating_add(driver.as_deref().unwrap_or("-").len())
            .saturating_add(functions_len)
            .saturating_add(resources_len)
            .saturating_add(deferred_len)
            .saturating_add(128);
        if out.try_reserve(line_reserve).is_err() {
            return out;
        }
        let _ = write!(
            out,
            "{}\t{}\t{}\t{}\t{}\t",
            dev.info.bus_name(),
            dev.id,
            dev.name,
            proc_pnp_state_name(dev.state()),
            driver.as_deref().unwrap_or("-"),
        );
        ProcPnpSchema::write_functions(&mut out, &functions);
        out.push('\t');
        ProcPnpSchema::write_resources(&mut out, &resources);
        out.push('\t');
        if let Some(dependency) = deferred {
            write_proc_pnp_dependency(&mut out, dependency);
        }
        out.push('\n');
    }
    out
}

// ── /proc/<pid>/ns ───────────────────────────────────────────────────────────

/// `/proc/<pid>/ns` 目录 inode。
fn proc_ns_dir_inode(fs_id: FsId, weak_sb: &Weak<Superblock>, pid: PidT) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        proc_ns_dir_ino(pid),
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcNsDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
            pid,
        }),
    )
}

struct ProcNsDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
    pid: PidT,
}

impl ProcNsDirOps {
    fn ns_file_inode(&self, kind: ProcNsKind) -> Arc<Inode> {
        mk_inode(
            self.fs_id,
            &self.weak_sb,
            proc_ns_file_ino(self.pid, kind),
            FileType::Symlink,
            0o777,
            1,
            Arc::new(ProcNsFileOps {
                pid: self.pid,
                kind,
            }),
        )
    }

    fn ns_backing_inode(&self, namespace: Arc<dyn ns::Namespace>) -> Arc<Inode> {
        let ino = proc_ns_backing_ino(namespace.inum());
        mk_inode(
            self.fs_id,
            &self.weak_sb,
            ino,
            FileType::Regular,
            0o444,
            1,
            Arc::new(ProcNsBackingInodeOps { namespace }),
        )
    }
}

fn proc_ns_file_ino(pid: PidT, kind: ProcNsKind) -> u64 {
    PROC_NS_LINK_BASE + pid as u64 * ProcNsKind::ALL.len() as u64 + proc_ns_kind_slot(kind)
}

fn proc_ns_backing_ino(namespace_inum: u64) -> u64 {
    PROC_NS_BACKING_BASE + namespace_inum
}

const fn proc_ns_kind_slot(kind: ProcNsKind) -> u64 {
    match kind {
        ProcNsKind::Uts => 0,
        ProcNsKind::Ipc => 1,
        ProcNsKind::Time => 2,
        ProcNsKind::Cgroup => 3,
        ProcNsKind::Pid => 4,
        ProcNsKind::Mount => 5,
        ProcNsKind::User => 6,
        ProcNsKind::Net => 7,
    }
}

fn proc_ns_link_target(kind: ProcNsKind, namespace: &dyn ns::Namespace) -> String {
    format!("{}:[{}]", kind.name(), namespace.inum())
}

fn parse_proc_ns_link_target(name: &str) -> Option<(ProcNsKind, u64)> {
    for kind in ProcNsKind::ALL {
        let Some(encoded_inum) = name
            .strip_prefix(kind.name())
            .and_then(|suffix| suffix.strip_prefix(":["))
            .and_then(|suffix| suffix.strip_suffix(']'))
        else {
            continue;
        };
        return Some((kind, encoded_inum.parse().ok()?));
    }
    None
}

impl InodeOps for ProcNsDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        if name == "." || name == ".." {
            return Err(VfsError::NotFound);
        }
        let kind = ProcNsKind::ALL
            .iter()
            .find(|kind| kind.name() == name)
            .copied();
        if let Some(kind) = kind {
            return Ok(self.ns_file_inode(kind));
        }

        // Linux 的 namespace 条目是 magic link。通用 VFS 会把 readlink 文本继续
        // 当作路径解析，因此这里提供一个不参与 readdir 的 nsfs backing inode，
        // 既保留 Symlink ABI，也不破坏 open + setns 路径。
        let (kind, expected_inum) = parse_proc_ns_link_target(name).ok_or(VfsError::NotFound)?;
        let provider = super::nsfs::ns_provider().ok_or(VfsError::NotFound)?;
        let namespace = provider(self.pid, kind).ok_or(VfsError::NotFound)?;
        if namespace.inum() != expected_inum {
            return Err(VfsError::NotFound);
        }
        Ok(self.ns_backing_inode(namespace))
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let mut snapshot = Vec::new();
        snapshot.try_reserve(8).map_err(|_| VfsError::NoSpace)?;
        for kind in ProcNsKind::ALL {
            snapshot.push(DirEntry {
                ino: proc_ns_file_ino(self.pid, kind),
                name: SmallStr::new(kind.name()),
                kind: FileType::Symlink,
            });
        }
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// `/proc/<pid>/ns/<type>` magic link：readlink 显示命名空间标识，打开时经
/// 隐藏 backing inode 绑定具体命名空间。
struct ProcNsFileOps {
    pid: PidT,
    kind: ProcNsKind,
}

impl InodeOps for ProcNsFileOps {
    fn lookup(&self, _: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let provider = super::nsfs::ns_provider().ok_or(VfsError::NotFound)?;
        let namespace = provider(self.pid, self.kind).ok_or(VfsError::NotFound)?;
        Ok(Box::new(super::nsfs::NsfsFileOps::new(namespace)))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        let provider = super::nsfs::ns_provider().ok_or(VfsError::NotFound)?;
        let namespace = provider(self.pid, self.kind).ok_or(VfsError::NotFound)?;
        Ok(proc_ns_link_target(self.kind, namespace.as_ref()))
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcNsBackingInodeOps {
    namespace: Arc<dyn ns::Namespace>,
}

impl InodeOps for ProcNsBackingInodeOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(super::nsfs::NsfsFileOps::new(Arc::clone(
            &self.namespace,
        ))))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── 顶层补充文件 ───────────────────────────────────────────────────────────────

/// 全局 (running, total) 任务计数（/proc/loadavg 第 4 字段）。
fn running_total_tasks() -> (usize, usize) {
    let mut running = 0usize;
    let mut total = 0usize;
    if sched::is_ready() {
        for (_, weak) in sched::root_pid_ns().registry().snapshot() {
            let Some(task) = weak.upgrade() else {
                continue;
            };
            total += 1;
            match task.state() {
                TaskState::New
                | TaskState::Runnable
                | TaskState::Running
                | TaskState::Continued => running += 1,
                _ => {}
            }
        }
    }
    (running, total)
}

/// 当前已分配的最大 pid（/proc/loadavg 第 5 字段的近似）。
fn last_allocated_pid() -> PidT {
    if !sched::is_ready() {
        return 0;
    }
    sched::root_pid_ns()
        .registry()
        .snapshot()
        .into_iter()
        .map(|(pid, _)| pid)
        .max()
        .unwrap_or(0)
}

fn render_loadavg() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    // loads_scaled() 单位 1/65536；转成两位小数定点十进制（避免内核引入浮点）。
    for (index, scaled) in sched::avenrun::loads_scaled().iter().enumerate() {
        if index != 0 {
            out.push(' ');
        }
        let integer = scaled / 65536;
        let frac = (scaled % 65536) * 100 / 65536;
        let _ = write!(out, "{integer}.{frac:02}");
    }
    let (running, total) = running_total_tasks();
    let last_pid = last_allocated_pid();
    let _ = write!(out, " {running}/{total} {last_pid}\n");
    out
}

fn render_cmdline() -> String {
    let Some(bytes) = crate::start::start_cmdline() else {
        return String::new();
    };
    let text = crate::cmdline::Cmdline::new(bytes).as_str();
    if text.is_empty() {
        String::new()
    } else {
        format!("{text}\n")
    }
}

fn render_partitions() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "major minor  #blocks  name");
    for projection in published_block_devnodes(&DEVICES.functions) {
        let dev = projection.dev();
        let rdev = projection.rdev();
        let blocks = dev.geometry().block_count().unwrap_or(0);
        let _ = writeln!(
            out,
            "{:>5} {:>5} {:>8} {}",
            rdev.major,
            rdev.minor,
            blocks,
            dev.name()
        );
    }
    out
}

fn render_diskstats() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    for projection in published_block_devnodes(&DEVICES.functions) {
        let dev = projection.dev();
        let rdev = projection.rdev();
        let stats = dev.io_stats();
        let ms = |ns: u64| ns / 1_000_000;
        let _ = writeln!(
            out,
            "{:>5} {:>5} {} {} 0 {} {} {} 0 {} {} {} {} {}",
            rdev.major,
            rdev.minor,
            dev.name(),
            stats.read_ios,
            stats.read_sectors,
            ms(stats.read_time_ns),
            stats.write_ios,
            stats.write_sectors,
            ms(stats.write_time_ns),
            stats.read_inflight + stats.write_inflight,
            ms(stats.read_time_ns + stats.write_time_ns + stats.flush_time_ns),
            ms(stats.read_time_ns + stats.write_time_ns),
        );
    }
    out
}

fn render_kallsyms() -> String {
    use alloc::fmt::Write;
    // 无完整内核符号表。只导出内核直接符号目录锚点这一真实符号，避免伪造地址。
    let mut out = String::new();
    let anchor = crate::kernel_symbol_catalog_anchor as *const () as usize;
    let _ = writeln!(out, "{anchor:016x} t mygo_kernel_symbol_catalog_anchor");
    out
}

fn render_vmstat() -> String {
    use alloc::fmt::Write;
    let overview = allocator::KERNEL_ALLOCATOR.detailed_stats();
    let buddy = allocator::KERNEL_ALLOCATOR.buddy_stats();
    let page_size = page_size();
    let anon = crate::mm::memstat::ANON_PAGES.load(Ordering::Relaxed);
    let shared_anon = crate::mm::memstat::SHARED_ANON_PAGES.load(Ordering::Relaxed);
    let private_file = crate::mm::memstat::PRIVATE_FILE_PAGES.load(Ordering::Relaxed);
    let shared_file = crate::mm::memstat::SHARED_FILE_PAGES.load(Ordering::Relaxed);
    let mut out = String::new();
    let _ = writeln!(out, "nr_free_pages {}", overview.free_physical / page_size);
    let _ = writeln!(
        out,
        "nr_zone_inactive_anon 0\n\
         nr_zone_active_anon 0\n\
         nr_zone_inactive_file 0\n\
         nr_zone_active_file 0\n\
         nr_zone_unevictable 0\n\
         nr_zone_write_pending 0\n\
         nr_mlock {}\n\
         nr_anon_pages {}\n\
         nr_mapped {}\n\
         nr_file_pages {}\n\
         nr_dirty 0\n\
         nr_writeback 0\n\
         nr_slab_reclaimable 0\n\
         nr_slab_unreclaimable 0\n\
         nr_page_table_pages 0\n\
         nr_kernel_stack 0\n\
         nr_unstable 0\n\
         nr_bounce 0\n\
         nr_vmscan_write 0\n\
         nr_vmscan_immediate_reclaim 0\n\
         nr_writeback_temp 0\n\
         nr_isolated_anon 0\n\
         nr_isolated_file 0\n\
         nr_shmem {}\n\
         nr_dirtied 0\n\
         nr_written 0\n\
         numa_hit 0\n\
         numa_miss 0\n\
         numa_foreign 0\n\
         numa_interleave 0\n\
         numa_local 0\n\
         numa_other 0\n\
         nr_anon_transparent_hugepages 0\n\
         nr_free_cma 0\n\
         nr_dirty_threshold 0\n\
         nr_dirty_background_threshold 0\n",
        crate::mm::memstat::locked_pages(),
        anon,
        anon + shared_anon + private_file + shared_file,
        private_file + shared_file,
        shared_anon,
    );
    let _ = writeln!(out, "pgpgin 0");
    let _ = writeln!(out, "pgpgout 0");
    let _ = writeln!(out, "pswpin 0");
    let _ = writeln!(out, "pswpout 0");
    let _ = writeln!(
        out,
        "pgalloc_normal {}\npgalloc_movable 0\npgfree {}\n",
        buddy.free_pages, buddy.free_pages
    );
    out
}

fn render_zoneinfo() -> String {
    use alloc::fmt::Write;
    let overview = allocator::KERNEL_ALLOCATOR.detailed_stats();
    let page_size = page_size();
    let mut out = String::new();
    let _ = writeln!(out, "Node 0, zone   Normal");
    let _ = writeln!(
        out,
        "  per-node stats\n      nr_free_pages {}\n",
        overview.free_physical / page_size
    );
    let _ = writeln!(
        out,
        "  pages free     {}\n        min      0\n        low      0\n        high     0\n        spanned  {}\n        present  {}\n        managed  {}\n",
        overview.free_physical / page_size,
        overview.total_physical / page_size,
        overview.total_physical / page_size,
        overview.total_physical / page_size,
    );
    let _ = writeln!(out, "  protection: (0,)");
    out
}

fn render_buddyinfo() -> String {
    use alloc::fmt::Write;
    let buddy = allocator::KERNEL_ALLOCATOR.buddy_stats();
    let mut out = String::new();
    out.push_str("Node 0, zone   Normal ");
    for count in buddy.free_count_per_order.iter() {
        let _ = write!(out, "{:>6}", count);
    }
    out.push('\n');
    out
}

fn render_iomem() -> String {
    use alloc::fmt::Write;
    let overview = allocator::KERNEL_ALLOCATOR.detailed_stats();
    let mut out = String::new();
    // 无逐段物理地址分类视图；用分配器总览构造 RAM/保留两段的兼容视图。
    let total = overview.total_physical;
    let reserved = overview.reserved_physical;
    let ram_end = total.saturating_sub(reserved);
    if total > 0 {
        let _ = writeln!(
            out,
            "00000000-{:08x} : System RAM",
            ram_end.saturating_sub(1)
        );
    }
    if reserved > 0 {
        let _ = writeln!(out, "{:08x}-{:08x} : reserved", ram_end, total - 1);
    }
    out
}

fn render_softirqs() -> String {
    use alloc::fmt::Write;
    let mut online = sched::online_cpu_mask();
    if online == 0 {
        online = 1;
    }
    let mut out = String::new();
    out.push_str("                    ");
    for cpu in 0..sched::NR_CPUS {
        if online & (1u64 << cpu) != 0 {
            let _ = write!(out, "CPU{cpu:>10}");
        }
    }
    out.push('\n');
    // 软中断计数数据源不足，输出 Linux 兼容的常见软中断行（全 0）。
    for name in [
        "HI", "TIMER", "NET_TX", "NET_RX", "BLOCK", "IRQ_POLL", "TASKLET", "SCHED", "HRTIMER",
        "RCU",
    ] {
        let _ = write!(out, "{name:>12}:");
        for cpu in 0..sched::NR_CPUS {
            if online & (1u64 << cpu) != 0 {
                let _ = write!(out, " {:>10}", 0u64);
            }
        }
        out.push('\n');
    }
    out
}

// ── /proc/sysvipc 与 /proc/keys ────────────────────────────────────────────────

struct ProcSysvipcDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
}

impl InodeOps for ProcSysvipcDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        let (ino, kind) = match name {
            "shm" => (SYSV_SHM_INO, RootFileKind::SysvipcShm),
            "sem" => (SYSV_SEM_INO, RootFileKind::SysvipcSem),
            "msg" => (SYSV_MSG_INO, RootFileKind::SysvipcMsg),
            _ => return Err(VfsError::NotFound),
        };
        Ok(mk_inode(
            self.fs_id,
            &self.weak_sb,
            ino,
            FileType::Regular,
            0o444,
            1,
            Arc::new(ProcRegularInodeOps {
                kind: ProcFileKind::Root(kind),
            }),
        ))
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(ProcDirFile {
            snapshot: vec![
                DirEntry {
                    ino: SYSV_SHM_INO,
                    name: SmallStr::new("shm"),
                    kind: FileType::Regular,
                },
                DirEntry {
                    ino: SYSV_SEM_INO,
                    name: SmallStr::new("sem"),
                    kind: FileType::Regular,
                },
                DirEntry {
                    ino: SYSV_MSG_INO,
                    name: SmallStr::new("msg"),
                    kind: FileType::Regular,
                },
            ],
        }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn render_sysvipc_shm() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "       key      shmid perms       size  cpid  lpid nattch   uid   gid  cuid  cgid      atime      dtime      ctime"
    );
    for entry in SYSV_SHM_PROVIDER.lock().map(|p| p()).unwrap_or_default() {
        let _ = writeln!(
            out,
            "{:>10} {:>10} {:>5o} {:>10} {:>5} {:>5} {:>6} {:>5} {:>5} {:>5} {:>5} {:>10} {:>10} {:>10}",
            entry.key,
            entry.id,
            entry.mode,
            entry.size_bytes,
            entry.cpid,
            entry.lpid,
            entry.nattch,
            entry.uid,
            entry.gid,
            entry.cuid,
            entry.cgid,
            entry.atime,
            entry.dtime,
            entry.ctime,
        );
    }
    out
}

fn render_sysvipc_sem() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime"
    );
    for entry in SYSV_SEM_PROVIDER.lock().map(|p| p()).unwrap_or_default() {
        let _ = writeln!(
            out,
            "{:>10} {:>10} {:>5o} {:>10} {:>5} {:>5} {:>5} {:>5} {:>10} {:>10}",
            entry.key,
            entry.id,
            entry.mode,
            entry.nsems,
            entry.uid,
            entry.gid,
            entry.cuid,
            entry.cgid,
            entry.otime,
            entry.ctime,
        );
    }
    out
}

fn render_sysvipc_msg() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "       key      msqid perms      qbytes   qnum lspid lrpid   uid   gid  cuid  cgid      stime      rtime      ctime"
    );
    for entry in SYSV_MSG_PROVIDER.lock().map(|p| p()).unwrap_or_default() {
        let _ = writeln!(
            out,
            "{:>10} {:>10} {:>5o} {:>10} {:>5} {:>5} {:>5} {:>5} {:>5} {:>5} {:>5} {:>10} {:>10} {:>10}",
            entry.key,
            entry.id,
            entry.mode,
            entry.qbytes,
            entry.qnum,
            entry.lspid,
            entry.lrpid,
            entry.uid,
            entry.gid,
            entry.cuid,
            entry.cgid,
            entry.stime,
            entry.rtime,
            entry.ctime,
        );
    }
    out
}

fn render_proc_keys() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "serial     flags      uid   gid   perm    bytes  description"
    );
    for entry in KEYS_PROVIDER.lock().map(|p| p()).unwrap_or_default() {
        let _ = writeln!(
            out,
            "{:08x} {:<10} {:>5} {:>5} {:>8} {:>6} {}",
            entry.id,
            entry.state,
            entry.uid,
            entry.gid,
            entry.perm,
            entry.payload_len,
            entry.description,
        );
    }
    out
}

fn render_proc_key_users() -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "    uid      usage");
    for (uid, keys, bytes) in KEY_USERS_PROVIDER.lock().map(|p| p()).unwrap_or_default() {
        let _ = writeln!(out, "{:>8} {:>10} {:>10}", uid, keys, bytes);
    }
    out
}

// ── 补充 sysctl（procfs 自有，不依赖 memstat VmParam 的缺项）──────────────────

const KERNEL_EXTRA_SYSCTLS: &[&str] = &[
    "hostname",
    "domainname",
    "osrelease",
    "ostype",
    "osversion",
    "core_pattern",
    "panic",
    "threads-max",
];
const KERNEL_RANDOM_SYSCTLS: &[&str] = &["entropy_avail", "uuid", "boot_id"];
const FS_EXTRA_SYSCTLS: &[&str] = &[
    "file-nr",
    "inode-nr",
    "dentry-state",
    "nr_open",
    "aio-max-nr",
    "aio-nr",
    "suid_dumpable",
    "protected_symlinks",
    "protected_hardlinks",
];
const VM_EXTRA_SYSCTLS: &[&str] = &[
    "mmap_min_addr",
    "nr_hugepages",
    "nr_overcommit_hugepages",
    "admin_reserve_kbytes",
    "user_reserve_kbytes",
    "watermark_scale_factor",
];
const NET_CORE_SYSCTLS: &[&str] = &[
    "somaxconn",
    "rmem_default",
    "wmem_default",
    "netdev_max_backlog",
];
const NET_IPV4_SYSCTLS: &[&str] = &[
    "ip_forward",
    "tcp_syncookies",
    "ip_default_ttl",
    "ip_local_port_range",
];

fn extra_sysctl_ino(name: &str) -> u64 {
    let mut base = SYS_EXTRA_SYSCTL_BASE;
    for list in [
        KERNEL_EXTRA_SYSCTLS,
        KERNEL_RANDOM_SYSCTLS,
        FS_EXTRA_SYSCTLS,
        VM_EXTRA_SYSCTLS,
        NET_CORE_SYSCTLS,
        NET_IPV4_SYSCTLS,
    ] {
        if let Some(pos) = list.iter().position(|n| *n == name) {
            return base + pos as u64;
        }
        base += list.len() as u64;
    }
    SYS_EXTRA_SYSCTL_BASE + name.len() as u64
}

fn extra_sysctl_is_text(name: &str) -> bool {
    matches!(
        name,
        "hostname"
            | "domainname"
            | "osrelease"
            | "ostype"
            | "osversion"
            | "core_pattern"
            | "uuid"
            | "boot_id"
            | "ip_local_port_range"
    )
}

fn extra_sysctl_is_writable(name: &str) -> bool {
    !matches!(
        name,
        "osrelease"
            | "ostype"
            | "osversion"
            | "file-nr"
            | "inode-nr"
            | "dentry-state"
            | "aio-nr"
            | "entropy_avail"
            | "uuid"
            | "boot_id"
    )
}

fn extra_sysctl_default_text(name: &str) -> String {
    match name {
        "hostname" => String::from("mygo"),
        "domainname" => String::from("(none)"),
        "osrelease" => String::from("6.6.0-mygo"),
        "ostype" => String::from("Linux"),
        "osversion" => String::from("#1 MyGo SMP"),
        "core_pattern" => String::from("core"),
        "uuid" => String::from("00000000-0000-0000-0000-000000000000"),
        "boot_id" => String::from("00000000-0000-0000-0000-000000000000"),
        "ip_local_port_range" => String::from("32768\t60999"),
        _ => String::new(),
    }
}

fn extra_sysctl_default_num(name: &str) -> u64 {
    match name {
        "panic" => 0,
        "threads-max" => 65535,
        "nr_open" => 1048576,
        "aio-max-nr" => 65536,
        "suid_dumpable" => 0,
        "protected_symlinks" => 0,
        "protected_hardlinks" => 0,
        "mmap_min_addr" => 4096,
        "nr_hugepages" => 0,
        "nr_overcommit_hugepages" => 0,
        "admin_reserve_kbytes" => 8192,
        "user_reserve_kbytes" => 131072,
        "watermark_scale_factor" => 10,
        "somaxconn" => 4096,
        "rmem_default" => 212992,
        "wmem_default" => 212992,
        "netdev_max_backlog" => 1000,
        "ip_forward" => 0,
        "tcp_syncookies" => 1,
        "ip_default_ttl" => 64,
        _ => 0,
    }
}

fn extra_sysctl_valid_num(name: &str, value: u64) -> bool {
    match name {
        "panic" => true,
        "threads-max" => value >= 20,
        "nr_open" => value >= 1024,
        "aio-max-nr" => true,
        "suid_dumpable" => value <= 2,
        "protected_symlinks" | "protected_hardlinks" => value <= 1,
        "mmap_min_addr" => true,
        "nr_hugepages" | "nr_overcommit_hugepages" => true,
        "admin_reserve_kbytes" | "user_reserve_kbytes" => true,
        "watermark_scale_factor" => value <= 3000,
        "somaxconn" => true,
        "rmem_default" | "wmem_default" => true,
        "netdev_max_backlog" => true,
        "ip_forward" | "tcp_syncookies" => value <= 1,
        "ip_default_ttl" => value >= 1 && value <= 255,
        _ => true,
    }
}

fn render_extra_sysctl(name: &'static str) -> String {
    match name {
        "file-nr" => format!(
            "{}\t0\t{}\n",
            vfs::file::file_diag().live,
            FILE_MAX.load(Ordering::Relaxed)
        ),
        "inode-nr" => String::from("0\t0\n"),
        "dentry-state" => format!("{}\t0\t45\t0\n", vfs::DCACHE.len()),
        "aio-nr" => String::from("0\n"),
        "entropy_avail" => String::from("256\n"),
        _ => {
            if extra_sysctl_is_text(name) {
                let map = EXTRA_SYSCTL_TEXT.lock();
                format!(
                    "{}\n",
                    map.get(name)
                        .cloned()
                        .unwrap_or_else(|| extra_sysctl_default_text(name))
                )
            } else {
                let map = EXTRA_SYSCTL_NUM.lock();
                format!(
                    "{}\n",
                    map.get(name)
                        .copied()
                        .unwrap_or_else(|| extra_sysctl_default_num(name))
                )
            }
        }
    }
}

fn write_extra_sysctl(name: &'static str, buf: &[u8], offset: u64) -> VfsResult<usize> {
    if !extra_sysctl_is_writable(name) {
        return Err(VfsError::ReadOnlyFilesystem);
    }
    if offset != 0 {
        return Err(VfsError::InvalidArgument);
    }
    let text = core::str::from_utf8(buf).map_err(|_| VfsError::InvalidArgument)?;
    let trimmed = text.trim_matches(|ch: char| ch.is_ascii_whitespace() || ch == '\0');
    if extra_sysctl_is_text(name) {
        EXTRA_SYSCTL_TEXT.lock().insert(name, String::from(trimmed));
    } else {
        let value = trimmed
            .parse::<u64>()
            .map_err(|_| VfsError::InvalidArgument)?;
        if !extra_sysctl_valid_num(name, value) {
            return Err(VfsError::InvalidArgument);
        }
        EXTRA_SYSCTL_NUM.lock().insert(name, value);
    }
    Ok(buf.len())
}

fn proc_sys_extra_inode(fs_id: FsId, weak_sb: &Weak<Superblock>, name: &'static str) -> Arc<Inode> {
    let mode = if extra_sysctl_is_writable(name) {
        0o644
    } else {
        0o444
    };
    mk_inode(
        fs_id,
        weak_sb,
        extra_sysctl_ino(name),
        FileType::Regular,
        mode,
        1,
        Arc::new(ProcRegularInodeOps {
            kind: ProcFileKind::SysExtra(name),
        }),
    )
}

fn push_extra_sysctl_entries(
    snapshot: &mut Vec<DirEntry>,
    names: &[&'static str],
) -> VfsResult<()> {
    for name in names {
        push_proc_dir_entry(snapshot, extra_sysctl_ino(name), name, FileType::Regular)?;
    }
    Ok(())
}

// ── /proc/sys/net 目录 ─────────────────────────────────────────────────────────

fn proc_sys_net_dir_inode(fs_id: FsId, weak_sb: &Weak<Superblock>) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        SYS_NET_DIR_INO,
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcSysNetDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
        }),
    )
}

struct ProcSysNetDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
}

impl InodeOps for ProcSysNetDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        match name {
            "core" => Ok(proc_sys_net_subdir_inode(
                self.fs_id,
                &self.weak_sb,
                SYS_NET_CORE_INO,
                NET_CORE_SYSCTLS,
            )),
            "ipv4" => Ok(proc_sys_net_subdir_inode(
                self.fs_id,
                &self.weak_sb,
                SYS_NET_IPV4_INO,
                NET_IPV4_SYSCTLS,
            )),
            "ipv6" => Ok(proc_sys_net_subdir_inode(
                self.fs_id,
                &self.weak_sb,
                SYS_NET_IPV6_INO,
                &[],
            )),
            _ => Err(VfsError::NotFound),
        }
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(ProcDirFile {
            snapshot: vec![
                DirEntry {
                    ino: SYS_NET_CORE_INO,
                    name: SmallStr::new("core"),
                    kind: FileType::Directory,
                },
                DirEntry {
                    ino: SYS_NET_IPV4_INO,
                    name: SmallStr::new("ipv4"),
                    kind: FileType::Directory,
                },
                DirEntry {
                    ino: SYS_NET_IPV6_INO,
                    name: SmallStr::new("ipv6"),
                    kind: FileType::Directory,
                },
            ],
        }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn proc_sys_net_subdir_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    ino: u64,
    entries: &'static [&'static str],
) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        ino,
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcSysNetSubDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
            entries,
        }),
    )
}

struct ProcSysNetSubDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
    entries: &'static [&'static str],
}

impl InodeOps for ProcSysNetSubDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        let entry = self
            .entries
            .iter()
            .copied()
            .find(|entry| *entry == name)
            .ok_or(VfsError::NotFound)?;
        Ok(proc_sys_extra_inode(self.fs_id, &self.weak_sb, entry))
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let mut snapshot = Vec::new();
        push_extra_sysctl_entries(&mut snapshot, self.entries)?;
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── /proc/sys/kernel/random 目录 ───────────────────────────────────────────────

const SYS_RANDOM_DIR_INO: u64 = 304;

fn proc_sys_random_dir_inode(fs_id: FsId, weak_sb: &Weak<Superblock>) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        SYS_RANDOM_DIR_INO,
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcSysRandomDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
        }),
    )
}

struct ProcSysRandomDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
}

impl InodeOps for ProcSysRandomDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        let entry = KERNEL_RANDOM_SYSCTLS
            .iter()
            .copied()
            .find(|entry| *entry == name)
            .ok_or(VfsError::NotFound)?;
        Ok(proc_sys_extra_inode(self.fs_id, &self.weak_sb, entry))
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let mut snapshot = Vec::new();
        push_extra_sysctl_entries(&mut snapshot, KERNEL_RANDOM_SYSCTLS)?;
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn write_pid_max(buf: &[u8], offset: u64) -> VfsResult<usize> {
    if offset != 0 {
        return Err(VfsError::InvalidArgument);
    }
    let value = vfs::sysctl::parse_nonnegative_long(buf)?;
    if value < 20 {
        return Err(VfsError::InvalidArgument);
    }
    PID_MAX.store(value.min(i32::MAX as u64) as i32, Ordering::Relaxed);
    Ok(buf.len())
}

fn render_pid_max() -> String {
    format!("{}\n", PID_MAX.load(Ordering::Relaxed))
}

// ── /proc/[pid] 补充文件 ───────────────────────────────────────────────────────

/// 构造 maps 头部行（start-end perms offset dev inode + 路径/标注）。
fn vma_maps_header(
    out: &mut String,
    exec_path: Option<&str>,
    first_exec: &mut bool,
    range: &Range<usize>,
    flags: VmFlags,
) {
    use alloc::fmt::Write;
    let perms = vm_flags_to_maps_perms(flags);
    let suffix = if flags.has(VmFlags::GROWS_DOWN) {
        " [stack]".to_string()
    } else if flags.has(VmFlags::EXEC) {
        if *first_exec {
            *first_exec = false;
            exec_path.map(|path| format!(" {path}")).unwrap_or_default()
        } else {
            // 无 VM_SPECIAL/vdso 标志；把第二个及以后的 EXEC VMA 近似标注为 vdso。
            " [vdso]".to_string()
        }
    } else {
        String::new()
    };
    let _ = write!(
        out,
        "{:016x}-{:016x} {} 00000000 00:00 0{}\n",
        range.start, range.end, perms, suffix,
    );
}

/// 统计一个 VMA 范围内的驻留页（resident_bitmap 返回每页 0/1）。
fn vma_resident_pages(vm: &VmSpace, range: &Range<usize>) -> usize {
    let Ok(bitmap) = vm.resident_bitmap(range.clone()) else {
        return 0;
    };
    bitmap.iter().filter(|byte| **byte != 0).count()
}

fn render_task_smaps(task: &Arc<Task>) -> String {
    use alloc::fmt::Write;
    let Some(vm) = task_vm_space(task) else {
        return String::new();
    };
    let page_size = page_size();
    let exec_path = task_exec_path(task).ok();
    let mut first_exec = true;
    let mut out = String::new();
    for (range, flags) in dump_vmas(&vm) {
        vma_maps_header(
            &mut out,
            exec_path.as_deref(),
            &mut first_exec,
            &range,
            flags,
        );
        let size = (range.end - range.start) as u64;
        let resident = vma_resident_pages(&vm, &range) as u64 * page_size as u64;
        let anon = flags.has(VmFlags::ANON) && !flags.has(VmFlags::SHARED);
        let anonymous = if anon { resident } else { 0 };
        let private_dirty = if anon { resident } else { 0 };
        let private_clean = if anon { 0 } else { resident };
        let _ = writeln!(
            out,
            "Size:          {:>8} kB\n\
             KernelPageSize:{:>4} kB\n\
             MMUPageSize:   {:>4} kB\n\
             Rss:           {:>8} kB\n\
             Pss:           {:>8} kB\n\
             Shared_Clean:  {:>8} kB\n\
             Shared_Dirty:  {:>8} kB\n\
             Private_Clean: {:>8} kB\n\
             Private_Dirty: {:>8} kB\n\
             Referenced:    {:>8} kB\n\
             Anonymous:     {:>8} kB\n\
             Swap:          {:>8} kB\n\
             SwapPss:       {:>8} kB\n\
             Locked:        {:>8} kB\n",
            size / 1024,
            page_size / 1024,
            page_size / 1024,
            resident / 1024,
            resident / 1024,
            0u64,
            0u64,
            private_clean / 1024,
            private_dirty / 1024,
            0u64,
            anonymous / 1024,
            0u64,
            0u64,
            if flags.has(VmFlags::LOCKED) {
                resident / 1024
            } else {
                0
            },
        );
    }
    out
}

fn render_task_numa_maps(task: &Arc<Task>) -> String {
    use alloc::fmt::Write;
    let Some(vm) = task_vm_space(task) else {
        return String::new();
    };
    let mut out = String::new();
    for (range, _flags) in dump_vmas(&vm) {
        // 无 NUMA 迁移/结点策略；统一标注 default。
        let _ = writeln!(out, "{:016x} default", range.start);
    }
    out
}

fn render_task_limits(task: &Arc<Task>) -> String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let mut pairs = [RlimitPair::default(); sched::Resource::COUNT];
    task.thread_group()
        .rlimits()
        .lock()
        .snapshot_into(&mut pairs);
    let units = [
        "seconds",
        "bytes",
        "bytes",
        "bytes",
        "bytes",
        "bytes",
        "processes",
        "files",
        "bytes",
        "bytes",
        "locks",
        "signals",
        "bytes",
        "",
        "",
        "us",
    ];
    let _ = writeln!(
        out,
        "Limit                     Soft Limit           Hard Limit           Units"
    );
    for index in 0..sched::Resource::COUNT {
        let resource = sched::Resource::from_raw(index as u32).unwrap();
        let pair = pairs[index];
        let soft = if pair.soft.is_infinity() {
            "unlimited".to_string()
        } else {
            format!("{}", pair.soft.raw())
        };
        let hard = if pair.hard.is_infinity() {
            "unlimited".to_string()
        } else {
            format!("{}", pair.hard.raw())
        };
        let _ = writeln!(
            out,
            "{:<25} {:<21} {:<21} {}",
            resource.name(),
            soft,
            hard,
            units[index],
        );
    }
    out
}

// ELF auxiliary vector 常量（与 kernel/src/user.rs 一致）。
const AT_NULL: u64 = 0;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_PAGESZ: u64 = 6;
const AT_CLKTCK: u64 = 17;

/// `/proc/[pid]/auxv`：exec 未保存原始 auxv，这里从进程元数据重建最小向量。
fn render_task_auxv(task: &Arc<Task>) -> Vec<u8> {
    let creds = task.credentials();
    let entries: [(u64, u64); 6] = [
        (AT_PAGESZ, page_size() as u64),
        (AT_CLKTCK, 100),
        (AT_UID, u64::from(creds.uid.0)),
        (AT_EUID, u64::from(creds.euid.0)),
        (AT_GID, u64::from(creds.gid.0)),
        (AT_EGID, u64::from(creds.egid.0)),
    ];
    let mut out = Vec::new();
    for (key, value) in entries {
        out.extend_from_slice(&key.to_ne_bytes());
        out.extend_from_slice(&value.to_ne_bytes());
    }
    out.extend_from_slice(&AT_NULL.to_ne_bytes());
    out.extend_from_slice(&0u64.to_ne_bytes());
    out
}

fn render_task_io(_task: &Arc<Task>) -> String {
    // 无 per-process I/O 记账接口；输出 Linux 兼容字段形状（全 0）。
    "rchar: 0\nwchar: 0\nsyscr: 0\nsyscw: 0\nread_bytes: 0\nwrite_bytes: 0\ncancelled_write_bytes: 0\n"
        .to_string()
}

fn task_oom_score_adj(pid: PidT) -> i32 {
    OOM_SCORE_ADJ.lock().get(&pid).copied().unwrap_or(0)
}

fn render_task_oom_score(task: &Arc<Task>) -> String {
    // 无 OOM killer；评分按 RSS 页数近似（Linux 默认 oom_score_adj=0 时也近似正比 RSS）。
    let (_, rss, _) = task_memory_usage(task);
    let pages = rss / page_size() as u64;
    format!("{}\n", pages)
}

fn render_task_oom_score_adj(pid: PidT) -> String {
    format!("{}\n", task_oom_score_adj(pid))
}

fn render_task_oom_adj(pid: PidT) -> String {
    // 旧版 oom_adj（-16..15）按 oom_score_adj（-1000..1000）线性近似。
    let adj = task_oom_score_adj(pid);
    format!("{}\n", (adj * 15 / 1000).clamp(-16, 15))
}

fn write_task_oom_score_adj(pid: PidT, buf: &[u8], offset: u64) -> VfsResult<usize> {
    if offset != 0 {
        return Err(VfsError::InvalidArgument);
    }
    let text = core::str::from_utf8(buf).map_err(|_| VfsError::InvalidArgument)?;
    let value = text
        .trim_matches(|ch: char| ch.is_ascii_whitespace() || ch == '\0')
        .parse::<i32>()
        .map_err(|_| VfsError::InvalidArgument)?;
    if !(-1000..=1000).contains(&value) {
        return Err(VfsError::InvalidArgument);
    }
    OOM_SCORE_ADJ.lock().insert(pid, value);
    Ok(buf.len())
}

fn write_task_oom_adj(pid: PidT, buf: &[u8], offset: u64) -> VfsResult<usize> {
    if offset != 0 {
        return Err(VfsError::InvalidArgument);
    }
    let text = core::str::from_utf8(buf).map_err(|_| VfsError::InvalidArgument)?;
    let value = text
        .trim_matches(|ch: char| ch.is_ascii_whitespace() || ch == '\0')
        .parse::<i32>()
        .map_err(|_| VfsError::InvalidArgument)?;
    if !(-16..=15).contains(&value) {
        return Err(VfsError::InvalidArgument);
    }
    OOM_SCORE_ADJ
        .lock()
        .insert(pid, (value * 1000 / 15).clamp(-1000, 1000));
    Ok(buf.len())
}

fn sched_policy_name(policy: SchedPolicy) -> &'static str {
    match policy {
        SchedPolicy::Fair => "SCHED_NORMAL",
        SchedPolicy::RtFifo => "SCHED_FIFO",
        SchedPolicy::RtRoundRobin => "SCHED_RR",
        SchedPolicy::Deadline => "SCHED_DEADLINE",
        SchedPolicy::Idle => "SCHED_IDLE",
        SchedPolicy::Batch => "SCHED_BATCH",
    }
}

/// Linux `/proc/[pid]/stat` 的 policy 字段编号（sched 内部策略号与 Linux UAPI 不同）。
fn sched_policy_linux_id(policy: SchedPolicy) -> u32 {
    match policy {
        SchedPolicy::Fair => 0, // SCHED_OTHER
        SchedPolicy::RtFifo => 1,
        SchedPolicy::RtRoundRobin => 2,
        SchedPolicy::Deadline => 6,
        SchedPolicy::Idle => 5,
        SchedPolicy::Batch => 3, // SCHED_BATCH
    }
}

fn render_task_sched(task: &Arc<Task>) -> String {
    use alloc::fmt::Write;
    let policy = task.sched.policy();
    let class = task.sched.class();
    let nice = task.sched.nice();
    let rt_priority = task.sched.rt_priority();
    let mut out = String::new();
    let comm = render_task_comm(task).trim_end().to_string();
    let _ = writeln!(out, "{comm} ({pid})", pid = task.pid_root().unwrap_or(0));
    let _ = writeln!(out, "policy {}", sched_policy_name(policy));
    let _ = writeln!(out, "sched_class {:?}", class);
    let _ = writeln!(out, "nice {}", nice);
    let _ = writeln!(out, "rt_priority {}", rt_priority);
    let _ = writeln!(out, "se.exec_start 0");
    let _ = writeln!(out, "se.vruntime 0");
    let _ = writeln!(out, "nr_switches 0");
    let _ = writeln!(out, "nr_voluntary_switches 0");
    let _ = writeln!(out, "nr_involuntary_switches 0");
    out
}

fn render_task_syscall(_task: &Arc<Task>) -> String {
    // 无当前阻塞 syscall 快照接口；输出 Linux 兼容的 7 字段形状（全 0）。
    "0 0x0 0x0 0x0 0x0 0x0 0x0\n".to_string()
}

fn render_task_stack(_task: &Arc<Task>) -> String {
    // 无内核栈回溯数据源。
    String::new()
}

fn render_task_cgroup(_task: &Arc<Task>) -> String {
    // 无 cgroup 控制器；输出 cgroup v2 统一层级根视图。
    "0::/\n".to_string()
}

fn render_task_seccomp(task: &Arc<Task>) -> String {
    let mode = task
        .ext_lookup(crate::syscall::TASKEXT_SECCOMP)
        .and_then(|payload| payload.downcast::<crate::seccomp::SeccompState>().ok())
        .map(|state| state.mode())
        .unwrap_or(0);
    format!("{mode}\n")
}

fn render_task_timers(_task: &Arc<Task>) -> String {
    // 无 POSIX 定时器列表快照；空输出（Linux 无定时器时也为空）。
    String::new()
}

fn render_task_loginuid(_task: &Arc<Task>) -> String {
    // 无 audit login uid；输出 -1（INVALID_UID）。
    "-1\n".to_string()
}

fn render_task_sessionid(_task: &Arc<Task>) -> String {
    // 无 audit session id；输出 0。
    "0\n".to_string()
}

fn render_task_uid_map(_task: &Arc<Task>) -> String {
    // 无用户命名空间；输出初始命名空间的全量恒等映射行。
    "         0          0 4294967295\n".to_string()
}

fn render_task_gid_map(_task: &Arc<Task>) -> String {
    "         0          0 4294967295\n".to_string()
}

// ── /proc/[pid]/mem 与 /proc/[pid]/pagemap ─────────────────────────────────────

struct ProcMemInodeOps {
    pid: PidT,
}

impl InodeOps for ProcMemInodeOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }
    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(ProcMemFileOps { pid: self.pid }))
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcMemFileOps {
    pid: PidT,
}

impl ProcMemFileOps {
    fn vm(&self) -> VfsResult<Arc<VmSpace>> {
        let task = lookup_task(self.pid).ok_or(VfsError::NotFound)?;
        ensure_task_access(&task)?;
        task_vm_space(&task).ok_or(VfsError::NotFound)
    }
}

fn read_mem_at(vm: &VmSpace, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
    let page_size = page_size();
    let mut done = 0usize;
    let start = offset as usize;
    while done < buf.len() {
        let addr = start + done;
        let within_page = page_size - (addr % page_size);
        let want = (buf.len() - done).min(within_page);
        if vm
            .ensure_remote_page(addr, crate::mm::FaultKind::Load)
            .is_err()
        {
            break;
        }
        if vm
            .copy_resident_bytes_out(addr..addr + want, &mut buf[done..done + want])
            .is_err()
        {
            break;
        }
        done += want;
    }
    if done == 0 && !buf.is_empty() {
        // Linux 对无法读取的地址返回 EIO。
        return Err(VfsError::Io);
    }
    Ok(done)
}

fn write_mem_at(vm: &VmSpace, buf: &[u8], offset: u64) -> VfsResult<usize> {
    let page_size = page_size();
    let mut done = 0usize;
    let start = offset as usize;
    while done < buf.len() {
        let addr = start + done;
        let within_page = page_size - (addr % page_size);
        let want = (buf.len() - done).min(within_page);
        if vm
            .ensure_remote_page(addr, crate::mm::FaultKind::Store)
            .is_err()
        {
            break;
        }
        if vm
            .copy_resident_bytes_in(addr..addr + want, &buf[done..done + want])
            .is_err()
        {
            break;
        }
        done += want;
    }
    if done == 0 && !buf.is_empty() {
        return Err(VfsError::Io);
    }
    Ok(done)
}

impl FileOps for ProcMemFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let vm = self.vm()?;
        read_mem_at(&vm, buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let vm = self.vm()?;
        write_mem_at(&vm, buf, offset)
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
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcPagemapInodeOps {
    pid: PidT,
}

impl InodeOps for ProcPagemapInodeOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }
    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(ProcPagemapFileOps { pid: self.pid }))
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcPagemapFileOps {
    pid: PidT,
}

impl ProcPagemapFileOps {
    fn vm(&self) -> VfsResult<Arc<VmSpace>> {
        let task = lookup_task(self.pid).ok_or(VfsError::NotFound)?;
        ensure_task_access(&task)?;
        task_vm_space(&task).ok_or(VfsError::NotFound)
    }
}

/// pagemap 每页一个 8 字节项；`offset` 换算成虚拟地址范围后按驻留位填 `1<<63`。
fn read_pagemap_at(vm: &VmSpace, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
    const PM_PRESENT: u64 = 1 << 63;
    const PAGE_SHIFT_OFFSET: u64 = 8;
    let page_size = page_size();
    let entries_per_chunk = buf.len() / 8;
    if entries_per_chunk == 0 {
        return Ok(0);
    }
    let start_page = offset / PAGE_SHIFT_OFFSET;
    let start_addr = (start_page as usize).saturating_mul(page_size);
    let end_addr = start_addr.saturating_add(entries_per_chunk * page_size);
    let bitmap = vm.resident_bitmap(start_addr..end_addr).unwrap_or_default();
    let mut written = 0usize;
    for present in bitmap.iter().take(entries_per_chunk) {
        let entry = if *present != 0 { PM_PRESENT } else { 0 };
        buf[written..written + 8].copy_from_slice(&entry.to_ne_bytes());
        written += 8;
    }
    Ok(written)
}

impl FileOps for ProcPagemapFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if buf.len() < 8 {
            return Ok(0);
        }
        let vm = self.vm()?;
        read_pagemap_at(&vm, buf, offset)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::PermissionDenied)
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
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── /proc/[pid]/attr ───────────────────────────────────────────────────────────

fn proc_task_attr_dir_ino(pid: PidT) -> u64 {
    proc_task_base(pid) + TASK_SLOT_ATTR_DIR
}

fn proc_task_attr_dir_inode(fs_id: FsId, weak_sb: &Weak<Superblock>, pid: PidT) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        proc_task_attr_dir_ino(pid),
        FileType::Directory,
        0o555,
        2,
        Arc::new(ProcTaskAttrDirOps {
            fs_id,
            weak_sb: weak_sb.clone(),
            pid,
        }),
    )
}

struct ProcTaskAttrDirOps {
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
    pid: PidT,
}

const TASK_ATTR_FILES: &[&str] = &[
    "current",
    "prev",
    "exec",
    "fscreate",
    "keycreate",
    "sockcreate",
];

impl InodeOps for ProcTaskAttrDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        let Some(entry) = TASK_ATTR_FILES.iter().copied().find(|e| *e == name) else {
            return Err(VfsError::NotFound);
        };
        Ok(mk_inode(
            self.fs_id,
            &self.weak_sb,
            proc_task_attr_file_ino(self.pid, entry),
            FileType::Regular,
            0o644,
            1,
            Arc::new(ProcTaskAttrFileOps {
                pid: self.pid,
                name: entry,
            }),
        ))
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let mut snapshot = Vec::new();
        for name in TASK_ATTR_FILES {
            push_proc_dir_entry(
                &mut snapshot,
                proc_task_attr_file_ino(self.pid, name),
                name,
                FileType::Regular,
            )?;
        }
        Ok(Box::new(ProcDirFile { snapshot }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn proc_task_attr_file_ino(pid: PidT, name: &str) -> u64 {
    proc_task_base(pid) + 200 + name.len() as u64
}

struct ProcTaskAttrFileOps {
    pid: PidT,
    name: &'static str,
}

impl InodeOps for ProcTaskAttrFileOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }
    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(ProcTaskAttrFile {
            pid: self.pid,
            name: self.name,
        }))
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct ProcTaskAttrFile {
    pid: PidT,
    name: &'static str,
}

impl FileOps for ProcTaskAttrFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        // 无 LSM attr；`current` 为空，其它项未实现。
        let _ = lookup_task(self.pid).ok_or(VfsError::NotFound)?;
        if self.name != "current" {
            return Err(VfsError::NotSupported);
        }
        slice_bytes(buf, offset, b"")
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let _ = lookup_task(self.pid).ok_or(VfsError::NotFound)?;
        if self.name != "current" {
            return Err(VfsError::NotSupported);
        }
        if offset != 0 {
            return Err(VfsError::InvalidArgument);
        }
        // 无 LSM；接受写入并忽略（保持 ABI 可写）。
        Ok(buf.len())
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
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ns::Namespace as _;

    struct TestNamespace {
        inum: u64,
    }

    impl ns::Namespace for TestNamespace {
        fn ns_type(&self) -> ns::NsType {
            ns::NsType::Uts
        }

        fn inum(&self) -> u64 {
            self.inum
        }
    }

    #[test]
    fn task_namespace_directory_uses_a_unique_slot_and_is_listed() {
        let pid = 42;
        let mut snapshot = Vec::new();
        push_proc_task_ns_entry(&mut snapshot, pid);

        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].name.as_str(), "ns");
        assert_eq!(snapshot[0].kind, FileType::Directory);
        assert_eq!(snapshot[0].ino, proc_ns_dir_ino(pid));
        assert_ne!(
            proc_ns_dir_ino(pid),
            proc_task_file_ino(pid, TaskFileKind::Maps)
        );
        assert_ne!(proc_ns_dir_ino(pid), proc_fdinfo_dir_ino(pid));
    }

    #[test]
    fn namespace_directory_exposes_symlink_entries_with_unique_inodes() {
        let pid = 73;
        let weak_sb = Weak::<Superblock>::new();
        let dir = proc_ns_dir_inode(FsId(9), &weak_sb, pid);
        let ops = ProcNsDirOps {
            fs_id: FsId(9),
            weak_sb,
            pid,
        };
        let file = InodeOps::open(
            &ops,
            dir.as_ref(),
            &OpenOptions::default(),
            &Credentials::root(),
        )
        .unwrap();
        let mut entries = Vec::new();
        file.readdir(0, &mut |entry| {
            entries.push(entry);
            ControlFlow::Continue(())
        })
        .unwrap();

        assert_eq!(entries.len(), ProcNsKind::ALL.len());
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(entry.name.as_str(), ProcNsKind::ALL[index].name());
            assert_eq!(entry.kind, FileType::Symlink);
            assert_eq!(entry.ino, proc_ns_file_ino(pid, ProcNsKind::ALL[index]));
            assert_ne!(entry.ino, proc_fd_link_ino(pid, 0x60 + index as u32));
            for other in entries.iter().skip(index + 1) {
                assert_ne!(entry.ino, other.ino);
            }
        }

        let uts = InodeOps::lookup(&ops, dir.as_ref(), "uts").unwrap();
        assert_eq!(uts.kind(), FileType::Symlink);
    }

    #[test]
    fn namespace_link_target_round_trips_to_hidden_backing_name() {
        let namespace = TestNamespace { inum: 0x1234_5678 };
        let target = proc_ns_link_target(ProcNsKind::Uts, &namespace);
        assert_eq!(target, "uts:[305419896]");

        let (kind, inum) = parse_proc_ns_link_target(&target).unwrap();
        assert_eq!(kind.name(), "uts");
        assert_eq!(inum, namespace.inum());
        assert!(parse_proc_ns_link_target("uts:[]").is_none());
        assert!(parse_proc_ns_link_target("unknown:[1]").is_none());

        assert_ne!(
            proc_ns_file_ino(1, ProcNsKind::Uts),
            proc_ns_backing_ino(namespace.inum()),
        );
    }

    #[test]
    fn shared_namespace_uses_the_same_backing_inode_across_processes() {
        let namespace: Arc<dyn ns::Namespace> = Arc::new(TestNamespace { inum: 0x4000_0100 });
        let first = ProcNsDirOps {
            fs_id: FsId(11),
            weak_sb: Weak::new(),
            pid: 101,
        }
        .ns_backing_inode(Arc::clone(&namespace));
        let second = ProcNsDirOps {
            fs_id: FsId(11),
            weak_sb: Weak::new(),
            pid: 202,
        }
        .ns_backing_inode(namespace);

        assert_eq!(first.ino(), second.ino());
        assert_eq!(first.ino(), proc_ns_backing_ino(0x4000_0100));
        assert_eq!(first.kind(), FileType::Regular);
    }

    #[test]
    fn maps_header_annotates_stack_and_vdso() {
        let mut out = String::new();
        let mut first_exec = true;
        vma_maps_header(
            &mut out,
            Some("/bin/sh"),
            &mut first_exec,
            &(0x1000..0x2000),
            VmFlags::from_bits(VmFlags::READ | VmFlags::EXEC),
        );
        assert!(out.contains("/bin/sh"));

        let mut out = String::new();
        vma_maps_header(
            &mut out,
            Some("/bin/sh"),
            &mut first_exec,
            &(0x2000..0x3000),
            VmFlags::from_bits(VmFlags::READ | VmFlags::EXEC),
        );
        assert!(out.contains("[vdso]"));

        let mut out = String::new();
        let mut first_exec = true;
        vma_maps_header(
            &mut out,
            None,
            &mut first_exec,
            &(0x3000..0x4000),
            VmFlags::from_bits(VmFlags::READ | VmFlags::WRITE | VmFlags::GROWS_DOWN),
        );
        assert!(out.contains("[stack]"));
    }

    #[test]
    fn extra_sysctl_inodes_are_stable_and_distinct() {
        assert_ne!(extra_sysctl_ino("hostname"), extra_sysctl_ino("panic"));
        assert_eq!(extra_sysctl_ino("hostname"), extra_sysctl_ino("hostname"));
        assert!(extra_sysctl_is_writable("hostname"));
        assert!(!extra_sysctl_is_writable("osrelease"));
    }

    #[test]
    fn sched_policy_linux_id_matches_uapi_numbers() {
        assert_eq!(sched_policy_linux_id(SchedPolicy::Fair), 0);
        assert_eq!(sched_policy_linux_id(SchedPolicy::RtFifo), 1);
        assert_eq!(sched_policy_linux_id(SchedPolicy::RtRoundRobin), 2);
        assert_eq!(sched_policy_linux_id(SchedPolicy::Idle), 5);
        assert_eq!(sched_policy_linux_id(SchedPolicy::Deadline), 6);
    }
}

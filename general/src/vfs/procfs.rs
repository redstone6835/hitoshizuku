//! procfs：`/proc` 虚拟文件系统。
//!
//! 本模块提供进程、挂载、内存和设备等运行时状态的文本视图。设备相关视图通过
//! function 注册表的兼容层 helper 获取字符/块设备快照，不直接依赖具体 function 类型。

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, Ordering};

use mm::VmFlags;
use sched::ids::{Capability as SchedCapability, Credentials as SchedCredentials};
use sched::{PidT, Task, TaskState};
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
use crate::dev::enumerate::PNP_DEVICES;
use crate::dev::pnp::{PnpDependency, PnpId, PnpResourceKind, PnpState};
use crate::vfs::device_files::projection::render_function_projection_diagnostics;
use crate::vfs::user_api::device_numbers::{self, DeviceNumberKind};

static PROCFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);
static HOTPLUG_PATH: Spinlock<String> = Spinlock::new(String::new());
static FILE_MAX: AtomicU64 = AtomicU64::new(i64::MAX as u64);
static KERNEL_TAINT_FLAGS: AtomicU64 = AtomicU64::new(0);

// ── /proc/net 数据源（由内核 net_runtime 安装）───────────────────────────────

static ROUTE_SNAPSHOT_PROVIDER: Spinlock<Option<fn() -> Vec<net::control::RouteEntry>>> =
    Spinlock::new(None);
static NEIGHBOR_SNAPSHOT_PROVIDER:
    Spinlock<Option<fn() -> Vec<net::control::NeighborSnapshotEntry>>> = Spinlock::new(None);
static DNS_SNAPSHOT_PROVIDER: Spinlock<Option<fn() -> Vec<net::IpAddr>>> = Spinlock::new(None);

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

const PROC_DYNAMIC_BASE: u64 = 1_000_000;
const PROC_FD_BASE: u64 = 10_000_000_000;

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
const TASK_SLOT_NS_DIR: u64 = 11;
const TASK_SLOT_TASK_DIR: u64 = 13;
const TASK_SLOT_MOUNTINFO: u64 = 14;
const TASK_SLOT_MOUNTS: u64 = 15;
const TASK_SLOT_FDINFO_DIR: u64 = 16;

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
    Uptime,
    Stat,
    Interrupts,
    Devices,
    Pnp,
    DeviceFunctions,
    TaskSnapshot,
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
}

#[derive(Clone, Copy)]
enum ProcFileKind {
    Root(RootFileKind),
    Task { pid: PidT, kind: TaskFileKind },
    SysHotplug,
    SysPidMax,
    SysFileMax,
    SysSchedRtPeriod,
    SysSchedRtRuntime,
    SysSchedRrTimeslice,
    SysPipeMaxSize,
    SysTainted,
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
    Route,
    Unix,
    Arp,
    Sockstat,
    Dns,
}

impl ProcNetSnapshotKind {
    const ALL: [Self; 7] = [
        Self::Tcp,
        Self::Udp,
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
            }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.name() == name)
    }

    fn render(self) -> String {
        match self {
            Self::Tcp => render_proc_net_tcp(),
            Self::Udp => render_proc_net_udp(),
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
        format_args!("{:02X}{:02X}{:02X}{:02X}:{:04X}", address.0[3], address.0[2], address.0[1], address.0[0], port),
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
        1 => 0x01, // ESTABLISHED
        2 => 0x02, // SYN_SENT
        3 => 0x03, // SYN_RECV
        4 => 0x04, // FIN_WAIT1
        5 => 0x05, // FIN_WAIT2
        6 => 0x06, // TIME_WAIT
        7 => 0x07, // CLOSE
        8 => 0x08, // CLOSE_WAIT
        9 => 0x09, // LAST_ACK
        10 => 0x0a, // LISTEN
        11 => 0x0b, // CLOSING
        _ => 0x07,
    }
}

fn render_proc_net_tcp_lines(sockets: &[net::InetSocketSnapshot]) -> alloc::string::String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode"
    );
    for (index, socket) in sockets.iter().enumerate() {
        if socket.kind != net::SocketKind::Stream {
            continue;
        }
        let local = socket.local.map(proc_endpoint).unwrap_or_else(|| "00000000:0000".into());
        let peer = socket.peer.map(proc_endpoint).unwrap_or_else(|| "00000000:0000".into());
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

fn render_proc_net_udp_lines(sockets: &[net::InetSocketSnapshot]) -> alloc::string::String {
    use alloc::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode"
    );
    for (index, socket) in sockets.iter().enumerate() {
        if socket.kind != net::SocketKind::Datagram {
            continue;
        }
        let local = socket.local.map(proc_endpoint).unwrap_or_else(|| "00000000:0000".into());
        let peer = socket.peer.map(proc_endpoint).unwrap_or_else(|| "00000000:0000".into());
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
            iface,
            destination_raw,
            gateway_raw,
            flags,
            route.metric,
            mask,
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
    render_proc_net_tcp_lines(&net::snapshot_inet_sockets())
}

fn render_proc_net_udp() -> String {
    render_proc_net_udp_lines(&net::snapshot_inet_sockets())
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
    let unix_total = socket::snapshot_sockets().len();
    let total = tcp_total + udp_total + raw_total + unix_total;
    let _ = writeln!(out, "sockets: used {}", total);
    let _ = writeln!(
        out,
        "TCP: inuse {} orphan 0 tw 0 alloc {} mem 0",
        tcp_total, tcp_total
    );
    let _ = writeln!(out, "UDP: inuse {} mem 0", udp_total);
    let _ = writeln!(out, "RAW: inuse {}", raw_total);
    let _ = writeln!(out, "FRAG: inuse 0 memory 0");
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
                    ino: SYS_FILE_MAX_INO,
                    name: SmallStr::new("file-max"),
                    kind: FileType::Regular,
                },
                DirEntry {
                    ino: SYS_PIPE_MAX_SIZE_INO,
                    name: SmallStr::new("pipe-max-size"),
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
        0o444,
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
    mk_inode(
        fs_id,
        weak_sb,
        proc_task_file_ino(pid, kind),
        FileType::Regular,
        0o444,
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
                ino: proc_fd_dir_ino(self.pid),
                name: SmallStr::new("fd"),
                kind: FileType::Directory,
            },
        ];
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
        Ok(proc_fdinfo_file_inode(self.fs_id, &self.weak_sb, self.pid, fd))
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

fn proc_fdinfo_file_inode(fs_id: FsId, weak_sb: &Weak<Superblock>, pid: PidT, fd: u32) -> Arc<Inode> {
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
        let file = fdt.get_file(Fd::from_raw(self.fd)).ok_or(VfsError::NotFound)?;
        let mut out = alloc::string::String::new();
        let _ = writeln!(out, "pos:\t{}", file.pos());
        let _ = writeln!(out, "flags:\t{:o}", file.status_flags());
        let _ = writeln!(out, "mnt_id:\t0");
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
            RootFileKind::Uptime => render_uptime().into_bytes(),
            RootFileKind::Stat => render_stat().into_bytes(),
            RootFileKind::Interrupts => render_interrupts().into_bytes(),
            RootFileKind::Devices => render_devices().into_bytes(),
            RootFileKind::Pnp => render_pnp().into_bytes(),
            RootFileKind::DeviceFunctions => render_device_functions().into_bytes(),
            RootFileKind::TaskSnapshot => return render_task_snapshot(),
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
    }
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
    let cap_inh = creds.cap_inheritable.raw() & LINUX_CAP_VALID_MASK;
    let cap_prm = creds.cap_permitted.raw() & LINUX_CAP_VALID_MASK;
    let cap_eff = creds.caps.raw() & LINUX_CAP_VALID_MASK;
    let cap_bnd = creds.cap_bset.raw() & LINUX_CAP_VALID_MASK;
    let seccomp = task
        .ext_lookup(crate::syscall::TASKEXT_SECCOMP)
        .and_then(|payload| payload.downcast::<crate::seccomp::SeccompState>().ok())
        .map(|state| state.mode())
        .unwrap_or(0);
    format!(
        "Name:\t{}\nState:\t{} ({})\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nUid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nFDSize:\t{}\nVmSize:\t{} kB\nVmRSS:\t{} kB\nVmData:\t{} kB\nThreads:\t{}\nCapInh:\t{:016x}\nCapPrm:\t{:016x}\nCapEff:\t{:016x}\nCapBnd:\t{:016x}\nNoNewPrivs:\t{}\nSeccomp:\t{}\n",
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
        data / 1024,
        task_thread_count(task),
        cap_inh,
        cap_prm,
        cap_eff,
        cap_bnd,
        task.no_new_privs() as usize,
        seccomp,
    )
}

const LINUX_CAP_VALID_MASK: u64 = (1u64 << 41) - 1;

fn render_task_stat(task: &Arc<Task>) -> String {
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
    // fault 计数、信号掩码等尚未进入 sched/mm 的公共快照接口，对应字段保持 0；
    // 已有可靠来源的 CPU 时间、创建时间和内存字段必须按 Linux stat 位置导出。
    format!(
        "{} ({}) {} {} {} {} 0 0 0 0 0 0 0 {} {} {} {} 20 0 {} 0 {} {} {} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        pid,
        comm,
        state,
        ppid,
        pgrp,
        session,
        utime,
        stime,
        cutime,
        cstime,
        num_threads,
        starttime,
        vsize,
        rss_pages,
    )
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
        let perms = vm_flags_to_maps_perms(flags);
        let suffix = if flags.has(VmFlags::GROWS_DOWN) {
            " [stack]".to_string()
        } else if first_exec && flags.has(VmFlags::EXEC) {
            first_exec = false;
            exec_path
                .as_ref()
                .map(|path| format!(" {}", path))
                .unwrap_or_default()
        } else {
            String::new()
        };
        out.push_str(&format!(
            "{:016x}-{:016x} {} 00000000 00:00 0{}\n",
            range.start, range.end, perms, suffix,
        ));
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

fn render_pid_max() -> String {
    format!("{}\n", sched::pid::DEFAULT_PID_MAX)
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
    format!("MyGo kernel version 0.1.0 (loongarch64)\n")
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
    let swap_total_kb = 0usize;
    let swap_free_kb = 0usize;
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
        0usize, // TODO: 实现 Buffers（块设备缓冲区统计）
        0usize, // TODO: 汇总文件页缓存与块缓存后实现标准 Cached 字段
        0usize, // TODO: 实现 SwapCached
        kb(slab_bytes),
        0usize, // TODO: 实现 KernelStack（内核栈统计）
        0usize, // TODO: 实现 PageTables（页表统计）
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
    let mut processes = 0usize;
    let mut running = 0usize;
    let mut blocked = 0usize;
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
        }
    }
    // 当前 sched 公共接口还没有导出 CPU jiffies、上下文切换、启动时间和中断计数。
    // 这里保留字段形状并只填入可观测的进程数量，避免让兼容层反向依赖内部实现。
    format!(
        "cpu  0 0 0 0 0 0 0 0 0 0\ncpu0 0 0 0 0 0 0 0 0 0 0\n\
         intr 0\nctxt 0\nbtime 0\nprocesses {}\nprocs_running {}\nprocs_blocked {}\n",
        processes, running, blocked
    )
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
fn proc_ns_dir_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    pid: PidT,
) -> Arc<Inode> {
    mk_inode(
        fs_id,
        weak_sb,
        proc_task_base(pid) + TASK_SLOT_NS_DIR,
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
            FileType::Regular,
            0o444,
            1,
            Arc::new(ProcNsFileOps { pid: self.pid, kind }),
        )
    }
}

fn proc_ns_file_ino(pid: PidT, kind: ProcNsKind) -> u64 {
    let base = match kind {
        ProcNsKind::Uts => 0x60,
        ProcNsKind::Ipc => 0x61,
        ProcNsKind::Time => 0x62,
        ProcNsKind::Cgroup => 0x63,
        ProcNsKind::Pid => 0x64,
        ProcNsKind::Mount => 0x65,
        ProcNsKind::User => 0x66,
        ProcNsKind::Net => 0x67,
    };
    PROC_FD_BASE + pid as u64 * 1_000_000 + base
}

impl InodeOps for ProcNsDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        if name == "." || name == ".." {
            return Err(VfsError::NotFound);
        }
        let kind = ProcNsKind::ALL
            .iter()
            .find(|kind| kind.name() == name)
            .ok_or(VfsError::NotFound)?;
        Ok(self.ns_file_inode(*kind))
    }

    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve(8)
            .map_err(|_| VfsError::NoSpace)?;
        for kind in ProcNsKind::ALL {
            snapshot.push(DirEntry {
                ino: proc_ns_file_ino(self.pid, kind),
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

/// `/proc/<pid>/ns/<type>` 文件：打开时经 provider 取命名空间。
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
        Ok(super::nsfs::ns_file_content(namespace.as_ref()))
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}


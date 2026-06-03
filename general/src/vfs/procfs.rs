//! Procfs: /proc virtual filesystem.

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

use crate::mm::vm_space::dump_vmas;
use crate::mm::{VmSpace, page_size};

use super::{current_vfs_context, namespace_path};

static PROCFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);
static HOTPLUG_PATH: Spinlock<String> = Spinlock::new(String::new());

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
const TASK_SLOT_TASK_DIR: u64 = 13;
const TASK_SLOT_MOUNTINFO: u64 = 14;

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
    Devices,
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
}

#[derive(Clone, Copy)]
enum ProcFileKind {
    Root(RootFileKind),
    Task { pid: PidT, kind: TaskFileKind },
    SysHotplug,
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
        ("devices", mk_root_file(DEVICES_INO, RootFileKind::Devices)),
        ("self", self_inode),
        ("thread-self", thread_self_inode),
        ("sys", sys_inode),
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
        let mut snapshot: Vec<DirEntry> = self
            .entries
            .iter()
            .map(|(name, inode)| DirEntry {
                ino: inode.ino(),
                name: SmallStr::new(name),
                kind: inode.kind(),
            })
            .collect();
        for pid in snapshot_root_processes() {
            snapshot.push(DirEntry {
                ino: proc_task_dir_ino(pid, TaskDirView::Process),
                name: SmallStr::new(&format!("{}", pid)),
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

impl InodeOps for ProcSysDirOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        match name {
            "kernel" => Ok(proc_sys_kernel_dir_inode(self.fs_id, &self.weak_sb)),
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
            snapshot: vec![DirEntry {
                ino: SYS_KERNEL_INO,
                name: SmallStr::new("kernel"),
                kind: FileType::Directory,
            }],
        }))
    }

    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
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
            snapshot: vec![DirEntry {
                ino: SYS_HOTPLUG_INO,
                name: SmallStr::new("hotplug"),
                kind: FileType::Regular,
            }],
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
        }
}

fn proc_fd_dir_ino(pid: PidT) -> u64 {
    proc_task_base(pid) + TASK_SLOT_FD_DIR
}

fn proc_task_list_ino(pid: PidT) -> u64 {
    proc_task_base(pid) + TASK_SLOT_TASK_DIR
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
            "fd" => Ok(proc_fd_dir_inode(self.fs_id, &self.weak_sb, self.pid)),
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
        let snapshot = tids
            .into_iter()
            .map(|tid| DirEntry {
                ino: proc_task_dir_ino(tid, TaskDirView::Thread),
                name: SmallStr::new(&format!("{}", tid)),
                kind: FileType::Directory,
            })
            .collect();
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
        let snapshot = fds
            .into_iter()
            .map(|(fd, _)| DirEntry {
                ino: proc_fd_link_ino(self.pid, fd),
                name: SmallStr::new(&format!("{}", fd)),
                kind: FileType::Symlink,
            })
            .collect();
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
    fn poll(&self, _: PollEvents) -> PollEvents {
        PollEvents(0)
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
        Ok(Box::new(ProcRegularFile { kind: self.kind }))
    }

    fn truncate(&self, _: &Inode, size: u64) -> VfsResult<()> {
        match self.kind {
            ProcFileKind::SysHotplug if size == 0 => {
                HOTPLUG_PATH.lock().clear();
                Ok(())
            }
            ProcFileKind::SysHotplug => Err(VfsError::InvalidArgument),
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
}

impl FileOps for ProcRegularFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
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
            _ => Err(VfsError::ReadOnlyFilesystem),
        }
    }

    fn readdir(&self, _: u64, _: &mut dyn FnMut(DirEntry) -> ControlFlow<()>) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, _: PollEvents) -> PollEvents {
        PollEvents(0)
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
            RootFileKind::Devices => render_devices().into_bytes(),
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
            })
        }
        ProcFileKind::SysHotplug => Ok(render_hotplug().into_bytes()),
    }
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

fn basename(path: &str) -> &str {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
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

fn task_memory_usage(task: &Arc<Task>) -> (u64, u64) {
    let Some(vm) = task_vm_space(task) else {
        return (0, 0);
    };
    let vsize = dump_vmas(&vm).into_iter().fold(0u64, |acc, (range, _)| {
        acc.saturating_add((range.end - range.start) as u64)
    });
    let rss = vm.mapped_pages() as u64 * page_size() as u64;
    (vsize, rss)
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
    let (vsize, rss) = task_memory_usage(task);
    format!(
        "Name:\t{}\nState:\t{} ({})\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nUid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nFDSize:\t{}\nVmSize:\t{} kB\nVmRSS:\t{} kB\nThreads:\t{}\n",
        name,
        task_state_char(state),
        task_state_name(state),
        tgid,
        pid,
        ppid,
        creds.uid.0,
        creds.euid.0,
        creds.suid.0,
        creds.euid.0,
        creds.gid.0,
        creds.egid.0,
        creds.sgid.0,
        creds.egid.0,
        fd_count,
        vsize / 1024,
        rss / 1024,
        task_thread_count(task),
    )
}

fn render_task_stat(task: &Arc<Task>) -> String {
    let pid = task.pid_root().unwrap_or(0);
    let comm = render_task_comm(task).trim_end().to_string();
    let state = task_state_char(task.state());
    let ppid = task_parent_pid(task);
    let pgrp = task_pgrp(task);
    let session = task_session(task);
    let num_threads = task_thread_count(task);
    let (vsize, rss_bytes) = task_memory_usage(task);
    let rss_pages = rss_bytes / page_size() as u64;
    // TODO: 填充真实的 tty_nr, minflt, cminflt, majflt, cmajflt, utime, stime,
    //       cutime, cstime, priority, nice, starttime, signal, blocked, sigignore, sigcatch 等字段
    format!(
        "{} ({}) {} {} {} {} 0 0 0 0 0 0 0 0 0 0 0 20 0 {} 0 0 {} {} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        pid, comm, state, ppid, pgrp, session, num_threads, vsize, rss_pages,
    )
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
    if let Some(args) = task_exec_args(task)
        && let Some(argv0) = args.first()
        && !argv0.is_empty()
    {
        return format!("{}\n", basename(argv0));
    }
    let name = task_exec_path(task)
        .map(|path| basename(&path).to_string())
        .unwrap_or_else(|_| String::from("unknown"));
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

fn render_hotplug() -> String {
    let value = HOTPLUG_PATH.lock();
    if value.is_empty() {
        String::new()
    } else {
        format!("{}\n", &*value)
    }
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

// TODO: 从硬件读取真实 CPU 特征标志、BogoMIPS、cache 信息等
fn render_cpuinfo() -> String {
    let mut out = String::new();
    let mut online_mask = sched::online_cpu_mask();
    if online_mask == 0 {
        online_mask = 1;
    }
    for cpu_id in 0..sched::NR_CPUS {
        if (online_mask & (1u64 << cpu_id)) == 0 || !sched::is_cpu_online(cpu_id) {
            continue;
        }
        let _ = write!(
            out,
            "processor\t: {cpu_id}\n\
             vendor_id\t: MyGo\n\
             cpu family\t: LoongArch\n\
             model name\t: LoongArch64 Virtual CPU\n\
             CPU architecture\t: loongarch64\n\
             isa\t\t: loongarch64\n\
             fpu\t\t: yes\n\
             BogoMIPS\t: 100.00\n\n"
        );
    }
    if out.is_empty() {
        let _ = write!(
            out,
            "processor\t: 0\n\
             vendor_id\t: MyGo\n\
             cpu family\t: LoongArch\n\
             model name\t: LoongArch64 Virtual CPU\n\
             CPU architecture\t: loongarch64\n\
             isa\t\t: loongarch64\n\
             fpu\t\t: yes\n\
             BogoMIPS\t: 100.00\n\n"
        );
    }
    out
}

fn render_meminfo() -> String {
    let overview = allocator::KERNEL_ALLOCATOR.detailed_stats();
    let kb = |bytes: usize| -> usize { bytes / 1024 };
    format!(
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
         BootFree:       {:>8} kB\n",
        kb(overview.total_physical),
        kb(overview.free_physical),
        kb(overview.free_physical),
        0usize,                        // TODO: 实现 Buffers（块设备缓冲区统计）
        0usize,                        // TODO: 实现 Cached（页缓存统计）
        0usize,                        // TODO: 实现 SwapCached
        kb(overview.kernel_heap_used), // Slab: 暂用 kernel heap 近似
        0usize,                        // TODO: 实现 KernelStack（内核栈统计）
        0usize,                        // TODO: 实现 PageTables（页表统计）
        kb(overview.kernel_vmem_total),
        kb(overview.kernel_vmem_allocated),
        kb(overview.kernel_vmem_free),
        0usize, // TODO: 实现 SwapTotal
        0usize, // TODO: 实现 SwapFree
        kb(overview.direct_map_total),
        kb(overview.direct_map_allocated),
        kb(overview.direct_map_free),
        kb(overview.reserved_physical),
        kb(overview.kernel_heap_used),
        kb(overview.boot_used),
        kb(overview.boot_free),
    )
}

fn render_uptime() -> String {
    let ns = sched::now_ns_public();
    let secs = ns / 1_000_000_000;
    // TODO: 第二个字段是 idle 时间，需要从调度器读取累计 idle 累积值
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
    // TODO: 实现 cpu jiffies 统计（user/nice/system/idle/iowait/irq/softirq/steal/guest/guest_nice）
    //       以及 intr、ctxt、btime；当前所有字段为 0
    format!(
        "cpu  0 0 0 0 0 0 0 0 0 0\ncpu0 0 0 0 0 0 0 0 0 0 0\n\
         intr 0\nctxt 0\nbtime 0\nprocesses {}\nprocs_running {}\nprocs_blocked {}\n",
        processes, running, blocked
    )
}

fn render_devices() -> String {
    // TODO: 当前所有 char/block 设备硬编码 major=254，需要从设备注册表中读取真实主设备号
    let mut out = String::from("Character devices:\n");
    for dev in crate::dev::enumerate::DEVICES.char_devs.iter() {
        out.push_str(&format!("  254 {}\n", dev.fw_name()));
    }
    out.push_str("\nBlock devices:\n");
    if let Ok(devs) = crate::dev::enumerate::DEVICES.block_devs.list() {
        for dev in &devs {
            out.push_str(&format!("  254 {}\n", dev.name()));
        }
    }
    out
}

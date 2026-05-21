//! Procfs: /proc virtual filesystem.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, Ordering};

use vfs::FS_REGISTRY;
use vfs::cred::{Credentials, Gid, Uid};
use vfs::dentry::{Dentry, SmallStr};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::mount::MountFlags;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{FsDriver, FsDriverFlags, Superblock, SuperblockOps};

use super::current_vfs_context;

static PROCFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);

pub struct ProcFsDriver;

impl FsDriver for ProcFsDriver {
    fn name(&self) -> &'static str { "proc" }
    fn flags(&self) -> FsDriverFlags { FsDriverFlags::NODEV.with(FsDriverFlags::SINGLE) }

    fn mount(&self, _dev: Option<&str>, _data: &str) -> VfsResult<Arc<Superblock>> {
        let fs_id = FsId::new(PROCFS_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));
        Ok(Superblock::new(|weak_sb| {
            let now = Timespec::now();
            let root_inode = root_inode(fs_id, &weak_sb, now);
            let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));
            Superblock {
                fs_type: "proc", fs_id, dev_id: None, block_size: 4096, name_max: 255,
                root_inode, root_dentry,
                inode_cache: vfs::superblock::InodeCache::new(),
                ops: Box::new(ProcSuperblockOps), self_weak: weak_sb,
            }
        }))
    }

    fn kill_sb(&self, _sb: Arc<Superblock>) {}
    fn as_any(&self) -> &dyn core::any::Any { self }
}

struct ProcSuperblockOps;
impl SuperblockOps for ProcSuperblockOps {
    fn alloc_inode(&self, _: &Arc<Superblock>) -> VfsResult<Arc<Inode>> { Err(VfsError::ReadOnlyFilesystem) }
    fn write_inode(&self, _: &Arc<Inode>) -> VfsResult<()> { Ok(()) }
    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Ok(FsStat { fs_type: 0x9fa0, block_size: sb.block_size as u64,
            total_blocks: 0, free_blocks: 0, avail_blocks: 0,
            total_inodes: 0, free_inodes: 0, fs_id: sb.fs_id.raw(), name_max: sb.name_max })
    }
    fn sync_fs(&self, _: &Arc<Superblock>) -> VfsResult<()> { Ok(()) }
    fn remount(&self, _: &Arc<Superblock>, _: MountFlags) -> VfsResult<()> { Ok(()) }
    fn as_any(&self) -> &dyn core::any::Any { self }
}

#[derive(Clone, Copy)]
enum ProcFileKind { Filesystems, Mounts, Version, CpuInfo, MemInfo, Uptime, Stat, Devices }

fn root_inode(fs_id: FsId, weak_sb: &alloc::sync::Weak<Superblock>, now: Timespec) -> Arc<Inode> {
    let mk = |ino, kind: ProcFileKind| {
        let meta = InodeMeta { size: 0, nlink: 1, mode: FileMode::new(0o444),
            uid: Uid::ROOT, gid: Gid::ROOT, atime: now, mtime: now, ctime: now, blocks: 0 };
        Inode::new(InodeId { fs_id, ino }, FileType::Regular, DevId::new(0, 0), 4096,
            None, meta, Arc::new(ProcFileOps { kind }), weak_sb.clone())
    };
    let self_ino = Inode::new(InodeId { fs_id, ino: 10 }, FileType::Symlink,
        DevId::new(0, 0), 4096, None,
        InodeMeta { size: 0, nlink: 1, mode: FileMode::new(0o777), uid: Uid::ROOT,
            gid: Gid::ROOT, atime: now, mtime: now, ctime: now, blocks: 0 },
        Arc::new(ProcSelfOps), weak_sb.clone());
    let entries: Vec<(&str, Arc<Inode>)> = vec![
        ("filesystems", mk(2, ProcFileKind::Filesystems)),
        ("mounts",      mk(3, ProcFileKind::Mounts)),
        ("version",     mk(4, ProcFileKind::Version)),
        ("cpuinfo",     mk(5, ProcFileKind::CpuInfo)),
        ("meminfo",     mk(6, ProcFileKind::MemInfo)),
        ("uptime",      mk(7, ProcFileKind::Uptime)),
        ("stat",        mk(8, ProcFileKind::Stat)),
        ("devices",     mk(9, ProcFileKind::Devices)),
        ("self",        self_ino),
    ];
    let root_meta = InodeMeta { size: 4096, nlink: 2, mode: FileMode::new(0o555),
        uid: Uid::ROOT, gid: Gid::ROOT, atime: now, mtime: now, ctime: now, blocks: 0 };
    Inode::new(InodeId { fs_id, ino: 1 }, FileType::Directory, DevId::new(0, 0), 4096,
        None, root_meta, Arc::new(ProcRootOps { entries }), weak_sb.clone())
}

struct ProcRootOps { entries: Vec<(&'static str, Arc<Inode>)> }
impl InodeOps for ProcRootOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        self.entries.iter().find(|(n, _)| *n == name).map(|(_, i)| Arc::clone(i)).ok_or(VfsError::NotFound)
    }
    fn open(&self, _: &Inode, _: &OpenOptions, _: &Credentials) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(ProcDirFile {
            snapshot: self.entries.iter().map(|(n, i)| DirEntry { ino: i.ino(), name: SmallStr::new(n), kind: i.kind() }).collect(),
        }))
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> { Err(VfsError::InvalidArgument) }
    fn as_any(&self) -> &dyn core::any::Any { self }
}

struct ProcSelfOps;
impl InodeOps for ProcSelfOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> { Err(VfsError::NotADirectory) }
    fn open(&self, _: &Inode, _: &OpenOptions, _: &Credentials) -> VfsResult<Box<dyn FileOps + Send + Sync>> { Err(VfsError::NotFound) }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        if !sched::is_ready() { return Err(VfsError::NotFound); }
        let pid = sched::current_task().pid_root().ok_or(VfsError::NotFound)?;
        Ok(format!("{}", pid))
    }
    fn as_any(&self) -> &dyn core::any::Any { self }
}

struct ProcDirFile { snapshot: Vec<DirEntry> }
impl FileOps for ProcDirFile {
    fn read_at(&self, _: &mut [u8], _: u64) -> VfsResult<usize> { Err(VfsError::IsADirectory) }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> { Err(VfsError::IsADirectory) }
    fn readdir(&self, pos: u64, sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>) -> VfsResult<u64> {
        let start = pos as usize;
        for (i, e) in self.snapshot.iter().enumerate().skip(start) {
            if sink(e.clone()).is_break() { return Ok(i as u64); }
        }
        Ok(self.snapshot.len() as u64)
    }
    fn sync(&self) -> VfsResult<()> { Ok(()) }
    fn poll(&self, _: PollEvents) -> PollEvents { PollEvents(0) }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any { self }
}

struct ProcFileOps { kind: ProcFileKind }
impl InodeOps for ProcFileOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> { Err(VfsError::NotADirectory) }
    fn open(&self, _: &Inode, _: &OpenOptions, _: &Credentials) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(ProcRegularFile { kind: self.kind }))
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> { Err(VfsError::InvalidArgument) }
    fn as_any(&self) -> &dyn core::any::Any { self }
}

struct ProcRegularFile { kind: ProcFileKind }
impl FileOps for ProcRegularFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let content = match self.kind {
            ProcFileKind::Filesystems => render_filesystems(),
            ProcFileKind::Mounts      => render_mounts(),
            ProcFileKind::Version    => render_version(),
            ProcFileKind::CpuInfo    => render_cpuinfo(),
            ProcFileKind::MemInfo    => render_meminfo(),
            ProcFileKind::Uptime     => render_uptime(),
            ProcFileKind::Stat       => render_stat(),
            ProcFileKind::Devices    => render_devices(),
        };
        slice_str(buf, offset, &content)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> { Err(VfsError::ReadOnlyFilesystem) }
    fn readdir(&self, _: u64, _: &mut dyn FnMut(DirEntry) -> ControlFlow<()>) -> VfsResult<u64> { Err(VfsError::NotADirectory) }
    fn sync(&self) -> VfsResult<()> { Ok(()) }
    fn poll(&self, _: PollEvents) -> PollEvents { PollEvents(0) }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any { self }
}

fn slice_str(buf: &mut [u8], offset: u64, content: &str) -> VfsResult<usize> {
    if offset > usize::MAX as u64 { return Ok(0); }
    let bytes = content.as_bytes();
    let start = (offset as usize).min(bytes.len());
    let end = start.saturating_add(buf.len()).min(bytes.len());
    let n = end - start;
    buf[..n].copy_from_slice(&bytes[start..end]);
    Ok(n)
}

fn render_filesystems() -> String {
    let mut out = String::new();
    for entry in FS_REGISTRY.iter() {
        if entry.driver.flags().has(FsDriverFlags::NODEV) { out.push_str("nodev\t"); } else { out.push('\t'); }
        out.push_str(entry.driver.name());
        out.push('\n');
    }
    out
}

fn render_mounts() -> String {
    current_vfs_context().map(|ctx| ctx.mount_ns.dump_mounts()).unwrap_or_default()
}

fn render_version() -> String {
    format!("MyGo kernel version 0.1.0 (loongarch64)\n")
}

fn render_cpuinfo() -> String {
    format!(
        "processor\t: 0\ncpu family\t: loongarch64\nvendor_id\t: QEMU Virtual CPU\n\
         model name\t: QEMU Virtual CPU version 2.5+\nCPU architecture: loongarch64\n\
         fpu\t\t: yes\nBogoMIPS\t: 100.00\n\n")
}

fn render_meminfo() -> String {
    format!(
        "MemTotal:       {:>8} kB\nMemFree:        {:>8} kB\nMemAvailable:   {:>8} kB\n\
         Buffers:        {:>8} kB\nCached:         {:>8} kB\n\
         SwapTotal:             0 kB\nSwapFree:              0 kB\n",
        1024*1024u64, 512*1024u64, 512*1024u64, 0u64, 0u64)
}

fn render_uptime() -> String {
    let ns = sched::now_ns_public();
    let secs = ns / 1_000_000_000;
    format!("{}.{:02} {}.{:02}\n", secs, (ns % 1_000_000_000) / 10_000_000, 0u64, 0u64)
}

fn render_stat() -> String {
    let ns = sched::now_ns_public();
    format!(
        "cpu  0 0 0 0 0 0 0 0 0 0\ncpu0 0 0 0 0 0 0 0 0 0 0\n\
         intr 0\nctxt 0\nbtime {}\nprocesses 1\nprocs_running 1\nprocs_blocked 0\n",
        ns / 1_000_000_000)
}

fn render_devices() -> String {
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
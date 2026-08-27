//! devpts 伪终端文件系统。
//!
//! 与 Linux 的 devpts 对应:每挂载点独立实例(Linux newinstance 语义),
//! 挂载于 `/dev/pts`;`/dev/pts/N` 节点在 ptmx open 分配 pty 对时动态创建,
//! 配对销毁(两端全部关闭)时删除。节点打开即复用现有字符设备/TTY 投影,
//! 设备身份仍由 [`PtyPair`] 持有,本文件系统只做节点投影。

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, Ordering};

use alloc::boxed::Box;
use vfs::dentry::{Dentry, SmallStr};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::mount::MountFlags;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{FsDriver, FsDriverFlags, Superblock, SuperblockOps};
use vfs::sync::Spinlock;

use crate::dev::tty::pty::{self, PtyPair};
use crate::vfs::user_api::device_numbers;

const DEVPTS_NAME_MAX: usize = 255;

/// 挂载选项(当前只消费权限相关;与 Linux 同键名)。
#[derive(Clone, Copy)]
struct DevPtsOptions {
    root_mode: FileMode,
    node_mode: FileMode,
}

impl DevPtsOptions {
    fn parse(data: &str) -> VfsResult<Self> {
        let mut options = Self {
            root_mode: FileMode::new(0o755),
            node_mode: FileMode::new(0o620),
        };
        for item in data.split(',').filter(|item| !item.is_empty()) {
            let (key, value) = item.split_once('=').unwrap_or((item, ""));
            match key {
                "mode" => options.root_mode = FileMode::new(parse_octal(value)?),
                "ptmxmode" | "gid" | "uid" | "newinstance" | "max" | "nosuid" | "nodev"
                | "noexec" | "rw" | "ro" => {}
                _ => return Err(VfsError::InvalidArgument),
            }
        }
        Ok(options)
    }
}

fn parse_octal(value: &str) -> VfsResult<u16> {
    if value.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    let mut result = 0u16;
    for byte in value.bytes() {
        if !(b'0'..=b'7').contains(&byte) {
            return Err(VfsError::InvalidArgument);
        }
        result = result
            .checked_mul(8)
            .and_then(|value| value.checked_add((byte - b'0') as u16))
            .ok_or(VfsError::InvalidArgument)?;
    }
    Ok(result)
}

/// 已挂载的 devpts 实例(节点创建/删除需要广播到所有实例)。
static DEVPTS_INSTANCES: Spinlock<Vec<Weak<Superblock>>> = Spinlock::new(Vec::new());

fn register_instance(sb: &Arc<Superblock>) {
    let mut instances = DEVPTS_INSTANCES.lock();
    instances.retain(|weak| weak.upgrade().is_some());
    instances.push(Arc::downgrade(sb));
}

fn live_instances() -> Vec<Arc<Superblock>> {
    let mut out = Vec::new();
    {
        let mut instances = DEVPTS_INSTANCES.lock();
        instances.retain(|weak| {
            let Some(sb) = weak.upgrade() else {
                return false;
            };
            out.push(sb);
            true
        });
    }
    out
}

// ── inode 操作 ───────────────────────────────────────────────────────────────

/// devpts 根目录:子项 = 活动 pty 的 slave 节点。
struct DevPtsRootOps {
    nodes: Spinlock<BTreeMap<String, Arc<Inode>>>,
}

impl DevPtsRootOps {
    fn new() -> Self {
        Self {
            nodes: Spinlock::new(BTreeMap::new()),
        }
    }
}

impl InodeOps for DevPtsRootOps {
    fn lookup(&self, _inode: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        self.nodes
            .lock()
            .get(name)
            .cloned()
            .ok_or(VfsError::NotFound)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &vfs::cred::Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let nodes = self.nodes.lock();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve(nodes.len())
            .map_err(|_| VfsError::OutOfMemory)?;
        for (name, inode) in nodes.iter() {
            snapshot.push(DirEntry {
                ino: inode.ino(),
                name: SmallStr::new(name),
                kind: inode.kind(),
            });
        }
        drop(nodes);
        Ok(Box::new(DevPtsDirFileOps { snapshot }))
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// devpts 根目录的文件操作(打开时快照节点列表)。
struct DevPtsDirFileOps {
    snapshot: Vec<DirEntry>,
}

impl FileOps for DevPtsDirFileOps {
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

    fn poll(&self, interest: PollEvents) -> PollEvents {
        PollEvents::READ_WRITE_READY.intersect(interest)
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// 单个 `/dev/pts/N` 节点:open 时按编号解析 pty 对并复用字符设备投影。
struct DevPtsNodeOps {
    index: u32,
}

impl InodeOps for DevPtsNodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        opts: &OpenOptions,
        _cred: &vfs::cred::Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let Some(pair) = pty::lookup_pair(self.index) else {
            return Err(VfsError::NoSuchDeviceOrAddress);
        };
        if pair.is_locked() {
            // TIOCSPTLCK 锁定后打开 slave 返回 EIO(Linux 语义)。
            return Err(VfsError::Io);
        }
        let dev = pair.slave_char_device()?;
        crate::vfs::devtmpfs::char_dev_file_ops(dev, opts.nonblock)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── superblock 操作 ──────────────────────────────────────────────────────────

struct DevPtsSuperblockOps {
    fs_id: FsId,
    sb: Spinlock<Option<Weak<Superblock>>>,
    next_ino: AtomicU64,
    options: DevPtsOptions,
}

impl SuperblockOps for DevPtsSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Ok(FsStat {
            fs_type: 0x6470_7473, // "dpts"
            block_size: 512,
            total_blocks: 0,
            free_blocks: 0,
            avail_blocks: 0,
            total_inodes: self.next_ino.load(Ordering::Relaxed),
            free_inodes: 0,
            fs_id: sb.fs_id.raw(),
            name_max: DEVPTS_NAME_MAX as u32,
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

impl DevPtsSuperblockOps {
    fn alloc_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    /// 在实例中创建 `/dev/pts/N` 节点。
    fn publish_slave(&self, pair: &Arc<PtyPair>) -> VfsResult<()> {
        let sb = self
            .sb
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(VfsError::NoDevice)?;
        let root = sb.root_inode.clone();
        let ops = root
            .downcast_ops::<DevPtsRootOps>()
            .ok_or(VfsError::InvalidArgument)?;
        let index = pair.index();
        let mut name = String::new();
        name.try_reserve(8).ok();
        name.push_str(&index.to_string());
        let inode = Inode::new(
            InodeId {
                fs_id: self.fs_id,
                ino: self.alloc_ino(),
            },
            FileType::CharDevice,
            device_numbers::pty_rdev(index),
            DEVPTS_NAME_MAX as u32,
            None,
            InodeMeta {
                size: 0,
                nlink: 1,
                mode: self.options.node_mode,
                uid: vfs::cred::Uid::ROOT,
                gid: vfs::cred::Gid::ROOT,
                atime: Timespec::now(),
                mtime: Timespec::now(),
                ctime: Timespec::now(),
                blocks: 0,
            },
            Arc::new(DevPtsNodeOps { index }),
            Arc::downgrade(&sb),
        );
        let mut nodes = ops.nodes.lock();
        if nodes.contains_key(&name) {
            return Ok(());
        }
        nodes.insert(name, Arc::clone(&inode));
        Ok(())
    }

    /// 查询 `/dev/pts/N` 节点 inode。
    fn node_inode(&self, index: u32) -> Option<Arc<Inode>> {
        let mut name = String::new();
        name.try_reserve(8).ok()?;
        name.push_str(&index.to_string());
        self.sb
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)?
            .root_inode
            .downcast_ops::<DevPtsRootOps>()?
            .nodes
            .lock()
            .get(&name)
            .cloned()
    }

    /// 从实例中删除 `/dev/pts/N` 节点。
    fn unpublish_slave(&self, index: u32) {
        let Ok(sb) = self
            .sb
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(VfsError::NoDevice)
        else {
            return;
        };
        let Some(ops) = sb.root_inode.downcast_ops::<DevPtsRootOps>() else {
            return;
        };
        let mut name = String::new();
        name.try_reserve(8).ok();
        name.push_str(&index.to_string());
        ops.nodes.lock().remove(&name);
    }
}

// ── 文件系统驱动 ─────────────────────────────────────────────────────────────

pub struct DevPtsDriver;

impl FsDriver for DevPtsDriver {
    fn name(&self) -> &'static str {
        "devpts"
    }

    fn flags(&self) -> FsDriverFlags {
        FsDriverFlags::NODEV
    }

    fn mount(&self, _dev: Option<&str>, data: &str) -> VfsResult<Arc<Superblock>> {
        let options = DevPtsOptions::parse(data)?;
        let fs_id = FsId::new(0x6470_7473); // "dpts"

        let sb_ops = DevPtsSuperblockOps {
            fs_id,
            sb: Spinlock::new(None),
            next_ino: AtomicU64::new(2),
            options,
        };

        let sb = Superblock::new(move |weak_sb| {
            sb_ops.sb.lock().replace(weak_sb.clone());

            let now = Timespec::now();
            let root_meta = InodeMeta {
                size: 0,
                nlink: 2,
                mode: sb_ops.options.root_mode,
                uid: vfs::cred::Uid::ROOT,
                gid: vfs::cred::Gid::ROOT,
                atime: now,
                mtime: now,
                ctime: now,
                blocks: 0,
            };

            let root_inode = Inode::new(
                InodeId { fs_id, ino: 1 },
                FileType::Directory,
                DevId::new(0, 0),
                512,
                None,
                root_meta,
                Arc::new(DevPtsRootOps::new()) as Arc<dyn InodeOps + Send + Sync>,
                weak_sb.clone(),
            );

            let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));

            Superblock {
                fs_type: "devpts",
                fs_id,
                dev_id: None,
                block_size: 512,
                name_max: DEVPTS_NAME_MAX as u32,
                root_inode,
                root_dentry,
                inode_cache: vfs::superblock::InodeCache::new(),
                ops: Box::new(sb_ops),
                self_weak: weak_sb,
            }
        });

        // 每挂载点独立实例(Linux newinstance 语义);已存活的 pty 对补建节点。
        register_instance(&sb);
        for pair in pty::live_pairs() {
            let _ = sb
                .downcast_ops::<DevPtsSuperblockOps>()
                .and_then(|ops| ops.publish_slave(&pair).ok());
        }
        Ok(sb)
    }

    fn kill_sb(&self, _sb: Arc<Superblock>) {}

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── 节点广播 ─────────────────────────────────────────────────────────────────

/// ptmx open 分配 pty 对后,在所有已挂载 devpts 实例中创建 slave 节点。
pub fn publish_pty_slave(pair: &Arc<PtyPair>) {
    for sb in live_instances() {
        let Some(ops) = sb.downcast_ops::<DevPtsSuperblockOps>() else {
            continue;
        };
        let _ = ops.publish_slave(pair);
    }
}

/// 配对销毁时从所有实例删除 slave 节点。
pub fn unpublish_pty_slave(index: u32) {
    for sb in live_instances() {
        let Some(ops) = sb.downcast_ops::<DevPtsSuperblockOps>() else {
            continue;
        };
        ops.unpublish_slave(index);
    }
}

/// 为 TIOCGPTPEER 构造 slave 的 `File`(复用 devpts 节点 inode/dentry)。
pub fn open_slave_file(
    pair: &Arc<PtyPair>,
    opts: OpenOptions,
    cred: Arc<vfs::cred::Credentials>,
) -> VfsResult<Arc<vfs::file::File>> {
    let sb = live_instances()
        .into_iter()
        .next()
        .ok_or(VfsError::NoDevice)?;
    let index = pair.index();
    let Some(node_inode) = sb
        .downcast_ops::<DevPtsSuperblockOps>()
        .and_then(|ops| ops.node_inode(index))
    else {
        return Err(VfsError::NoSuchDeviceOrAddress);
    };
    let mut name = String::new();
    name.try_reserve(8).ok();
    name.push_str(&index.to_string());
    let dentry = Dentry::new_positive(
        &name,
        Some(Arc::clone(&sb.root_dentry)),
        Arc::clone(&node_inode),
    );
    let mount = vfs::mount::Mount::new(
        Arc::clone(&sb),
        Arc::clone(&sb.root_dentry),
        Arc::clone(&sb.root_dentry),
        vfs::mount::MountFlags::default(),
        None,
    );
    let ops = crate::vfs::devtmpfs::char_dev_file_ops(pair.slave_char_device()?, opts.nonblock)?;
    Ok(Arc::new(vfs::file::File::new(
        node_inode, opts, cred, ops, dentry, mount,
    )))
}

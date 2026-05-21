//! Tmpfs - 内存文件系统驱动。
//!
//! Tmpfs 是一个完全基于内存的文件系统，所有数据存储在 RAM 中，重启后丢失。
//! 常用于 `/tmp`、`/dev/shm` 等临时存储场景。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, Ordering};

use vfs::cred::{Credentials, Gid, Uid};
use vfs::dentry::{Dentry, SmallStr};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::mount::MountFlags;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{FsDriver, FsDriverFlags, Superblock, SuperblockOps};
use vfs::sync::Spinlock;

// ── 全局状态 ──────────────────────────────────────────────────────────────────

/// 全局 tmpfs 实例计数器，用于生成唯一的 fs_id。
static TMPFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);

// ── Tmpfs 驱动 ────────────────────────────────────────────────────────────────

/// Tmpfs 文件系统驱动。
pub struct TmpfsDriver;

impl FsDriver for TmpfsDriver {
    fn name(&self) -> &'static str {
        "tmpfs"
    }

    fn flags(&self) -> FsDriverFlags {
        FsDriverFlags::NODEV
    }

    fn mount(&self, _source: Option<&str>, _data: &str) -> VfsResult<Arc<Superblock>> {
        let fs_id = FsId::new(TMPFS_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));

        let sb = Superblock::new(|weak_sb| {
            let sb_ops = Box::new(TmpfsSuperblockOps {
                next_ino: AtomicU64::new(2),
                total_inodes: AtomicU64::new(1),
            });

            // 创建根目录 inode
            let now = Timespec::now();
            let root_meta = InodeMeta {
                size: 0,
                nlink: 2,
                mode: FileMode::new(0o755),
                uid: Uid::ROOT,
                gid: Gid::ROOT,
                atime: now,
                mtime: now,
                ctime: now,
                blocks: 0,
            };

            let root_ops = Arc::new(TmpfsInodeOps {
                data: Arc::new(Spinlock::new(TmpfsInodeData::Directory(BTreeMap::new()))),
            });

            let root_inode = Inode::new(
                InodeId { fs_id, ino: 1 },
                FileType::Directory,
                DevId::new(0, 0),
                4096,
                None,
                root_meta,
                root_ops,
                weak_sb.clone(),
            );

            let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));

            Superblock {
                fs_type: "tmpfs",
                fs_id,
                dev_id: None,
                block_size: 4096,
                name_max: 255,
                root_inode,
                root_dentry,
                inode_cache: vfs::superblock::InodeCache::new(),
                ops: sb_ops,
                self_weak: weak_sb,
            }
        });

        sb.insert_inode(Arc::clone(&sb.root_inode));
        Ok(sb)
    }

    fn kill_sb(&self, _sb: Arc<Superblock>) {
        // tmpfs 全在内存，卸载时自动释放
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── Superblock 操作 ───────────────────────────────────────────────────────────

struct TmpfsSuperblockOps {
    next_ino: AtomicU64,
    total_inodes: AtomicU64,
}

impl TmpfsSuperblockOps {
    fn alloc_ino(&self) -> u64 {
        let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
        self.total_inodes.fetch_add(1, Ordering::Relaxed);
        ino
    }
}

impl SuperblockOps for TmpfsSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Ok(FsStat {
            fs_type: 0x01021994,
            block_size: sb.block_size as u64,
            total_blocks: u64::MAX,
            free_blocks: u64::MAX,
            avail_blocks: u64::MAX,
            total_inodes: self.total_inodes.load(Ordering::Relaxed),
            free_inodes: u64::MAX,
            fs_id: sb.fs_id.raw(),
            name_max: sb.name_max,
        })
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }

    fn remount(&self, _sb: &Arc<Superblock>, _new_flags: MountFlags) -> VfsResult<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── Inode 数据 ────────────────────────────────────────────────────────────────

enum TmpfsInodeData {
    File(Vec<u8>),
    Directory(BTreeMap<String, u64>),
    Symlink(String),
}

struct TmpfsInodeOps {
    data: Arc<Spinlock<TmpfsInodeData>>,
}

impl InodeOps for TmpfsInodeOps {
    fn lookup(&self, dir: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }

        let data = self.data.lock();
        let entries = match &*data {
            TmpfsInodeData::Directory(entries) => entries,
            _ => return Err(VfsError::NotADirectory),
        };

        let ino = *entries.get(name).ok_or(VfsError::NotFound)?;
        drop(data);

        let sb = dir.superblock().ok_or(VfsError::InvalidArgument)?;
        sb.find_inode(ino).ok_or(VfsError::NotFound)
    }

    fn create(
        &self,
        dir: &Inode,
        name: &str,
        mode: FileMode,
        cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }

        let sb = dir.superblock().ok_or(VfsError::InvalidArgument)?;
        let sb_ops = sb
            .ops
            .as_any()
            .downcast_ref::<TmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;

        let mut data = self.data.lock();
        let entries = match &mut *data {
            TmpfsInodeData::Directory(entries) => entries,
            _ => return Err(VfsError::NotADirectory),
        };

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }

        let ino = sb_ops.alloc_ino();
        let now = Timespec::now();
        let meta = InodeMeta {
            size: 0,
            nlink: 1,
            mode,
            uid: cred.euid,
            gid: cred.egid,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        let new_inode = Inode::new(
            InodeId {
                fs_id: sb.fs_id,
                ino,
            },
            FileType::Regular,
            DevId::new(0, 0),
            4096,
            sb.dev_id,
            meta,
            Arc::new(TmpfsInodeOps {
                data: Arc::new(Spinlock::new(TmpfsInodeData::File(Vec::new()))),
            }),
            sb.self_weak.clone(),
        );

        entries.insert(name.to_string(), ino);
        dir.touch_mtime();
        dir.touch_ctime();
        drop(data);

        Ok(sb.insert_inode(new_inode))
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

        let sb = dir.superblock().ok_or(VfsError::InvalidArgument)?;
        let sb_ops = sb
            .ops
            .as_any()
            .downcast_ref::<TmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;

        let mut data = self.data.lock();
        let entries = match &mut *data {
            TmpfsInodeData::Directory(entries) => entries,
            _ => return Err(VfsError::NotADirectory),
        };

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }

        let ino = sb_ops.alloc_ino();
        let now = Timespec::now();
        let meta = InodeMeta {
            size: 0,
            nlink: 2,
            mode,
            uid: cred.euid,
            gid: cred.egid,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        let new_inode = Inode::new(
            InodeId {
                fs_id: sb.fs_id,
                ino,
            },
            FileType::Directory,
            DevId::new(0, 0),
            4096,
            sb.dev_id,
            meta,
            Arc::new(TmpfsInodeOps {
                data: Arc::new(Spinlock::new(TmpfsInodeData::Directory(BTreeMap::new()))),
            }),
            sb.self_weak.clone(),
        );

        entries.insert(name.to_string(), ino);
        dir.inc_nlink();
        dir.touch_mtime();
        dir.touch_ctime();
        drop(data);

        Ok(sb.insert_inode(new_inode))
    }

    fn unlink(&self, dir: &Inode, name: &str, child: &Inode) -> VfsResult<()> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }

        if child.kind() == FileType::Directory {
            return Err(VfsError::IsADirectory);
        }

        let mut data = self.data.lock();
        let entries = match &mut *data {
            TmpfsInodeData::Directory(entries) => entries,
            _ => return Err(VfsError::NotADirectory),
        };

        entries.remove(name).ok_or(VfsError::NotFound)?;
        child.dec_nlink();
        child.touch_ctime();
        dir.touch_mtime();
        dir.touch_ctime();

        Ok(())
    }

    fn rmdir(&self, dir: &Inode, name: &str, child: &Inode) -> VfsResult<()> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }

        if child.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }

        let child_ops = child
            .downcast_ops::<TmpfsInodeOps>()
            .ok_or(VfsError::InvalidArgument)?;
        let child_data = child_ops.data.lock();
        let child_entries = match &*child_data {
            TmpfsInodeData::Directory(entries) => entries,
            _ => return Err(VfsError::NotADirectory),
        };

        if !child_entries.is_empty() {
            return Err(VfsError::DirectoryNotEmpty);
        }

        let mut data = self.data.lock();
        let entries = match &mut *data {
            TmpfsInodeData::Directory(entries) => entries,
            _ => return Err(VfsError::NotADirectory),
        };

        entries.remove(name).ok_or(VfsError::NotFound)?;
        dir.dec_nlink();
        dir.touch_mtime();
        dir.touch_ctime();
        child.set_nlink(0);
        child.touch_ctime();
        drop(data);
        drop(child_data);

        Ok(())
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

        let sb = dir.superblock().ok_or(VfsError::InvalidArgument)?;
        let sb_ops = sb
            .ops
            .as_any()
            .downcast_ref::<TmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;

        let mut data = self.data.lock();
        let entries = match &mut *data {
            TmpfsInodeData::Directory(entries) => entries,
            _ => return Err(VfsError::NotADirectory),
        };

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }

        let ino = sb_ops.alloc_ino();
        let now = Timespec::now();
        let meta = InodeMeta {
            size: target.len() as u64,
            nlink: 1,
            mode: FileMode::new(0o777),
            uid: cred.euid,
            gid: cred.egid,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        let new_inode = Inode::new(
            InodeId {
                fs_id: sb.fs_id,
                ino,
            },
            FileType::Symlink,
            DevId::new(0, 0),
            4096,
            sb.dev_id,
            meta,
            Arc::new(TmpfsInodeOps {
                data: Arc::new(Spinlock::new(TmpfsInodeData::Symlink(target.to_string()))),
            }),
            sb.self_weak.clone(),
        );

        entries.insert(name.to_string(), ino);
        drop(data);

        Ok(sb.insert_inode(new_inode))
    }

    fn link(&self, dir: &Inode, target: &Inode, name: &str) -> VfsResult<()> {
        if dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }

        if target.kind() == FileType::Directory {
            return Err(VfsError::OperationNotPermitted);
        }

        let mut data = self.data.lock();
        let entries = match &mut *data {
            TmpfsInodeData::Directory(entries) => entries,
            _ => return Err(VfsError::NotADirectory),
        };

        if entries.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }

        entries.insert(name.to_string(), target.ino());
        target.inc_nlink();
        target.touch_ctime();

        Ok(())
    }

    fn readlink(&self, inode: &Inode) -> VfsResult<String> {
        if inode.kind() != FileType::Symlink {
            return Err(VfsError::InvalidArgument);
        }

        let data = self.data.lock();
        match &*data {
            TmpfsInodeData::Symlink(target) => Ok(target.clone()),
            _ => Err(VfsError::InvalidArgument),
        }
    }

    fn truncate(&self, inode: &Inode, new_size: u64) -> VfsResult<()> {
        if inode.kind() != FileType::Regular {
            return Err(VfsError::InvalidArgument);
        }

        let mut data = self.data.lock();
        let file_data = match &mut *data {
            TmpfsInodeData::File(data) => data,
            _ => return Err(VfsError::InvalidArgument),
        };

        let new_len = usize::try_from(new_size).map_err(|_| VfsError::FileTooLarge)?;
        file_data.resize(new_len, 0);
        inode.set_size(new_size);
        inode.touch_mtime();
        inode.touch_ctime();

        Ok(())
    }

    fn open(
        &self,
        inode: &Inode,
        _options: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let inode_arc = inode
            .superblock()
            .and_then(|sb| sb.find_inode(inode.ino()))
            .ok_or(VfsError::InvalidArgument)?;
        Ok(Box::new(TmpfsFileOps {
            inode: inode_arc,
            data: Arc::clone(
                &inode
                    .downcast_ops::<TmpfsInodeOps>()
                    .ok_or(VfsError::InvalidArgument)?
                    .data,
            ),
        }))
    }

    fn evict(&self, _inode: &Inode) {
        // tmpfs 数据随 InodeOps 一起释放
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── File 操作 ─────────────────────────────────────────────────────────────────

struct TmpfsFileOps {
    inode: Arc<Inode>,
    data: Arc<Spinlock<TmpfsInodeData>>,
}

impl FileOps for TmpfsFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let data = self.data.lock();
        let file_data = match &*data {
            TmpfsInodeData::File(data) => data,
            _ => return Err(VfsError::InvalidArgument),
        };

        let offset = usize::try_from(offset).map_err(|_| VfsError::FileTooLarge)?;
        let start = offset.min(file_data.len());
        let end = (start + buf.len()).min(file_data.len());
        let n = end - start;

        buf[..n].copy_from_slice(&file_data[start..end]);
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let mut data = self.data.lock();
        let file_data = match &mut *data {
            TmpfsInodeData::File(data) => data,
            _ => return Err(VfsError::InvalidArgument),
        };

        let start = if offset == u64::MAX {
            file_data.len()
        } else {
            usize::try_from(offset).map_err(|_| VfsError::FileTooLarge)?
        };
        let end = start.checked_add(buf.len()).ok_or(VfsError::FileTooLarge)?;

        if end > file_data.len() {
            file_data.resize(end, 0);
        }

        file_data[start..end].copy_from_slice(buf);
        let new_size = file_data.len() as u64;
        drop(data);
        self.inode.set_size(new_size);
        self.inode.touch_mtime();
        self.inode.touch_ctime();
        Ok(buf.len())
    }

    fn readdir(
        &self,
        pos: u64,
        sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        let data = self.data.lock();
        let entries = match &*data {
            TmpfsInodeData::Directory(entries) => entries,
            _ => return Err(VfsError::NotADirectory),
        };

        let mut current_pos = pos;
        let start_pos = usize::try_from(pos).map_err(|_| VfsError::InvalidArgument)?;
        for (name, ino) in entries.iter().skip(start_pos) {
            let entry = DirEntry {
                ino: *ino,
                name: SmallStr::from(name.as_str()),
                kind: FileType::Regular, // tmpfs 不在 DirEntry 中存储类型
            };

            if sink(entry).is_break() {
                return Ok(current_pos);
            }

            current_pos += 1;
        }

        Ok(current_pos)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, _events: PollEvents) -> PollEvents {
        PollEvents::POLLIN.with(PollEvents::POLLOUT)
    }

    fn release(&self) {
        // 无需特殊清理
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

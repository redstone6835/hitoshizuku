//! Tmpfs - 内存文件系统驱动。
//!
//! Tmpfs 是一个完全基于内存的文件系统，所有数据存储在 RAM 中，重启后丢失。
//! 常用于 `/tmp`、`/dev/shm` 等临时存储场景。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
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
                data: Spinlock::new(TmpfsInodeData::Directory(BTreeMap::new())),
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
    Special,
}

fn resize_file_data(file_data: &mut Vec<u8>, new_len: usize) -> VfsResult<()> {
    if new_len > file_data.len() {
        file_data
            .try_reserve_exact(new_len - file_data.len())
            .map_err(|_| VfsError::OutOfMemory)?;
    }
    file_data.resize(new_len, 0);
    Ok(())
}

fn tmpfs_blocks_for_len(len: u64) -> u64 {
    // stat.st_blocks 的单位固定为 512 字节，不等同于 tmpfs 的页大小。
    len.saturating_add(511) / 512
}

fn ensure_empty_tmpfs_dir(inode: &Inode) -> VfsResult<()> {
    if inode.kind() != FileType::Directory {
        return Err(VfsError::NotADirectory);
    }
    let ops = inode
        .downcast_ops::<TmpfsInodeOps>()
        .ok_or(VfsError::InvalidArgument)?;
    let data = ops.data.lock();
    let entries = match &*data {
        TmpfsInodeData::Directory(entries) => entries,
        _ => return Err(VfsError::NotADirectory),
    };
    if entries.is_empty() {
        Ok(())
    } else {
        Err(VfsError::DirectoryNotEmpty)
    }
}

fn validate_rename_replacement(old_inode: &Inode, replaced: &Inode) -> VfsResult<()> {
    match (old_inode.kind(), replaced.kind()) {
        (FileType::Directory, FileType::Directory) => ensure_empty_tmpfs_dir(replaced),
        (FileType::Directory, _) => Err(VfsError::NotADirectory),
        (_, FileType::Directory) => Err(VfsError::IsADirectory),
        _ => Ok(()),
    }
}

fn retire_replaced_entry(replaced: &Inode, parent: &Inode) {
    if replaced.kind() == FileType::Directory {
        replaced.set_nlink(0);
        parent.dec_nlink();
    } else {
        replaced.dec_nlink();
    }
    replaced.touch_ctime();
}

fn rename_entry(
    old_entries: &mut BTreeMap<String, u64>,
    new_entries: &mut BTreeMap<String, u64>,
    sb: &Superblock,
    old_name: &str,
    old_inode: &Inode,
    new_dir: &Inode,
    new_name: &str,
) -> VfsResult<bool> {
    let old_ino = *old_entries.get(old_name).ok_or(VfsError::NotFound)?;
    if old_ino != old_inode.ino() {
        return Err(VfsError::NotFound);
    }

    let replaced = if let Some(existing_ino) = new_entries.get(new_name).copied() {
        if existing_ino == old_ino {
            old_entries.remove(old_name);
            old_inode.dec_nlink();
            old_inode.touch_ctime();
            return Ok(false);
        }
        let inode = sb.find_inode(existing_ino).ok_or(VfsError::NotFound)?;
        validate_rename_replacement(old_inode, &inode)?;
        Some(inode)
    } else {
        None
    };

    old_entries.remove(old_name);
    new_entries.insert(new_name.to_string(), old_ino);

    if let Some(replaced) = replaced {
        retire_replaced_entry(&replaced, new_dir);
    }
    Ok(true)
}

struct TmpfsInodeOps {
    data: Spinlock<TmpfsInodeData>,
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
                data: Spinlock::new(TmpfsInodeData::File(Vec::new())),
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
                data: Spinlock::new(TmpfsInodeData::Directory(BTreeMap::new())),
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
        drop(child_data);

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
            blocks: tmpfs_blocks_for_len(target.len() as u64),
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
                data: Spinlock::new(TmpfsInodeData::Symlink(target.to_string())),
            }),
            sb.self_weak.clone(),
        );

        entries.insert(name.to_string(), ino);
        dir.touch_mtime();
        dir.touch_ctime();
        drop(data);

        Ok(sb.insert_inode(new_inode))
    }

    fn mknod(
        &self,
        dir: &Inode,
        name: &str,
        kind: FileType,
        mode: FileMode,
        dev: DevId,
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
            kind,
            dev,
            4096,
            sb.dev_id,
            meta,
            Arc::new(TmpfsInodeOps {
                data: Spinlock::new(TmpfsInodeData::Special),
            }),
            sb.self_weak.clone(),
        );

        entries.insert(name.to_string(), ino);
        dir.touch_mtime();
        dir.touch_ctime();
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
        dir.touch_mtime();
        dir.touch_ctime();

        Ok(())
    }

    fn rename(
        &self,
        dir: &Inode,
        old_name: &str,
        old_inode: &Inode,
        new_dir: &Inode,
        new_name: &str,
    ) -> VfsResult<()> {
        if dir.kind() != FileType::Directory || new_dir.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }

        let sb = dir.superblock().ok_or(VfsError::InvalidArgument)?;
        if new_dir.fs_id() != dir.fs_id() {
            return Err(VfsError::CrossDevice);
        }
        let new_ops = new_dir
            .downcast_ops::<TmpfsInodeOps>()
            .ok_or(VfsError::InvalidArgument)?;

        if dir.ino() == new_dir.ino() {
            let mut data = self.data.lock();
            let entries = match &mut *data {
                TmpfsInodeData::Directory(entries) => entries,
                _ => return Err(VfsError::NotADirectory),
            };
            let old_ino = *entries.get(old_name).ok_or(VfsError::NotFound)?;
            if old_ino != old_inode.ino() {
                return Err(VfsError::NotFound);
            }

            let replaced = if let Some(existing_ino) = entries.get(new_name).copied() {
                if existing_ino == old_ino {
                    entries.remove(old_name);
                    old_inode.dec_nlink();
                    old_inode.touch_ctime();
                    dir.touch_mtime();
                    dir.touch_ctime();
                    return Ok(());
                }
                let inode = sb.find_inode(existing_ino).ok_or(VfsError::NotFound)?;
                validate_rename_replacement(old_inode, &inode)?;
                Some(inode)
            } else {
                None
            };

            entries.remove(old_name);
            entries.insert(new_name.to_string(), old_ino);
            if let Some(replaced) = replaced {
                retire_replaced_entry(&replaced, dir);
            }
        } else if dir.ino() < new_dir.ino() {
            let mut old_data = self.data.lock();
            let mut new_data = new_ops.data.lock();
            let old_entries = match &mut *old_data {
                TmpfsInodeData::Directory(entries) => entries,
                _ => return Err(VfsError::NotADirectory),
            };
            let new_entries = match &mut *new_data {
                TmpfsInodeData::Directory(entries) => entries,
                _ => return Err(VfsError::NotADirectory),
            };
            let inserted = rename_entry(
                old_entries,
                new_entries,
                &sb,
                old_name,
                old_inode,
                new_dir,
                new_name,
            )?;
            if inserted && old_inode.kind() == FileType::Directory {
                dir.dec_nlink();
                new_dir.inc_nlink();
            }
        } else {
            let mut new_data = new_ops.data.lock();
            let mut old_data = self.data.lock();
            let old_entries = match &mut *old_data {
                TmpfsInodeData::Directory(entries) => entries,
                _ => return Err(VfsError::NotADirectory),
            };
            let new_entries = match &mut *new_data {
                TmpfsInodeData::Directory(entries) => entries,
                _ => return Err(VfsError::NotADirectory),
            };
            let inserted = rename_entry(
                old_entries,
                new_entries,
                &sb,
                old_name,
                old_inode,
                new_dir,
                new_name,
            )?;
            if inserted && old_inode.kind() == FileType::Directory {
                dir.dec_nlink();
                new_dir.inc_nlink();
            }
        }

        old_inode.touch_ctime();
        dir.touch_mtime();
        dir.touch_ctime();
        if dir.ino() != new_dir.ino() {
            new_dir.touch_mtime();
            new_dir.touch_ctime();
        }
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

    fn chmod(&self, inode: &Inode, mode: FileMode) -> VfsResult<()> {
        inode.set_mode(mode);
        Ok(())
    }

    fn chown(&self, inode: &Inode, uid: Option<Uid>, gid: Option<Gid>) -> VfsResult<()> {
        if uid.is_some() || gid.is_some() {
            inode.set_owner(uid, gid);
        }
        Ok(())
    }

    fn utimes(
        &self,
        inode: &Inode,
        atime: Option<Timespec>,
        mtime: Option<Timespec>,
    ) -> VfsResult<()> {
        inode.set_times(atime, mtime);
        Ok(())
    }

    fn truncate(&self, inode: &Inode, new_size: u64) -> VfsResult<()> {
        if inode.kind() != FileType::Regular {
            return Err(VfsError::InvalidArgument);
        }
        if new_size > usize::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        }

        let mut data = self.data.lock();
        let file_data = match &mut *data {
            TmpfsInodeData::File(data) => data,
            _ => return Err(VfsError::InvalidArgument),
        };

        resize_file_data(file_data, new_size as usize)?;
        inode.set_size_and_blocks(new_size, tmpfs_blocks_for_len(new_size));
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
        {
            let data = self.data.lock();
            if matches!(&*data, TmpfsInodeData::Special) {
                return Err(VfsError::NotSupported);
            }
        }
        let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
        Ok(Box::new(TmpfsFileOps {
            inode_ops: inode
                .downcast_ops::<TmpfsInodeOps>()
                .ok_or(VfsError::InvalidArgument)? as *const TmpfsInodeOps,
            sb: Arc::downgrade(&sb),
            ino: inode.ino(),
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
    inode_ops: *const TmpfsInodeOps,
    sb: Weak<Superblock>,
    ino: u64,
}

unsafe impl Send for TmpfsFileOps {}
unsafe impl Sync for TmpfsFileOps {}

impl TmpfsFileOps {
    fn inode(&self) -> Option<Arc<Inode>> {
        self.sb.upgrade().and_then(|sb| sb.find_inode(self.ino))
    }
}

impl FileOps for TmpfsFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let ops = unsafe { &*self.inode_ops };
        let data = ops.data.lock();
        let file_data = match &*data {
            TmpfsInodeData::File(data) => data,
            _ => return Err(VfsError::InvalidArgument),
        };

        if offset > usize::MAX as u64 {
            return Ok(0);
        }

        let start = (offset as usize).min(file_data.len());
        let end = start
            .checked_add(buf.len())
            .unwrap_or(usize::MAX)
            .min(file_data.len());
        let n = end - start;

        buf[..n].copy_from_slice(&file_data[start..end]);
        if n != 0
            && let Some(inode) = self.inode()
        {
            inode.touch_atime();
        }
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let ops = unsafe { &*self.inode_ops };
        let mut data = ops.data.lock();
        let file_data = match &mut *data {
            TmpfsInodeData::File(data) => data,
            _ => return Err(VfsError::InvalidArgument),
        };

        let start = if offset == u64::MAX {
            file_data.len()
        } else if offset > usize::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        } else {
            offset as usize
        };
        let end = start.checked_add(buf.len()).ok_or(VfsError::FileTooLarge)?;

        if end > file_data.len() {
            resize_file_data(file_data, end)?;
        }

        file_data[start..end].copy_from_slice(buf);
        if let Some(inode) = self.inode() {
            if inode.size() != file_data.len() as u64 {
                let size = file_data.len() as u64;
                inode.set_size_and_blocks(size, tmpfs_blocks_for_len(size));
            }
            if !buf.is_empty() {
                inode.touch_mtime();
                inode.touch_ctime();
            }
        }
        Ok(buf.len())
    }

    fn fallocate(&self, offset: u64, len: u64) -> VfsResult<()> {
        let end = offset.checked_add(len).ok_or(VfsError::FileTooLarge)?;
        if end > usize::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        }

        let ops = unsafe { &*self.inode_ops };
        let mut data = ops.data.lock();
        let file_data = match &mut *data {
            TmpfsInodeData::File(data) => data,
            _ => return Err(VfsError::InvalidArgument),
        };

        let end = end as usize;
        if end > file_data.len() {
            resize_file_data(file_data, end)?;
            if let Some(inode) = self.inode() {
                let size = end as u64;
                inode.set_size_and_blocks(size, tmpfs_blocks_for_len(size));
                inode.touch_mtime();
                inode.touch_ctime();
            }
        }
        Ok(())
    }

    fn readdir(
        &self,
        pos: u64,
        sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        let ops = unsafe { &*self.inode_ops };
        let data = ops.data.lock();
        let entries = match &*data {
            TmpfsInodeData::Directory(entries) => entries,
            _ => return Err(VfsError::NotADirectory),
        };
        let sb = self.sb.upgrade().ok_or(VfsError::InvalidArgument)?;

        let mut current_pos = pos;
        for (name, ino) in entries.iter().skip(pos as usize) {
            let kind = sb.find_inode(*ino).ok_or(VfsError::NotFound)?.kind();
            let entry = DirEntry {
                ino: *ino,
                name: SmallStr::from(name.as_str()),
                kind,
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

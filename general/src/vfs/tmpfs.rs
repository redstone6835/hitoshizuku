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

const TMPFS_VIRTUAL_BLOCKS: u64 = 256 * 1024;
const TMPFS_VIRTUAL_INODES: u64 = 1_000_000;

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

        // 根目录不会经过 create/mkdir 路径，必须在挂载完成后显式放入 inode 缓存，
        // 否则 open("/") 会因为找不到 ino=1 而返回 EINVAL。
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
        let used_inodes = self.total_inodes.load(Ordering::Relaxed);
        let total_inodes = TMPFS_VIRTUAL_INODES.max(used_inodes);
        let free_inodes = total_inodes.saturating_sub(used_inodes);
        Ok(FsStat {
            fs_type: 0x01021994,
            block_size: sb.block_size as u64,
            total_blocks: TMPFS_VIRTUAL_BLOCKS,
            free_blocks: TMPFS_VIRTUAL_BLOCKS,
            avail_blocks: TMPFS_VIRTUAL_BLOCKS,
            total_inodes,
            free_inodes,
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
    File(TmpfsFileData),
    Directory(BTreeMap<String, u64>),
    Symlink(String),
    Fifo(Arc<vfs::pipe::Pipe>),
    Special,
}

const TMPFS_PAGE_SIZE: usize = 4096;
const TMPFS_PAGE_SIZE_U64: u64 = TMPFS_PAGE_SIZE as u64;

struct TmpfsPage {
    index: u64,
    data: Vec<u8>,
}

struct TmpfsFileData {
    size: u64,
    pages: Vec<TmpfsPage>,
}

impl TmpfsFileData {
    const fn new() -> Self {
        Self {
            size: 0,
            pages: Vec::new(),
        }
    }

    fn blocks(&self) -> u64 {
        (self.pages.len() as u64 * TMPFS_PAGE_SIZE_U64).div_ceil(512)
    }

    fn truncate(&mut self, new_size: u64) {
        if new_size < self.size {
            let keep_pages = new_size.div_ceil(TMPFS_PAGE_SIZE_U64);
            self.pages.retain(|page| page.index < keep_pages);
            if new_size % TMPFS_PAGE_SIZE_U64 != 0 {
                let tail_index = new_size / TMPFS_PAGE_SIZE_U64;
                let tail_offset = (new_size % TMPFS_PAGE_SIZE_U64) as usize;
                if let Some(pos) = self.page_pos(tail_index).ok() {
                    self.pages[pos].data[tail_offset..].fill(0);
                }
            }
        }
        self.size = new_size;
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> usize {
        if offset >= self.size || buf.is_empty() {
            return 0;
        }
        let end = offset.saturating_add(buf.len() as u64).min(self.size);
        let n = (end - offset) as usize;
        let out = &mut buf[..n];
        out.fill(0);

        let first_page = offset / TMPFS_PAGE_SIZE_U64;
        let last_page = (end - 1) / TMPFS_PAGE_SIZE_U64;
        for page_index in first_page..=last_page {
            let Ok(pos) = self.page_pos(page_index) else {
                continue;
            };
            let page_start = page_index * TMPFS_PAGE_SIZE_U64;
            let copy_start = offset.max(page_start);
            let copy_end = end.min(page_start + TMPFS_PAGE_SIZE_U64);
            let src_start = (copy_start - page_start) as usize;
            let dst_start = (copy_start - offset) as usize;
            let len = (copy_end - copy_start) as usize;
            out[dst_start..dst_start + len]
                .copy_from_slice(&self.pages[pos].data[src_start..src_start + len]);
        }
        n
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(VfsError::FileTooLarge)?;
        if end > usize::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        }

        let mut written = 0usize;
        while written < buf.len() {
            let file_off = offset + written as u64;
            let page_index = file_off / TMPFS_PAGE_SIZE_U64;
            let page_offset = (file_off % TMPFS_PAGE_SIZE_U64) as usize;
            let chunk = (TMPFS_PAGE_SIZE - page_offset).min(buf.len() - written);
            let page = match self.get_or_create_page(page_index) {
                Ok(page) => page,
                Err(_) if written != 0 => {
                    self.size = self.size.max(offset + written as u64);
                    return Ok(written);
                }
                Err(err) => return Err(err),
            };
            page[page_offset..page_offset + chunk].copy_from_slice(&buf[written..written + chunk]);
            written += chunk;
        }

        self.size = self.size.max(end);
        Ok(written)
    }

    fn reserve(&mut self, offset: u64, len: u64) -> VfsResult<()> {
        let end = offset.checked_add(len).ok_or(VfsError::FileTooLarge)?;
        if end > usize::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        }
        self.size = self.size.max(end);
        Ok(())
    }

    fn page_pos(&self, index: u64) -> Result<usize, usize> {
        self.pages.binary_search_by_key(&index, |page| page.index)
    }

    fn get_or_create_page(&mut self, index: u64) -> VfsResult<&mut [u8]> {
        match self.page_pos(index) {
            Ok(pos) => Ok(self.pages[pos].data.as_mut_slice()),
            Err(pos) => {
                self.pages
                    .try_reserve(1)
                    .map_err(|_| VfsError::OutOfMemory)?;
                let mut data = Vec::new();
                data.try_reserve_exact(TMPFS_PAGE_SIZE)
                    .map_err(|_| VfsError::OutOfMemory)?;
                data.resize(TMPFS_PAGE_SIZE, 0);
                self.pages.insert(pos, TmpfsPage { index, data });
                Ok(self.pages[pos].data.as_mut_slice())
            }
        }
    }
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
            uid: cred.fsuid,
            gid: cred.fsgid,
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
                data: Spinlock::new(TmpfsInodeData::File(TmpfsFileData::new())),
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
            uid: cred.fsuid,
            gid: cred.fsgid,
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
            uid: cred.fsuid,
            gid: cred.fsgid,
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
            uid: cred.fsuid,
            gid: cred.fsgid,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        let inode_data = match kind {
            FileType::Fifo => TmpfsInodeData::Fifo(vfs::pipe::new_fifo()),
            _ => TmpfsInodeData::Special,
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
                data: Spinlock::new(inode_data),
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

        file_data.truncate(new_size);
        inode.set_size_and_blocks(new_size, file_data.blocks());
        inode.touch_mtime();
        inode.touch_ctime();

        Ok(())
    }

    fn open(
        &self,
        inode: &Inode,
        options: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        {
            let data = self.data.lock();
            match &*data {
                TmpfsInodeData::Special => return Err(VfsError::NotSupported),
                TmpfsInodeData::Fifo(pipe) => {
                    return vfs::pipe::open_fifo(Arc::clone(pipe), options);
                }
                _ => {}
            }
        }
        let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
        let opened_inode = sb
            .find_inode(inode.ino())
            .ok_or(VfsError::InvalidArgument)?;
        Ok(Box::new(TmpfsFileOps {
            inode_ops: inode
                .downcast_ops::<TmpfsInodeOps>()
                .ok_or(VfsError::InvalidArgument)? as *const TmpfsInodeOps,
            inode: opened_inode,
            sb,
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
    inode: Arc<Inode>,
    sb: Arc<Superblock>,
}

unsafe impl Send for TmpfsFileOps {}
unsafe impl Sync for TmpfsFileOps {}

impl FileOps for TmpfsFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let ops = unsafe { &*self.inode_ops };
        let data = ops.data.lock();
        let file_data = match &*data {
            TmpfsInodeData::File(data) => data,
            _ => return Err(VfsError::InvalidArgument),
        };

        let n = file_data.read_at(buf, offset);
        if n != 0 {
            self.inode.touch_atime();
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
            file_data.size
        } else if offset > usize::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        } else {
            offset
        };

        let n = file_data.write_at(buf, start)?;
        self.inode
            .set_size_and_blocks(file_data.size, file_data.blocks());
        if n != 0 {
            self.inode.touch_mtime();
            self.inode.touch_ctime();
        }
        Ok(n)
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

        if end > file_data.size {
            file_data.reserve(offset, len)?;
            let size = file_data.size;
            self.inode.set_size_and_blocks(size, file_data.blocks());
            self.inode.touch_mtime();
            self.inode.touch_ctime();
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
        let mut current_pos = pos;
        for (name, ino) in entries.iter().skip(pos as usize) {
            let kind = self.sb.find_inode(*ino).ok_or(VfsError::NotFound)?.kind();
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

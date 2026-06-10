//! VFS `InodeOps` 实现:目录 inode 和普通文件 inode。
//!
//! 目录 inode 持有 [`DirBacking`],普通文件 inode 持有 first_cluster + size 与
//! 在父目录里的 SFN 槽位置(以便修改 size 时回写父目录条目)。
//!
//! 所有写操作在 `state.is_read_only()` 时拒绝并返回 `ReadOnlyFilesystem`。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use core::any::Any;
use core::sync::atomic::{AtomicU32, Ordering};

use vfs::cred::{Credentials, Gid, Uid};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{FileOps, OpenOptions};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::stat::{DevId, FileMode, FileType, Timespec};
use vfs::sync::Spinlock;

use crate::bpb::FatKind;
use crate::dir::{self, ATTR_ARCHIVE, ATTR_DIRECTORY, DirBacking, DirEntryView};
use crate::file::{DirFileOps, RegFileOps};
use crate::state::FsState;
use crate::sync_layer::backend_to_vfs;

/// 目录 inode 在父目录里的"位置"信息(允许后续修改其大小/簇等)。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParentSlot {
    pub backing: DirBacking,
    pub sfn_slot: u32,
}

/// 目录的 InodeOps。
pub struct DirInodeOps {
    pub(crate) state: Arc<FsState>,
    pub(crate) backing: DirBacking,
    /// 在父目录里的位置(根目录为 None)。将来 rename 跨目录时会用到。
    #[allow(dead_code)]
    pub(crate) parent_slot: Option<ParentSlot>,
}

impl DirInodeOps {
    pub(crate) fn new_root(state: Arc<FsState>) -> Self {
        let backing = match state.kind {
            FatKind::Fat32 => DirBacking::ChainFromCluster(state.root_cluster),
            FatKind::Fat12 | FatKind::Fat16 => DirBacking::FixedRange {
                start_lba: state.reserved_sectors as u64
                    + state.num_fats as u64 * state.fat_size_sectors as u64,
                sector_count: state.root_dir_sectors,
            },
        };
        Self {
            state,
            backing,
            parent_slot: None,
        }
    }

    pub(crate) fn new_sub(state: Arc<FsState>, first_cluster: u32, parent: ParentSlot) -> Self {
        Self {
            state,
            backing: DirBacking::ChainFromCluster(first_cluster),
            parent_slot: Some(parent),
        }
    }

    /// 在 inode 指向的目录里查找名称(忽略大小写,先尝试用户原始名)。
    fn find_entry(&self, name: &str) -> VfsResult<Option<DirEntryView>> {
        dir::find_entry(&self.state, self.backing, name).map_err(backend_to_vfs)
    }
}

fn build_inode_for_entry(
    state: &Arc<FsState>,
    parent_backing: DirBacking,
    sb: &Arc<vfs::superblock::Superblock>,
    entry: &DirEntryView,
) -> Arc<Inode> {
    let now = Timespec::ZERO;
    let parent = ParentSlot {
        backing: parent_backing,
        sfn_slot: entry.slot_sfn,
    };
    if entry.is_dir() {
        let synth = state.next_synth_ino();
        let meta = InodeMeta {
            size: 0,
            nlink: 2,
            mode: if state.is_read_only() {
                FileMode::new(0o555)
            } else {
                FileMode::new(0o755)
            },
            uid: Uid::ROOT,
            gid: Gid::ROOT,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };
        let ops = DirInodeOps::new_sub(Arc::clone(state), entry.first_cluster, parent);
        let inode = Inode::new(
            InodeId {
                fs_id: sb.fs_id,
                ino: synth,
            },
            FileType::Directory,
            DevId::new(0, 0),
            state.cluster_size,
            None,
            meta,
            Arc::new(ops) as Arc<dyn InodeOps + Send + Sync>,
            sb.self_weak.clone(),
        );
        sb.insert_inode(inode)
    } else {
        let meta = InodeMeta {
            size: entry.size as u64,
            nlink: 1,
            mode: if state.is_read_only() {
                FileMode::new(0o444)
            } else {
                FileMode::new(0o644)
            },
            uid: Uid::ROOT,
            gid: Gid::ROOT,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: ((entry.size as u64 + 511) / 512),
        };
        let ino = if entry.first_cluster >= 2 {
            entry.first_cluster as u64
        } else {
            state.next_synth_ino()
        };
        let ops = FileInodeOps {
            state: Arc::clone(state),
            first_cluster: AtomicU32::new(entry.first_cluster),
            size: AtomicU32::new(entry.size),
            tail_cache: Spinlock::new(None),
            parent: Spinlock::new(parent),
        };
        let inode = Inode::new(
            InodeId {
                fs_id: sb.fs_id,
                ino,
            },
            FileType::Regular,
            DevId::new(0, 0),
            state.cluster_size,
            None,
            meta,
            Arc::new(ops) as Arc<dyn InodeOps + Send + Sync>,
            sb.self_weak.clone(),
        );
        sb.insert_inode(inode)
    }
}

impl InodeOps for DirInodeOps {
    fn lookup(&self, inode: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
        let Some(entry) = self.find_entry(name)? else {
            return Err(VfsError::NotFound);
        };
        Ok(build_inode_for_entry(
            &self.state,
            self.backing,
            &sb,
            &entry,
        ))
    }

    fn create(
        &self,
        inode: &Inode,
        name: &str,
        _mode: FileMode,
        _cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        if self.state.is_read_only() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        if name.is_empty() || name == "." || name == ".." {
            return Err(VfsError::InvalidArgument);
        }
        let mut dir_scratch = Vec::new();
        let (existing, used) = dir::find_entry_and_sfns_with_scratch(
            &self.state,
            self.backing,
            name,
            &mut dir_scratch,
        )
        .map_err(backend_to_vfs)?;
        if existing.is_some() {
            return Err(VfsError::AlreadyExists);
        }
        let (sfn, lfn_entries) = dir::build_entries_for_name(name, &used);
        let entry = dir::build_sfn_entry(sfn, ATTR_ARCHIVE, 0, 0);
        let sfn_slot = dir::insert_new_entry_with_scratch(
            &self.state,
            self.backing,
            &lfn_entries,
            &entry,
            &mut dir_scratch,
        )
        .map_err(backend_to_vfs)?;

        let view = DirEntryView {
            name: name.into(),
            short_name: sfn,
            attr: ATTR_ARCHIVE,
            first_cluster: 0,
            size: 0,
            slot_start: sfn_slot - lfn_entries.len() as u32,
            slot_sfn: sfn_slot,
        };
        let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
        Ok(build_inode_for_entry(&self.state, self.backing, &sb, &view))
    }

    fn mkdir(
        &self,
        inode: &Inode,
        name: &str,
        _mode: FileMode,
        _cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        if self.state.is_read_only() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        if name.is_empty() || name == "." || name == ".." {
            return Err(VfsError::InvalidArgument);
        }
        let mut dir_scratch = Vec::new();
        let (existing, used) = dir::find_entry_and_sfns_with_scratch(
            &self.state,
            self.backing,
            name,
            &mut dir_scratch,
        )
        .map_err(backend_to_vfs)?;
        if existing.is_some() {
            return Err(VfsError::AlreadyExists);
        }
        let new_c = self.state.alloc_cluster(None).map_err(backend_to_vfs)?;

        let res = (|| -> VfsResult<_> {
            let dot_sfn = {
                let mut s = [b' '; 11];
                s[0] = b'.';
                s
            };
            let dotdot_sfn = {
                let mut s = [b' '; 11];
                s[0] = b'.';
                s[1] = b'.';
                s
            };
            let parent_first = match self.backing {
                DirBacking::ChainFromCluster(c) => c,
                DirBacking::FixedRange { .. } => 0,
            };
            let dot_entry = dir::build_sfn_entry(dot_sfn, ATTR_DIRECTORY, new_c, 0);
            let dotdot_entry = dir::build_sfn_entry(dotdot_sfn, ATTR_DIRECTORY, parent_first, 0);
            // 新分配目录簇本应全零；直接在零缓冲中放入 `.`/`..`,避免两次读改写。
            dir_scratch.resize(self.state.cluster_size as usize, 0);
            dir_scratch.fill(0);
            dir_scratch[..dir::DIR_ENTRY_SIZE].copy_from_slice(&dot_entry);
            dir_scratch[dir::DIR_ENTRY_SIZE..dir::DIR_ENTRY_SIZE * 2]
                .copy_from_slice(&dotdot_entry);
            self.state
                .backend
                .write_sectors(
                    self.state.cluster_to_lba(new_c).map_err(backend_to_vfs)?,
                    self.state.sectors_per_cluster,
                    &dir_scratch,
                )
                .map_err(backend_to_vfs)?;

            let (sfn, lfn_entries) = dir::build_entries_for_name(name, &used);
            let entry = dir::build_sfn_entry(sfn, ATTR_DIRECTORY, new_c, 0);
            let sfn_slot = dir::insert_new_entry_with_scratch(
                &self.state,
                self.backing,
                &lfn_entries,
                &entry,
                &mut dir_scratch,
            )
            .map_err(backend_to_vfs)?;

            let view = DirEntryView {
                name: name.into(),
                short_name: sfn,
                attr: ATTR_DIRECTORY,
                first_cluster: new_c,
                size: 0,
                slot_start: sfn_slot - lfn_entries.len() as u32,
                slot_sfn: sfn_slot,
            };
            let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
            Ok(build_inode_for_entry(&self.state, self.backing, &sb, &view))
        })();

        match res {
            Ok(v) => Ok(v),
            Err(e) => {
                let _ = self.state.free_chain(new_c);
                Err(e)
            }
        }
    }

    fn unlink(&self, _inode: &Inode, name: &str, child: &Inode) -> VfsResult<()> {
        if self.state.is_read_only() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        let mut dir_scratch = Vec::new();
        let Some(entry) =
            dir::find_entry_with_scratch(&self.state, self.backing, name, &mut dir_scratch)
                .map_err(backend_to_vfs)?
        else {
            return Err(VfsError::NotFound);
        };
        if entry.is_dir() {
            return Err(VfsError::IsADirectory);
        }
        // 释放数据簇
        if entry.first_cluster >= 2 {
            self.state
                .free_chain(entry.first_cluster)
                .map_err(backend_to_vfs)?;
        }
        dir::remove_entry_slots_with_scratch(
            &self.state,
            self.backing,
            entry.slot_start,
            entry.slot_sfn,
            &mut dir_scratch,
        )
        .map_err(backend_to_vfs)?;
        let _ = child;
        Ok(())
    }

    fn rmdir(&self, _inode: &Inode, name: &str, child: &Inode) -> VfsResult<()> {
        if self.state.is_read_only() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        let mut dir_scratch = Vec::new();
        let Some(entry) =
            dir::find_entry_with_scratch(&self.state, self.backing, name, &mut dir_scratch)
                .map_err(backend_to_vfs)?
        else {
            return Err(VfsError::NotFound);
        };
        if !entry.is_dir() {
            return Err(VfsError::NotADirectory);
        }
        // 子目录是否非空(忽略 "." 和 ".." 与已删条目)
        let sub_backing = DirBacking::ChainFromCluster(entry.first_cluster);
        if !dir::is_dir_empty_with_scratch(&self.state, sub_backing, &mut dir_scratch)
            .map_err(backend_to_vfs)?
        {
            return Err(VfsError::DirectoryNotEmpty);
        }
        if entry.first_cluster >= 2 {
            self.state
                .free_chain(entry.first_cluster)
                .map_err(backend_to_vfs)?;
        }
        dir::remove_entry_slots_with_scratch(
            &self.state,
            self.backing,
            entry.slot_start,
            entry.slot_sfn,
            &mut dir_scratch,
        )
        .map_err(backend_to_vfs)?;
        let _ = child;
        Ok(())
    }

    fn rename(
        &self,
        _inode: &Inode,
        old_name: &str,
        _old_inode: &Inode,
        new_dir: &Inode,
        new_name: &str,
    ) -> VfsResult<()> {
        if self.state.is_read_only() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        if new_name.is_empty() || new_name == "." || new_name == ".." {
            return Err(VfsError::InvalidArgument);
        }
        let mut dir_scratch = Vec::new();
        let Some(entry) =
            dir::find_entry_with_scratch(&self.state, self.backing, old_name, &mut dir_scratch)
                .map_err(backend_to_vfs)?
        else {
            return Err(VfsError::NotFound);
        };
        // 目标目录的 InodeOps 必须也是同一个 FsState 上的 DirInodeOps
        let new_dir_ops = new_dir
            .downcast_ops::<DirInodeOps>()
            .ok_or(VfsError::CrossDevice)?;
        if !Arc::ptr_eq(&new_dir_ops.state, &self.state) {
            return Err(VfsError::CrossDevice);
        }
        // 如果新名字已存在则覆盖(unlink 之);本实现拒绝覆盖目录。
        // 单次扫描同时检测冲突 + 收集 SFN
        let (existing, mut used) = dir::find_entry_and_sfns_with_scratch(
            &self.state,
            new_dir_ops.backing,
            new_name,
            &mut dir_scratch,
        )
        .map_err(backend_to_vfs)?;
        if let Some(existing) = existing {
            if existing.is_dir() {
                return Err(VfsError::IsADirectory);
            }
            if existing.first_cluster >= 2 {
                self.state
                    .free_chain(existing.first_cluster)
                    .map_err(backend_to_vfs)?;
            }
            dir::remove_entry_slots_with_scratch(
                &self.state,
                new_dir_ops.backing,
                existing.slot_start,
                existing.slot_sfn,
                &mut dir_scratch,
            )
            .map_err(backend_to_vfs)?;
            used.retain(|s| *s != existing.short_name);
        }
        // 在目标目录写新条目
        let (sfn, lfn_entries) = dir::build_entries_for_name(new_name, &used);
        let attr = entry.attr;
        let new_entry = dir::build_sfn_entry(sfn, attr, entry.first_cluster, entry.size);
        dir::insert_new_entry_with_scratch(
            &self.state,
            new_dir_ops.backing,
            &lfn_entries,
            &new_entry,
            &mut dir_scratch,
        )
        .map_err(backend_to_vfs)?;
        // 删旧条目
        dir::remove_entry_slots_with_scratch(
            &self.state,
            self.backing,
            entry.slot_start,
            entry.slot_sfn,
            &mut dir_scratch,
        )
        .map_err(backend_to_vfs)?;
        Ok(())
    }

    fn open(
        &self,
        inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let _ = inode;
        Ok(Box::new(DirFileOps::new(&self.state, self.backing)?))
    }

    fn truncate(&self, _inode: &Inode, _new_size: u64) -> VfsResult<()> {
        Err(VfsError::IsADirectory)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 普通文件 inode。
pub struct FileInodeOps {
    pub(crate) state: Arc<FsState>,
    pub(crate) first_cluster: AtomicU32,
    pub(crate) size: AtomicU32,
    tail_cache: Spinlock<Option<TailCache>>,
    pub(crate) parent: Spinlock<ParentSlot>,
}

#[derive(Clone, Copy)]
struct TailCache {
    first_cluster: u32,
    clusters: u32,
    tail_cluster: u32,
}

impl FileInodeOps {
    fn writeback_meta(&self, new_size: u32, new_first: u32) -> VfsResult<()> {
        let parent = *self.parent.lock();
        let mut dir_scratch = Vec::new();
        dir::update_sfn_metadata_with_scratch(
            &self.state,
            parent.backing,
            parent.sfn_slot,
            new_first,
            new_size,
            &mut dir_scratch,
        )
        .map_err(backend_to_vfs)
    }

    pub(crate) fn grow_to(&self, new_size: u64) -> VfsResult<()> {
        let cur_size = self.size.load(Ordering::Acquire) as u64;
        if new_size <= cur_size {
            return Ok(());
        }
        let cluster_size = self.state.cluster_size as u64;
        let cur_clusters = (cur_size + cluster_size - 1) / cluster_size;
        let need_clusters = (new_size + cluster_size - 1) / cluster_size;
        let mut first = self.first_cluster.load(Ordering::Acquire);

        if cur_clusters == 0 && need_clusters > 0 {
            let (head, tail) = self
                .state
                .alloc_cluster_run(None, need_clusters as u32)
                .map_err(backend_to_vfs)?;
            first = head;
            self.first_cluster.store(head, Ordering::Release);
            self.update_tail_cache(head, need_clusters as u32, tail);
        } else if cur_clusters > 0 {
            let add = (need_clusters - cur_clusters) as u32;
            if add > 0 {
                if first < 2 {
                    return Err(VfsError::Io);
                }
                let tail = match self.cached_tail(first, cur_clusters as u32) {
                    Some(tail) => tail,
                    None => match self
                        .state
                        .fat
                        .walk_chain(self.state.backend.as_ref(), first, cur_clusters as u32 - 1)
                        .map_err(backend_to_vfs)?
                    {
                        Some(c) => c,
                        None => return Err(VfsError::Io),
                    },
                };
                let (_, new_tail) = self
                    .state
                    .alloc_cluster_run(Some(tail), add)
                    .map_err(backend_to_vfs)?;
                self.update_tail_cache(first, need_clusters as u32, new_tail);
            }
        }
        self.size.store(new_size as u32, Ordering::Release);
        self.writeback_meta(new_size as u32, first)?;
        Ok(())
    }

    pub(crate) fn shrink_to(&self, new_size: u64) -> VfsResult<()> {
        let cur_size = self.size.load(Ordering::Acquire) as u64;
        if new_size >= cur_size {
            return Ok(());
        }
        let cluster_size = self.state.cluster_size as u64;
        let need_clusters = (new_size + cluster_size - 1) / cluster_size;
        let first = self.first_cluster.load(Ordering::Acquire);
        if need_clusters == 0 {
            if first >= 2 {
                self.state
                    .fat
                    .free_chain(self.state.backend.as_ref(), first)
                    .map_err(backend_to_vfs)?;
            }
            self.first_cluster.store(0, Ordering::Release);
            self.size.store(0, Ordering::Release);
            self.clear_tail_cache();
            self.writeback_meta(0, 0)?;
            return Ok(());
        }
        let mut cur = first;
        for _ in 0..(need_clusters as u32 - 1).min(self.state.total_clusters) {
            cur = self
                .state
                .fat
                .next_cluster(self.state.backend.as_ref(), cur)
                .map_err(backend_to_vfs)?
                .ok_or(VfsError::Io)?;
        }
        if let Some(next) = self
            .state
            .fat
            .next_cluster(self.state.backend.as_ref(), cur)
            .map_err(backend_to_vfs)?
        {
            self.state
                .fat
                .free_chain(self.state.backend.as_ref(), next)
                .map_err(backend_to_vfs)?;
            self.state
                .fat
                .set(
                    self.state.backend.as_ref(),
                    cur,
                    self.state.fat.eoc_marker(),
                )
                .map_err(backend_to_vfs)?;
        }
        self.size.store(new_size as u32, Ordering::Release);
        self.update_tail_cache(first, need_clusters as u32, cur);
        self.writeback_meta(new_size as u32, first)?;
        Ok(())
    }

    fn cached_tail(&self, first_cluster: u32, clusters: u32) -> Option<u32> {
        let cache = self.tail_cache.lock();
        let cache = cache.as_ref()?;
        (cache.first_cluster == first_cluster
            && cache.clusters == clusters
            && cache.tail_cluster >= 2)
            .then_some(cache.tail_cluster)
    }

    fn update_tail_cache(&self, first_cluster: u32, clusters: u32, tail_cluster: u32) {
        // tail cache 只作为 FAT 顺序扩容的加速提示;不匹配时会回退到 walk_chain。
        *self.tail_cache.lock() = Some(TailCache {
            first_cluster,
            clusters,
            tail_cluster,
        });
    }

    fn clear_tail_cache(&self) {
        *self.tail_cache.lock() = None;
    }

    /// 当前文件大小。
    #[inline]
    pub(crate) fn current_size(&self) -> u32 {
        self.size.load(Ordering::Acquire)
    }

    /// 当前 first_cluster。
    #[inline]
    pub(crate) fn current_first(&self) -> u32 {
        self.first_cluster.load(Ordering::Acquire)
    }
}

impl InodeOps for FileInodeOps {
    fn lookup(&self, _i: &Inode, _n: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        inode: &Inode,
        opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        if opts.truncate {
            if self.state.is_read_only() {
                return Err(VfsError::ReadOnlyFilesystem);
            }
            self.shrink_to(0)?;
            inode.set_size(0);
        }
        Ok(Box::new(RegFileOps::new(
            Arc::clone(&self.state),
            inode.superblock().ok_or(VfsError::InvalidArgument)?,
            inode.ino(),
        )))
    }

    fn truncate(&self, inode: &Inode, new_size: u64) -> VfsResult<()> {
        if self.state.is_read_only() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        let cur = self.size.load(Ordering::Acquire) as u64;
        if new_size > cur {
            self.grow_to(new_size)?;
        } else if new_size < cur {
            self.shrink_to(new_size)?;
        }
        inode.set_size(new_size);
        Ok(())
    }

    fn chmod(&self, _i: &Inode, _m: FileMode) -> VfsResult<()> {
        Ok(())
    }

    fn chown(&self, _i: &Inode, _u: Option<Uid>, _g: Option<Gid>) -> VfsResult<()> {
        Ok(())
    }

    fn evict(&self, _i: &Inode) {}

    fn sync_metadata(&self, _i: &Inode) -> VfsResult<()> {
        let _ = self.state.sync_all();
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

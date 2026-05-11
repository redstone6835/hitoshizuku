//! VFS `FileOps` 实现:目录读取 + 普通文件读写。
//!
//! - [`DirFileOps`] 在 open() 时把目录条目快照进 [`Vec`],之后 readdir()
//!   按游标线性返回。
//! - [`RegFileOps`] 持有 `Arc<Inode>` 反向引用以读取 first_cluster/size,
//!   再借 [`FatTable`](crate::fat::FatTable) 沿簇链定位 LBA。
//!
//! 写路径在 `state.is_read_only()` 时返回 EROFS。O_APPEND 通过传入
//! `offset == u64::MAX` 区分;调用方持锁,与读路径不混叠。

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;

use vfs::dentry::SmallStr;
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, PollEvents};
use vfs::inode::Inode;
use vfs::stat::FileType;
use vfs::superblock::Superblock;
use vfs::sync::Spinlock;

use crate::dir::{ATTR_DIRECTORY, DirEntryView};
use crate::inode::FileInodeOps;
use crate::state::FsState;
use crate::sync_layer::backend_to_vfs;

// ── 目录 FileOps ─────────────────────────────────────────────────────────

pub struct DirFileOps {
    snapshot: Spinlock<Vec<DirEntry>>,
}

impl DirFileOps {
    pub(crate) fn new(entries: Vec<DirEntryView>) -> Self {
        let snapshot = entries
            .into_iter()
            .filter(|e| !e.is_volume())
            .map(|e| DirEntry {
                ino: if e.first_cluster >= 2 {
                    e.first_cluster as u64
                } else {
                    0
                },
                name: SmallStr::new(&e.name),
                kind: if e.attr & ATTR_DIRECTORY != 0 {
                    FileType::Directory
                } else {
                    FileType::Regular
                },
            })
            .collect();
        Self {
            snapshot: Spinlock::new(snapshot),
        }
    }
}

impl FileOps for DirFileOps {
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
        let snap = self.snapshot.lock();
        let mut idx = pos as usize;
        while idx < snap.len() {
            if sink(snap[idx].clone()).is_break() {
                return Ok(idx as u64);
            }
            idx += 1;
        }
        Ok(snap.len() as u64)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, _interest: PollEvents) -> PollEvents {
        PollEvents(0)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ── 普通文件 FileOps ─────────────────────────────────────────────────────

/// 每打开一个文件就造一个 RegFileOps;它持 `Arc<Superblock>` 以便按 ino
/// 反查到 Inode 与其 [`FileInodeOps`]。
pub struct RegFileOps {
    state: Arc<FsState>,
    sb: Arc<Superblock>,
    ino: u64,
    chain_cache: Spinlock<ChainCache>,
    /// 串行化 append 与扩容(避免两条 O_APPEND 同时读到旧 EOF 再写同位置)。
    io_mu: Spinlock<()>,
}

#[derive(Default)]
struct ChainCache {
    first_cluster: u32,
    clusters: Vec<u32>,
}

impl ChainCache {
    fn reset(&mut self, first_cluster: u32) {
        self.first_cluster = first_cluster;
        self.clusters.clear();
        if first_cluster >= 2 {
            self.clusters.push(first_cluster);
        }
    }
}

impl RegFileOps {
    pub(crate) fn new(state: Arc<FsState>, sb: Arc<Superblock>, ino: u64) -> Self {
        Self {
            state,
            sb,
            ino,
            chain_cache: Spinlock::new(ChainCache::default()),
            io_mu: Spinlock::new(()),
        }
    }

    /// 通过 sb + ino 找回 FileInodeOps。失败返回 NotFound。
    fn with_inode_ops<R>(
        &self,
        f: impl FnOnce(&Arc<Inode>, &FileInodeOps) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let inode = self.sb.find_inode(self.ino).ok_or(VfsError::NotFound)?;
        let ops = inode
            .downcast_ops::<FileInodeOps>()
            .ok_or(VfsError::InvalidArgument)?;
        f(&inode, ops)
    }

    fn contiguous_run(
        &self,
        first_cluster: u32,
        start_idx: u32,
        max_clusters: u32,
    ) -> VfsResult<Option<(u32, u32)>> {
        if first_cluster < 2 {
            return Ok(None);
        }
        if max_clusters == 0 {
            return Ok(None);
        }

        if !self.ensure_cached_cluster(first_cluster, start_idx)? {
            return Ok(None);
        }
        let start_cluster = {
            let cache = self.chain_cache.lock();
            cache.clusters[start_idx as usize]
        };
        let run = self
            .state
            .fat
            .contiguous_run(self.state.backend.as_ref(), start_cluster, max_clusters)
            .map_err(backend_to_vfs)?;
        self.extend_contiguous_cache(first_cluster, start_idx, start_cluster, run);
        Ok(Some((start_cluster, run)))
    }

    fn extend_contiguous_cache(
        &self,
        first_cluster: u32,
        start_idx: u32,
        start_cluster: u32,
        run: u32,
    ) {
        if run <= 1 {
            return;
        }
        let mut cache = self.chain_cache.lock();
        if cache.first_cluster != first_cluster {
            return;
        }
        let Some(&cached_start) = cache.clusters.get(start_idx as usize) else {
            return;
        };
        if cached_start != start_cluster {
            return;
        }
        let target_len = start_idx.saturating_add(run) as usize;
        while cache.clusters.len() < target_len {
            let off = cache.clusters.len() as u32 - start_idx;
            cache.clusters.push(start_cluster.saturating_add(off));
        }
    }

    fn ensure_cached_cluster(&self, first_cluster: u32, target_idx: u32) -> VfsResult<bool> {
        loop {
            let last = {
                let mut cache = self.chain_cache.lock();
                if cache.first_cluster != first_cluster || cache.clusters.is_empty() {
                    cache.reset(first_cluster);
                }
                if cache.clusters.len() > target_idx as usize {
                    return Ok(true);
                }
                *cache.clusters.last().ok_or(VfsError::Io)?
            };

            let Some(next) = self
                .state
                .fat
                .next_cluster(self.state.backend.as_ref(), last)
                .map_err(backend_to_vfs)?
            else {
                return Ok(false);
            };

            let mut cache = self.chain_cache.lock();
            if cache.first_cluster == first_cluster && cache.clusters.last().copied() == Some(last)
            {
                cache.clusters.push(next);
            }
        }
    }

    fn read_run(&self, cluster: u32, in_cluster: u64, out: &mut [u8]) -> VfsResult<()> {
        let bps = self.state.bytes_per_sector as u64;
        let start_lba = self.state.cluster_to_lba(cluster).map_err(backend_to_vfs)?;
        let len = out.len() as u64;
        let aligned = in_cluster.is_multiple_of(bps) && len.is_multiple_of(bps);
        let start_sector = in_cluster / bps;
        if aligned {
            self.state
                .backend
                .read_sectors(start_lba + start_sector, (len / bps) as u32, out)
                .map_err(backend_to_vfs)?;
            return Ok(());
        }

        let end_sector = (in_cluster + len).div_ceil(bps);
        let sector_count = (end_sector - start_sector) as u32;
        let mut chunk = vec![0u8; (sector_count as u64 * bps) as usize];
        self.state
            .backend
            .read_sectors(start_lba + start_sector, sector_count, &mut chunk)
            .map_err(backend_to_vfs)?;
        let chunk_off = (in_cluster - start_sector * bps) as usize;
        out.copy_from_slice(&chunk[chunk_off..chunk_off + out.len()]);
        Ok(())
    }

    fn write_run(&self, cluster: u32, in_cluster: u64, data: &[u8]) -> VfsResult<()> {
        let bps = self.state.bytes_per_sector as u64;
        let start_lba = self.state.cluster_to_lba(cluster).map_err(backend_to_vfs)?;
        let len = data.len() as u64;
        let aligned = in_cluster.is_multiple_of(bps) && len.is_multiple_of(bps);
        let start_sector = in_cluster / bps;
        if aligned {
            self.state
                .backend
                .write_sectors(start_lba + start_sector, (len / bps) as u32, data)
                .map_err(backend_to_vfs)?;
            return Ok(());
        }

        let end_sector = (in_cluster + len).div_ceil(bps);
        let sector_count = (end_sector - start_sector) as u32;
        let mut chunk = vec![0u8; (sector_count as u64 * bps) as usize];
        self.state
            .backend
            .read_sectors(start_lba + start_sector, sector_count, &mut chunk)
            .map_err(backend_to_vfs)?;
        let chunk_off = (in_cluster - start_sector * bps) as usize;
        chunk[chunk_off..chunk_off + data.len()].copy_from_slice(data);
        self.state
            .backend
            .write_sectors(start_lba + start_sector, sector_count, &chunk)
            .map_err(backend_to_vfs)
    }
}

impl FileOps for RegFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.with_inode_ops(|_inode, fops| {
            let size = fops.current_size() as u64;
            if offset >= size || buf.is_empty() {
                return Ok(0);
            }
            let first_cluster = fops.current_first();
            if first_cluster < 2 {
                return Ok(0);
            }
            let remaining = (size - offset).min(buf.len() as u64) as usize;

            let cluster_size = self.state.cluster_size as u64;
            let mut written = 0usize;

            while written < remaining {
                let file_pos = offset + written as u64;
                let cluster_idx = (file_pos / cluster_size) as u32;
                let in_cluster = file_pos % cluster_size;
                let max_clusters =
                    ((remaining - written) as u64 + in_cluster).div_ceil(cluster_size) as u32;
                let Some((cluster, run_clusters)) =
                    self.contiguous_run(first_cluster, cluster_idx, max_clusters)?
                else {
                    break;
                };
                let run_bytes =
                    (run_clusters as u64 * cluster_size).saturating_sub(in_cluster) as usize;
                let want = run_bytes.min(remaining - written);
                self.read_run(cluster, in_cluster, &mut buf[written..written + want])?;
                written += want;
            }
            Ok(written)
        })
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if self.state.is_read_only() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        if buf.is_empty() {
            return Ok(0);
        }

        let _io = self.io_mu.lock();
        self.with_inode_ops(|inode, fops| {
            // 处理 O_APPEND:offset == u64::MAX 时从 EOF 开始
            let cur_size = fops.current_size() as u64;
            let start_offset = if offset == u64::MAX { cur_size } else { offset };
            let new_end = start_offset
                .checked_add(buf.len() as u64)
                .ok_or(VfsError::FileTooLarge)?;
            if new_end > u32::MAX as u64 {
                return Err(VfsError::FileTooLarge);
            }

            // 稀疏写暂不支持:若起点超过当前 size,先扩 size 并用 0 填补
            if new_end > cur_size {
                fops.grow_to(new_end)?;
                // 如果起点超 EOF,把中间洞清零
                if start_offset > cur_size {
                    self.zero_range(fops.current_first(), cur_size, start_offset)?;
                }
                inode.set_size(new_end);
            }

            // 真正写盘
            let first_cluster = fops.current_first();
            let cluster_size = self.state.cluster_size as u64;
            let mut written = 0usize;
            let total = buf.len();

            while written < total {
                let file_pos = start_offset + written as u64;
                let cluster_idx = (file_pos / cluster_size) as u32;
                let in_cluster = file_pos % cluster_size;
                let max_clusters =
                    ((total - written) as u64 + in_cluster).div_ceil(cluster_size) as u32;
                let Some((cluster, run_clusters)) =
                    self.contiguous_run(first_cluster, cluster_idx, max_clusters)?
                else {
                    return Err(VfsError::Io);
                };
                let run_bytes =
                    (run_clusters as u64 * cluster_size).saturating_sub(in_cluster) as usize;
                let want = run_bytes.min(total - written);
                self.write_run(cluster, in_cluster, &buf[written..written + want])?;
                written += want;
            }
            Ok(written)
        })
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> {
        self.state.sync_all().map_err(backend_to_vfs)
    }
    fn poll(&self, _interest: PollEvents) -> PollEvents {
        PollEvents(0)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl RegFileOps {
    /// 把 `[lo, hi)` 范围的文件内容置 0,仅用于 write past-EOF 的空洞填补。
    fn zero_range(&self, first_cluster: u32, lo: u64, hi: u64) -> VfsResult<()> {
        if hi <= lo || first_cluster < 2 {
            return Ok(());
        }
        let cluster_size = self.state.cluster_size as u64;
        let bps = self.state.bytes_per_sector as u64;
        let mut pos = lo;
        while pos < hi {
            let cluster_idx = (pos / cluster_size) as u32;
            let in_cluster = pos % cluster_size;
            let max_clusters = (hi - pos + in_cluster).div_ceil(cluster_size) as u32;
            let Some((cluster, run_clusters)) =
                self.contiguous_run(first_cluster, cluster_idx, max_clusters)?
            else {
                return Err(VfsError::Io);
            };
            let cluster_lba = self.state.cluster_to_lba(cluster).map_err(backend_to_vfs)?;
            let want = ((run_clusters as u64 * cluster_size) - in_cluster).min(hi - pos);
            let start_sector = in_cluster / bps;
            let end_sector = (in_cluster + want).div_ceil(bps);
            let sector_count = (end_sector - start_sector) as u32;
            let chunk_bytes = (sector_count as u64 * bps) as usize;
            let head_u = !in_cluster.is_multiple_of(bps);
            let tail_u = !(in_cluster + want).is_multiple_of(bps);
            let mut chunk = vec![0u8; chunk_bytes];
            if head_u || tail_u {
                self.state
                    .backend
                    .read_sectors(cluster_lba + start_sector, sector_count, &mut chunk)
                    .map_err(backend_to_vfs)?;
            }
            let chunk_off = (in_cluster - start_sector * bps) as usize;
            for b in &mut chunk[chunk_off..chunk_off + want as usize] {
                *b = 0;
            }
            self.state
                .backend
                .write_sectors(cluster_lba + start_sector, sector_count, &chunk)
                .map_err(backend_to_vfs)?;
            pos += want;
        }
        Ok(())
    }
}

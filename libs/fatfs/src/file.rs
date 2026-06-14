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

use crate::dir::{self, ATTR_DIRECTORY, DirBacking};
use crate::inode::FileInodeOps;
use crate::state::FsState;
use crate::sync_layer::backend_to_vfs;

/// 句柄级簇链缓存的连续预取窗口。
///
/// bench 的 4K x1024 覆盖写每次只请求一个簇,如果只按需追一跳 FAT 链,会把
/// FAT entry 读取和锁开销放大到每次 write_at。小批量预取连续簇可把顺序小 I/O
/// 合并成少量 FAT 扫描,同时避免一次随机读把整条大文件链都缓存下来。
const CHAIN_CACHE_PREFETCH_CLUSTERS: u32 = 64;

// ── 目录 FileOps ─────────────────────────────────────────────────────────

pub struct DirFileOps {
    snapshot: Spinlock<Vec<DirEntry>>,
}

impl DirFileOps {
    pub(crate) fn new(state: &FsState, backing: DirBacking) -> VfsResult<Self> {
        let mut snapshot = Vec::new();
        // 保持打开目录时快照的语义,但扫描时直接生成 VFS DirEntry,避免中间 Vec。
        dir::visit_entries(state, backing, |e| {
            if e.is_volume() {
                return true;
            }
            snapshot.push(DirEntry {
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
            });
            true
        })
        .map_err(backend_to_vfs)?;
        Ok(Self {
            snapshot: Spinlock::new(snapshot),
        })
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
    fn poll(&self, interest: PollEvents) -> PollEvents {
        // 目录枚举不会等待外部事件；readiness 表示可立即尝试 I/O。
        PollEvents::READ_WRITE_READY.intersect(interest)
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
    scratch: IoScratch,
    /// 串行化 append 与扩容(避免两条 O_APPEND 同时读到旧 EOF 再写同位置)。
    io_mu: Spinlock<()>,
}

/// 文件句柄内的临时 I/O 缓冲区。
///
/// 非扇区对齐的读写需要先读出边界扇区再局部修改。这里把临时缓冲区以
/// `Option<Vec<u8>>` 的形式缓存起来:取出缓冲区后立即释放锁,磁盘 I/O 在锁外
/// 完成,归还时再短暂加锁。因此并发读写最多退化为一次临时分配,不会共享同一
/// 个可变缓冲区,也不会在自旋锁临界区里等待块设备。
struct IoScratch {
    slot: Spinlock<Option<Vec<u8>>>,
}

impl IoScratch {
    const fn new() -> Self {
        Self {
            slot: Spinlock::new(None),
        }
    }

    fn take(&self, len: usize) -> Vec<u8> {
        let mut buf = self.slot.lock().take().unwrap_or_default();
        if buf.len() != len {
            buf.resize(len, 0);
        }
        buf
    }

    fn take_zeroed(&self, len: usize) -> Vec<u8> {
        let mut buf = self.take(len);
        buf.fill(0);
        buf
    }

    fn recycle(&self, buf: Vec<u8>) {
        let mut slot = self.slot.lock();
        if slot.is_none() {
            *slot = Some(buf);
        }
    }
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
            scratch: IoScratch::new(),
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
            let (last_idx, last) = {
                let mut cache = self.chain_cache.lock();
                if cache.first_cluster != first_cluster || cache.clusters.is_empty() {
                    cache.reset(first_cluster);
                }
                if cache.clusters.len() > target_idx as usize {
                    return Ok(true);
                }
                let last_idx = cache.clusters.len().saturating_sub(1) as u32;
                (last_idx, *cache.clusters.last().ok_or(VfsError::Io)?)
            };

            let missing = target_idx
                .saturating_add(1)
                .saturating_sub(last_idx.saturating_add(1));
            let max_run = missing
                .saturating_add(CHAIN_CACHE_PREFETCH_CLUSTERS)
                .saturating_add(1);
            let run = self
                .state
                .fat
                .contiguous_run(self.state.backend.as_ref(), last, max_run)
                .map_err(backend_to_vfs)?;
            if run > 1 {
                self.extend_contiguous_cache(first_cluster, last_idx, last, run);
                continue;
            }

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
        let bps_usize = self.state.bytes_per_sector as usize;
        let start_lba = self.state.cluster_to_lba(cluster).map_err(backend_to_vfs)?;
        let mut done = 0usize;
        let mut disk_off = in_cluster;

        // 头部非对齐扇区只能读-拷贝需要的窗口,后续对齐部分直接读入用户缓冲。
        if !disk_off.is_multiple_of(bps) {
            let sector_off = (disk_off % bps) as usize;
            let take = (bps_usize - sector_off).min(out.len());
            let mut sector = self.scratch.take(bps_usize);
            self.state
                .backend
                .read_sectors(start_lba + disk_off / bps, 1, &mut sector)
                .map_err(backend_to_vfs)?;
            out[..take].copy_from_slice(&sector[sector_off..sector_off + take]);
            self.scratch.recycle(sector);
            done += take;
            disk_off += take as u64;
        }

        let aligned_len = ((out.len() - done) / bps_usize) * bps_usize;
        if aligned_len != 0 {
            self.state
                .backend
                .read_sectors(
                    start_lba + disk_off / bps,
                    (aligned_len / bps_usize) as u32,
                    &mut out[done..done + aligned_len],
                )
                .map_err(backend_to_vfs)?;
            done += aligned_len;
            disk_off += aligned_len as u64;
        }

        if done < out.len() {
            let take = out.len() - done;
            let mut sector = self.scratch.take(bps_usize);
            self.state
                .backend
                .read_sectors(start_lba + disk_off / bps, 1, &mut sector)
                .map_err(backend_to_vfs)?;
            out[done..].copy_from_slice(&sector[..take]);
            self.scratch.recycle(sector);
        }
        Ok(())
    }

    fn write_run(&self, cluster: u32, in_cluster: u64, data: &[u8]) -> VfsResult<()> {
        let bps = self.state.bytes_per_sector as u64;
        let bps_usize = self.state.bytes_per_sector as usize;
        let start_lba = self.state.cluster_to_lba(cluster).map_err(backend_to_vfs)?;
        let mut done = 0usize;
        let mut disk_off = in_cluster;

        // 只对头尾非对齐扇区做读-改-写,中间完整扇区直接提交给后端。
        if !disk_off.is_multiple_of(bps) {
            let sector_off = (disk_off % bps) as usize;
            let take = (bps_usize - sector_off).min(data.len());
            let lba = start_lba + disk_off / bps;
            let mut sector = self.scratch.take(bps_usize);
            self.state
                .backend
                .read_sectors(lba, 1, &mut sector)
                .map_err(backend_to_vfs)?;
            sector[sector_off..sector_off + take].copy_from_slice(&data[..take]);
            self.state
                .backend
                .write_sectors(lba, 1, &sector)
                .map_err(backend_to_vfs)?;
            self.scratch.recycle(sector);
            done += take;
            disk_off += take as u64;
        }

        let aligned_len = ((data.len() - done) / bps_usize) * bps_usize;
        if aligned_len != 0 {
            self.state
                .backend
                .write_sectors(
                    start_lba + disk_off / bps,
                    (aligned_len / bps_usize) as u32,
                    &data[done..done + aligned_len],
                )
                .map_err(backend_to_vfs)?;
            done += aligned_len;
            disk_off += aligned_len as u64;
        }

        if done < data.len() {
            let take = data.len() - done;
            let lba = start_lba + disk_off / bps;
            let mut sector = self.scratch.take(bps_usize);
            self.state
                .backend
                .read_sectors(lba, 1, &mut sector)
                .map_err(backend_to_vfs)?;
            sector[..take].copy_from_slice(&data[done..]);
            self.state
                .backend
                .write_sectors(lba, 1, &sector)
                .map_err(backend_to_vfs)?;
            self.scratch.recycle(sector);
        }
        Ok(())
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
    fn poll(&self, interest: PollEvents) -> PollEvents {
        // 普通文件不会阻塞等待设备事件；读写 readiness 应立即满足。
        PollEvents::READ_WRITE_READY.intersect(interest)
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
            self.zero_run(cluster_lba, in_cluster, want)?;
            pos += want;
        }
        Ok(())
    }

    fn zero_run(&self, start_lba: u64, in_cluster: u64, len: u64) -> VfsResult<()> {
        let bps = self.state.bytes_per_sector as u64;
        let bps_usize = self.state.bytes_per_sector as usize;
        let mut done = 0u64;
        let mut disk_off = in_cluster;

        if !disk_off.is_multiple_of(bps) {
            let sector_off = (disk_off % bps) as usize;
            let take = ((bps_usize - sector_off) as u64).min(len);
            let lba = start_lba + disk_off / bps;
            let mut sector = self.scratch.take(bps_usize);
            self.state
                .backend
                .read_sectors(lba, 1, &mut sector)
                .map_err(backend_to_vfs)?;
            sector[sector_off..sector_off + take as usize].fill(0);
            self.state
                .backend
                .write_sectors(lba, 1, &sector)
                .map_err(backend_to_vfs)?;
            self.scratch.recycle(sector);
            done += take;
            disk_off += take;
        }

        let aligned_bytes = ((len - done) / bps) * bps;
        if aligned_bytes != 0 {
            self.write_zeroed_sectors(start_lba + disk_off / bps, aligned_bytes / bps)?;
            done += aligned_bytes;
            disk_off += aligned_bytes;
        }

        if done < len {
            let take = (len - done) as usize;
            let lba = start_lba + disk_off / bps;
            let mut sector = self.scratch.take(bps_usize);
            self.state
                .backend
                .read_sectors(lba, 1, &mut sector)
                .map_err(backend_to_vfs)?;
            sector[..take].fill(0);
            self.state
                .backend
                .write_sectors(lba, 1, &sector)
                .map_err(backend_to_vfs)?;
            self.scratch.recycle(sector);
        }
        Ok(())
    }

    fn write_zeroed_sectors(&self, mut lba: u64, mut sectors: u64) -> VfsResult<()> {
        if sectors == 0 {
            return Ok(());
        }
        let bps = self.state.bytes_per_sector as usize;
        let sectors_per_chunk = self.state.sectors_per_cluster.max(1) as u64;
        let mut zero = self
            .scratch
            .take_zeroed((sectors.min(sectors_per_chunk) as usize) * bps);
        while sectors != 0 {
            let take = sectors.min(sectors_per_chunk) as u32;
            let bytes = take as usize * bps;
            if zero.len() != bytes {
                zero.resize(bytes, 0);
            }
            zero.fill(0);
            self.state
                .backend
                .write_sectors(lba, take, &zero[..bytes])
                .map_err(backend_to_vfs)?;
            lba += u64::from(take);
            sectors -= u64::from(take);
        }
        self.scratch.recycle(zero);
        Ok(())
    }
}

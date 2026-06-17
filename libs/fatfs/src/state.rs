//! 驱动外暴露的类型 + 共享状态。
//!
//! [`BlockBackend`] 是 FAT 驱动对外的同步 I/O 契约,由调用方把块设备驱动
//! 适配到它。[`FatFsDriver`] 实现 [`vfs::superblock::FsDriver`],挂载时
//! 产生一个持有 [`FsState`] 的 [`Superblock`](vfs::superblock::Superblock)。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use vfs::cred::{Gid, Uid};
use vfs::dentry::Dentry;
use vfs::error::{VfsError, VfsResult};
use vfs::inode::{Inode, InodeId, InodeMeta};
use vfs::mount::MountFlags;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{FsDriver, FsDriverFlags, InodeCache, Superblock, SuperblockOps};
use vfs::sync::Spinlock;

use crate::bpb::{self, FatKind};
use crate::fat::FatTable;
use crate::inode::DirInodeOps;

/// 块设备错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockBackendError {
    Io,
    OutOfRange,
    Unsupported,
}

/// 文件系统与块设备之间的同步契约。
///
/// 语义:单次 `read_sectors` / `write_sectors` 必须在返回前完成全部 `count`
/// 个扇区,或整体失败返回错误;实现方负责排队、DMA、中断回收等。
pub trait BlockBackend: Send + Sync {
    fn sector_size(&self) -> u32;
    fn sector_count(&self) -> u64;
    fn read_sectors(&self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), BlockBackendError>;
    fn write_sectors(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), BlockBackendError>;
}

static FATFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// FAT32 FSInfo 扇区信息(仅 FAT32 有意义,其它变体字段为 `None`)。
pub(crate) struct FsInfo {
    pub(crate) sector_lba: u64,
    pub(crate) free_count: AtomicU32,
    pub(crate) next_free: AtomicU32,
    pub(crate) dirty: Spinlock<bool>,
}

/// 一个已挂载 FAT 实例的共享状态。所有 Inode/File 通过 `Arc<FsState>` 引用。
pub(crate) struct FsState {
    pub(crate) backend: Arc<dyn BlockBackend>,
    pub(crate) kind: FatKind,
    pub(crate) bytes_per_sector: u32,
    pub(crate) sectors_per_cluster: u32,
    pub(crate) cluster_size: u32,
    pub(crate) reserved_sectors: u32,
    pub(crate) num_fats: u32,
    pub(crate) fat_size_sectors: u32,
    pub(crate) root_cluster: u32,
    pub(crate) root_dir_sectors: u32,
    #[allow(dead_code)]
    pub(crate) root_entries: u32,
    pub(crate) first_data_sector: u64,
    pub(crate) total_clusters: u32,
    pub(crate) fat: FatTable,
    pub(crate) fs_info: Option<FsInfo>,
    pub(crate) read_only: core::sync::atomic::AtomicBool,
    /// 挂载实例是否被驱动强制为只读。
    ///
    /// 该标志来自 `FatFsDriver::new_read_only()`，不同于普通 remount(ro)。
    /// 强制只读实例不能再通过 remount 切回可写，避免上层把诊断/探测挂载误升级
    /// 成会修改介质的挂载。
    force_read_only: bool,
    pub(crate) next_synth_ino: AtomicU64,
    /// 文件系统级写锁：所有修改操作（create/mkdir/unlink/rmdir/rename/write/truncate）
    /// 必须持有此锁。读操作不需要。
    pub(crate) write_lock: Spinlock<()>,
}

impl FsState {
    #[inline]
    pub(crate) fn cluster_to_lba(&self, cluster: u32) -> Result<u64, BlockBackendError> {
        if cluster < 2 {
            return Err(BlockBackendError::OutOfRange);
        }
        Ok(self.first_data_sector + ((cluster as u64) - 2) * self.sectors_per_cluster as u64)
    }

    pub(crate) fn next_synth_ino(&self) -> u64 {
        0x8000_0000_0000_0000 | self.next_synth_ino.fetch_add(1, Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only.load(Ordering::Acquire)
    }

    pub(crate) fn remount_read_only(&self, read_only: bool) -> Result<(), VfsError> {
        if self.force_read_only && !read_only {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        if read_only {
            // 切换到只读前必须先等所有写事务退出并刷回 FAT/FSInfo。
            // 如果先设置 read_only，sync_all() 会按只读挂载直接返回，可能遗漏
            // 之前已经变脏但尚未写回的 FAT 缓存。
            let _guard = self.write_lock.lock();
            if !self.is_read_only() {
                self.sync_all().map_err(crate::sync_layer::backend_to_vfs)?;
            }
            self.read_only.store(true, Ordering::Release);
        } else {
            self.read_only.store(false, Ordering::Release);
        }
        Ok(())
    }

    /// 串行执行一次 FAT 写事务。
    ///
    /// FAT 目录项、FAT 表和 FSInfo 没有日志保护；单个 VFS 修改操作可能同时更新
    /// 多处结构。这里用文件系统级写锁把这些更新收束成互斥事务，防止并发
    /// create/write/truncate/rename 交错造成目录项和 FAT 链不一致。
    pub(crate) fn with_write_transaction<R>(
        &self,
        f: impl FnOnce() -> VfsResult<R>,
    ) -> VfsResult<R> {
        if self.is_read_only() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        let _guard = self.write_lock.lock();
        if self.is_read_only() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        f()
    }

    pub(crate) fn alloc_cluster(&self, prev: Option<u32>) -> Result<u32, BlockBackendError> {
        let c = self.fat.alloc_cluster(self.backend.as_ref(), prev)?;
        if let Some(fi) = &self.fs_info {
            let prev_count = fi.free_count.load(Ordering::Acquire);
            if prev_count > 0 {
                fi.free_count.store(prev_count - 1, Ordering::Release);
            }
            let total_with_2 = self.total_clusters.saturating_add(2);
            let next_candidate = c.saturating_add(1);
            let next = if next_candidate >= total_with_2 {
                2
            } else {
                next_candidate
            };
            fi.next_free.store(next, Ordering::Release);
            *fi.dirty.lock() = true;
        }
        Ok(c)
    }

    pub(crate) fn alloc_cluster_run(
        &self,
        prev: Option<u32>,
        count: u32,
    ) -> Result<(u32, u32), BlockBackendError> {
        let (first, last) = self
            .fat
            .alloc_cluster_run(self.backend.as_ref(), prev, count)?;
        if let Some(fi) = &self.fs_info {
            let prev_count = fi.free_count.load(Ordering::Acquire);
            fi.free_count
                .store(prev_count.saturating_sub(count), Ordering::Release);
            let total_with_2 = self.total_clusters.saturating_add(2);
            let next_candidate = last.saturating_add(1);
            let next = if next_candidate >= total_with_2 {
                2
            } else {
                next_candidate
            };
            fi.next_free.store(next, Ordering::Release);
            *fi.dirty.lock() = true;
        }
        Ok((first, last))
    }

    pub(crate) fn free_chain(&self, head: u32) -> Result<u32, BlockBackendError> {
        let count = self.fat.free_chain(self.backend.as_ref(), head)?;
        if count > 0
            && let Some(fi) = &self.fs_info
        {
            fi.free_count.fetch_add(count, Ordering::AcqRel);
            let cur = fi.next_free.load(Ordering::Acquire);
            if head >= 2 && head < cur {
                fi.next_free.store(head, Ordering::Release);
            }
            *fi.dirty.lock() = true;
        }
        Ok(count)
    }

    pub(crate) fn sync_all(&self) -> Result<(), BlockBackendError> {
        if self.is_read_only() {
            return Ok(());
        }
        if let Some(fi) = &self.fs_info {
            self.flush_fs_info(fi)?;
        }
        self.fat.flush_with_mirror(
            self.backend.as_ref(),
            self.reserved_sectors as u64,
            self.fat_size_sectors as u64,
            self.num_fats,
            self.bytes_per_sector,
        )
    }

    fn flush_fs_info(&self, fi: &FsInfo) -> Result<(), BlockBackendError> {
        let mut dirty_guard = fi.dirty.lock();
        if !*dirty_guard {
            return Ok(());
        }
        let mut sec = vec![0u8; self.bytes_per_sector as usize];
        self.backend.read_sectors(fi.sector_lba, 1, &mut sec)?;
        let free = fi.free_count.load(Ordering::Acquire);
        let next = fi.next_free.load(Ordering::Acquire);
        sec[488..492].copy_from_slice(&free.to_le_bytes());
        sec[492..496].copy_from_slice(&next.to_le_bytes());
        self.backend.write_sectors(fi.sector_lba, 1, &sec)?;
        *dirty_guard = false;
        Ok(())
    }
}

/// 对外暴露的 FAT 驱动句柄。
pub struct FatFsDriver {
    backend: Spinlock<Option<Arc<dyn BlockBackend>>>,
    force_ro: bool,
}

impl FatFsDriver {
    /// 创建新驱动。需要在调用 VFS mount 前 [`bind_backend`]。
    pub const fn new() -> Self {
        Self {
            backend: Spinlock::new(None),
            force_ro: false,
        }
    }

    /// 创建只读驱动:无论 MountFlags 如何都不允许写。
    pub const fn new_read_only() -> Self {
        Self {
            backend: Spinlock::new(None),
            force_ro: true,
        }
    }

    /// 绑定底层块设备。必须在 mount 之前调用。
    pub fn bind_backend(&self, backend: Arc<dyn BlockBackend>) {
        *self.backend.lock() = Some(backend);
    }
}

impl Default for FatFsDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl FsDriver for FatFsDriver {
    fn name(&self) -> &'static str {
        "fatfs"
    }

    fn flags(&self) -> FsDriverFlags {
        // FAT 没有"独立型"限制,也允许多卷同驱动挂载多次。
        FsDriverFlags::default()
    }

    fn mount(&self, _dev: Option<&str>, _data: &str) -> VfsResult<Arc<Superblock>> {
        let backend = self
            .backend
            .lock()
            .as_ref()
            .map(Arc::clone)
            .ok_or(VfsError::NoDevice)?;
        mount_impl(backend, self.force_ro)
    }

    fn kill_sb(&self, sb: Arc<Superblock>) {
        // 卸载时把 FAT 缓存 / FSInfo 刷回磁盘。
        if let Some(ops) = sb.ops.as_any().downcast_ref::<FatFsSuperblockOps>() {
            let _ = ops.state.sync_all();
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub(crate) struct FatFsSuperblockOps {
    pub(crate) state: Arc<FsState>,
}

impl SuperblockOps for FatFsSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        // 创建逻辑由 InodeOps::create/mkdir 完成,这里不暴露独立分配。
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        let total = self.state.total_clusters as u64;
        let free = self
            .state
            .fs_info
            .as_ref()
            .map(|fi| fi.free_count.load(Ordering::Relaxed) as u64)
            .unwrap_or(0);
        Ok(FsStat {
            fs_type: match self.state.kind {
                FatKind::Fat12 => 0x4649_4531, // "FIE1"
                FatKind::Fat16 => 0x4649_4532, // "FIE2"
                FatKind::Fat32 => 0x4d44_4f53, // "MDOS"
            },
            block_size: self.state.cluster_size as u64,
            total_blocks: total,
            free_blocks: free,
            avail_blocks: free,
            total_inodes: 0,
            free_inodes: 0,
            fs_id: sb.fs_id.raw(),
            name_max: 255,
        })
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        self.state
            .sync_all()
            .map_err(crate::sync_layer::backend_to_vfs)
    }

    fn remount(&self, _sb: &Arc<Superblock>, new_flags: MountFlags) -> VfsResult<()> {
        self.state.remount_read_only(new_flags.is_rdonly())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn mount_impl(backend: Arc<dyn BlockBackend>, force_ro: bool) -> VfsResult<Arc<Superblock>> {
    let info = bpb::parse(backend.as_ref()).map_err(crate::sync_layer::backend_to_vfs)?;

    // FSInfo(仅 FAT32 才会真的有意义)
    let fs_info =
        if info.kind == FatKind::Fat32 && info.fs_info_sector != 0 && info.fs_info_sector != 0xffff
        {
            let lba = info.fs_info_sector as u64;
            let mut sec = vec![0u8; info.bytes_per_sector as usize];
            backend
                .read_sectors(lba, 1, &mut sec)
                .map_err(crate::sync_layer::backend_to_vfs)?;
            // 校验 leading/struct/trailing signature
            let lead = u32::from_le_bytes([sec[0], sec[1], sec[2], sec[3]]);
            let struct_sig = u32::from_le_bytes([sec[484], sec[485], sec[486], sec[487]]);
            let trail = u32::from_le_bytes([sec[508], sec[509], sec[510], sec[511]]);
            let free = u32::from_le_bytes([sec[488], sec[489], sec[490], sec[491]]);
            let next = u32::from_le_bytes([sec[492], sec[493], sec[494], sec[495]]);
            if lead == 0x4161_5252 && struct_sig == 0x6141_7272 && trail == 0xaa55_0000 {
                Some(FsInfo {
                    sector_lba: lba,
                    free_count: AtomicU32::new(free),
                    next_free: AtomicU32::new(if next == 0xffff_ffff { 2 } else { next.max(2) }),
                    dirty: Spinlock::new(false),
                })
            } else {
                None
            }
        } else {
            None
        };
    let fat_hint = fs_info
        .as_ref()
        .map(|fi| fi.next_free.load(Ordering::Relaxed))
        .unwrap_or(2);

    // FAT 表缓存
    let fat = FatTable::new(
        info.kind,
        info.reserved_sectors as u64,
        info.fat_size_sectors,
        info.num_fats,
        info.bytes_per_sector,
        info.total_clusters,
        256,
        fat_hint,
    );

    let state = Arc::new(FsState {
        backend: Arc::clone(&backend),
        kind: info.kind,
        bytes_per_sector: info.bytes_per_sector,
        sectors_per_cluster: info.sectors_per_cluster,
        cluster_size: info.bytes_per_sector * info.sectors_per_cluster,
        reserved_sectors: info.reserved_sectors,
        num_fats: info.num_fats,
        fat_size_sectors: info.fat_size_sectors,
        root_cluster: info.root_cluster,
        root_dir_sectors: info.root_dir_sectors,
        root_entries: info.root_entries,
        first_data_sector: info.first_data_sector as u64,
        total_clusters: info.total_clusters,
        fat,
        fs_info,
        read_only: core::sync::atomic::AtomicBool::new(force_ro),
        force_read_only: force_ro,
        next_synth_ino: AtomicU64::new(1),
        write_lock: Spinlock::new(()),
    });

    let fs_id = FsId::new(FATFS_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));

    // 根目录 Inode:FAT32 用 `root_cluster`;FAT12/16 用合成 ino。
    let root_ino = if info.kind == FatKind::Fat32 {
        info.root_cluster as u64
    } else {
        state.next_synth_ino()
    };

    let sb = Superblock::new(|weak_sb| {
        let sb_ops = Box::new(FatFsSuperblockOps {
            state: Arc::clone(&state),
        });

        let now = Timespec::ZERO;
        let mode = if state.is_read_only() {
            FileMode::new(0o555)
        } else {
            FileMode::new(0o755)
        };
        let root_meta = InodeMeta {
            size: 0,
            nlink: 2,
            mode,
            uid: Uid::ROOT,
            gid: Gid::ROOT,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        let root_ops = DirInodeOps::new_root(Arc::clone(&state));
        let root_inode = Inode::new(
            InodeId {
                fs_id,
                ino: root_ino,
            },
            FileType::Directory,
            DevId::new(0, 0),
            state.cluster_size,
            None,
            root_meta,
            Arc::new(root_ops) as Arc<dyn vfs::inode::InodeOps + Send + Sync>,
            weak_sb.clone(),
        );
        let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));

        Superblock {
            fs_type: "fatfs",
            fs_id,
            dev_id: None,
            block_size: state.cluster_size,
            name_max: 255,
            root_inode,
            root_dentry,
            inode_cache: InodeCache::new(),
            ops: sb_ops,
            self_weak: weak_sb,
        }
    });

    Ok(sb)
}

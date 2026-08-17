//! Tmpfs - 内存文件系统驱动。
//!
//! Tmpfs 是一个完全基于内存的文件系统，所有数据存储在 RAM 中，重启后丢失。
//! 常用于 `/tmp`、`/dev/shm` 等临时存储场景。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, Ordering};

use vfs::cred::{Credentials, Gid, Uid};
use vfs::dentry::{Dentry, SmallStr};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FallocateMode, FileOps, OpenOptions, PollEvents};
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
const TMPFS_PAGE_SIZE: usize = 4096;
const TMPFS_PAGE_SIZE_U64: u64 = TMPFS_PAGE_SIZE as u64;
// 一个槽池批量管理 16 个页，但每个页单独申请，避免依赖连续的 64 KiB 大块。
const TMPFS_SLAB_PAGES: usize = 16;
const TMPFS_SLAB_FREE: u16 = u16::MAX;
const TMPFS_MAX_BATCH_PAGES: usize = 8;

#[derive(Clone, Copy)]
struct TmpfsMountOptions {
    total_blocks: u64,
    total_inodes: u64,
    mode: FileMode,
    uid: Uid,
    gid: Gid,
}

impl Default for TmpfsMountOptions {
    fn default() -> Self {
        Self {
            total_blocks: TMPFS_VIRTUAL_BLOCKS,
            total_inodes: TMPFS_VIRTUAL_INODES,
            mode: FileMode::new(0o755),
            uid: Uid::ROOT,
            gid: Gid::ROOT,
        }
    }
}

fn parse_decimal(value: &str) -> VfsResult<u64> {
    if value.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    let mut result = 0u64;
    for byte in value.bytes() {
        if !byte.is_ascii_digit() {
            return Err(VfsError::InvalidArgument);
        }
        result = result
            .checked_mul(10)
            .and_then(|value| value.checked_add((byte - b'0') as u64))
            .ok_or(VfsError::FileTooLarge)?;
    }
    Ok(result)
}

fn parse_quantity(value: &str) -> VfsResult<u64> {
    let value = value.trim();
    // Linux 的 tmpfs 支持 `size=20%` 这类按物理内存总量计算的百分比值，
    // OpenRC 等发行版启动脚本会直接使用该写法挂载 /run。
    if let Some(percent) = value.strip_suffix('%') {
        let number = parse_decimal(percent.trim())?;
        if number > 100 {
            return Err(VfsError::InvalidArgument);
        }
        let total_physical = allocator::KERNEL_ALLOCATOR.detailed_stats().total_physical as u64;
        return Ok(total_physical / 100 * number);
    }
    let digit_end = value
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(value.len());
    let number = parse_decimal(&value[..digit_end])?;
    let suffix = &value[digit_end..];
    let multiplier = match suffix {
        "" | "b" | "B" => 1,
        "k" | "K" | "kb" | "KB" => 1024,
        "m" | "M" | "mb" | "MB" => 1024 * 1024,
        "g" | "G" | "gb" | "GB" => 1024 * 1024 * 1024,
        "t" | "T" | "tb" | "TB" => 1024u64 * 1024 * 1024 * 1024,
        "p" | "P" | "pb" | "PB" => 1024u64 * 1024 * 1024 * 1024 * 1024,
        _ => return Err(VfsError::InvalidArgument),
    };
    number.checked_mul(multiplier).ok_or(VfsError::FileTooLarge)
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

fn parse_mount_options(data: &str) -> VfsResult<TmpfsMountOptions> {
    let mut options = TmpfsMountOptions::default();
    for item in data.split(',').filter(|item| !item.is_empty()) {
        let (key, value) = item.split_once('=').unwrap_or((item, ""));
        match key {
            "size" => {
                let bytes = parse_quantity(value)?;
                options.total_blocks = bytes
                    .checked_add(TMPFS_PAGE_SIZE_U64 - 1)
                    .ok_or(VfsError::FileTooLarge)?
                    / TMPFS_PAGE_SIZE_U64;
            }
            "nr_blocks" => options.total_blocks = parse_quantity(value)?,
            "nr_inodes" => options.total_inodes = parse_quantity(value)?,
            "mode" => options.mode = FileMode::new(parse_octal(value)?),
            "uid" => {
                options.uid = Uid(parse_decimal(value)?
                    .try_into()
                    .map_err(|_| VfsError::InvalidArgument)?)
            }
            "gid" => {
                options.gid = Gid(parse_decimal(value)?
                    .try_into()
                    .map_err(|_| VfsError::InvalidArgument)?)
            }
            // 这些选项影响 Linux 的其他 tmpfs 后端；当前内存后端没有对应
            // 的策略状态，但接受它们可以保持通用挂载工具的参数兼容性。
            "huge" | "mpol" | "noswap" | "inode32" | "inode64" => {}
            // busybox mount 会把 `-o nosuid,nodev` 等约束也放进 data 串而不是
            // 全部转成 mount(2) 标志位；时间戳与执行约束由 VFS 层处理，这里
            // 只负责不拒绝常见组合（OpenRC 挂 /run 使用 strictatime 等）。
            "strictatime" | "relatime" | "noatime" | "nodiratime" | "lazytime" | "nosuid"
            | "nodev" | "noexec" | "suid" | "dev" | "exec" | "sync" | "async" | "dirsync"
            | "rw" | "ro" | "defaults" | "mand" | "nomand" => {}
            _ => return Err(VfsError::InvalidArgument),
        }
    }
    if options.total_inodes == 0 {
        return Err(VfsError::InvalidArgument);
    }
    Ok(options)
}

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

    fn mount(&self, _source: Option<&str>, data: &str) -> VfsResult<Arc<Superblock>> {
        let options = parse_mount_options(data)?;
        let fs_id = FsId::new(TMPFS_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));

        let sb = Superblock::new(|weak_sb| {
            let sb_ops = Box::new(TmpfsSuperblockOps {
                next_ino: AtomicU64::new(2),
                total_blocks: options.total_blocks,
                used_blocks: AtomicU64::new(0),
                total_inodes: options.total_inodes,
                used_inodes: AtomicU64::new(1),
                page_pool: Arc::new(TmpfsPagePool::new()),
            });

            // 创建根目录 inode
            let now = Timespec::now();
            let root_meta = InodeMeta {
                size: 0,
                nlink: 2,
                mode: options.mode,
                uid: options.uid,
                gid: options.gid,
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
    total_blocks: u64,
    used_blocks: AtomicU64,
    total_inodes: u64,
    used_inodes: AtomicU64,
    page_pool: Arc<TmpfsPagePool>,
}

impl TmpfsSuperblockOps {
    fn try_reserve(counter: &AtomicU64, limit: u64, amount: u64) -> VfsResult<()> {
        if amount == 0 {
            return Ok(());
        }
        let mut current = counter.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(amount) else {
                return Err(VfsError::NoSpace);
            };
            if next > limit {
                return Err(VfsError::NoSpace);
            }
            match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }

    fn release(counter: &AtomicU64, amount: u64) {
        if amount == 0 {
            return;
        }
        let previous = counter.fetch_sub(amount, Ordering::AcqRel);
        debug_assert!(previous >= amount, "tmpfs resource counter underflow");
    }

    fn alloc_ino(&self) -> VfsResult<u64> {
        Self::try_reserve(&self.used_inodes, self.total_inodes, 1)?;
        Ok(self.next_ino.fetch_add(1, Ordering::Relaxed))
    }

    fn release_inode(&self) {
        Self::release(&self.used_inodes, 1);
    }

    fn reserve_blocks(&self, blocks: u64) -> VfsResult<()> {
        Self::try_reserve(&self.used_blocks, self.total_blocks, blocks)
    }

    fn release_blocks(&self, blocks: u64) {
        Self::release(&self.used_blocks, blocks);
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
        let used_blocks = self.used_blocks.load(Ordering::Acquire);
        let used_inodes = self.used_inodes.load(Ordering::Acquire);
        let free_blocks = self.total_blocks.saturating_sub(used_blocks);
        let free_inodes = self.total_inodes.saturating_sub(used_inodes);
        Ok(FsStat {
            fs_type: 0x01021994,
            block_size: sb.block_size as u64,
            total_blocks: self.total_blocks,
            free_blocks,
            avail_blocks: free_blocks,
            total_inodes: self.total_inodes,
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

// ── 分片页槽池 ───────────────────────────────────────────────────────────────

struct TmpfsPageSlab {
    data: Box<[Box<[UnsafeCell<u8>]>]>,
    free: Spinlock<u16>,
    shard: usize,
    pool: Weak<TmpfsPagePool>,
}

impl TmpfsPageSlab {
    fn new(shard: usize, pool: Weak<TmpfsPagePool>) -> VfsResult<Self> {
        let mut data: Vec<Box<[UnsafeCell<u8>]>> = Vec::new();
        data.try_reserve_exact(TMPFS_SLAB_PAGES)
            .map_err(|_| VfsError::OutOfMemory)?;
        for _ in 0..TMPFS_SLAB_PAGES {
            let mut page: Vec<UnsafeCell<u8>> = Vec::new();
            page.try_reserve_exact(TMPFS_PAGE_SIZE)
                .map_err(|_| VfsError::OutOfMemory)?;
            // Safety: `UnsafeCell<u8>` 与 `u8` 布局相同且全零有效。capacity 已经精确
            // 预留；初始化全部元素后才发布 Vec 长度。
            unsafe {
                core::ptr::write_bytes(page.as_mut_ptr(), 0, TMPFS_PAGE_SIZE);
                page.set_len(TMPFS_PAGE_SIZE);
            }
            data.push(page.into_boxed_slice());
        }
        Ok(Self {
            data: data.into_boxed_slice(),
            free: Spinlock::new(TMPFS_SLAB_FREE),
            shard,
            pool,
        })
    }
}

// Safety: 每个已分配 slot 只有一个 `TmpfsPageSlot` 句柄；不同 slot 对应 slab 中
// 不重叠的 4 KiB 区间。同一 inode 的访问由 data 锁串行化，跨 inode 只能访问不同
// slot，因此不会形成重叠的可变引用。
unsafe impl Send for TmpfsPageSlab {}
unsafe impl Sync for TmpfsPageSlab {}

#[derive(Clone, Copy)]
struct TmpfsSlotBatch {
    slots: [u8; TMPFS_MAX_BATCH_PAGES],
    len: usize,
}

impl TmpfsSlotBatch {
    const fn new() -> Self {
        Self {
            slots: [0; TMPFS_MAX_BATCH_PAGES],
            len: 0,
        }
    }
}

struct TmpfsPagePool {
    /// slab 固定归属创建 CPU 对应的 shard；Weak 不延长空 slab 生命周期。任务迁核
    /// 只影响下一次补批选择哪个 shard，已租给 inode 的 slot 不依赖当前 CPU。
    available: [Spinlock<Vec<Weak<TmpfsPageSlab>>>; sched::NR_CPUS],
}

impl TmpfsPagePool {
    fn new() -> Self {
        Self {
            available: [const { Spinlock::new(Vec::new()) }; sched::NR_CPUS],
        }
    }

    fn current_shard() -> usize {
        if sched::is_ready() {
            sched::current_cpu_id().min(sched::NR_CPUS - 1)
        } else {
            0
        }
    }

    fn alloc_batch(
        self: &Arc<Self>,
        count: usize,
        output: &mut Vec<TmpfsPageSlot>,
    ) -> VfsResult<()> {
        debug_assert!((1..=TMPFS_MAX_BATCH_PAGES).contains(&count));
        debug_assert!(output.is_empty());
        output
            .try_reserve(count)
            .map_err(|_| VfsError::OutOfMemory)?;

        let shard_index = Self::current_shard();
        while output.len() < count {
            let candidate = self.available[shard_index].lock().pop();
            let Some(candidate) = candidate else {
                break;
            };
            let Some(slab) = candidate.upgrade() else {
                continue;
            };
            let (batch, remains_available) = {
                let mut free = slab.free.lock();
                let batch = take_tmpfs_slots(&mut free, count - output.len());
                (batch, *free != 0)
            };
            if batch.len == 0 {
                continue;
            }
            if remains_available {
                self.available[shard_index]
                    .lock()
                    .push(Arc::downgrade(&slab));
            }
            Self::append_slots(output, &slab, &batch);
        }

        while output.len() < count {
            let slab = match TmpfsPageSlab::new(shard_index, Arc::downgrade(self)) {
                Ok(slab) => Arc::new(slab),
                Err(error) => {
                    output.clear();
                    return Err(error);
                }
            };
            let batch = {
                let mut free = slab.free.lock();
                take_tmpfs_slots(&mut free, count - output.len())
            };
            debug_assert!(batch.len != 0);
            if batch.len != 0 {
                let remains_available = *slab.free.lock() != 0;
                if remains_available {
                    self.available[shard_index]
                        .lock()
                        .push(Arc::downgrade(&slab));
                }
                Self::append_slots(output, &slab, &batch);
            }
        }

        debug_assert_eq!(output.len(), count);
        Ok(())
    }

    fn append_slots(
        output: &mut Vec<TmpfsPageSlot>,
        slab: &Arc<TmpfsPageSlab>,
        batch: &TmpfsSlotBatch,
    ) {
        for &slot in &batch.slots[..batch.len] {
            output.push(TmpfsPageSlot::new(slab, slot));
        }
    }
}

fn take_tmpfs_slots(free: &mut u16, max: usize) -> TmpfsSlotBatch {
    let mut batch = TmpfsSlotBatch::new();
    let max = max.min(TMPFS_MAX_BATCH_PAGES);
    while *free != 0 && batch.len < max {
        let slot = free.trailing_zeros() as u8;
        *free &= !(1u16 << slot);
        batch.slots[batch.len] = slot;
        batch.len += 1;
    }
    batch
}

fn release_tmpfs_slot(free: &mut u16, slot: u8) -> bool {
    debug_assert!((slot as usize) < TMPFS_SLAB_PAGES);
    let bit = 1u16 << slot;
    debug_assert_eq!(*free & bit, 0, "tmpfs page slot released twice");
    *free |= bit;
    *free == TMPFS_SLAB_FREE
}

struct TmpfsPageLease {
    slab: Weak<TmpfsPageSlab>,
    slot: u8,
}

impl Drop for TmpfsPageLease {
    fn drop(&mut self) {
        let Some(slab) = self.slab.upgrade() else {
            return;
        };
        let was_full = {
            let mut free = slab.free.lock();
            let was_full = *free == 0;
            release_tmpfs_slot(&mut free, self.slot);
            was_full
        };
        if was_full && let Some(pool) = slab.pool.upgrade() {
            pool.available[slab.shard]
                .lock()
                .push(Arc::downgrade(&slab));
        }
    }
}

struct TmpfsPageSlot {
    /// Rust 按声明顺序析构字段：先撤销本页的 slab 强引用，再由 lease 归还 slot。
    /// 若本页是 slab 最后一个强引用，backing 直接释放；否则 Weak 才能升级并复用。
    slab: Arc<TmpfsPageSlab>,
    lease: TmpfsPageLease,
}

impl TmpfsPageSlot {
    fn new(slab: &Arc<TmpfsPageSlab>, slot: u8) -> Self {
        Self {
            slab: Arc::clone(slab),
            lease: TmpfsPageLease {
                slab: Arc::downgrade(slab),
                slot,
            },
        }
    }

    fn range(&self) -> core::ops::Range<usize> {
        0..TMPFS_PAGE_SIZE
    }

    fn data(&self) -> &[u8] {
        let range = self.range();
        // Safety: 活跃 slot 唯一且固定映射到该区间；backing 在 Arc 生命周期内不移动。
        unsafe {
            core::slice::from_raw_parts(
                self.slab.data[self.lease.slot as usize]
                    .as_ptr()
                    .add(range.start)
                    .cast::<u8>(),
                range.len(),
            )
        }
    }

    fn data_mut(&mut self) -> &mut [u8] {
        let range = self.range();
        // Safety: `&mut TmpfsPageSlot` 排除同一 slot 的第二个访问；池位图保证跨 inode
        // slot 不重叠，同一 inode 又由 data 锁串行化。
        unsafe {
            core::slice::from_raw_parts_mut(
                self.slab.data[self.lease.slot as usize]
                    .as_ptr()
                    .add(range.start)
                    .cast_mut()
                    .cast::<u8>(),
                range.len(),
            )
        }
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

struct TmpfsPage {
    index: u64,
    data: TmpfsPageSlot,
}

struct TmpfsFileData {
    size: u64,
    pages: Vec<TmpfsPage>,
    unused_slots: Vec<TmpfsPageSlot>,
    next_batch: usize,
}

impl TmpfsFileData {
    const fn new() -> Self {
        Self {
            size: 0,
            pages: Vec::new(),
            unused_slots: Vec::new(),
            next_batch: 1,
        }
    }

    fn blocks(&self) -> u64 {
        (self.pages.len() as u64 * TMPFS_PAGE_SIZE_U64).div_ceil(512)
    }

    fn alloc_page_slot(&mut self, accounting: &TmpfsSuperblockOps) -> VfsResult<TmpfsPageSlot> {
        if self.unused_slots.is_empty() {
            accounting
                .page_pool
                .alloc_batch(self.next_batch, &mut self.unused_slots)?;
            self.next_batch = next_tmpfs_batch_size(self.next_batch);
        }
        let mut page = self.unused_slots.pop().ok_or(VfsError::OutOfMemory)?;
        // 回收槽可能保留旧文件内容；真正消费时才清零，避免小文件关闭时为未使用
        // 的批量槽做无效内存写入。
        page.data_mut().fill(0);
        Ok(page)
    }

    fn release_unused_slots(&mut self) {
        self.unused_slots.clear();
        self.next_batch = 1;
    }

    fn truncate(&mut self, new_size: u64) -> u64 {
        let old_pages = self.pages.len();
        if new_size < self.size {
            let keep_pages = new_size.div_ceil(TMPFS_PAGE_SIZE_U64);
            self.pages.retain(|page| page.index < keep_pages);
            if new_size % TMPFS_PAGE_SIZE_U64 != 0 {
                let tail_index = new_size / TMPFS_PAGE_SIZE_U64;
                let tail_offset = (new_size % TMPFS_PAGE_SIZE_U64) as usize;
                if let Some(pos) = self.page_pos(tail_index).ok() {
                    self.pages[pos].data.data_mut()[tail_offset..].fill(0);
                }
            }
        }
        self.size = new_size;
        (old_pages - self.pages.len()) as u64
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> usize {
        if offset >= self.size || buf.is_empty() {
            return 0;
        }
        let end = offset.saturating_add(buf.len() as u64).min(self.size);
        let n = (end - offset) as usize;
        let out = &mut buf[..n];

        let first_page = offset / TMPFS_PAGE_SIZE_U64;
        let last_page = (end - 1) / TMPFS_PAGE_SIZE_U64;
        let first_pos = self.lower_bound(first_page);
        let mut filled = 0usize;
        for page in &self.pages[first_pos..] {
            if page.index > last_page {
                break;
            }
            let page_start = page.index * TMPFS_PAGE_SIZE_U64;
            let copy_start = offset.max(page_start);
            let copy_end = end.min(page_start + TMPFS_PAGE_SIZE_U64);
            let src_start = (copy_start - page_start) as usize;
            let dst_start = (copy_start - offset) as usize;
            let len = (copy_end - copy_start) as usize;
            if filled < dst_start {
                out[filled..dst_start].fill(0);
            }
            out[dst_start..dst_start + len]
                .copy_from_slice(&page.data.data()[src_start..src_start + len]);
            filled = dst_start + len;
        }
        if filled < n {
            out[filled..].fill(0);
        }
        n
    }

    fn seek_data(&self, offset: u64) -> VfsResult<u64> {
        if offset >= self.size {
            return Err(VfsError::NoSuchDeviceOrAddress);
        }
        let page_index = offset / TMPFS_PAGE_SIZE_U64;
        let pos = self.lower_bound(page_index);
        let Some(page) = self.pages.get(pos) else {
            return Err(VfsError::NoSuchDeviceOrAddress);
        };
        if page.index == page_index {
            return Ok(offset);
        }
        let candidate = page.index * TMPFS_PAGE_SIZE_U64;
        if candidate < self.size {
            Ok(candidate)
        } else {
            Err(VfsError::NoSuchDeviceOrAddress)
        }
    }

    fn seek_hole(&self, offset: u64) -> VfsResult<u64> {
        if offset >= self.size {
            return Err(VfsError::NoSuchDeviceOrAddress);
        }
        let page_index = offset / TMPFS_PAGE_SIZE_U64;
        let pos = self.lower_bound(page_index);
        if self
            .pages
            .get(pos)
            .is_none_or(|page| page.index != page_index)
        {
            return Ok(offset);
        }

        let mut expected = page_index + 1;
        for page in &self.pages[pos + 1..] {
            if page.index != expected {
                break;
            }
            expected += 1;
        }
        Ok((expected * TMPFS_PAGE_SIZE_U64).min(self.size))
    }

    fn write_at(
        &mut self,
        buf: &[u8],
        offset: u64,
        accounting: &TmpfsSuperblockOps,
    ) -> VfsResult<usize> {
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
            let page = match self.get_or_create_page(page_index, accounting) {
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

    fn reserve(
        &mut self,
        offset: u64,
        len: u64,
        accounting: &TmpfsSuperblockOps,
    ) -> VfsResult<u64> {
        let end = offset.checked_add(len).ok_or(VfsError::FileTooLarge)?;
        if end > usize::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        }
        let first_page = offset / TMPFS_PAGE_SIZE_U64;
        let end_page = end.div_ceil(TMPFS_PAGE_SIZE_U64);
        let first_pos = self.lower_bound(first_page);
        let end_pos = self.lower_bound(end_page);
        let requested = end_page.saturating_sub(first_page);
        let existing = (end_pos - first_pos) as u64;
        let missing = requested.saturating_sub(existing);
        if missing == 0 {
            return Ok(0);
        }
        let missing_usize: usize = missing.try_into().map_err(|_| VfsError::NoSpace)?;

        accounting.reserve_blocks(missing)?;
        if self.pages.try_reserve(missing_usize).is_err() {
            accounting.release_blocks(missing);
            return Err(VfsError::OutOfMemory);
        }

        let mut pending = Vec::new();
        if pending.try_reserve_exact(missing_usize).is_err() {
            accounting.release_blocks(missing);
            return Err(VfsError::OutOfMemory);
        }
        for index in first_page..end_page {
            if self.page_pos(index).is_ok() {
                continue;
            }
            let data = match self.alloc_page_slot(accounting) {
                Ok(data) => data,
                Err(error) => {
                    accounting.release_blocks(missing);
                    return Err(error);
                }
            };
            pending.push(TmpfsPage { index, data });
        }
        debug_assert_eq!(pending.len(), missing_usize);
        for page in pending {
            let pos = self.lower_bound(page.index);
            self.pages.insert(pos, page);
        }
        Ok(missing)
    }

    fn punch_hole(&mut self, offset: u64, len: u64) -> VfsResult<u64> {
        let end = offset.checked_add(len).ok_or(VfsError::FileTooLarge)?;
        if end > usize::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        }
        let old_pages = self.pages.len();
        self.pages.retain_mut(|page| {
            let page_start = page.index * TMPFS_PAGE_SIZE_U64;
            let page_end = page_start + TMPFS_PAGE_SIZE_U64;
            let zero_start = offset.max(page_start);
            let zero_end = end.min(page_end);
            if zero_start >= zero_end {
                return true;
            }
            if zero_start == page_start && zero_end == page_end {
                return false;
            }
            let start = (zero_start - page_start) as usize;
            let end = (zero_end - page_start) as usize;
            page.data.data_mut()[start..end].fill(0);
            true
        });
        Ok((old_pages - self.pages.len()) as u64)
    }

    fn page_pos(&self, index: u64) -> Result<usize, usize> {
        self.pages.binary_search_by_key(&index, |page| page.index)
    }

    fn lower_bound(&self, index: u64) -> usize {
        match self.page_pos(index) {
            Ok(pos) | Err(pos) => pos,
        }
    }

    fn get_or_create_page(
        &mut self,
        index: u64,
        accounting: &TmpfsSuperblockOps,
    ) -> VfsResult<&mut [u8]> {
        if let Some(last_index) = self.pages.last().map(|page| page.index) {
            if last_index == index {
                return Ok(self
                    .pages
                    .last_mut()
                    .expect("tmpfs last page disappeared")
                    .data
                    .data_mut());
            }
            if last_index.checked_add(1) == Some(index) {
                self.pages
                    .try_reserve(1)
                    .map_err(|_| VfsError::OutOfMemory)?;
                accounting.reserve_blocks(1)?;
                let data = match self.alloc_page_slot(accounting) {
                    Ok(data) => data,
                    Err(error) => {
                        accounting.release_blocks(1);
                        return Err(error);
                    }
                };
                self.pages.push(TmpfsPage { index, data });
                return Ok(self
                    .pages
                    .last_mut()
                    .expect("tmpfs appended page disappeared")
                    .data
                    .data_mut());
            }
        }

        match self.page_pos(index) {
            Ok(pos) => Ok(self.pages[pos].data.data_mut()),
            Err(pos) => {
                self.pages
                    .try_reserve(1)
                    .map_err(|_| VfsError::OutOfMemory)?;
                accounting.reserve_blocks(1)?;
                let data = match self.alloc_page_slot(accounting) {
                    Ok(data) => data,
                    Err(error) => {
                        accounting.release_blocks(1);
                        return Err(error);
                    }
                };
                self.pages.insert(pos, TmpfsPage { index, data });
                Ok(self.pages[pos].data.data_mut())
            }
        }
    }
}

fn next_tmpfs_batch_size(current: usize) -> usize {
    match current {
        0 | 1 => 4,
        _ => TMPFS_MAX_BATCH_PAGES,
    }
}

fn tmpfs_blocks_for_len(len: u64) -> u64 {
    // stat.st_blocks 的单位固定为 512 字节，不等同于 tmpfs 的页大小。
    len.div_ceil(TMPFS_PAGE_SIZE_U64) * (TMPFS_PAGE_SIZE_U64 / 512)
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

        let ino = sb_ops.alloc_ino()?;
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

        let ino = sb_ops.alloc_ino()?;
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

        let ino = sb_ops.alloc_ino()?;
        let target_pages = (target.len() as u64).div_ceil(TMPFS_PAGE_SIZE_U64);
        if let Err(error) = sb_ops.reserve_blocks(target_pages) {
            sb_ops.release_inode();
            return Err(error);
        }
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

        let ino = sb_ops.alloc_ino()?;
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

        let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
        let sb_ops = sb
            .ops
            .as_any()
            .downcast_ref::<TmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;

        let mut data = self.data.lock();
        let file_data = match &mut *data {
            TmpfsInodeData::File(data) => data,
            _ => return Err(VfsError::InvalidArgument),
        };

        let released = file_data.truncate(new_size);
        sb_ops.release_blocks(released);
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
            release_batch: options.writable(),
        }))
    }

    fn evict(&self, inode: &Inode) {
        let Some(sb) = inode.superblock() else {
            return;
        };
        let Some(sb_ops) = sb.ops.as_any().downcast_ref::<TmpfsSuperblockOps>() else {
            return;
        };
        let released = {
            let mut data = self.data.lock();
            match &mut *data {
                TmpfsInodeData::File(file) => {
                    let pages = file.pages.len() as u64;
                    file.pages.clear();
                    pages
                }
                TmpfsInodeData::Symlink(target) => {
                    (target.len() as u64).div_ceil(TMPFS_PAGE_SIZE_U64)
                }
                _ => 0,
            }
        };
        sb_ops.release_blocks(released);
        sb_ops.release_inode();
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
    release_batch: bool,
}

unsafe impl Send for TmpfsFileOps {}
unsafe impl Sync for TmpfsFileOps {}

impl FileOps for TmpfsFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        // Safety: `inode_ops` 指向由 `inode` 持有的同一个 InodeOps，`inode` 字段
        // 保证该对象在整个 FileOps 生命周期内保持存活。
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
        // Safety: `inode_ops` 的生命周期由同一 inode 的 Arc 保证，见 `read_at`。
        let ops = unsafe { &*self.inode_ops };
        let sb_ops = self
            .sb
            .ops
            .as_any()
            .downcast_ref::<TmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
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

        let n = file_data.write_at(buf, start, sb_ops)?;
        if n != 0 {
            self.inode
                .set_size_blocks_and_modified(file_data.size, file_data.blocks());
        } else {
            // 保留空写入只同步 size/blocks、不更新时间戳的既有行为。
            self.inode
                .set_size_and_blocks(file_data.size, file_data.blocks());
        }
        Ok(n)
    }

    fn seek_data(&self, offset: u64, _file_size: u64) -> VfsResult<u64> {
        // Safety: `inode_ops` 的生命周期由 `inode` 字段中的强引用保证。
        let ops = unsafe { &*self.inode_ops };
        let data = ops.data.lock();
        match &*data {
            TmpfsInodeData::File(file) => file.seek_data(offset),
            _ => Err(VfsError::InvalidArgument),
        }
    }

    fn seek_hole(&self, offset: u64, _file_size: u64) -> VfsResult<u64> {
        // Safety: `inode_ops` 的生命周期由 `inode` 字段中的强引用保证。
        let ops = unsafe { &*self.inode_ops };
        let data = ops.data.lock();
        match &*data {
            TmpfsInodeData::File(file) => file.seek_hole(offset),
            _ => Err(VfsError::InvalidArgument),
        }
    }

    fn fallocate(&self, mode: FallocateMode, offset: u64, len: u64) -> VfsResult<()> {
        let end = offset.checked_add(len).ok_or(VfsError::FileTooLarge)?;
        if end > usize::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        }

        // Safety: `inode_ops` 的生命周期由 `inode` 字段中的强引用保证。
        let ops = unsafe { &*self.inode_ops };
        let sb_ops = self
            .sb
            .ops
            .as_any()
            .downcast_ref::<TmpfsSuperblockOps>()
            .ok_or(VfsError::InvalidArgument)?;
        let mut data = ops.data.lock();
        let file_data = match &mut *data {
            TmpfsInodeData::File(data) => data,
            _ => return Err(VfsError::InvalidArgument),
        };

        match mode.bits() {
            0 => {
                let old_size = file_data.size;
                file_data.reserve(offset, len, sb_ops)?;
                if end > old_size {
                    file_data.size = end;
                }
                self.inode
                    .set_size_and_blocks(file_data.size, file_data.blocks());
                self.inode.touch_mtime();
                self.inode.touch_ctime();
            }
            bits if bits == FallocateMode::KEEP_SIZE.bits() => {
                file_data.reserve(offset, len, sb_ops)?;
                self.inode
                    .set_size_and_blocks(file_data.size, file_data.blocks());
                self.inode.touch_ctime();
            }
            bits if bits
                == FallocateMode::PUNCH_HOLE
                    .with(FallocateMode::KEEP_SIZE)
                    .bits() =>
            {
                let released = file_data.punch_hole(offset, len)?;
                sb_ops.release_blocks(released);
                self.inode
                    .set_size_and_blocks(file_data.size, file_data.blocks());
                self.inode.touch_mtime();
                self.inode.touch_ctime();
            }
            _ => return Err(VfsError::NotSupported),
        }
        Ok(())
    }

    fn readdir(
        &self,
        pos: u64,
        sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        // Safety: `inode_ops` 的生命周期由 `inode` 字段中的强引用保证。
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
        if !self.release_batch {
            return;
        }
        // 文件关闭后立即归还尚未消费的批量页槽，避免 dentry/inode 长期驻留时让
        // 小文件各自占住一批物理页。并发打开只会让下一位写者重新补批，不影响数据。
        // Safety: `inode_ops` 的生命周期由 `inode` 字段中的强引用保证。
        let ops = unsafe { &*self.inode_ops };
        let mut data = ops.data.lock();
        if let TmpfsInodeData::File(file) = &mut *data {
            file.release_unused_slots();
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{
        TMPFS_MAX_BATCH_PAGES, TMPFS_SLAB_FREE, next_tmpfs_batch_size, release_tmpfs_slot,
        take_tmpfs_slots,
    };

    #[test]
    fn tmpfs_batch_size_grows_without_penalizing_one_page_files() {
        assert_eq!(next_tmpfs_batch_size(1), 4);
        assert_eq!(next_tmpfs_batch_size(4), TMPFS_MAX_BATCH_PAGES);
        assert_eq!(
            next_tmpfs_batch_size(TMPFS_MAX_BATCH_PAGES),
            TMPFS_MAX_BATCH_PAGES
        );
    }

    #[test]
    fn tmpfs_page_slab_releases_all_slots() {
        let mut free = TMPFS_SLAB_FREE;
        let first = take_tmpfs_slots(&mut free, TMPFS_MAX_BATCH_PAGES);
        assert_eq!(first.len, TMPFS_MAX_BATCH_PAGES);
        assert_eq!(&first.slots[..first.len], &[0, 1, 2, 3, 4, 5, 6, 7]);
        let second = take_tmpfs_slots(&mut free, TMPFS_MAX_BATCH_PAGES);
        assert_eq!(second.len, TMPFS_MAX_BATCH_PAGES);
        assert_eq!(&second.slots[..second.len], &[8, 9, 10, 11, 12, 13, 14, 15]);
        for slot in first.slots[..first.len]
            .iter()
            .chain(second.slots[..second.len].iter())
            .copied()
        {
            release_tmpfs_slot(&mut free, slot);
        }
        assert_eq!(free, TMPFS_SLAB_FREE);
    }
}

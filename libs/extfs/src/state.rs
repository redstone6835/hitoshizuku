//! 驱动对外暴露的类型 + 共享状态。
//!
//! [`BlockBackend`] 是 ext 驱动对块设备的同步 I/O 契约。[`ExtFsDriver`]
//! 实现 [`vfs::superblock::FsDriver`],挂载时产生一个持有 [`FsState`]
//! 的 [`Superblock`](vfs::superblock::Superblock)。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vfs::cred::{Gid, Uid};
use vfs::dentry::Dentry;
use vfs::error::{VfsError, VfsResult};
use vfs::inode::{Inode, InodeId, InodeMeta};
use vfs::mount::MountFlags;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat};
use vfs::superblock::{
    FsDriver, FsDriverFlags, InodeCache, Superblock as VfsSuperblock, SuperblockOps,
};
use vfs::sync::Spinlock;

use crate::bgd::{self, GroupDesc};
use crate::crc;
use crate::inode::{ExtInodeOps, load_inode};
use crate::inode_wr::RawInode;
use crate::layout::{EXT4_ROOT_INO, ExtKind};
use crate::sb::{self, Superblock as ExtSb};

const BLOCK_CACHE_CAP: usize = 8192;

struct BlockCacheSlot {
    block: u64,
    data: Arc<Vec<u8>>,
    referenced: bool,
    occupied: bool,
    dirty: bool,
    version: u64,
}

#[derive(Clone)]
struct DirtyBlockSnapshot {
    block: u64,
    data: Arc<Vec<u8>>,
    version: u64,
}

enum PartialWriteOutcome {
    Wait,
    Retry,
    Done(Option<DirtyBlockSnapshot>),
}

struct PendingBlockWriteback {
    data: Arc<Vec<u8>>,
    version: u64,
    /// `true` 表示恰有一个调用方负责把当前块写回后端。
    ///
    /// 同一物理块的新版本只替换 `data/version`，不能再启动第二个并行 I/O；
    /// 原 owner 完成旧版本后会继续 drain 最新版本，避免旧写后到覆盖新写。
    in_flight: bool,
}

/// O(log n) 块缓存：BTreeMap 索引 + Clock eviction。
pub(crate) struct BlockCache {
    slots: Vec<BlockCacheSlot>,
    /// block_no → slot 索引。
    index: BTreeMap<u64, usize>,
    /// Clock eviction 指针（循环扫描）。
    hand: usize,
    capacity: usize,
    block_size: usize,
    write_seq: u64,
    /// 已落盘快照从 active/pending 消失，或 cache 失效时推进的序号。
    ///
    /// 普通 dirty 发布不推进：它仍可由 overlay 覆盖后端旧读。
    coherence_epoch: u64,
    coherence_seq: u64,
    /// 每个物理块最近一次“可见快照消失/失效”的序号。
    coherence_stamps: BTreeMap<u64, u64>,
    /// 被驱逐的脏块在锁外写回期间仍须可读，不能让并发读回退到旧磁盘内容。
    ///
    /// 同一块的新驱逐会覆盖可见 pending 版本；旧写回完成时必须比较版本，不能误删新值。
    pending_writebacks: BTreeMap<u64, PendingBlockWriteback>,
    /// 正在锁外执行的 range direct write，记录每块所属的写入版本。
    ///
    /// 直写期间的新 dirty 版本可以进入 cache/pending，但不得先于直写落盘。
    active_direct_writes: BTreeMap<u64, u64>,
}

impl BlockCache {
    fn new(block_size: u32) -> Self {
        Self::with_capacity(block_size, BLOCK_CACHE_CAP)
    }

    fn with_capacity(block_size: u32, capacity: usize) -> Self {
        let bs = block_size as usize;
        let capacity = capacity.max(1);
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(BlockCacheSlot {
                block: 0,
                data: Arc::new(vec![0u8; bs]),
                referenced: false,
                occupied: false,
                dirty: false,
                version: 0,
            });
        }
        Self {
            slots,
            index: BTreeMap::new(),
            hand: 0,
            capacity,
            block_size: bs,
            write_seq: 0,
            coherence_epoch: 0,
            coherence_seq: 0,
            coherence_stamps: BTreeMap::new(),
            pending_writebacks: BTreeMap::new(),
            active_direct_writes: BTreeMap::new(),
        }
    }

    fn mark_coherence_change(&mut self, block: u64) {
        self.coherence_seq = self.coherence_seq.wrapping_add(1);
        if self.coherence_seq == 0 {
            // 0 作为“该块从未记录”的 stamp；回绕时清空旧时代并从 1 重新开始。
            // epoch 使读者仍能识别因清表恢复为 0 的目标 stamp；再次 ABA 需要 2^128 次事件。
            self.coherence_stamps.clear();
            self.coherence_epoch = self.coherence_epoch.wrapping_add(1);
            self.coherence_seq = 1;
        }
        self.coherence_stamps.insert(block, self.coherence_seq);
    }

    fn coherence_stamp_in_range(&self, start: u64, count: u32) -> (u64, u64) {
        if count == 0 {
            return (self.coherence_epoch, 0);
        }
        let end = start.saturating_add(count as u64);
        let max_seq = self
            .coherence_stamps
            .range(start..end)
            .map(|(_, &stamp)| stamp)
            .max()
            .unwrap_or(0);
        (self.coherence_epoch, max_seq)
    }

    fn next_version(&mut self) -> u64 {
        self.write_seq = self.write_seq.wrapping_add(1);
        if self.write_seq == 0 {
            self.write_seq = 1;
        }
        self.write_seq
    }

    fn read(&mut self, block: u64, out: &mut [u8]) -> bool {
        if out.len() != self.block_size {
            return false;
        }
        if let Some(&idx) = self.index.get(&block) {
            let slot = &mut self.slots[idx];
            slot.referenced = true;
            out.copy_from_slice(slot.data.as_slice());
            return true;
        }
        if let Some(pending) = self.pending_writebacks.get(&block) {
            out.copy_from_slice(pending.data.as_slice());
            return true;
        }
        false
    }

    /// 在 cache 内原地修改指定块的部分字节，同时将该块标记为 dirty。
    /// 如果该块不在 cache 中则返回 false，调用方需要回退到 read + modify + write 路径。
    fn modify_inplace(&mut self, block: u64, offset: usize, src: &[u8]) -> bool {
        if offset + src.len() > self.block_size {
            return false;
        }
        if let Some(&idx) = self.index.get(&block) {
            let version = self.next_version();
            let slot = &mut self.slots[idx];
            Arc::make_mut(&mut slot.data)[offset..offset + src.len()].copy_from_slice(src);
            slot.referenced = true;
            slot.dirty = true;
            slot.version = version;
            return true;
        }
        false
    }

    /// cache miss 的整块读完成后，在同一把 cache 锁下重新合并部分写。
    ///
    /// 不同 inode 可能位于同一 inode-table 块。若在后端读与整块插入之间
    /// 另一 owner 已更新该块，必须在锁内叠加到最新 cache/pending 上，
    /// 不能用旧的后端快照覆盖它。
    fn merge_partial_after_read(
        &mut self,
        block: u64,
        offset: usize,
        src: &[u8],
        base: &mut [u8],
        read_stamp: (u64, u64),
    ) -> PartialWriteOutcome {
        if self.modify_inplace(block, offset, src) {
            return PartialWriteOutcome::Done(None);
        }
        if let Some(pending) = self.pending_writebacks.get(&block) {
            base.copy_from_slice(pending.data.as_slice());
        } else if self.has_active_direct(block) {
            return PartialWriteOutcome::Wait;
        } else if self.coherence_stamp_in_range(block, 1) != read_stamp {
            // backend read 返回后，更新版本可能已写盘并从
            // active/pending 消失。此时 base 仍是旧版，必须重读。
            return PartialWriteOutcome::Retry;
        }
        base[offset..offset + src.len()].copy_from_slice(src);
        PartialWriteOutcome::Done(self.insert_wb(block, base))
    }

    /// 在 cache 内原地读取指定块的部分字节到输出缓冲区。
    /// 如果该块不在 cache 中则返回 false。
    pub(crate) fn read_partial(&mut self, block: u64, offset: usize, dst: &mut [u8]) -> bool {
        if offset + dst.len() > self.block_size {
            return false;
        }
        if let Some(&idx) = self.index.get(&block) {
            let slot = &mut self.slots[idx];
            slot.referenced = true;
            dst.copy_from_slice(&slot.data[offset..offset + dst.len()]);
            return true;
        }
        if let Some(pending) = self.pending_writebacks.get(&block) {
            dst.copy_from_slice(&pending.data[offset..offset + dst.len()]);
            return true;
        }
        false
    }

    fn contains(&self, block: u64) -> bool {
        self.index.contains_key(&block) || self.pending_writebacks.contains_key(&block)
    }

    fn invalidate(&mut self, block: u64) {
        self.mark_coherence_change(block);
        if let Some(&idx) = self.index.get(&block) {
            self.slots[idx].occupied = false;
            self.slots[idx].dirty = false;
            self.index.remove(&block);
        }
    }

    fn read_range(&mut self, start: u64, count: u32, out: &mut [u8]) -> bool {
        if out.len() != self.block_size * count as usize {
            return false;
        }
        for i in 0..count {
            if !self.contains(start + i as u64) {
                return false;
            }
        }
        for i in 0..count {
            let block = start + i as u64;
            let off = i as usize * self.block_size;
            if let Some(&idx) = self.index.get(&block) {
                let slot = &mut self.slots[idx];
                slot.referenced = true;
                out[off..off + self.block_size].copy_from_slice(slot.data.as_slice());
            } else if let Some(pending) = self.pending_writebacks.get(&block) {
                out[off..off + self.block_size].copy_from_slice(pending.data.as_slice());
            }
        }
        true
    }

    fn read_cached_prefix(&mut self, start: u64, count: u32, out: &mut [u8]) -> u32 {
        let mut copied = 0u32;
        while copied < count {
            let offset = copied as usize * self.block_size;
            if !self.read(
                start + copied as u64,
                &mut out[offset..offset + self.block_size],
            ) {
                break;
            }
            copied += 1;
        }
        copied
    }

    fn uncached_prefix_len(&self, start: u64, count: u32) -> u32 {
        let mut missing = 0u32;
        while missing < count {
            let block = start + missing as u64;
            if self.contains(block) || self.has_active_direct(block) {
                break;
            }
            missing += 1;
        }
        missing
    }

    fn read_range_vectored(&mut self, start: u64, count: u32, out: &mut [&mut [u8]]) -> bool {
        if vectored_block_count(out, self.block_size) != Some(count) {
            return false;
        }
        for i in 0..count {
            if !self.contains(start + i as u64) {
                return false;
            }
        }

        let mut block = start;
        for buf in out.iter_mut() {
            for block_out in buf.chunks_exact_mut(self.block_size) {
                if !self.read(block, block_out) {
                    return false;
                }
                block += 1;
            }
        }
        true
    }

    fn overlay_range(&mut self, start: u64, count: u32, out: &mut [u8]) {
        if out.len() != self.block_size * count as usize {
            return;
        }
        for i in 0..count {
            let block = start + i as u64;
            if let Some(&idx) = self.index.get(&block) {
                let slot = &mut self.slots[idx];
                slot.referenced = true;
                let off = i as usize * self.block_size;
                out[off..off + self.block_size].copy_from_slice(slot.data.as_slice());
            } else if let Some(pending) = self.pending_writebacks.get(&block) {
                let off = i as usize * self.block_size;
                out[off..off + self.block_size].copy_from_slice(pending.data.as_slice());
            }
        }
    }

    fn overlay_range_vectored(&mut self, start: u64, count: u32, out: &mut [&mut [u8]]) {
        if vectored_block_count(out, self.block_size) != Some(count) {
            return;
        }

        let mut block = start;
        for buf in out.iter_mut() {
            for block_out in buf.chunks_exact_mut(self.block_size) {
                if self.contains(block) {
                    let copied = self.read(block, block_out);
                    debug_assert!(copied);
                }
                block += 1;
            }
        }
    }

    fn has_active_direct(&self, block: u64) -> bool {
        self.active_direct_writes.contains_key(&block)
    }

    fn has_active_direct_writes(&self) -> bool {
        !self.active_direct_writes.is_empty()
    }

    fn has_active_direct_in_range(&self, start: u64, count: u32) -> bool {
        if count == 0 {
            return false;
        }
        let end = start.saturating_add(count as u64);
        self.active_direct_writes.range(start..end).next().is_some()
    }

    /// 范围内若有直写块尚无 cache/pending 快照，读者必须等直写完成。
    fn has_unreadable_direct_in_range(&self, start: u64, count: u32) -> bool {
        if count == 0 {
            return false;
        }
        let end = start.saturating_add(count as u64);
        self.active_direct_writes
            .range(start..end)
            .any(|(&block, _)| !self.contains(block))
    }

    /// 在同一 cache 锁内为 range direct write 占位，并以直写数据取代旧 dirty 版本。
    ///
    /// 返回 `None` 表示范围内仍有 pending 或另一个 active direct owner。
    fn begin_direct_write(&mut self, start: u64, count: u32, data: &[u8]) -> Option<u64> {
        if count == 0 || data.len() != self.block_size * count as usize {
            return None;
        }
        if self.has_pending_in_range(start, count) || self.has_active_direct_in_range(start, count)
        {
            return None;
        }

        let version = self.next_version();
        for i in 0..count {
            let block = start + i as u64;
            self.active_direct_writes.insert(block, version);
            if let Some(&idx) = self.index.get(&block) {
                let off = i as usize * self.block_size;
                let slot = &mut self.slots[idx];
                Arc::make_mut(&mut slot.data).copy_from_slice(&data[off..off + self.block_size]);
                slot.referenced = true;
                // I/O 成功前保持 dirty；失败时即可由 sync 重试。
                slot.dirty = true;
                slot.version = version;
            }
        }
        Some(version)
    }

    /// 结束 range direct write。只清理本 owner 的版本，不能把并发新写标 clean。
    fn finish_direct_write(
        &mut self,
        start: u64,
        count: u32,
        version: u64,
        data: &[u8],
        succeeded: bool,
    ) {
        for i in 0..count {
            let block = start + i as u64;
            if self.active_direct_writes.get(&block).copied() != Some(version) {
                continue;
            }
            self.active_direct_writes.remove(&block);
            self.mark_coherence_change(block);

            if succeeded {
                self.mark_clean(block, version);
                let remove_pending = self
                    .pending_writebacks
                    .get(&block)
                    .is_some_and(|pending| pending.version == version);
                if remove_pending {
                    self.pending_writebacks.remove(&block);
                }
                continue;
            }

            let slot_version = self.index.get(&block).map(|&idx| self.slots[idx].version);
            let pending_version = self
                .pending_writebacks
                .get(&block)
                .map(|pending| pending.version);
            if slot_version.is_some_and(|current| current != version)
                || pending_version.is_some_and(|current| current != version)
            {
                continue;
            }
            if slot_version == Some(version) {
                // begin_direct_write 已把直写数据留在 dirty slot 内。
                continue;
            }
            if let Some(pending) = self.pending_writebacks.get_mut(&block) {
                if pending.version == version {
                    pending.in_flight = false;
                    continue;
                }
            }
            let off = i as usize * self.block_size;
            self.pending_writebacks.insert(
                block,
                PendingBlockWriteback {
                    data: Arc::new(data[off..off + self.block_size].to_vec()),
                    version,
                    in_flight: false,
                },
            );
        }
    }

    /// 发布一个刚被驱逐的 dirty 快照，并在该块没有 owner 时取得写回所有权。
    fn remember_pending(&mut self, snapshot: DirtyBlockSnapshot) -> Option<DirtyBlockSnapshot> {
        use alloc::collections::btree_map::Entry;

        let direct_active = self.has_active_direct(snapshot.block);
        match self.pending_writebacks.entry(snapshot.block) {
            Entry::Vacant(entry) => {
                entry.insert(PendingBlockWriteback {
                    data: Arc::clone(&snapshot.data),
                    version: snapshot.version,
                    in_flight: !direct_active,
                });
                (!direct_active).then_some(snapshot)
            }
            Entry::Occupied(mut entry) => {
                let pending = entry.get_mut();
                // snapshot 在同一 cache 锁内刚产生，时序上必然新于 pending；
                // 版本号回绕后不能再用数值大小判断新旧。
                pending.data = snapshot.data;
                pending.version = snapshot.version;
                if pending.in_flight || direct_active {
                    None
                } else {
                    pending.in_flight = true;
                    Some(DirtyBlockSnapshot {
                        block: snapshot.block,
                        data: Arc::clone(&pending.data),
                        version: pending.version,
                    })
                }
            }
        }
    }

    /// 当前 owner 成功写完一个版本。若期间出现更新，继续返回最新版本给同一 owner。
    fn complete_pending(&mut self, block: u64, version: u64) -> Option<DirtyBlockSnapshot> {
        let Some(pending) = self.pending_writebacks.get(&block) else {
            return None;
        };
        if pending.version == version {
            self.pending_writebacks.remove(&block);
            self.mark_coherence_change(block);
            return None;
        }
        Some(DirtyBlockSnapshot {
            block,
            data: Arc::clone(&pending.data),
            version: pending.version,
        })
    }

    /// 后端写失败时释放 owner，但保留最新数据，供后续写入或 sync 重试。
    fn fail_pending(&mut self, block: u64) {
        if let Some(pending) = self.pending_writebacks.get_mut(&block) {
            pending.in_flight = false;
        }
    }

    /// 取得一个当前无人负责的 pending 块。
    fn claim_pending(&mut self) -> Option<DirtyBlockSnapshot> {
        let block = self
            .pending_writebacks
            .iter()
            .find_map(|(&block, pending)| {
                (!pending.in_flight && !self.has_active_direct(block)).then_some(block)
            })?;
        let pending = self.pending_writebacks.get_mut(&block)?;
        pending.in_flight = true;
        Some(DirtyBlockSnapshot {
            block,
            data: Arc::clone(&pending.data),
            version: pending.version,
        })
    }

    fn claim_pending_in_range(&mut self, start: u64, count: u32) -> Option<DirtyBlockSnapshot> {
        let end = start.saturating_add(count as u64);
        let block = self
            .pending_writebacks
            .range(start..end)
            .find_map(|(&block, pending)| {
                (!pending.in_flight && !self.has_active_direct(block)).then_some(block)
            })?;
        let pending = self.pending_writebacks.get_mut(&block)?;
        pending.in_flight = true;
        Some(DirtyBlockSnapshot {
            block,
            data: Arc::clone(&pending.data),
            version: pending.version,
        })
    }

    fn has_pending(&self) -> bool {
        !self.pending_writebacks.is_empty()
    }

    fn has_pending_in_range(&self, start: u64, count: u32) -> bool {
        if count == 0 {
            return false;
        }
        let end = start.saturating_add(count as u64);
        self.pending_writebacks.range(start..end).next().is_some()
    }

    /// Write-back 插入：标记 dirty，驱逐时不做 I/O，并登记被驱逐脏块的可读快照。
    fn insert_wb(&mut self, block: u64, data: &[u8]) -> Option<DirtyBlockSnapshot> {
        if data.len() != self.block_size {
            return None;
        }
        let version = self.next_version();
        // 命中：原地更新
        if let Some(&idx) = self.index.get(&block) {
            let slot = &mut self.slots[idx];
            Arc::make_mut(&mut slot.data).copy_from_slice(data);
            slot.referenced = true;
            slot.dirty = true;
            slot.version = version;
            return None;
        }
        // 未满：直接 push
        if self.slots.len() < self.capacity {
            let idx = self.slots.len();
            self.slots.push(BlockCacheSlot {
                block,
                data: Arc::new(Vec::from(data)),
                referenced: true,
                occupied: true,
                dirty: true,
                version,
            });
            self.index.insert(block, idx);
            return None;
        }
        // 已满：Clock eviction，优先驱逐 clean 块
        let cap = self.slots.len();
        let mut evicted: Option<DirtyBlockSnapshot> = None;
        let mut steps = 0usize;
        loop {
            let i = self.hand;
            self.hand = (self.hand + 1) % cap;
            let slot = &mut self.slots[i];
            if !slot.occupied {
                slot.block = block;
                Arc::make_mut(&mut slot.data).copy_from_slice(data);
                slot.referenced = true;
                slot.occupied = true;
                slot.dirty = true;
                slot.version = version;
                self.index.insert(block, i);
                return evicted;
            }
            if slot.referenced {
                slot.referenced = false;
                steps += 1;
                if steps > cap * 2 {
                    let old_block = slot.block;
                    if slot.dirty {
                        evicted = Some(DirtyBlockSnapshot {
                            block: old_block,
                            data: Arc::clone(&slot.data),
                            version: slot.version,
                        });
                    }
                    self.index.remove(&old_block);
                    slot.block = block;
                    Arc::make_mut(&mut slot.data).copy_from_slice(data);
                    slot.referenced = true;
                    slot.dirty = true;
                    slot.version = version;
                    self.index.insert(block, i);
                    return evicted.and_then(|snapshot| self.remember_pending(snapshot));
                }
                continue;
            }
            // 可驱逐
            let old_block = slot.block;
            if slot.dirty {
                evicted = Some(DirtyBlockSnapshot {
                    block: old_block,
                    data: Arc::clone(&slot.data),
                    version: slot.version,
                });
            }
            self.index.remove(&old_block);
            slot.block = block;
            Arc::make_mut(&mut slot.data).copy_from_slice(data);
            slot.referenced = true;
            slot.dirty = true;
            slot.version = version;
            self.index.insert(block, i);
            return evicted.and_then(|snapshot| self.remember_pending(snapshot));
        }
    }

    fn insert_clean<F>(
        &mut self,
        block: u64,
        data: &[u8],
        flush: F,
    ) -> Result<(), BlockBackendError>
    where
        F: FnMut(u64, &[u8]) -> Result<(), BlockBackendError>,
    {
        if self.index.contains_key(&block) {
            return Ok(());
        }
        self.insert(block, data, false, flush)
    }

    fn insert<F>(
        &mut self,
        block: u64,
        data: &[u8],
        dirty: bool,
        mut flush: F,
    ) -> Result<(), BlockBackendError>
    where
        F: FnMut(u64, &[u8]) -> Result<(), BlockBackendError>,
    {
        if data.len() != self.block_size {
            return Err(BlockBackendError::OutOfRange);
        }
        let version = if dirty { self.next_version() } else { 0 };
        // 命中：原地更新
        if let Some(&idx) = self.index.get(&block) {
            let slot = &mut self.slots[idx];
            Arc::make_mut(&mut slot.data).copy_from_slice(data);
            slot.referenced = true;
            if dirty {
                slot.dirty = true;
                slot.version = version;
            }
            return Ok(());
        }
        // 未满：直接 push
        if self.slots.len() < self.capacity {
            let idx = self.slots.len();
            self.slots.push(BlockCacheSlot {
                block,
                data: Arc::new(Vec::from(data)),
                referenced: true,
                occupied: true,
                dirty,
                version,
            });
            self.index.insert(block, idx);
            return Ok(());
        }
        // 已满：Clock eviction
        let cap = self.slots.len();
        let mut steps = 0usize;
        loop {
            let i = self.hand;
            self.hand = (self.hand + 1) % cap;
            let slot = &mut self.slots[i];
            if !slot.occupied {
                slot.block = block;
                Arc::make_mut(&mut slot.data).copy_from_slice(data);
                slot.referenced = true;
                slot.occupied = true;
                slot.dirty = dirty;
                slot.version = version;
                self.index.insert(block, i);
                return Ok(());
            }
            if slot.referenced {
                slot.referenced = false;
                steps += 1;
                if steps > cap * 2 {
                    // 保险：兜底 LRU 化淘汰
                    let old_block = slot.block;
                    if slot.dirty {
                        flush(old_block, slot.data.as_slice())?;
                    }
                    self.index.remove(&old_block);
                    slot.block = block;
                    Arc::make_mut(&mut slot.data).copy_from_slice(data);
                    slot.referenced = true;
                    slot.dirty = dirty;
                    slot.version = version;
                    self.index.insert(block, i);
                    return Ok(());
                }
                continue;
            }
            // 命中淘汰
            let old_block = slot.block;
            if slot.dirty {
                flush(old_block, slot.data.as_slice())?;
            }
            self.index.remove(&old_block);
            slot.block = block;
            Arc::make_mut(&mut slot.data).copy_from_slice(data);
            slot.referenced = true;
            slot.dirty = dirty;
            slot.version = version;
            self.index.insert(block, i);
            return Ok(());
        }
    }

    fn try_insert_clean(&mut self, block: u64, data: &[u8]) -> bool {
        self.try_insert(block, data, false)
    }

    fn try_insert_dirty(&mut self, block: u64, data: &[u8]) -> bool {
        self.try_insert(block, data, true)
    }

    fn try_insert(&mut self, block: u64, data: &[u8], dirty: bool) -> bool {
        if data.len() != self.block_size {
            return false;
        }
        if let Some(&idx) = self.index.get(&block) {
            let version = if dirty { self.next_version() } else { 0 };
            let slot = &mut self.slots[idx];
            Arc::make_mut(&mut slot.data).copy_from_slice(data);
            slot.referenced = true;
            if dirty {
                slot.dirty = true;
                slot.version = version;
            }
            return true;
        }
        if self.slots.len() < self.capacity {
            let idx = self.slots.len();
            let version = if dirty { self.next_version() } else { 0 };
            self.slots.push(BlockCacheSlot {
                block,
                data: Arc::new(Vec::from(data)),
                referenced: true,
                occupied: true,
                dirty,
                version,
            });
            self.index.insert(block, idx);
            return true;
        }
        let cap = self.slots.len();
        let version = if dirty { self.next_version() } else { 0 };
        let start = self.hand;
        let mut i = start;
        let mut steps = 0u32;
        loop {
            if steps >= 16 {
                return false;
            }
            let slot = &mut self.slots[i];
            if !slot.occupied || (!slot.dirty && !slot.referenced) {
                let old_block = slot.block;
                if slot.occupied {
                    self.index.remove(&old_block);
                }
                slot.block = block;
                Arc::make_mut(&mut slot.data).copy_from_slice(data);
                slot.referenced = true;
                slot.occupied = true;
                slot.dirty = dirty;
                slot.version = version;
                self.hand = (i + 1) % cap;
                self.index.insert(block, i);
                return true;
            }
            slot.referenced = false;
            i = (i + 1) % cap;
            steps += 1;
            if i == start {
                return false;
            }
        }
    }

    fn dirty_snapshots(&self) -> Vec<DirtyBlockSnapshot> {
        let mut dirty = Vec::new();
        for slot in &self.slots {
            if slot.occupied && slot.dirty {
                dirty.push(DirtyBlockSnapshot {
                    block: slot.block,
                    data: Arc::clone(&slot.data),
                    version: slot.version,
                });
            }
        }
        dirty
    }

    fn mark_clean(&mut self, block: u64, version: u64) {
        if let Some(&idx) = self.index.get(&block) {
            let slot = &mut self.slots[idx];
            if slot.occupied && slot.dirty && slot.version == version {
                slot.dirty = false;
            }
        }
    }

    #[allow(dead_code)]
    fn invalidate_range(&mut self, start: u64, count: u32) {
        if count == 0 {
            return;
        }
        let end = start.saturating_add(count as u64);
        // 即使范围当前未缓存，也要使并发 backend read 的旧结果失效。
        for block in start..end {
            self.mark_coherence_change(block);
        }
        // 收集需要移除的 block 号（避免在借用 self.index 时修改它）
        let to_remove: Vec<u64> = self.index.range(start..end).map(|(&b, _)| b).collect();
        for block in to_remove {
            if let Some(idx) = self.index.remove(&block) {
                let slot = &mut self.slots[idx];
                slot.occupied = false;
                slot.referenced = false;
                slot.dirty = false;
            }
        }
        let pending_to_remove: Vec<u64> = self
            .pending_writebacks
            .range(start..end)
            .map(|(&block, _)| block)
            .collect();
        for block in pending_to_remove {
            self.pending_writebacks.remove(&block);
        }
    }
}

fn vectored_block_count(bufs: &[&mut [u8]], block_size: usize) -> Option<u32> {
    if block_size == 0 {
        return None;
    }
    let mut count = 0usize;
    for buf in bufs {
        if buf.is_empty() || buf.len() % block_size != 0 {
            return None;
        }
        count = count.checked_add(buf.len() / block_size)?;
    }
    u32::try_from(count).ok()
}

/// 块设备同步 I/O 错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockBackendError {
    Io,
    OutOfRange,
    Unsupported,
}

/// 文件系统与块设备之间的同步契约。`read_sectors` / `write_sectors` 按扇区
/// 粒度工作:调用方保证 `buf.len() == sector_size * count`。只读驱动中
/// `write_sectors` 不会被 inode/file 调用,但仍保留在 trait 里以便未来扩展。
pub trait BlockBackend: Send + Sync {
    fn sector_size(&self) -> u32;
    fn sector_count(&self) -> u64;
    fn read_sectors(&self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), BlockBackendError>;

    /// 从连续扇区范围读取到多个缓冲区。
    ///
    /// 默认实现逐缓冲调用 [`Self::read_sectors`]；块设备可覆盖它以提交真正的
    /// scatter/gather 请求。每个 slice 必须非空并按扇区大小对齐。
    fn read_sectors_vectored(
        &self,
        lba: u64,
        bufs: &mut [&mut [u8]],
    ) -> Result<(), BlockBackendError> {
        let sector_size = self.sector_size() as usize;
        if sector_size == 0 {
            return Err(BlockBackendError::OutOfRange);
        }

        let mut total_sectors = 0u64;
        for buf in bufs.iter() {
            if buf.is_empty() || buf.len() % sector_size != 0 {
                return Err(BlockBackendError::OutOfRange);
            }
            let sectors = u64::try_from(buf.len() / sector_size)
                .map_err(|_| BlockBackendError::OutOfRange)?;
            if sectors > u32::MAX as u64 {
                return Err(BlockBackendError::OutOfRange);
            }
            total_sectors = total_sectors
                .checked_add(sectors)
                .ok_or(BlockBackendError::OutOfRange)?;
        }
        let end = lba
            .checked_add(total_sectors)
            .ok_or(BlockBackendError::OutOfRange)?;
        if end > self.sector_count() {
            return Err(BlockBackendError::OutOfRange);
        }

        let mut current_lba = lba;
        for buf in bufs.iter_mut() {
            let sectors = (buf.len() / sector_size) as u32;
            self.read_sectors(current_lba, sectors, buf)?;
            current_lba += sectors as u64;
        }
        Ok(())
    }

    fn write_sectors(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), BlockBackendError>;
}

static EXTFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// 单个块组的运行时计数(独立于描述符的 bitmap 布局,便于原子调整)。
#[derive(Debug, Clone, Copy)]
pub(crate) struct GroupCounts {
    pub free_blocks: u32,
    pub free_inodes: u32,
    pub used_dirs: u32,
}

struct PendingInodeWriteback {
    raw: RawInode,
    version: u64,
    in_flight: bool,
}

struct DirtyInodeState {
    seq: u64,
    pending: BTreeMap<u32, PendingInodeWriteback>,
}

enum InodeWritebackClaim {
    Clean,
    Busy,
    Owner { raw: RawInode, version: u64 },
}

impl DirtyInodeState {
    fn new() -> Self {
        Self {
            seq: 0,
            pending: BTreeMap::new(),
        }
    }

    fn next_version(&mut self) -> u64 {
        self.seq = self.seq.wrapping_add(1);
        if self.seq == 0 {
            self.seq = 1;
        }
        self.seq
    }

    fn stage(&mut self, raw: &RawInode) -> u64 {
        let version = self.next_version();
        let in_flight = self
            .pending
            .get(&raw.ino)
            .is_some_and(|pending| pending.in_flight);
        self.pending.insert(
            raw.ino,
            PendingInodeWriteback {
                raw: raw.clone(),
                version,
                in_flight,
            },
        );
        version
    }

    fn claim(&mut self, ino: u32) -> InodeWritebackClaim {
        let Some(pending) = self.pending.get_mut(&ino) else {
            return InodeWritebackClaim::Clean;
        };
        if pending.in_flight {
            return InodeWritebackClaim::Busy;
        }
        pending.in_flight = true;
        InodeWritebackClaim::Owner {
            raw: pending.raw.clone(),
            version: pending.version,
        }
    }
}

/// 已挂载 ext FS 的共享状态。
pub(crate) struct FsState {
    pub(crate) backend: Arc<dyn BlockBackend>,
    pub(crate) ext_sb: ExtSb,
    pub(crate) group_desc: Spinlock<alloc::vec::Vec<GroupDesc>>,
    pub(crate) group_counts: Spinlock<alloc::vec::Vec<GroupCounts>>,
    pub(crate) block_cache: Spinlock<BlockCache>,
    pub(crate) sb_free_blocks: core::sync::atomic::AtomicU64,
    pub(crate) sb_free_inodes: core::sync::atomic::AtomicU32,
    pub(crate) block_alloc_hint: core::sync::atomic::AtomicU64,
    pub(crate) inode_alloc_hint: core::sync::atomic::AtomicU32,
    pub(crate) alloc_group_dirty: Spinlock<alloc::vec::Vec<u8>>,
    pub(crate) alloc_sb_dirty: AtomicBool,
    inode_writeback: Spinlock<DirtyInodeState>,
    /// 只读挂载标志(由驱动 flags 或 remount 控制)。
    pub(crate) read_only: core::sync::atomic::AtomicBool,
    /// MMP 运行时心跳状态(未启用 MMP 的文件系统保持初始值,不参与 I/O)。
    pub(crate) mmp: MmpRuntime,
}

/// MMP 运行时心跳的可变状态(见 [`crate::mmp`])。
pub(crate) struct MmpRuntime {
    /// 当前心跳序列号(挂载夺占时置 1,每次心跳 +1)。
    pub(crate) seq: AtomicU32,
    /// 上次心跳的实时纳秒(用于按 `s_mmp_update_interval` 节流)。
    pub(crate) last_heartbeat_ns: AtomicU64,
}

impl MmpRuntime {
    fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            last_heartbeat_ns: AtomicU64::new(0),
        }
    }
}

impl FsState {
    /// 只读地取一个块组描述符副本。
    pub(crate) fn group_desc_ref(&self, group: u32) -> Result<GroupDesc, BlockBackendError> {
        self.group_desc
            .lock()
            .get(group as usize)
            .copied()
            .ok_or(BlockBackendError::OutOfRange)
    }
    /// 与 `group_desc_ref` 一样但供分配路径调用(语义保留)。
    pub(crate) fn group_desc_mut(&self, group: u32) -> Result<GroupDesc, BlockBackendError> {
        self.group_desc_ref(group)
    }

    pub(crate) fn group_counts(&self, group: u32) -> Result<GroupCounts, BlockBackendError> {
        self.group_counts
            .lock()
            .get(group as usize)
            .copied()
            .ok_or(BlockBackendError::OutOfRange)
    }

    pub(crate) fn adjust_group_free_blocks(
        &self,
        group: u32,
        delta: i32,
    ) -> Result<(), BlockBackendError> {
        {
            let mut g = self.group_counts.lock();
            let c = g
                .get_mut(group as usize)
                .ok_or(BlockBackendError::OutOfRange)?;
            c.free_blocks = apply_delta(c.free_blocks, delta);
        }
        self.mark_group_dirty(group)?;
        Ok(())
    }
    pub(crate) fn adjust_group_free_inodes(
        &self,
        group: u32,
        delta: i32,
    ) -> Result<(), BlockBackendError> {
        {
            let mut g = self.group_counts.lock();
            let c = g
                .get_mut(group as usize)
                .ok_or(BlockBackendError::OutOfRange)?;
            c.free_inodes = apply_delta(c.free_inodes, delta);
        }
        self.mark_group_dirty(group)?;
        Ok(())
    }
    pub(crate) fn adjust_group_used_dirs(
        &self,
        group: u32,
        delta: i32,
    ) -> Result<(), BlockBackendError> {
        {
            let mut g = self.group_counts.lock();
            let c = g
                .get_mut(group as usize)
                .ok_or(BlockBackendError::OutOfRange)?;
            c.used_dirs = apply_delta(c.used_dirs, delta);
        }
        self.mark_group_dirty(group)?;
        Ok(())
    }

    /// 对应位图进入共享块缓存后，清除块组的惰性初始化标志。
    pub(crate) fn clear_group_flags(
        &self,
        group: u32,
        flags: u16,
    ) -> Result<(), BlockBackendError> {
        {
            let mut descs = self.group_desc.lock();
            let desc = descs
                .get_mut(group as usize)
                .ok_or(BlockBackendError::OutOfRange)?;
            desc.flags &= !flags;
        }
        self.mark_group_dirty(group)
    }

    /// inode 分配推进高水位后收缩 `bg_itable_unused`。释放 inode 时不再增大该值，
    /// 因为对应 inode 表项已经完成过初始化。
    pub(crate) fn record_inode_table_use(
        &self,
        group: u32,
        trailing_unused: u32,
    ) -> Result<(), BlockBackendError> {
        {
            let mut descs = self.group_desc.lock();
            let desc = descs
                .get_mut(group as usize)
                .ok_or(BlockBackendError::OutOfRange)?;
            desc.itable_unused_count = desc.itable_unused_count.min(trailing_unused);
        }
        self.mark_group_dirty(group)
    }

    fn mark_group_dirty(&self, group: u32) -> Result<(), BlockBackendError> {
        let mut dirty = self.alloc_group_dirty.lock();
        let slot = dirty
            .get_mut(group as usize)
            .ok_or(BlockBackendError::OutOfRange)?;
        *slot = 1;
        Ok(())
    }

    pub(crate) fn adjust_sb_free_blocks(&self, delta: i64) -> Result<(), BlockBackendError> {
        let prev = self
            .sb_free_blocks
            .load(core::sync::atomic::Ordering::Acquire);
        let next = if delta < 0 {
            prev.saturating_sub((-delta) as u64)
        } else {
            prev + delta as u64
        };
        self.sb_free_blocks
            .store(next, core::sync::atomic::Ordering::Release);
        self.alloc_sb_dirty.store(true, Ordering::Release);
        Ok(())
    }
    pub(crate) fn adjust_sb_free_inodes(&self, delta: i32) -> Result<(), BlockBackendError> {
        let prev = self
            .sb_free_inodes
            .load(core::sync::atomic::Ordering::Acquire);
        let next = apply_delta(prev, delta);
        self.sb_free_inodes
            .store(next, core::sync::atomic::Ordering::Release);
        self.alloc_sb_dirty.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn ext_sb_free_blocks(&self) -> u64 {
        self.sb_free_blocks
            .load(core::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn ext_sb_free_inodes(&self) -> u32 {
        self.sb_free_inodes
            .load(core::sync::atomic::Ordering::Acquire)
    }

    fn statfs_counts(&self) -> (u64, u64, u64) {
        let free_blocks = self.ext_sb_free_blocks().min(self.ext_sb.blocks_count);
        let reserved_blocks = self
            .ext_sb
            .reserved_blocks_count
            .min(self.ext_sb.blocks_count);
        let avail_blocks = free_blocks.saturating_sub(reserved_blocks);
        let free_inodes = u64::from(self.ext_sb_free_inodes()).min(self.ext_sb.inodes_count as u64);
        (free_blocks, avail_blocks, free_inodes)
    }
    #[inline]
    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only.load(core::sync::atomic::Ordering::Acquire)
    }

    /// 以块为单位读取。
    #[inline]
    pub(crate) fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), BlockBackendError> {
        if out.len() != self.ext_sb.block_size as usize {
            return Err(BlockBackendError::OutOfRange);
        }
        self.read_blocks_coherent(block, 1, out)
    }

    /// 在不持 cache Spinlock 的情况下执行后端读，并用 range-local stamp 防止旧读回填。
    fn read_blocks_coherent(
        &self,
        start_block: u64,
        count: u32,
        out: &mut [u8],
    ) -> Result<(), BlockBackendError> {
        let bs = self.ext_sb.block_size as usize;
        if out.len() != bs * count as usize {
            return Err(BlockBackendError::OutOfRange);
        }
        const MAX_CHUNK_BLOCKS: u32 = 128;
        let mut off = 0usize;
        let mut remaining = count;
        let mut block = start_block;
        while remaining > 0 {
            let n = remaining.min(MAX_CHUNK_BLOCKS);
            let chunk_bytes = bs * n as usize;
            let chunk = &mut out[off..off + chunk_bytes];
            'read_chunk: loop {
                let chunk_stamp = {
                    let cache = self.block_cache.lock();
                    if cache.has_unreadable_direct_in_range(block, n) {
                        None
                    } else {
                        Some(cache.coherence_stamp_in_range(block, n))
                    }
                };
                let Some(chunk_stamp) = chunk_stamp else {
                    Self::wait_for_writeback_progress();
                    continue;
                };

                let mut completed = 0u32;
                while completed < n {
                    let current_block = block + completed as u64;
                    let current_offset = completed as usize * bs;
                    let current = &mut chunk[current_offset..];
                    let (cached, missing, coherence_stamp, wait_direct) = {
                        let mut cache = self.block_cache.lock();
                        let cached =
                            cache.read_cached_prefix(current_block, n - completed, current);
                        if cached != 0 {
                            (cached, 0, (0, 0), false)
                        } else if cache.has_unreadable_direct_in_range(current_block, 1) {
                            (0, 0, (0, 0), true)
                        } else {
                            let missing = cache.uncached_prefix_len(current_block, n - completed);
                            debug_assert!(missing != 0);
                            (
                                0,
                                missing,
                                cache.coherence_stamp_in_range(current_block, missing),
                                false,
                            )
                        }
                    };
                    if cached != 0 {
                        completed += cached;
                        continue;
                    }
                    if wait_direct {
                        Self::wait_for_writeback_progress();
                        continue;
                    }

                    let missing_bytes = missing as usize * bs;
                    let target = &mut current[..missing_bytes];

                    bgd::read_blocks(
                        self.backend.as_ref(),
                        &self.ext_sb,
                        current_block,
                        missing,
                        target,
                    )?;
                    let mut cache = self.block_cache.lock();
                    if cache.coherence_stamp_in_range(current_block, missing) != coherence_stamp {
                        // 写入可能已经落盘并从 active/pending 消失；仅 overlay
                        // 不足以证明 miss 部分仍是新数据，必须丢弃本次后端结果。
                        if cache.read_range(current_block, missing, target) {
                            completed += missing;
                            continue;
                        }
                        let wait_direct =
                            cache.has_unreadable_direct_in_range(current_block, missing);
                        drop(cache);
                        if wait_direct {
                            Self::wait_for_writeback_progress();
                        }
                        continue;
                    }
                    if cache.has_unreadable_direct_in_range(current_block, missing) {
                        drop(cache);
                        Self::wait_for_writeback_progress();
                        continue;
                    }

                    cache.overlay_range(current_block, missing, target);
                    for i in 0..missing {
                        let cur_block = current_block + i as u64;
                        let start = bs * i as usize;
                        let end = start + bs;
                        if !cache.contains(cur_block) && !cache.has_active_direct(cur_block) {
                            cache.try_insert_clean(cur_block, &target[start..end]);
                        }
                    }
                    completed += missing;
                }

                let mut cache = self.block_cache.lock();
                if cache.coherence_stamp_in_range(block, n) != chunk_stamp {
                    continue 'read_chunk;
                }
                if cache.has_unreadable_direct_in_range(block, n) {
                    drop(cache);
                    Self::wait_for_writeback_progress();
                    continue 'read_chunk;
                }
                cache.overlay_range(block, n, chunk);
                break;
            }
            off += chunk_bytes;
            block += n as u64;
            remaining -= n;
        }
        Ok(())
    }

    /// 把连续块直接读入多个块对齐缓冲区，并保持与 write-back/direct write 一致。
    ///
    /// 与普通 coherent read 不同，本路径不会把后端结果插入 clean block cache；
    /// 它只使用 cache 中已有的新版本覆盖对应目标块。
    fn read_blocks_coherent_vectored(
        &self,
        start_block: u64,
        count: u32,
        out: &mut [&mut [u8]],
    ) -> Result<(), BlockBackendError> {
        let block_size = self.ext_sb.block_size as usize;
        if vectored_block_count(out, block_size) != Some(count) {
            return Err(BlockBackendError::OutOfRange);
        }
        if count == 0 {
            return Ok(());
        }

        loop {
            let coherence_stamp = {
                let mut cache = self.block_cache.lock();
                if cache.read_range_vectored(start_block, count, out) {
                    break;
                }
                if cache.has_unreadable_direct_in_range(start_block, count) {
                    drop(cache);
                    Self::wait_for_writeback_progress();
                    continue;
                }
                cache.coherence_stamp_in_range(start_block, count)
            };

            bgd::read_blocks_vectored(
                self.backend.as_ref(),
                &self.ext_sb,
                start_block,
                count,
                out,
            )?;
            let mut cache = self.block_cache.lock();
            if cache.coherence_stamp_in_range(start_block, count) != coherence_stamp {
                if cache.read_range_vectored(start_block, count, out) {
                    break;
                }
                let wait_direct = cache.has_unreadable_direct_in_range(start_block, count);
                drop(cache);
                if wait_direct {
                    Self::wait_for_writeback_progress();
                }
                continue;
            }
            if cache.has_unreadable_direct_in_range(start_block, count) {
                drop(cache);
                Self::wait_for_writeback_progress();
                continue;
            }

            cache.overlay_range_vectored(start_block, count, out);
            break;
        }
        Ok(())
    }

    /// 在 cache 内原地修改块的部分字节（partial write 快速路径）。
    /// 块必须已在 cache 中；若不在则返回 false，调用方回退到 read + modify + write。
    /// 成功时只需一次加锁、零次 memcpy 整块。
    /// 注：此方法用于数据块覆盖写，不改变块映射关系，故不递增 epoch。
    #[inline]
    pub(crate) fn modify_block_partial(&self, block: u64, offset: usize, src: &[u8]) -> bool {
        let mut cache = self.block_cache.lock();
        let modified = cache.modify_inplace(block, offset, src);
        modified
    }

    /// 丢弃一段数据块缓存。释放块前必须清理 dirty cache，避免块被重新分配后
    /// 旧文件的延迟写回覆盖新文件数据。
    pub(crate) fn discard_cached_blocks(
        &self,
        start_block: u64,
        count: u32,
    ) -> Result<(), BlockBackendError> {
        if count == 0 {
            return Ok(());
        }
        loop {
            self.flush_pending_range(start_block, count)?;
            let mut cache = self.block_cache.lock();
            if cache.has_pending_in_range(start_block, count)
                || cache.has_active_direct_in_range(start_block, count)
            {
                drop(cache);
                Self::wait_for_writeback_progress();
                continue;
            }
            cache.invalidate_range(start_block, count);
            return Ok(());
        }
    }

    /// 修改块内部分字节。cache miss 时只读一次整块，之后转为 write-back dirty。
    ///
    /// inode table / bitmap 这类元数据经常在同一个 4K 块内连续修改多个小结构。
    /// 如果每次都 read-modify-write 整块，会把 iozone 的 create/unlink 阶段放大成
    /// 大量 4K virtio-mmio 请求；这里优先复用已缓存脏块，miss 时才补一次整块读。
    pub(crate) fn write_block_partial(
        &self,
        block: u64,
        offset: usize,
        src: &[u8],
    ) -> Result<(), BlockBackendError> {
        if offset + src.len() > self.ext_sb.block_size as usize {
            return Err(BlockBackendError::OutOfRange);
        }
        if self.modify_block_partial(block, offset, src) {
            return Ok(());
        }

        loop {
            let read_stamp = self.block_cache.lock().coherence_stamp_in_range(block, 1);
            let mut data = vec![0u8; self.ext_sb.block_size as usize];
            self.read_block(block, &mut data)?;
            let outcome = self
                .block_cache
                .lock()
                .merge_partial_after_read(block, offset, src, &mut data, read_stamp);
            match outcome {
                PartialWriteOutcome::Wait => Self::wait_for_writeback_progress(),
                PartialWriteOutcome::Retry => continue,
                PartialWriteOutcome::Done(evicted) => {
                    if let Some(evicted) = evicted {
                        self.flush_one_evicted(&evicted)?;
                    }
                    return Ok(());
                }
            }
        }
    }

    /// 以块为单位写入（write-back：只更新 cache，延迟落盘）。
    pub(crate) fn write_block(&self, block: u64, data: &[u8]) -> Result<(), BlockBackendError> {
        if data.len() != self.ext_sb.block_size as usize {
            return Err(BlockBackendError::OutOfRange);
        }
        let evicted = {
            let mut cache = self.block_cache.lock();
            cache.insert_wb(block, data)
        };
        if let Some(evicted) = evicted {
            self.flush_one_evicted(&evicted)?;
        }
        Ok(())
    }

    /// 数据块专用写入：与 write_block 相同的 write-back 语义，
    /// 但不递增 epoch（数据覆盖不改变块映射关系，避免 map_cache 无效化）。
    pub(crate) fn write_data_block(
        &self,
        block: u64,
        data: &[u8],
    ) -> Result<(), BlockBackendError> {
        if data.len() != self.ext_sb.block_size as usize {
            return Err(BlockBackendError::OutOfRange);
        }
        let evicted = {
            let mut cache = self.block_cache.lock();
            cache.insert_wb(block, data)
        };
        if let Some(evicted) = evicted {
            self.flush_one_evicted(&evicted)?;
        }
        Ok(())
    }

    /// 批量读块。
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn read_blocks(
        &self,
        start_block: u64,
        count: u32,
        out: &mut [u8],
    ) -> Result<(), BlockBackendError> {
        let bs = self.ext_sb.block_size as usize;
        let expected = bs * count as usize;
        if out.len() != expected {
            return Err(BlockBackendError::OutOfRange);
        }
        self.read_blocks_coherent(start_block, count, out)
    }

    pub(crate) fn read_data_blocks(
        &self,
        start_block: u64,
        count: u32,
        out: &mut [u8],
    ) -> Result<(), BlockBackendError> {
        let bs = self.ext_sb.block_size as usize;
        let expected = bs * count as usize;
        if out.len() != expected {
            return Err(BlockBackendError::OutOfRange);
        }
        self.read_blocks_coherent(start_block, count, out)
    }

    pub(crate) fn read_data_blocks_vectored(
        &self,
        start_block: u64,
        count: u32,
        out: &mut [&mut [u8]],
    ) -> Result<(), BlockBackendError> {
        self.read_blocks_coherent_vectored(start_block, count, out)
    }

    pub(crate) fn write_blocks(
        &self,
        start_block: u64,
        count: u32,
        data: &[u8],
    ) -> Result<(), BlockBackendError> {
        let expected = self.ext_sb.block_size as usize * count as usize;
        if data.len() != expected {
            return Err(BlockBackendError::OutOfRange);
        }
        if count == 0 {
            return Ok(());
        }
        let bs = self.ext_sb.block_size as usize;
        let mut evicted_list: Vec<DirtyBlockSnapshot> = Vec::new();
        {
            let mut cache = self.block_cache.lock();
            for i in 0..count {
                let off = bs * i as usize;
                let block = start_block + i as u64;
                if let Some(ev) = cache.insert_wb(block, &data[off..off + bs]) {
                    evicted_list.push(ev);
                }
            }
        }
        self.flush_evicted(&mut evicted_list)?;
        Ok(())
    }

    /// 数据块专用批量写入：不递增 epoch（数据覆盖不改变块映射）。
    pub(crate) fn write_data_blocks(
        &self,
        start_block: u64,
        count: u32,
        data: &[u8],
    ) -> Result<(), BlockBackendError> {
        let expected = self.ext_sb.block_size as usize * count as usize;
        if data.len() != expected {
            return Err(BlockBackendError::OutOfRange);
        }
        if count == 0 {
            return Ok(());
        }
        if count >= 4 {
            // 等待旧 victim 后，在同一 cache 锁内注册 direct owner。这个占位
            // 关闭了“检查 pending”到“发起 I/O”之间的窗口。
            let version = loop {
                self.flush_pending_range(start_block, count)?;
                let mut cache = self.block_cache.lock();
                if let Some(version) = cache.begin_direct_write(start_block, count, data) {
                    break version;
                }
                drop(cache);
                Self::wait_for_writeback_progress();
            };

            let result = bgd::write_blocks(
                self.backend.as_ref(),
                &self.ext_sb,
                start_block,
                count,
                data,
            );
            self.block_cache.lock().finish_direct_write(
                start_block,
                count,
                version,
                data,
                result.is_ok(),
            );
            return result;
        }
        let bs = self.ext_sb.block_size as usize;
        let mut evicted_list: Vec<DirtyBlockSnapshot> = Vec::new();
        {
            let mut cache = self.block_cache.lock();
            for i in 0..count as usize {
                let off = bs * i;
                let block = start_block + i as u64;
                if let Some(ev) = cache.insert_wb(block, &data[off..off + bs]) {
                    evicted_list.push(ev);
                }
            }
        }
        self.flush_evicted(&mut evicted_list)?;
        Ok(())
    }

    fn flush_one_evicted(&self, snapshot: &DirtyBlockSnapshot) -> Result<(), BlockBackendError> {
        crate::mmp::heartbeat(self);
        let mut current = snapshot.clone();
        loop {
            if let Err(err) = bgd::write_blocks(
                self.backend.as_ref(),
                &self.ext_sb,
                current.block,
                1,
                current.data.as_slice(),
            ) {
                self.block_cache.lock().fail_pending(current.block);
                return Err(err);
            }
            let next = self
                .block_cache
                .lock()
                .complete_pending(current.block, current.version);
            match next {
                Some(snapshot) => current = snapshot,
                None => return Ok(()),
            }
        }
    }

    fn flush_evicted(&self, list: &mut Vec<DirtyBlockSnapshot>) -> Result<(), BlockBackendError> {
        crate::mmp::heartbeat(self);
        let mut queue = core::mem::take(list);
        let bs = self.ext_sb.block_size as usize;
        const MAX_FLUSH_RUN_BLOCKS: usize = 128;
        while !queue.is_empty() {
            queue.sort_unstable_by_key(|snapshot| snapshot.block);
            let mut followups: Vec<DirtyBlockSnapshot> = Vec::new();
            let mut i = 0usize;
            while i < queue.len() {
                let run_start = queue[i].block;
                let mut j = i + 1;
                while j < queue.len()
                    && queue[j].block == queue[j - 1].block + 1
                    && j - i < MAX_FLUSH_RUN_BLOCKS
                {
                    j += 1;
                }
                let result = if j == i + 1 {
                    bgd::write_blocks(
                        self.backend.as_ref(),
                        &self.ext_sb,
                        run_start,
                        1,
                        &queue[i].data,
                    )
                } else {
                    let mut merged = Vec::with_capacity(bs * (j - i));
                    for snapshot in &queue[i..j] {
                        merged.extend_from_slice(&snapshot.data);
                    }
                    bgd::write_blocks(
                        self.backend.as_ref(),
                        &self.ext_sb,
                        run_start,
                        (j - i) as u32,
                        &merged,
                    )
                };
                if let Err(err) = result {
                    let mut cache = self.block_cache.lock();
                    for snapshot in &queue[i..] {
                        cache.fail_pending(snapshot.block);
                    }
                    for snapshot in &followups {
                        cache.fail_pending(snapshot.block);
                    }
                    return Err(err);
                }
                {
                    let mut cache = self.block_cache.lock();
                    for snapshot in &queue[i..j] {
                        if let Some(next) = cache.complete_pending(snapshot.block, snapshot.version)
                        {
                            followups.push(next);
                        }
                    }
                }
                i = j;
            }
            queue = followups;
        }
        Ok(())
    }

    fn wait_for_writeback_progress() {
        if sched::is_ready() {
            sched::poll_urgent_work();
            sched::schedule_once(sched::now_ns_public());
        } else {
            core::hint::spin_loop();
        }
    }

    fn flush_pending_writebacks(&self) -> Result<(), BlockBackendError> {
        loop {
            let (snapshot, has_pending) = {
                let mut cache = self.block_cache.lock();
                let snapshot = cache.claim_pending();
                let has_pending = cache.has_pending();
                (snapshot, has_pending)
            };
            if let Some(snapshot) = snapshot {
                self.flush_one_evicted(&snapshot)?;
                continue;
            }
            if !has_pending {
                return Ok(());
            }
            // 其余 pending 正由别的 owner 写盘；sync 必须等它完成，不能假成功。
            Self::wait_for_writeback_progress();
        }
    }

    fn flush_pending_range(&self, start: u64, count: u32) -> Result<(), BlockBackendError> {
        loop {
            let (snapshot, has_pending) = {
                let mut cache = self.block_cache.lock();
                let snapshot = cache.claim_pending_in_range(start, count);
                let has_pending = cache.has_pending_in_range(start, count);
                (snapshot, has_pending)
            };
            if let Some(snapshot) = snapshot {
                self.flush_one_evicted(&snapshot)?;
                continue;
            }
            if !has_pending {
                return Ok(());
            }
            Self::wait_for_writeback_progress();
        }
    }

    pub(crate) fn flush_dirty_blocks(&self) -> Result<(), BlockBackendError> {
        loop {
            // 先接管此前失败、无人负责的 victim；若另一个 owner 仍在 I/O，等待其完成。
            self.flush_pending_writebacks()?;

            let (snapshots, mut owners, quiescent) = {
                let mut cache = self.block_cache.lock();
                let snapshots = cache.dirty_snapshots();
                let owners = snapshots
                    .iter()
                    .filter_map(|snapshot| cache.remember_pending(snapshot.clone()))
                    .collect::<Vec<_>>();
                let quiescent = snapshots.is_empty()
                    && !cache.has_pending()
                    && !cache.has_active_direct_writes();
                (snapshots, owners, quiescent)
            };
            if snapshots.is_empty() {
                if quiescent {
                    return Ok(());
                }
                // pending 可能在首次 flush 与本次快照之间刚发布，或仍有
                // direct owner 在锁外 I/O；两种情况都不能把 sync 误报为完成。
                Self::wait_for_writeback_progress();
                continue;
            }
            self.flush_evicted(&mut owners)?;
            self.flush_pending_writebacks()?;
            {
                let mut cache = self.block_cache.lock();
                for snapshot in &snapshots {
                    cache.mark_clean(snapshot.block, snapshot.version);
                }
            }
            // 写回期间若同一块又被修改，version 不匹配会保留 dirty；重扫直到稳定。
        }
    }

    pub(crate) fn flush_alloc_metadata(&self) -> Result<(), BlockBackendError> {
        // 第一次先排空普通脏块，缩短全局分配锁的持有时间。随后取得分配锁并再次
        // 排空：此前进行中的 bitmap 修改至此已经发布，锁内快照的 flags/counts
        // 与落盘 bitmap 属于同一状态，sync 返回时也不会遗漏刚被摘走的 dirty 位。
        self.flush_dirty_blocks()?;
        let _alloc = crate::alloc_mod::lock_alloc();
        self.flush_dirty_blocks()?;
        let dirty_groups = {
            let mut dirty = self.alloc_group_dirty.lock();
            let mut groups = Vec::new();
            for (group, is_dirty) in dirty.iter_mut().enumerate() {
                if *is_dirty != 0 {
                    *is_dirty = 0;
                    groups.push(group as u32);
                }
            }
            groups
        };
        let sb_dirty = self.alloc_sb_dirty.swap(false, Ordering::AcqRel);

        for (idx, &group) in dirty_groups.iter().enumerate() {
            if let Err(err) = crate::alloc_mod::flush_group_desc(self, group) {
                let mut dirty = self.alloc_group_dirty.lock();
                for &pending_group in &dirty_groups[idx..] {
                    if let Some(slot) = dirty.get_mut(pending_group as usize) {
                        *slot = 1;
                    }
                }
                if sb_dirty {
                    self.alloc_sb_dirty.store(true, Ordering::Release);
                }
                return Err(err);
            }
        }

        if sb_dirty {
            if let Err(err) = crate::alloc_mod::write_superblock(self) {
                self.alloc_sb_dirty.store(true, Ordering::Release);
                return Err(err);
            }
        }
        Ok(())
    }

    /// 发布 inode 最新原始快照，不在此锁内做任何块 I/O。
    ///
    /// 同一 inode 若正有 owner 写回，新版本只替换 pending 快照；旧 owner
    /// 完成后会检查版本并继续 drain，不会误删新版本。
    pub(crate) fn stage_inode_write(&self, raw: &RawInode) -> u64 {
        self.inode_writeback.lock().stage(raw)
    }

    /// 将运行时 inode 变更纳入统一的按 ino 版本化写回。
    ///
    /// 普通 inode 操作必须走此接口，不能直接调用底层 `write_raw`，
    /// 否则会越过同 ino owner，让旧快照后到覆盖新的 size/nlink。
    pub(crate) fn publish_inode_write(&self, raw: &RawInode) -> Result<(), BlockBackendError> {
        self.stage_inode_write(raw);
        self.flush_inode_write(raw.ino)
    }

    fn drain_inode_writeback(
        &self,
        ino: u32,
        mut raw: RawInode,
        mut version: u64,
    ) -> Result<(), BlockBackendError> {
        loop {
            if let Err(err) = crate::inode_wr::write_raw(self, &raw) {
                // 失败时保留当前最新快照，仅释放 owner，供后续
                // write 协助或 sync 重试。
                let mut state = self.inode_writeback.lock();
                if let Some(pending) = state.pending.get_mut(&ino) {
                    pending.in_flight = false;
                }
                return Err(err);
            }

            let mut state = self.inode_writeback.lock();
            let Some(pending) = state.pending.get(&ino) else {
                return Ok(());
            };
            if pending.version == version {
                state.pending.remove(&ino);
                return Ok(());
            }

            // 写回期间出现了更新快照：当前 owner 保持 in_flight，
            // 在锁外继续写最新版本。
            raw = pending.raw.clone();
            version = pending.version;
        }
    }

    /// 等待或协助将指定 inode 的最新 pending 快照发布到 block cache。
    pub(crate) fn flush_inode_write(&self, ino: u32) -> Result<(), BlockBackendError> {
        loop {
            let claim = self.inode_writeback.lock().claim(ino);
            match claim {
                InodeWritebackClaim::Clean => return Ok(()),
                InodeWritebackClaim::Busy => Self::wait_for_writeback_progress(),
                InodeWritebackClaim::Owner { raw, version } => {
                    return self.drain_inode_writeback(ino, raw, version);
                }
            }
        }
    }

    fn flush_pending_inode_writes(&self) -> Result<(), BlockBackendError> {
        loop {
            let ino = self.inode_writeback.lock().pending.keys().next().copied();
            let Some(ino) = ino else {
                return Ok(());
            };
            self.flush_inode_write(ino)?;
        }
    }

    pub(crate) fn sync_all(&self) -> Result<(), BlockBackendError> {
        crate::mmp::heartbeat(self);
        loop {
            self.flush_pending_inode_writes()?;
            let staged_seq = self.inode_writeback.lock().seq;
            self.flush_alloc_metadata()?;
            let state = self.inode_writeback.lock();
            if state.pending.is_empty() && state.seq == staged_seq {
                return Ok(());
            }
            // block cache 写回期间若又发布了 inode 快照，重扫直到
            // pending 和版本化块缓存在同一轮中都稳定。
        }
    }

    /// 定位一个 inode 号所在的块号与块内字节偏移。
    pub(crate) fn inode_location(&self, ino: u32) -> Result<(u64, u32), BlockBackendError> {
        if ino == 0 || ino > self.ext_sb.inodes_count {
            return Err(BlockBackendError::OutOfRange);
        }
        let per_group = self.ext_sb.inodes_per_group;
        let inode_size = self.ext_sb.inode_size;
        let block_size = self.ext_sb.block_size;
        let group = (ino - 1) / per_group;
        let offset_in_group = (ino - 1) % per_group;
        let gd = self
            .group_desc
            .lock()
            .get(group as usize)
            .copied()
            .ok_or(BlockBackendError::OutOfRange)?;
        let byte_off = offset_in_group as u64 * inode_size as u64;
        let block = gd.inode_table + byte_off / block_size as u64;
        let in_block = (byte_off % block_size as u64) as u32;
        Ok((block, in_block))
    }
}

#[inline]
fn apply_delta(cur: u32, delta: i32) -> u32 {
    if delta < 0 {
        cur.saturating_sub((-delta) as u32)
    } else {
        cur + delta as u32
    }
}

/// 对外的驱动句柄。
pub struct ExtFsDriver {
    backend: Spinlock<Option<Arc<dyn BlockBackend>>>,
}

#[kernel_symbols::export]
impl ExtFsDriver {
    pub const fn new() -> Self {
        Self {
            backend: Spinlock::new(None),
        }
    }

    #[kernel_symbols::export(
        name = "extfs.ExtFsDriver.bind_backend",
        contract = "kernel.filesystem.ext-driver@1",
        version = 1,
        capabilities = kernel_symbols::capability::FILESYSTEM_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE,
        retained_args = 1 << 1
    )]
    pub fn bind_backend(&self, backend: Arc<dyn BlockBackend>) {
        *self.backend.lock() = Some(backend);
    }
}

impl Default for ExtFsDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl FsDriver for ExtFsDriver {
    fn name(&self) -> &'static str {
        "extfs"
    }

    fn flags(&self) -> FsDriverFlags {
        // ext 驱动是只读,强制只读挂载由驱动自己拦截。
        FsDriverFlags::default()
    }

    fn mount(&self, _dev: Option<&str>, _data: &str) -> VfsResult<Arc<VfsSuperblock>> {
        let backend = self
            .backend
            .lock()
            .as_ref()
            .map(Arc::clone)
            .ok_or(VfsError::NoDevice)?;
        mount_impl(backend)
    }

    fn kill_sb(&self, sb: Arc<VfsSuperblock>) {
        let Some(ops) = sb.ops.as_any().downcast_ref::<ExtFsSuperblockOps>() else {
            return;
        };
        let state = Arc::clone(&ops.state);
        // 先转入只读阻止新写,再全量回写,最后写回 VALID_FS 标记干净卸载。
        state
            .read_only
            .store(true, core::sync::atomic::Ordering::Release);
        let _ = state.sync_all();
        let _ = crate::alloc_mod::mark_clean_unmount(&state);
        // 释放 MMP 所有权,允许后续节点挂载。
        crate::mmp::mark_clean(&state);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub(crate) struct ExtFsSuperblockOps {
    pub(crate) state: Arc<FsState>,
}

impl SuperblockOps for ExtFsSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<VfsSuperblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::ReadOnlyFilesystem)
    }
    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }
    fn can_evict_positive_dentry(&self) -> bool {
        true
    }
    fn statfs(&self, sb: &Arc<VfsSuperblock>) -> VfsResult<FsStat> {
        let s = &self.state.ext_sb;
        let (free_blocks, avail_blocks, free_inodes) = self.state.statfs_counts();
        Ok(FsStat {
            fs_type: 0xef53,
            block_size: s.block_size as u64,
            total_blocks: s.blocks_count,
            free_blocks,
            avail_blocks,
            total_inodes: s.inodes_count as u64,
            free_inodes,
            fs_id: sb.fs_id.raw(),
            name_max: 255,
        })
    }
    fn sync_fs(&self, _sb: &Arc<VfsSuperblock>) -> VfsResult<()> {
        self.state.sync_all().map_err(map_err)
    }
    fn supports_direct_io(&self) -> bool {
        true
    }

    fn remount(&self, _sb: &Arc<VfsSuperblock>, flags: MountFlags) -> VfsResult<()> {
        use core::sync::atomic::Ordering;
        if flags.has(MountFlags::RDONLY) {
            // rw → ro:全量回写后标记干净卸载(与 Linux 行为一致)。
            self.state.sync_all().map_err(map_err)?;
            self.state.read_only.store(true, Ordering::Release);
            crate::alloc_mod::mark_clean_unmount(&self.state).map_err(map_err)?;
            crate::mmp::mark_clean(&self.state);
        } else {
            if self.state.ext_sb.force_read_only {
                // BIGALLOC/READONLY/SHARED_BLOCKS 等语义位不允许读写。
                return Err(VfsError::ReadOnlyFilesystem);
            }
            self.state.read_only.store(false, Ordering::Release);
            crate::alloc_mod::mark_mounted(&self.state, false).map_err(map_err)?;
            // 重新夺占 MMP 所有权。
            crate::mmp::claim(&self.state).map_err(map_err)?;
        }
        Ok(())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// MMP(多挂载保护)挂载时检查。
///
/// 与 Linux `ext4_multi_mount_protect` 的入口语义对齐:
/// - `EXT4_MMP_SEQ_CLEAN`:无其它节点挂载,允许挂载;
/// - `EXT4_MMP_SEQ_FSCK`:正被 fsck,拒绝;
/// - 其它非 CLEAN 序列:读取 `mmp_time`,若距上次心跳超过 `2 ×
///   s_mmp_update_interval` 视为陈旧(上个宿主崩溃/掉线),允许接管;否则视为
///   仍有存活节点,拒绝双挂载。
///
/// 运行时心跳见 [`crate::mmp`]。
fn mmp_check(backend: &dyn BlockBackend, sb: &ExtSb) -> Result<(), BlockBackendError> {
    use crate::layout::*;

    if sb.feature_incompat & INCOMPAT_MMP == 0 {
        return Ok(());
    }
    let mmp_block = sb.mmp_block;
    if mmp_block == 0 || mmp_block >= sb.blocks_count {
        return Err(BlockBackendError::OutOfRange);
    }
    let mut buf = vec![0u8; sb.block_size as usize];
    bgd::read_blocks(backend, sb, mmp_block, 1, &mut buf)?;
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != EXT4_MMP_MAGIC {
        return Err(BlockBackendError::OutOfRange);
    }
    if sb.metadata_csum {
        // ext4_mmp_csum:crc32c(csum_seed, mmp[..offsetof(mmp_checksum)])
        let provided = u32::from_le_bytes([buf[1020], buf[1021], buf[1022], buf[1023]]);
        let calculated = crc::update(sb.csum_seed, &buf[..1020]);
        if provided != calculated {
            return Err(BlockBackendError::OutOfRange);
        }
    }
    let seq = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if seq == EXT4_MMP_SEQ_CLEAN {
        return Ok(());
    }
    if seq == EXT4_MMP_SEQ_FSCK {
        log::error!("[extfs] MMP: 文件系统正被 fsck(序列号 {seq:#x}),拒绝挂载");
        return Err(BlockBackendError::Io);
    }
    // 非 CLEAN / 非 FSCK:判定心跳是否已过期。
    let mmp_time = u64::from_le_bytes([
        buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
    ]);
    let interval = if sb.mmp_update_interval == 0 {
        5
    } else {
        sb.mmp_update_interval as u64
    };
    let now = vfs::stat::Timespec::now().secs.max(0) as u64;
    if now.saturating_sub(mmp_time) >= interval.saturating_mul(2) {
        // 陈旧心跳:上个宿主崩溃或掉线,允许接管。
        log::warning!(
            "[extfs] MMP: 心跳陈旧(seq={seq:#x}, 距今 {}s),接管挂载",
            now.saturating_sub(mmp_time)
        );
        return Ok(());
    }
    log::error!("[extfs] MMP: 序列号 {seq:#x} 非 CLEAN,可能正被其它节点挂载,拒绝挂载");
    Err(BlockBackendError::Io)
}

fn mount_impl(backend: Arc<dyn BlockBackend>) -> VfsResult<Arc<VfsSuperblock>> {
    let ext_sb = sb::load(backend.as_ref()).map_err(|e| {
        log::warning!("[extfs] mount step sb::load failed: {:?}", e);
        map_err(e)
    })?;
    // MMP 检查:只读操作,先于一切写路径。
    mmp_check(backend.as_ref(), &ext_sb).map_err(|e| {
        log::warning!("[extfs] mount step mmp_check failed: {:?}", e);
        map_err(e)
    })?;
    let group_desc = bgd::load_all(backend.as_ref(), &ext_sb).map_err(|e| {
        log::warning!("[extfs] mount step bgd::load_all failed: {:?}", e);
        map_err(e)
    })?;
    let group_counts = group_desc
        .iter()
        .map(|g| GroupCounts {
            free_blocks: g.free_blocks_count,
            free_inodes: g.free_inodes_count,
            used_dirs: g.used_dirs_count,
        })
        .collect::<alloc::vec::Vec<_>>();
    let group_count = group_desc.len();
    let free_blocks = ext_sb.free_blocks_count;
    let free_inodes = ext_sb.free_inodes_count;
    let block_size = ext_sb.block_size;
    let force_ro = ext_sb.force_read_only;
    let needs_recovery = ext_sb.feature_incompat & crate::layout::INCOMPAT_RECOVER != 0;
    let state = Arc::new(FsState {
        backend: Arc::clone(&backend),
        ext_sb,
        group_desc: Spinlock::new(group_desc),
        group_counts: Spinlock::new(group_counts),
        block_cache: Spinlock::new(BlockCache::new(block_size)),
        sb_free_blocks: core::sync::atomic::AtomicU64::new(free_blocks),
        sb_free_inodes: core::sync::atomic::AtomicU32::new(free_inodes),
        block_alloc_hint: core::sync::atomic::AtomicU64::new(0),
        inode_alloc_hint: core::sync::atomic::AtomicU32::new(0),
        alloc_group_dirty: Spinlock::new(vec![0u8; group_count]),
        alloc_sb_dirty: AtomicBool::new(false),
        inode_writeback: Spinlock::new(DirtyInodeState::new()),
        read_only: core::sync::atomic::AtomicBool::new(force_ro),
        mmp: MmpRuntime::new(),
    });

    // MMP 夺占所有权:写回非 CLEAN 序列号,阻止第二个节点并发挂载。
    // 必须在日志恢复/孤儿清理等任何写路径之前完成。
    crate::mmp::claim(&state).map_err(map_err)?;

    // 日志恢复(NEEDS_RECOVERY):回放已提交事务 + fast commit 区域,
    // 复位日志头后清除主超级块的 RECOVER 位。
    if needs_recovery {
        if state.is_read_only() {
            log::error!("[extfs] 文件系统需要日志恢复,但特性强制只读挂载,拒绝");
            return Err(VfsError::NotSupported);
        }
        crate::journal::recover(&state).map_err(map_err)?;
        crate::alloc_mod::patch_superblock(&state, |sb| {
            let incompat = u32::from_le_bytes([
                sb[crate::layout::sb_off::FEATURE_INCOMPAT],
                sb[crate::layout::sb_off::FEATURE_INCOMPAT + 1],
                sb[crate::layout::sb_off::FEATURE_INCOMPAT + 2],
                sb[crate::layout::sb_off::FEATURE_INCOMPAT + 3],
            ]) & !crate::layout::INCOMPAT_RECOVER;
            sb[crate::layout::sb_off::FEATURE_INCOMPAT
                ..crate::layout::sb_off::FEATURE_INCOMPAT + 4]
                .copy_from_slice(&incompat.to_le_bytes());
        })
        .map_err(map_err)?;
    }

    // 孤儿 inode 清理(s_last_orphan 链表 + orphan file)。
    if !state.is_read_only() {
        crate::orphan::cleanup(&state).map_err(|e| {
            log::warning!("[extfs] mount step orphan::cleanup failed: {:?}", e);
            map_err(e)
        })?;
    }

    // 挂载记账:清/置 VALID_FS、s_mnt_count+1、s_mtime。
    crate::alloc_mod::mark_mounted(&state, state.is_read_only()).map_err(|e| {
        log::warning!("[extfs] mount step mark_mounted failed: {:?}", e);
        map_err(e)
    })?;
    // 恢复 + 孤儿清理 + 记账的结果先落盘,再开始对外服务。
    state.sync_all().map_err(|e| {
        log::warning!("[extfs] mount step sync_all failed: {:?}", e);
        map_err(e)
    })?;

    // 加载根 inode(2 号)
    let (root_meta_on_disk, root_raw) = load_inode(&state, EXT4_ROOT_INO).map_err(|e| {
        log::warning!("[extfs] mount step load_inode(root) failed: {:?}", e);
        map_err(e)
    })?;
    let fs_id = FsId::new(EXTFS_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));

    let kind_hint = match state.ext_sb.kind {
        ExtKind::Ext2 => "ext2",
        ExtKind::Ext3 => "ext3",
        ExtKind::Ext4 => "ext4",
    };

    let sb = VfsSuperblock::new(|weak_sb| {
        let meta = InodeMeta {
            size: root_meta_on_disk.size,
            nlink: root_meta_on_disk.nlink as u32,
            mode: FileMode::new((root_meta_on_disk.mode & 0o7777) as u16),
            uid: Uid(root_meta_on_disk.uid),
            gid: Gid(root_meta_on_disk.gid),
            atime: root_meta_on_disk.atime,
            mtime: root_meta_on_disk.mtime,
            ctime: root_meta_on_disk.ctime,
            blocks: root_meta_on_disk.blocks_512,
        };
        let ops = ExtInodeOps::new(Arc::clone(&state), EXT4_ROOT_INO, root_raw.clone());
        let root_inode = Inode::new(
            InodeId {
                fs_id,
                ino: EXT4_ROOT_INO as u64,
            },
            FileType::Directory,
            DevId::new(0, 0),
            block_size,
            None,
            meta,
            Arc::new(ops) as Arc<dyn vfs::inode::InodeOps + Send + Sync>,
            weak_sb.clone(),
        );
        let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));
        VfsSuperblock {
            fs_type: kind_hint,
            fs_id,
            dev_id: None,
            block_size,
            name_max: 255,
            root_inode,
            root_dentry,
            inode_cache: InodeCache::new(),
            ops: Box::new(ExtFsSuperblockOps {
                state: Arc::clone(&state),
            }),
            self_weak: weak_sb,
        }
    });

    Ok(sb)
}

/// 将 [`BlockBackendError`] 映射到 VFS 错误。
pub(crate) fn map_err(e: BlockBackendError) -> VfsError {
    match e {
        BlockBackendError::Io => VfsError::Io,
        BlockBackendError::OutOfRange => VfsError::InvalidArgument,
        BlockBackendError::Unsupported => VfsError::NotSupported,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;
    use std::sync::{Condvar, Mutex as StdMutex, mpsc};
    use std::thread;
    use std::time::Duration;

    use crate::bgd::GroupDesc;
    use crate::file::{ExtRegFileOps, read_aligned_blocks};
    use crate::inode::{ExtInodeOps, load_inode};
    use crate::inode_wr::{RawInode, write_raw};
    use crate::layout::{EXT4_EXTENTS_FL, ExtKind, S_IFREG};
    use crate::map_wr::{self, BlockAllocState};
    use crate::sb::Superblock;
    use vfs::dentry::Dentry;
    use vfs::file::FileOps;
    use vfs::inode::{Inode, InodeId, InodeMeta};
    use vfs::stat::{DevId, FileMode, FileType, FsId, Timespec};
    use vfs::superblock::{InodeCache, Superblock as VfsSuperblock};

    struct CountingBackend {
        data: Spinlock<Vec<u8>>,
        sector_size: u32,
        reads: Spinlock<Vec<(u64, u32)>>,
        writes: Spinlock<Vec<(u64, u32)>>,
    }

    impl CountingBackend {
        fn new(sector_count: u32, sector_size: u32) -> Self {
            Self {
                data: Spinlock::new(vec![0; sector_count as usize * sector_size as usize]),
                sector_size,
                reads: Spinlock::new(Vec::new()),
                writes: Spinlock::new(Vec::new()),
            }
        }

        fn seed_block(&self, block: u64, block_size: usize, data: &[u8]) {
            let start = block as usize * block_size;
            self.data.lock()[start..start + data.len()].copy_from_slice(data);
        }

        fn writes(&self) -> Vec<(u64, u32)> {
            self.writes.lock().clone()
        }

        fn reads(&self) -> Vec<(u64, u32)> {
            self.reads.lock().clone()
        }
    }

    impl BlockBackend for CountingBackend {
        fn sector_size(&self) -> u32 {
            self.sector_size
        }

        fn sector_count(&self) -> u64 {
            (self.data.lock().len() / self.sector_size as usize) as u64
        }

        fn read_sectors(
            &self,
            lba: u64,
            count: u32,
            buf: &mut [u8],
        ) -> Result<(), BlockBackendError> {
            let len = self.sector_size as usize * count as usize;
            if buf.len() < len {
                return Err(BlockBackendError::OutOfRange);
            }
            let start = lba as usize * self.sector_size as usize;
            let end = start
                .checked_add(len)
                .ok_or(BlockBackendError::OutOfRange)?;
            let data = self.data.lock();
            if end > data.len() {
                return Err(BlockBackendError::OutOfRange);
            }
            buf[..len].copy_from_slice(&data[start..end]);
            self.reads.lock().push((lba, count));
            Ok(())
        }

        fn write_sectors(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), BlockBackendError> {
            let len = self.sector_size as usize * count as usize;
            if buf.len() < len {
                return Err(BlockBackendError::OutOfRange);
            }
            let start = lba as usize * self.sector_size as usize;
            let end = start
                .checked_add(len)
                .ok_or(BlockBackendError::OutOfRange)?;
            let mut data = self.data.lock();
            if end > data.len() {
                return Err(BlockBackendError::OutOfRange);
            }
            data[start..end].copy_from_slice(&buf[..len]);
            self.writes.lock().push((lba, count));
            Ok(())
        }
    }

    struct BlockingBackend {
        data: StdMutex<Vec<u8>>,
        sector_size: u32,
        block_size: u32,
        gate_block: u64,
        write_started: (StdMutex<bool>, Condvar),
        release_write: (StdMutex<bool>, Condvar),
        read_gate_block: StdMutex<Option<u64>>,
        read_started: (StdMutex<bool>, Condvar),
        release_read: (StdMutex<bool>, Condvar),
        reads: StdMutex<Vec<(u64, u32)>>,
        writes: StdMutex<Vec<(u64, u32)>>,
        fail_read_block: StdMutex<Option<u64>>,
        fail_block: StdMutex<Option<u64>>,
    }

    impl BlockingBackend {
        fn new(block_count: usize, block_size: u32, gate_block: u64) -> Self {
            let sector_size = 512;
            Self {
                data: StdMutex::new(vec![0; block_count * block_size as usize]),
                sector_size,
                block_size,
                gate_block,
                write_started: (StdMutex::new(false), Condvar::new()),
                release_write: (StdMutex::new(false), Condvar::new()),
                read_gate_block: StdMutex::new(None),
                read_started: (StdMutex::new(false), Condvar::new()),
                release_read: (StdMutex::new(false), Condvar::new()),
                reads: StdMutex::new(Vec::new()),
                writes: StdMutex::new(Vec::new()),
                fail_read_block: StdMutex::new(None),
                fail_block: StdMutex::new(None),
            }
        }

        fn seed_block(&self, block: u64, data: &[u8]) {
            let start = block as usize * self.block_size as usize;
            self.data.lock().unwrap()[start..start + data.len()].copy_from_slice(data);
        }

        fn block_data(&self, block: u64) -> Vec<u8> {
            let start = block as usize * self.block_size as usize;
            let end = start + self.block_size as usize;
            self.data.lock().unwrap()[start..end].to_vec()
        }

        fn wait_for_gate(&self) {
            let (lock, cv) = &self.write_started;
            let mut started = lock.lock().unwrap();
            while !*started {
                started = cv.wait(started).unwrap();
            }
        }

        fn release_gate(&self) {
            let (lock, cv) = &self.release_write;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }

        fn gate_read(&self, block: u64) {
            *self.read_gate_block.lock().unwrap() = Some(block);
            *self.read_started.0.lock().unwrap() = false;
            *self.release_read.0.lock().unwrap() = false;
        }

        fn wait_for_read_gate(&self) {
            let (lock, cv) = &self.read_started;
            let mut started = lock.lock().unwrap();
            while !*started {
                started = cv.wait(started).unwrap();
            }
        }

        fn release_read_gate(&self) {
            let (lock, cv) = &self.release_read;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }

        fn reads(&self) -> Vec<(u64, u32)> {
            self.reads.lock().unwrap().clone()
        }

        fn writes(&self) -> Vec<(u64, u32)> {
            self.writes.lock().unwrap().clone()
        }

        fn fail_block(&self, block: u64) {
            *self.fail_block.lock().unwrap() = Some(block);
        }

        fn fail_read_block(&self, block: u64) {
            *self.fail_read_block.lock().unwrap() = Some(block);
        }

        fn clear_read_failure(&self) {
            *self.fail_read_block.lock().unwrap() = None;
        }

        fn clear_failure(&self) {
            *self.fail_block.lock().unwrap() = None;
        }
    }

    impl BlockBackend for BlockingBackend {
        fn sector_size(&self) -> u32 {
            self.sector_size
        }

        fn sector_count(&self) -> u64 {
            (self.data.lock().unwrap().len() / self.sector_size as usize) as u64
        }

        fn read_sectors(
            &self,
            lba: u64,
            count: u32,
            buf: &mut [u8],
        ) -> Result<(), BlockBackendError> {
            let len = self.sector_size as usize * count as usize;
            let start = lba as usize * self.sector_size as usize;
            let end = start
                .checked_add(len)
                .ok_or(BlockBackendError::OutOfRange)?;
            if buf.len() < len || end > self.data.lock().unwrap().len() {
                return Err(BlockBackendError::OutOfRange);
            }
            let first_block = start / self.block_size as usize;
            if self
                .fail_read_block
                .lock()
                .unwrap()
                .is_some_and(|block| block == first_block as u64)
            {
                return Err(BlockBackendError::Io);
            }
            {
                let data = self.data.lock().unwrap();
                buf[..len].copy_from_slice(&data[start..end]);
            }
            self.reads
                .lock()
                .unwrap()
                .push((first_block as u64, len as u32 / self.block_size));
            if self.read_gate_block.lock().unwrap().as_ref() == Some(&(first_block as u64)) {
                let (lock, cv) = &self.read_started;
                *lock.lock().unwrap() = true;
                cv.notify_all();
                let (release_lock, release_cv) = &self.release_read;
                let mut released = release_lock.lock().unwrap();
                while !*released {
                    released = release_cv.wait(released).unwrap();
                }
            }
            Ok(())
        }

        fn write_sectors(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), BlockBackendError> {
            let len = self.sector_size as usize * count as usize;
            let start = lba as usize * self.sector_size as usize;
            let end = start
                .checked_add(len)
                .ok_or(BlockBackendError::OutOfRange)?;
            if buf.len() < len || end > self.data.lock().unwrap().len() {
                return Err(BlockBackendError::OutOfRange);
            }
            let first_block = start / self.block_size as usize;
            if first_block as u64 == self.gate_block {
                let (lock, cv) = &self.write_started;
                *lock.lock().unwrap() = true;
                cv.notify_all();
                let (release_lock, release_cv) = &self.release_write;
                let mut released = release_lock.lock().unwrap();
                while !*released {
                    released = release_cv.wait(released).unwrap();
                }
            }
            if self
                .fail_block
                .lock()
                .unwrap()
                .is_some_and(|b| b == first_block as u64)
            {
                return Err(BlockBackendError::Io);
            }
            let mut data = self.data.lock().unwrap();
            data[start..end].copy_from_slice(&buf[..len]);
            self.writes
                .lock()
                .unwrap()
                .push((first_block as u64, len as u32 / self.block_size));
            Ok(())
        }
    }

    fn alloc_test_state<B: BlockBackend + 'static>(
        backend: Arc<B>,
        block_size: u32,
        blocks_count: u64,
    ) -> FsState {
        let free_blocks = blocks_count.saturating_sub(4);
        let blocks_per_group = blocks_count.min(u32::MAX as u64) as u32;
        let ext_sb = Superblock {
            kind: ExtKind::Ext2,
            inodes_count: 16,
            blocks_count,
            first_data_block: 0,
            block_size,
            log_cluster_size: 0,
            blocks_per_group,
            inodes_per_group: 16,
            inode_size: 128,
            desc_size: 32,
            first_ino: 11,
            s_magic: 0xef53,
            state: 0,
            feature_compat: 0,
            feature_incompat: 0,
            feature_ro_compat: 0,
            uuid: [0; 16],
            volume_name: [0; 16],
            metadata_csum: false,
            csum_seed: 0,
            free_blocks_count: free_blocks,
            reserved_blocks_count: 4,
            free_inodes_count: 16,
            orphan_file_inum: 0,
            journal_inum: 0,
            journal_dev: 0,
            last_orphan: 0,
            encoding: 0,
            mmp_block: 0,
            mmp_update_interval: 0,
            force_read_only: false,
            groups_count: 1,
        };
        let group_desc = vec![GroupDesc {
            block_bitmap: 1,
            inode_bitmap: 2,
            inode_table: 3,
            flags: 0,
            free_blocks_count: free_blocks.min(u32::MAX as u64) as u32,
            free_inodes_count: 16,
            used_dirs_count: 1,
            itable_unused_count: 0,
        }];
        let group_counts = vec![GroupCounts {
            free_blocks: free_blocks.min(u32::MAX as u64) as u32,
            free_inodes: 16,
            used_dirs: 1,
        }];
        let backend: Arc<dyn BlockBackend> = backend;
        FsState {
            backend,
            ext_sb,
            group_desc: Spinlock::new(group_desc),
            group_counts: Spinlock::new(group_counts),
            block_cache: Spinlock::new(BlockCache::new(block_size)),
            sb_free_blocks: AtomicU64::new(free_blocks),
            sb_free_inodes: core::sync::atomic::AtomicU32::new(16),
            block_alloc_hint: AtomicU64::new(0),
            inode_alloc_hint: core::sync::atomic::AtomicU32::new(0),
            alloc_group_dirty: Spinlock::new(vec![0; 1]),
            alloc_sb_dirty: AtomicBool::new(false),
            inode_writeback: Spinlock::new(DirtyInodeState::new()),
            read_only: AtomicBool::new(false),
            mmp: MmpRuntime::new(),
        }
    }

    #[test]
    fn pending_writeback_completion_is_versioned() {
        const BLOCK_SIZE: usize = 64;
        let mut cache = BlockCache::with_capacity(BLOCK_SIZE as u32, 1);
        let first = vec![0x11; BLOCK_SIZE];
        let second = vec![0x22; BLOCK_SIZE];
        let third = vec![0x33; BLOCK_SIZE];

        assert!(cache.insert_wb(10, &first).is_none());
        let old = cache.insert_wb(11, &second).expect("evict first");
        assert_eq!(old.block, 10);
        let newer = cache.insert_wb(10, &third).expect("evict second");
        assert_eq!(newer.block, 11);
        // block 10 已有 owner；新版本只更新 pending，不得启动第二个并行 I/O。
        assert!(cache.insert_wb(12, &vec![0x44; BLOCK_SIZE]).is_none());

        let mut out = vec![0; BLOCK_SIZE];
        assert!(cache.read(10, &mut out));
        assert_eq!(out, third);

        // 旧版本完成不能移除同一块更新的 pending 快照。
        let latest = cache
            .complete_pending(old.block, old.version)
            .expect("old owner must continue with latest version");
        assert!(cache.read(10, &mut out));
        assert_eq!(out, third);

        assert!(
            cache
                .complete_pending(latest.block, latest.version)
                .is_none()
        );
        assert!(!cache.read(10, &mut out));
    }

    #[test]
    fn evicted_snapshot_shares_immutable_buffer_with_pending_writeback() {
        const BLOCK_SIZE: usize = 64;
        let mut cache = BlockCache::with_capacity(BLOCK_SIZE as u32, 1);
        assert!(cache.insert_wb(10, &vec![0x11; BLOCK_SIZE]).is_none());

        let snapshot = cache
            .insert_wb(11, &vec![0x22; BLOCK_SIZE])
            .expect("dirty eviction must produce a writeback owner");
        let pending = cache
            .pending_writebacks
            .get(&snapshot.block)
            .expect("evicted block must remain pending");
        assert!(Arc::ptr_eq(&snapshot.data, &pending.data));
    }

    #[test]
    fn coherence_stamp_wrap_changes_epoch_and_prevents_zero_aba() {
        let mut cache = BlockCache::with_capacity(64, 1);
        cache.coherence_epoch = 7;
        cache.coherence_seq = u64::MAX - 1;
        let before = cache.coherence_stamp_in_range(7, 1);
        assert_eq!(before, (7, 0));

        cache.mark_coherence_change(7);
        assert_eq!(cache.coherence_stamp_in_range(7, 1), (7, u64::MAX));
        // 无关块的下一个事件触发回绕清表，目标块的 max_seq 再次为 0。
        cache.mark_coherence_change(9);

        assert_eq!(cache.coherence_epoch, 8);
        assert_eq!(cache.coherence_seq, 1);
        assert_eq!(cache.coherence_stamp_in_range(7, 1), (8, 0));
        assert_ne!(cache.coherence_stamp_in_range(7, 1), before);
        assert_eq!(cache.coherence_stamp_in_range(9, 1), (8, 1));
        assert_eq!(cache.coherence_stamps.len(), 1);
    }

    #[test]
    fn stale_partial_base_retries_after_newer_version_disappears() {
        const BLOCK_SIZE: usize = 64;
        const INODE_TABLE_BLOCK: u64 = 3;

        let mut cache = BlockCache::with_capacity(BLOCK_SIZE as u32, 1);
        assert!(cache.try_insert_clean(9, &vec![0x90; BLOCK_SIZE]));

        // 模拟 backend 返回旧 inode-table D0，但 cache 已满，
        // read 路径无法把 clean 快照插入。
        let read_stamp = cache.coherence_stamp_in_range(INODE_TABLE_BLOCK, 1);
        let mut stale_base = vec![0u8; BLOCK_SIZE];
        assert!(!cache.try_insert_clean(INODE_TABLE_BLOCK, &stale_base));

        // 另一 inode 的 partial write 发布 D1，随后被驱逐并完成写盘，
        // 因而 merge 时 index/pending 都已不可见，只有 stamp 能证明 D0 过期。
        let mut newer = vec![0u8; BLOCK_SIZE];
        newer[32..36].copy_from_slice(&[2, 2, 2, 2]);
        assert!(cache.insert_wb(INODE_TABLE_BLOCK, &newer).is_none());
        let written = cache
            .insert_wb(10, &vec![0xa0; BLOCK_SIZE])
            .expect("evict newer inode-table version");
        assert_eq!(written.block, INODE_TABLE_BLOCK);
        assert!(
            cache
                .complete_pending(written.block, written.version)
                .is_none()
        );

        assert!(matches!(
            cache.merge_partial_after_read(
                INODE_TABLE_BLOCK,
                0,
                &[1, 1, 1, 1],
                &mut stale_base,
                read_stamp,
            ),
            PartialWriteOutcome::Retry
        ));

        // 重读后的 base 已包含 D1；再合并本 inode 的字节时，
        // 两个 inode 区域都必须保留。
        let retry_stamp = cache.coherence_stamp_in_range(INODE_TABLE_BLOCK, 1);
        let mut retry_base = newer;
        assert!(matches!(
            cache.merge_partial_after_read(
                INODE_TABLE_BLOCK,
                0,
                &[1, 1, 1, 1],
                &mut retry_base,
                retry_stamp,
            ),
            PartialWriteOutcome::Done(_)
        ));
        let mut merged = vec![0u8; BLOCK_SIZE];
        assert!(cache.read(INODE_TABLE_BLOCK, &mut merged));
        assert_eq!(&merged[0..4], &[1, 1, 1, 1]);
        assert_eq!(&merged[32..36], &[2, 2, 2, 2]);
    }

    #[test]
    fn wrapped_snapshot_replaces_pending_without_numeric_comparison() {
        const BLOCK_SIZE: usize = 64;
        let mut cache = BlockCache::with_capacity(BLOCK_SIZE as u32, 1);
        cache.pending_writebacks.insert(
            10,
            PendingBlockWriteback {
                data: Arc::new(vec![0x11; BLOCK_SIZE]),
                version: u64::MAX,
                in_flight: true,
            },
        );
        cache.write_seq = u64::MAX;
        let wrapped_version = cache.next_version();
        assert_eq!(wrapped_version, 1);

        assert!(
            cache
                .remember_pending(DirtyBlockSnapshot {
                    block: 10,
                    data: Arc::new(vec![0x82; BLOCK_SIZE]),
                    version: wrapped_version,
                })
                .is_none()
        );
        let pending = &cache.pending_writebacks[&10];
        assert_eq!(pending.version, 1);
        assert_eq!(pending.data.as_slice(), vec![0x82; BLOCK_SIZE].as_slice());
    }

    #[test]
    fn pending_writeback_is_visible_during_blocked_eviction() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(16, BLOCK_SIZE, 0));
        let old = vec![0u8; BLOCK_SIZE as usize];
        let fresh = vec![0xa5u8; BLOCK_SIZE as usize];
        backend.seed_block(0, &old);

        let base = alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 16);
        let state = Arc::new(FsState {
            block_cache: Spinlock::new(BlockCache::with_capacity(BLOCK_SIZE, 1)),
            ..base
        });
        state.write_block(0, &fresh).expect("seed dirty victim");

        let writer_state = Arc::clone(&state);
        let writer = thread::spawn(move || {
            let data = vec![0x5a; BLOCK_SIZE as usize];
            writer_state.write_block(1, &data)
        });
        backend.wait_for_gate();

        let mut observed = vec![0u8; BLOCK_SIZE as usize];
        state
            .read_block(0, &mut observed)
            .expect("read pending victim");
        assert_eq!(observed, fresh);

        backend.release_gate();
        writer.join().unwrap().expect("flush evicted victim");
        let mut persisted = vec![0u8; BLOCK_SIZE as usize];
        state
            .read_block(0, &mut persisted)
            .expect("read persisted victim");
        assert_eq!(persisted, fresh);
    }

    #[test]
    fn pending_writeback_serializes_newer_version_of_same_block() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(16, BLOCK_SIZE, 0));
        let first = vec![0x31u8; BLOCK_SIZE as usize];
        let latest = vec![0x72u8; BLOCK_SIZE as usize];
        let base = alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 16);
        let state = Arc::new(FsState {
            block_cache: Spinlock::new(BlockCache::with_capacity(BLOCK_SIZE, 1)),
            ..base
        });

        state.write_block(0, &first).expect("seed first version");
        let writer_state = Arc::clone(&state);
        let first_owner =
            thread::spawn(move || writer_state.write_block(1, &vec![0x11; BLOCK_SIZE as usize]));
        backend.wait_for_gate();

        // 在 v1 写盘被阻塞时重新装入 block 0，并再次驱逐为 v2。v2 只能排队给
        // 原 owner，不能启动一个可能先完成的第二 I/O。
        state
            .write_block(0, &latest)
            .expect("publish latest version");
        state
            .write_block(2, &vec![0x22; BLOCK_SIZE as usize])
            .expect("evict latest version");

        backend.release_gate();
        first_owner.join().unwrap().expect("drain both versions");
        assert_eq!(backend.block_data(0), latest);
        assert!(!state.block_cache.lock().has_pending());
    }

    #[test]
    fn failed_pending_writeback_remains_readable() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(10_000, BLOCK_SIZE, u64::MAX));
        backend.fail_block(0);
        let old = vec![0u8; BLOCK_SIZE as usize];
        let fresh = vec![0x7cu8; BLOCK_SIZE as usize];
        backend.seed_block(0, &old);

        let state = alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 10_000);
        let mut cache = BlockCache::with_capacity(BLOCK_SIZE, 1);
        assert!(cache.insert_wb(0, &fresh).is_none());
        let evicted = cache
            .insert_wb(1, &vec![0x44; BLOCK_SIZE as usize])
            .expect("evict failed victim");
        let state = Arc::new(FsState {
            block_cache: Spinlock::new(cache),
            ..state
        });

        assert!(state.flush_one_evicted(&evicted).is_err());
        let mut observed = vec![0u8; BLOCK_SIZE as usize];
        state
            .read_block(0, &mut observed)
            .expect("read failed pending victim");
        assert_eq!(observed, fresh);

        backend.clear_failure();
        state
            .flush_dirty_blocks()
            .expect("sync retries failed pending victim");
        assert!(!state.block_cache.lock().has_pending());
        let mut persisted = vec![0u8; BLOCK_SIZE as usize];
        state
            .read_block(0, &mut persisted)
            .expect("read retried victim");
        assert_eq!(persisted, fresh);
    }

    #[test]
    fn mixed_range_read_overlays_pending_writeback() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(CountingBackend::new(64, 512));
        let fresh = vec![0x6du8; BLOCK_SIZE as usize];
        let state = alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32);
        let mut cache = BlockCache::with_capacity(BLOCK_SIZE, 1);
        assert!(cache.insert_wb(0, &fresh).is_none());
        assert!(
            cache
                .insert_wb(2, &vec![0x22; BLOCK_SIZE as usize])
                .is_some()
        );
        let state = FsState {
            block_cache: Spinlock::new(cache),
            ..state
        };

        let mut observed = vec![0u8; 2 * BLOCK_SIZE as usize];
        state
            .read_blocks(0, 2, &mut observed)
            .expect("read mixed cached range");
        assert_eq!(&observed[..BLOCK_SIZE as usize], fresh.as_slice());
        assert_eq!(
            &observed[BLOCK_SIZE as usize..],
            vec![0; BLOCK_SIZE as usize]
        );
    }

    #[test]
    fn vectored_data_read_skips_clean_cache_and_overlays_dirty_data() {
        const BLOCK_SIZE: usize = 1024;
        let backend = Arc::new(CountingBackend::new(64, 512));
        backend.seed_block(8, BLOCK_SIZE, &vec![0x18; BLOCK_SIZE]);
        backend.seed_block(9, BLOCK_SIZE, &vec![0x19; BLOCK_SIZE]);
        let state = alloc_test_state(Arc::clone(&backend), BLOCK_SIZE as u32, 32);
        state
            .write_data_block(9, &vec![0x99; BLOCK_SIZE])
            .expect("publish newer cached block");

        let mut first = vec![0u8; BLOCK_SIZE];
        let mut second = vec![0u8; BLOCK_SIZE];
        state
            .read_data_blocks_vectored(8, 2, &mut [&mut first, &mut second])
            .expect("scatter read");

        assert_eq!(first, vec![0x18; BLOCK_SIZE]);
        assert_eq!(second, vec![0x99; BLOCK_SIZE]);
        {
            let cache = state.block_cache.lock();
            assert!(
                !cache.contains(8),
                "backend clean data must not enter cache"
            );
            assert!(cache.contains(9), "newer dirty data must remain visible");
        }

        let mut again_first = vec![0u8; BLOCK_SIZE];
        let mut again_second = vec![0u8; BLOCK_SIZE];
        state
            .read_data_blocks_vectored(8, 2, &mut [&mut again_first, &mut again_second])
            .expect("second scatter read");
        assert_eq!(again_first, vec![0x18; BLOCK_SIZE]);
        assert_eq!(again_second, vec![0x99; BLOCK_SIZE]);
        assert_eq!(backend.reads(), vec![(16, 2), (18, 2), (16, 2), (18, 2)]);
    }

    #[test]
    fn failed_inode_writeback_is_retained_for_sync_retry() {
        const INO: u32 = 1;
        const BLOCK_SIZE: u32 = 1024;
        const INODE_TABLE_BLOCK: u64 = 3;

        let backend = Arc::new(BlockingBackend::new(64, BLOCK_SIZE, u64::MAX));
        let state = Arc::new(alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 64));
        let mut raw = RawInode::new(INO, vec![0u8; 128]);
        raw.set_mode(S_IFREG | 0o644);
        raw.set_nlink(1);
        raw.set_size(123);

        let version = state.stage_inode_write(&raw);
        backend.fail_read_block(INODE_TABLE_BLOCK);
        assert_eq!(state.flush_inode_write(INO), Err(BlockBackendError::Io));
        {
            let writeback = state.inode_writeback.lock();
            let pending = writeback
                .pending
                .get(&INO)
                .expect("failed inode snapshot must remain pending");
            assert_eq!(pending.version, version);
            assert_eq!(pending.raw.size(), 123);
            assert!(!pending.in_flight);
        }

        backend.clear_read_failure();
        state.sync_all().expect("sync retries pending inode");
        assert!(state.inode_writeback.lock().pending.is_empty());
        state
            .discard_cached_blocks(INODE_TABLE_BLOCK, 1)
            .expect("drop clean inode-table cache");
        let (reloaded, _) = load_inode(&state, INO).expect("reload persisted inode");
        assert_eq!(reloaded.size, 123);
    }

    #[test]
    fn newer_inode_snapshot_survives_blocked_old_owner() {
        const INO: u32 = 1;
        const BLOCK_SIZE: u32 = 1024;
        const INODE_TABLE_BLOCK: u64 = 3;

        let backend = Arc::new(BlockingBackend::new(64, BLOCK_SIZE, u64::MAX));
        let state = Arc::new(alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 64));
        backend.gate_read(INODE_TABLE_BLOCK);

        let mut old = RawInode::new(INO, vec![0u8; 128]);
        old.set_mode(S_IFREG | 0o644);
        old.set_nlink(1);
        old.set_size(11);
        let old_version = state.stage_inode_write(&old);

        let owner_state = Arc::clone(&state);
        let owner = thread::spawn(move || owner_state.flush_inode_write(INO));
        backend.wait_for_read_gate();

        let mut latest = old.clone();
        latest.set_nlink(2);
        latest.set_size(29);
        let publisher_state = Arc::clone(&state);
        let publisher = thread::spawn(move || publisher_state.publish_inode_write(&latest));

        let mut latest_version = None;
        for _ in 0..10_000 {
            let writeback = state.inode_writeback.lock();
            if let Some(pending) = writeback.pending.get(&INO) {
                if pending.version != old_version
                    && pending.raw.size() == 29
                    && pending.raw.nlink() == 2
                {
                    assert!(pending.in_flight);
                    latest_version = Some(pending.version);
                    break;
                }
            }
            drop(writeback);
            thread::yield_now();
        }
        if latest_version.is_none() {
            backend.release_read_gate();
            let _ = owner.join();
            let _ = publisher.join();
            panic!("direct-style publisher did not replace the old snapshot");
        }

        backend.release_read_gate();
        owner
            .join()
            .unwrap()
            .expect("old owner drains latest version");
        publisher
            .join()
            .unwrap()
            .expect("publisher waits for latest version");
        assert!(state.inode_writeback.lock().pending.is_empty());
        let (cached, _) = load_inode(&state, INO).expect("reload latest cached inode");
        assert_eq!(cached.size, 29);
        assert_eq!(cached.nlink, 2);

        state.flush_dirty_blocks().expect("persist latest inode");
        state
            .discard_cached_blocks(INODE_TABLE_BLOCK, 1)
            .expect("drop clean inode-table cache");
        let (persisted, _) = load_inode(&state, INO).expect("reload latest persisted inode");
        assert_eq!(persisted.size, 29);
        assert_eq!(persisted.nlink, 2);
    }

    #[test]
    fn file_write_publishes_inode_metadata_before_sync() {
        const INO: u32 = 1;
        const BLOCK_SIZE: usize = 1024;

        let backend = Arc::new(CountingBackend::new(128, 512));
        let mut bitmap = vec![0u8; BLOCK_SIZE];
        // 块 0..=3 分别留给引导区/位图/inode table，首个数据块为 4。
        bitmap[0] = 0b0000_1111;
        backend.seed_block(1, BLOCK_SIZE, &bitmap);
        let state = Arc::new(alloc_test_state(
            Arc::clone(&backend),
            BLOCK_SIZE as u32,
            64,
        ));

        let mut raw = RawInode::new(INO, vec![0u8; 128]);
        raw.set_mode(S_IFREG | 0o644);
        raw.set_nlink(1);
        write_raw(&state, &raw).expect("seed inode");
        state.flush_dirty_blocks().expect("persist seed inode");

        let inode_ops = ExtInodeOps::new(Arc::clone(&state), INO, raw.bytes.clone());
        let open_raw = Arc::clone(&inode_ops.raw);
        let fs_id = FsId::new(99);
        let state_for_sb = Arc::clone(&state);
        let sb = VfsSuperblock::new(move |weak_sb| {
            let inode = Inode::new(
                InodeId {
                    fs_id,
                    ino: INO as u64,
                },
                FileType::Regular,
                DevId::new(0, 0),
                BLOCK_SIZE as u32,
                Some(DevId::new(8, 1)),
                InodeMeta {
                    size: 0,
                    nlink: 1,
                    mode: FileMode::new(0o644),
                    uid: Uid(0),
                    gid: Gid(0),
                    atime: Timespec::ZERO,
                    mtime: Timespec::ZERO,
                    ctime: Timespec::ZERO,
                    blocks: 0,
                },
                Arc::new(inode_ops),
                weak_sb.clone(),
            );
            let root_dentry = Dentry::new_positive("", None, Arc::clone(&inode));
            VfsSuperblock {
                fs_type: "ext2-test",
                fs_id,
                dev_id: Some(DevId::new(8, 1)),
                block_size: BLOCK_SIZE as u32,
                name_max: 255,
                root_inode: inode,
                root_dentry,
                inode_cache: InodeCache::new(),
                ops: Box::new(ExtFsSuperblockOps {
                    state: Arc::clone(&state_for_sb),
                }),
                self_weak: weak_sb,
            }
        });
        sb.insert_inode(Arc::clone(&sb.root_inode));

        let mapping_generation = Arc::new(AtomicU64::new(0));
        let file = ExtRegFileOps::new(
            Arc::clone(&state),
            Arc::clone(&sb),
            INO,
            Arc::clone(&open_raw),
            Arc::clone(&mapping_generation),
        );
        assert_eq!(file.write_at(b"test", 0).expect("write file"), 4);
        // 越过 direct 区域写入一个新块时，除了数据块还会新建一级间接块。
        // `i_blocks` 必须同时反映这两个文件系统块（1024B 块即 4 sectors）。
        assert_eq!(
            file.write_at(b"x", (12 * BLOCK_SIZE) as u64)
                .expect("write through indirect block"),
            1
        );

        // 先把文件扩到另一个一级间接块槽位，再由第二个打开句柄缓存中间 hole。
        // 随后第一个句柄在文件现有 size 内补洞：一级间接块号、flags 和 size
        // 都不变化，只有 inode-local mapping generation 能让读句柄丢弃旧映射。
        assert_eq!(
            file.write_at(b"y", (20 * BLOCK_SIZE) as u64)
                .expect("grow sparse indirect file"),
            1
        );
        let reader = ExtRegFileOps::new(
            Arc::clone(&state),
            Arc::clone(&sb),
            INO,
            Arc::clone(&open_raw),
            Arc::clone(&mapping_generation),
        );
        let mut hole = [0xffu8; 1];
        assert_eq!(
            reader
                .read_at(&mut hole, (15 * BLOCK_SIZE) as u64)
                .expect("cache indirect hole"),
            1
        );
        assert_eq!(hole, [0]);
        assert_eq!(reader.map_rebuilds(), 1);

        // 其它 inode/元数据块写入不能再使本文件的映射缓存失效。
        state
            .write_block(50, &vec![0x5a; BLOCK_SIZE])
            .expect("write unrelated filesystem block");
        assert_eq!(
            reader
                .read_at(&mut hole, (15 * BLOCK_SIZE) as u64)
                .expect("reuse mapping after unrelated write"),
            1
        );
        assert_eq!(hole, [0]);
        assert_eq!(reader.map_rebuilds(), 1);

        assert_eq!(
            file.write_at(b"z", (15 * BLOCK_SIZE) as u64)
                .expect("fill cached indirect hole"),
            1
        );
        assert_eq!(
            reader
                .read_at(&mut hole, (15 * BLOCK_SIZE) as u64)
                .expect("read filled indirect hole"),
            1
        );
        assert_eq!(&hole, b"z");
        assert_eq!(reader.map_rebuilds(), 2);

        let mut first_page = vec![0xffu8; 2 * BLOCK_SIZE];
        let mut second_page = vec![0xffu8; 2 * BLOCK_SIZE];
        reader
            .read_pages_at(
                (12 * BLOCK_SIZE) as u64,
                &mut [&mut first_page, &mut second_page],
                3 * BLOCK_SIZE + 1,
            )
            .expect("read mapped blocks, holes and partial tail");
        assert_eq!(first_page[0], b'x');
        assert!(first_page[1..].iter().all(|&byte| byte == 0));
        assert!(second_page[..BLOCK_SIZE].iter().all(|&byte| byte == 0));
        assert_eq!(second_page[BLOCK_SIZE], b'z');
        assert!(second_page[BLOCK_SIZE + 1..].iter().all(|&byte| byte == 0));

        // 模拟 dentry 未缓存/被驱逐：不调用 sync，直接从 inode table 重建。
        // 新 size、块映射和数据必须已经对后续 lookup 可见。
        sb.remove_inode(INO as u64);
        drop(file);
        let (reloaded_meta, reloaded_raw) = load_inode(&state, INO).expect("reload inode");
        assert_eq!(reloaded_meta.size, 20 * BLOCK_SIZE as u64 + 1);
        // 4 个数据块 + 1 个一级间接块，每个 1 KiB 块占 2 个 512B sector。
        assert_eq!(reloaded_meta.blocks_512, 10);
        assert_ne!(&reloaded_raw[0x28..0x2c], &[0u8; 4]);

        let reloaded_file = ExtRegFileOps::new(
            Arc::clone(&state),
            Arc::clone(&sb),
            INO,
            Arc::new(Spinlock::new(RawInode::new(INO, reloaded_raw))),
            Arc::new(AtomicU64::new(0)),
        );
        let mut actual = [0u8; 4];
        assert_eq!(
            reloaded_file
                .read_at(&mut actual, 0)
                .expect("read reloaded file"),
            4
        );
        assert_eq!(&actual, b"test");
    }

    #[test]
    fn direct_write_waits_for_older_pending_victim() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, 4));
        let base = alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32);
        let state = Arc::new(FsState {
            block_cache: Spinlock::new(BlockCache::with_capacity(BLOCK_SIZE, 1)),
            ..base
        });
        let old = vec![0x19; BLOCK_SIZE as usize];
        let direct = vec![0x73; 4 * BLOCK_SIZE as usize];
        state
            .write_data_block(4, &old)
            .expect("publish old dirty block");

        let victim_state = Arc::clone(&state);
        let victim = thread::spawn(move || {
            victim_state.write_data_block(12, &vec![0x2a; BLOCK_SIZE as usize])
        });
        backend.wait_for_gate();

        let direct_state = Arc::clone(&state);
        let direct_data = direct.clone();
        let writer = thread::spawn(move || direct_state.write_data_blocks(4, 4, &direct_data));
        for _ in 0..100 {
            thread::yield_now();
        }
        assert!(
            !state.block_cache.lock().has_active_direct_in_range(4, 4),
            "direct owner must not pass the in-flight old victim"
        );

        backend.release_gate();
        victim.join().unwrap().expect("flush old victim");
        writer.join().unwrap().expect("write direct range");
        assert_eq!(backend.writes(), vec![(4, 1), (4, 4)]);
        for block in 4..8 {
            assert_eq!(backend.block_data(block), vec![0x73; BLOCK_SIZE as usize]);
        }
    }

    #[test]
    fn concurrent_update_survives_older_direct_completion() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, 4));
        let base = alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32);
        let state = Arc::new(FsState {
            block_cache: Spinlock::new(BlockCache::with_capacity(BLOCK_SIZE, 1)),
            ..base
        });
        let direct = vec![0x31; 4 * BLOCK_SIZE as usize];
        let latest = vec![0x72; BLOCK_SIZE as usize];

        let writer_state = Arc::clone(&state);
        let direct_data = direct.clone();
        let writer = thread::spawn(move || writer_state.write_data_blocks(4, 4, &direct_data));
        backend.wait_for_gate();
        state
            .write_data_block(4, &latest)
            .expect("publish update while direct I/O is active");

        backend.release_gate();
        writer.join().unwrap().expect("finish older direct write");
        let mut observed = vec![0; BLOCK_SIZE as usize];
        state
            .read_block(4, &mut observed)
            .expect("read concurrent update");
        assert_eq!(observed, latest);
        {
            let cache = state.block_cache.lock();
            let idx = cache.index[&4];
            assert!(
                cache.slots[idx].dirty,
                "old direct completion must not mark the newer cache version clean"
            );
        }

        state
            .flush_dirty_blocks()
            .expect("persist concurrent update");
        assert_eq!(backend.block_data(4), latest);
    }

    #[test]
    fn discard_waits_for_active_direct_write() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, 4));
        let state = Arc::new(alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32));
        let direct = vec![0x55; 4 * BLOCK_SIZE as usize];

        let writer_state = Arc::clone(&state);
        let writer = thread::spawn(move || writer_state.write_data_blocks(4, 4, &direct));
        backend.wait_for_gate();
        assert!(state.block_cache.lock().has_active_direct_in_range(4, 4));

        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let discard_state = Arc::clone(&state);
        let discard = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = discard_state.discard_cached_blocks(4, 4);
            done_tx.send(result).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "discard/free must not pass an active direct owner"
        );

        backend.release_gate();
        writer.join().unwrap().expect("finish direct write");
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("discard should resume after direct completion")
            .expect("discard range");
        discard.join().unwrap();
        assert!(!state.block_cache.lock().has_active_direct_in_range(4, 4));
    }

    #[test]
    fn stale_single_block_read_retries_after_direct_completion() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, u64::MAX));
        backend.seed_block(4, &vec![0x11; BLOCK_SIZE as usize]);
        backend.gate_read(4);
        let state = Arc::new(alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32));

        let reader_state = Arc::clone(&state);
        let reader = thread::spawn(move || {
            let mut out = vec![0; BLOCK_SIZE as usize];
            reader_state.read_block(4, &mut out).map(|()| out)
        });
        backend.wait_for_read_gate();
        state
            .write_data_blocks(4, 4, &vec![0x88; 4 * BLOCK_SIZE as usize])
            .expect("complete direct write during stale read");
        backend.release_read_gate();

        let observed = reader.join().unwrap().expect("retry stale read");
        assert_eq!(observed, vec![0x88; BLOCK_SIZE as usize]);
        assert_eq!(backend.reads(), vec![(4, 1), (4, 1)]);
    }

    #[test]
    fn backend_read_waits_for_direct_that_is_still_in_flight() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, 4));
        backend.seed_block(4, &vec![0x17; BLOCK_SIZE as usize]);
        backend.gate_read(4);
        let state = Arc::new(alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32));

        let (read_done_tx, read_done_rx) = mpsc::channel();
        let reader_state = Arc::clone(&state);
        let reader = thread::spawn(move || {
            let mut out = vec![0; BLOCK_SIZE as usize];
            let result = reader_state.read_block(4, &mut out).map(|()| out);
            read_done_tx.send(result).unwrap();
        });
        // reader 已从后端复制旧数据，但尚未回到 cache 锁内验证。
        backend.wait_for_read_gate();

        let writer_state = Arc::clone(&state);
        let writer = thread::spawn(move || {
            writer_state.write_data_blocks(4, 4, &vec![0x9b; 4 * BLOCK_SIZE as usize])
        });
        // direct owner 已注册，但后端写仍被阻塞，coherence stamp 尚未推进。
        backend.wait_for_gate();
        backend.release_read_gate();
        assert!(
            read_done_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "reader must discard old backend data while direct I/O remains active"
        );

        backend.release_gate();
        writer.join().unwrap().expect("finish direct write");
        let observed = read_done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader should retry after direct completion")
            .expect("retry in-flight direct read");
        reader.join().unwrap();
        assert_eq!(observed, vec![0x9b; BLOCK_SIZE as usize]);
        assert_eq!(backend.reads(), vec![(4, 1), (4, 1)]);
    }

    #[test]
    fn stale_batch_read_retries_after_direct_completion() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, u64::MAX));
        for block in 4..8 {
            backend.seed_block(block, &vec![0x21; BLOCK_SIZE as usize]);
        }
        backend.gate_read(4);
        let state = Arc::new(alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32));

        let reader_state = Arc::clone(&state);
        let reader = thread::spawn(move || {
            let mut out = vec![0; 4 * BLOCK_SIZE as usize];
            reader_state.read_data_blocks(4, 4, &mut out).map(|()| out)
        });
        backend.wait_for_read_gate();
        state
            .write_data_blocks(4, 4, &vec![0x92; 4 * BLOCK_SIZE as usize])
            .expect("complete direct write during stale range read");
        backend.release_read_gate();

        let observed = reader.join().unwrap().expect("retry stale range read");
        assert_eq!(observed, vec![0x92; 4 * BLOCK_SIZE as usize]);
        assert_eq!(backend.reads(), vec![(4, 4), (4, 4)]);
    }

    #[test]
    fn partial_cache_hit_rechecks_prefix_after_concurrent_write() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, u64::MAX));
        backend.seed_block(4, &vec![0x41; BLOCK_SIZE as usize]);
        backend.seed_block(5, &vec![0x51; BLOCK_SIZE as usize]);
        let state = Arc::new(alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32));

        let mut prefix = vec![0; BLOCK_SIZE as usize];
        state.read_block(4, &mut prefix).expect("预热缓存前缀");
        backend.gate_read(5);

        let reader_state = Arc::clone(&state);
        let reader = thread::spawn(move || {
            let mut out = vec![0; 2 * BLOCK_SIZE as usize];
            reader_state.read_data_blocks(4, 2, &mut out).map(|()| out)
        });
        backend.wait_for_read_gate();
        state
            .write_data_block(4, &vec![0x91; BLOCK_SIZE as usize])
            .expect("并发更新缓存前缀");
        backend.release_read_gate();

        let observed = reader.join().unwrap().expect("完成部分命中读取");
        assert_eq!(
            &observed[..BLOCK_SIZE as usize],
            &vec![0x91; BLOCK_SIZE as usize]
        );
        assert_eq!(
            &observed[BLOCK_SIZE as usize..],
            &vec![0x51; BLOCK_SIZE as usize]
        );
    }

    #[test]
    fn stale_vectored_read_retries_after_direct_completion() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, u64::MAX));
        for block in 4..8 {
            backend.seed_block(block, &vec![0x31; BLOCK_SIZE as usize]);
        }
        backend.gate_read(4);
        let state = Arc::new(alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32));

        let reader_state = Arc::clone(&state);
        let reader = thread::spawn(move || {
            let mut first = vec![0u8; 2 * BLOCK_SIZE as usize];
            let mut second = vec![0u8; 2 * BLOCK_SIZE as usize];
            let result =
                reader_state.read_data_blocks_vectored(4, 4, &mut [&mut first, &mut second]);
            result.map(|()| {
                first.extend_from_slice(&second);
                first
            })
        });
        backend.wait_for_read_gate();
        state
            .write_data_blocks(4, 4, &vec![0xa2; 4 * BLOCK_SIZE as usize])
            .expect("complete direct write during scatter read");
        backend.release_read_gate();

        let observed = reader.join().unwrap().expect("retry scatter read");
        assert_eq!(observed, vec![0xa2; 4 * BLOCK_SIZE as usize]);
        assert_eq!(backend.reads(), vec![(4, 2), (6, 2), (4, 2), (6, 2)]);
    }

    #[test]
    fn unrelated_dirty_write_does_not_retry_backend_read() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, u64::MAX));
        backend.seed_block(4, &vec![0x41; BLOCK_SIZE as usize]);
        backend.gate_read(4);
        let state = Arc::new(alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32));

        let reader_state = Arc::clone(&state);
        let reader = thread::spawn(move || {
            let mut out = vec![0; BLOCK_SIZE as usize];
            reader_state.read_block(4, &mut out).map(|()| out)
        });
        backend.wait_for_read_gate();
        state
            .write_data_block(20, &vec![0xa6; BLOCK_SIZE as usize])
            .expect("publish unrelated dirty block");
        backend.release_read_gate();

        let observed = reader.join().unwrap().expect("finish original read");
        assert_eq!(observed, vec![0x41; BLOCK_SIZE as usize]);
        assert_eq!(backend.reads(), vec![(4, 1)]);
    }

    #[test]
    fn unrelated_pending_completion_does_not_retry_backend_read() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, u64::MAX));
        backend.seed_block(4, &vec![0x43; BLOCK_SIZE as usize]);
        backend.gate_read(4);
        let base = alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32);
        let state = Arc::new(FsState {
            block_cache: Spinlock::new(BlockCache::with_capacity(BLOCK_SIZE, 1)),
            ..base
        });

        let reader_state = Arc::clone(&state);
        let reader = thread::spawn(move || {
            let mut out = vec![0; BLOCK_SIZE as usize];
            reader_state.read_block(4, &mut out).map(|()| out)
        });
        backend.wait_for_read_gate();
        state
            .write_data_block(20, &vec![0xa1; BLOCK_SIZE as usize])
            .expect("publish unrelated victim");
        state
            .write_data_block(21, &vec![0xa2; BLOCK_SIZE as usize])
            .expect("complete unrelated pending writeback");
        backend.release_read_gate();

        let observed = reader.join().unwrap().expect("finish original read");
        assert_eq!(observed, vec![0x43; BLOCK_SIZE as usize]);
        assert_eq!(backend.reads(), vec![(4, 1)]);
        assert_eq!(backend.writes(), vec![(20, 1)]);
    }

    #[test]
    fn unrelated_direct_completion_does_not_retry_backend_read() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, u64::MAX));
        backend.seed_block(4, &vec![0x45; BLOCK_SIZE as usize]);
        backend.gate_read(4);
        let state = Arc::new(alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32));

        let reader_state = Arc::clone(&state);
        let reader = thread::spawn(move || {
            let mut out = vec![0; BLOCK_SIZE as usize];
            reader_state.read_block(4, &mut out).map(|()| out)
        });
        backend.wait_for_read_gate();
        state
            .write_data_blocks(20, 4, &vec![0xb4; 4 * BLOCK_SIZE as usize])
            .expect("complete unrelated direct write");
        backend.release_read_gate();

        let observed = reader.join().unwrap().expect("finish original read");
        assert_eq!(observed, vec![0x45; BLOCK_SIZE as usize]);
        assert_eq!(backend.reads(), vec![(4, 1)]);
        assert_eq!(backend.writes(), vec![(20, 4)]);
    }

    #[test]
    fn wrapped_write_version_preserves_concurrent_direct_update() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, 4));
        backend.fail_block(4);
        let state = Arc::new(alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32));
        state.block_cache.lock().write_seq = u64::MAX - 1;

        let writer_state = Arc::clone(&state);
        let writer = thread::spawn(move || {
            writer_state.write_data_blocks(4, 4, &vec![0x51; 4 * BLOCK_SIZE as usize])
        });
        backend.wait_for_gate();
        let latest = vec![0xd2; BLOCK_SIZE as usize];
        state
            .write_data_block(4, &latest)
            .expect("publish version 1 after direct version MAX");

        backend.release_gate();
        assert_eq!(writer.join().unwrap(), Err(BlockBackendError::Io));
        {
            let cache = state.block_cache.lock();
            assert_eq!(cache.write_seq, 1);
            assert!(!cache.pending_writebacks.contains_key(&4));
            let idx = cache.index[&4];
            assert_eq!(cache.slots[idx].version, 1);
            assert!(cache.slots[idx].dirty);
        }

        let mut observed = vec![0; BLOCK_SIZE as usize];
        state
            .read_block(4, &mut observed)
            .expect("read wrapped concurrent update");
        assert_eq!(observed, latest);
        backend.clear_failure();
        state
            .flush_dirty_blocks()
            .expect("persist wrapped concurrent update");
        assert_eq!(backend.block_data(4), latest);
    }

    #[test]
    fn sync_waits_for_active_direct_write() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(BlockingBackend::new(32, BLOCK_SIZE, 4));
        let state = Arc::new(alloc_test_state(Arc::clone(&backend), BLOCK_SIZE, 32));
        let writer_state = Arc::clone(&state);
        let writer = thread::spawn(move || {
            writer_state.write_data_blocks(4, 4, &vec![0xc3; 4 * BLOCK_SIZE as usize])
        });
        backend.wait_for_gate();

        let (done_tx, done_rx) = mpsc::channel();
        let sync_state = Arc::clone(&state);
        let syncer = thread::spawn(move || {
            done_tx.send(sync_state.flush_dirty_blocks()).unwrap();
        });
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "sync must not report success while direct I/O is active"
        );

        backend.release_gate();
        writer.join().unwrap().expect("finish direct write");
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("sync should resume")
            .expect("sync direct write");
        syncer.join().unwrap();
    }

    #[test]
    fn ensure_block_for_write_skips_new_direct_zero_write() {
        let backend = Arc::new(CountingBackend::new(128, 512));
        let mut bitmap = vec![0u8; 1024];
        // 前 4 个块视为元数据，首个可分配数据块为物理块 4。
        bitmap[0] = 0b0000_1111;
        backend.seed_block(1, 1024, &bitmap);

        let state = alloc_test_state(Arc::clone(&backend), 1024, 64);
        let mut i_block = [0u8; 60];

        let block = map_wr::ensure_block_for_write(&state, &mut i_block, 0, false)
            .expect("allocate direct block");

        assert_eq!(block, BlockAllocState::NewlyAllocated(4));
        assert_eq!(
            u32::from_le_bytes([i_block[0], i_block[1], i_block[2], i_block[3]]),
            4
        );
        // direct 新块由文件写路径覆盖/补零；分配位图先进入 write-back cache，
        // 分配阶段不应同步写块设备。显式 flush 后只能出现位图写入，不能提前
        // 清零物理数据块 4，否则小块覆盖写会被大量无意义 I/O 拖慢。
        assert!(backend.writes().is_empty());
        state.flush_dirty_blocks().expect("flush allocation bitmap");
        let writes = backend.writes();
        assert!(writes.contains(&(2, 2)));
        assert!(!writes.contains(&(8, 2)));
    }

    #[test]
    fn indirect_allocation_reports_only_new_index_blocks() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(CountingBackend::new(4096, 512));
        let mut bitmap = vec![0u8; BLOCK_SIZE as usize];
        bitmap[0] = 0b0000_1111;
        backend.seed_block(1, BLOCK_SIZE as usize, &bitmap);

        let state = alloc_test_state(backend, BLOCK_SIZE, 2048);
        let mut i_block = [0u8; 60];
        let mut scratch = Vec::new();

        let (first_l1, first_l1_indexes) = map_wr::ensure_block_for_write_with_scratch_count(
            &state,
            &mut i_block,
            12,
            false,
            &mut scratch,
        )
        .expect("allocate first L1 data block");
        assert_eq!(first_l1, BlockAllocState::NewlyAllocated(5));
        assert_eq!(first_l1_indexes, 1);

        let (_, next_l1_indexes) = map_wr::ensure_block_for_write_with_scratch_count(
            &state,
            &mut i_block,
            13,
            false,
            &mut scratch,
        )
        .expect("reuse L1 index block");
        assert_eq!(next_l1_indexes, 0);

        let (first_l2, first_l2_indexes) = map_wr::ensure_block_for_write_with_scratch_count(
            &state,
            &mut i_block,
            12 + BLOCK_SIZE / 4,
            false,
            &mut scratch,
        )
        .expect("allocate first L2 data block");
        assert_eq!(first_l2, BlockAllocState::NewlyAllocated(9));
        assert_eq!(first_l2_indexes, 2);
        assert_eq!(map_wr::count_all_blocks(&state, &i_block), Ok(6));
    }

    #[test]
    fn extent_demotion_reports_new_indirect_indexes() {
        const BLOCK_SIZE: u32 = 1024;
        let backend = Arc::new(CountingBackend::new(4096, 512));
        let mut bitmap = vec![0u8; BLOCK_SIZE as usize];
        for bit in 0..24usize {
            bitmap[bit / 8] |= 1 << (bit % 8);
        }
        backend.seed_block(1, BLOCK_SIZE as usize, &bitmap);

        let state = alloc_test_state(backend, BLOCK_SIZE, 2048);
        let mut i_block = [0u8; 60];
        crate::extent_wr::init_empty_root(&mut i_block);
        for (logical, physical) in [(0, 20), (12, 21), (24, 22), (36, 23)] {
            assert!(crate::extent_wr::try_append_leaf(
                &mut i_block,
                logical,
                physical,
                1
            ));
        }
        let mut flags = EXT4_EXTENTS_FL;

        let (converted, new_indexes) =
            crate::extent_wr::demote_preserve_if_extent_count(&state, &mut flags, &mut i_block)
                .expect("demote extent mappings");

        assert!(converted);
        assert_eq!(new_indexes, 1);
        assert_eq!(flags & EXT4_EXTENTS_FL, 0);
        assert_eq!(map_wr::count_all_blocks(&state, &i_block), Ok(5));
    }

    #[test]
    fn statfs_counts_follow_runtime_allocations_and_reserved_blocks() {
        let backend = Arc::new(CountingBackend::new(128, 512));
        let state = alloc_test_state(backend, 1024, 64);

        assert_eq!(state.statfs_counts(), (60, 56, 16));
        state.adjust_sb_free_blocks(-7).expect("扣减空闲块");
        state.adjust_sb_free_inodes(-3).expect("扣减空闲 inode");
        assert_eq!(state.statfs_counts(), (53, 49, 13));
    }

    #[test]
    fn read_blocks_single_block_uses_block_cache() {
        let backend = Arc::new(CountingBackend::new(128, 512));
        let mut data = vec![0u8; 1024];
        data[0] = 0xaa;
        backend.seed_block(8, 1024, &data);

        let state = alloc_test_state(Arc::clone(&backend), 1024, 64);
        let mut first = vec![0u8; 1024];
        let mut second = vec![0u8; 1024];

        state.read_blocks(8, 1, &mut first).expect("first read");
        state.read_blocks(8, 1, &mut second).expect("cached read");

        assert_eq!(first, data);
        assert_eq!(second, data);
        assert_eq!(backend.reads(), vec![(16, 2)]);
    }

    #[test]
    fn partially_cached_range_reads_only_missing_suffix() {
        let backend = Arc::new(CountingBackend::new(128, 512));
        for block in 8..12u64 {
            backend.seed_block(block, 1024, &vec![block as u8; 1024]);
        }
        let state = alloc_test_state(Arc::clone(&backend), 1024, 64);
        let mut first = vec![0u8; 1024];
        let mut range = vec![0u8; 4 * 1024];

        state.read_blocks(8, 1, &mut first).expect("prime cache");
        state.read_blocks(8, 4, &mut range).expect("range read");

        for index in 0..4 {
            assert_eq!(
                &range[index * 1024..(index + 1) * 1024],
                &vec![(8 + index) as u8; 1024]
            );
        }
        assert_eq!(backend.reads(), vec![(16, 2), (18, 6)]);
    }

    #[test]
    fn aligned_sequential_read_prefetches_once_then_hits_cache() {
        const BLOCK_SIZE: usize = 4096;
        const PHYS_START: u64 = 8;
        let backend = Arc::new(CountingBackend::new(256, 512));
        for block in PHYS_START..PHYS_START + 16 {
            backend.seed_block(block, BLOCK_SIZE, &vec![block as u8; BLOCK_SIZE]);
        }
        let state = alloc_test_state(Arc::clone(&backend), BLOCK_SIZE as u32, 32);
        let mut scratch = Vec::new();
        let mut first = vec![0u8; BLOCK_SIZE];
        let mut second = vec![0u8; BLOCK_SIZE];

        read_aligned_blocks(&state, &mut scratch, PHYS_START, 1, 16, &mut first)
            .expect("first sequential read");
        read_aligned_blocks(&state, &mut scratch, PHYS_START + 1, 1, 15, &mut second)
            .expect("cached sequential read");

        assert_eq!(first, vec![PHYS_START as u8; BLOCK_SIZE]);
        assert_eq!(second, vec![(PHYS_START + 1) as u8; BLOCK_SIZE]);
        assert_eq!(backend.reads(), vec![(PHYS_START * 8, 16 * 8)]);
    }

    #[test]
    fn aligned_random_read_keeps_single_block_io() {
        const BLOCK_SIZE: usize = 4096;
        const PHYS_START: u64 = 24;
        let backend = Arc::new(CountingBackend::new(256, 512));
        backend.seed_block(PHYS_START, BLOCK_SIZE, &vec![PHYS_START as u8; BLOCK_SIZE]);
        let state = alloc_test_state(Arc::clone(&backend), BLOCK_SIZE as u32, 32);
        let mut scratch = Vec::new();
        let mut dst = vec![0u8; BLOCK_SIZE];

        read_aligned_blocks(&state, &mut scratch, PHYS_START, 1, 1, &mut dst)
            .expect("random aligned read");

        assert_eq!(dst, vec![PHYS_START as u8; BLOCK_SIZE]);
        assert_eq!(backend.reads(), vec![(PHYS_START * 8, 8)]);
    }

    #[test]
    fn flush_alloc_metadata_writes_only_dirty_group() {
        let backend = Arc::new(CountingBackend::new(256, 512));
        let state = alloc_test_state(Arc::clone(&backend), 1024, 64);
        state.adjust_group_free_blocks(0, -1).expect("mark dirty");
        backend.writes.lock().clear();

        state.flush_alloc_metadata().expect("flush metadata");

        assert_eq!(backend.writes(), vec![(2, 2)]);
    }
}

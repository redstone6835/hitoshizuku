//! FAT 表访问层:FAT12/16/32 统一抽象 + LRU 扇区缓存 + 簇分配/释放。
//!
//! FAT12 条目跨两个 FAT 字节,且奇偶簇的 nibble 顺序不同;FAT16 每项 2 字节;
//! FAT32 每项 4 字节,低 28 位有效。本模块把这些差异隐藏在 [`FatTable::get`] /
//! [`FatTable::set`] 之后。
//!
//! 扇区缓存按 LRU 管理,写路径打 `dirty` 位,卸载/sync 时 [`FatTable::flush_with_mirror`]
//! 把脏扇区写回,必要时镜像到备份 FAT。
//!
//! ## 并发安全
//!
//! 所有 FAT 表访问都经过 `with_slots_lock`,它在一次锁持有内完成扇区加载、修改、
//! 驱逐,消除 FAT12 跨扇区读-改-写过程中锁释放带来的 TOCTOU 窗口。

use alloc::vec::Vec;
use vfs::sync::Spinlock;

use crate::bpb::FatKind;
use crate::state::{BlockBackend, BlockBackendError};

pub(crate) const FAT12_EOC: u32 = 0x0ff8;
pub(crate) const FAT16_EOC: u32 = 0xfff8;
pub(crate) const FAT32_EOC: u32 = 0x0fff_fff8;

struct FatSlot {
    lba: u64,
    data: Vec<u8>,
    dirty: bool,
}

pub(crate) struct FatTable {
    slots: Spinlock<Vec<FatSlot>>,
    capacity: usize,
    kind: FatKind,
    first_fat_sector: u64,
    #[allow(dead_code)]
    fat_size_sectors: u32,
    #[allow(dead_code)]
    num_fats: u32,
    bytes_per_sector: u32,
    total_clusters: u32,
    next_free_hint: Spinlock<u32>,
}

impl FatTable {
    pub(crate) fn new(
        kind: FatKind,
        first_fat_sector: u64,
        fat_size_sectors: u32,
        num_fats: u32,
        bytes_per_sector: u32,
        total_clusters: u32,
        capacity: usize,
        hint: u32,
    ) -> Self {
        Self {
            slots: Spinlock::new(Vec::with_capacity(capacity.max(1))),
            capacity: capacity.max(1),
            kind,
            first_fat_sector,
            fat_size_sectors,
            num_fats,
            bytes_per_sector,
            total_clusters,
            next_free_hint: Spinlock::new(hint.max(2)),
        }
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn kind(&self) -> FatKind {
        self.kind
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn total_clusters(&self) -> u32 {
        self.total_clusters
    }

    #[inline]
    pub(crate) fn eoc_marker(&self) -> u32 {
        match self.kind {
            FatKind::Fat12 => 0x0fff,
            FatKind::Fat16 => 0xffff,
            FatKind::Fat32 => 0x0fff_ffff,
        }
    }

    #[inline]
    pub(crate) fn is_eoc(&self, v: u32) -> bool {
        match self.kind {
            FatKind::Fat12 => v >= FAT12_EOC,
            FatKind::Fat16 => v >= FAT16_EOC,
            FatKind::Fat32 => v >= FAT32_EOC,
        }
    }

    #[inline]
    fn byte_offset(&self, cluster: u32) -> u64 {
        match self.kind {
            FatKind::Fat12 => (cluster as u64) + (cluster as u64 / 2),
            FatKind::Fat16 => (cluster as u64) * 2,
            FatKind::Fat32 => (cluster as u64) * 4,
        }
    }

    #[inline]
    fn validate_cluster(&self, cluster: u32) -> Result<(), BlockBackendError> {
        if cluster < 2 || cluster >= self.total_clusters + 2 {
            Err(BlockBackendError::OutOfRange)
        } else {
            Ok(())
        }
    }

    fn with_slots_lock<R>(
        &self,
        f: impl FnOnce(&mut Vec<FatSlot>) -> Result<R, BlockBackendError>,
    ) -> Result<R, BlockBackendError> {
        let mut guard = self.slots.lock();
        f(&mut guard)
    }

    fn ensure_slot_present<'a>(
        slots: &'a mut Vec<FatSlot>,
        backend: &dyn BlockBackend,
        capacity: usize,
        lba: u64,
        sector_bytes: u32,
    ) -> Result<&'a mut FatSlot, BlockBackendError> {
        if let Some(pos) = slots.iter().position(|s| s.lba == lba) {
            let slot = slots.remove(pos);
            slots.insert(0, slot);
            return Ok(&mut slots[0]);
        }
        let mut data = alloc::vec![0u8; sector_bytes as usize];
        backend.read_sectors(lba, 1, &mut data)?;
        if slots.len() >= capacity {
            if let Some(ev) = slots.pop() {
                if ev.dirty {
                    backend.write_sectors(ev.lba, 1, &ev.data)?;
                }
            }
        }
        slots.insert(
            0,
            FatSlot {
                lba,
                data,
                dirty: false,
            },
        );
        Ok(&mut slots[0])
    }

    fn ensure_slot_present_mut<'a>(
        slots: &'a mut Vec<FatSlot>,
        backend: &dyn BlockBackend,
        capacity: usize,
        lba: u64,
        sector_bytes: u32,
    ) -> Result<&'a mut FatSlot, BlockBackendError> {
        if let Some(pos) = slots.iter().position(|s| s.lba == lba) {
            let mut slot = slots.remove(pos);
            slot.dirty = true;
            slots.insert(0, slot);
            return Ok(&mut slots[0]);
        }
        let mut data = alloc::vec![0u8; sector_bytes as usize];
        backend.read_sectors(lba, 1, &mut data)?;
        if slots.len() >= capacity {
            if let Some(ev) = slots.pop() {
                if ev.dirty {
                    backend.write_sectors(ev.lba, 1, &ev.data)?;
                }
            }
        }
        slots.insert(
            0,
            FatSlot {
                lba,
                data,
                dirty: true,
            },
        );
        Ok(&mut slots[0])
    }

    pub(crate) fn get(
        &self,
        backend: &dyn BlockBackend,
        cluster: u32,
    ) -> Result<u32, BlockBackendError> {
        self.validate_cluster(cluster)?;
        let bps = self.bytes_per_sector as u64;
        let off = self.byte_offset(cluster);
        let sector_no = off / bps;
        let in_sector = (off % bps) as usize;
        let base_lba = self.first_fat_sector + sector_no;

        match self.kind {
            FatKind::Fat12 => self.with_slots_lock(|slots| {
                let slot1 = Self::ensure_slot_present(
                    slots,
                    backend,
                    self.capacity,
                    base_lba,
                    self.bytes_per_sector,
                )?;
                let low = slot1.data[in_sector];
                let next_off = in_sector + 1;
                let high = if next_off < self.bytes_per_sector as usize {
                    slot1.data[next_off]
                } else {
                    let slot2 = Self::ensure_slot_present(
                        slots,
                        backend,
                        self.capacity,
                        base_lba + 1,
                        self.bytes_per_sector,
                    )?;
                    slot2.data[0]
                };
                let raw = u16::from_le_bytes([low, high]) as u32;
                Ok(if cluster & 1 == 0 {
                    raw & 0x0fff
                } else {
                    raw >> 4
                })
            }),
            FatKind::Fat16 => self.with_slots_lock(|slots| {
                let slot = Self::ensure_slot_present(
                    slots,
                    backend,
                    self.capacity,
                    base_lba,
                    self.bytes_per_sector,
                )?;
                Ok(u16::from_le_bytes([slot.data[in_sector], slot.data[in_sector + 1]]) as u32)
            }),
            FatKind::Fat32 => self.with_slots_lock(|slots| {
                let slot = Self::ensure_slot_present(
                    slots,
                    backend,
                    self.capacity,
                    base_lba,
                    self.bytes_per_sector,
                )?;
                Ok(u32::from_le_bytes([
                    slot.data[in_sector],
                    slot.data[in_sector + 1],
                    slot.data[in_sector + 2],
                    slot.data[in_sector + 3],
                ]) & 0x0fff_ffff)
            }),
        }
    }

    pub(crate) fn set(
        &self,
        backend: &dyn BlockBackend,
        cluster: u32,
        value: u32,
    ) -> Result<(), BlockBackendError> {
        self.validate_cluster(cluster)?;
        let bps = self.bytes_per_sector as u64;
        let off = self.byte_offset(cluster);
        let sector_no = off / bps;
        let in_sector = (off % bps) as usize;
        let base_lba = self.first_fat_sector + sector_no;

        match self.kind {
            FatKind::Fat12 => self.with_slots_lock(|slots| {
                let next_off = in_sector + 1;
                let same_sector = next_off < self.bytes_per_sector as usize;

                // 先读出 low/high 两个字节(两次独立借用,避免闭包对 slots 的双借)
                let low = {
                    let slot = Self::ensure_slot_present(
                        slots,
                        backend,
                        self.capacity,
                        base_lba,
                        self.bytes_per_sector,
                    )?;
                    slot.data[in_sector]
                };
                let high = if same_sector {
                    let slot = Self::ensure_slot_present(
                        slots,
                        backend,
                        self.capacity,
                        base_lba,
                        self.bytes_per_sector,
                    )?;
                    slot.data[next_off]
                } else {
                    let slot = Self::ensure_slot_present(
                        slots,
                        backend,
                        self.capacity,
                        base_lba + 1,
                        self.bytes_per_sector,
                    )?;
                    slot.data[0]
                };

                let mut raw = u16::from_le_bytes([low, high]);
                if cluster & 1 == 0 {
                    raw = (raw & 0xf000) | (value as u16 & 0x0fff);
                } else {
                    raw = (raw & 0x000f) | ((value as u16 & 0x0fff) << 4);
                }
                let bytes = raw.to_le_bytes();

                let slot1 = Self::ensure_slot_present_mut(
                    slots,
                    backend,
                    self.capacity,
                    base_lba,
                    self.bytes_per_sector,
                )?;
                slot1.data[in_sector] = bytes[0];
                if same_sector {
                    slot1.data[next_off] = bytes[1];
                } else {
                    let slot2 = Self::ensure_slot_present_mut(
                        slots,
                        backend,
                        self.capacity,
                        base_lba + 1,
                        self.bytes_per_sector,
                    )?;
                    slot2.data[0] = bytes[1];
                }
                Ok(())
            }),
            FatKind::Fat16 => self.with_slots_lock(|slots| {
                let slot = Self::ensure_slot_present_mut(
                    slots,
                    backend,
                    self.capacity,
                    base_lba,
                    self.bytes_per_sector,
                )?;
                let bytes = (value as u16).to_le_bytes();
                slot.data[in_sector] = bytes[0];
                slot.data[in_sector + 1] = bytes[1];
                Ok(())
            }),
            FatKind::Fat32 => self.with_slots_lock(|slots| {
                let slot = Self::ensure_slot_present_mut(
                    slots,
                    backend,
                    self.capacity,
                    base_lba,
                    self.bytes_per_sector,
                )?;
                let prev = u32::from_le_bytes([
                    slot.data[in_sector],
                    slot.data[in_sector + 1],
                    slot.data[in_sector + 2],
                    slot.data[in_sector + 3],
                ]);
                let new = (prev & 0xf000_0000) | (value & 0x0fff_ffff);
                let b = new.to_le_bytes();
                slot.data[in_sector] = b[0];
                slot.data[in_sector + 1] = b[1];
                slot.data[in_sector + 2] = b[2];
                slot.data[in_sector + 3] = b[3];
                Ok(())
            }),
        }
    }

    pub(crate) fn next_cluster(
        &self,
        backend: &dyn BlockBackend,
        cluster: u32,
    ) -> Result<Option<u32>, BlockBackendError> {
        let v = self.get(backend, cluster)?;
        if v < 2 || self.is_eoc(v) {
            Ok(None)
        } else {
            Ok(Some(v))
        }
    }

    pub(crate) fn walk_chain(
        &self,
        backend: &dyn BlockBackend,
        start: u32,
        steps: u32,
    ) -> Result<Option<u32>, BlockBackendError> {
        if start < 2 {
            return Ok(None);
        }
        let mut cur = start;
        for _ in 0..steps {
            match self.next_cluster(backend, cur)? {
                Some(n) => cur = n,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    pub(crate) fn contiguous_run(
        &self,
        backend: &dyn BlockBackend,
        start: u32,
        max_clusters: u32,
    ) -> Result<u32, BlockBackendError> {
        self.validate_cluster(start)?;
        if max_clusters == 0 {
            return Ok(0);
        }

        match self.kind {
            FatKind::Fat12 => self.contiguous_run_slow(backend, start, max_clusters),
            FatKind::Fat16 | FatKind::Fat32 => self.with_slots_lock(|slots| {
                let mut cur = start;
                let mut run = 1u32;
                let entry_bytes = match self.kind {
                    FatKind::Fat16 => 2usize,
                    FatKind::Fat32 => 4usize,
                    FatKind::Fat12 => unreachable!(),
                };
                while run < max_clusters {
                    let bps = self.bytes_per_sector as u64;
                    let off = self.byte_offset(cur);
                    let sector_no = off / bps;
                    let mut in_sector = (off % bps) as usize;
                    let lba = self.first_fat_sector + sector_no;
                    let slot = Self::ensure_slot_present(
                        slots,
                        backend,
                        self.capacity,
                        lba,
                        self.bytes_per_sector,
                    )?;

                    while run < max_clusters && in_sector + entry_bytes <= slot.data.len() {
                        let next = match self.kind {
                            FatKind::Fat16 => {
                                u16::from_le_bytes([slot.data[in_sector], slot.data[in_sector + 1]])
                                    as u32
                            }
                            FatKind::Fat32 => {
                                u32::from_le_bytes([
                                    slot.data[in_sector],
                                    slot.data[in_sector + 1],
                                    slot.data[in_sector + 2],
                                    slot.data[in_sector + 3],
                                ]) & 0x0fff_ffff
                            }
                            FatKind::Fat12 => unreachable!(),
                        };
                        if next < 2 || self.is_eoc(next) || next != cur.saturating_add(1) {
                            return Ok(run);
                        }
                        cur = next;
                        run += 1;
                        in_sector += entry_bytes;
                    }
                }
                Ok(run)
            }),
        }
    }

    fn contiguous_run_slow(
        &self,
        backend: &dyn BlockBackend,
        start: u32,
        max_clusters: u32,
    ) -> Result<u32, BlockBackendError> {
        let mut cur = start;
        let mut run = 1u32;
        while run < max_clusters {
            let Some(next) = self.next_cluster(backend, cur)? else {
                break;
            };
            if next != cur.saturating_add(1) {
                break;
            }
            cur = next;
            run += 1;
        }
        Ok(run)
    }

    pub(crate) fn alloc_cluster(
        &self,
        backend: &dyn BlockBackend,
        prev: Option<u32>,
    ) -> Result<u32, BlockBackendError> {
        let start = (*self.next_free_hint.lock()).max(2);
        let total_with_2 = self.total_clusters.saturating_add(2);
        let mut probed = 0u32;
        let mut c = start;
        let new_cluster;
        loop {
            if probed >= self.total_clusters {
                return Err(BlockBackendError::OutOfRange);
            }
            if c < 2 {
                c = 2;
            }
            if c >= total_with_2 {
                c = 2;
            }
            let v = self.get(backend, c)?;
            if v == 0 {
                self.set(backend, c, self.eoc_marker())?;
                new_cluster = c;
                let next_candidate = c.saturating_add(1);
                let next = if next_candidate >= total_with_2 {
                    2
                } else {
                    next_candidate
                };
                *self.next_free_hint.lock() = next;
                break;
            }
            c = c.wrapping_add(1);
            probed += 1;
        }
        if let Some(p) = prev {
            self.set(backend, p, new_cluster)?;
        }
        Ok(new_cluster)
    }

    pub(crate) fn free_chain(
        &self,
        backend: &dyn BlockBackend,
        head: u32,
    ) -> Result<u32, BlockBackendError> {
        if head < 2 {
            return Ok(0);
        }
        let mut count = 0u32;
        let mut cur = head;
        loop {
            let next = self.get(backend, cur)?;
            self.set(backend, cur, 0)?;
            count += 1;
            if next < 2 || self.is_eoc(next) {
                break;
            }
            cur = next;
        }
        Ok(count)
    }

    pub(crate) fn flush_with_mirror(
        &self,
        backend: &dyn BlockBackend,
        first_fat_sector: u64,
        fat_size_sectors: u64,
        num_fats: u32,
        _bytes_per_sector: u32,
    ) -> Result<(), BlockBackendError> {
        if fat_size_sectors > u64::MAX / num_fats.max(1) as u64 {
            return Err(BlockBackendError::OutOfRange);
        }
        let mut guard = self.slots.lock();
        for slot in guard.iter_mut() {
            if !slot.dirty {
                continue;
            }
            backend.write_sectors(slot.lba, 1, &slot.data)?;
            let in_fat_off = slot.lba - first_fat_sector;
            for i in 1..num_fats as u64 {
                let mirror_lba = first_fat_sector
                    .checked_add(
                        i.checked_mul(fat_size_sectors)
                            .ok_or(BlockBackendError::OutOfRange)?,
                    )
                    .and_then(|v| v.checked_add(in_fat_off))
                    .ok_or(BlockBackendError::OutOfRange)?;
                backend.write_sectors(mirror_lba, 1, &slot.data)?;
            }
            slot.dirty = false;
        }
        Ok(())
    }
}

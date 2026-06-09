//! FAT 目录项读写。
//!
//! 一条 32 字节目录项要么是 SFN(普通 8.3 短名条目),要么是 LFN(`attr=0x0F`)。
//! LFN 条目在 SFN 之前按 order 递减(末位先落盘)排列,中间不能被其他条目打断。
//!
//! 本模块提供:
//! - [`DirCursor`] 顺序遍历器,沿簇链/根目录区域逐 32 字节扫描;
//! - [`DirEntryView`] 是一条聚合好的条目(已合并 LFN);
//! - [`insert_entry`] / [`remove_entry`] / [`update_entry`] 支持目录修改。

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::bpb::FatKind;
use crate::lfn::{
    decode_lfn_entry_fixed, encode_lfn_entry, lfn_checksum, str_to_ucs2, ucs2_to_string,
};
use crate::state::{BlockBackendError, FsState};

pub(crate) const DIR_ENTRY_SIZE: usize = 32;

// SFN 字段偏移
pub(crate) const OFF_NAME: usize = 0;
pub(crate) const OFF_ATTR: usize = 11;
#[allow(dead_code)]
pub(crate) const OFF_NT_RES: usize = 12;
pub(crate) const OFF_CTIME_TENTHS: usize = 13;
pub(crate) const OFF_CTIME: usize = 14;
pub(crate) const OFF_CDATE: usize = 16;
pub(crate) const OFF_ADATE: usize = 18;
pub(crate) const OFF_CLUSTER_HI: usize = 20;
pub(crate) const OFF_MTIME: usize = 22;
pub(crate) const OFF_MDATE: usize = 24;
pub(crate) const OFF_CLUSTER_LO: usize = 26;
pub(crate) const OFF_SIZE: usize = 28;

#[allow(dead_code)]
pub(crate) const ATTR_READ_ONLY: u8 = 0x01;
#[allow(dead_code)]
pub(crate) const ATTR_HIDDEN: u8 = 0x02;
#[allow(dead_code)]
pub(crate) const ATTR_SYSTEM: u8 = 0x04;
pub(crate) const ATTR_VOLUME_ID: u8 = 0x08;
pub(crate) const ATTR_DIRECTORY: u8 = 0x10;
pub(crate) const ATTR_ARCHIVE: u8 = 0x20;
pub(crate) const ATTR_LFN: u8 = 0x0f;
pub(crate) const LFN_END_FLAG: u8 = 0x40;

pub(crate) const ENTRY_FREE: u8 = 0xe5;
pub(crate) const ENTRY_END: u8 = 0x00;

/// 目录中一条已聚合好的条目(已合并 LFN 与 SFN)。
#[derive(Debug, Clone)]
pub(crate) struct DirEntryView {
    pub name: String,
    pub short_name: [u8; 11],
    pub attr: u8,
    pub first_cluster: u32,
    pub size: u32,
    /// 起始物理位置:在目录区域内的 32 字节槽序号(LFN 第一条所在槽)。
    pub slot_start: u32,
    /// SFN 槽序号(LFN 之后的那一条)。
    pub slot_sfn: u32,
}

impl DirEntryView {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIRECTORY != 0
    }
    #[inline]
    pub fn is_volume(&self) -> bool {
        self.attr & ATTR_VOLUME_ID != 0
    }
}

/// 目录的物理后端:FAT12/16 根目录是固定扇区范围;其他都是簇链。
#[derive(Debug, Clone, Copy)]
pub(crate) enum DirBacking {
    /// FAT12/16 根目录:从 LBA `start_lba` 起的 `sector_count` 个扇区。
    FixedRange { start_lba: u64, sector_count: u32 },
    /// 簇链:`first_cluster` 起的链。
    ChainFromCluster(u32),
}

/// 把目录中的"槽序号"(0-based, 32-byte)定位到 LBA + 扇区内偏移。
///
/// 返回 `Ok(None)` 表示槽序号超出当前目录已分配空间,需要扩容(只对簇链有效)。
pub(crate) fn locate_slot(
    state: &FsState,
    backing: DirBacking,
    slot: u32,
) -> Result<Option<(u64, usize)>, BlockBackendError> {
    let bps = state.bytes_per_sector;
    let entries_per_sector = bps / DIR_ENTRY_SIZE as u32;
    let sector_no = slot / entries_per_sector;
    let in_sector = (slot % entries_per_sector) as usize * DIR_ENTRY_SIZE;
    match backing {
        DirBacking::FixedRange {
            start_lba,
            sector_count,
        } => {
            if sector_no >= sector_count {
                return Ok(None);
            }
            Ok(Some((start_lba + sector_no as u64, in_sector)))
        }
        DirBacking::ChainFromCluster(first_cluster) => {
            let sectors_per_cluster = state.sectors_per_cluster;
            let cluster_idx = sector_no / sectors_per_cluster;
            let in_cluster_sector = sector_no % sectors_per_cluster;
            let cur =
                match state
                    .fat
                    .walk_chain(state.backend.as_ref(), first_cluster, cluster_idx)?
                {
                    Some(c) => c,
                    None => return Ok(None),
                };
            let lba = state.cluster_to_lba(cur)? + in_cluster_sector as u64;
            Ok(Some((lba, in_sector)))
        }
    }
}

/// 写一个目录槽。槽必须已存在(预先用 [`ensure_slot`] 扩容)。
pub(crate) fn write_slot(
    state: &FsState,
    backing: DirBacking,
    slot: u32,
    data: &[u8; DIR_ENTRY_SIZE],
) -> Result<(), BlockBackendError> {
    let Some((lba, off)) = locate_slot(state, backing, slot)? else {
        return Err(BlockBackendError::OutOfRange);
    };
    let mut sec = vec![0u8; state.bytes_per_sector as usize];
    state.backend.read_sectors(lba, 1, &mut sec)?;
    sec[off..off + DIR_ENTRY_SIZE].copy_from_slice(data);
    state.backend.write_sectors(lba, 1, &sec)?;
    Ok(())
}

/// 确保槽 `slot` 可以容纳:对于簇链型目录,必要时从 FAT 申请新簇并清零;
/// 对于固定范围(FAT12/16 根目录)无法扩容,会返回 `OutOfRange`。
pub(crate) fn ensure_slot(
    state: &FsState,
    backing: DirBacking,
    slot: u32,
) -> Result<(), BlockBackendError> {
    let bps = state.bytes_per_sector;
    let entries_per_sector = bps / DIR_ENTRY_SIZE as u32;
    let sector_no = slot / entries_per_sector;
    match backing {
        DirBacking::FixedRange { sector_count, .. } => {
            if sector_no >= sector_count {
                Err(BlockBackendError::OutOfRange)
            } else {
                Ok(())
            }
        }
        DirBacking::ChainFromCluster(first_cluster) => {
            let sectors_per_cluster = state.sectors_per_cluster;
            let cluster_idx = sector_no / sectors_per_cluster;
            let max_steps = cluster_idx.min(state.total_clusters);
            let mut cur = first_cluster;
            for _ in 0..max_steps {
                match state.fat.next_cluster(state.backend.as_ref(), cur)? {
                    Some(next) => cur = next,
                    None => {
                        let new_c = state.alloc_cluster(Some(cur))?;
                        zero_cluster(state, new_c)?;
                        cur = new_c;
                    }
                }
            }
            if max_steps < cluster_idx {
                return Err(BlockBackendError::OutOfRange);
            }
            Ok(())
        }
    }
}

fn zero_cluster(state: &FsState, cluster: u32) -> Result<(), BlockBackendError> {
    let lba = state.cluster_to_lba(cluster)?;
    let zero = vec![0u8; (state.bytes_per_sector * state.sectors_per_cluster) as usize];
    state
        .backend
        .write_sectors(lba, state.sectors_per_cluster, &zero)
}

fn scan_dir_sectors<F>(
    state: &FsState,
    backing: DirBacking,
    mut f: F,
) -> Result<u32, BlockBackendError>
where
    F: FnMut(u32, &[u8]) -> Result<bool, BlockBackendError>,
{
    let bps = state.bytes_per_sector as usize;
    let entries_per_sector = state.bytes_per_sector / DIR_ENTRY_SIZE as u32;
    let mut next_slot = 0u32;
    match backing {
        DirBacking::FixedRange {
            start_lba,
            sector_count,
        } => {
            let mut sec = vec![0u8; bps];
            for sec_idx in 0..sector_count {
                state
                    .backend
                    .read_sectors(start_lba + sec_idx as u64, 1, &mut sec)?;
                let slot_base = sec_idx * entries_per_sector;
                next_slot = slot_base + entries_per_sector;
                if !f(slot_base, &sec)? {
                    break;
                }
            }
        }
        DirBacking::ChainFromCluster(first_cluster) => {
            if first_cluster < 2 {
                return Ok(0);
            }
            let mut cluster = first_cluster;
            let mut cluster_index = 0u32;
            let cluster_bytes = (state.bytes_per_sector * state.sectors_per_cluster) as usize;
            let mut cluster_buf = vec![0u8; cluster_bytes];
            loop {
                let lba = state.cluster_to_lba(cluster)?;
                state
                    .backend
                    .read_sectors(lba, state.sectors_per_cluster, &mut cluster_buf)?;
                let cluster_slot_base =
                    cluster_index * state.sectors_per_cluster * entries_per_sector;
                for sec_idx in 0..state.sectors_per_cluster {
                    let off = sec_idx as usize * bps;
                    let slot_base = cluster_slot_base + sec_idx * entries_per_sector;
                    next_slot = slot_base + entries_per_sector;
                    if !f(slot_base, &cluster_buf[off..off + bps])? {
                        return Ok(next_slot);
                    }
                }
                match state.fat.next_cluster(state.backend.as_ref(), cluster)? {
                    Some(next) => {
                        cluster = next;
                        cluster_index = cluster_index.saturating_add(1);
                    }
                    None => break,
                }
            }
        }
    }
    Ok(next_slot)
}

fn parse_dir_entries<F>(
    state: &FsState,
    backing: DirBacking,
    mut f: F,
) -> Result<(), BlockBackendError>
where
    F: FnMut(DirEntryView) -> bool,
{
    let entries_per_sector = state.bytes_per_sector / DIR_ENTRY_SIZE as u32;
    let mut lfn_units: Vec<u16> = Vec::new();
    let mut lfn_checksum_val: u8 = 0;
    let mut lfn_start_slot: Option<u32> = None;

    scan_dir_sectors(state, backing, |slot_base, sec| {
        for idx in 0..entries_per_sector {
            let slot = slot_base + idx;
            let off = idx as usize * DIR_ENTRY_SIZE;
            let entry = &sec[off..off + DIR_ENTRY_SIZE];
            let first = entry[OFF_NAME];
            if first == ENTRY_END {
                return Ok(false);
            }
            let attr = entry[OFF_ATTR];
            if first == ENTRY_FREE {
                lfn_units.clear();
                lfn_start_slot = None;
                continue;
            }
            if attr == ATTR_LFN {
                let order_byte = entry[0];
                let is_last = order_byte & LFN_END_FLAG != 0;
                let order = order_byte & 0x1f;
                let checksum = entry[13];
                if is_last || lfn_start_slot.is_none() {
                    lfn_units.clear();
                    lfn_units.resize((order as usize) * 13, 0);
                    lfn_checksum_val = checksum;
                    lfn_start_slot = Some(slot);
                } else if checksum != lfn_checksum_val {
                    lfn_units.clear();
                    lfn_start_slot = None;
                    continue;
                }
                let out_idx = (order as usize).saturating_sub(1) * 13;
                if out_idx + 13 <= lfn_units.len() {
                    let mut chars13 = [0u16; 13];
                    let _ = decode_lfn_entry_fixed(entry, &mut chars13);
                    lfn_units[out_idx..out_idx + 13].copy_from_slice(&chars13);
                }
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 {
                lfn_units.clear();
                lfn_start_slot = None;
                continue;
            }

            let mut raw_name = [0u8; 11];
            raw_name.copy_from_slice(&entry[OFF_NAME..OFF_NAME + 11]);
            let fc_hi =
                u16::from_le_bytes([entry[OFF_CLUSTER_HI], entry[OFF_CLUSTER_HI + 1]]) as u32;
            let fc_lo =
                u16::from_le_bytes([entry[OFF_CLUSTER_LO], entry[OFF_CLUSTER_LO + 1]]) as u32;
            let first_cluster = if state.kind == FatKind::Fat32 {
                (fc_hi << 16) | fc_lo
            } else {
                fc_lo
            };
            let size = u32::from_le_bytes([
                entry[OFF_SIZE],
                entry[OFF_SIZE + 1],
                entry[OFF_SIZE + 2],
                entry[OFF_SIZE + 3],
            ]);
            let long_name = if lfn_start_slot.is_some() {
                let expect = lfn_checksum(&raw_name);
                if expect == lfn_checksum_val && !lfn_units.is_empty() {
                    let mut end = lfn_units.len();
                    while end > 0 && (lfn_units[end - 1] == 0 || lfn_units[end - 1] == 0xffff) {
                        end -= 1;
                    }
                    Some(ucs2_to_string(&lfn_units[..end]))
                } else {
                    None
                }
            } else {
                None
            };
            let slot_start = lfn_start_slot.unwrap_or(slot);
            let name = long_name.unwrap_or_else(|| decode_sfn(&raw_name));
            let view = DirEntryView {
                name,
                short_name: raw_name,
                attr,
                first_cluster,
                size,
                slot_start,
                slot_sfn: slot,
            };
            lfn_units.clear();
            lfn_start_slot = None;
            if !f(view) {
                return Ok(false);
            }
        }
        Ok(true)
    })?;
    Ok(())
}

/// 提取 SFN 11 字节中的基名+扩展名并拼成 `BASE.EXT`(全大写,尾随空格去除)。
fn decode_sfn(raw: &[u8; 11]) -> String {
    let mut s = String::new();
    let mut base_end = 8;
    while base_end > 0 && raw[base_end - 1] == b' ' {
        base_end -= 1;
    }
    // 首字节 0x05 是合法的(代替 0xE5 防止与"已删"混淆),还原之
    let first = if raw[0] == 0x05 { 0xe5 } else { raw[0] };
    for i in 0..base_end {
        let b = if i == 0 { first } else { raw[i] };
        s.push(b as char);
    }
    let mut ext_end = 11;
    while ext_end > 8 && raw[ext_end - 1] == b' ' {
        ext_end -= 1;
    }
    if ext_end > 8 {
        s.push('.');
        for i in 8..ext_end {
            s.push(raw[i] as char);
        }
    }
    s
}

/// 完整读取目录的所有有效条目(跳过已删除与卷标)。LFN 会被自动合并。
pub(crate) fn read_all_entries(
    state: &FsState,
    backing: DirBacking,
) -> Result<Vec<DirEntryView>, BlockBackendError> {
    let mut out = Vec::new();
    parse_dir_entries(state, backing, |entry| {
        out.push(entry);
        true
    })?;
    Ok(out)
}

/// 在目录中直接查找名称,找到后立即停止扫描。
pub(crate) fn find_entry(
    state: &FsState,
    backing: DirBacking,
    name: &str,
) -> Result<Option<DirEntryView>, BlockBackendError> {
    let mut found = None;
    parse_dir_entries(state, backing, |entry| {
        if entry.name.eq_ignore_ascii_case(name) {
            found = Some(entry);
            false
        } else {
            true
        }
    })?;
    Ok(found)
}

/// 单次遍历同时完成:按名称查找 + 收集所有已用 SFN。
/// 用于 create/mkdir/rename 路径,合并冲突检查与 SFN 冲突避让两次全扫描。
pub(crate) fn find_entry_and_sfns(
    state: &FsState,
    backing: DirBacking,
    name: &str,
) -> Result<(Option<DirEntryView>, Vec<[u8; 11]>), BlockBackendError> {
    let mut found: Option<DirEntryView> = None;
    let mut sfns: Vec<[u8; 11]> = Vec::new();
    parse_dir_entries(state, backing, |entry| {
        sfns.push(entry.short_name);
        if found.is_none() && entry.name.eq_ignore_ascii_case(name) {
            found = Some(entry);
        }
        true
    })?;
    Ok((found, sfns))
}

/// 在目录中查找连续 `need` 个空闲(或 0x00 终止)槽,从 0 号槽开始扫描。
/// 若 backing 是簇链且没找到,会返回 `Ok(end_slot)` 标记需要扩容的起点。
pub(crate) fn find_free_slots(
    state: &FsState,
    backing: DirBacking,
    need: u32,
) -> Result<u32, BlockBackendError> {
    if need == 0 {
        return Err(BlockBackendError::OutOfRange);
    }
    let mut run_start: u32 = 0;
    let mut run_len: u32 = 0;
    let mut found = None;
    let mut end_slot: u32 = 0;
    let entries_per_sector = state.bytes_per_sector / DIR_ENTRY_SIZE as u32;

    scan_dir_sectors(state, backing, |slot_base, sec| {
        for idx in 0..entries_per_sector {
            let slot = slot_base + idx;
            let off = idx as usize * DIR_ENTRY_SIZE;
            let first = sec[off + OFF_NAME];
            end_slot = slot.saturating_add(1);
            if first == ENTRY_END {
                found = Some(if run_len > 0 { run_start } else { slot });
                return Ok(false);
            }
            if first == ENTRY_FREE {
                if run_len == 0 {
                    run_start = slot;
                }
                run_len += 1;
                if run_len >= need {
                    found = Some(run_start);
                    return Ok(false);
                }
            } else {
                run_len = 0;
            }
        }
        Ok(true)
    })?;

    if let Some(slot) = found {
        Ok(slot)
    } else if matches!(backing, DirBacking::FixedRange { .. }) {
        Err(BlockBackendError::OutOfRange)
    } else {
        Ok(end_slot)
    }
}

fn write_slots(
    state: &FsState,
    backing: DirBacking,
    start: u32,
    entries: &[[u8; DIR_ENTRY_SIZE]],
) -> Result<(), BlockBackendError> {
    let bps = state.bytes_per_sector as usize;
    let mut index = 0usize;
    while index < entries.len() {
        let slot = start
            .checked_add(index as u32)
            .ok_or(BlockBackendError::OutOfRange)?;
        let Some((lba, off)) = locate_slot(state, backing, slot)? else {
            return Err(BlockBackendError::OutOfRange);
        };
        let fit = ((bps - off) / DIR_ENTRY_SIZE).min(entries.len() - index);
        let mut sec = vec![0u8; bps];
        state.backend.read_sectors(lba, 1, &mut sec)?;
        for i in 0..fit {
            let dst = off + i * DIR_ENTRY_SIZE;
            sec[dst..dst + DIR_ENTRY_SIZE].copy_from_slice(&entries[index + i]);
        }
        state.backend.write_sectors(lba, 1, &sec)?;
        index += fit;
    }
    Ok(())
}

/// 为 `name` 生成 `(lfn_entries, sfn_11bytes)`。`used_sfns` 是目录里已用的 SFN
/// 集合(用于 `~N` 冲突避免)。
///
/// 规则(简化版,与 Windows 可互操作):
/// - 先尝试生成"朴素 SFN"(全 ASCII 大写 + 8.3 长度),成功则不生成 LFN;
/// - 否则生成 `BASE~N.EXT` 并写 LFN。
pub(crate) fn build_entries_for_name(
    name: &str,
    used_sfns: &[[u8; 11]],
) -> ([u8; 11], Vec<[u8; DIR_ENTRY_SIZE]>) {
    if let Some(sfn) = crate::name::try_plain_sfn(name) {
        if !used_sfns.iter().any(|u| u == &sfn) {
            return (sfn, Vec::new());
        }
    }
    // 需要 LFN + `~N` 混叠
    let mut n: u32 = 1;
    let sfn = loop {
        let candidate = crate::name::build_tilde_sfn(name, n);
        if !used_sfns.iter().any(|u| u == &candidate) {
            break candidate;
        }
        n += 1;
        if n > 9_999_999 {
            // 极端冲突:随便返回,调用方会继续重试
            break candidate;
        }
    };
    let checksum = lfn_checksum(&sfn);

    // LFN 条目数 = ceil(chars / 13)
    let units = str_to_ucs2(name);
    let blocks = (units.len() + 12) / 13;
    let blocks = blocks.max(1);
    let mut entries: Vec<[u8; DIR_ENTRY_SIZE]> = Vec::with_capacity(blocks);
    for order in 1..=blocks {
        let mut chars13 = [0u16; 13];
        let start = (order - 1) * 13;
        for i in 0..13 {
            let idx = start + i;
            chars13[i] = if idx < units.len() {
                units[idx]
            } else if idx == units.len() {
                0x0000
            } else {
                0xffff
            };
        }
        let mut entry = [0u8; DIR_ENTRY_SIZE];
        let mut order_byte = order as u8;
        if order == blocks {
            order_byte |= LFN_END_FLAG;
        }
        encode_lfn_entry(order_byte, &chars13, checksum, &mut entry);
        entries.push(entry);
    }
    // 实际写盘时 LFN 条目要倒序放置:末位 order 在前
    entries.reverse();
    (sfn, entries)
}

/// 新建一条目录项:在 `backing` 里找(或扩容)足够的连续槽,顺序写入
/// 全部 LFN 条目 + SFN 条目。返回 SFN 所在的槽号。
pub(crate) fn insert_new_entry(
    state: &FsState,
    backing: DirBacking,
    lfn_entries: &[[u8; DIR_ENTRY_SIZE]],
    sfn_entry: &[u8; DIR_ENTRY_SIZE],
) -> Result<u32, BlockBackendError> {
    let need = (lfn_entries.len() + 1) as u32;
    let start = find_free_slots(state, backing, need)?;
    ensure_slot(state, backing, start + need - 1)?;
    let mut entries = Vec::with_capacity(need as usize);
    entries.extend_from_slice(lfn_entries);
    entries.push(*sfn_entry);
    let sfn_slot = start + lfn_entries.len() as u32;
    write_slots(state, backing, start, &entries)?;
    Ok(sfn_slot)
}

/// 把 `[slot_start, slot_end]`(闭区间)的条目都打上 `0xE5`。
pub(crate) fn remove_entry_slots(
    state: &FsState,
    backing: DirBacking,
    slot_start: u32,
    slot_end: u32,
) -> Result<(), BlockBackendError> {
    if slot_end < slot_start {
        return Ok(());
    }
    let bps = state.bytes_per_sector as usize;
    let mut slot = slot_start;
    while slot <= slot_end {
        let Some((lba, off)) = locate_slot(state, backing, slot)? else {
            break;
        };
        let remain = (slot_end - slot + 1) as usize;
        let fit = ((bps - off) / DIR_ENTRY_SIZE).min(remain);
        let mut sec = vec![0u8; bps];
        state.backend.read_sectors(lba, 1, &mut sec)?;
        for i in 0..fit {
            sec[off + i * DIR_ENTRY_SIZE] = ENTRY_FREE;
        }
        state.backend.write_sectors(lba, 1, &sec)?;
        slot = slot
            .checked_add(fit as u32)
            .ok_or(BlockBackendError::OutOfRange)?;
    }
    Ok(())
}

/// 原地更新 SFN 条目的 size / first_cluster / 时间字段。
pub(crate) fn update_sfn_metadata(
    state: &FsState,
    backing: DirBacking,
    sfn_slot: u32,
    first_cluster: u32,
    size: u32,
) -> Result<(), BlockBackendError> {
    let Some((lba, off)) = locate_slot(state, backing, sfn_slot)? else {
        return Err(BlockBackendError::OutOfRange);
    };
    let mut sec = vec![0u8; state.bytes_per_sector as usize];
    state.backend.read_sectors(lba, 1, &mut sec)?;
    let buf = &mut sec[off..off + DIR_ENTRY_SIZE];
    let hi = (first_cluster >> 16) as u16;
    let lo = first_cluster as u16;
    buf[OFF_CLUSTER_HI..OFF_CLUSTER_HI + 2].copy_from_slice(&hi.to_le_bytes());
    buf[OFF_CLUSTER_LO..OFF_CLUSTER_LO + 2].copy_from_slice(&lo.to_le_bytes());
    buf[OFF_SIZE..OFF_SIZE + 4].copy_from_slice(&size.to_le_bytes());
    let (t, d, _th) = crate::time::EPOCH_1980;
    buf[OFF_MTIME..OFF_MTIME + 2].copy_from_slice(&t.to_le_bytes());
    buf[OFF_MDATE..OFF_MDATE + 2].copy_from_slice(&d.to_le_bytes());
    buf[OFF_ADATE..OFF_ADATE + 2].copy_from_slice(&d.to_le_bytes());
    state.backend.write_sectors(lba, 1, &sec)
}

/// 构造一个 SFN 条目的 32 字节:给定 11 字节 SFN、属性、first_cluster、size。
pub(crate) fn build_sfn_entry(
    sfn: [u8; 11],
    attr: u8,
    first_cluster: u32,
    size: u32,
) -> [u8; DIR_ENTRY_SIZE] {
    let mut e = [0u8; DIR_ENTRY_SIZE];
    e[OFF_NAME..OFF_NAME + 11].copy_from_slice(&sfn);
    e[OFF_ATTR] = attr;
    let (t, d, th) = crate::time::EPOCH_1980;
    e[OFF_CTIME_TENTHS] = th;
    e[OFF_CTIME..OFF_CTIME + 2].copy_from_slice(&t.to_le_bytes());
    e[OFF_CDATE..OFF_CDATE + 2].copy_from_slice(&d.to_le_bytes());
    e[OFF_ADATE..OFF_ADATE + 2].copy_from_slice(&d.to_le_bytes());
    e[OFF_MTIME..OFF_MTIME + 2].copy_from_slice(&t.to_le_bytes());
    e[OFF_MDATE..OFF_MDATE + 2].copy_from_slice(&d.to_le_bytes());
    let hi = (first_cluster >> 16) as u16;
    let lo = first_cluster as u16;
    e[OFF_CLUSTER_HI..OFF_CLUSTER_HI + 2].copy_from_slice(&hi.to_le_bytes());
    e[OFF_CLUSTER_LO..OFF_CLUSTER_LO + 2].copy_from_slice(&lo.to_le_bytes());
    e[OFF_SIZE..OFF_SIZE + 4].copy_from_slice(&size.to_le_bytes());
    e
}

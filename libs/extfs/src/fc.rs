//! ext4 fast commit(FC)区域回放。
//!
//! 与 Linux `fs/ext4/fast_commit.c` 的 `ext4_fc_replay_scan` /
//! `ext4_fc_replay` 对齐:SCAN 阶段校验 TLV 流并统计有效 tag 数(以最后一个
//! 合法 TAIL 为准),REPLAY 阶段按序应用 dentry/inode/range 增量。
//!
//! 与内核的两处刻意差异(均为安全方向):
//! - ADD_RANGE/DEL_RANGE 只展开 depth-0 的 extent 根;更深的树返回
//!   `Unsupported`,挂载失败并提示先跑 fsck(绝不写坏盘);
//! - 回放期间的块分配安全靠"预先把 ADD_RANGE 物理块置位"保证,等价于
//!   内核回放分配器的排除区域(ext4_fc_replay_check_excluded)。

use alloc::vec::Vec;

use crate::journal::{Journal, Pass, RecoveryStats};
use crate::layout::*;
use crate::state::{BlockBackendError, FsState};

// ── on-disk TLV(fs/ext4/fast_commit.h,全小端) ───────────────────────────

const EXT4_FC_TAG_ADD_RANGE: u16 = 0x0001;
const EXT4_FC_TAG_DEL_RANGE: u16 = 0x0002;
const EXT4_FC_TAG_CREAT: u16 = 0x0003;
const EXT4_FC_TAG_LINK: u16 = 0x0004;
const EXT4_FC_TAG_UNLINK: u16 = 0x0005;
const EXT4_FC_TAG_INODE: u16 = 0x0006;
const EXT4_FC_TAG_PAD: u16 = 0x0007;
const EXT4_FC_TAG_TAIL: u16 = 0x0008;
const EXT4_FC_TAG_HEAD: u16 = 0x0009;

/// `struct ext4_fc_tl` 大小(tag + len)。
const FC_TAG_BASE_LEN: usize = 4;

const FC_REPLAY_STOP: i32 = 0;
const FC_REPLAY_CONTINUE: i32 = 1;

#[inline]
fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
#[inline]
fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// tag value 长度合法性(内核 `ext4_fc_value_len_isvalid`)。
fn value_len_isvalid(tag: u16, len: usize, inode_size: usize) -> bool {
    match tag {
        EXT4_FC_TAG_ADD_RANGE => len == 16,
        EXT4_FC_TAG_DEL_RANGE => len == 12,
        EXT4_FC_TAG_CREAT | EXT4_FC_TAG_LINK | EXT4_FC_TAG_UNLINK => {
            let name_len = len.wrapping_sub(8);
            (1..=255).contains(&name_len)
        }
        EXT4_FC_TAG_INODE => {
            let raw_len = len.wrapping_sub(4);
            (128..=inode_size).contains(&raw_len)
        }
        EXT4_FC_TAG_PAD => true,
        EXT4_FC_TAG_TAIL => len >= 8,
        EXT4_FC_TAG_HEAD => len == 8,
        _ => false,
    }
}

/// ADD_RANGE 记录的物理区域(回放前预先置位,防止目录写路径把它们分走)。
#[derive(Clone, Copy)]
struct FcRegion {
    ino: u32,
    pblk: u64,
    len: u32,
}

/// fast commit 回放状态(对应内核 `struct ext4_fc_replay_state`)。
pub(crate) struct FcReplay {
    expected_off: u32,
    cur_tag: u32,
    num_tags: u32,
    crc: u32,
    started: bool,
    regions: Vec<FcRegion>,
    regions_valid: usize,
    modified_inodes: Vec<u32>,
    /// 已为本次 replay 预置位 ADD_RANGE 物理块。
    pre_marked: bool,
}

impl Default for FcReplay {
    fn default() -> Self {
        Self {
            expected_off: 0,
            cur_tag: 0,
            num_tags: 0,
            crc: 0,
            started: false,
            regions: Vec::new(),
            regions_valid: 0,
            modified_inodes: Vec::new(),
            pre_marked: false,
        }
    }
}

impl FcReplay {
    /// 对应内核 `fc_do_one_pass`:顺序扫描 FC 区域,逐块交给 scan/replay。
    pub(crate) fn do_one_pass(
        &mut self,
        j: &Journal,
        state: &FsState,
        info: &RecoveryStats,
        pass: Pass,
    ) -> Result<(), BlockBackendError> {
        let (fc_first, fc_last) = j.fc_range();
        if fc_first == 0 {
            return Ok(());
        }
        let expected_tid = info.end_transaction;
        let mut buf: Vec<u8> = Vec::new();
        let mut next = fc_first;
        while next <= fc_last {
            j.read_fc_block(state, next, &mut buf)?;
            let off = next - fc_first;
            let ret = self.handle_block(j, state, &buf, pass, off, expected_tid)?;
            next += 1;
            if ret == FC_REPLAY_STOP {
                break;
            }
        }
        Ok(())
    }

    fn handle_block(
        &mut self,
        _j: &Journal,
        state: &FsState,
        block: &[u8],
        pass: Pass,
        off: u32,
        expected_tid: u32,
    ) -> Result<i32, BlockBackendError> {
        match pass {
            Pass::Scan => self.scan_block(state, block, off, expected_tid),
            Pass::Replay => self.replay_block(state, block, expected_tid),
            Pass::Revoke => Ok(FC_REPLAY_CONTINUE),
        }
    }

    // ── SCAN 阶段 ────────────────────────────────────────────────────────

    fn scan_block(
        &mut self,
        state: &FsState,
        block: &[u8],
        off: u32,
        expected_tid: u32,
    ) -> Result<i32, BlockBackendError> {
        if !self.started {
            self.started = true;
            self.cur_tag = 0;
            self.num_tags = 0;
            self.crc = 0;
            self.regions.clear();
            self.regions_valid = 0;
            // 第一个 tag 不是 HEAD:FC 区域没有有效内容,提前结束。
            if block.len() < FC_TAG_BASE_LEN || le16(block, 0) != EXT4_FC_TAG_HEAD {
                return Ok(FC_REPLAY_STOP);
            }
        }
        if off != self.expected_off {
            return Err(BlockBackendError::OutOfRange);
        }
        self.expected_off += 1;

        let inode_size = state.ext_sb.inode_size as usize;
        let end = block.len();
        let mut cur = 0usize;
        while cur + FC_TAG_BASE_LEN <= end {
            let fc_tag = le16(block, cur);
            let fc_len = le16(block, cur + 2) as usize;
            let val = cur + FC_TAG_BASE_LEN;
            if fc_len > end - val || !value_len_isvalid(fc_tag, fc_len, inode_size) {
                return Ok(if self.num_tags != 0 {
                    FC_REPLAY_STOP
                } else {
                    return Err(BlockBackendError::OutOfRange);
                });
            }
            match fc_tag {
                EXT4_FC_TAG_ADD_RANGE => {
                    // 记录物理区域(回放前置位用)。
                    let ex = &block[val + 4..val + 16];
                    let ee_block = le32(ex, 0);
                    let _ = ee_block;
                    let ee_len = le16(ex, 4);
                    let real_len = if ee_len > 0x8000 {
                        (ee_len - 0x8000) as u32
                    } else {
                        ee_len as u32
                    };
                    let pblk = ((le16(ex, 6) as u64) << 32) | le32(ex, 8) as u64;
                    self.regions.push(FcRegion {
                        ino: le32(block, val),
                        pblk,
                        len: real_len,
                    });
                    self.cur_tag += 1;
                    self.crc = crate::crc::update(self.crc, &block[cur..val + fc_len]);
                }
                EXT4_FC_TAG_DEL_RANGE
                | EXT4_FC_TAG_LINK
                | EXT4_FC_TAG_UNLINK
                | EXT4_FC_TAG_CREAT
                | EXT4_FC_TAG_INODE
                | EXT4_FC_TAG_PAD => {
                    self.cur_tag += 1;
                    self.crc = crate::crc::update(self.crc, &block[cur..val + fc_len]);
                }
                EXT4_FC_TAG_TAIL => {
                    self.cur_tag += 1;
                    let tail_tid = le32(block, val);
                    let tail_crc = le32(block, val + 4);
                    // crc 只覆盖到 fc_crc 字段之前(tl + fc_tid)。
                    self.crc = crate::crc::update(self.crc, &block[cur..val + 4]);
                    if tail_tid == expected_tid && tail_crc == self.crc {
                        self.num_tags = self.cur_tag;
                        self.regions_valid = self.regions.len();
                    } else {
                        return Ok(if self.num_tags != 0 {
                            FC_REPLAY_STOP
                        } else {
                            return Err(BlockBackendError::Io);
                        });
                    }
                    self.crc = 0;
                }
                EXT4_FC_TAG_HEAD => {
                    let features = le32(block, val);
                    let head_tid = le32(block, val + 4);
                    if features != 0 {
                        return Err(BlockBackendError::Unsupported);
                    }
                    if head_tid != expected_tid {
                        return Ok(FC_REPLAY_STOP);
                    }
                    self.cur_tag += 1;
                    self.crc = crate::crc::update(self.crc, &block[cur..val + fc_len]);
                }
                _ => {
                    return Ok(if self.num_tags != 0 {
                        FC_REPLAY_STOP
                    } else {
                        return Err(BlockBackendError::OutOfRange);
                    });
                }
            }
            cur = val + fc_len;
        }
        Ok(FC_REPLAY_CONTINUE)
    }

    // ── REPLAY 阶段 ──────────────────────────────────────────────────────

    fn replay_block(
        &mut self,
        state: &FsState,
        block: &[u8],
        expected_tid: u32,
    ) -> Result<i32, BlockBackendError> {
        // 第一次进入 replay:预先把 ADD_RANGE 引用的物理块置位,
        // 防止 dentry 写路径把它们当作空闲块分出去(等价内核的排除区域)。
        if !self.pre_marked {
            self.pre_marked = true;
            let regions: Vec<FcRegion> = self.regions[..self.regions_valid].to_vec();
            for region in regions {
                crate::alloc_mod::mark_blocks_used(state, region.pblk, region.len)?;
                self.record_modified(region.ino);
            }
        }

        if self.num_tags == 0 {
            self.set_bitmaps_and_counters(state)?;
            return Ok(FC_REPLAY_STOP);
        }

        let end = block.len();
        let mut cur = 0usize;
        while cur + FC_TAG_BASE_LEN <= end {
            if self.num_tags == 0 {
                self.set_bitmaps_and_counters(state)?;
                return Ok(FC_REPLAY_STOP);
            }
            let fc_tag = le16(block, cur);
            let fc_len = le16(block, cur + 2) as usize;
            let val = cur + FC_TAG_BASE_LEN;
            if fc_len > end - val {
                return Err(BlockBackendError::OutOfRange);
            }
            self.num_tags -= 1;
            match fc_tag {
                EXT4_FC_TAG_LINK => self.replay_link(state, block, cur, fc_len)?,
                EXT4_FC_TAG_UNLINK => self.replay_unlink(state, block, cur, fc_len)?,
                EXT4_FC_TAG_ADD_RANGE => self.replay_add_range(state, block, val)?,
                EXT4_FC_TAG_CREAT => self.replay_create(state, block, cur, fc_len)?,
                EXT4_FC_TAG_DEL_RANGE => self.replay_del_range(state, block, val)?,
                EXT4_FC_TAG_INODE => self.replay_inode(state, block, val, fc_len)?,
                EXT4_FC_TAG_PAD => {}
                EXT4_FC_TAG_TAIL => {
                    let _ = le32(block, val); // fc_tid 已在 scan 校验
                    let _ = expected_tid;
                }
                EXT4_FC_TAG_HEAD => {}
                _ => return Err(BlockBackendError::OutOfRange),
            }
            cur = val + fc_len;
        }
        Ok(FC_REPLAY_CONTINUE)
    }

    fn record_modified(&mut self, ino: u32) {
        if !self.modified_inodes.contains(&ino) {
            self.modified_inodes.push(ino);
        }
    }

    /// 解析 dentry 类 tag 的 value(parent_ino, ino, dname)。
    fn parse_dentry(block: &[u8], cur: usize, fc_len: usize) -> (u32, u32, &[u8]) {
        let val = cur + FC_TAG_BASE_LEN;
        let parent_ino = le32(block, val);
        let ino = le32(block, val + 4);
        let dname = &block[val + 8..val + fc_len];
        (parent_ino, ino, dname)
    }

    /// LINK:把目录项插回父目录(幂等;已存在则跳过)。
    fn replay_link(
        &mut self,
        state: &FsState,
        block: &[u8],
        cur: usize,
        fc_len: usize,
    ) -> Result<(), BlockBackendError> {
        let (parent_ino, ino, dname) = Self::parse_dentry(block, cur, fc_len);
        self.link_internal(state, parent_ino, ino, dname, true)
    }

    /// 内核 `ext4_fc_replay_link_internal` + `__ext4_link` 的语义:
    /// 插入 entry 并 `inc_nlink`。`bump_nlink` 为 false 时(CREAT)由调用方
    /// 自行 set_nlink(1)。
    fn link_internal(
        &mut self,
        state: &FsState,
        parent_ino: u32,
        ino: u32,
        dname: &[u8],
        bump_nlink: bool,
    ) -> Result<(), BlockBackendError> {
        let Ok(mut parent) = crate::inode_wr::read_raw(state, parent_ino) else {
            return Ok(());
        };
        let Ok(mut target) = crate::inode_wr::read_raw(state, ino) else {
            return Ok(());
        };
        let mut pib = [0u8; 60];
        pib.copy_from_slice(parent.i_block());
        let mut pflags = parent.flags();
        if crate::dir::find_entry_bytes(
            state,
            &pib,
            pflags,
            parent.size(),
            dname,
            Some((parent_ino, parent.generation())),
        )?
        .is_some()
        {
            return Ok(()); // 幂等:entry 已存在
        }
        let file_type = match target.mode() & S_IFMT {
            S_IFREG => DT_REG,
            S_IFDIR => DT_DIR,
            S_IFLNK => DT_LNK,
            S_IFCHR => DT_CHR,
            S_IFBLK => DT_BLK,
            S_IFIFO => DT_FIFO,
            S_IFSOCK => DT_SOCK,
            _ => DT_UNKNOWN,
        };
        let new_size = crate::dir_wr::insert_entry_bytes(
            state,
            parent_ino,
            parent.generation(),
            &mut pib,
            &mut pflags,
            parent.size(),
            ino,
            file_type,
            dname,
        )?;
        parent.i_block_mut().copy_from_slice(&pib);
        parent.set_flags(pflags);
        parent.set_size(new_size);
        crate::inode_wr::write_raw(state, &parent)?;

        if bump_nlink {
            let nl = target.nlink().saturating_add(1);
            target.set_nlink(nl);
            crate::inode_wr::write_raw(state, &target)?;
        }
        self.record_modified(ino);
        Ok(())
    }

    /// UNLINK:删除父目录中的 entry(幂等;不存在则跳过)并 `drop_nlink`。
    fn replay_unlink(
        &mut self,
        state: &FsState,
        block: &[u8],
        cur: usize,
        fc_len: usize,
    ) -> Result<(), BlockBackendError> {
        let (parent_ino, ino, dname) = Self::parse_dentry(block, cur, fc_len);
        let Ok(mut parent) = crate::inode_wr::read_raw(state, parent_ino) else {
            return Ok(());
        };
        let mut pib = [0u8; 60];
        pib.copy_from_slice(parent.i_block());
        let mut pflags = parent.flags();
        if crate::dir::find_entry_bytes(
            state,
            &pib,
            pflags,
            parent.size(),
            dname,
            Some((parent_ino, parent.generation())),
        )?
        .is_none()
        {
            return Ok(()); // 幂等:entry 已不存在
        }
        crate::dir_wr::remove_entry_bytes(
            state,
            parent_ino,
            parent.generation(),
            &pib,
            &mut pflags,
            parent.size(),
            dname,
        )?;
        parent.i_block_mut().copy_from_slice(&pib);
        parent.set_flags(pflags);
        crate::inode_wr::write_raw(state, &parent)?;

        if let Ok(mut target) = crate::inode_wr::read_raw(state, ino) {
            let nl = target.nlink().saturating_sub(1);
            target.set_nlink(nl);
            crate::inode_wr::write_raw(state, &target)?;
            self.record_modified(ino);
        }
        Ok(())
    }

    /// CREAT:标记 inode 已用,必要时初始化新目录,然后链接进父目录。
    fn replay_create(
        &mut self,
        state: &FsState,
        block: &[u8],
        cur: usize,
        fc_len: usize,
    ) -> Result<(), BlockBackendError> {
        let (parent_ino, ino, dname) = Self::parse_dentry(block, cur, fc_len);
        let Ok(mut target) = crate::inode_wr::read_raw(state, ino) else {
            // 内核此处直接失败;保持一致,不允许半个 CREAT。
            return Err(BlockBackendError::Io);
        };
        let is_dir = target.mode() & S_IFMT == S_IFDIR;
        crate::alloc_mod::mark_inode_used(state, ino, is_dir)?;

        if is_dir {
            // ext4_init_new_dir:仅在还没初始化首个数据块时执行。
            let first = le32(target.i_block(), 0);
            if first == 0 {
                let phys = crate::alloc_mod::alloc_block(state)?;
                let has_dir_tail = state.ext_sb.metadata_csum
                    && state.ext_sb.feature_incompat & INCOMPAT_FILETYPE != 0;
                let mut blk = crate::dir_wr::make_init_dir_block(
                    state.ext_sb.block_size,
                    ino,
                    parent_ino,
                    state.ext_sb.feature_incompat & INCOMPAT_FILETYPE != 0,
                    has_dir_tail,
                );
                crate::dir_wr::finish_dir_block(state, ino, target.generation(), &mut blk)?;
                state.write_block(phys, &blk)?;
                target.i_block_mut()[0..4].copy_from_slice(&(phys as u32).to_le_bytes());
                target.set_size(state.ext_sb.block_size as u64);
                target.set_blocks_lo((state.ext_sb.block_size / 512) as u32);
                crate::inode_wr::write_raw(state, &target)?;
            }
        }

        self.link_internal(state, parent_ino, ino, dname, false)?;
        // __ext4_link 之后内核强制 nlink = 1(新建文件)。
        let mut target = crate::inode_wr::read_raw(state, ino)?;
        target.set_nlink(1);
        crate::inode_wr::write_raw(state, &target)?;
        Ok(())
    }

    /// INODE:恢复 inode 快照(不含 extent 文件的 i_block,那由 ADD_RANGE 给)。
    fn replay_inode(
        &mut self,
        state: &FsState,
        block: &[u8],
        val: usize,
        fc_len: usize,
    ) -> Result<(), BlockBackendError> {
        let ino = le32(block, val);
        let fc_raw = &block[val + 4..val + fc_len];
        let inode_len = fc_len - 4;
        let mut raw = crate::inode_wr::read_raw(state, ino)?;
        self.record_modified(ino);

        // 与内核一致:[0, i_block) 与 [i_generation, inode_len) 两段拷贝;
        // i_block 只在 INLINE_DATA 时拷贝,extents 文件由 ADD_RANGE 重建映射。
        raw.bytes[..0x28].copy_from_slice(&fc_raw[..0x28]);
        let off_gen = 100usize; // offsetof(struct ext4_inode, i_generation)
        let copy_end = inode_len.min(raw.bytes.len());
        if copy_end > off_gen {
            raw.bytes[off_gen..copy_end].copy_from_slice(&fc_raw[off_gen..copy_end]);
        }

        let flags = raw.flags();
        if flags & EXT4_EXTENTS_FL != 0 {
            let eh = raw.i_block();
            let bad_magic = eh.len() < 2 || u16::from_le_bytes([eh[0], eh[1]]) != EXT4_EXT_MAGIC;
            if bad_magic {
                crate::extent_wr::init_empty_root(raw.i_block_mut());
            }
        } else if flags & EXT4_INLINE_DATA_FL != 0 {
            raw.i_block_mut().copy_from_slice(&fc_raw[0x28..0x28 + 60]);
        }

        let is_dir = raw.mode() & S_IFMT == S_IFDIR;
        crate::alloc_mod::mark_inode_used(state, ino, is_dir)?;

        // 重算 i_blocks(inline 数据不占块,跳过)。
        if flags & EXT4_INLINE_DATA_FL == 0 {
            let blocks = if raw.flags() & EXT4_EXTENTS_FL != 0 {
                crate::extent_wr::count_tree_blocks(state, raw.i_block())?
            } else {
                crate::map_wr::count_all_blocks(state, raw.i_block())?
            };
            let sectors_per_block = (state.ext_sb.block_size / 512) as u64;
            let sectors = (blocks * sectors_per_block).min(u32::MAX as u64);
            raw.set_blocks_lo(sectors as u32);
        }
        crate::inode_wr::write_raw(state, &raw)?;
        Ok(())
    }

    /// ADD_RANGE:把一段逻辑块映射为给定物理块(仅限 depth-0 extent 根)。
    fn replay_add_range(
        &mut self,
        state: &FsState,
        block: &[u8],
        val: usize,
    ) -> Result<(), BlockBackendError> {
        let ino = le32(block, val);
        let ex = &block[val + 4..val + 16];
        let start = le32(ex, 0);
        let ee_len = le16(ex, 4);
        let unwritten = ee_len > 0x8000;
        let len = if unwritten {
            (ee_len - 0x8000) as u32
        } else {
            ee_len as u32
        };
        let start_pblk = ((le16(ex, 6) as u64) << 32) | le32(ex, 8) as u64;

        let Ok(mut raw) = crate::inode_wr::read_raw(state, ino) else {
            return Ok(());
        };
        if raw.flags() & EXT4_EXTENTS_FL == 0 {
            return Err(BlockBackendError::Unsupported);
        }
        let mut i_block = [0u8; 60];
        i_block.copy_from_slice(raw.i_block());
        match crate::extent_wr::root_depth(&i_block) {
            Some(0) => {}
            _ => return Err(BlockBackendError::Unsupported),
        }
        self.record_modified(ino);

        let mut cur = start;
        let mut remaining = len;
        while remaining > 0 {
            let want_phys = start_pblk + (cur - start) as u64;
            match find_covering_leaf(&i_block, cur) {
                None => {
                    // 洞:插入新 extent,长度截到下一条 extent 起点。
                    let hole = clip_hole(&i_block, cur, remaining)
                        .ok_or(BlockBackendError::Unsupported)?;
                    if !crate::extent_wr::try_append_leaf_uninit(
                        &mut i_block,
                        cur,
                        want_phys,
                        hole,
                        !unwritten,
                    ) {
                        return Err(BlockBackendError::Unsupported);
                    }
                    cur += hole;
                    remaining -= hole;
                }
                Some(ext) => {
                    let cover = ((ext.logical_end() - cur as u64) as u32).min(remaining);
                    let ext_phys = ext.physical + (cur - ext.logical) as u64;
                    if ext_phys != want_phys {
                        // 映射变更:释放旧物理块,改写为新映射。
                        crate::alloc_mod::free_blocks_run(state, ext_phys, cover)?;
                        if !crate::extent_wr::leaf_root_set_range(
                            &mut i_block,
                            cur,
                            cover,
                            want_phys,
                            !unwritten,
                        ) {
                            return Err(BlockBackendError::Unsupported);
                        }
                    } else if ext.initialized != !unwritten {
                        // 同物理块的状态翻转(unwritten <-> written)。
                        if !crate::extent_wr::leaf_root_set_range(
                            &mut i_block,
                            cur,
                            cover,
                            want_phys,
                            !unwritten,
                        ) {
                            return Err(BlockBackendError::Unsupported);
                        }
                    }
                    cur += cover;
                    remaining -= cover;
                }
            }
        }

        raw.i_block_mut().copy_from_slice(&i_block);
        crate::inode_wr::write_raw(state, &raw)?;
        Ok(())
    }

    /// DEL_RANGE:释放并删除一段逻辑块映射(仅限 depth-0 extent 根)。
    fn replay_del_range(
        &mut self,
        state: &FsState,
        block: &[u8],
        val: usize,
    ) -> Result<(), BlockBackendError> {
        let ino = le32(block, val);
        let lblk = le32(block, val + 4);
        let len = le32(block, val + 8);

        let Ok(mut raw) = crate::inode_wr::read_raw(state, ino) else {
            return Ok(());
        };
        if raw.flags() & EXT4_EXTENTS_FL == 0 {
            return Err(BlockBackendError::Unsupported);
        }
        let mut i_block = [0u8; 60];
        i_block.copy_from_slice(raw.i_block());
        match crate::extent_wr::root_depth(&i_block) {
            Some(0) => {}
            _ => return Err(BlockBackendError::Unsupported),
        }
        self.record_modified(ino);

        // 先按现有映射释放物理块(含 unwritten extent)。
        if let Some((entries, _)) = crate::extent_wr::leaf_root_shape(&i_block) {
            let rs = lblk as u64;
            let re = rs + len as u64;
            for index in 0..entries {
                let Some(ext) = crate::extent_wr::read_leaf_extent(&i_block, index) else {
                    break;
                };
                let es = ext.logical as u64;
                let ee = ext.logical_end();
                if ee <= rs || es >= re {
                    continue;
                }
                let os = es.max(rs);
                let oe = ee.min(re);
                let phys = ext.physical + (os - es);
                crate::alloc_mod::free_blocks_run(state, phys, (oe - os) as u32)?;
            }
        }
        if !crate::extent_wr::leaf_root_punch(&mut i_block, lblk, len) {
            return Err(BlockBackendError::Unsupported);
        }
        raw.i_block_mut().copy_from_slice(&i_block);
        crate::inode_wr::write_raw(state, &raw)?;
        Ok(())
    }

    /// 回放收尾:按最终映射把所有引用块置为已用(内核
    /// `ext4_fc_set_bitmaps_and_counters`),并重算 i_blocks。
    fn set_bitmaps_and_counters(&mut self, state: &FsState) -> Result<(), BlockBackendError> {
        let inodes = core::mem::take(&mut self.modified_inodes);
        for ino in inodes {
            let Ok(mut raw) = crate::inode_wr::read_raw(state, ino) else {
                continue;
            };
            let flags = raw.flags();
            if flags & EXT4_INLINE_DATA_FL != 0 {
                continue;
            }
            let blocks = if flags & EXT4_EXTENTS_FL != 0 {
                crate::extent_wr::mark_tree_blocks_used(state, raw.i_block())?
            } else {
                let ranges = crate::map::map_contiguous(state, raw.i_block(), 0, u32::MAX)?;
                let mut total = 0u64;
                for (_, count, phys) in ranges {
                    crate::alloc_mod::mark_blocks_used(state, phys, count)?;
                    total += count as u64;
                }
                total
            };
            let sectors_per_block = (state.ext_sb.block_size / 512) as u64;
            let sectors = (blocks * sectors_per_block).min(u32::MAX as u64);
            raw.set_blocks_lo(sectors as u32);
            crate::inode_wr::write_raw(state, &raw)?;
        }
        Ok(())
    }
}

/// 在 depth-0 extent 根中查找覆盖逻辑块 `cur` 的叶子条目。
fn find_covering_leaf(i_block: &[u8], cur: u32) -> Option<crate::extent_wr::LeafExtent> {
    let (entries, _) = crate::extent_wr::leaf_root_shape(i_block)?;
    for index in 0..entries {
        let ext = crate::extent_wr::read_leaf_extent(i_block, index)?;
        if cur >= ext.logical && (cur as u64) < ext.logical_end() {
            return Some(ext);
        }
    }
    None
}

/// 洞的长度:不超过 `remaining`,也不越过下一条 extent 的起点。
fn clip_hole(i_block: &[u8], cur: u32, remaining: u32) -> Option<u32> {
    let (entries, _) = crate::extent_wr::leaf_root_shape(i_block)?;
    let mut limit = remaining;
    for index in 0..entries {
        let ext = crate::extent_wr::read_leaf_extent(i_block, index)?;
        if ext.logical > cur {
            limit = limit.min(ext.logical - cur);
            break;
        }
    }
    (limit != 0).then_some(limit)
}

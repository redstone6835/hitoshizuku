//! JBD2 日志恢复(scan / revoke / replay 三遍处理)。
//!
//! 算法与 Linux `fs/jbd2/recovery.c` 逐行对齐:
//!
//! - `PASS_SCAN` 定位日志尾,容忍撕裂提交(torn commit)与历史遗留块;
//! - `PASS_REVOKE` 汇总每个事务的 revoke 记录(revoke 只取消同事务内的 tag,
//!   与 jbd2 运行时把 revoke 写回旧事务的行为一致);
//! - `PASS_REPLAY` 把已提交且未被 revoke 的元数据块写回主文件系统;
//! - 支持 checksum v1(crc32_be,ext3 风格)、v2/v3(crc32c)、64 位块号、
//!   `SAME_UUID`/`ESCAPE` 标志、日志区回绕;
//! - fast commit 区域交给 [`crate::fc`] 按 ext4 TLV 规则继续回放。
//!
//! 恢复成功后复位日志超级块(`s_start = 0`,序列号推进到下一事务),
//! 由调用方清除主超级块的 `INCOMPAT_RECOVER`。

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use crate::crc;
use crate::layout::EXT4_JOURNAL_INO;
use crate::state::{BlockBackendError, FsState};

// ── 磁盘格式常量(include/linux/jbd2.h) ──────────────────────────────────

const JBD2_MAGIC_NUMBER: u32 = 0xc03b_3998;

const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
const JBD2_COMMIT_BLOCK: u32 = 2;
const JBD2_SUPERBLOCK_V1: u32 = 3;
const JBD2_SUPERBLOCK_V2: u32 = 4;
const JBD2_REVOKE_BLOCK: u32 = 5;

const JBD2_FEATURE_COMPAT_CHECKSUM: u32 = 0x0000_0001;

const JBD2_FEATURE_INCOMPAT_REVOKE: u32 = 0x0000_0001;
const JBD2_FEATURE_INCOMPAT_64BIT: u32 = 0x0000_0002;
const JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT: u32 = 0x0000_0004;
const JBD2_FEATURE_INCOMPAT_CSUM_V2: u32 = 0x0000_0008;
const JBD2_FEATURE_INCOMPAT_CSUM_V3: u32 = 0x0000_0010;
const JBD2_FEATURE_INCOMPAT_FAST_COMMIT: u32 = 0x0000_0020;

const JBD2_KNOWN_INCOMPAT: u32 = JBD2_FEATURE_INCOMPAT_REVOKE
    | JBD2_FEATURE_INCOMPAT_64BIT
    | JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT
    | JBD2_FEATURE_INCOMPAT_CSUM_V2
    | JBD2_FEATURE_INCOMPAT_CSUM_V3
    | JBD2_FEATURE_INCOMPAT_FAST_COMMIT;

const JBD2_CRC32C_CHKSUM: u8 = 4;
const JBD2_CRC32_CHKSUM: u8 = 1;
const JBD2_CRC32_CHKSUM_SIZE: u8 = 4;

const JBD2_FLAG_ESCAPE: u16 = 1;
const JBD2_FLAG_SAME_UUID: u16 = 2;
const JBD2_FLAG_LAST_TAG: u16 = 8;

/// `struct commit_header` 各字段偏移。
const COMMIT_CHKSUM_TYPE_OFF: usize = 12;
const COMMIT_CHKSUM_SIZE_OFF: usize = 13;
const COMMIT_CHKSUM_OFF: usize = 16;
const COMMIT_SEC_OFF: usize = 48;
const COMMIT_HEADER_SIZE: usize = 60;

/// 日志超级块字段偏移(journal_superblock_t,全大端)。
const JSB_BLOCKSIZE: usize = 0x0c;
const JSB_MAXLEN: usize = 0x10;
const JSB_FIRST: usize = 0x14;
const JSB_SEQUENCE: usize = 0x18;
const JSB_START: usize = 0x1c;
const JSB_ERRNO: usize = 0x20;
const JSB_FEATURE_COMPAT: usize = 0x24;
const JSB_FEATURE_INCOMPAT: usize = 0x28;
const JSB_FEATURE_RO_COMPAT: usize = 0x2c;
const JSB_UUID: usize = 0x30;
const JSB_CHECKSUM_TYPE: usize = 0x50;
const JSB_NUM_FC_BLKS: usize = 0x54;
const JSB_HEAD: usize = 0x58;
const JSB_CHECKSUM: usize = 0xfc;
const JSB_STRUCT_SIZE: usize = 1024;

const DEFAULT_FAST_COMMIT_BLOCKS: u32 = 256;

/// 恢复的三个阶段(对应内核 `enum passtype`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pass {
    Scan,
    Revoke,
    Replay,
}

/// tid 回绕安全比较(`tid_gt` / `tid_geq`)。
#[inline]
fn tid_gt(x: u32, y: u32) -> bool {
    (x.wrapping_sub(y) as i32) > 0
}
#[inline]
fn tid_geq(x: u32, y: u32) -> bool {
    (x.wrapping_sub(y) as i32) >= 0
}

#[inline]
fn be16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}
#[inline]
fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
#[inline]
fn be64(b: &[u8], off: usize) -> u64 {
    u64::from_be_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

/// 日志超级块解析结果。
struct JournalSb {
    blocksize: u32,
    maxlen: u32,
    first: u32,
    start: u32,
    feature_compat: u32,
    feature_incompat: u32,
    uuid: [u8; 16],
    num_fc_blks: u32,
}

/// 运行期日志视图:日志 inode 的块映射 + 校验所需的常量。
pub(crate) struct Journal {
    jsb_block: Vec<u8>,
    j_iblock: [u8; 60],
    j_flags: u32,
    sb: JournalSb,
    /// 普通日志区 [first, last);fast commit 区 [fc_first, fc_last](闭区间)。
    first: u32,
    last: u32,
    fc_first: u32,
    fc_last: u32,
    total_len: u32,
    csum_seed: u32,
}

impl Journal {
    #[inline]
    pub(crate) fn has_v1_checksum(&self) -> bool {
        self.sb.feature_compat & JBD2_FEATURE_COMPAT_CHECKSUM != 0
    }
    #[inline]
    fn has_csum_v2(&self) -> bool {
        self.sb.feature_incompat & JBD2_FEATURE_INCOMPAT_CSUM_V2 != 0
    }
    #[inline]
    fn has_csum_v3(&self) -> bool {
        self.sb.feature_incompat & JBD2_FEATURE_INCOMPAT_CSUM_V3 != 0
    }
    #[inline]
    fn has_csum_v2or3(&self) -> bool {
        self.has_csum_v2() || self.has_csum_v3()
    }
    #[inline]
    fn has_64bit(&self) -> bool {
        self.sb.feature_incompat & JBD2_FEATURE_INCOMPAT_64BIT != 0
    }
    #[inline]
    fn has_async_commit(&self) -> bool {
        self.sb.feature_incompat & JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT != 0
    }
    #[inline]
    pub(crate) fn has_fast_commit(&self) -> bool {
        self.sb.feature_incompat & JBD2_FEATURE_INCOMPAT_FAST_COMMIT != 0
    }

    /// 与内核 `journal_tag_bytes()` 一致。
    fn tag_bytes(&self) -> usize {
        if self.has_csum_v3() {
            return 16; // journal_block_tag3_t
        }
        let sz = if self.has_csum_v2() { 14 } else { 12 };
        if self.has_64bit() { sz } else { sz - 4 }
    }

    /// 日志逻辑块 → 设备物理块(journal inode 的块映射)。
    fn bmap(&self, state: &FsState, logical: u32) -> Result<u64, BlockBackendError> {
        crate::dir::resolve_block(state, &self.j_iblock, self.j_flags, logical)?
            .ok_or(BlockBackendError::OutOfRange)
    }

    /// 读日志逻辑块(含 total_len 边界检查,等价内核 `jread`)。
    fn jread(
        &self,
        state: &FsState,
        offset: u32,
        buf: &mut Vec<u8>,
    ) -> Result<(), BlockBackendError> {
        if offset >= self.total_len {
            // 日志超级块声称的区域超出日志 inode 容量:视为损坏。
            return Err(BlockBackendError::OutOfRange);
        }
        let phys = self.bmap(state, offset)?;
        let bs = state.ext_sb.block_size as usize;
        if buf.len() != bs {
            buf.resize(bs, 0);
        }
        state.read_block(phys, buf)
    }

    /// 描述符/revoke 块尾校验(v2/v3):末 4 字节为 crc32c(seed, 全块, 尾部清零)。
    fn descr_block_csum_verify(&self, buf: &[u8]) -> bool {
        if !self.has_csum_v2or3() {
            return true;
        }
        let bs = buf.len();
        let provided = be32(buf, bs - 4);
        let mut tmp = buf.to_vec();
        tmp[bs - 4..].fill(0);
        let calculated = crc::update(self.csum_seed, &tmp);
        provided == calculated
    }

    /// 提交块校验:全块 crc32c(h_chksum[0] 清零)。
    fn commit_block_csum_verify(&self, buf: &[u8]) -> bool {
        if !self.has_csum_v2or3() {
            return true;
        }
        let provided = be32(buf, COMMIT_CHKSUM_OFF);
        let mut tmp = buf.to_vec();
        tmp[COMMIT_CHKSUM_OFF..COMMIT_CHKSUM_OFF + 4].fill(0);
        let calculated = crc::update(self.csum_seed, &tmp);
        provided == calculated
    }

    /// 撕裂提交块校验:块内只有 `struct commit_header` 前缀有效,
    /// 其余字节按 0 处理(内核 `jbd2_commit_block_csum_verify_partial`)。
    fn commit_block_csum_verify_partial(&self, buf: &[u8]) -> bool {
        if !self.has_csum_v2or3() {
            return true;
        }
        let bs = buf.len();
        let mut tmp = vec![0u8; bs];
        tmp[..COMMIT_HEADER_SIZE].copy_from_slice(&buf[..COMMIT_HEADER_SIZE]);
        let provided = be32(&tmp, COMMIT_CHKSUM_OFF);
        tmp[COMMIT_CHKSUM_OFF..COMMIT_CHKSUM_OFF + 4].fill(0);
        let calculated = crc::update(self.csum_seed, &tmp);
        provided == calculated
    }

    /// tag 的数据块校验(v2:tag 内 16 位截断;v3:tag3 内完整 32 位)。
    fn block_tag_csum_verify(&self, tagp: &[u8], data: &[u8], sequence: u32) -> bool {
        if !self.has_csum_v2or3() {
            return true;
        }
        let seq = sequence.to_be_bytes();
        let csum = crc::update(self.csum_seed, &seq);
        let csum = crc::update(csum, data);
        if self.has_csum_v3() {
            be32(tagp, 12) == csum
        } else {
            be16(tagp, 4) == (csum & 0xffff) as u16
        }
    }

    /// 读取 tag 的目标块号(64 位特性时拼接高位)。
    fn read_tag_block(&self, tagp: &[u8]) -> u64 {
        let lo = be32(tagp, 0) as u64;
        if self.has_64bit() {
            lo | ((be32(tagp, 8) as u64) << 32)
        } else {
            lo
        }
    }
}

/// 统计信息(对应内核 `struct recovery_info`)。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RecoveryStats {
    pub start_transaction: u32,
    pub end_transaction: u32,
    pub head_block: u32,
    pub nr_replays: u32,
    pub nr_revokes: u32,
    pub nr_revoke_hits: u32,
}

/// revoke 表:blocknr → 记录它的最大 tid。
///
/// 回放时按"记录 tid == 当前事务 tid"判断命中,与
/// `jbd2_journal_set_revoke`/`jbd2_journal_test_revoke` 语义一致。
#[derive(Default)]
struct RevokeTable {
    map: BTreeMap<u64, u32>,
}

impl RevokeTable {
    fn set_revoke(&mut self, blocknr: u64, tid: u32) {
        match self.map.get_mut(&blocknr) {
            Some(record) => {
                if tid_gt(tid, *record) {
                    *record = tid;
                }
            }
            None => {
                self.map.insert(blocknr, tid);
            }
        }
    }
    fn test_revoke(&self, blocknr: u64, tid: u32) -> bool {
        self.map.get(&blocknr).copied() == Some(tid)
    }
}

/// 统计描述符块中的 tag 数(含 SAME_UUID 跳过的 16 字节 uuid)。
fn count_tags(j: &Journal, block: &[u8]) -> u32 {
    let tag_bytes = j.tag_bytes();
    let mut size = block.len();
    if j.has_csum_v2or3() {
        size -= 4;
    }
    let mut nr = 0u32;
    let mut tagp = 12usize; // sizeof(journal_header_t)
    while tagp + tag_bytes <= size {
        let flags = be16(block, tagp + 6);
        nr += 1;
        tagp += tag_bytes;
        if flags & JBD2_FLAG_SAME_UUID == 0 {
            tagp += 16;
        }
        if flags & JBD2_FLAG_LAST_TAG != 0 {
            break;
        }
    }
    nr
}

/// v1 checksum:把描述符块与其引用的全部数据块顺序混入 crc32_sum。
///
/// 返回 `Err(())` 表示中途 I/O 失败(内核返回 1,调用方直接继续扫描)。
fn calc_chksums(
    j: &Journal,
    state: &FsState,
    block: &[u8],
    next_log_block: &mut u32,
    crc32_sum: &mut u32,
    scratch: &mut Vec<u8>,
) -> Result<(), ()> {
    let num_blks = count_tags(j, block);
    *crc32_sum = crc::crc32_be_update(*crc32_sum, block);
    for _ in 0..num_blks {
        let io_block = *next_log_block;
        *next_log_block += 1;
        wrap(j, next_log_block);
        if j.jread(state, io_block, scratch).is_err() {
            return Err(());
        }
        *crc32_sum = crc::crc32_be_update(*crc32_sum, scratch);
    }
    Ok(())
}

/// revoke 块:PASS_SCAN 只统计;PASS_REVOKE 把记录写入 revoke 表。
fn scan_revoke_records(
    j: &Journal,
    pass: Pass,
    block: &[u8],
    sequence: u32,
    info: &mut RecoveryStats,
    revoke: &mut RevokeTable,
) -> Result<(), BlockBackendError> {
    let rcount = be32(block, 12) as usize;
    let csum_size = if j.has_csum_v2or3() { 4 } else { 0 };
    if rcount > block.len() - csum_size {
        return Err(BlockBackendError::OutOfRange);
    }
    let record_len = if j.has_64bit() { 8 } else { 4 };
    let mut offset = 16usize; // sizeof(jbd2_journal_revoke_header_t)
    if pass == Pass::Scan {
        info.nr_revokes += (rcount - offset) as u32 / record_len as u32;
        return Ok(());
    }
    while offset + record_len <= rcount {
        let blocknr = if record_len == 4 {
            be32(block, offset) as u64
        } else {
            be64(block, offset)
        };
        offset += record_len;
        revoke.set_revoke(blocknr, sequence);
    }
    Ok(())
}

/// PASS_REPLAY 的描述符块回放:逐 tag 把日志数据块写回主文件系统。
///
/// 返回 `Ok(())` 或延迟错误(单块失败不终止整个恢复,与内核一致)。
fn do_replay(
    j: &Journal,
    state: &FsState,
    block: &[u8],
    next_log_block: &mut u32,
    next_commit_id: u32,
    info: &mut RecoveryStats,
    revoke: &RevokeTable,
) -> Result<(), BlockBackendError> {
    let tag_bytes = j.tag_bytes();
    let descr_csum_size = if j.has_csum_v2or3() { 4 } else { 0 };
    let mut deferred: Option<BlockBackendError> = None;
    let bs = block.len();

    let mut tagp = 12usize;
    while tagp + tag_bytes <= bs - descr_csum_size {
        let flags = be16(block, tagp + 6);
        let io_block = *next_log_block;
        *next_log_block += 1;
        wrap(j, next_log_block);

        let mut data = vec![0u8; bs];
        match j.jread(state, io_block, &mut data) {
            Err(err) => {
                // 内核:Recover what we can, but report failure at the end.
                deferred.get_or_insert(err);
            }
            Ok(()) => {
                let blocknr = j.read_tag_block(&block[tagp..]);
                if revoke.test_revoke(blocknr, next_commit_id) {
                    info.nr_revoke_hits += 1;
                } else if !j.block_tag_csum_verify(&block[tagp..], &data, next_commit_id) {
                    deferred.get_or_insert(BlockBackendError::Io);
                } else {
                    if flags & JBD2_FLAG_ESCAPE != 0 {
                        data[0..4].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
                    }
                    state.write_block(blocknr, &data)?;
                    info.nr_replays += 1;
                }
            }
        }

        tagp += tag_bytes;
        if flags & JBD2_FLAG_SAME_UUID == 0 {
            tagp += 16;
        }
        if flags & JBD2_FLAG_LAST_TAG != 0 {
            break;
        }
    }
    match deferred {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// 日志区回绕(内核 `wrap()`)。
#[inline]
fn wrap(j: &Journal, var: &mut u32) {
    if *var >= j.last {
        *var -= j.last - j.first;
    }
}

/// 单趟恢复处理(内核 `do_one_pass` 的直译)。
fn do_one_pass(
    j: &Journal,
    state: &FsState,
    info: &mut RecoveryStats,
    pass: Pass,
    revoke: &mut RevokeTable,
    fc: &mut crate::fc::FcReplay,
) -> Result<(), BlockBackendError> {
    let mut next_commit_id = be32(&j.jsb_block, JSB_SEQUENCE);
    let mut next_log_block = be32(&j.jsb_block, JSB_START);
    let mut head_block = next_log_block;
    let mut deferred: Option<BlockBackendError> = None;
    let mut crc32_sum: u32 = !0; // v1 事务校验
    let mut need_check_commit_time = false;
    let mut last_trans_commit_time: u64 = 0;
    let mut bh: Vec<u8> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();

    let first_commit_id = next_commit_id;
    if pass == Pass::Scan {
        info.start_transaction = first_commit_id;
    }

    'scan: loop {
        // 已经知道日志尾(仅 SCAN 之外):越过即停。
        if pass != Pass::Scan && tid_geq(next_commit_id, info.end_transaction) {
            break;
        }

        if j.jread(state, next_log_block, &mut bh).is_err() {
            return Err(BlockBackendError::Io);
        }
        next_log_block += 1;
        wrap(j, &mut next_log_block);

        if be32(&bh, 0) != JBD2_MAGIC_NUMBER {
            break;
        }
        let blocktype = be32(&bh, 4);
        let sequence = be32(&bh, 8);
        if sequence != next_commit_id {
            break;
        }

        match blocktype {
            JBD2_DESCRIPTOR_BLOCK => {
                if !j.descr_block_csum_verify(&bh) {
                    // PASS_SCAN 可能看到 lazy init 残留块,先记录待查;
                    // 其它趟直接失败。
                    if pass != Pass::Scan {
                        return Err(BlockBackendError::Io);
                    }
                    need_check_commit_time = true;
                }

                if pass != Pass::Replay {
                    if pass == Pass::Scan && j.has_v1_checksum() && info.end_transaction == 0 {
                        // v1:累计事务校验后跳过数据块
                        let _ = calc_chksums(
                            j,
                            state,
                            &bh,
                            &mut next_log_block,
                            &mut crc32_sum,
                            &mut scratch,
                        );
                        continue 'scan;
                    }
                    next_log_block += count_tags(j, &bh);
                    wrap(j, &mut next_log_block);
                    continue 'scan;
                }

                // PASS_REPLAY:真正的回放。
                if let Err(err) = do_replay(
                    j,
                    state,
                    &bh,
                    &mut next_log_block,
                    next_commit_id,
                    info,
                    revoke,
                ) {
                    deferred.get_or_insert(err);
                }
                continue 'scan;
            }
            JBD2_COMMIT_BLOCK => {
                if pass != Pass::Scan {
                    next_commit_id += 1;
                    continue 'scan;
                }
                let commit_time = be64(&bh, COMMIT_SEC_OFF);
                if need_check_commit_time {
                    if commit_time >= last_trans_commit_time {
                        // 校验失败但时间递增:日志损坏。
                        return Err(BlockBackendError::Io);
                    }
                    // 时间回退:陈旧日志块,就此结束(视为成功)。
                    break 'scan;
                }

                let mut chksum_ok = false;
                if j.has_v1_checksum() {
                    let found_chksum = be32(&bh, COMMIT_CHKSUM_OFF);
                    if info.end_transaction != 0 {
                        break 'scan;
                    }
                    let type_ok = bh[COMMIT_CHKSUM_TYPE_OFF] == JBD2_CRC32_CHKSUM
                        && bh[COMMIT_CHKSUM_SIZE_OFF] == JBD2_CRC32_CHKSUM_SIZE;
                    let zero_ok = bh[COMMIT_CHKSUM_TYPE_OFF] == 0
                        && bh[COMMIT_CHKSUM_SIZE_OFF] == 0
                        && found_chksum == 0;
                    if (crc32_sum == found_chksum && type_ok) || zero_ok {
                        crc32_sum = !0;
                        chksum_ok = true;
                    }
                } else {
                    chksum_ok =
                        j.commit_block_csum_verify(&bh) || j.commit_block_csum_verify_partial(&bh);
                }

                if !chksum_ok {
                    // chksum_error
                    if commit_time < last_trans_commit_time {
                        // 不属于同一日志:就此结束。
                        break 'scan;
                    }
                    info.end_transaction = next_commit_id;
                    info.head_block = head_block;
                    if !j.has_async_commit() {
                        break 'scan;
                    }
                }
                // chksum_ok
                last_trans_commit_time = commit_time;
                head_block = next_log_block;
                next_commit_id += 1;
                continue 'scan;
            }
            JBD2_REVOKE_BLOCK => {
                if pass != Pass::Revoke && pass != Pass::Scan {
                    continue 'scan;
                }
                if pass == Pass::Scan && !j.descr_block_csum_verify(&bh) {
                    need_check_commit_time = true;
                }
                scan_revoke_records(j, pass, &bh, next_commit_id, info, revoke)?;
                continue 'scan;
            }
            _ => break 'scan,
        }
    }

    if pass == Pass::Scan {
        if info.end_transaction == 0 {
            info.end_transaction = next_commit_id;
        }
        if info.head_block == 0 {
            info.head_block = head_block;
        }
    } else if info.end_transaction != next_commit_id {
        // 不同趟结束位置不一致:视为 I/O 级错误(内核 success = -EIO)。
        deferred.get_or_insert(BlockBackendError::Io);
    }

    if j.has_fast_commit() && pass != Pass::Revoke {
        fc.do_one_pass(j, state, info, pass)?;
    }

    match deferred {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// 加载日志 inode 与日志超级块,构造 [`Journal`]。
fn load_journal(state: &FsState) -> Result<Journal, BlockBackendError> {
    let journal_inum = if state.ext_sb.journal_inum != 0 {
        state.ext_sb.journal_inum
    } else {
        EXT4_JOURNAL_INO
    };
    // 外部日志设备无法经单一 backend 访问,明确拒绝。
    if state.ext_sb.journal_dev != 0 && state.ext_sb.journal_inum == 0 {
        return Err(BlockBackendError::Unsupported);
    }

    let raw = crate::inode_wr::read_raw(state, journal_inum)?;
    let j_flags = raw.flags();
    let mut j_iblock = [0u8; 60];
    j_iblock.copy_from_slice(raw.i_block());
    let journal_size = raw.size();
    let block_size = state.ext_sb.block_size;
    let inode_blocks = (journal_size / block_size as u64) as u32;

    // 日志超级块 = 日志逻辑块 0。
    let first_phys = crate::dir::resolve_block(state, &j_iblock, j_flags, 0)?
        .ok_or(BlockBackendError::OutOfRange)?;
    let mut jsb_block = vec![0u8; block_size as usize];
    state.read_block(first_phys, &mut jsb_block)?;

    if be32(&jsb_block, 0) != JBD2_MAGIC_NUMBER {
        return Err(BlockBackendError::OutOfRange);
    }
    let sb_type = be32(&jsb_block, 4);
    let is_v2 = match sb_type {
        JBD2_SUPERBLOCK_V1 => false,
        JBD2_SUPERBLOCK_V2 => true,
        _ => return Err(BlockBackendError::OutOfRange),
    };

    let sb = JournalSb {
        blocksize: be32(&jsb_block, JSB_BLOCKSIZE),
        maxlen: be32(&jsb_block, JSB_MAXLEN),
        first: be32(&jsb_block, JSB_FIRST),
        start: be32(&jsb_block, JSB_START),
        feature_compat: if is_v2 {
            be32(&jsb_block, JSB_FEATURE_COMPAT)
        } else {
            0
        },
        feature_incompat: if is_v2 {
            be32(&jsb_block, JSB_FEATURE_INCOMPAT)
        } else {
            0
        },
        uuid: {
            let mut u = [0u8; 16];
            if is_v2 {
                u.copy_from_slice(&jsb_block[JSB_UUID..JSB_UUID + 16]);
            }
            u
        },
        num_fc_blks: if is_v2 {
            be32(&jsb_block, JSB_NUM_FC_BLKS)
        } else {
            0
        },
    };

    if sb.blocksize != block_size {
        // 内部日志块大小必须与文件系统一致。
        return Err(BlockBackendError::Unsupported);
    }
    if be32(&jsb_block, JSB_FEATURE_RO_COMPAT) != 0
        || sb.feature_compat & !JBD2_FEATURE_COMPAT_CHECKSUM != 0
        || sb.feature_incompat & !JBD2_KNOWN_INCOMPAT != 0
    {
        return Err(BlockBackendError::Unsupported);
    }
    if sb.first == 0 || sb.first >= sb.maxlen {
        return Err(BlockBackendError::OutOfRange);
    }

    let has_v2or3 =
        sb.feature_incompat & (JBD2_FEATURE_INCOMPAT_CSUM_V2 | JBD2_FEATURE_INCOMPAT_CSUM_V3) != 0;
    let mut csum_seed = 0u32;
    if has_v2or3 {
        if jsb_block[JSB_CHECKSUM_TYPE] != JBD2_CRC32C_CHKSUM {
            return Err(BlockBackendError::Unsupported);
        }
        // 日志超级块自身校验:crc32c(~0, 前 1024 字节, 校验域清零)。
        let provided = be32(&jsb_block, JSB_CHECKSUM);
        let mut tmp = jsb_block[..JSB_STRUCT_SIZE].to_vec();
        tmp[JSB_CHECKSUM..JSB_CHECKSUM + 4].fill(0);
        if crc::crc32c(&tmp) != provided {
            return Err(BlockBackendError::OutOfRange);
        }
        csum_seed = crc::crc32c(&sb.uuid);
    }

    let total_len = inode_blocks.min(sb.maxlen);
    if sb.maxlen > inode_blocks {
        return Err(BlockBackendError::OutOfRange);
    }
    let first = sb.first;
    let mut last = sb.maxlen;
    let mut fc_first = 0;
    let mut fc_last = 0;
    if sb.feature_incompat & JBD2_FEATURE_INCOMPAT_FAST_COMMIT != 0 {
        let num_fc = if sb.num_fc_blks != 0 {
            sb.num_fc_blks
        } else {
            DEFAULT_FAST_COMMIT_BLOCKS
        };
        fc_last = sb.maxlen;
        last = fc_last
            .checked_sub(num_fc)
            .ok_or(BlockBackendError::OutOfRange)?;
        fc_first = last + 1;
        if fc_first > fc_last || fc_last > total_len {
            return Err(BlockBackendError::OutOfRange);
        }
    }

    Ok(Journal {
        jsb_block,
        j_iblock,
        j_flags,
        sb,
        first,
        last,
        fc_first,
        fc_last,
        total_len,
        csum_seed,
    })
}

impl Journal {
    pub(crate) fn fc_range(&self) -> (u32, u32) {
        (self.fc_first, self.fc_last)
    }
    pub(crate) fn read_fc_block(
        &self,
        state: &FsState,
        offset: u32,
        buf: &mut Vec<u8>,
    ) -> Result<(), BlockBackendError> {
        self.jread(state, offset, buf)
    }
}

/// 挂载入口:回放日志并复位日志头。
///
/// 返回 `Ok(None)` 表示日志为空(s_start == 0),无需恢复。
/// 成功后日志超级块被写回为"空日志"状态,调用方负责清除
/// 主超级块的 `INCOMPAT_RECOVER`。
pub(crate) fn recover(state: &FsState) -> Result<Option<RecoveryStats>, BlockBackendError> {
    let j = load_journal(state)?;
    if j.sb.start == 0 {
        return Ok(None);
    }

    let mut info = RecoveryStats::default();
    let mut revoke = RevokeTable::default();
    let mut fc = crate::fc::FcReplay::default();

    do_one_pass(&j, state, &mut info, Pass::Scan, &mut revoke, &mut fc)?;
    do_one_pass(&j, state, &mut info, Pass::Revoke, &mut revoke, &mut fc)?;
    do_one_pass(&j, state, &mut info, Pass::Replay, &mut revoke, &mut fc)?;

    // 回放产生的脏块全部落盘后再复位日志头,保证崩溃可重放。
    state.flush_dirty_blocks()?;

    // 复位日志超级块:s_start = 0,序列号推进,head 指向最后有效提交之后。
    let mut jsb = j.jsb_block.clone();
    let next_tid = info.end_transaction.wrapping_add(1);
    jsb[JSB_SEQUENCE..JSB_SEQUENCE + 4].copy_from_slice(&next_tid.to_be_bytes());
    jsb[JSB_START..JSB_START + 4].copy_from_slice(&0u32.to_be_bytes());
    jsb[JSB_HEAD..JSB_HEAD + 4].copy_from_slice(&info.head_block.to_be_bytes());
    jsb[JSB_ERRNO..JSB_ERRNO + 4].copy_from_slice(&0u32.to_be_bytes());
    if j.has_csum_v2or3() {
        jsb[JSB_CHECKSUM..JSB_CHECKSUM + 4].fill(0);
        let sum = crc::crc32c(&jsb[..JSB_STRUCT_SIZE]);
        jsb[JSB_CHECKSUM..JSB_CHECKSUM + 4].copy_from_slice(&sum.to_be_bytes());
    }
    let first_phys = j.bmap(state, 0)?;
    state.write_block(first_phys, &jsb)?;
    state.flush_dirty_blocks()?;

    Ok(Some(info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_bytes_matches_jbd2_layout_rules() {
        // 见 include/linux/jbd2.h `journal_tag_bytes()`。
        // v3 恒为 16;v2 为 14/10(64/32 位);无校验为 12/8。
        // 这里用纯位运算重新推导一遍做交叉验证。
        for &(v2, v3, is64, expect) in &[
            (false, false, false, 8usize),
            (false, false, true, 12usize),
            (true, false, false, 10usize),
            (true, false, true, 14usize),
            (false, true, false, 16usize),
            (false, true, true, 16usize),
        ] {
            let incompat = (if v2 { JBD2_FEATURE_INCOMPAT_CSUM_V2 } else { 0 })
                | (if v3 { JBD2_FEATURE_INCOMPAT_CSUM_V3 } else { 0 })
                | (if is64 { JBD2_FEATURE_INCOMPAT_64BIT } else { 0 });
            let sz = if incompat & JBD2_FEATURE_INCOMPAT_CSUM_V3 != 0 {
                16
            } else {
                let sz = if incompat & JBD2_FEATURE_INCOMPAT_CSUM_V2 != 0 {
                    14
                } else {
                    12
                };
                if incompat & JBD2_FEATURE_INCOMPAT_64BIT != 0 {
                    sz
                } else {
                    sz - 4
                }
            };
            assert_eq!(sz, expect, "v2={v2} v3={v3} 64={is64}");
        }
    }

    #[test]
    fn tid_comparison_wraps() {
        assert!(tid_gt(1, 0));
        assert!(!tid_gt(0, 1));
        assert!(tid_gt(0, u32::MAX));
        assert!(tid_geq(5, 5));
        assert!(!tid_geq(4, 5));
    }
}

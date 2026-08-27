//! MMP(多挂载保护)运行时心跳。
//!
//! 挂载时 [`crate::state::mmp_check`] 校验 MMP 块序列号(接受 CLEAN,或拒绝
//! 正在运行的其它节点/fsck);通过后 [`claim`] 立即写回非 CLEAN 序列号夺占
//! 所有权。运行期每次向设备回写脏块时 [`heartbeat`] 按 `s_mmp_update_interval`
//! 节流地推进序列号并刷新 `mmp_time`,使第二个节点在挂载时看到"仍存活"的
//! 心跳而拒绝并发挂载;干净卸载时 [`mark_clean`] 写回 `EXT4_MMP_SEQ_CLEAN`。
//!
//! ## 取舍(与 Linux 的差异)
//!
//! - Linux 用独立周期线程(`kmpd`)+ 信号量串行化心跳;本驱动没有周期性写
//!   线程,改为 **活动驱动** 的最小间隔心跳:仅在向设备写脏块 / `sync_all`
//!   时检查时间间隔并写心跳,空闲时心跳暂停(空闲节点本身不会产生并发写入,
//!   不影响防脑裂语义)。
//! - 心跳时间使用 VFS realtime 时钟(`Timespec::now`);时钟未安装前不写心跳
//!   (安全无操作)。
//! - 心跳失败仅记录日志不回滚序列号,下一次间隔到点重试;设备 I/O 错误由
//!   正常写路径另行上报。
//!
//! ## `mmp_struct` 布局(include/linux/ext4.h,1024 字节)
//!
//! ```text
//! 0x00 mmp_magic           u32  (0x004D4D50)
//! 0x04 mmp_seq             u32
//! 0x08 mmp_time            u64  (实时秒)
//! 0x10 mmp_nodename[64]
//! 0x50 mmp_bdevname[32]
//! 0x70 mmp_check_interval  u16
//! 0x74 mmp_pad2[226]
//! 0x3fc mmp_checksum      u32  (crc32c(csum_seed, mmp[..0x3fc]))
//! ```

use alloc::vec;
use core::sync::atomic::Ordering;

use vfs::stat::Timespec;

use crate::bgd;
use crate::crc;
use crate::layout::{EXT4_MMP_SEQ_CLEAN, INCOMPAT_MMP};
use crate::state::{BlockBackendError, FsState};

const MMP_SEQ_OFF: usize = 4;
const MMP_TIME_OFF: usize = 8;
const MMP_CHECKSUM_OFF: usize = 1020;
const DEFAULT_MMP_UPDATE_INTERVAL_SECS: u64 = 5;

/// 该文件系统是否启用 MMP(特性位 + 有效 MMP 块号)。
fn mmp_enabled(state: &FsState) -> bool {
    state.ext_sb.feature_incompat & INCOMPAT_MMP != 0
        && state.ext_sb.mmp_block != 0
        && state.ext_sb.mmp_block < state.ext_sb.blocks_count
}

/// 心跳节流间隔(纳秒);`s_mmp_update_interval == 0` 时取 Linux 默认 5 秒。
fn interval_ns(state: &FsState) -> u64 {
    let secs = if state.ext_sb.mmp_update_interval == 0 {
        DEFAULT_MMP_UPDATE_INTERVAL_SECS
    } else {
        state.ext_sb.mmp_update_interval as u64
    };
    secs.saturating_mul(1_000_000_000)
}

/// 实时时钟的当前秒(时钟未安装时为 0)。
fn now_secs() -> u64 {
    Timespec::now().secs.max(0) as u64
}

/// 实时时钟的当前纳秒(时钟未安装时为 0)。
fn now_ns() -> u64 {
    let ts = Timespec::now();
    (ts.secs.max(0) as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.nsecs as u64)
}

/// 读-改-写 MMP 块:保留 magic/nodename/bdevname/interval 等字段,
/// 只更新 `mmp_seq` / `mmp_time` / `mmp_checksum`,并 **直接写设备**
/// (绕过块缓存,保证另一节点能看到)。
fn write_mmp(state: &FsState, seq: u32, time_secs: u64) -> Result<(), BlockBackendError> {
    let sb = &state.ext_sb;
    let mut buf = vec![0u8; sb.block_size as usize];
    bgd::read_blocks(state.backend.as_ref(), sb, sb.mmp_block, 1, &mut buf)?;
    buf[MMP_SEQ_OFF..MMP_SEQ_OFF + 4].copy_from_slice(&seq.to_le_bytes());
    buf[MMP_TIME_OFF..MMP_TIME_OFF + 8].copy_from_slice(&time_secs.to_le_bytes());
    if sb.metadata_csum {
        let sum = crc::update(sb.csum_seed, &buf[..MMP_CHECKSUM_OFF]);
        buf[MMP_CHECKSUM_OFF..MMP_CHECKSUM_OFF + 4].copy_from_slice(&sum.to_le_bytes());
    }
    bgd::write_blocks(state.backend.as_ref(), sb, sb.mmp_block, 1, &buf)
}

/// 挂载时夺占 MMP 所有权:写回首个非 CLEAN 序列号并记录心跳起点。
///
/// 只读挂载不夺占(读共享不构成脑裂)。
pub(crate) fn claim(state: &FsState) -> Result<(), BlockBackendError> {
    if !mmp_enabled(state) || state.is_read_only() {
        return Ok(());
    }
    write_mmp(state, 1, now_secs())?;
    state.mmp.seq.store(1, Ordering::Release);
    state
        .mmp
        .last_heartbeat_ns
        .store(now_ns(), Ordering::Release);
    Ok(())
}

/// 活动驱动的运行时心跳:仅在距上次心跳超过 `s_mmp_update_interval` 时写回。
pub(crate) fn heartbeat(state: &FsState) {
    heartbeat_at(state, now_ns(), now_secs());
}

/// 心跳核心(分离时间参数便于测试)。用 `last_heartbeat_ns` 的 CAS 保证同一
/// 时刻只有一个 owner 写 MMP 块;序列号经 `mmp_seq` 单调推进。
pub(crate) fn heartbeat_at(state: &FsState, at_ns: u64, at_secs: u64) {
    if !mmp_enabled(state) || state.is_read_only() {
        return;
    }
    let last = state.mmp.last_heartbeat_ns.load(Ordering::Acquire);
    if at_ns.saturating_sub(last) < interval_ns(state) {
        return;
    }
    if state
        .mmp
        .last_heartbeat_ns
        .compare_exchange(last, at_ns, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // 另一 owner 已在本间隔写过心跳,本次让出。
        return;
    }
    let seq = state.mmp.seq.fetch_add(1, Ordering::Relaxed) + 1;
    if let Err(err) = write_mmp(state, seq, at_secs) {
        log::warning!("[extfs] MMP 心跳写回失败(seq={seq}): {err:?}");
    }
}

/// 干净卸载 / 转只读时写回 CLEAN 序列号,允许后续节点挂载。
pub(crate) fn mark_clean(state: &FsState) {
    if !mmp_enabled(state) {
        return;
    }
    if let Err(err) = write_mmp(state, EXT4_MMP_SEQ_CLEAN, now_secs()) {
        log::warning!("[extfs] MMP 清理写回失败: {err:?}");
    }
}

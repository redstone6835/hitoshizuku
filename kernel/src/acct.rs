//! BSD/Linux 进程记账。
//!
//! `acct(2)` 持有一个内核级追加文件；线程组最后一个成员退出时写入一条
//! Linux `acct_v3` 记录。文件 I/O 失败不能阻断退出路径，只记录告警。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use sched::sync::Spinlock;
use sched::{Task, TaskUsage};
use vfs::file::File;

const ACCT_RECORD_SIZE: usize = 64;
const ACCT_VERSION: u8 = 2;
const ACCT_HZ: u64 = 100;
const NSEC_PER_SEC: u64 = 1_000_000_000;

const ASU: u8 = 0x02;
const ACORE: u8 = 0x08;
const AXSIG: u8 = 0x10;
const AGROUP: u8 = 0x20;

static ACCT_FILE: Spinlock<Option<Arc<File>>> = Spinlock::new(None);
static ACCT_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn install(file: Arc<File>) {
    *ACCT_FILE.lock() = Some(file);
    ACCT_ACTIVE.store(true, Ordering::Release);
}

pub(crate) fn disable() {
    ACCT_ACTIVE.store(false, Ordering::Release);
    ACCT_FILE.lock().take();
}

/// 把一个退出成员纳入线程组统计，并在最后一个成员处输出进程记录。
pub(crate) fn account_task_exit(task: &Task) {
    let now_ns = crate::vdso::monotonic_ns();
    let group = task.thread_group();
    let last = group.account_member_exit(task.usage_snapshot(now_ns));
    if !last || !task.is_user_task() || !ACCT_ACTIVE.load(Ordering::Acquire) {
        return;
    }

    let Some(file) = ACCT_FILE.lock().as_ref().map(Arc::clone) else {
        return;
    };
    if !group.try_claim_acct_record() {
        return;
    }

    let representative = group.leader();
    let record_task = representative.as_deref().unwrap_or(task);
    let status = task
        .exit_wait_status()
        .unwrap_or_else(|| sched::WaitStatus::from_exit(task.exit_code().map_or(0, |code| code.0)));
    let record = encode_record(
        record_task,
        status,
        group.exited_usage_snapshot(),
        now_ns,
        crate::vdso::realtime_ns(),
    );
    match file.write(&record) {
        Ok(ACCT_RECORD_SIZE) => {}
        Ok(written) => log::warning!(
            "[acct] 进程记账记录写入不完整: pid={} written={}",
            group.tgid(),
            written,
        ),
        Err(error) => log::warning!(
            "[acct] 进程记账记录写入失败: pid={} error={:?}",
            group.tgid(),
            error,
        ),
    }
}

fn encode_record(
    task: &Task,
    status: sched::WaitStatus,
    usage: TaskUsage,
    now_ns: u64,
    realtime_ns: u64,
) -> [u8; ACCT_RECORD_SIZE] {
    let mut out = [0u8; ACCT_RECORD_SIZE];
    let creds = task.credentials();
    let elapsed_ns = now_ns.saturating_sub(task.start_time_ns());
    let start_realtime_ns = realtime_ns.saturating_sub(elapsed_ns);

    let mut flags = AGROUP;
    if creds.euid.is_root() {
        flags |= ASU;
    }
    if status.wifsignaled() {
        flags |= AXSIG;
    }
    if status.wcoredump() {
        flags |= ACORE;
    }

    out[0] = flags;
    out[1] = ACCT_VERSION;
    put_u16(&mut out, 2, 0);
    put_u32(&mut out, 4, status.raw() as u32);
    put_u32(&mut out, 8, creds.uid.0);
    put_u32(&mut out, 12, creds.gid.0);
    put_u32(&mut out, 16, task.tgid_cached().unwrap_or(0).max(0) as u32);
    put_u32(
        &mut out,
        20,
        task.parent()
            .and_then(|parent| parent.tgid_cached())
            .unwrap_or(0)
            .max(0) as u32,
    );
    put_u32(
        &mut out,
        24,
        (start_realtime_ns / NSEC_PER_SEC).min(u32::MAX as u64) as u32,
    );
    put_u32(&mut out, 28, ns_to_f32_bits(elapsed_ns));
    put_u16(&mut out, 32, encode_comp_t(ns_to_acct_ticks(usage.user_ns)));
    put_u16(
        &mut out,
        34,
        encode_comp_t(ns_to_acct_ticks(usage.system_ns)),
    );
    put_u16(&mut out, 42, encode_comp_t(usage.minflt));
    put_u16(&mut out, 44, encode_comp_t(usage.majflt));
    out[48..64].copy_from_slice(&task.comm());
    out
}

fn ns_to_acct_ticks(ns: u64) -> u64 {
    ns.saturating_mul(ACCT_HZ) / NSEC_PER_SEC
}

/// 用纯整数生成秒数的 IEEE-754 binary32，内核不能触发浮点保存/恢复指令。
pub(crate) fn ns_to_f32_bits(ns: u64) -> u32 {
    if ns == 0 {
        return 0;
    }
    let mut numerator = ns as u128;
    let mut denominator = NSEC_PER_SEC as u128;
    let mut exponent = 0i32;
    if numerator >= denominator {
        while numerator >= (denominator << 1) && exponent < 127 {
            denominator <<= 1;
            exponent += 1;
        }
    } else {
        while numerator < denominator && exponent > -126 {
            numerator <<= 1;
            exponent -= 1;
        }
    }

    let scaled = numerator << 23;
    let mut significand = scaled / denominator;
    let remainder = scaled % denominator;
    if remainder.saturating_mul(2) >= denominator {
        significand += 1;
    }
    if significand >= (1 << 24) {
        significand >>= 1;
        exponent += 1;
    }
    if exponent > 127 {
        return 0x7f80_0000;
    }
    (((exponent + 127) as u32) << 23) | ((significand as u32) & 0x007f_ffff)
}

/// Linux comp_t：13 位尾数和 3 位以 8 为底的指数，并在移位时四舍五入。
pub(crate) fn encode_comp_t(mut value: u64) -> u16 {
    let mut exponent = 0u16;
    let mut round = 0u64;
    while value > 0x1fff && exponent < 7 {
        round = value & 0x7;
        value >>= 3;
        exponent += 1;
    }
    if round > 3 {
        value = value.saturating_add(1);
        if value > 0x1fff && exponent < 7 {
            value >>= 3;
            exponent += 1;
        }
    }
    ((exponent << 13) | value.min(0x1fff) as u16) as u16
}

fn put_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

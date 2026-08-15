//! RISC-V AIA IMSIC 的常驻 per-CPU CSR 状态桥。
//!
//! IMSIC 的 EIE/EIP 和 delivery 寄存器只能由目标 hart 通过间接 CSR 访问。设备
//! ELM 负责解析 DT、分配 identity 和生成 MSI message；本模块只保存不含函数指针
//! 的期望位图，并在每个 hart 的启动或 timer trap 边界把状态同步到本地 CSR。

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const IMSIC_MAX_ID: u32 = 2047;
const IMSIC_ID_BITS: usize = u64::BITS as usize;
const IMSIC_WORDS: usize = 32;
const IMSIC_EIDELIVERY: usize = 0x70;
const IMSIC_EITHRESHOLD: usize = 0x72;
const IMSIC_EIP0: usize = 0x80;
const IMSIC_EIE0: usize = 0xc0;
const IMSIC_ISELECT_STRIDE: usize = 2;
const IMSIC_TOPEI_ID_SHIFT: usize = 16;
const IMSIC_TOPEI_ID_MASK: usize = 0x7ff;
const SIE_SEIE: usize = 1 << 9;
const SSTATUS_SIE: usize = 1 << 1;
const INSTALLING_HANDLE: u64 = u64::MAX;

static ACTIVE_HANDLE: AtomicU64 = AtomicU64::new(0);
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
static NUM_IDS: AtomicU32 = AtomicU32::new(0);
static CPU_MASK: AtomicU64 = AtomicU64::new(0);
static REVISION: AtomicU64 = AtomicU64::new(1);
static DESIRED_EIE: [AtomicU64; sched::NR_CPUS * IMSIC_WORDS] =
    [const { AtomicU64::new(0) }; sched::NR_CPUS * IMSIC_WORDS];
static PENDING_CLEAR: [AtomicU64; sched::NR_CPUS * IMSIC_WORDS] =
    [const { AtomicU64::new(0) }; sched::NR_CPUS * IMSIC_WORDS];
static APPLIED_HANDLE: [AtomicU64; sched::NR_CPUS] = [const { AtomicU64::new(0) }; sched::NR_CPUS];
static APPLIED_NUM_IDS: [AtomicU32; sched::NR_CPUS] = [const { AtomicU32::new(0) }; sched::NR_CPUS];
static APPLIED_REVISION: [AtomicU64; sched::NR_CPUS] =
    [const { AtomicU64::new(0) }; sched::NR_CPUS];

fn next_handle() -> u64 {
    loop {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        if handle != 0 && handle != INSTALLING_HANDLE {
            return handle;
        }
    }
}

/// 安装一个不含 ELM 回调地址的 IMSIC CSR 配置。
pub fn install_imsic_config(num_ids: u32, cpu_mask: u64) -> Option<u64> {
    let supported_mask = if sched::NR_CPUS >= u64::BITS as usize {
        u64::MAX
    } else {
        (1u64 << sched::NR_CPUS) - 1
    };
    let cpu_mask = cpu_mask & supported_mask;
    if num_ids == 0 || num_ids > IMSIC_MAX_ID || cpu_mask == 0 {
        return None;
    }
    ACTIVE_HANDLE
        .compare_exchange(0, INSTALLING_HANDLE, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    for word in &DESIRED_EIE {
        word.store(0, Ordering::Relaxed);
    }
    for word in &PENDING_CLEAR {
        word.store(0, Ordering::Relaxed);
    }
    NUM_IDS.store(num_ids, Ordering::Relaxed);
    CPU_MASK.store(cpu_mask, Ordering::Relaxed);
    let handle = next_handle();
    REVISION.fetch_add(1, Ordering::AcqRel);
    ACTIVE_HANDLE.store(handle, Ordering::Release);
    for cpu in 0..sched::NR_CPUS.min(u64::BITS as usize) {
        if cpu_mask & (1u64 << cpu) != 0 {
            let _ = super::smp::request_aia_sync(cpu);
        }
    }
    Some(handle)
}

/// 撤销当前 IMSIC 配置；各 hart 会在下一同步边界关闭 delivery。
pub fn uninstall_imsic_config(handle: u64) -> bool {
    if handle == 0
        || ACTIVE_HANDLE
            .compare_exchange(handle, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return false;
    }
    let cpu_mask = CPU_MASK.swap(0, Ordering::AcqRel);
    NUM_IDS.store(0, Ordering::Release);
    REVISION.fetch_add(1, Ordering::AcqRel);
    for cpu in 0..sched::NR_CPUS.min(u64::BITS as usize) {
        if cpu_mask & (1u64 << cpu) != 0 {
            let _ = super::smp::request_aia_sync(cpu);
        }
    }
    true
}

/// 修改某个逻辑 CPU interrupt file 的 identity enable 期望值。
pub fn set_imsic_identity_enabled(handle: u64, cpu: usize, id: u32, enabled: bool) -> bool {
    if handle == 0
        || ACTIVE_HANDLE.load(Ordering::Acquire) != handle
        || cpu >= sched::NR_CPUS
        || id == 0
        || id > NUM_IDS.load(Ordering::Acquire)
        || CPU_MASK.load(Ordering::Acquire) & (1u64 << cpu) == 0
    {
        return false;
    }
    let word = id as usize / IMSIC_ID_BITS;
    let bit = 1u64 << (id as usize % IMSIC_ID_BITS);
    let slot = cpu * IMSIC_WORDS + word;
    if enabled {
        DESIRED_EIE[slot].fetch_or(bit, Ordering::AcqRel);
    } else {
        DESIRED_EIE[slot].fetch_and(!bit, Ordering::AcqRel);
    }
    REVISION.fetch_add(1, Ordering::AcqRel);
    if ACTIVE_HANDLE.load(Ordering::Acquire) != handle {
        return false;
    }
    let _ = super::smp::request_aia_sync(cpu);
    true
}

/// 请求目标 hart 在下一次同步时清除一个 IMSIC pending identity。
pub fn clear_imsic_identity(handle: u64, cpu: usize, id: u32) -> bool {
    if handle == 0
        || ACTIVE_HANDLE.load(Ordering::Acquire) != handle
        || cpu >= sched::NR_CPUS
        || cpu >= u64::BITS as usize
        || id == 0
        || id > NUM_IDS.load(Ordering::Acquire)
        || CPU_MASK.load(Ordering::Acquire) & (1u64 << cpu) == 0
    {
        return false;
    }
    let word = id as usize / IMSIC_ID_BITS;
    let bit = 1u64 << (id as usize % IMSIC_ID_BITS);
    PENDING_CLEAR[cpu * IMSIC_WORDS + word].fetch_or(bit, Ordering::AcqRel);
    REVISION.fetch_add(1, Ordering::AcqRel);
    if ACTIVE_HANDLE.load(Ordering::Acquire) != handle {
        return false;
    }
    let _ = super::smp::request_aia_sync(cpu);
    true
}

#[inline]
unsafe fn write_indirect(register: usize, value: usize) {
    // Safety: 调用方只传入 AIA 规定的 supervisor interrupt-file 间接寄存器编号；
    // 本函数在本地中断关闭期间成对更新 siselect/sireg。
    unsafe {
        core::arch::asm!(
            "csrw 0x150, {select}",
            "csrw 0x151, {value}",
            select = in(reg) register,
            value = in(reg) value,
            options(nostack),
        );
    }
}

#[inline]
unsafe fn clear_indirect(register: usize, mask: usize) {
    // Safety: 调用方与 `write_indirect` 相同，并且只对 EIP 位图执行 CSR clear。
    unsafe {
        core::arch::asm!(
            "csrw 0x150, {select}",
            "csrrc zero, 0x151, {mask}",
            select = in(reg) register,
            mask = in(reg) mask,
            options(nostack),
        );
    }
}

fn valid_identity_mask(num_ids: u32, word: usize) -> usize {
    let first = word * IMSIC_ID_BITS;
    if first > num_ids as usize {
        return 0;
    }
    let bits = (num_ids as usize - first + 1).min(IMSIC_ID_BITS);
    if bits == IMSIC_ID_BITS {
        usize::MAX
    } else {
        (1usize << bits) - 1
    }
}

#[inline]
fn identity_words(num_ids: u32) -> usize {
    (num_ids as usize / IMSIC_ID_BITS + 1).min(IMSIC_WORDS)
}

unsafe fn program_local_file(cpu: usize, handle: u64, num_ids: u32) {
    let words = identity_words(num_ids);
    let previous_handle = APPLIED_HANDLE[cpu].load(Ordering::Acquire);
    let clear_num_ids = num_ids.max(APPLIED_NUM_IDS[cpu].load(Ordering::Acquire));
    let clear_words = identity_words(clear_num_ids);
    // Safety: 调用点已经关闭本地中断，避免 siselect/sireg 序列被另一个间接 CSR
    // 使用者打断；寄存器编号和 identity 上限都经过安装入口校验。
    unsafe {
        write_indirect(IMSIC_EIDELIVERY, 0);
        // 未实现的 EIP/EIE selector 允许触发非法指令，不能按软件最大值访问。
        // handle 切换或 num_ids 缩小时只额外清理先前实际使用过的寄存器。
        for word in 0..clear_words {
            let pending_clear =
                PENDING_CLEAR[cpu * IMSIC_WORDS + word].swap(0, Ordering::AcqRel) as usize;
            let pending_clear = if previous_handle != handle {
                pending_clear | valid_identity_mask(clear_num_ids, word)
            } else {
                pending_clear
            };
            if pending_clear != 0 {
                clear_indirect(IMSIC_EIP0 + word * IMSIC_ISELECT_STRIDE, pending_clear);
            }
            let mut value = if word < words {
                DESIRED_EIE[cpu * IMSIC_WORDS + word].load(Ordering::Acquire) as usize
            } else {
                0
            };
            if word + 1 == words {
                let valid_bits = num_ids as usize % IMSIC_ID_BITS + 1;
                if valid_bits < IMSIC_ID_BITS {
                    value &= (1usize << valid_bits) - 1;
                }
            }
            write_indirect(IMSIC_EIE0 + word * IMSIC_ISELECT_STRIDE, value);
        }
        write_indirect(IMSIC_EITHRESHOLD, 0);
        write_indirect(IMSIC_EIDELIVERY, 1);
        core::arch::asm!("csrs sie, {mask}", mask = in(reg) SIE_SEIE, options(nostack));
    }
    APPLIED_NUM_IDS[cpu].store(num_ids, Ordering::Release);
    APPLIED_HANDLE[cpu].store(handle, Ordering::Release);
}

unsafe fn disable_local_file(cpu: usize) {
    let num_ids = APPLIED_NUM_IDS[cpu].load(Ordering::Acquire);
    // Safety: 与 `program_local_file` 相同，本地中断已关闭；写入的都是 IMSIC EIE
    // 和 delivery 寄存器的规范值。
    unsafe {
        write_indirect(IMSIC_EIDELIVERY, 0);
        write_indirect(IMSIC_EITHRESHOLD, 1);
        for word in 0..identity_words(num_ids) {
            write_indirect(IMSIC_EIE0 + word * IMSIC_ISELECT_STRIDE, 0);
            let pending = valid_identity_mask(num_ids, word);
            if pending != 0 {
                clear_indirect(IMSIC_EIP0 + word * IMSIC_ISELECT_STRIDE, pending);
            }
            PENDING_CLEAR[cpu * IMSIC_WORDS + word].store(0, Ordering::Release);
        }
    }
    APPLIED_NUM_IDS[cpu].store(0, Ordering::Release);
    APPLIED_HANDLE[cpu].store(0, Ordering::Release);
}

/// 在当前 hart 上落实最新的 IMSIC CSR 状态。
pub fn sync_current_cpu() {
    let cpu = super::specific::current_cpu_id();
    if cpu >= sched::NR_CPUS || cpu >= u64::BITS as usize {
        return;
    }
    let revision = REVISION.load(Ordering::Acquire);
    let handle = ACTIVE_HANDLE.load(Ordering::Acquire);
    if handle == INSTALLING_HANDLE {
        return;
    }
    if APPLIED_HANDLE[cpu].load(Ordering::Acquire) == handle
        && APPLIED_REVISION[cpu].load(Ordering::Acquire) == revision
    {
        return;
    }

    let previous_sstatus: usize;
    // Safety: 只暂时清除当前 hart 的 sstatus.SIE，并在完成不可分割的间接 CSR
    // 序列后按原状态恢复；不改变其它 sstatus 位。
    unsafe {
        core::arch::asm!(
            "csrrc {previous}, sstatus, {mask}",
            previous = out(reg) previous_sstatus,
            mask = in(reg) SSTATUS_SIE,
            options(nostack),
        );
        let cpu_enabled = handle != 0 && CPU_MASK.load(Ordering::Acquire) & (1u64 << cpu) != 0;
        if cpu_enabled {
            program_local_file(cpu, handle, NUM_IDS.load(Ordering::Acquire));
        } else if APPLIED_HANDLE[cpu].load(Ordering::Acquire) != 0 {
            disable_local_file(cpu);
        }
        if previous_sstatus & SSTATUS_SIE != 0 {
            core::arch::asm!("csrs sstatus, {mask}", mask = in(reg) SSTATUS_SIE, options(nostack));
        }
    }
    APPLIED_REVISION[cpu].store(revision, Ordering::Release);
}

/// 原子 claim/complete 当前 supervisor interrupt file 的最高优先级 identity。
pub fn claim_imsic_identity() -> Option<u32> {
    let raw: usize;
    // Safety: `stopei` 是 AIA 定义的 supervisor CSR；以一次 csrrw 写零完成上一个
    // identity 并取得下一个 identity，不访问内存，也不修改栈。
    unsafe {
        core::arch::asm!(
            "csrrw {raw}, 0x15c, zero",
            raw = out(reg) raw,
            options(nomem, nostack),
        );
    }
    let id = ((raw >> IMSIC_TOPEI_ID_SHIFT) & IMSIC_TOPEI_ID_MASK) as u32;
    (id != 0).then_some(id)
}

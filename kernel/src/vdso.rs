//! vDSO 共享数据页与时间语义。

use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering, fence};

use errno::Errno;
use general::dev::pnp::RealtimeClockSource;
use sched::sync::Spinlock;

const NSEC_PER_SEC: u64 = 1_000_000_000;
const VDSO_CLOCK_MODE_RDTIME: u32 = 0;
const VDSO_CLOCK_MODE_SYSCALL: u32 = 1;
const CYCLE_TO_NS_SHIFT: u32 = 24;

pub const CLOCK_REALTIME: usize = 0;
pub const CLOCK_MONOTONIC: usize = 1;
pub const CLOCK_MONOTONIC_RAW: usize = 4;
pub const CLOCK_REALTIME_COARSE: usize = 5;
pub const CLOCK_MONOTONIC_COARSE: usize = 6;
pub const CLOCK_BOOTTIME: usize = 7;

#[repr(C)]
struct VdsoData {
    seq: AtomicU32,
    clock_mode: u32,
    hz: u64,
    wall_time_sec: i64,
    wall_time_nsec: i64,
    monotonic_base_ns: u64,
    cs_cycle_last: u64,
    cs_mult: u64,
    cs_shift: u32,
    cpu_id: u32,
    node_id: u32,
    clock_realtime_res: u32,
}

const _: () = {
    assert!(size_of::<VdsoData>() == 72);
    assert!(size_of::<VdsoData>() <= 4096);
};

static DATA_PAGE_PADDR: AtomicUsize = AtomicUsize::new(0);
static DATA_PAGE_KVA: AtomicUsize = AtomicUsize::new(0);
static INIT_LOCK: Spinlock<()> = Spinlock::new(());
static DATA_WRITE_BUSY: AtomicBool = AtomicBool::new(false);
static REALTIME_OFFSET_NS: AtomicI64 = AtomicI64::new(0);
static REALTIME_SOURCE_ID: AtomicUsize = AtomicUsize::new(0);

pub fn monotonic_ns() -> u64 {
    hal::time::monotonic_ns()
}

pub fn realtime_ns() -> u64 {
    apply_realtime_offset(monotonic_ns())
}

pub fn set_realtime_ns(realtime_ns: u64) {
    let now_ns = monotonic_ns();
    let offset = (realtime_ns as i128).saturating_sub(now_ns as i128);
    let offset = offset.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    REALTIME_OFFSET_NS.store(offset, Ordering::Relaxed);
    try_write_data(now_ns);
}

pub fn install_realtime_source(source: RealtimeClockSource) -> bool {
    if source.id == 0 {
        return false;
    }

    let current = REALTIME_SOURCE_ID.load(Ordering::Acquire);
    if current == source.id {
        set_realtime_ns(source.realtime_ns);
        return true;
    }
    if current != 0 {
        return false;
    }
    if REALTIME_SOURCE_ID
        .compare_exchange(0, source.id, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        set_realtime_ns(source.realtime_ns);
        return true;
    }
    false
}

pub fn unregister_realtime_source(source_id: usize) {
    if source_id == 0 {
        return;
    }
    // 只释放匹配 owner。已经设置过的 realtime offset 保留，避免 RTC 热移除
    // 让 CLOCK_REALTIME 倒退；source id 清空后，替代 RTC 可以接管。
    let _ = REALTIME_SOURCE_ID.compare_exchange(source_id, 0, Ordering::AcqRel, Ordering::Acquire);
}

pub fn clock_time_ns(clock_id: usize) -> Option<u64> {
    match clock_id {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => Some(realtime_ns()),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            Some(monotonic_ns())
        }
        _ => None,
    }
}

pub fn clock_getres_ns(clock_id: usize) -> Option<u32> {
    match clock_id {
        CLOCK_REALTIME
        | CLOCK_MONOTONIC
        | CLOCK_MONOTONIC_RAW
        | CLOCK_REALTIME_COARSE
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME => Some(1),
        _ => None,
    }
}

pub fn shared_data_page_paddr() -> Result<usize, Errno> {
    ensure_data_page()
}

pub fn update_on_timer_tick(now_ns: u64) {
    if sched::current_cpu_id() == 0 {
        try_write_data(now_ns);
    }
}

fn ensure_data_page() -> Result<usize, Errno> {
    let existing = DATA_PAGE_PADDR.load(Ordering::Acquire);
    if existing != 0 {
        return Ok(existing);
    }

    let _guard = INIT_LOCK.lock();
    let existing = DATA_PAGE_PADDR.load(Ordering::Acquire);
    if existing != 0 {
        return Ok(existing);
    }

    let (paddr, kva) = alloc_zeroed_page()?;
    DATA_PAGE_KVA.store(kva, Ordering::Release);
    DATA_PAGE_PADDR.store(paddr, Ordering::Release);
    try_write_data(monotonic_ns());
    Ok(paddr)
}

fn alloc_zeroed_page() -> Result<(usize, usize), Errno> {
    let allocation = allocator::KERNEL_ALLOCATOR
        .allocate_physical(allocator::PhysicalAllocRequest::new(
            allocator::PAGE_SIZE,
            allocator::PAGE_SIZE,
        ))
        .map_err(|_| Errno::ENOMEM)?;
    let Some(phys_to_virt) = allocator::KERNEL_ALLOCATOR.load_phys_to_virt() else {
        let _ = allocator::KERNEL_ALLOCATOR.try_free_physical(allocation);
        return Err(Errno::ENOMEM);
    };
    let paddr = allocation.paddr;
    let kva = phys_to_virt(paddr);
    unsafe { core::ptr::write_bytes(kva as *mut u8, 0, hal::user::vdso_data_page_offset()) };
    Ok((paddr, kva))
}

fn try_write_data(now_ns: u64) {
    if DATA_WRITE_BUSY
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let kva = DATA_PAGE_KVA.load(Ordering::Acquire);
    if kva != 0 {
        unsafe { write_data(&mut *(kva as *mut VdsoData), now_ns) };
    }
    DATA_WRITE_BUSY.store(false, Ordering::Release);
}

fn write_data(data: &mut VdsoData, now_ns: u64) {
    let seq = data.seq.load(Ordering::Relaxed);
    data.seq.store(seq.wrapping_add(1), Ordering::Release);
    fence(Ordering::Release);

    let hz = hal::time::stable_counter_hz();
    let cycle_last = hal::time::stable_counter_raw();
    let monotonic_base_ns = if hz == 0 {
        now_ns
    } else {
        counter_to_ns(cycle_last, hz)
    };
    let (wall_time_sec, wall_time_nsec) = split_realtime_parts(monotonic_base_ns);
    data.clock_mode = if hz == 0 {
        VDSO_CLOCK_MODE_SYSCALL
    } else {
        VDSO_CLOCK_MODE_RDTIME
    };
    data.hz = hz;
    data.wall_time_sec = wall_time_sec;
    data.wall_time_nsec = wall_time_nsec;
    data.monotonic_base_ns = monotonic_base_ns;
    data.cs_cycle_last = cycle_last;
    data.cs_mult = cycle_to_ns_mult(hz);
    data.cs_shift = CYCLE_TO_NS_SHIFT;
    data.cpu_id = sched::current_cpu_id() as u32;
    data.node_id = 0;
    data.clock_realtime_res = 1;

    fence(Ordering::Release);
    data.seq.store(seq.wrapping_add(2), Ordering::Release);
}

fn counter_to_ns(cycle: u64, hz: u64) -> u64 {
    let secs = cycle / hz;
    let frac_ns = (cycle % hz) * NSEC_PER_SEC / hz;
    secs * NSEC_PER_SEC + frac_ns
}

fn cycle_to_ns_mult(hz: u64) -> u64 {
    if hz == 0 {
        0
    } else {
        (((NSEC_PER_SEC as u128) << CYCLE_TO_NS_SHIFT) / (hz as u128)) as u64
    }
}

fn apply_realtime_offset(now_ns: u64) -> u64 {
    let offset = REALTIME_OFFSET_NS.load(Ordering::Relaxed) as i128;
    let realtime_ns = (now_ns as i128).saturating_add(offset);
    realtime_ns.max(0) as u64
}

fn split_realtime_parts(now_ns: u64) -> (i64, i64) {
    let realtime_ns = apply_realtime_offset(now_ns);
    (
        (realtime_ns / NSEC_PER_SEC) as i64,
        (realtime_ns % NSEC_PER_SEC) as i64,
    )
}

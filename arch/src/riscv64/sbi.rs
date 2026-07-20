//! RISC-V Supervisor Binary Interface 封装。
//!
//! 封装 BASE、TIME、SRST 以及 SMP 所需的 HSM、IPI、RFENCE 扩展。

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

pub const SBI_EXT_BASE: usize = 0x10;
pub const SBI_EXT_TIME: usize = 0x5449_4d45;
pub const SBI_EXT_SRST: usize = 0x5352_5354;
pub const SBI_EXT_HSM: usize = 0x4853_4d;
pub const SBI_EXT_IPI: usize = 0x7350_49;
pub const SBI_EXT_RFENCE: usize = 0x5246_4e43;

const SBI_BASE_GET_SPEC_VERSION: usize = 0;
const SBI_BASE_GET_IMPL_ID: usize = 1;
const SBI_BASE_GET_IMPL_VERSION: usize = 2;
const SBI_BASE_PROBE_EXTENSION: usize = 3;
const SBI_TIME_SET_TIMER: usize = 0;
const SBI_SRST_SYSTEM_RESET: usize = 0;
const SBI_HSM_HART_START: usize = 0;
const SBI_HSM_HART_STOP: usize = 1;
const SBI_HSM_HART_GET_STATUS: usize = 2;
const SBI_IPI_SEND_IPI: usize = 0;
const SBI_RFENCE_REMOTE_FENCE_I: usize = 0;
const SBI_RFENCE_REMOTE_SFENCE_VMA: usize = 1;
const SBI_RFENCE_REMOTE_SFENCE_VMA_ASID: usize = 2;

pub const SBI_SUCCESS: isize = 0;
pub const SBI_ERR_FAILED: isize = -1;
pub const SBI_ERR_NOT_SUPPORTED: isize = -2;
pub const SBI_ERR_INVALID_PARAM: isize = -3;
pub const SBI_ERR_DENIED: isize = -4;
pub const SBI_ERR_INVALID_ADDRESS: isize = -5;
pub const SBI_ERR_ALREADY_AVAILABLE: isize = -6;
pub const SBI_ERR_ALREADY_STARTED: isize = -7;
pub const SBI_ERR_ALREADY_STOPPED: isize = -8;
pub const SBI_ERR_NO_SHMEM: isize = -9;

const CAPABILITY_UNKNOWN: u8 = 0;
const CAPABILITY_UNAVAILABLE: u8 = 1;
const CAPABILITY_AVAILABLE: u8 = 2;

static SBI_INITIALIZED: AtomicBool = AtomicBool::new(false);
static SBI_BASE_AVAILABLE: AtomicBool = AtomicBool::new(false);
static SBI_HSM_AVAILABLE: AtomicBool = AtomicBool::new(false);
static SBI_IPI_AVAILABLE: AtomicBool = AtomicBool::new(false);
static SBI_RFENCE_AVAILABLE: AtomicBool = AtomicBool::new(false);
static SBI_SPEC_VERSION: AtomicUsize = AtomicUsize::new(0);
static SBI_IMPL_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static SBI_IMPL_VERSION: AtomicUsize = AtomicUsize::new(0);
static SBI_SRST_CAPABILITY: AtomicU8 = AtomicU8::new(CAPABILITY_UNKNOWN);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SbiRet {
    pub error: isize,
    pub value: usize,
}

impl SbiRet {
    #[inline]
    pub const fn is_ok(self) -> bool {
        self.error == SBI_SUCCESS
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SbiInfo {
    pub base_available: bool,
    pub spec_version: Option<usize>,
    pub implementation_id: Option<usize>,
    pub implementation_version: Option<usize>,
    pub srst_available: bool,
    pub hsm_available: bool,
    pub ipi_available: bool,
    pub rfence_available: bool,
}

/// 执行 SBI v0.2+ ecall。
#[inline]
pub fn call(extension: usize, function: usize, args: [usize; 6]) -> SbiRet {
    let [mut a0, mut a1, a2, a3, a4, a5] = args;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") a0,
            inlateout("a1") a1,
            in("a2") a2,
            in("a3") a3,
            in("a4") a4,
            in("a5") a5,
            in("a6") function,
            in("a7") extension,
            options(nostack)
        );
    }
    SbiRet {
        error: a0 as isize,
        value: a1,
    }
}

#[inline]
fn base_call(function: usize, arg0: usize) -> SbiRet {
    call(SBI_EXT_BASE, function, [arg0, 0, 0, 0, 0, 0])
}

/// 探测 BASE，并缓存当前阶段使用的固件能力。
pub fn init() -> SbiInfo {
    let spec = base_call(SBI_BASE_GET_SPEC_VERSION, 0);
    let base_available = spec.is_ok();
    SBI_BASE_AVAILABLE.store(base_available, Ordering::Release);

    let (
        spec_version,
        implementation_id,
        implementation_version,
        srst_available,
        hsm_available,
        ipi_available,
        rfence_available,
    ) = if base_available {
        let impl_id = base_call(SBI_BASE_GET_IMPL_ID, 0);
        let impl_version = base_call(SBI_BASE_GET_IMPL_VERSION, 0);
        let srst = base_call(SBI_BASE_PROBE_EXTENSION, SBI_EXT_SRST);
        let srst_available = srst.is_ok() && srst.value != 0;
        let extension_available = |extension| {
            let ret = base_call(SBI_BASE_PROBE_EXTENSION, extension);
            ret.is_ok() && ret.value != 0
        };
        let hsm_available = extension_available(SBI_EXT_HSM);
        let ipi_available = extension_available(SBI_EXT_IPI);
        let rfence_available = extension_available(SBI_EXT_RFENCE);

        SBI_SPEC_VERSION.store(spec.value, Ordering::Release);
        if impl_id.is_ok() {
            SBI_IMPL_ID.store(impl_id.value, Ordering::Release);
        }
        if impl_version.is_ok() {
            SBI_IMPL_VERSION.store(impl_version.value, Ordering::Release);
        }
        SBI_SRST_CAPABILITY.store(
            if srst_available {
                CAPABILITY_AVAILABLE
            } else {
                CAPABILITY_UNAVAILABLE
            },
            Ordering::Release,
        );
        SBI_HSM_AVAILABLE.store(hsm_available, Ordering::Release);
        SBI_IPI_AVAILABLE.store(ipi_available, Ordering::Release);
        SBI_RFENCE_AVAILABLE.store(rfence_available, Ordering::Release);

        (
            Some(spec.value),
            impl_id.is_ok().then_some(impl_id.value),
            impl_version.is_ok().then_some(impl_version.value),
            srst_available,
            hsm_available,
            ipi_available,
            rfence_available,
        )
    } else {
        SBI_SRST_CAPABILITY.store(CAPABILITY_UNAVAILABLE, Ordering::Release);
        SBI_HSM_AVAILABLE.store(false, Ordering::Release);
        SBI_IPI_AVAILABLE.store(false, Ordering::Release);
        SBI_RFENCE_AVAILABLE.store(false, Ordering::Release);
        (None, None, None, false, false, false, false)
    };

    SBI_INITIALIZED.store(true, Ordering::Release);
    SbiInfo {
        base_available,
        spec_version,
        implementation_id,
        implementation_version,
        srst_available,
        hsm_available,
        ipi_available,
        rfence_available,
    }
}

#[inline]
pub fn initialized() -> bool {
    SBI_INITIALIZED.load(Ordering::Acquire)
}

#[inline]
pub fn base_available() -> bool {
    SBI_BASE_AVAILABLE.load(Ordering::Acquire)
}

#[inline]
pub fn spec_version() -> Option<usize> {
    base_available().then(|| SBI_SPEC_VERSION.load(Ordering::Acquire))
}

#[inline]
pub fn implementation_id() -> Option<usize> {
    let value = SBI_IMPL_ID.load(Ordering::Acquire);
    (value != usize::MAX).then_some(value)
}

#[inline]
pub fn implementation_version() -> Option<usize> {
    implementation_id().map(|_| SBI_IMPL_VERSION.load(Ordering::Acquire))
}

#[inline]
pub fn probe_extension(extension: usize) -> bool {
    if !base_available() {
        return false;
    }
    let ret = base_call(SBI_BASE_PROBE_EXTENSION, extension);
    ret.is_ok() && ret.value != 0
}

#[inline]
pub fn hsm_available() -> bool {
    SBI_HSM_AVAILABLE.load(Ordering::Acquire)
}

#[inline]
pub fn ipi_available() -> bool {
    SBI_IPI_AVAILABLE.load(Ordering::Acquire)
}

#[inline]
pub fn rfence_available() -> bool {
    SBI_RFENCE_AVAILABLE.load(Ordering::Acquire)
}

#[inline]
pub fn hart_start(hart_id: usize, start_addr: usize, opaque: usize) -> SbiRet {
    call(
        SBI_EXT_HSM,
        SBI_HSM_HART_START,
        [hart_id, start_addr, opaque, 0, 0, 0],
    )
}

#[inline]
pub fn hart_stop() -> SbiRet {
    call(SBI_EXT_HSM, SBI_HSM_HART_STOP, [0; 6])
}

#[inline]
pub fn hart_get_status(hart_id: usize) -> SbiRet {
    call(
        SBI_EXT_HSM,
        SBI_HSM_HART_GET_STATUS,
        [hart_id, 0, 0, 0, 0, 0],
    )
}

#[inline]
pub fn send_ipi(hart_mask: usize, hart_mask_base: usize) -> SbiRet {
    call(
        SBI_EXT_IPI,
        SBI_IPI_SEND_IPI,
        [hart_mask, hart_mask_base, 0, 0, 0, 0],
    )
}

#[inline]
pub fn remote_fence_i(hart_mask: usize, hart_mask_base: usize) -> SbiRet {
    call(
        SBI_EXT_RFENCE,
        SBI_RFENCE_REMOTE_FENCE_I,
        [hart_mask, hart_mask_base, 0, 0, 0, 0],
    )
}

#[inline]
pub fn remote_sfence_vma(
    hart_mask: usize,
    hart_mask_base: usize,
    start_addr: usize,
    size: usize,
) -> SbiRet {
    call(
        SBI_EXT_RFENCE,
        SBI_RFENCE_REMOTE_SFENCE_VMA,
        [hart_mask, hart_mask_base, start_addr, size, 0, 0],
    )
}

#[inline]
pub fn remote_sfence_vma_asid(
    hart_mask: usize,
    hart_mask_base: usize,
    start_addr: usize,
    size: usize,
    asid: usize,
) -> SbiRet {
    call(
        SBI_EXT_RFENCE,
        SBI_RFENCE_REMOTE_SFENCE_VMA_ASID,
        [hart_mask, hart_mask_base, start_addr, size, asid, 0],
    )
}

#[inline]
pub fn set_timer(deadline: u64) -> SbiRet {
    call(
        SBI_EXT_TIME,
        SBI_TIME_SET_TIMER,
        [deadline as usize, 0, 0, 0, 0, 0],
    )
}

/// SBI v0.1 legacy `set_timer`（EID=0）。RV64 直接在 a0 传 64-bit deadline。
#[inline]
pub fn legacy_set_timer(deadline: u64) {
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") deadline as usize => _,
            lateout("a1") _,
            lateout("a2") _,
            lateout("a3") _,
            lateout("a4") _,
            lateout("a5") _,
            lateout("a6") _,
            in("a7") 0usize,
            options(nostack)
        );
    }
}

#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetType {
    Shutdown = 0,
    ColdReboot = 1,
    WarmReboot = 2,
}

#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetReason {
    NoReason = 0,
    SystemFailure = 1,
}

#[inline]
pub fn system_reset(reset_type: ResetType, reason: ResetReason) -> SbiRet {
    call(
        SBI_EXT_SRST,
        SBI_SRST_SYSTEM_RESET,
        [reset_type as usize, reason as usize, 0, 0, 0, 0],
    )
}

/// fatal trap 使用的无分配停机路径。成功的 SRST 调用按规范不会返回。
pub fn emergency_shutdown() -> ! {
    let capability = SBI_SRST_CAPABILITY.load(Ordering::Acquire);
    if capability != CAPABILITY_UNAVAILABLE {
        let _ = system_reset(ResetType::Shutdown, ResetReason::SystemFailure);
    }

    unsafe { core::arch::asm!("csrci sstatus, 2", options(nomem, nostack)) };
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}

//! RISC-V Supervisor Binary Interface 封装。
//!
//! 基于 `sbi-rt` 封装 BASE、TIME、SRST 以及 SMP 所需的 HSM、IPI、RFENCE 扩展。

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

pub const SBI_EXT_BASE: usize = 0x10;
pub const SBI_EXT_TIME: usize = 0x5449_4d45;
pub const SBI_EXT_SRST: usize = 0x5352_5354;
pub const SBI_EXT_HSM: usize = 0x4853_4d;
pub const SBI_EXT_IPI: usize = 0x7350_49;
pub const SBI_EXT_RFENCE: usize = 0x5246_4e43;

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

impl From<sbi_rt::SbiRet> for SbiRet {
    #[inline]
    fn from(ret: sbi_rt::SbiRet) -> Self {
        Self {
            error: ret.error as isize,
            value: ret.value,
        }
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

#[derive(Clone, Copy)]
struct RawExtension(usize);

impl sbi_rt::Extension for RawExtension {
    #[inline]
    fn extension_id(&self) -> usize {
        self.0
    }
}

/// 探测 BASE，并缓存当前阶段使用的固件能力。
pub fn init() -> SbiInfo {
    let spec = sbi_rt::get_spec_version();
    let spec_version = (spec.major() << 24) | spec.minor();
    let implementation_id = sbi_rt::get_sbi_impl_id();
    let implementation_version = sbi_rt::get_sbi_impl_version();
    let srst_available = sbi_rt::probe_extension(sbi_rt::Reset).is_available();
    let hsm_available = sbi_rt::probe_extension(sbi_rt::Hsm).is_available();
    let ipi_available = sbi_rt::probe_extension(sbi_rt::Ipi).is_available();
    let rfence_available = sbi_rt::probe_extension(sbi_rt::Fence).is_available();

    SBI_BASE_AVAILABLE.store(true, Ordering::Release);
    SBI_SPEC_VERSION.store(spec_version, Ordering::Release);
    SBI_IMPL_ID.store(implementation_id, Ordering::Release);
    SBI_IMPL_VERSION.store(implementation_version, Ordering::Release);
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
    SBI_INITIALIZED.store(true, Ordering::Release);

    SbiInfo {
        base_available: true,
        spec_version: Some(spec_version),
        implementation_id: Some(implementation_id),
        implementation_version: Some(implementation_version),
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
    base_available() && sbi_rt::probe_extension(RawExtension(extension)).is_available()
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
    sbi_rt::hart_start(hart_id, start_addr, opaque).into()
}

#[inline]
pub fn hart_stop() -> SbiRet {
    sbi_rt::hart_stop().into()
}

#[inline]
pub fn hart_get_status(hart_id: usize) -> SbiRet {
    sbi_rt::hart_get_status(hart_id).into()
}

#[inline]
pub fn send_ipi(hart_mask: usize, hart_mask_base: usize) -> SbiRet {
    sbi_rt::send_ipi(sbi_rt::HartMask::from_mask_base(hart_mask, hart_mask_base)).into()
}

#[inline]
pub fn remote_fence_i(hart_mask: usize, hart_mask_base: usize) -> SbiRet {
    sbi_rt::remote_fence_i(sbi_rt::HartMask::from_mask_base(hart_mask, hart_mask_base)).into()
}

#[inline]
pub fn remote_sfence_vma(
    hart_mask: usize,
    hart_mask_base: usize,
    start_addr: usize,
    size: usize,
) -> SbiRet {
    sbi_rt::remote_sfence_vma(
        sbi_rt::HartMask::from_mask_base(hart_mask, hart_mask_base),
        start_addr,
        size,
    )
    .into()
}

#[inline]
pub fn remote_sfence_vma_asid(
    hart_mask: usize,
    hart_mask_base: usize,
    start_addr: usize,
    size: usize,
    asid: usize,
) -> SbiRet {
    sbi_rt::remote_sfence_vma_asid(
        sbi_rt::HartMask::from_mask_base(hart_mask, hart_mask_base),
        start_addr,
        size,
        asid,
    )
    .into()
}

#[inline]
pub fn set_timer(deadline: u64) -> SbiRet {
    sbi_rt::set_timer(deadline).into()
}

/// SBI v0.1 legacy `set_timer`（EID=0）。
#[inline]
#[allow(deprecated)]
pub fn legacy_set_timer(deadline: u64) {
    let _ = sbi_rt::legacy::set_timer(deadline);
}

#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetType {
    Shutdown = 0,
    ColdReboot = 1,
    WarmReboot = 2,
}

impl sbi_rt::ResetType for ResetType {
    #[inline]
    fn raw(&self) -> u32 {
        *self as u32
    }
}

#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetReason {
    NoReason = 0,
    SystemFailure = 1,
}

impl sbi_rt::ResetReason for ResetReason {
    #[inline]
    fn raw(&self) -> u32 {
        *self as u32
    }
}

#[inline]
pub fn system_reset(reset_type: ResetType, reason: ResetReason) -> SbiRet {
    sbi_rt::system_reset(reset_type, reason).into()
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

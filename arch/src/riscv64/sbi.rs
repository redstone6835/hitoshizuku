//! RISC-V Supervisor Binary Interface 封装。
//!
//! 封装 BASE、TIME、SRST、PMU 以及 SMP 所需的 HSM、IPI、RFENCE 扩展。

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use general::dev::pmu::{PmuBackendOps, PmuCounterInfo, PmuCounterKind, PmuError, install_backend};

pub const SBI_EXT_BASE: usize = 0x10;
pub const SBI_EXT_TIME: usize = 0x5449_4d45;
pub const SBI_EXT_SRST: usize = 0x5352_5354;
pub const SBI_EXT_HSM: usize = 0x4853_4d;
pub const SBI_EXT_IPI: usize = 0x7350_49;
pub const SBI_EXT_RFENCE: usize = 0x5246_4e43;
pub const SBI_EXT_PMU: usize = 0x50_4d_55;

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
const SBI_PMU_NUM_COUNTERS: usize = 0;
const SBI_PMU_COUNTER_GET_INFO: usize = 1;
const SBI_PMU_COUNTER_CONFIG_MATCHING: usize = 2;
const SBI_PMU_COUNTER_START: usize = 3;
const SBI_PMU_COUNTER_STOP: usize = 4;
const SBI_PMU_COUNTER_FW_READ: usize = 5;

const SBI_PMU_CFG_FLAG_CLEAR_VALUE: usize = 1 << 1;
const SBI_PMU_START_SET_INIT_VALUE: usize = 1 << 0;
const SBI_PMU_STOP_FLAG_RESET: usize = 1 << 0;
const SBI_PMU_COUNTER_INFO_CSR_MASK: usize = 0x0fff;
const SBI_PMU_COUNTER_INFO_WIDTH_SHIFT: usize = 12;
const SBI_PMU_COUNTER_INFO_WIDTH_MASK: usize = 0x3f << SBI_PMU_COUNTER_INFO_WIDTH_SHIFT;
const SBI_PMU_COUNTER_INFO_TYPE_FIRMWARE: usize = 1usize << (usize::BITS - 1);
const SBI_PMU_EVENT_INDEX_MASK: u32 = (1 << 20) - 1;
const SBI_PMU_HW_CPU_CYCLES_EVENT: u32 = 1;
const SBI_PMU_HW_INSTRUCTIONS_EVENT: u32 = 2;
const SBI_PMU_FIXED_CYCLE_COUNTER: usize = 0;
const SBI_PMU_FIXED_INSTRUCTION_COUNTER: usize = 2;

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
static SBI_PMU_AVAILABLE: AtomicBool = AtomicBool::new(false);
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
    pub pmu_available: bool,
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
        pmu_available,
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
        let pmu_available = extension_available(SBI_EXT_PMU);

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
        SBI_PMU_AVAILABLE.store(pmu_available, Ordering::Release);

        (
            Some(spec.value),
            impl_id.is_ok().then_some(impl_id.value),
            impl_version.is_ok().then_some(impl_version.value),
            srst_available,
            hsm_available,
            ipi_available,
            rfence_available,
            pmu_available,
        )
    } else {
        SBI_SRST_CAPABILITY.store(CAPABILITY_UNAVAILABLE, Ordering::Release);
        SBI_HSM_AVAILABLE.store(false, Ordering::Release);
        SBI_IPI_AVAILABLE.store(false, Ordering::Release);
        SBI_RFENCE_AVAILABLE.store(false, Ordering::Release);
        SBI_PMU_AVAILABLE.store(false, Ordering::Release);
        (None, None, None, false, false, false, false, false)
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
        pmu_available,
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
pub fn pmu_available() -> bool {
    SBI_PMU_AVAILABLE.load(Ordering::Acquire)
}

#[inline]
pub fn pmu_num_counters() -> SbiRet {
    call(SBI_EXT_PMU, SBI_PMU_NUM_COUNTERS, [0; 6])
}

#[inline]
pub fn pmu_counter_get_info(counter_idx: usize) -> SbiRet {
    call(
        SBI_EXT_PMU,
        SBI_PMU_COUNTER_GET_INFO,
        [counter_idx, 0, 0, 0, 0, 0],
    )
}

#[inline]
pub fn pmu_counter_config_matching(
    counter_idx_base: usize,
    counter_idx_mask: usize,
    config_flags: usize,
    event_idx: u32,
    event_data: u64,
) -> SbiRet {
    call(
        SBI_EXT_PMU,
        SBI_PMU_COUNTER_CONFIG_MATCHING,
        [
            counter_idx_base,
            counter_idx_mask,
            config_flags,
            event_idx as usize,
            event_data as usize,
            0,
        ],
    )
}

#[inline]
pub fn pmu_counter_start(
    counter_idx_base: usize,
    counter_idx_mask: usize,
    start_flags: usize,
    initial_value: u64,
) -> SbiRet {
    call(
        SBI_EXT_PMU,
        SBI_PMU_COUNTER_START,
        [
            counter_idx_base,
            counter_idx_mask,
            start_flags,
            initial_value as usize,
            0,
            0,
        ],
    )
}

#[inline]
pub fn pmu_counter_stop(
    counter_idx_base: usize,
    counter_idx_mask: usize,
    stop_flags: usize,
) -> SbiRet {
    call(
        SBI_EXT_PMU,
        SBI_PMU_COUNTER_STOP,
        [counter_idx_base, counter_idx_mask, stop_flags, 0, 0, 0],
    )
}

#[inline]
pub fn pmu_counter_fw_read(counter_idx: usize) -> SbiRet {
    call(
        SBI_EXT_PMU,
        SBI_PMU_COUNTER_FW_READ,
        [counter_idx, 0, 0, 0, 0, 0],
    )
}

fn map_pmu_error(error: isize) -> PmuError {
    match error {
        SBI_ERR_NOT_SUPPORTED | SBI_ERR_DENIED => PmuError::Unsupported,
        SBI_ERR_INVALID_PARAM | SBI_ERR_INVALID_ADDRESS => PmuError::Invalid,
        SBI_ERR_ALREADY_STARTED => PmuError::AlreadyRunning,
        SBI_ERR_ALREADY_STOPPED => PmuError::NotRunning,
        other => PmuError::Backend(other),
    }
}

fn pmu_backend_num_counters() -> Result<usize, PmuError> {
    if !pmu_available() {
        return Err(PmuError::NoBackend);
    }
    let ret = pmu_num_counters();
    if !ret.is_ok() {
        return Err(map_pmu_error(ret.error));
    }
    if ret.value == 0 {
        return Err(PmuError::Unsupported);
    }
    Ok(ret.value)
}

fn pmu_backend_valid_counter_mask() -> Result<usize, PmuError> {
    let counter_count = pmu_backend_num_counters()?;
    if counter_count > usize::BITS as usize {
        // 当前 backend 使用一个 XLEN 宽 SBI counter mask；更大的实现必须按
        // counter_idx_base 分窗，不能静默截断高位 counter。
        return Err(PmuError::Unsupported);
    }

    let mut mask = 0usize;
    for counter_idx in 0..counter_count {
        let ret = pmu_counter_get_info(counter_idx);
        if ret.is_ok() {
            mask |= 1usize << counter_idx;
            continue;
        }
        if matches!(ret.error, SBI_ERR_INVALID_PARAM | SBI_ERR_NOT_SUPPORTED) {
            continue;
        }
        return Err(map_pmu_error(ret.error));
    }
    if mask == 0 {
        Err(PmuError::Unsupported)
    } else {
        Ok(mask)
    }
}

fn pmu_backend_counter_info(counter_idx: usize) -> Result<PmuCounterInfo, PmuError> {
    let ret = pmu_counter_get_info(counter_idx);
    if !ret.is_ok() {
        return Err(map_pmu_error(ret.error));
    }
    if ret.value & SBI_PMU_COUNTER_INFO_TYPE_FIRMWARE != 0 {
        return Ok(PmuCounterInfo::firmware(counter_idx));
    }
    let csr = (ret.value & SBI_PMU_COUNTER_INFO_CSR_MASK) as u16;
    let width = ((ret.value & SBI_PMU_COUNTER_INFO_WIDTH_MASK) >> SBI_PMU_COUNTER_INFO_WIDTH_SHIFT)
        as u8
        + 1;
    PmuCounterInfo::hardware(counter_idx, csr, width).ok_or(PmuError::Invalid)
}

fn pmu_backend_configure(
    counter_mask: usize,
    event_idx: u32,
    event_data: u64,
) -> Result<usize, PmuError> {
    if counter_mask == 0 || event_idx == 0 || event_idx & !SBI_PMU_EVENT_INDEX_MASK != 0 {
        return Err(PmuError::Invalid);
    }
    if let Some(counter_idx) = fixed_counter_for_event(event_idx)
        && counter_mask & (1usize << counter_idx) == 0
    {
        // OpenSBI 在没有 Sscofpmf 时会优先返回 fixed counter，即使调用者已从
        // mask 中排除它。该 counter 已被另一 session 占用时必须在 ecall 前拒绝，
        // 否则固件可能清零正在使用的全局 cycle/instret。
        return Err(PmuError::Unsupported);
    }
    let config_flags = if fixed_counter_for_event(event_idx).is_some() {
        // cycle/instret 是架构共享、持续运行的 fixed counter。配置它们时不能清零
        // 全局值；session 对 fixed counter 只提供排他读取视图。
        0
    } else {
        SBI_PMU_CFG_FLAG_CLEAR_VALUE
    };
    let ret = pmu_counter_config_matching(0, counter_mask, config_flags, event_idx, event_data);
    if ret.is_ok() {
        Ok(ret.value)
    } else {
        Err(map_pmu_error(ret.error))
    }
}

fn fixed_counter_for_event(event_idx: u32) -> Option<usize> {
    match event_idx {
        SBI_PMU_HW_CPU_CYCLES_EVENT => Some(SBI_PMU_FIXED_CYCLE_COUNTER),
        SBI_PMU_HW_INSTRUCTIONS_EVENT => Some(SBI_PMU_FIXED_INSTRUCTION_COUNTER),
        _ => None,
    }
}

fn is_fixed_counter(counter_idx: usize) -> bool {
    matches!(
        counter_idx,
        SBI_PMU_FIXED_CYCLE_COUNTER | SBI_PMU_FIXED_INSTRUCTION_COUNTER
    )
}

fn single_counter_mask(counter_idx: usize) -> Result<(usize, usize), PmuError> {
    if counter_idx >= usize::BITS as usize {
        return Err(PmuError::Invalid);
    }
    Ok((counter_idx, 1))
}

fn pmu_backend_start(counter_idx: usize, initial_value: Option<u64>) -> Result<(), PmuError> {
    if is_fixed_counter(counter_idx) {
        // fixed cycle/instret 由架构全局维护；停止、清零或重启都会影响内核及其它
        // 使用者。session 只读取其持续递增值。
        return if initial_value.is_some() {
            Err(PmuError::Unsupported)
        } else {
            Ok(())
        };
    }
    let (base, mask) = single_counter_mask(counter_idx)?;
    let flags = if initial_value.is_some() {
        SBI_PMU_START_SET_INIT_VALUE
    } else {
        0
    };
    let ret = pmu_counter_start(base, mask, flags, initial_value.unwrap_or(0));
    if ret.is_ok() {
        Ok(())
    } else {
        Err(map_pmu_error(ret.error))
    }
}

fn pmu_backend_stop(counter_idx: usize, reset: bool) -> Result<(), PmuError> {
    if is_fixed_counter(counter_idx) {
        return Ok(());
    }
    let (base, mask) = single_counter_mask(counter_idx)?;
    let flags = if reset { SBI_PMU_STOP_FLAG_RESET } else { 0 };
    let ret = pmu_counter_stop(base, mask, flags);
    if ret.is_ok() || reset && ret.error == SBI_ERR_ALREADY_STOPPED {
        Ok(())
    } else {
        Err(map_pmu_error(ret.error))
    }
}

fn pmu_enter_critical() -> usize {
    // Safety: 只保存并清除当前 hart 的 SSTATUS.SIE，形成覆盖 SBI PMU ecall 与
    // per-hart CSR 访问的短临界区；不会修改地址空间或其它 hart 状态。
    unsafe { crate::riscv64::trap::Riscv64InterruptOps::save_and_disable() }
}

fn pmu_exit_critical(state: usize) {
    // Safety: `state` 由同一 backend 的 `pmu_enter_critical` 在当前 hart 返回；
    // restore 只按原状态恢复 SSTATUS.SIE。
    unsafe { crate::riscv64::trap::Riscv64InterruptOps::restore_interrupt_state(state) }
}

fn pmu_backend_read(info: PmuCounterInfo) -> Result<u64, PmuError> {
    match info.kind {
        PmuCounterKind::Firmware => {
            let ret = pmu_counter_fw_read(info.index);
            if ret.is_ok() {
                Ok(ret.value as u64)
            } else {
                Err(map_pmu_error(ret.error))
            }
        }
        PmuCounterKind::Hardware { csr, width } => {
            let value = read_hardware_counter(csr)?;
            if width == 64 {
                Ok(value)
            } else {
                Ok(value & ((1u64 << width) - 1))
            }
        }
    }
}

fn read_hardware_counter(csr: u16) -> Result<u64, PmuError> {
    let value = match csr {
        0xc00 => crate::read_csr!(cycle),
        0xc02 => crate::read_csr!(instret),
        0xc03 => crate::read_csr!(hpmcounter3),
        0xc04 => crate::read_csr!(hpmcounter4),
        0xc05 => crate::read_csr!(hpmcounter5),
        0xc06 => crate::read_csr!(hpmcounter6),
        0xc07 => crate::read_csr!(hpmcounter7),
        0xc08 => crate::read_csr!(hpmcounter8),
        0xc09 => crate::read_csr!(hpmcounter9),
        0xc0a => crate::read_csr!(hpmcounter10),
        0xc0b => crate::read_csr!(hpmcounter11),
        0xc0c => crate::read_csr!(hpmcounter12),
        0xc0d => crate::read_csr!(hpmcounter13),
        0xc0e => crate::read_csr!(hpmcounter14),
        0xc0f => crate::read_csr!(hpmcounter15),
        0xc10 => crate::read_csr!(hpmcounter16),
        0xc11 => crate::read_csr!(hpmcounter17),
        0xc12 => crate::read_csr!(hpmcounter18),
        0xc13 => crate::read_csr!(hpmcounter19),
        0xc14 => crate::read_csr!(hpmcounter20),
        0xc15 => crate::read_csr!(hpmcounter21),
        0xc16 => crate::read_csr!(hpmcounter22),
        0xc17 => crate::read_csr!(hpmcounter23),
        0xc18 => crate::read_csr!(hpmcounter24),
        0xc19 => crate::read_csr!(hpmcounter25),
        0xc1a => crate::read_csr!(hpmcounter26),
        0xc1b => crate::read_csr!(hpmcounter27),
        0xc1c => crate::read_csr!(hpmcounter28),
        0xc1d => crate::read_csr!(hpmcounter29),
        0xc1e => crate::read_csr!(hpmcounter30),
        0xc1f => crate::read_csr!(hpmcounter31),
        _ => return Err(PmuError::Unsupported),
    };
    Ok(value as u64)
}

/// 把 SBI PMU 的真实计数入口安装到通用 PMU session 层。
pub fn install_pmu_backend() -> Result<(), PmuError> {
    if !pmu_available() {
        return Err(PmuError::NoBackend);
    }
    install_backend(PmuBackendOps {
        current_cpu_id: super::specific::current_cpu_id,
        valid_counter_mask: pmu_backend_valid_counter_mask,
        counter_info: pmu_backend_counter_info,
        configure: pmu_backend_configure,
        start: pmu_backend_start,
        stop: pmu_backend_stop,
        read: pmu_backend_read,
        enter_critical: pmu_enter_critical,
        exit_critical: pmu_exit_critical,
    })
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

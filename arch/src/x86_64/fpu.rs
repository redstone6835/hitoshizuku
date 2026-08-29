//! x86 浮点/向量扩展状态（x87、MMX、SSE、AVX、AVX-512）。
//!
//! MMX 与 x87 共享寄存器文件，因此不单独维护 MMX 状态。SSE 的 XMM/MXCSR
//! 位于 FXSAVE legacy 区；AVX 及后续扩展由 XSAVE component 表示。该模块只
//! 负责硬件能力与状态搬运，调度器通过 arch hook 决定何时调用它。

use core::arch::x86_64::__cpuid_count;
#[cfg(target_os = "none")]
use core::sync::atomic::AtomicU8;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Once;

/// x87/SSE 状态组件。
pub const XFEATURE_X87: u64 = 1 << 0;
pub const XFEATURE_SSE: u64 = 1 << 1;
/// AVX YMM 高半部分。
pub const XFEATURE_YMM: u64 = 1 << 2;
/// AVX-512 opmask、ZMM_Hi256、Hi16_ZMM。
pub const XFEATURE_OPMASK: u64 = 1 << 5;
pub const XFEATURE_ZMM_HI256: u64 = 1 << 6;
pub const XFEATURE_HI16_ZMM: u64 = 1 << 7;
pub const XFEATURE_AVX512: u64 = XFEATURE_OPMASK | XFEATURE_ZMM_HI256 | XFEATURE_HI16_ZMM;

pub const XFEATURE_BASE: u64 = XFEATURE_X87 | XFEATURE_SSE;
pub const XFEATURE_AVX: u64 = XFEATURE_BASE | XFEATURE_YMM;

/// Return the standard-layout offset and size for an XSAVE component.
/// CPUID leaf `0xD, subleaf n` is the architectural source for these values;
/// hard-coding AVX/AVX-512 offsets would be wrong for future components.
pub fn xsave_component_range(feature: u32) -> Option<(usize, usize)> {
    if feature == 0 || feature >= 64 {
        return None;
    }
    let leaf = __cpuid_count(0xD, feature);
    let size = leaf.eax as usize;
    let offset = leaf.ebx as usize;
    if size == 0 || offset < FXSAVE_AREA_SIZE + XSAVE_HEADER_SIZE {
        return None;
    }
    Some((offset, size))
}

/// FXSAVE legacy 区的大小；完整 XSAVE 区由 CPUID leaf 0xD 给出。
pub const FXSAVE_AREA_SIZE: usize = 512;
pub const XSAVE_ALIGNMENT: usize = 64;
const XSAVE_HEADER_SIZE: usize = 64;
const XSAVE_MIN_SIZE: usize = FXSAVE_AREA_SIZE + XSAVE_HEADER_SIZE;

const fn max_usize(a: usize, b: usize) -> usize {
    if a > b { a } else { b }
}
/// 为尚未动态分配 xstate 的早期任务提供保守上限（含 AVX-512，未含 AMX）。
pub const MAX_XSAVE_SIZE: usize = 4096;
/// Intel/Linux 的保守 MXCSR 可写位掩码。bit 6 保留，不能把它暴露给
/// `FXRSTOR`/ptrace，否则恶意上下文可能触发 #GP。
pub const DEFAULT_MXCSR_MASK: u32 = 0x0000_ffbf;

/// CPU 对 x86 浮点/向量扩展的能力快照。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuFeatures {
    pub fpu: bool,
    pub fxsr: bool,
    pub sse: bool,
    pub xsave: bool,
    pub osxsave: bool,
    pub avx: bool,
    pub avx2: bool,
    pub avx512f: bool,
    pub avx512bw: bool,
    pub avx512vl: bool,
    /// Linux `AT_HWCAP` exposes CPUID.1:EDX at its architectural bit numbers.
    pub hwcap_edx: u32,
    pub xsave_user_mask: u64,
    /// Size reported by CPUID.(EAX=0xD,ECX=0).EBX for the currently enabled
    /// XCR0 mask.  This is not necessarily large enough for AVX/AVX-512.
    pub xsave_size: usize,
    /// Maximum user xstate area size from CPUID.(D,0).ECX.
    pub xsave_max_size: usize,
    /// Required area sizes for the common baseline/AVX/AVX-512 masks.
    pub xsave_avx_size: usize,
    pub xsave_avx512_size: usize,
    pub mxcsr_mask: u32,
}

impl CpuFeatures {
    /// 当前 CPU 上可安全开启的最低 xstate mask。
    pub const fn baseline_mask(self) -> u64 {
        if self.fpu && self.fxsr && self.sse {
            XFEATURE_BASE
        } else {
            0
        }
    }

    /// 返回是否可以开启 YMM 状态。
    pub const fn supports_avx_state(self) -> bool {
        self.avx && self.xsave && self.osxsave && self.xsave_user_mask & XFEATURE_YMM != 0
    }

    /// 返回是否可以开启完整 AVX-512 状态。
    pub const fn supports_avx512_state(self) -> bool {
        self.avx512f
            && self.supports_avx_state()
            && self.xsave_user_mask & XFEATURE_AVX512 == XFEATURE_AVX512
    }
}

/// 启动时选定的 xstate 策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XStatePolicy {
    pub mask: u64,
    pub size: usize,
    pub mxcsr_mask: u32,
}

impl XStatePolicy {
    pub const fn baseline(features: CpuFeatures) -> Self {
        Self {
            mask: features.baseline_mask(),
            // FXSAVE/FXRSTOR only consume the legacy 512-byte area.  Do not
            // advertise the CPUID.(D,0).EBX size here: that value can include
            // components enabled by a firmware-provided XCR0 that this policy
            // intentionally leaves disabled.
            size: FXSAVE_AREA_SIZE,
            mxcsr_mask: if features.mxcsr_mask == 0 {
                DEFAULT_MXCSR_MASK
            } else {
                features.mxcsr_mask
            },
        }
    }

    pub const fn with_avx(self, features: CpuFeatures) -> Self {
        if self.mask != 0 && features.supports_avx_state() {
            Self {
                mask: self.mask | XFEATURE_YMM,
                size: if features.xsave_avx_size >= FXSAVE_AREA_SIZE {
                    features.xsave_avx_size
                } else {
                    self.size
                },
                ..self
            }
        } else {
            self
        }
    }

    pub const fn with_avx512(self, features: CpuFeatures) -> Self {
        if self.mask & XFEATURE_AVX != 0 && features.supports_avx512_state() {
            Self {
                mask: self.mask | XFEATURE_AVX512,
                size: if features.xsave_avx512_size >= self.size {
                    features.xsave_avx512_size
                } else {
                    self.size
                },
                ..self
            }
        } else {
            self
        }
    }
}

impl CpuFeatures {
    /// Return the smallest xstate buffer that can hold `mask`.
    pub const fn size_for_mask(self, mask: u64) -> usize {
        let mut size = FXSAVE_AREA_SIZE;
        if mask & XFEATURE_YMM != 0 {
            // A synthetic/early CPUID snapshot may not publish EBX yet.  An
            // extended image still needs the standard XSAVE header and must
            // never collapse to a legacy 512-byte buffer.
            size = max_usize(max_usize(self.xsave_avx_size, XSAVE_MIN_SIZE), size);
        }
        if mask & XFEATURE_AVX512 != 0 {
            size = max_usize(max_usize(self.xsave_avx512_size, XSAVE_MIN_SIZE), size);
        }
        // Unknown user components (for example AMX) must use the full size
        // reported by CPUID rather than being silently truncated.
        let known = XFEATURE_BASE | XFEATURE_YMM | XFEATURE_AVX512;
        if mask & !known != 0 {
            size = max_usize(max_usize(self.xsave_max_size, XSAVE_MIN_SIZE), size);
        }
        size
    }
}

/// Errors returned when a loader requests an extended user xstate policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XStatePolicyError {
    /// The CPU/OS does not expose the requested component.
    Unsupported,
    /// Mandatory legacy components or component dependencies are missing.
    InvalidMask,
    /// The reported XSAVE area exceeds the bounded per-task allocation.
    TooLarge,
    /// A different policy was requested after boot published the xstate ABI.
    AlreadyConfigured,
}

/// Return the largest common user xstate mask that this CPU and the bounded
/// kernel save area can represent.
///
/// The result is only a capability query; it does not modify XCR0.  Boot code
/// may pass it to [`configure_user_policy`] before publishing the first task.
/// Unknown components (for example AMX tiles) are intentionally omitted until
/// a separately sized owner/context backend exists.
pub fn supported_user_mask() -> u64 {
    best_user_mask_for_features(*init())
}

fn best_user_mask_for_features(features: CpuFeatures) -> u64 {
    let mut mask = features.baseline_mask();
    if mask == 0 {
        return 0;
    }
    if features.supports_avx_state() {
        let candidate = mask | XFEATURE_YMM;
        if features.size_for_mask(candidate) <= MAX_XSAVE_SIZE {
            mask = candidate;
        }
    }
    if features.supports_avx512_state() {
        let candidate = mask | XFEATURE_AVX512;
        if features.size_for_mask(candidate) <= MAX_XSAVE_SIZE {
            mask = candidate;
        }
    }
    mask
}

/// Check whether a CPU can run the selected global xstate policy after its
/// local CR4.OSXSAVE bit has been enabled.  CPUID.OSXSAVE itself is excluded
/// from this check because that bit reflects the local CR4 state before AP
/// setup, not a hardware capability.
fn cpu_supports_policy(features: CpuFeatures, mask: u64) -> bool {
    let known = XFEATURE_BASE | XFEATURE_YMM | XFEATURE_AVX512;
    if features.baseline_mask() != XFEATURE_BASE
        || mask & XFEATURE_BASE != XFEATURE_BASE
        || mask & !known != 0
        || (mask & XFEATURE_AVX512 != 0 && mask & XFEATURE_AVX512 != XFEATURE_AVX512)
        || features.size_for_mask(mask) > MAX_XSAVE_SIZE
    {
        return false;
    }
    if mask == XFEATURE_BASE {
        return true;
    }
    if !features.xsave || mask & !features.xsave_user_mask != 0 {
        return false;
    }
    if mask & XFEATURE_YMM != 0 && !features.avx {
        return false;
    }
    mask & XFEATURE_AVX512 == 0
        || (features.avx512f
            && mask & XFEATURE_YMM != 0
            && features.xsave_user_mask & XFEATURE_AVX512 == XFEATURE_AVX512)
}

/// Enable the largest bounded user xstate policy during boot.
///
/// This must run before any task can execute user code; changing XCR0 after
/// that point would invalidate already-live register images.
pub fn configure_best_user_policy() -> Result<XStatePolicy, XStatePolicyError> {
    if POLICY_CONFIGURED.load(Ordering::Acquire) {
        return Ok(policy());
    }
    configure_user_policy(supported_user_mask())
}

/// Select an xstate policy during early boot.
///
/// This is intentionally a startup-only operation: changing XCR0 while tasks
/// are running would invalidate their saved images.  The default selected by
/// [`init`] remains the 512-byte x87/SSE baseline.
pub fn configure_user_policy(mask: u64) -> Result<XStatePolicy, XStatePolicyError> {
    if POLICY_CONFIGURED.load(Ordering::Acquire) {
        let selected = policy();
        return if selected.mask == mask {
            Ok(selected)
        } else {
            Err(XStatePolicyError::AlreadyConfigured)
        };
    }
    let features = *init();
    let known = XFEATURE_BASE | XFEATURE_YMM | XFEATURE_AVX512;
    if mask & XFEATURE_BASE != XFEATURE_BASE {
        return Err(XStatePolicyError::InvalidMask);
    }
    if mask & !known != 0 || !cpu_supports_policy(features, mask) {
        return Err(XStatePolicyError::Unsupported);
    }
    let size = features.size_for_mask(mask);
    let requested_policy = XStatePolicy {
        mask,
        size,
        mxcsr_mask: if features.mxcsr_mask == 0 {
            DEFAULT_MXCSR_MASK
        } else {
            features.mxcsr_mask
        },
    };
    // Publishing a policy changes the user ABI (including HWCAP), therefore
    // no later caller may reconfigure XCR0 underneath live task images.
    if POLICY_CONFIGURED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        let selected = policy();
        return if selected.mask == mask {
            Ok(selected)
        } else {
            Err(XStatePolicyError::AlreadyConfigured)
        };
    }
    // SAFETY: callers must invoke this only during boot before any task can
    // execute user code; the checks above establish XCR0 dependencies.
    if features.xsave {
        unsafe { write_xcr0(mask) };
        if read_xcr0() != mask {
            return Err(XStatePolicyError::Unsupported);
        }
    } else {
        POLICY_MASK.store(mask, Ordering::Release);
    }
    Ok(requested_policy)
}

static FEATURES: Once<CpuFeatures> = Once::new();
static POLICY_MASK: AtomicU64 = AtomicU64::new(0);
/// Set once the BSP has selected and published the user-visible XCR0 ABI.
/// It deliberately never clears: changing a task's xstate layout after it has
/// run would make XRSTOR accept an image with a different component contract.
static POLICY_CONFIGURED: AtomicBool = AtomicBool::new(false);

/// Whether the kernel context switch may issue XSAVE/XRSTOR.
///
/// This is deliberately a byte-sized, lock-free flag so the naked scheduler
/// entry can test it without calling Rust.  It remains clear until `init()`
/// has enabled CR4.OSXSAVE on the current (BSP) processor.  Hosted builds keep
/// it clear and use their non-privileged fallback paths.
#[cfg(target_os = "none")]
pub(crate) static XSAVE_ENABLED: AtomicU8 = AtomicU8::new(0);

/// Read the policy from an entry-safe atomic without triggering CPUID or the
/// `spin::Once` initializer.  Trap assembly calls into this boundary before
/// any compiler-generated Rust/FPU work is allowed.
#[cfg(target_os = "none")]
#[inline]
pub(crate) fn enabled_mask_raw() -> u64 {
    POLICY_MASK.load(Ordering::Acquire)
}

/// Return whether the current CPU has completed OSXSAVE/XCR0 setup.
#[cfg(target_os = "none")]
#[inline]
pub(crate) fn xsave_enabled() -> bool {
    XSAVE_ENABLED.load(Ordering::Acquire) != 0
}

/// 读取当前 CPU 的 CPUID 能力。
pub fn detect() -> CpuFeatures {
    // Safety: CPUID 在所有 x86_64 CPU 上可执行；leaf 越界时硬件返回零。
    let max_basic = __cpuid_count(0, 0).eax;
    let leaf1 = if max_basic >= 1 {
        __cpuid_count(1, 0)
    } else {
        unsafe { core::mem::zeroed() }
    };
    let leaf7 = if max_basic >= 7 {
        __cpuid_count(7, 0)
    } else {
        unsafe { core::mem::zeroed() }
    };
    let leafd0 = if max_basic >= 0xD {
        __cpuid_count(0xD, 0)
    } else {
        unsafe { core::mem::zeroed() }
    };
    let mxcsr_mask = if max_basic >= 1 {
        // FXSAVE exposes the precise mask after initialization. The architectural
        // default below is used until a task executes FXSAVE for the first time.
        DEFAULT_MXCSR_MASK
    } else {
        0
    };
    let xsave = leaf1.ecx & (1 << 26) != 0;
    let osxsave = leaf1.ecx & (1 << 27) != 0;
    let avx_hw = leaf1.ecx & (1 << 28) != 0;
    let xsave_user_mask = ((leafd0.edx as u64) << 32) | leafd0.eax as u64;
    let (xsave_avx_size, xsave_avx512_size) = if xsave && max_basic >= 0xD {
        // CPUID.(D,n).EAX is the component size and EBX its offset in the
        // compacted standard layout.  Leaf 2 is YMM_Hi128; 5/6/7 are the
        // AVX-512 opmask/ZMM components.  Unknown leaves return zero.
        let component_end = |leaf: u32| {
            let state = __cpuid_count(0xD, leaf);
            (state.ebx as usize).saturating_add(state.eax as usize)
        };
        let avx = component_end(2).max(FXSAVE_AREA_SIZE);
        let avx512 = avx
            .max(component_end(5))
            .max(component_end(6))
            .max(component_end(7));
        (avx, avx512)
    } else {
        (FXSAVE_AREA_SIZE, FXSAVE_AREA_SIZE)
    };
    CpuFeatures {
        fpu: leaf1.edx & (1 << 0) != 0,
        fxsr: leaf1.edx & (1 << 24) != 0,
        sse: leaf1.edx & (1 << 25) != 0,
        xsave,
        osxsave,
        // `avx` describes hardware capability. Whether YMM is exposed to users is
        // decided separately by the selected XCR0 policy after CR4.OSXSAVE is set.
        avx: avx_hw && xsave && xsave_user_mask & XFEATURE_AVX == XFEATURE_AVX,
        avx2: leaf7.ebx & (1 << 5) != 0,
        avx512f: leaf7.ebx & (1 << 16) != 0,
        avx512bw: leaf7.ebx & (1 << 30) != 0,
        avx512vl: leaf7.ebx & (1 << 31) != 0,
        hwcap_edx: leaf1.edx,
        xsave_user_mask,
        xsave_size: if xsave {
            leafd0.ebx as usize
        } else {
            FXSAVE_AREA_SIZE
        },
        xsave_max_size: if xsave {
            leafd0.ecx as usize
        } else {
            FXSAVE_AREA_SIZE
        },
        xsave_avx_size,
        xsave_avx512_size,
        mxcsr_mask,
    }
}

/// 初始化并返回全局能力快照。
///
/// 在裸机目标上，该函数还负责打开 CR0/CR4 的 FPU/SSE/XSAVE 位并设置 XCR0。
/// hosted 单测只探测 CPUID，不执行特权指令。
pub fn init() -> &'static CpuFeatures {
    FEATURES.call_once(|| {
        let mut features = detect();
        let policy = XStatePolicy::baseline(features);
        POLICY_MASK.store(policy.mask, Ordering::Release);
        #[cfg(target_os = "none")]
        unsafe {
            enable_control_registers(features, features.xsave && policy.mask != 0);
            // CPUID.1:ECX.OSXSAVE reports the state of CR4.OSXSAVE, not just
            // hardware capability.  `enable_control_registers` has now set
            // that bit for an XSAVE-capable CPU, so publish the post-enable
            // state in the immutable snapshot used by policy checks.
            if features.xsave && policy.mask != 0 {
                write_xcr0(policy.mask);
            }
            if features.xsave && policy.mask != 0 {
                features.osxsave = true;
                // Publish the scheduler/trap flag only after both CR4 and
                // XCR0 have been committed on this processor.
                XSAVE_ENABLED.store(1, Ordering::Release);
            }
        }
        features
    })
}

/// Initialize per-CPU control registers after the BSP has populated the
/// immutable feature snapshot. CR0/CR4/XCR0 are local architectural state.
#[cfg(target_os = "none")]
pub(crate) fn init_secondary_cpu() -> Result<(), XStatePolicyError> {
    let selected = policy();
    let local = detect();
    let xsave_required = xsave_enabled();
    if !cpu_supports_policy(local, selected.mask) || (xsave_required && !local.xsave) {
        return Err(XStatePolicyError::Unsupported);
    }
    unsafe {
        enable_control_registers(local, xsave_required);
        if xsave_required {
            write_xcr0(selected.mask);
            if read_xcr0() != selected.mask {
                return Err(XStatePolicyError::Unsupported);
            }
            XSAVE_ENABLED.store(1, Ordering::Release);
        }
    }
    Ok(())
}

/// 返回已选策略；未初始化时执行只读 CPUID 探测。
pub fn policy() -> XStatePolicy {
    let features = *init();
    let mask = POLICY_MASK.load(Ordering::Acquire);
    let baseline = XStatePolicy::baseline(features);
    XStatePolicy {
        mask,
        size: features.size_for_mask(mask),
        ..baseline
    }
}

/// 返回启动期实际启用的 xstate mask。
pub fn enabled_mask() -> u64 {
    let _ = init();
    POLICY_MASK.load(Ordering::Acquire)
}

/// 读取 XCR0。hosted 目标返回软件镜像，避免 CPL3 执行 XGETBV。
pub fn read_xcr0() -> u64 {
    #[cfg(target_os = "none")]
    {
        // Safety: init() 仅在设置 CR4.OSXSAVE 后调用此指令。
        unsafe {
            let eax: u32;
            let edx: u32;
            core::arch::asm!("xgetbv", in("ecx") 0u32, out("eax") eax, out("edx") edx, options(nostack));
            return (edx as u64) << 32 | eax as u64;
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        POLICY_MASK.load(Ordering::Acquire)
    }
}

/// 设置 XCR0。仅裸机初始化路径允许调用；hosted 构建只更新软件镜像，便于
/// ABI/ptrace 单元测试在 CPL3 安全运行。
#[cfg(target_os = "none")]
pub unsafe fn write_xcr0(mask: u64) {
    // Safety: 调用者已确认 CPUID.OSXSAVE 且 mask 是 XCR0 用户组件子集。
    unsafe {
        core::arch::asm!(
            "xsetbv",
            in("ecx") 0u32,
            in("eax") mask as u32,
            in("edx") (mask >> 32) as u32,
            options(nostack)
        );
    }
    POLICY_MASK.store(mask, Ordering::Release);
}

#[cfg(not(target_os = "none"))]
pub unsafe fn write_xcr0(mask: u64) {
    POLICY_MASK.store(mask, Ordering::Release);
}

/// 保存当前处理器的 xstate 到 64 字节对齐缓冲区。
///
/// `mask` 必须是 `enabled_mask()` 的子集；传入空指针或过短缓冲区属于调用方
/// 违反契约。hosted 目标只清零目标区，方便 ptrace/布局测试而不执行特权指令。
pub unsafe fn save(area: *mut u8, mask: u64) {
    debug_assert!(!area.is_null());
    #[cfg(target_os = "none")]
    {
        unsafe {
            if mask == XFEATURE_BASE {
                core::arch::asm!("fxsave64 [{area}]", area = in(reg) area, options(nostack));
            } else {
                core::arch::asm!(
                    "xsave64 [{area}]",
                    area = in(reg) area,
                    in("eax") mask as u32,
                    in("edx") (mask >> 32) as u32,
                    options(nostack)
                );
            }
        }
    }
    #[cfg(not(target_os = "none"))]
    unsafe {
        // Hosted fallback deliberately does not pretend to capture process SSE state.
        let size = init().size_for_mask(mask).max(FXSAVE_AREA_SIZE);
        core::ptr::write_bytes(area, 0, size);
    }
}

/// 从缓冲区恢复当前处理器的 xstate。
pub unsafe fn restore(area: *const u8, mask: u64) {
    debug_assert!(!area.is_null());
    #[cfg(target_os = "none")]
    {
        unsafe {
            if mask == XFEATURE_BASE {
                core::arch::asm!("fxrstor64 [{area}]", area = in(reg) area, options(nostack));
            } else {
                core::arch::asm!(
                    "xrstor64 [{area}]",
                    area = in(reg) area,
                    in("eax") mask as u32,
                    in("edx") (mask >> 32) as u32,
                    options(nostack)
                );
            }
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (area, mask);
    }
}

/// 裸机设置 CR0/CR4 的 FPU/SSE/XSAVE 控制位。
#[cfg(target_os = "none")]
unsafe fn enable_control_registers(features: CpuFeatures, enable_xsave: bool) {
    let mut cr0: usize;
    let mut cr4: usize;
    unsafe {
        // Control-register reads are volatile and, like Linux's
        // `native_read_cr*()`, need no memory clobber.
        core::arch::asm!("mov {0}, cr0", out(reg) cr0, options(nostack, nomem));
        core::arch::asm!("mov {0}, cr4", out(reg) cr4, options(nostack, nomem));
    }
    // MP=1, NE=1, EM=0, TS=0；OSFXSR/OSXMMEXCPT 对应 bit 9/10。
    cr0 = (cr0 | (1 << 1) | (1 << 5)) & !((1 << 2) | (1 << 3));
    if features.fxsr {
        cr4 |= 1 << 9;
    }
    if features.sse {
        cr4 |= 1 << 10;
    }
    if enable_xsave && features.xsave && features.baseline_mask() != 0 {
        cr4 |= 1 << 18;
    }
    unsafe {
        // Linux's native CR writers carry a memory clobber.  Changing CR0 or
        // CR4 can alter whether later instructions access FPU state and which
        // protection regime is active, so surrounding initialization stores
        // must remain ordered across these writes.
        core::arch::asm!("mov cr0, {0}", in(reg) cr0, options(nostack));
        core::arch::asm!("mov cr4, {0}", in(reg) cr4, options(nostack));
    }
}

/// 从 FXSAVE 区取出硬件报告的 MXCSR mask；无效值回退到架构默认值。
pub fn validate_mxcsr(mxcsr: u32, mask: u32) -> bool {
    let mask = if mask == 0 { DEFAULT_MXCSR_MASK } else { mask };
    mxcsr & !mask == 0
}

/// Normalize the legacy FXSAVE control words at an arch/kernel boundary.
///
/// Some CPUs report implementation-specific MXCSR capability bits in the
/// saved `MXCSR_MASK` word (for example, bits outside Linux's conservative
/// user policy).  That word is metadata, not user state; retaining it would
/// make a later return-frame validator reject an otherwise valid hardware
/// snapshot.  Clamp both words to the policy before exposing the image to
/// signal/ptrace code or `FXRSTOR`.
pub fn sanitize_fxsave_area(area: &mut [u8]) -> bool {
    if area.len() < 32 {
        return false;
    }
    let mask = policy().mxcsr_mask;
    let mut mxcsr = [0u8; 4];
    mxcsr.copy_from_slice(&area[24..28]);
    let value = u32::from_le_bytes(mxcsr) & mask;
    area[24..28].copy_from_slice(&value.to_le_bytes());
    area[28..32].copy_from_slice(&mask.to_le_bytes());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_policy_is_subset_of_hardware() {
        let f = detect();
        let p = XStatePolicy::baseline(f);
        // The legacy FXSAVE baseline is valid even on CPUs without XSAVE, in
        // which case CPUID leaf 0xD reports no XCR0 mask at all.
        if f.xsave {
            assert_eq!(p.mask & !f.xsave_user_mask, 0);
        }
        if f.fpu && f.fxsr && f.sse {
            assert_eq!(p.mask, XFEATURE_BASE);
        }
    }

    #[test]
    fn mxcsr_validation_rejects_reserved_bits() {
        assert!(validate_mxcsr(0x1f80, DEFAULT_MXCSR_MASK));
        assert!(!validate_mxcsr(0x0040, DEFAULT_MXCSR_MASK));
        assert!(!validate_mxcsr(0x1_0000, DEFAULT_MXCSR_MASK));
        assert!(!validate_mxcsr(0x0040, 0));
    }

    #[test]
    fn xstate_policy_grows_for_vector_components() {
        let f = CpuFeatures {
            fpu: true,
            fxsr: true,
            sse: true,
            xsave: true,
            osxsave: true,
            avx: true,
            avx512f: true,
            xsave_user_mask: XFEATURE_BASE | XFEATURE_YMM | XFEATURE_AVX512,
            xsave_avx_size: 832,
            xsave_avx512_size: 2688,
            xsave_max_size: 2688,
            ..CpuFeatures::default()
        };
        let baseline = XStatePolicy::baseline(f);
        assert_eq!(baseline.size, FXSAVE_AREA_SIZE);
        let avx = baseline.with_avx(f);
        assert_eq!(avx.size, 832);
        let avx512 = avx.with_avx512(f);
        assert_eq!(avx512.size, 2688);
    }

    #[test]
    fn best_policy_stops_before_an_oversized_avx512_image() {
        let features = CpuFeatures {
            fpu: true,
            fxsr: true,
            sse: true,
            xsave: true,
            osxsave: true,
            avx: true,
            avx512f: true,
            xsave_user_mask: XFEATURE_BASE | XFEATURE_YMM | XFEATURE_AVX512,
            xsave_avx_size: 832,
            xsave_avx512_size: MAX_XSAVE_SIZE + 64,
            xsave_max_size: MAX_XSAVE_SIZE + 64,
            ..CpuFeatures::default()
        };

        assert_eq!(best_user_mask_for_features(features), XFEATURE_AVX);
    }

    #[test]
    fn secondary_cpu_policy_requires_every_enabled_component() {
        let features = CpuFeatures {
            fpu: true,
            fxsr: true,
            sse: true,
            xsave: true,
            avx: true,
            avx512f: true,
            xsave_user_mask: XFEATURE_BASE | XFEATURE_YMM | XFEATURE_AVX512,
            xsave_avx_size: 832,
            xsave_avx512_size: 2688,
            xsave_max_size: 2688,
            ..CpuFeatures::default()
        };
        assert!(cpu_supports_policy(features, XFEATURE_AVX));

        let mut missing_ymm = features;
        missing_ymm.xsave_user_mask &= !XFEATURE_YMM;
        missing_ymm.avx = false;
        assert!(!cpu_supports_policy(missing_ymm, XFEATURE_AVX));

        assert!(!cpu_supports_policy(
            features,
            XFEATURE_AVX | XFEATURE_OPMASK
        ));
    }
}

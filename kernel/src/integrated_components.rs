//! 直接链接进内核的集成组件执行器。
//!
//! 集成组件不是 ELM cell，不进入 elm-mgr。设备阶段在固件设备枚举前执行，运行时
//! 阶段在调度器基础环境建立后执行；已经成功初始化的组件始终按描述符逆序终结。

use core::mem::{align_of, size_of};
use core::slice;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kernel_symbols::{
    KERNEL_INTEGRATED_PHASE_DEVICE, KERNEL_INTEGRATED_PHASE_RUNTIME, KernelIntegratedComponentV1,
};

unsafe extern "C" {
    static __kernel_integrated_components_start: u8;
    static __kernel_integrated_components_end: u8;
}

const MAX_INTEGRATED_COMPONENTS: usize = 256;
const INITIALIZED_WORDS: usize = MAX_INTEGRATED_COMPONENTS / u64::BITS as usize;

static INITIALIZED: [AtomicU64; INITIALIZED_WORDS] =
    [const { AtomicU64::new(0) }; INITIALIZED_WORDS];
static INITIALIZED_COUNT: AtomicUsize = AtomicUsize::new(0);
static DEVICE_PHASE_DONE: AtomicBool = AtomicBool::new(false);
static RUNTIME_PHASE_DONE: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
static FINALIZED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntegratedPhase {
    Device,
    Runtime,
}

impl IntegratedPhase {
    const fn raw(self) -> u16 {
        match self {
            Self::Device => KERNEL_INTEGRATED_PHASE_DEVICE,
            Self::Runtime => KERNEL_INTEGRATED_PHASE_RUNTIME,
        }
    }

    const fn state(self) -> &'static AtomicBool {
        match self {
            Self::Device => &DEVICE_PHASE_DONE,
            Self::Runtime => &RUNTIME_PHASE_DONE,
        }
    }
}

fn descriptors() -> Result<&'static [KernelIntegratedComponentV1], &'static str> {
    let start = core::ptr::addr_of!(__kernel_integrated_components_start) as usize;
    let end = core::ptr::addr_of!(__kernel_integrated_components_end) as usize;
    let bytes = end.checked_sub(start).ok_or("集成组件链接区范围倒置")?;
    if start % align_of::<KernelIntegratedComponentV1>() != 0
        || bytes % size_of::<KernelIntegratedComponentV1>() != 0
    {
        return Err("集成组件链接区未按完整描述符对齐");
    }
    let count = bytes / size_of::<KernelIntegratedComponentV1>();
    if count > MAX_INTEGRATED_COMPONENTS {
        return Err("集成组件数量超过执行器容量");
    }
    // Safety: 链接脚本提供同一只读段的起止符号，上面已验证顺序、对齐和完整元素长度。
    Ok(unsafe { slice::from_raw_parts(start as *const KernelIntegratedComponentV1, count) })
}

fn is_initialized(index: usize) -> bool {
    let word = index / u64::BITS as usize;
    let bit = index % u64::BITS as usize;
    INITIALIZED[word].load(Ordering::Acquire) & (1u64 << bit) != 0
}

fn mark_initialized(index: usize) {
    let word = index / u64::BITS as usize;
    let bit = index % u64::BITS as usize;
    INITIALIZED[word].fetch_or(1u64 << bit, Ordering::AcqRel);
    INITIALIZED_COUNT.fetch_add(1, Ordering::AcqRel);
}

fn clear_initialized(index: usize) {
    let word = index / u64::BITS as usize;
    let bit = index % u64::BITS as usize;
    let previous = INITIALIZED[word].fetch_and(!(1u64 << bit), Ordering::AcqRel);
    if previous & (1u64 << bit) != 0 {
        INITIALIZED_COUNT.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) fn initialize_phase(phase: IntegratedPhase) -> Result<usize, &'static str> {
    if FAILED.load(Ordering::Acquire) || FINALIZED.load(Ordering::Acquire) {
        return Err("集成组件执行器已失败或已经终结");
    }
    if phase == IntegratedPhase::Runtime && !DEVICE_PHASE_DONE.load(Ordering::Acquire) {
        return Err("运行时组件不能早于设备组件初始化");
    }
    if phase.state().swap(true, Ordering::AcqRel) {
        return Err("集成组件阶段只能初始化一次");
    }

    let components = descriptors()?;
    let interface_hash = crate::elm::kernel_interface_profile_hash()?;
    for (index, component) in components.iter().enumerate() {
        if !component.valid(interface_hash) {
            log::error!(
                "[kernel-integrated] 描述符无效: index={} magic={:#x} abi={} size={} phase={} flags={:#x} component_hash={:02x?} kernel_hash={:02x?} init={:#x} finalize={:#x}",
                index,
                component.magic,
                component.abi_version,
                component.struct_size,
                component.phase,
                component.flags,
                component.interface_hash,
                interface_hash,
                component.initialize as usize,
                component.finalize as usize,
            );
            FAILED.store(true, Ordering::Release);
            finalize_initialized(components);
            return Err("集成组件描述符无效");
        }
    }

    let mut initialized = 0usize;
    for (index, component) in components.iter().enumerate() {
        if component.phase != phase.raw() {
            continue;
        }
        if (component.initialize)() != 0 {
            FAILED.store(true, Ordering::Release);
            finalize_initialized(components);
            return Err("集成组件初始化失败");
        }
        mark_initialized(index);
        initialized += 1;
    }
    Ok(initialized)
}

pub(crate) fn finalize_all() -> Result<usize, &'static str> {
    if FINALIZED.swap(true, Ordering::AcqRel) {
        return Ok(0);
    }
    let components = descriptors()?;
    let (finalized, failed) = finalize_initialized(components);
    if failed {
        Err("一个或多个集成组件终结失败")
    } else {
        Ok(finalized)
    }
}

fn finalize_initialized(components: &[KernelIntegratedComponentV1]) -> (usize, bool) {
    let mut finalized = 0usize;
    let mut failed = false;
    for (index, component) in components.iter().enumerate().rev() {
        if !is_initialized(index) {
            continue;
        }
        if (component.finalize)() == 0 {
            finalized += 1;
        } else {
            failed = true;
        }
        clear_initialized(index);
    }
    debug_assert_eq!(INITIALIZED_COUNT.load(Ordering::Acquire), 0);
    (finalized, failed)
}

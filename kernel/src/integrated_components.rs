//! 直接链接内核组件的普通 initcall 执行器。
//!
//! 这里处理的对象不是 ELM cell，不进入 elm-mgr，也没有 EBI、来源或代际。构建工具在
//! `y` 模式把组件描述符链接到专用只读段，内核按链接顺序初始化，并在有序关机时逆序终结。

use core::mem::{align_of, size_of};
use core::slice;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kernel_symbols::KernelIntegratedComponentV1;

unsafe extern "C" {
    static __kernel_integrated_components_start: u8;
    static __kernel_integrated_components_end: u8;
}

static INITIALIZED: AtomicUsize = AtomicUsize::new(0);
static INITIALIZATION_STARTED: AtomicBool = AtomicBool::new(false);
static FINALIZED: AtomicBool = AtomicBool::new(false);

fn descriptors() -> Result<&'static [KernelIntegratedComponentV1], &'static str> {
    let start = core::ptr::addr_of!(__kernel_integrated_components_start) as usize;
    let end = core::ptr::addr_of!(__kernel_integrated_components_end) as usize;
    let bytes = end.checked_sub(start).ok_or("集成组件链接区范围倒置")?;
    if start % align_of::<KernelIntegratedComponentV1>() != 0
        || bytes % size_of::<KernelIntegratedComponentV1>() != 0
    {
        return Err("集成组件链接区未按完整描述符对齐");
    }
    // Safety: 链接脚本提供同一只读段的起止符号，上面已验证顺序、对齐和完整元素长度。
    Ok(unsafe {
        slice::from_raw_parts(
            start as *const KernelIntegratedComponentV1,
            bytes / size_of::<KernelIntegratedComponentV1>(),
        )
    })
}

pub(crate) fn initialize_all() -> Result<usize, &'static str> {
    let components = descriptors()?;
    let interface_hash = crate::elm::kernel_interface_profile_hash()?;
    if INITIALIZATION_STARTED.swap(true, Ordering::AcqRel) || FINALIZED.load(Ordering::Acquire) {
        return Err("集成组件只能初始化一次");
    }
    for (index, component) in components.iter().enumerate() {
        if !component.valid(interface_hash) {
            rollback(components, index);
            return Err("集成组件描述符无效");
        }
        if (component.initialize)() != 0 {
            rollback(components, index);
            return Err("集成组件初始化失败");
        }
        INITIALIZED.store(index + 1, Ordering::Release);
    }
    Ok(components.len())
}

pub(crate) fn finalize_all() -> Result<usize, &'static str> {
    if FINALIZED.swap(true, Ordering::AcqRel) {
        return Ok(0);
    }
    let components = descriptors()?;
    let initialized = INITIALIZED.swap(0, Ordering::AcqRel).min(components.len());
    let mut finalized = 0usize;
    let mut failed = false;
    for component in components[..initialized].iter().rev() {
        if (component.finalize)() == 0 {
            finalized += 1;
        } else {
            failed = true;
        }
    }
    if failed {
        Err("一个或多个集成组件终结失败")
    } else {
        Ok(finalized)
    }
}

fn rollback(components: &[KernelIntegratedComponentV1], initialized: usize) {
    for component in components[..initialized].iter().rev() {
        let _ = (component.finalize)();
    }
    INITIALIZED.store(0, Ordering::Release);
    FINALIZED.store(true, Ordering::Release);
}

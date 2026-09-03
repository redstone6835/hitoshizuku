//! x86_64 特权寄存器、屏障和地址转换的集中封装。

#[cfg(target_os = "none")]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;

/// 编译器级屏障，不改变处理器状态。
///
/// Linux 将 `barrier()` 与设备所需的硬件 fence 分开；DMA coherent 路径通常
/// 只需要前者来阻止编译器重排，MMIO 路径则使用下面的架构屏障。
#[inline(always)]
#[cfg(test)]
pub fn compiler_barrier() {
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
}

/// 排序此前的设备/内存读取，对应 Linux x86 `__rmb()`。
#[inline(always)]
#[cfg(test)]
pub fn read_memory_barrier() {
    #[cfg(target_os = "none")]
    unsafe {
        // 不使用 `nomem`，让 LLVM 也把此指令视作内存屏障。
        core::arch::asm!("lfence", options(nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    core::sync::atomic::fence(Ordering::Acquire);
}

/// 排序此前的设备/内存写入，对应 Linux x86 `__wmb()`。
#[inline(always)]
#[cfg(test)]
pub fn write_memory_barrier() {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("sfence", options(nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    core::sync::atomic::fence(Ordering::Release);
}

/// 完整的处理器内存屏障，对应 Linux x86 `__mb()`。
#[inline(always)]
#[cfg(any(test, not(target_os = "none")))]
pub fn full_memory_barrier() {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("mfence", options(nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    core::sync::atomic::fence(Ordering::SeqCst);
}

/// 忙等循环提示，对应 Linux `cpu_relax()`。
#[inline(always)]
#[cfg(any(test, target_os = "none"))]
pub fn cpu_relax() {
    #[cfg(target_os = "none")]
    unsafe {
        // Linux's native_pause() uses a memory clobber so a polling load/store
        // cannot be moved out of the busy-wait iteration by the compiler.
        core::arch::asm!("pause", options(nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    core::hint::spin_loop();
}

/// 无序读取 TSC。与 Linux `rdtsc()` 一样，不保证与周围内存访问排序。
#[inline(always)]
pub fn rdtsc() -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") low,
            out("edx") high,
            options(nomem, nostack)
        );
    }
    ((high as u64) << 32) | low as u64
}

/// 按程序顺序读取 TSC，与 Linux `rdtsc_ordered()` 的 LFENCE 路径一致。
#[inline(always)]
pub fn rdtsc_ordered() -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        // RDTSC must not be marked `nomem`: the fence is also a compiler
        // ordering point for memory accesses around the timestamp.
        core::arch::asm!(
            "lfence",
            "rdtsc",
            out("eax") low,
            out("edx") high,
            options(nostack)
        );
    }
    ((high as u64) << 32) | low as u64
}

/// Linux-style x86_64 direct-map base used for physical page-table/data access.
/// The value is LA48-canonical and can be overridden by a loader before the
/// allocator is activated.  Hosted tests deliberately keep an identity alias.
#[cfg(target_os = "none")]
pub const DIRECT_MAP_BASE: usize = 0xffff_8000_0000_0000;
#[cfg(target_os = "none")]
pub const KERNEL_VA_OFFSET: usize = 0xffff_ffff_8000_0000;
#[cfg(target_os = "none")]
static DIRECT_MAP_OFFSET: AtomicUsize = AtomicUsize::new(DIRECT_MAP_BASE);

#[inline]
#[cfg(target_os = "none")]
pub fn set_direct_map_base(base: usize) {
    assert!(super::paging::is_canonical(base as u64, false));
    assert_eq!(base & 0xfff, 0);
    DIRECT_MAP_OFFSET.store(base, Ordering::Release);
}

#[inline]
pub fn phys_to_virt(physical: usize) -> usize {
    #[cfg(target_os = "none")]
    {
        DIRECT_MAP_OFFSET
            .load(Ordering::Acquire)
            .wrapping_add(physical)
    }
    #[cfg(not(target_os = "none"))]
    {
        physical
    }
}

#[inline]
pub fn virt_to_phys(virtual_address: usize) -> usize {
    #[cfg(target_os = "none")]
    {
        let direct = DIRECT_MAP_OFFSET.load(Ordering::Acquire);
        // Dynamic kernel-heap addresses live above the fixed higher-half image
        // window and therefore cannot be decoded by simple subtraction. Ask
        // the MM backend for the leaf mapping first; it returns `None` until
        // the formal heap page table has been published.
        if let Some(paddr) = crate::x86_64::mm::heap_vm::virt_to_phys(virtual_address) {
            return paddr;
        }
        // The higher-half kernel window is numerically above the direct-map
        // window. Check it first; otherwise a kernel symbol would be decoded
        // as a gigantic physical address by subtracting DIRECT_MAP_BASE.
        if virtual_address >= KERNEL_VA_OFFSET {
            return virtual_address.wrapping_sub(KERNEL_VA_OFFSET);
        }
        if virtual_address >= direct {
            return virtual_address.wrapping_sub(direct);
        }
        virtual_address
    }
    #[cfg(not(target_os = "none"))]
    {
        virtual_address
    }
}

#[inline]
pub fn current_cpu_id() -> usize {
    super::smp::current_cpu_id()
}

#[inline]
pub fn device_io_barrier() {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("mfence", options(nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    full_memory_barrier();
}

#[inline]
pub unsafe fn dma_clean_range(_vaddr: usize, _len: usize) -> bool {
    device_io_barrier();
    true
}

#[inline]
pub unsafe fn dma_invalidate_range(_vaddr: usize, _len: usize) -> bool {
    device_io_barrier();
    true
}

/// Publish the scheduler current task and its stable user-return work hint.
///
/// # Safety
/// `task` is a live scheduler current pointer and `cpu_work_ptr` points to an
/// `AtomicU32` whose lifetime covers the scheduler runtime.
#[inline]
pub(crate) unsafe fn set_current_task_ptr_with_work(task: usize, cpu_work_ptr: usize) {
    super::smp::set_current_task_with_work(task, cpu_work_ptr);
}

#[inline]
pub fn current_task_ptr() -> usize {
    super::smp::current_task()
}

/// Linux x86 `AT_HWCAP` uses the raw CPUID.1:EDX bit positions.  AVX-family
/// features are intentionally not invented here; Linux users discover those
/// through CPUID/XGETBV (and HWCAP2 is reserved for kernel-enabled facilities
/// such as FSGSBASE).
pub const HWCAP_X86_FPU: usize = 1 << 0;
pub const HWCAP_X86_MMX: usize = 1 << 23;
pub const HWCAP_X86_FXSR: usize = 1 << 24;
pub const HWCAP_X86_SSE: usize = 1 << 25;
pub const HWCAP_X86_SSE2: usize = 1 << 26;

/// 根据实际启用的 xstate 组件发布用户可见能力。
pub fn user_hwcap() -> usize {
    let f = crate::x86_64::fpu::init();
    let mask = crate::x86_64::fpu::enabled_mask();
    user_hwcap_for(f, mask)
}

fn user_hwcap_for(f: &crate::x86_64::fpu::CpuFeatures, mask: u64) -> usize {
    let mut hwcap = f.hwcap_edx as usize;
    if mask & crate::x86_64::fpu::XFEATURE_BASE != crate::x86_64::fpu::XFEATURE_BASE {
        hwcap &= !(HWCAP_X86_FPU | HWCAP_X86_MMX | HWCAP_X86_FXSR | HWCAP_X86_SSE | HWCAP_X86_SSE2);
    }
    hwcap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_barriers_and_relax_are_callable() {
        compiler_barrier();
        read_memory_barrier();
        write_memory_barrier();
        full_memory_barrier();
        device_io_barrier();
        cpu_relax();
    }

    #[test]
    fn hosted_tsc_is_ordered_enough_for_local_measurements() {
        // The test must not assume synchronized TSCs when the host migrates the
        // test thread between virtual CPUs; merely execute both paths here.
        let _before = rdtsc_ordered();
        let _after = rdtsc_ordered();
    }

    #[test]
    fn hwcap_does_not_advertise_vectors_outside_the_xcr0_policy() {
        let features = crate::x86_64::fpu::CpuFeatures {
            fxsr: true,
            sse: true,
            avx: true,
            avx2: true,
            avx512f: true,
            hwcap_edx: (HWCAP_X86_FPU
                | HWCAP_X86_MMX
                | HWCAP_X86_FXSR
                | HWCAP_X86_SSE
                | HWCAP_X86_SSE2) as u32,
            ..crate::x86_64::fpu::CpuFeatures::default()
        };
        let base = crate::x86_64::fpu::XFEATURE_BASE;
        assert_eq!(user_hwcap_for(&features, 0), 0);
        assert_eq!(
            user_hwcap_for(&features, base),
            HWCAP_X86_FPU | HWCAP_X86_MMX | HWCAP_X86_FXSR | HWCAP_X86_SSE | HWCAP_X86_SSE2
        );

        let avx = base | crate::x86_64::fpu::XFEATURE_YMM;
        assert_eq!(
            user_hwcap_for(&features, avx),
            user_hwcap_for(&features, base)
        );
        assert_eq!(
            user_hwcap_for(&features, avx | crate::x86_64::fpu::XFEATURE_AVX512),
            user_hwcap_for(&features, base)
        );
    }
}

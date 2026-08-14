//! RISC-V64 用户虚拟地址布局。
//!
//! ```text
//! 0x0000_0000_0000_0000 ┬─────────────────────── NULL guard page
//! 0x0000_0000_0040_0000 │ PIE 主程序 (.text/.data)
//! 0x0000_0000_2000_0000 │ 动态链接器 (ld-linux)
//! 0x0000_0000_3000_0000 │ brk heap ──────────────── ↓ grows up
//!                       │
//! 0x0000_7F00_0000_0000 │ mmap 区 (~1 TiB)
//! 0x0000_7FFF_FFFF_0000 │ mmap 上界
//! 0x0000_7FFF_FFFE_C000 │ 主线程栈底 ────────────── ↑ grows down
//! 0x0000_7FFF_FFFF_C000 │ 主线程栈顶
//! 0x0000_7FFF_FFFF_C000 │ vDSO (2 pages)
//! 0x0000_7FFF_FFFF_E000 │ Sv48 用户空间天花板
//! ```

use crate::riscv64::paging::Riscv64Paging;
use general::mm::UserVmLayoutOps;

pub(super) static SV48_USER_VM_LAYOUT_OPS: UserVmLayoutOps = UserVmLayoutOps {
    page_size: <Riscv64Paging as general::PagingArch>::PAGE_SIZE,
    max_grows_down_bytes: 8 * 1024 * 1024, // 栈最大向下增长 8 MiB
    user_heap_base: 0x0000_0000_3000_0000, // brk 起始
    user_mmap_base: 0x0000_7F00_0000_0000, // mmap 区底部（~1 TiB 空间）
    user_mmap_limit: 0x0000_7FFF_FFFF_0000, // mmap 区上界
    default_stack_top: 0x0000_7FFF_FFFF_C000, // 主线程栈顶（vDSO 下方）
    default_stack_size: 64 * 1024,         // 默认栈大小 64 KiB
    main_pie_base: 0x0000_0000_0040_0000,  // PIE 主程序加载基址
    interp_base: 0x0000_0000_2000_0000,    // 动态链接器加载基址
    vdso_base: 0x0000_7FFF_FFFF_C000,      // vDSO 映射（2 pages，栈顶上方）
};

pub(super) static SV39_USER_VM_LAYOUT_OPS: UserVmLayoutOps = UserVmLayoutOps {
    page_size: <Riscv64Paging as general::PagingArch>::PAGE_SIZE,
    max_grows_down_bytes: 8 * 1024 * 1024,
    user_heap_base: 0x0000_0000_3000_0000,
    user_mmap_base: 0x0000_0020_0000_0000,
    user_mmap_limit: 0x0000_003F_FFFF_0000,
    default_stack_top: 0x0000_003F_FFFF_C000,
    default_stack_size: 64 * 1024,
    main_pie_base: 0x0000_0000_0040_0000,
    interp_base: 0x0000_0000_2000_0000,
    vdso_base: 0x0000_003F_FFFF_C000,
};

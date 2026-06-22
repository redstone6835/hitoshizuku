//! LoongArch64 用户虚拟地址布局。

use general::PagingArch;
use general::mm::UserVmLayoutOps;

use crate::loongarch64::paging::LoongArch64Paging;

pub(super) static USER_VM_LAYOUT_OPS: UserVmLayoutOps = UserVmLayoutOps {
    page_size: <LoongArch64Paging as PagingArch>::PAGE_SIZE,
    max_grows_down_bytes: 8 * 1024 * 1024,
    user_heap_base: 0x0000_0000_3000_0000,
    user_mmap_base: 0x0000_0010_0000_0000,
    user_mmap_limit: 0x0000_8000_0000_0000,
    default_stack_top: 0x0000_3FFF_FFFF_F000,
    default_stack_size: 64 * 1024,
    main_pie_base: 0x0000_0001_0000_0000,
    interp_base: 0x0000_0002_0000_0000,
    vdso_base: 0x0000_0000_7FFF_0000,
};

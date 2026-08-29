//! x86_64 Linux 风格用户虚拟地址布局（LA48）。

use general::PagingArch;
use general::mm::UserVmLayoutOps;

use crate::x86_64::paging::X86_64Paging;

pub(super) static USER_VM_LAYOUT_OPS: UserVmLayoutOps = UserVmLayoutOps {
    page_size: <X86_64Paging as PagingArch>::PAGE_SIZE,
    max_grows_down_bytes: 8 * 1024 * 1024,
    user_heap_base: 0x0000_0000_3000_0000,
    user_mmap_base: 0x0000_4000_0000_0000,
    user_mmap_limit: 0x0000_7f00_0000_0000,
    default_stack_top: 0x0000_7fff_ffff_c000,
    default_stack_size: 64 * 1024,
    main_pie_base: 0x0000_0000_0040_0000,
    interp_base: 0x0000_0000_2000_0000,
    // Keep the VDSO below the complete default stack range.  The previous
    // value reused the stack top and allowed the two fixed mappings to alias.
    vdso_base: 0x0000_7fff_fffd_0000,
};

const _: () = {
    let stack_bottom = USER_VM_LAYOUT_OPS.default_stack_top - USER_VM_LAYOUT_OPS.default_stack_size;
    let vdso_end = USER_VM_LAYOUT_OPS.vdso_base + USER_VM_LAYOUT_OPS.page_size;
    assert!(USER_VM_LAYOUT_OPS.vdso_base >= USER_VM_LAYOUT_OPS.page_size);
    assert!(vdso_end <= stack_bottom);
};

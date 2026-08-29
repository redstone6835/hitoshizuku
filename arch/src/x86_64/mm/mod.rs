//! x86_64 用户地址空间后端。
//!
//! 所有页表遍历仍复用 `general::page_walk`；本目录只负责 CR3、物理页表页
//! 生命周期、用户拷贝异常表和 x86 页故障字段的翻译。这样条件编译停留在
//! `arch`，HAL 只看到 `general::mm::*Ops`。

mod fault_decode;
pub(crate) mod heap_vm;
mod layout;
mod user_copy;
mod user_pgd;

/// 启动时一次性注册 x86 的用户地址空间契约。
pub fn register() {
    fault_decode::validate_exception_table();
    general::mm::register_user_vm_layout(&layout::USER_VM_LAYOUT_OPS);
    general::mm::register_user_pgd(&user_pgd::USER_PGD_OPS);
    general::mm::register_user_access(&user_copy::USER_ACCESS_OPS);
    general::mm::register_fault_decode(&fault_decode::FAULT_DECODE_OPS);
}

/// 由正式内核页表初始化阶段发布根物理地址。
pub fn set_kernel_page_table(root: usize) {
    user_pgd::set_kernel_root(root);
}

/// x86_64 kernel heap/page-table callbacks consumed by the common start path.
pub use heap_vm::{
    KERNEL_HEAP_BASE, KERNEL_HEAP_SIZE, TRACKED_HEAP_BASE, TRACKED_HEAP_SIZE,
    init_kernel_page_table, kernel_heap_region, map_kernel_heap_range, protect_kernel_heap_range,
    tracked_heap_region, unmap_kernel_heap_range, validate_kernel_heap_range,
};

/// 由 HAL 的地址空间切换入口调用，清除当前 CPU 的用户 PGD 驻留。
pub(crate) unsafe fn activate_kernel_for_arch() {
    unsafe { user_pgd::activate_kernel_for_arch() };
}

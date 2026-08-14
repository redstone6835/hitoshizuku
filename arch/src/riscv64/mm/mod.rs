//! RISC-V64 mm 注入总入口。
//!
//! 本子树只做一件事：向 [`general::mm`] 注入用户布局和三套函数指针 vtable。
//! sibling 文件各自定义 `pub(super) static *_OPS`，本 `mod.rs` 把它们
//! 拧到一起、通过 [`register`] 暴露给 `arch::riscv64::sched_ctx::register`。
//!
//! 这里**唯一**的 pub 符号是 [`register`]——满足项目的"arch 只通过 Ops
//! 暴露"约束。
mod fault_decode;
mod layout;
pub(crate) mod user_copy;
mod user_pgd;

/// 由 `arch::riscv64::sched_ctx::register` 在启动装契约阶段调用一次。
pub fn register() {
    #[cfg(debug_assertions)]
    crate::riscv64::heap_vm::debug_verify_heap_mapping_transactions();
    fault_decode::validate_exception_table();
    user_pgd::init_asid_allocator();
    let user_layout = match crate::riscv64::paging::active_paging_mode() {
        crate::riscv64::paging_geometry::RiscvPagingMode::Sv39 => &layout::SV39_USER_VM_LAYOUT_OPS,
        crate::riscv64::paging_geometry::RiscvPagingMode::Sv48 => &layout::SV48_USER_VM_LAYOUT_OPS,
    };
    general::mm::register_user_vm_layout(user_layout);
    general::mm::register_user_pgd(&user_pgd::USER_PGD_OPS);
    general::mm::register_user_access(&user_copy::USER_ACCESS_OPS);
    general::mm::register_fault_decode(&fault_decode::FAULT_DECODE_OPS);
}

//! 进程地址空间抽象层。
//!
//! 把 `libs/mm` 的纯 VMA 算法、arch 注入的用户布局/页表 ops、用户内存访问 fixup、
//! page-fault 分派四件事缝起来。kernel / sched 通过 `general::mm::VmSpace`
//! 与之打交道，**不**直接调用 arch；arch 也只通过 [`ops`] 模块向上暴露
//! 函数指针，不暴露任何 `pub` 结构体。
//!
//! ## 模块
//!
//! - [`ops`] —— 四套注入契约：`UserVmLayoutOps` / `UserPgdOps` /
//!   `UserAccessOps` / `FaultDecodeOps`，外加 [`PgdHandle`] 类型本身。
//! - [`vm_space`] —— 把 `libs/mm::VmaSet` 与 `PgdHandle` 组合成 `VmSpace`，
//!   实现 map / unmap / mprotect / fork / handle_fault。
//! - [`user_access`] —— 给 syscall 实现层用的 copy_from/to_user 安全包装。
//! - [`fault`] —— 由 arch trap handler 调用的 `dispatch_page_fault`。
//! - [`smoketest`] —— 启动期自检（debug 模式下编入）。

pub mod fault;
pub mod ops;
pub mod smoketest;
pub mod user_access;
pub mod vm_space;

pub use fault::{FaultKind, FaultOutcome, KernelFaultReason, dispatch_page_fault};
pub use ops::{
    FaultDecodeOps, PgdHandle, UserAccessOps, UserPgdOps, UserVmLayoutOps, fault_decode_ops,
    register_fault_decode, register_user_access, register_user_pgd, register_user_vm_layout,
    user_pgd_ops, user_vm_layout,
};
pub use user_access::{copy_cstr_from_user, copy_from_user, copy_to_user};
pub use vm_space::{UserReadWindows, VmFutexKey, VmSpace, page_size};

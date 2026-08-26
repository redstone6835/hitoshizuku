//! # HAL 层
//! HAL（Hardware Abstraction Layer）层定义了与硬件相关的抽象接口，隔离了上层通用逻辑
//! 与具体架构实现之间的依赖关系。
//!
//! HAL 层的接口主要包括与 CPU、内存、分页、异常处理等相关的操作，这些接口由具体架构
//! 的实现提供，并由上层通用逻辑调用。通过 HAL 层的抽象，我们可以在不修改上层逻辑的情
//! 况下支持不同的架构实现，保证上层代码的统一性和可移植性。

#![no_std]

extern crate alloc;
extern crate arch;

pub mod abi;
pub mod console;
pub mod interrupt;
pub mod memory;
pub mod platform;
pub mod random;
pub mod sched;
pub mod time;
pub mod user;
pub mod user_context;

/// 强制链接器保留稳定 HAL 直接符号所在的代码生成单元。
#[doc(hidden)]
pub fn kernel_symbol_catalog_anchor() -> usize {
    let anchor = abi::decode_dev_t as usize
        ^ console::early_write_bytes as usize
        ^ interrupt::save_and_disable_local as usize
        ^ interrupt::restore_local as usize
        ^ memory::page_size as usize
        ^ memory::dma_clean_range as usize
        ^ memory::dma_invalidate_range as usize
        ^ memory::device_io_barrier as usize
        ^ platform::arch_name as usize
        ^ time::monotonic_ns as usize
        ^ user::default_stack_top as usize
        ^ user_context::set_kernel_trap_stack as usize;
    anchor
}

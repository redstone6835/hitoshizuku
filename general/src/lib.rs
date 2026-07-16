//! # General 层
//!
//! General 层为平台具体实现提供了一个标准接口，使得平台有关的功能方法签名得到高度统一。
//! 我们引出了一般性的功能模块 trait，如分页、任务、异常处理等，并将其具体实现交给
//! arch crate 来完成。这样，arch crate 只需专注于实现这些 trait 的方法，而不需要关心上层
//! 接口的设计和调用细节。
//!
//! 这些 trait 不依赖于特定的架构或硬件平台，可以被不同的架构和平台共享使用。并且我们希望
//! 内核支持的每一个平台都能实现这些 trait，以保证内核功能的完整性和代码层次的一致性。

#![no_std]

extern crate alloc;

mod page_walk;
mod paging;
mod platform;
mod start;
mod task;
mod trap;

pub mod elm_guard;
pub mod elm_image;
pub use page_walk::*;
pub use paging::*;
pub use platform::*;
pub use start::*;
pub use task::*;
pub use trap::*;

pub mod cmdline;
pub mod console;
pub mod dev;
pub mod dtb;
pub mod firmware;
pub mod ipc;
pub mod mm;
pub mod syscall;
pub mod vfs;

/// 强制链接器抽取设备抽象直接符号目录所在的代码生成单元。
#[doc(hidden)]
pub fn kernel_symbol_catalog_anchor() -> usize {
    dev::pnp::register_driver_factory as usize
        ^ dev::pnp::device_mmio_to_virt as usize
        ^ dev::function::register_function_class as usize
        ^ dev::firmware_bus::register as usize
        ^ dev::dma::set_dma_ops as usize
        ^ dev::irq::register_irq_request as usize
        ^ dev::irq::register_irq_domain as usize
        ^ dev::msi::register_msi_controller as usize
        ^ dev::pci::register_host_bridge as usize
        ^ dev::pci::pci_scan_and_register as usize
        ^ dev::platform::register_and_probe_platform_device as usize
}

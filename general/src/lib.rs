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

/// 语言运行时桥接所使用的规范 ABI 类型。
///
/// 集成 ELM 必须经由 General 的接口投影使用这些类型，避免同时链接源 crate 与
/// Profile 投影 crate 后形成两套不兼容的 Rust 类型身份。
pub use elm_language_abi as language_abi;

pub mod cmdline;
pub mod console;
pub mod dev;
pub mod dtb;
pub mod firmware;
pub mod ipc;
pub mod mm;
pub mod seccomp;
pub mod syscall;
pub mod vfs;

/// 强制链接器抽取设备抽象直接符号和集成组件内部桥所在的代码生成单元。
#[doc(hidden)]
pub fn kernel_symbol_catalog_anchor() -> usize {
    dev::block::BlockDevice::mark_gone as *const () as usize
        ^ dev::cpu::cpu_reg_for_interrupt_controller as *const () as usize
        ^ dev::pnp::register_driver_factory as *const () as usize
        ^ dev::pnp::register_function as *const () as usize
        ^ dev::pnp::device_mmio_to_virt as *const () as usize
        ^ dev::pnp::PnpDevice::parent as *const () as usize
        ^ dev::function::register_function_class as *const () as usize
        ^ dev::function::CharFunction::from_driver_arc as *const () as usize
        ^ dev::firmware_bus::register as *const () as usize
        ^ dev::dma::set_dma_ops as *const () as usize
        ^ dev::language::dispatch as *const () as usize
        ^ dev::language::revoke_owner as *const () as usize
        ^ dev::language::call as *const () as usize
        ^ dev::language::dispatch_for_provider as *const () as usize
        ^ dev::language::revoke_owner_for_provider as *const () as usize
        ^ dev::language::call_for_provider as *const () as usize
        ^ dev::dt_bus::register_i2c_controller as *const () as usize
        ^ dev::dt_provider::register as *const () as usize
        ^ dev::dt_provider::acquire_reference as *const () as usize
        ^ dev::dt_provider::acquire_reference_rate_for_device as *const () as usize
        ^ dev::dt_provider::acquire_reference_configure_for_device as *const () as usize
        ^ dev::dt_provider::provider_pnp_resource_boxed as *const () as usize
        ^ dev::dt_provider::lease_pnp_resource_boxed as *const () as usize
        ^ dev::flash::pnp_resource_v2_boxed as *const () as usize
        ^ dev::irq::register_irq_request as *const () as usize
        ^ dev::irq::register_irq_domain as *const () as usize
        ^ dev::irq::irq_handler_pnp_resource_boxed as *const () as usize
        ^ dev::iommu::register_iommu_controller as *const () as usize
        ^ dev::iommu::controller_pnp_resource_boxed as *const () as usize
        ^ dev::msi::register_msi_controller as *const () as usize
        ^ dev::numa::memory_node as *const () as usize
        ^ dev::pci::register_host_bridge as *const () as usize
        ^ dev::pci::pci_scan_and_register as *const () as usize
        ^ dev::pci::PciDevice::pnp_id as *const () as usize
        ^ dev::pci::PciDevice::info as *const () as usize
        ^ dev::pci::PciDevice::try_read_config_u16 as *const () as usize
        ^ dev::pci::PciDevice::try_read_config_u32 as *const () as usize
        ^ dev::pci::PciDevice::try_write_config_u16 as *const () as usize
        ^ dev::pci::PciDevice::try_write_config_u32 as *const () as usize
        ^ dev::pci::PciDevice::try_command as *const () as usize
        ^ dev::pci::PciDevice::try_set_command as *const () as usize
        ^ dev::pci::PciDevice::bar_count as *const () as usize
        ^ dev::pci::PciDevice::new_unregistered as *const () as usize
        ^ dev::pci::PciDevice::register_and_probe as *const () as usize
        ^ dev::platform::firmware_u32_list_get as *const () as usize
        ^ dev::platform::PlatformDeviceInfo::dtb_reference_by_name as *const () as usize
        ^ dev::platform::register_and_probe_platform_device as *const () as usize
        ^ dev::pmu::register as *const () as usize
        ^ dev::pmu::open_session as *const () as usize
        ^ dev::random::add_bootloader_randomness as *const () as usize
        ^ dev::syscon::pnp_resource_boxed as *const () as usize
        ^ dev::usb::usb_device_pnp_info_boxed as *const () as usize
        ^ dev::usb::UsbDevice::from_pnp as *const () as usize
        ^ dev::usb::UsbDevice::interfaces as *const () as usize
        ^ dev::usb::UsbDevice::create_interface as *const () as usize
        ^ dev::wdt::WdtDevice::set_timeout as *const () as usize
        ^ dev::wdt::WdtDevice::max_timeout_secs as *const () as usize
        ^ console::console_write as *const () as usize
        ^ firmware::power::pnp_resource_boxed as *const () as usize
        ^ firmware::power::shutdown as *const () as usize
        ^ ipc::ShmManager::info as *const () as usize
        ^ mm::page_size as *const () as usize
        ^ mm::VmSpace::mapped_pages as *const () as usize
        ^ vfs::namespace_path as *const () as usize
}

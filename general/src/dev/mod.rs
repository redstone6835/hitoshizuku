//! 通用设备对象层。
//!
//! 此模块只描述"设备对象本身"及其注册关系：
//! - `char` / `block` 定义设备对象与驱动接口；
//! - `pnp` 定义 PnP 设备抽象框架（硬件身份、拓扑、热插拔）；
//! - `pci` / `usb` 提供总线级设备封装；
//! - `enumerate` 提供启动期已注册设备的全局对象表；
//! 具体驱动位于顶层 `drivers/`，本 crate 只提供可扩展设备抽象。
//!
//! 这里不再承担设备号分配、`major/minor` 编码或
//! `device number -> driver` 查找职责；这些若仍有需要，只能存在于
//! ABI/VFS 兼容边界，而不能回流到 `dev` 层。

pub mod bio;
pub mod block;
pub mod block_sync;
pub mod char;
pub mod completion;
pub mod control;
pub mod cpu;
pub mod dma;
pub mod elm;
pub mod enumerate;
pub mod firmware_bus;
pub mod flash;
pub mod function;
pub mod fwcfg;
pub mod irq;
pub mod language;
pub mod loopdev;
pub mod msi;
pub mod naming;
pub mod net;
pub mod pci;
pub mod platform;
pub mod pm_qos;
pub mod pnp;
pub mod random;
pub mod random_source;
pub mod rtc;
pub mod syscon;
pub mod usb;

mod elm_lifecycle;
mod registry_id;

pub use control::DriverControl;

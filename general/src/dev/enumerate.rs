//! 全局设备枚举。
//!
//! # 概览
//!
//! ```text
//! devices()               → &'static DeviceList
//!   ├── .char_devs        → CharDevList   （动态字符设备注册表）
//!   │     每项: CharDev 共享句柄
//!   └── .block_devs       → BlockDevList  （动态块设备注册表）
//! ```
//!
//! # PnP 设备与驱动
//!
//! ```text
//! PNP_DEVICES             → PnpDeviceList （PnP 设备全局注册表）
//! PNP_DRIVERS             → PnpDriverRegistry（PnP 驱动注册表）
//! ```
//!
//! # 注册字符设备
//!
//! ```rust,ignore
//! use general::dev::{char::{CharDev, CharDevKind}, enumerate::DEVICES, drivers::Uart16550};
//!
//! let uart: &'static Uart16550 = /* ... */;
//! DEVICES
//!     .char_devs
//!     .push(CharDev::new(CharDevKind::Ns16550, "serial@9000000", uart))
//!     .expect("char dev registration failed");
//! ```
//!
//! # 遍历所有字符设备
//!
//! ```rust,ignore
//! use general::dev::enumerate::DEVICES;
//!
//! for dev in DEVICES.char_devs.iter() {
//!     println!("{:?} -> {}", dev.kind(), dev.fw_name());
//! }
//! ```

// ─────────────────────────── 顶层设备列表 ─────────────────────────────────

use crate::dev::*;

/// 全局设备列表，包含各类设备的对象注册表。
///
/// 此层只管理"已注册的设备对象"，不再维护设备号分配或
/// `major/minor -> driver` 映射。
pub struct DeviceList {
    /// 字符设备子列表。
    pub char_devs: char::CharDeviceList,
    /// 块设备子列表。
    pub block_devs: block::BlockDeviceList,
}

impl DeviceList {
    const fn new() -> Self {
        Self {
            char_devs: char::CharDeviceList::new(),
            block_devs: block::BlockDeviceList::new(),
        }
    }
}

pub static DEVICES: DeviceList = DeviceList::new();

pub use crate::dev::pnp::{PNP_DEVICES, PNP_DRIVERS};

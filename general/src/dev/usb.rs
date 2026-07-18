//! USB 设备抽象层。
//!
//! USB 采用 device/interface 两级建模：
//!
//! ```text
//! UsbDevice (PnpDevice)
//!   ├── UsbInterface 0 (PnpDevice, child)
//!   ├── UsbInterface 1 (PnpDevice, child)
//!   └── UsbInterface 2 (PnpDevice, child)
//! ```
//!
//! USB class driver 通常绑定到 interface level 的 PnpDevice。
//! 父设备移除时，子 interface 在 PnpDevice::remove_device() 中被递归移除。
//!
//! # 用法
//!
//! ```rust,ignore
//! // Bus 层：USB host controller 发现设备后
//! let dev_info = Box::new(UsbDeviceInfo { ... });
//! let dev_id = PnpId::Usb { bus_id: 0, address: 1, interface: None };
//! let usb_dev_pnp = PnpDevice::new(dev_id, "usb-0:1".into(), dev_info)?;
//! PNP_DEVICES.get_or_insert(Arc::clone(&usb_dev_pnp))?;
//!
//! // 为每个 interface 创建子 PnpDevice
//! for iface in &device_desc.interfaces {
//!     let iface_info = Box::new(UsbInterfaceInfo { class: iface.class, ... });
//!     let iface_id = PnpId::Usb { bus_id: 0, address: 1, interface: Some(iface.num) };
//!     let iface_pnp = PnpDevice::new(iface_id, "usb-0:1.0".into(), iface_info)?;
//!     usb_dev_pnp.attach_child(&iface_pnp)?;
//!     PNP_DEVICES.get_or_insert(Arc::clone(&iface_pnp))?;
//!     PNP_DRIVERS.probe_device(&iface_pnp)?;
//! }
//! ```

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use super::pnp::{
    BusType, PNP_DEVICES, PNP_DRIVERS, PnpBusInfo, PnpDevice, PnpError, PnpId, PnpState,
};

// ── UsbDeviceInfo ────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct UsbDeviceInfo {
    pub vendor: u16,
    pub product: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub max_packet_size: u8,
    pub manufacturer_str: Option<Box<str>>,
    pub product_str: Option<Box<str>>,
    pub serial_str: Option<Box<str>>,
    pub num_configurations: u8,
    pub speed: UsbSpeed,
}

impl UsbDeviceInfo {
    pub fn is_hub(&self) -> bool {
        self.device_class == 0x09
    }
}

impl PnpBusInfo for UsbDeviceInfo {
    fn bus_type(&self) -> BusType {
        BusType::USB
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── UsbInterfaceInfo ─────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct UsbInterfaceInfo {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub interface_number: u8,
    pub num_endpoints: u8,
    pub endpoints: Vec<UsbEndpointDesc>,
    pub vendor: u16,
    pub product: u16,
}

impl UsbInterfaceInfo {
    pub fn is_hid(&self) -> bool {
        self.class == 0x03
    }

    pub fn is_mass_storage(&self) -> bool {
        self.class == 0x08
    }

    pub fn is_hub(&self) -> bool {
        self.class == 0x09
    }

    pub fn is_vendor_specific(&self) -> bool {
        self.class == 0xFF
    }
}

impl PnpBusInfo for UsbInterfaceInfo {
    fn bus_type(&self) -> BusType {
        BusType::USB
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── UsbEndpointDesc ──────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct UsbEndpointDesc {
    pub address: u8,
    pub attributes: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}

impl UsbEndpointDesc {
    pub fn number(&self) -> u8 {
        self.address & 0x0F
    }

    pub fn direction(&self) -> UsbDirection {
        if self.address & 0x80 != 0 {
            UsbDirection::In
        } else {
            UsbDirection::Out
        }
    }

    pub fn transfer_type(&self) -> UsbTransferType {
        match self.attributes & 0x03 {
            0 => UsbTransferType::Control,
            1 => UsbTransferType::Isochronous,
            2 => UsbTransferType::Bulk,
            3 => UsbTransferType::Interrupt,
            _ => UsbTransferType::Control,
        }
    }
}

// ── USB 枚举类型 ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbSpeed {
    Low,
    Full,
    High,
    Super,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbDirection {
    Out,
    In,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbTransferType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

// ── UsbDevice ────────────────────────────────────────────────────────────

/// USB 设备级包装。
///
/// 持有 device-level 的 [`PnpDevice`] 引用。
/// USB device 通常不需要绑定 driver（除非是 hub 等全设备驱动），
/// 驱动绑定在 interface level 的 [`UsbInterface`] 上。
pub struct UsbDevice {
    pnp: Arc<PnpDevice>,
}

impl fmt::Debug for UsbDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsbDevice")
            .field("pnp_id", &self.pnp.id)
            .field("name", &self.pnp.name)
            .finish()
    }
}

impl UsbDevice {
    pub fn from_pnp(pnp: &Arc<PnpDevice>) -> Option<Self> {
        let PnpId::Usb {
            interface: None, ..
        } = pnp.id
        else {
            return None;
        };

        pnp.info.as_any().downcast_ref::<UsbDeviceInfo>()?;

        Some(Self {
            pnp: Arc::clone(pnp),
        })
    }

    pub fn pnp(&self) -> &Arc<PnpDevice> {
        &self.pnp
    }

    pub fn info(&self) -> Option<&UsbDeviceInfo> {
        self.pnp.info.as_any().downcast_ref::<UsbDeviceInfo>()
    }

    pub fn bus_id(&self) -> Option<u8> {
        match self.pnp.id {
            PnpId::Usb { bus_id, .. } => Some(bus_id),
            _ => None,
        }
    }

    pub fn address(&self) -> Option<u8> {
        match self.pnp.id {
            PnpId::Usb { address, .. } => Some(address),
            _ => None,
        }
    }

    /// 获取所有已注册的子 interface。
    pub fn interfaces(&self) -> Vec<UsbInterface> {
        self.pnp
            .children()
            .into_iter()
            .filter_map(|child| UsbInterface::from_pnp(&child))
            .collect()
    }

    /// 查找指定 interface 号的子接口。
    pub fn find_interface(&self, interface_num: u8) -> Option<UsbInterface> {
        self.interfaces()
            .into_iter()
            .find(|iface| iface.interface_number() == Some(interface_num))
    }

    /// 创建并附加一个 interface 子 PnpDevice。
    ///
    /// 调用方应在创建 interface PnpDevice 后调用
    /// `PNP_DEVICES.get_or_insert()` 和 `PNP_DRIVERS.probe_device()`。
    pub fn create_interface(
        &self,
        num: u8,
        name: Box<str>,
        info: UsbInterfaceInfo,
    ) -> Result<Arc<PnpDevice>, PnpError> {
        let (bus_id, address) = match self.pnp.id {
            PnpId::Usb {
                bus_id, address, ..
            } => (bus_id, address),
            _ => return Err(PnpError::InvalidState),
        };

        let id = PnpId::Usb {
            bus_id,
            address,
            interface: Some(num),
        };

        let child = PnpDevice::new(id, name, Box::new(info))?;
        self.pnp.attach_child(&child)?;
        Ok(child)
    }
}

// ── UsbInterface ─────────────────────────────────────────────────────────

/// USB interface 级包装。
///
/// 大部分 USB class driver 绑定到 interface level 的 [`PnpDevice`] 上。
/// [`UsbInterface`] 提供类型安全的 interface 描述符访问和到父
/// [`UsbDevice`] 的导航。
pub struct UsbInterface {
    pnp: Arc<PnpDevice>,
}

impl fmt::Debug for UsbInterface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsbInterface")
            .field("pnp_id", &self.pnp.id)
            .field("name", &self.pnp.name)
            .finish()
    }
}

impl UsbInterface {
    pub fn from_pnp(pnp: &Arc<PnpDevice>) -> Option<Self> {
        let PnpId::Usb {
            interface: Some(_), ..
        } = pnp.id
        else {
            return None;
        };

        pnp.info.as_any().downcast_ref::<UsbInterfaceInfo>()?;

        Some(Self {
            pnp: Arc::clone(pnp),
        })
    }

    pub fn pnp(&self) -> &Arc<PnpDevice> {
        &self.pnp
    }

    pub fn info(&self) -> Option<&UsbInterfaceInfo> {
        self.pnp.info.as_any().downcast_ref::<UsbInterfaceInfo>()
    }

    pub fn bus_id(&self) -> Option<u8> {
        match self.pnp.id {
            PnpId::Usb { bus_id, .. } => Some(bus_id),
            _ => None,
        }
    }

    pub fn address(&self) -> Option<u8> {
        match self.pnp.id {
            PnpId::Usb { address, .. } => Some(address),
            _ => None,
        }
    }

    pub fn interface_number(&self) -> Option<u8> {
        match self.pnp.id {
            PnpId::Usb {
                interface: Some(iface),
                ..
            } => Some(iface),
            _ => None,
        }
    }

    pub fn class(&self) -> Option<(u8, u8, u8)> {
        self.info().map(|i| (i.class, i.subclass, i.protocol))
    }

    pub fn endpoints(&self) -> Vec<UsbEndpointDesc> {
        self.info().map(|i| i.endpoints.clone()).unwrap_or_default()
    }

    /// 查找父 [`UsbDevice`]。
    pub fn parent_device(&self) -> Option<UsbDevice> {
        let parent = self.pnp.parent()?;
        UsbDevice::from_pnp(&parent)
    }

    /// 返回此 interface 的 vendor/product 信息（从 interface info 获取）。
    pub fn vendor_product(&self) -> Option<(u16, u16)> {
        self.info().map(|i| (i.vendor, i.product))
    }
}

// ── 动态设备管理 ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbProbeStatus {
    Bound,
    NoDriver,
    Deferred,
}

#[derive(Clone)]
pub struct UsbRegistration {
    pub device: Arc<PnpDevice>,
    pub status: UsbProbeStatus,
}

impl UsbRegistration {
    const fn new(device: Arc<PnpDevice>, status: UsbProbeStatus) -> Self {
        Self { device, status }
    }
}

fn rollback_usb_registration(pnp: &Arc<PnpDevice>, inserted: bool) {
    if !inserted {
        return;
    }
    if let Some(parent) = pnp.parent() {
        parent.detach_child(pnp);
    }
    PNP_DEVICES.remove_exact(pnp);
}

fn probe_registered_usb_pnp(
    pnp: &Arc<PnpDevice>,
    inserted: bool,
) -> Result<UsbRegistration, PnpError> {
    match pnp.state() {
        PnpState::Bound => Ok(UsbRegistration::new(Arc::clone(pnp), UsbProbeStatus::Bound)),
        PnpState::Discovered => match PNP_DRIVERS.probe_device(pnp) {
            Ok(()) => Ok(UsbRegistration::new(Arc::clone(pnp), UsbProbeStatus::Bound)),
            Err(PnpError::NoDriver) => Ok(UsbRegistration::new(
                Arc::clone(pnp),
                UsbProbeStatus::NoDriver,
            )),
            Err(err) if err.is_deferred() => Ok(UsbRegistration::new(
                Arc::clone(pnp),
                UsbProbeStatus::Deferred,
            )),
            Err(err) => {
                rollback_usb_registration(pnp, inserted);
                Err(err)
            }
        },
        PnpState::Probing | PnpState::Removing | PnpState::Gone => {
            rollback_usb_registration(pnp, inserted);
            Err(PnpError::InvalidState)
        }
    }
}

impl UsbDevice {
    /// 将 USB device 注册到全局 PnP 列表，并 probe device-level 驱动（如有）。
    ///
    /// USB device 通常不需要绑定驱动（hub 例外），但调用此方法可以：
    /// 1. 将 device 注册到 `PNP_DEVICES`，提供拓扑可见性
    /// 2. 为 hub 等全设备驱动提供 probe 机会
    ///
    /// `NoDriver` 和 `ProbeDeferred` 不视为硬失败，而是通过 [`UsbProbeStatus`]
    /// 返回给调用方，便于扫描器统计和后续依赖恢复重试。
    pub fn register_and_probe(&self) -> Result<UsbRegistration, PnpError> {
        let registration = PNP_DEVICES.get_or_insert(Arc::clone(&self.pnp))?;
        probe_registered_usb_pnp(&registration.device, registration.inserted)
    }

    /// 创建 USB interface 子 PnpDevice、注册到全局列表并 probe 驱动。
    ///
    /// 一步完成 interface 的创建→注册→probe 流程。
    /// 这是 USB host controller 发现新 interface 时的推荐调用方式。
    ///
    /// # 参数
    /// - `num`: interface 号（0-based）
    /// - `name`: 用户可见名（如 `"usb-0:1.0"`）
    /// - `info`: interface 描述符信息
    ///
    /// # 返回
    pub fn register_interface_and_probe(
        &self,
        num: u8,
        name: Box<str>,
        info: UsbInterfaceInfo,
    ) -> Result<UsbRegistration, PnpError> {
        if let Some(existing) = self.find_interface(num) {
            return probe_registered_usb_pnp(&existing.pnp, false);
        }

        let child = self.create_interface(num, name, info)?;

        let registration = match PNP_DEVICES.get_or_insert(Arc::clone(&child)) {
            Ok(registration) => registration,
            Err(err) => {
                self.pnp.detach_child(&child);
                return Err(err);
            }
        };

        if !registration.inserted && !Arc::ptr_eq(&registration.device, &child) {
            self.pnp.detach_child(&child);
        }

        let result = probe_registered_usb_pnp(&registration.device, registration.inserted);
        if result.is_err() && registration.inserted {
            self.pnp.detach_child(&child);
        }
        result
    }

    /// 一次性为目标 USB device 的所有 interface 执行注册和 probe。
    ///
    /// 这是设备枚举完成的常用终点：传入 interface 描述符列表，
    /// 自动创建子 PnpDevice、注册并 probe。
    ///
    /// 对于每个 interface 独立处理：单个 interface 失败不影响其他 interface。
    pub fn register_all_interfaces(
        &self,
        name_prefix: &str,
        interfaces: &[UsbInterfaceInfo],
    ) -> usize {
        let mut count = 0usize;
        for info in interfaces {
            let name: Box<str> =
                alloc::format!("{}{}.{}", name_prefix, info.interface_number, info.class).into();
            if self
                .register_interface_and_probe(info.interface_number, name, info.clone())
                .is_ok()
            {
                count += 1;
            }
        }
        count
    }

    /// 触发设备热拔移除。
    ///
    /// 递归移除所有子 interface 和 device 自身。
    /// 等效于 `self.pnp.remove_device()`。
    pub fn remove_from_bus(&self) {
        self.pnp.remove_device();
    }
}

impl UsbInterface {
    /// 将 interface PnpDevice 注册到全局列表并 probe 驱动。
    ///
    /// 适用于 interface PnpDevice 由外部构造的场景
    /// （如通过 [`UsbDevice::create_interface`] 分开创建和注册）。
    pub fn register_and_probe(&self) -> Result<UsbRegistration, PnpError> {
        let registration = PNP_DEVICES.get_or_insert(Arc::clone(&self.pnp))?;
        probe_registered_usb_pnp(&registration.device, registration.inserted)
    }

    /// 触发 interface 热拔移除。
    ///
    /// 仅移除此 interface，不影响父 device 或其他子 interface。
    /// 等效于 `self.pnp.remove_device()`。
    pub fn remove_from_bus(&self) {
        self.pnp.remove_device();
    }
}

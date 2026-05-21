//! PCI 设备抽象层。
//!
//! 将 PCI/PCIe 设备封装为类型安全的 `PciDevice`，驱动通过它访问
//! config space、BAR 和 MSI/MSI-X，而不是直接操作裸寄存器。
//!
//! # 架构
//!
//! ```text
//! PciDevice  → 持有 Arc<PnpDevice>，提供类型安全访问
//! PciInfo    →  实现 PnpBusInfo，存储 vendor/device/class 等
//! PciBar     →  BAR 描述符
//! ```
//!
//! # 用法
//!
//! ```rust,ignore
//! // Bus 层：扫描到设备后创建 PnpDevice + PciDevice
//! let info = Box::new(PciInfo { vendor: 0x8086, device_id: 0x100e, ... });
//! let id = PnpId::Pci { segment: 0, bus: 1, device: 0, function: 0 };
//! let pnp = PnpDevice::new(id, "eth0".into(), info);
//! let pci_dev = PciDevice::from_pnp(&pnp).unwrap();
//!
//! // 驱动 probe 中使用 PciDevice
//! let bar0 = pci_dev.map_bar(0).unwrap();
//! pci_dev.enable_bus_master();
//! ```

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::fmt;

use vfs::sync::Spinlock;

use super::pnp::{
    PnpBusInfo, PnpDevice, PnpError, PnpId, PNP_DEVICES, PNP_DRIVERS,
};

// ── PciInfo ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct PciInfo {
    pub vendor: u16,
    pub device_id: u16,
    pub revision: u8,
    pub class: u32,
    pub subclass: u8,
    pub prog_if: u8,
    pub subsystem_vendor: u16,
    pub subsystem_id: u16,
    pub header_type: u8,
    pub multi_function: bool,
}

impl PciInfo {
    pub const fn class_code(self) -> (u8, u8, u8) {
        (
            (self.class >> 16) as u8,
            self.subclass,
            self.prog_if,
        )
    }

    pub fn is_storage_controller(&self) -> bool {
        (self.class >> 16) as u8 == 0x01
    }

    pub fn is_network_controller(&self) -> bool {
        (self.class >> 16) as u8 == 0x02
    }

    pub fn is_display_controller(&self) -> bool {
        (self.class >> 16) as u8 == 0x03
    }

    pub fn is_bridge(&self) -> bool {
        (self.class >> 16) as u8 == 0x06
    }
}

impl PnpBusInfo for PciInfo {
    fn bus_type(&self) -> &'static str {
        "pci"
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── PciBar ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub enum PciBarType {
    Memory,
    Io,
}

#[derive(Clone, Copy, Debug)]
pub struct PciBar {
    pub idx: usize,
    pub bar_type: PciBarType,
    pub prefetchable: bool,
    pub phys_addr: u64,
    pub size: u64,
}

// ── PCI config space 访问回调 ────────────────────────────────────────────

pub struct PciConfigAccess {
    pub read_u8: fn(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u8,
    pub read_u16: fn(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u16,
    pub read_u32: fn(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u32,
    pub write_u8: fn(segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u8),
    pub write_u16: fn(segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u16),
    pub write_u32: fn(segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u32),
    pub device_mmio_to_virt: fn(phys_addr: usize) -> usize,
}

static PCI_CONFIG: Spinlock<Option<PciConfigAccess>> = Spinlock::new(None);

pub fn set_pci_config_access(access: PciConfigAccess) {
    *PCI_CONFIG.lock() = Some(access);
}

// ── PciDevice ────────────────────────────────────────────────────────────

pub struct PciDevice {
    pnp: Arc<PnpDevice>,
}

impl fmt::Debug for PciDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PciDevice")
            .field("pnp_id", &self.pnp.id)
            .field("name", &self.pnp.name)
            .finish()
    }
}

impl PciDevice {
    pub fn from_pnp(pnp: &Arc<PnpDevice>) -> Option<Self> {
        let PnpId::Pci { .. } = pnp.id else {
            return None;
        };

        pnp.info.as_any().downcast_ref::<PciInfo>()?;

        Some(Self {
            pnp: Arc::clone(pnp),
        })
    }

    pub fn pnp(&self) -> &Arc<PnpDevice> {
        &self.pnp
    }

    pub fn pnp_id(&self) -> &PnpId {
        &self.pnp.id
    }

    pub fn info(&self) -> Option<&PciInfo> {
        self.pnp.info.as_any().downcast_ref::<PciInfo>()
    }

    fn bdf(&self) -> Option<(u16, u8, u8, u8)> {
        match self.pnp.id {
            PnpId::Pci {
                segment,
                bus,
                device,
                function,
            } => Some((segment, bus, device, function)),
            _ => None,
        }
    }

    // ── Config space 访问 ──

    pub fn read_config_u8(&self, offset: u16) -> u8 {
        let (seg, bus, dev, func) = match self.bdf() {
            Some(bdf) => bdf,
            None => return 0,
        };
        let guard = PCI_CONFIG.lock();
        guard
            .as_ref()
            .map(|cfg| (cfg.read_u8)(seg, bus, dev, func, offset))
            .unwrap_or(0)
    }

    pub fn read_config_u16(&self, offset: u16) -> u16 {
        let (seg, bus, dev, func) = match self.bdf() {
            Some(bdf) => bdf,
            None => return 0,
        };
        let guard = PCI_CONFIG.lock();
        guard
            .as_ref()
            .map(|cfg| (cfg.read_u16)(seg, bus, dev, func, offset))
            .unwrap_or(0)
    }

    pub fn read_config_u32(&self, offset: u16) -> u32 {
        let (seg, bus, dev, func) = match self.bdf() {
            Some(bdf) => bdf,
            None => return 0,
        };
        let guard = PCI_CONFIG.lock();
        guard
            .as_ref()
            .map(|cfg| (cfg.read_u32)(seg, bus, dev, func, offset))
            .unwrap_or(0)
    }

    pub fn write_config_u8(&self, offset: u16, value: u8) {
        let (seg, bus, dev, func) = match self.bdf() {
            Some(bdf) => bdf,
            None => return,
        };
        let guard = PCI_CONFIG.lock();
        if let Some(cfg) = guard.as_ref() {
            (cfg.write_u8)(seg, bus, dev, func, offset, value);
        }
    }

    pub fn write_config_u16(&self, offset: u16, value: u16) {
        let (seg, bus, dev, func) = match self.bdf() {
            Some(bdf) => bdf,
            None => return,
        };
        let guard = PCI_CONFIG.lock();
        if let Some(cfg) = guard.as_ref() {
            (cfg.write_u16)(seg, bus, dev, func, offset, value);
        }
    }

    pub fn write_config_u32(&self, offset: u16, value: u32) {
        let (seg, bus, dev, func) = match self.bdf() {
            Some(bdf) => bdf,
            None => return,
        };
        let guard = PCI_CONFIG.lock();
        if let Some(cfg) = guard.as_ref() {
            (cfg.write_u32)(seg, bus, dev, func, offset, value);
        }
    }

    // ── BAR ──

    pub fn map_bar(&self, idx: usize) -> Option<PciBar> {
        if idx > 5 {
            return None;
        }
        let offset = 0x10u16 + (idx as u16) * 4;
        let bar_val = self.read_config_u32(offset);

        if bar_val == 0 {
            return None;
        }

        let is_mmio = bar_val & 0x1 == 0;
        let prefetchable = is_mmio && (bar_val & 0x8) != 0;

        let (bar_type, phys_addr, size_mask) = if is_mmio {
            let bar_type = match (bar_val >> 1) & 0x3 {
                0 => PciBarType::Memory,   // 32-bit
                2 => PciBarType::Memory,   // 64-bit (lower half returned here)
                _ => return None,
            };
            let phys_addr = (bar_val & 0xFFFF_FFF0) as u64;
            (bar_type, phys_addr, !0xFu32)
        } else {
            let phys_addr = (bar_val & 0xFFFF_FFFC) as u64;
            (PciBarType::Io, phys_addr, !0x3u32)
        };

        // Read the size by writing all 1s and reading back
        self.write_config_u32(offset, size_mask);
        let size_val = self.read_config_u32(offset) & size_mask;
        self.write_config_u32(offset, bar_val);

        let size = if size_val == 0 {
            return None;
        } else {
            (!size_val).wrapping_add(1) as u64
        };

        // Enable memory/IO decode
        let cmd = self.read_config_u16(0x04);
        if is_mmio {
            self.write_config_u16(0x04, cmd | 0x2);
        } else {
            self.write_config_u16(0x04, cmd | 0x1);
        }

        Some(PciBar {
            idx,
            bar_type,
            prefetchable,
            phys_addr,
            size,
        })
    }

    pub fn map_bar_virt(&self, idx: usize) -> Option<(PciBar, usize)> {
        let bar = self.map_bar(idx)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref()?;
        let vaddr = match bar.bar_type {
            PciBarType::Memory => (cfg.device_mmio_to_virt)(bar.phys_addr as usize),
            PciBarType::Io => bar.phys_addr as usize,
        };
        Some((bar, vaddr))
    }

    // ── 设备控制 ──

    pub fn enable_bus_master(&self) {
        let cmd = self.read_config_u16(0x04);
        self.write_config_u16(0x04, cmd | 0x4);
    }

    pub fn disable_bus_master(&self) {
        let cmd = self.read_config_u16(0x04);
        self.write_config_u16(0x04, cmd & !0x4);
    }

    pub fn bus_master_enabled(&self) -> bool {
        self.read_config_u16(0x04) & 0x4 != 0
    }

    pub fn enable_mmio(&self) {
        let cmd = self.read_config_u16(0x04);
        self.write_config_u16(0x04, cmd | 0x2);
    }

    pub fn enable_io(&self) {
        let cmd = self.read_config_u16(0x04);
        self.write_config_u16(0x04, cmd | 0x1);
    }

    pub fn disable_interrupts(&self) {
        let cmd = self.read_config_u16(0x04);
        self.write_config_u16(0x04, cmd | 0x400);
    }

    pub fn enable_interrupts(&self) {
        let cmd = self.read_config_u16(0x04);
        self.write_config_u16(0x04, cmd & !0x400);
    }

    // ── IRQ ──

    pub fn irq_line(&self) -> Option<u8> {
        let irq = self.read_config_u8(0x3C);
        if irq == 0 || irq == 0xFF {
            None
        } else {
            Some(irq)
        }
    }

    pub fn irq_pin(&self) -> Option<u8> {
        let pin = self.read_config_u8(0x3D);
        if pin == 0 {
            None
        } else {
            Some(pin)
        }
    }

    // ── capability 遍历 ──

    pub fn capabilities_offset(&self) -> Option<u16> {
        let status = self.read_config_u16(0x06);
        if status & 0x10 == 0 {
            return None;
        }
        let cap_ptr = self.read_config_u8(0x34);
        if cap_ptr == 0 {
            None
        } else {
            Some(cap_ptr as u16 & 0xFC)
        }
    }

    pub fn find_capability(&self, cap_id: u8) -> Option<u16> {
        let mut ptr = self.capabilities_offset()?;
        loop {
            if ptr < 0x40 {
                return None;
            }
            let id = self.read_config_u8(ptr);
            if id == cap_id {
                return Some(ptr);
            }
            ptr = self.read_config_u8(ptr + 1) as u16 & 0xFC;
            if ptr == 0 {
                return None;
            }
        }
    }

    // ── MSI ──

    pub fn msi_capability(&self) -> Option<u16> {
        self.find_capability(0x05)
    }

    pub fn msi_enable(&self, cap_offset: u16) {
        let msg_ctrl = self.read_config_u16(cap_offset + 2);
        self.write_config_u16(cap_offset + 2, msg_ctrl | 0x1);
    }

    pub fn msi_disable(&self, cap_offset: u16) {
        let msg_ctrl = self.read_config_u16(cap_offset + 2);
        self.write_config_u16(cap_offset + 2, msg_ctrl & !0x1);
    }

    // ── MSI-X ──

    pub fn msix_capability(&self) -> Option<u16> {
        self.find_capability(0x11)
    }
}

// ── 动态设备管理 ────────────────────────────────────────────────────────

impl PciDevice {
    /// 从 config space 读取 PCI 信息，构造 PnpDevice 并注册到全局列表，
    /// 然后自动 probe 驱动。一步完成设备的完整发现-绑定流程。
    ///
    /// `dev_name_prefix` 为 `/dev` 下的命名前缀（如 `"nvme"`），
    /// 实际名称会拼接 bus-device-function 信息。
    pub fn register_and_probe(
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        dev_name_prefix: &str,
    ) -> Option<Arc<PnpDevice>> {
        let id = PnpId::Pci {
            segment,
            bus,
            device,
            function,
        };

        let info = PciDevice::read_device_info(segment, bus, device, function)?;

        let name: Box<str> = if function == 0 {
            alloc::format!("{}{:02x}:{:02x}", dev_name_prefix, bus, device).into()
        } else {
            alloc::format!("{}{:02x}:{:02x}.{}", dev_name_prefix, bus, device, function).into()
        };

        let pnp = PnpDevice::new(id, name, Box::new(info));

        if PNP_DEVICES.push(Arc::clone(&pnp)).is_err() {
            return None;
        }

        match PNP_DRIVERS.probe_device(&pnp) {
            Ok(()) | Err(PnpError::NoDriver) => Some(pnp),
            Err(_) => {
                PNP_DEVICES.remove(&pnp.id);
                None
            }
        }
    }

    /// 从 config space 读取完整的 [`PciInfo`]。
    ///
    /// 需要 `PCI_CONFIG` 已设置。返回 `None` 表示设备不存在
    /// （vendor == 0xFFFF）或无法访问 config space。
    pub fn read_device_info(
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
    ) -> Option<PciInfo> {
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref()?;

        let vendor = (cfg.read_u16)(segment, bus, device, function, 0x00);
        if vendor == 0xFFFF {
            return None;
        }

        let device_id = (cfg.read_u16)(segment, bus, device, function, 0x02);
        let class_raw = (cfg.read_u32)(segment, bus, device, function, 0x08);
        let revision = (cfg.read_u8)(segment, bus, device, function, 0x08);
        let header_type = (cfg.read_u8)(segment, bus, device, function, 0x0E);
        let subsystem_vendor = (cfg.read_u16)(segment, bus, device, function, 0x2C);
        let subsystem_id = (cfg.read_u16)(segment, bus, device, function, 0x2E);

        let class = class_raw >> 8;
        let subclass = (class_raw >> 16) as u8;
        let prog_if = (class_raw >> 24) as u8;
        let multi_function = header_type & 0x80 != 0;

        Some(PciInfo {
            vendor,
            device_id,
            revision,
            class,
            subclass,
            prog_if,
            subsystem_vendor,
            subsystem_id,
            header_type: header_type & 0x7F,
            multi_function,
        })
    }

    /// 触发设备热拔移除。
    ///
    /// 等效于 `self.pnp().remove_device()`。
    pub fn remove_from_bus(&self) {
        self.pnp.remove_device();
    }
}

// ── PCI Bus 扫描器 ──────────────────────────────────────────────────────

/// 对指定 segment 内的 bus 范围进行 PCI 设备扫描。
///
/// 对每个存在且有效的 PCI function 调用 `on_device` 回调。
/// 回调返回 `true` 继续扫描下一设备，`false` 提前终止。
///
/// # 参数
/// - `segment`: PCI segment group（通常为 0）
/// - `start_bus`: 起始 bus 号（含）
/// - `end_bus`: 结束 bus 号（含）
/// - `on_device`: 设备发现回调
///
/// # 返回
/// 扫描到的设备数量。
pub fn pci_scan_bus_range(
    segment: u16,
    start_bus: u8,
    end_bus: u8,
    on_device: &mut dyn FnMut(u16, u8, u8, u8) -> bool,
) -> usize {
    let guard = PCI_CONFIG.lock();
    let cfg = match guard.as_ref() {
        Some(cfg) => cfg,
        None => return 0,
    };

    let read_u16 = cfg.read_u16;
    let read_u8 = cfg.read_u8;
    drop(guard);

    let mut count = 0usize;

    for bus in start_bus..=end_bus {
        for device in 0u8..32 {
            let vendor = read_u16(segment, bus, device, 0, 0x00);
            if vendor == 0xFFFF {
                continue;
            }

            if !on_device(segment, bus, device, 0) {
                return count + 1;
            }
            count += 1;

            let header_type = read_u8(segment, bus, device, 0, 0x0E);
            if header_type & 0x80 == 0 {
                continue;
            }

            for function in 1u8..8 {
                let vendor = read_u16(segment, bus, device, function, 0x00);
                if vendor == 0xFFFF {
                    continue;
                }
                if !on_device(segment, bus, device, function) {
                    return count + 1;
                }
                count += 1;
            }
        }
    }

    count
}

/// 扫描 PCI bus 并以 PnP 之名注册所有发现的设备。
///
/// 这是 [`pci_scan_bus_range`] 的便捷封装：自动调用
/// [`PciDevice::register_and_probe`] 完成发现→注册→probe 流程。
///
/// 返回成功注册的设备数量。
pub fn pci_scan_and_register(
    segment: u16,
    start_bus: u8,
    end_bus: u8,
    dev_name_prefix: &str,
) -> usize {
    let mut count = 0usize;
    pci_scan_bus_range(segment, start_bus, end_bus, &mut |seg, bus, dev, func| {
        if PciDevice::register_and_probe(seg, bus, dev, func, dev_name_prefix).is_some() {
            count += 1;
        }
        true
    });
    count
}

/// 扫描总线上的设备并只返回 BDF + vendor/device 的原始信息，
/// 不执行 PnP 注册。
///
/// 用于固件早期阶段仅需检查是否有特定设备存在的场景。
#[derive(Clone, Copy, Debug)]
pub struct PciRawDevice {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor: u16,
    pub device_id: u16,
    pub class: u32,
    pub header_type: u8,
    pub multi_function: bool,
}

pub fn pci_scan_raw(segment: u16, start_bus: u8, end_bus: u8) -> alloc::vec::Vec<PciRawDevice> {
    let mut devices = alloc::vec::Vec::new();

    let guard = PCI_CONFIG.lock();
    let cfg = match guard.as_ref() {
        Some(cfg) => cfg,
        None => return devices,
    };

    let read_u16 = cfg.read_u16;
    let read_u32 = cfg.read_u32;
    let read_u8 = cfg.read_u8;
    drop(guard);

    for bus in start_bus..=end_bus {
        for device in 0u8..32 {
            let vendor = read_u16(segment, bus, device, 0, 0x00);
            if vendor == 0xFFFF {
                continue;
            }

            let device_id = read_u16(segment, bus, device, 0, 0x02);
            let class_raw = read_u32(segment, bus, device, 0, 0x08);
            let header_type_raw = read_u8(segment, bus, device, 0, 0x0E);

            devices.push(PciRawDevice {
                segment,
                bus,
                device,
                function: 0,
                vendor,
                device_id,
                class: class_raw & 0x00FF_FFFF,
                header_type: header_type_raw & 0x7F,
                multi_function: header_type_raw & 0x80 != 0,
            });

            if header_type_raw & 0x80 == 0 {
                continue;
            }

            for function in 1u8..8 {
                let vendor = read_u16(segment, bus, device, function, 0x00);
                if vendor == 0xFFFF {
                    continue;
                }

                let device_id = read_u16(segment, bus, device, function, 0x02);
                let class_raw = read_u32(segment, bus, device, function, 0x08);
                let header_type = read_u8(segment, bus, device, function, 0x0E);

                devices.push(PciRawDevice {
                    segment,
                    bus,
                    device,
                    function,
                    vendor,
                    device_id,
                    class: class_raw & 0x00FF_FFFF,
                    header_type: header_type & 0x7F,
                    multi_function: false,
                });
            }
        }
    }

    devices
}

// ── PciBar helpers ───────────────────────────────────────────────────────

impl PciBar {
    pub fn is_memory(&self) -> bool {
        matches!(self.bar_type, PciBarType::Memory)
    }

    pub fn is_io(&self) -> bool {
        matches!(self.bar_type, PciBarType::Io)
    }
}

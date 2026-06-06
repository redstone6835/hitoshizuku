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
//! let pnp = PnpDevice::new(id, "pci-0000:01:00.0".into(), info);
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

use super::pnp::{BusType, PNP_DEVICES, PNP_DRIVERS, PnpBusInfo, PnpDevice, PnpError, PnpId};

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
        ((self.class >> 16) as u8, self.subclass, self.prog_if)
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
    fn bus_type(&self) -> BusType {
        BusType::PCI
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

// ── PCI config space 常量 ────────────────────────────────────────────────

const PCI_COMMAND_OFFSET: u16 = 0x04;
const PCI_STATUS_OFFSET: u16 = 0x06;
const PCI_CAPABILITY_LIST_OFFSET: u16 = 0x34;

const PCI_COMMAND_IO_SPACE: u16 = 0x0001;
const PCI_COMMAND_MEMORY_SPACE: u16 = 0x0002;
const PCI_COMMAND_BUS_MASTER: u16 = 0x0004;
const PCI_COMMAND_INTERRUPT_DISABLE: u16 = 0x0400;
const PCI_STATUS_CAPABILITIES_LIST: u16 = 0x0010;

const PCI_STANDARD_CONFIG_SPACE_SIZE: u16 = 0x100;
const PCI_CAPABILITY_MIN_OFFSET: u16 = 0x40;
const PCI_CAPABILITY_MAX_OFFSET: u16 = 0xFC;
const PCI_CAPABILITY_HEADER_SIZE: u16 = 2;
const PCI_CAPABILITY_MAX_STEPS: usize =
    ((PCI_CAPABILITY_MAX_OFFSET - PCI_CAPABILITY_MIN_OFFSET) / 4 + 1) as usize;
// PCI 常规配置空间每条 bus 最多有 32 个 device 编号。
const PCI_DEVICES_PER_BUS: u8 = 32;
// 每个 PCI device 最多有 8 个 function，0 号 function 必须先探测。
const PCI_FUNCTIONS_PER_DEVICE: u8 = 8;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciConfigError {
    InvalidDevice,
    Uninitialized,
}

// ── PCI capability 遍历 ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciCapability {
    pub offset: u16,
    pub id: u8,
    pub next_offset: Option<u16>,
}

pub struct PciCapabilityIter<'a> {
    device: &'a PciDevice,
    next_offset: Option<u16>,
    remaining: usize,
    visited: u64,
}

fn pci_config_range_valid(offset: u16, len: u16) -> bool {
    offset <= PCI_STANDARD_CONFIG_SPACE_SIZE
        && len <= PCI_STANDARD_CONFIG_SPACE_SIZE.saturating_sub(offset)
}

fn valid_capability_offset(offset: u16) -> bool {
    offset >= PCI_CAPABILITY_MIN_OFFSET
        && offset <= PCI_CAPABILITY_MAX_OFFSET
        && offset & 0x3 == 0
        && pci_config_range_valid(offset, PCI_CAPABILITY_HEADER_SIZE)
}

fn valid_capability_pointer(raw: u8) -> Option<u16> {
    let offset = raw as u16;
    if offset == 0 || !valid_capability_offset(offset) {
        return None;
    }
    Some(offset)
}

fn capability_visited_bit(offset: u16) -> Option<u64> {
    if !valid_capability_offset(offset) {
        return None;
    }
    let idx = (offset - PCI_CAPABILITY_MIN_OFFSET) / 4;
    Some(1u64 << idx as u32)
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

    pub fn try_read_config_u8(&self, offset: u16) -> Result<u8, PciConfigError> {
        let (seg, bus, dev, func) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
        Ok((cfg.read_u8)(seg, bus, dev, func, offset))
    }

    pub fn try_read_config_u16(&self, offset: u16) -> Result<u16, PciConfigError> {
        let (seg, bus, dev, func) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
        Ok((cfg.read_u16)(seg, bus, dev, func, offset))
    }

    pub fn try_read_config_u32(&self, offset: u16) -> Result<u32, PciConfigError> {
        let (seg, bus, dev, func) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
        Ok((cfg.read_u32)(seg, bus, dev, func, offset))
    }

    pub fn try_write_config_u8(&self, offset: u16, value: u8) -> Result<(), PciConfigError> {
        let (seg, bus, dev, func) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
        (cfg.write_u8)(seg, bus, dev, func, offset, value);
        Ok(())
    }

    pub fn try_write_config_u16(&self, offset: u16, value: u16) -> Result<(), PciConfigError> {
        let (seg, bus, dev, func) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
        (cfg.write_u16)(seg, bus, dev, func, offset, value);
        Ok(())
    }

    pub fn try_write_config_u32(&self, offset: u16, value: u32) -> Result<(), PciConfigError> {
        let (seg, bus, dev, func) = self.bdf().ok_or(PciConfigError::InvalidDevice)?;
        let guard = PCI_CONFIG.lock();
        let cfg = guard.as_ref().ok_or(PciConfigError::Uninitialized)?;
        (cfg.write_u32)(seg, bus, dev, func, offset, value);
        Ok(())
    }

    // 这些兼容读取只用于容错查询；probe/初始化路径需要区分错误时应使用 try_*。
    pub fn read_config_u8(&self, offset: u16) -> u8 {
        self.try_read_config_u8(offset).unwrap_or(0)
    }

    pub fn read_config_u16(&self, offset: u16) -> u16 {
        self.try_read_config_u16(offset).unwrap_or(0)
    }

    pub fn read_config_u32(&self, offset: u16) -> u32 {
        self.try_read_config_u32(offset).unwrap_or(0)
    }

    pub fn write_config_u8(&self, offset: u16, value: u8) {
        let _ = self.try_write_config_u8(offset, value);
    }

    pub fn write_config_u16(&self, offset: u16, value: u16) {
        let _ = self.try_write_config_u16(offset, value);
    }

    pub fn write_config_u32(&self, offset: u16, value: u32) {
        let _ = self.try_write_config_u32(offset, value);
    }

    // ── BAR ──

    pub fn map_bar(&self, idx: usize) -> Option<PciBar> {
        if idx > 5 {
            return None;
        }
        let offset = 0x10u16 + (idx as u16) * 4;
        let bar_val = self.try_read_config_u32(offset).ok()?;

        if bar_val == 0 {
            return None;
        }

        let is_mmio = bar_val & 0x1 == 0;
        let prefetchable = is_mmio && (bar_val & 0x8) != 0;

        let (bar_type, phys_addr, size) = if is_mmio {
            let is_64 = match (bar_val >> 1) & 0x3 {
                0 => false,
                2 if idx < 5 => true,
                _ => return None,
            };
            let high_offset = offset + 4;
            let high_val = if is_64 {
                self.try_read_config_u32(high_offset).ok()?
            } else {
                0
            };
            let phys_addr = ((high_val as u64) << 32) | ((bar_val & 0xFFFF_FFF0) as u64);

            let cmd = self.try_read_config_u16(PCI_COMMAND_OFFSET).ok()?;
            self.try_write_config_u16(PCI_COMMAND_OFFSET, cmd & !PCI_COMMAND_MEMORY_SPACE)
                .ok()?;

            let size_bits = (|| -> Option<u64> {
                if is_64 {
                    self.try_write_config_u32(high_offset, u32::MAX).ok()?;
                }
                self.try_write_config_u32(offset, 0xFFFF_FFF0).ok()?;
                let size_lo = self.try_read_config_u32(offset).ok()? & 0xFFFF_FFF0;
                let size_hi = if is_64 {
                    self.try_read_config_u32(high_offset).ok()?
                } else {
                    0
                };
                Some(((size_hi as u64) << 32) | size_lo as u64)
            })();

            let _ = self.try_write_config_u32(offset, bar_val);
            if is_64 {
                let _ = self.try_write_config_u32(high_offset, high_val);
            }
            let _ = self.try_write_config_u16(PCI_COMMAND_OFFSET, cmd);

            let size_bits = size_bits?;
            if size_bits == 0 {
                return None;
            }
            (PciBarType::Memory, phys_addr, (!size_bits).wrapping_add(1))
        } else {
            let phys_addr = (bar_val & 0xFFFF_FFFC) as u64;
            let cmd = self.try_read_config_u16(PCI_COMMAND_OFFSET).ok()?;
            self.try_write_config_u16(PCI_COMMAND_OFFSET, cmd & !PCI_COMMAND_IO_SPACE)
                .ok()?;

            let size_bits = (|| -> Option<u32> {
                self.try_write_config_u32(offset, 0xFFFF_FFFC).ok()?;
                Some(self.try_read_config_u32(offset).ok()? & 0xFFFF_FFFC)
            })();

            let _ = self.try_write_config_u32(offset, bar_val);
            let _ = self.try_write_config_u16(PCI_COMMAND_OFFSET, cmd);

            let size_bits = size_bits?;
            if size_bits == 0 {
                return None;
            }
            (
                PciBarType::Io,
                phys_addr,
                (!size_bits).wrapping_add(1) as u64,
            )
        };

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
        let cmd = self.read_config_u16(PCI_COMMAND_OFFSET);
        self.write_config_u16(PCI_COMMAND_OFFSET, cmd | PCI_COMMAND_BUS_MASTER);
    }

    pub fn disable_bus_master(&self) {
        let cmd = self.read_config_u16(PCI_COMMAND_OFFSET);
        self.write_config_u16(PCI_COMMAND_OFFSET, cmd & !PCI_COMMAND_BUS_MASTER);
    }

    pub fn bus_master_enabled(&self) -> bool {
        self.read_config_u16(PCI_COMMAND_OFFSET) & PCI_COMMAND_BUS_MASTER != 0
    }

    pub fn enable_mmio(&self) {
        let cmd = self.read_config_u16(PCI_COMMAND_OFFSET);
        self.write_config_u16(PCI_COMMAND_OFFSET, cmd | PCI_COMMAND_MEMORY_SPACE);
    }

    pub fn enable_io(&self) {
        let cmd = self.read_config_u16(PCI_COMMAND_OFFSET);
        self.write_config_u16(PCI_COMMAND_OFFSET, cmd | PCI_COMMAND_IO_SPACE);
    }

    pub fn disable_interrupts(&self) {
        let cmd = self.read_config_u16(PCI_COMMAND_OFFSET);
        self.write_config_u16(PCI_COMMAND_OFFSET, cmd | PCI_COMMAND_INTERRUPT_DISABLE);
    }

    pub fn enable_interrupts(&self) {
        let cmd = self.read_config_u16(PCI_COMMAND_OFFSET);
        self.write_config_u16(PCI_COMMAND_OFFSET, cmd & !PCI_COMMAND_INTERRUPT_DISABLE);
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
        if pin == 0 { None } else { Some(pin) }
    }

    // ── capability 遍历 ──

    pub fn capabilities_offset(&self) -> Option<u16> {
        let status = self.try_read_config_u16(PCI_STATUS_OFFSET).ok()?;
        if status & PCI_STATUS_CAPABILITIES_LIST == 0 {
            return None;
        }
        valid_capability_pointer(self.try_read_config_u8(PCI_CAPABILITY_LIST_OFFSET).ok()?)
    }

    pub fn capabilities(&self) -> PciCapabilityIter<'_> {
        PciCapabilityIter::new(self)
    }

    pub fn find_capability(&self, cap_id: u8) -> Option<u16> {
        self.capabilities()
            .find(|cap| cap.id == cap_id)
            .map(|cap| cap.offset)
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

impl<'a> PciCapabilityIter<'a> {
    fn new(device: &'a PciDevice) -> Self {
        Self {
            device,
            next_offset: device.capabilities_offset(),
            remaining: PCI_CAPABILITY_MAX_STEPS,
            visited: 0,
        }
    }
}

impl<'a> Iterator for PciCapabilityIter<'a> {
    type Item = PciCapability;

    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next_offset?;
        if self.remaining == 0 {
            self.next_offset = None;
            return None;
        }
        self.remaining -= 1;

        let visited_bit = match capability_visited_bit(offset) {
            Some(bit) => bit,
            None => {
                self.next_offset = None;
                return None;
            }
        };
        if self.visited & visited_bit != 0 {
            self.next_offset = None;
            return None;
        }
        self.visited |= visited_bit;

        let id = match self.device.try_read_config_u8(offset) {
            Ok(id) => id,
            Err(_) => {
                self.next_offset = None;
                return None;
            }
        };
        let next_offset = match self.device.try_read_config_u8(offset + 1) {
            Ok(raw) => valid_capability_pointer(raw),
            Err(_) => None,
        };

        self.next_offset = next_offset;

        Some(PciCapability {
            offset,
            id,
            next_offset,
        })
    }
}

// ── 动态设备管理 ────────────────────────────────────────────────────────

fn pci_hardware_name(segment: u16, bus: u8, device: u8, function: u8) -> Box<str> {
    alloc::format!("pci-{segment:04x}:{bus:02x}:{device:02x}.{function}").into()
}

impl PciDevice {
    /// 从 config space 读取 PCI 信息，构造 PnpDevice 并注册到全局列表，
    /// 然后自动 probe 驱动。一步完成设备的完整发现-绑定流程。
    ///
    /// PnP 设备名是稳定硬件名 `pci-{seg:04x}:{bus:02x}:{dev:02x}.{func}`，
    /// 不承载 `/dev` 节点命名前缀语义。
    pub fn register_and_probe(
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
    ) -> Option<Arc<PnpDevice>> {
        let id = PnpId::Pci {
            segment,
            bus,
            device,
            function,
        };

        let info = PciDevice::read_device_info(segment, bus, device, function)?;

        let name = pci_hardware_name(segment, bus, device, function);
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
    pub fn read_device_info(segment: u16, bus: u8, device: u8, function: u8) -> Option<PciInfo> {
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

        let class = (class_raw >> 8) & 0x00FF_FFFF;
        let subclass = (class_raw >> 16) as u8;
        let prog_if = (class_raw >> 8) as u8;
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
        for device in 0u8..PCI_DEVICES_PER_BUS {
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

            for function in 1u8..PCI_FUNCTIONS_PER_DEVICE {
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
pub fn pci_scan_and_register(segment: u16, start_bus: u8, end_bus: u8) -> usize {
    let mut count = 0usize;
    pci_scan_bus_range(segment, start_bus, end_bus, &mut |seg, bus, dev, func| {
        if PciDevice::register_and_probe(seg, bus, dev, func).is_some() {
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
        for device in 0u8..PCI_DEVICES_PER_BUS {
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

            for function in 1u8..PCI_FUNCTIONS_PER_DEVICE {
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

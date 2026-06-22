//! DTB 路径下的 PCIe host bridge 发现 + ECAM 配置空间访问 + BAR 资源分配。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use general::dev::irq::{self, IrqLine};
use general::dev::msi;
use general::dev::pci::{
    PCI_DEVICES_PER_BUS, PCI_EXTENDED_CONFIG_SPACE_SIZE, PCI_FUNCTIONS_PER_DEVICE, PciConfigAccess,
    PciConfigError, PciDevice, PciHostAddressSpace, PciHostBridgeError, PciHostBridgeInfo,
    PciHostBridgeWindow, pci_scan_raw, register_host_bridge, set_pci_config_access,
};
use general::dev::pnp::PnpDevice;
use general::firmware::dtb::{DtbPciAddressSpace, DtbPciRangeInfo, DtbPcieHostInfo};
use vfs::sync::Spinlock;

/// ECAM 全局状态:base(虚拟地址)+ 总线范围。
///
/// `ECAM_VBASE == 0` 表示尚未初始化,config access 函数会安全地返回默认值。
static ECAM_VBASE: AtomicU64 = AtomicU64::new(0);
static ECAM_SIZE: AtomicU64 = AtomicU64::new(0);
static BUS_START: AtomicU32 = AtomicU32::new(0);
static BUS_END: AtomicU32 = AtomicU32::new(0);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

struct PciIrqRoute {
    child_key: Box<[u32]>,
    parent: u32,
    parent_specifier: Box<[u32]>,
}

struct PciIrqRouting {
    segment: u16,
    bus_start: u8,
    bus_end: u8,
    address_cells: usize,
    interrupt_cells: usize,
    mask: Box<[u32]>,
    routes: Vec<PciIrqRoute>,
}

static PCI_IRQ_ROUTING: Spinlock<Option<PciIrqRouting>> = Spinlock::new(None);

struct PciMsiRoute {
    requester_base: u32,
    controller: u32,
    msi_base: u32,
    length: u32,
}

struct PciMsiRouting {
    segment: u16,
    bus_start: u8,
    bus_end: u8,
    routes: Vec<PciMsiRoute>,
}

static PCI_MSI_ROUTING: Spinlock<Option<PciMsiRouting>> = Spinlock::new(None);

pub(crate) fn register_pci_host_bridge(host: &DtbPcieHostInfo, pnp: Option<Arc<PnpDevice>>) {
    let info = PciHostBridgeInfo {
        name: host.name.into(),
        firmware_path: Some(host.path.into()),
        domain: host.domain,
        bus_start: host.bus_start,
        bus_end: host.bus_end,
        ecam_phys: host.ecam_phys,
        ecam_size: host.ecam_size,
        dma_coherent: host.dma_coherent,
        windows: host.ranges.iter().map(pci_host_window).collect(),
        irq_route_count: host.interrupt_map.len(),
        msi_route_count: host.msi_map.len(),
    };
    match register_host_bridge(info, pnp) {
        Ok(handle) => log::printk!(
            "[kernel-start][dtb] registered PCI host bridge {} handle={} windows={} irq-routes={} msi-routes={}",
            host.path,
            handle.id(),
            host.ranges.len(),
            host.interrupt_map.len(),
            host.msi_map.len()
        ),
        Err(PciHostBridgeError::AlreadyRegistered) => log::debug!(
            "[kernel-start][dtb] PCI host bridge domain {} already registered",
            host.domain
        ),
        Err(err) => log::printk!(
            "[kernel-start][dtb] failed to register PCI host bridge {}: {:?}",
            host.path,
            err
        ),
    }
}

fn pci_host_window(range: &DtbPciRangeInfo) -> PciHostBridgeWindow {
    PciHostBridgeWindow {
        space: match range.space {
            DtbPciAddressSpace::Io => PciHostAddressSpace::Io,
            DtbPciAddressSpace::Memory => PciHostAddressSpace::Memory,
            DtbPciAddressSpace::PrefetchableMemory => PciHostAddressSpace::PrefetchableMemory,
            DtbPciAddressSpace::Unknown(value) => PciHostAddressSpace::Unknown(value),
        },
        pci_start: range.child_addr,
        cpu_start: range.parent_addr,
        size: range.size,
    }
}

/// ECAM 配置空间地址计算。
///
/// 未安装 ECAM 与 BDF/offset 越界属于不同错误：前者表示平台初始化顺序错误，
/// 后者表示调用方访问了当前 host bridge 不覆盖的 function 或寄存器。
#[inline]
fn ecam_addr(bus: u8, device: u8, function: u8, offset: u16) -> Result<usize, PciConfigError> {
    let base = ECAM_VBASE.load(Ordering::Acquire);
    if base == 0 {
        return Err(PciConfigError::Uninitialized);
    }
    let size = ECAM_SIZE.load(Ordering::Acquire);
    let start = BUS_START.load(Ordering::Acquire) as u8;
    let end = BUS_END.load(Ordering::Acquire) as u8;
    if bus < start
        || bus > end
        || device >= PCI_DEVICES_PER_BUS
        || function >= PCI_FUNCTIONS_PER_DEVICE
        || offset >= PCI_EXTENDED_CONFIG_SPACE_SIZE
    {
        return Err(PciConfigError::InvalidOffset);
    }
    let rel_bus = (bus - start) as u64;
    let off = (rel_bus << 20) | ((device as u64) << 15) | ((function as u64) << 12) | offset as u64;
    if off >= size {
        return Err(PciConfigError::InvalidOffset);
    }
    Ok((base + off) as usize)
}

fn ecam_read_u8(_seg: u16, bus: u8, dev: u8, func: u8, offset: u16) -> Result<u8, PciConfigError> {
    let a = ecam_addr(bus, dev, func, offset)?;
    Ok(unsafe { core::ptr::read_volatile(a as *const u8) })
}
fn ecam_read_u16(
    _seg: u16,
    bus: u8,
    dev: u8,
    func: u8,
    offset: u16,
) -> Result<u16, PciConfigError> {
    let a = ecam_addr(bus, dev, func, offset)?;
    Ok(unsafe { core::ptr::read_volatile(a as *const u16) })
}
fn ecam_read_u32(
    _seg: u16,
    bus: u8,
    dev: u8,
    func: u8,
    offset: u16,
) -> Result<u32, PciConfigError> {
    let a = ecam_addr(bus, dev, func, offset)?;
    Ok(unsafe { core::ptr::read_volatile(a as *const u32) })
}
fn ecam_write_u8(
    _seg: u16,
    bus: u8,
    dev: u8,
    func: u8,
    offset: u16,
    v: u8,
) -> Result<(), PciConfigError> {
    let a = ecam_addr(bus, dev, func, offset)?;
    unsafe { core::ptr::write_volatile(a as *mut u8, v) };
    Ok(())
}
fn ecam_write_u16(
    _seg: u16,
    bus: u8,
    dev: u8,
    func: u8,
    offset: u16,
    v: u16,
) -> Result<(), PciConfigError> {
    let a = ecam_addr(bus, dev, func, offset)?;
    unsafe { core::ptr::write_volatile(a as *mut u16, v) };
    Ok(())
}
fn ecam_write_u32(
    _seg: u16,
    bus: u8,
    dev: u8,
    func: u8,
    offset: u16,
    v: u32,
) -> Result<(), PciConfigError> {
    let a = ecam_addr(bus, dev, func, offset)?;
    unsafe { core::ptr::write_volatile(a as *mut u32, v) };
    Ok(())
}

// ECAM config access 转发的 device_mmio_to_virt —— 装载时由启动上下文传入,
// 用于把 BAR 物理地址转成当前平台可访问的内核虚拟地址。
// 默认 identity(装载前不会被用到,只是为了让类型检查通过)。
static DEVICE_MMIO_TO_VIRT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

const PCI_BAR0_OFFSET: u16 = 0x10;
const PCI_BAR_STRIDE: u16 = 4;
const PCI_BAR_IO_SPACE: u32 = 0x1;
const PCI_BAR_MEM_TYPE_MASK: u32 = 0x6;
const PCI_BAR_MEM_TYPE_64: u32 = 0x4;
const PCI_BAR_MEM_ADDR_MASK: u32 = 0xffff_fff0;
const PCI_BAR_PROBE_VALUE: u32 = 0xffff_ffff;
const PCI_BAR_MIN_ALIGN: u64 = 0x10;
const PCI_32BIT_MMIO_END: u64 = 1u64 << 32;

fn mmio_to_virt_via_stored(phys: usize) -> usize {
    let f = DEVICE_MMIO_TO_VIRT.load(Ordering::Acquire);
    if f == 0 {
        return phys;
    }
    // Safety: 存入的是合法的 fn 指针(由 install_ecam 只写一次)。
    let f: fn(usize) -> usize = unsafe { core::mem::transmute(f) };
    f(phys)
}

fn pci_child_interrupt_key(
    bus: u8,
    device: u8,
    function: u8,
    interrupt_pin: u8,
    address_cells: usize,
    interrupt_cells: usize,
) -> Option<Box<[u32]>> {
    let total = address_cells.checked_add(interrupt_cells)?;
    if address_cells == 0 || interrupt_cells == 0 {
        return None;
    }
    let mut cells = Vec::new();
    cells.resize(total, 0);
    // Open Firmware PCI child address 的第一个 cell 保存 bus/device/function。
    // 其余地址 cell 对 INTx 路由通常为 0；interrupt-map-mask 会决定参与匹配的位。
    cells[0] = ((bus as u32) << 16) | ((device as u32) << 11) | ((function as u32) << 8);
    cells[address_cells] = interrupt_pin as u32;
    Some(cells.into_boxed_slice())
}

fn masked_cells_match(candidate: &[u32], route: &[u32], mask: &[u32]) -> bool {
    candidate.len() == route.len()
        && route.len() == mask.len()
        && candidate
            .iter()
            .zip(route.iter())
            .zip(mask.iter())
            .all(|((candidate, route), mask)| (candidate & mask) == (route & mask))
}

fn resolve_pci_irq(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    interrupt_pin: Option<u8>,
    _interrupt_line: Option<u8>,
) -> Option<IrqLine> {
    let interrupt_pin = interrupt_pin?;
    let routing = PCI_IRQ_ROUTING.lock();
    let routing = routing.as_ref()?;
    if segment != routing.segment || bus < routing.bus_start || bus > routing.bus_end {
        return None;
    }
    let key = pci_child_interrupt_key(
        bus,
        device,
        function,
        interrupt_pin,
        routing.address_cells,
        routing.interrupt_cells,
    )?;
    routing
        .routes
        .iter()
        .find(|route| masked_cells_match(&key, &route.child_key, &routing.mask))
        .and_then(|route| irq::translate_firmware_irq(Some(route.parent), &route.parent_specifier))
}

fn pci_requester_id(bus: u8, device: u8, function: u8) -> u32 {
    ((bus as u32) << 8) | ((device as u32) << 3) | function as u32
}

fn resolve_pci_msi(segment: u16, bus: u8, device: u8, function: u8) -> Option<msi::MsiHandle> {
    let routing = PCI_MSI_ROUTING.lock();
    let routing = routing.as_ref()?;
    if segment != routing.segment || bus < routing.bus_start || bus > routing.bus_end {
        return None;
    }
    let requester = pci_requester_id(bus, device, function);
    routing.routes.iter().find_map(|route| {
        let offset = requester.checked_sub(route.requester_base)?;
        if offset >= route.length {
            return None;
        }
        let mapped = route.msi_base.checked_add(offset)?;
        msi::allocate_msi(route.controller, mapped).ok()
    })
}

pub(crate) fn install_irq_routing(segment: u16, host: &DtbPcieHostInfo) -> bool {
    let expected = match host.address_cells.checked_add(host.interrupt_cells) {
        Some(expected) if expected != 0 => expected,
        _ => return false,
    };
    let Some(mask) = host.interrupt_map_mask.as_ref() else {
        return false;
    };
    if mask.len() != expected || host.interrupt_map.is_empty() {
        return false;
    }

    let mut routes = Vec::new();
    for entry in &host.interrupt_map {
        if entry.child_address.len() != host.address_cells
            || entry.child_interrupt.len() != host.interrupt_cells
            || entry.child_address.len() + entry.child_interrupt.len() != expected
            || entry.parent_specifier.is_empty()
        {
            continue;
        }
        let mut child_key = Vec::new();
        child_key.extend_from_slice(&entry.child_address);
        child_key.extend_from_slice(&entry.child_interrupt);
        routes.push(PciIrqRoute {
            child_key: child_key.into_boxed_slice(),
            parent: entry.parent,
            parent_specifier: entry.parent_specifier.clone(),
        });
    }
    if routes.is_empty() {
        return false;
    }

    *PCI_IRQ_ROUTING.lock() = Some(PciIrqRouting {
        segment,
        bus_start: host.bus_start,
        bus_end: host.bus_end,
        address_cells: host.address_cells,
        interrupt_cells: host.interrupt_cells,
        mask: mask.clone(),
        routes,
    });
    true
}

pub(crate) fn install_msi_routing(segment: u16, host: &DtbPcieHostInfo) -> bool {
    let mut routes = Vec::new();
    for entry in &host.msi_map {
        if entry.length == 0 {
            continue;
        }
        routes.push(PciMsiRoute {
            requester_base: entry.requester_base,
            controller: entry.controller,
            msi_base: entry.msi_base,
            length: entry.length,
        });
    }
    if routes.is_empty() {
        return false;
    }
    *PCI_MSI_ROUTING.lock() = Some(PciMsiRouting {
        segment,
        bus_start: host.bus_start,
        bus_end: host.bus_end,
        routes,
    });
    true
}

/// 装载 ECAM 访问。`phys_base` 是物理地址,`device_mmio_to_virt` 负责转虚拟。
/// 成功返回 `true`。重复调用会覆盖。
pub(crate) fn install_ecam(
    phys_base: u64,
    size: u64,
    bus_start: u8,
    bus_end: u8,
    device_mmio_to_virt: fn(usize) -> usize,
) -> bool {
    let vbase = device_mmio_to_virt(phys_base as usize) as u64;
    ECAM_VBASE.store(vbase, Ordering::Release);
    ECAM_SIZE.store(size, Ordering::Release);
    BUS_START.store(bus_start as u32, Ordering::Release);
    BUS_END.store(bus_end as u32, Ordering::Release);
    DEVICE_MMIO_TO_VIRT.store(device_mmio_to_virt as usize, Ordering::Release);
    if !INITIALIZED.swap(true, Ordering::AcqRel) {
        set_pci_config_access(PciConfigAccess {
            read_u8: ecam_read_u8,
            read_u16: ecam_read_u16,
            read_u32: ecam_read_u32,
            write_u8: ecam_write_u8,
            write_u16: ecam_write_u16,
            write_u32: ecam_write_u32,
            // 关键:BAR 物理地址要经过启动上下文提供的 MMIO 翻译,而不是 identity。
            device_mmio_to_virt: mmio_to_virt_via_stored,
            resolve_irq: Some(resolve_pci_irq),
            allocate_msi: Some(resolve_pci_msi),
        });
    }
    true
}

// 这一段处理启动入口未给 PCI function 预分配 BAR 的情况。fallback allocator
// 只消费平台层声明的 PCI MMIO 窗口，并按每个 function 的 header 类型枚举
// 实际 BAR 槽位；它不参与驱动匹配，也不制造 `/dev` 节点。
//
/// 扫一遍 PCI 总线并给每个 MMIO BAR 分配一段物理地址。必须在
/// [`install_ecam`] 之后、`pci_scan_and_register` 之前调用。
pub(crate) fn assign_bars(host: &DtbPcieHostInfo) {
    let Some(mmio_windows) = select_host_mmio_windows(host) else {
        log::printk!("[kernel-start][dtb] no fallback PCI MMIO window for this platform");
        return;
    };
    log::printk!(
        "[kernel-start][dtb] PCI BAR allocator window from {}: low={:#x}..{:#x} high={:#x}..{:#x}",
        host.path,
        mmio_windows.low32.start,
        mmio_windows.low32.end,
        mmio_windows.high.start,
        mmio_windows.high.end
    );
    let devices = pci_scan_raw(host.domain, host.bus_start, host.bus_end);
    let mut next = PciBarAllocatorCursor {
        low32: mmio_windows.low32.start,
        high: mmio_windows.high.start,
    };
    for d in devices.iter() {
        if d.bar_count() == 0 {
            log::debug!(
                "[kernel-start][dtb] skip PCI BAR fallback for unsupported header type {:#x} @ {:02x}:{:02x}.{}",
                d.header_type,
                d.bus,
                d.device,
                d.function
            );
            continue;
        }
        let pci = match PciDevice::new_unregistered(d.segment, d.bus, d.device, d.function) {
            Some(p) => p,
            None => continue,
        };
        assign_device_bars(&pci, &mut next, &mmio_windows);
    }
    log::printk!(
        "[kernel-start][dtb] assigned PCI BARs up to low={:#x} high={:#x}",
        next.low32,
        next.high
    );
}

struct PciBarMmioWindows {
    low32: Range<u64>,
    high: Range<u64>,
}

struct PciBarAllocatorCursor {
    low32: u64,
    high: u64,
}

fn select_host_mmio_windows(host: &DtbPcieHostInfo) -> Option<PciBarMmioWindows> {
    let low32 = host
        .ranges
        .iter()
        .filter(|range| matches!(range.space, DtbPciAddressSpace::Memory))
        .filter_map(pci_range_low32_mmio_window)
        .max_by_key(|range| range.end.saturating_sub(range.start));

    let high = select_host_mmio_window(host).or_else(hal::platform::default_pci_mmio_window);

    match (low32, high) {
        (Some(low32), Some(high)) => Some(PciBarMmioWindows { low32, high }),
        (Some(low32), None) => Some(PciBarMmioWindows {
            low32: low32.clone(),
            high: low32,
        }),
        (None, Some(high)) => Some(PciBarMmioWindows {
            low32: high.clone(),
            high,
        }),
        (None, None) => None,
    }
}

fn select_host_mmio_window(host: &DtbPcieHostInfo) -> Option<Range<u64>> {
    host.ranges
        .iter()
        .filter(|range| matches!(range.space, DtbPciAddressSpace::Memory))
        .filter_map(pci_range_mmio_window)
        .max_by_key(|range| range.end.saturating_sub(range.start))
        .or_else(|| {
            host.ranges
                .iter()
                .filter(|range| matches!(range.space, DtbPciAddressSpace::PrefetchableMemory))
                .filter_map(pci_range_mmio_window)
                .max_by_key(|range| range.end.saturating_sub(range.start))
        })
}

fn pci_range_mmio_window(range: &DtbPciRangeInfo) -> Option<Range<u64>> {
    match range.space {
        DtbPciAddressSpace::Memory | DtbPciAddressSpace::PrefetchableMemory => {}
        DtbPciAddressSpace::Io | DtbPciAddressSpace::Unknown(_) => return None,
    }
    let start = range.parent_addr as u64;
    let end = start.checked_add(range.size as u64)?;
    (end > start).then_some(start..end)
}

fn pci_range_low32_mmio_window(range: &DtbPciRangeInfo) -> Option<Range<u64>> {
    let window = pci_range_mmio_window(range)?;
    let end = window.end.min(PCI_32BIT_MMIO_END);
    (end > window.start).then_some(window.start..end)
}

/// 给一个 PCI 设备的所有 BAR 分配地址。`next` 是 bump allocator 游标。
fn assign_device_bars(
    pci: &PciDevice,
    next: &mut PciBarAllocatorCursor,
    mmio_windows: &PciBarMmioWindows,
) {
    let bar_count = pci.bar_count();
    if bar_count == 0 {
        return;
    }
    let original_command = match pci.try_command() {
        Ok(command) => command,
        Err(err) => {
            log::printk!(
                "[kernel-start][dtb] skip PCI BAR fallback for {}; command read failed: {:?}",
                pci.pnp_id(),
                err
            );
            return;
        }
    };
    if let Err(err) = pci.try_disable_mmio() {
        log::printk!(
            "[kernel-start][dtb] skip PCI BAR fallback for {}; cannot disable MMIO: {:?}",
            pci.pnp_id(),
            err
        );
        return;
    }

    let mut idx: u16 = 0;
    let mut assigned_any = false;
    while (idx as usize) < bar_count {
        let offset = PCI_BAR0_OFFSET + idx * PCI_BAR_STRIDE;
        let bar_val = match pci.try_read_config_u32(offset) {
            Ok(value) => value,
            Err(err) => {
                log::printk!(
                    "[kernel-start][dtb] stop PCI BAR fallback for {}; BAR{} read failed: {:?}",
                    pci.pnp_id(),
                    idx,
                    err
                );
                break;
            }
        };
        // fallback allocator 只处理 MMIO BAR。I/O BAR 需要独立的 I/O 空间窗口。
        let is_mmio = bar_val & PCI_BAR_IO_SPACE == 0;
        if !is_mmio {
            idx += 1;
            continue;
        }
        let is_64 = bar_val & PCI_BAR_MEM_TYPE_MASK == PCI_BAR_MEM_TYPE_64;
        if is_64 && (idx as usize + 1) >= bar_count {
            log::printk!(
                "[kernel-start][dtb] malformed 64-bit PCI BAR{} @ {}; missing high BAR slot",
                idx,
                pci.pnp_id()
            );
            break;
        }

        // 按 PCI BAR 规则写全 1 探测 size；探测期间 memory decode 已关闭。
        if let Err(err) = pci.try_write_config_u32(offset, PCI_BAR_PROBE_VALUE) {
            log::printk!(
                "[kernel-start][dtb] stop PCI BAR fallback for {}; BAR{} probe write failed: {:?}",
                pci.pnp_id(),
                idx,
                err
            );
            break;
        }
        let lo_size_raw = match pci.try_read_config_u32(offset) {
            Ok(value) => value,
            Err(err) => {
                let _ = pci.try_write_config_u32(offset, bar_val);
                log::printk!(
                    "[kernel-start][dtb] stop PCI BAR fallback for {}; BAR{} size read failed: {:?}",
                    pci.pnp_id(),
                    idx,
                    err
                );
                break;
            }
        };
        let (size, hi_offset): (u64, Option<u16>) = if is_64 {
            let hi_offset = offset + PCI_BAR_STRIDE;
            let hi_bar_val = match pci.try_read_config_u32(hi_offset) {
                Ok(value) => value,
                Err(err) => {
                    let _ = pci.try_write_config_u32(offset, bar_val);
                    log::printk!(
                        "[kernel-start][dtb] stop PCI BAR fallback for {}; BAR{} high read failed: {:?}",
                        pci.pnp_id(),
                        idx,
                        err
                    );
                    break;
                }
            };
            if let Err(err) = pci.try_write_config_u32(hi_offset, PCI_BAR_PROBE_VALUE) {
                let _ = pci.try_write_config_u32(offset, bar_val);
                log::printk!(
                    "[kernel-start][dtb] stop PCI BAR fallback for {}; BAR{} high probe write failed: {:?}",
                    pci.pnp_id(),
                    idx,
                    err
                );
                break;
            }
            let hi_size_raw = match pci.try_read_config_u32(hi_offset) {
                Ok(value) => value,
                Err(err) => {
                    let _ = pci.try_write_config_u32(offset, bar_val);
                    let _ = pci.try_write_config_u32(hi_offset, hi_bar_val);
                    log::printk!(
                        "[kernel-start][dtb] stop PCI BAR fallback for {}; BAR{} high size read failed: {:?}",
                        pci.pnp_id(),
                        idx,
                        err
                    );
                    break;
                }
            };
            let combined =
                ((hi_size_raw as u64) << 32) | ((lo_size_raw & PCI_BAR_MEM_ADDR_MASK) as u64);
            let sz = (!combined).wrapping_add(1);
            // 恢复低/高 BAR 原值；后续只有成功分配时才写入新地址。
            let _ = pci.try_write_config_u32(offset, bar_val);
            let _ = pci.try_write_config_u32(hi_offset, hi_bar_val);
            (sz, Some(hi_offset))
        } else {
            let masked = lo_size_raw & PCI_BAR_MEM_ADDR_MASK;
            let sz = (!(masked as u64) as u32).wrapping_add(1) as u64;
            let _ = pci.try_write_config_u32(offset, bar_val);
            (sz, None)
        };

        if size == 0 {
            idx += if is_64 { 2 } else { 1 };
            continue;
        }

        // BAR size 本身就是对齐要求；至少按 BAR 最低地址粒度对齐。
        let align = size.max(PCI_BAR_MIN_ALIGN);
        // 优先使用低 32-bit MMIO window：RISC-V 早期内核只直接映射了
        // PA 0..0x80000000，QEMU virtio-pci 的 64-bit BAR 放到高地址会无法访问。
        let (cursor, limit) = if is_64 && next.low32 >= mmio_windows.low32.end {
            (&mut next.high, mmio_windows.high.end)
        } else {
            (&mut next.low32, mmio_windows.low32.end)
        };
        let addr = (*cursor + align - 1) & !(align - 1);
        let Some(end) = addr.checked_add(size) else {
            log::printk!(
                "[kernel-start][dtb] PCI BAR pool address overflow (next={:#x} size={:#x})",
                *cursor,
                size
            );
            break;
        };
        if !is_64 && end > PCI_32BIT_MMIO_END {
            log::printk!(
                "[kernel-start][dtb] cannot place 32-bit PCI BAR{} @ {} above 4GiB (addr={:#x} size={:#x})",
                idx,
                pci.pnp_id(),
                addr,
                size
            );
            break;
        }
        if end > limit {
            log::printk!(
                "[kernel-start][dtb] PCI BAR pool exhausted (next={:#x})",
                *cursor
            );
            break;
        }
        *cursor = end;

        // 写回分配到的基址，保留 BAR 类型/属性位。
        let type_bits = bar_val & 0xf;
        let lo_val = (addr as u32 & PCI_BAR_MEM_ADDR_MASK) | type_bits;
        if let Err(err) = pci.try_write_config_u32(offset, lo_val) {
            log::printk!(
                "[kernel-start][dtb] stop PCI BAR fallback for {}; BAR{} assign write failed: {:?}",
                pci.pnp_id(),
                idx,
                err
            );
            break;
        }
        if let Some(hi) = hi_offset {
            if let Err(err) = pci.try_write_config_u32(hi, (addr >> 32) as u32) {
                log::printk!(
                    "[kernel-start][dtb] stop PCI BAR fallback for {}; BAR{} high assign write failed: {:?}",
                    pci.pnp_id(),
                    idx,
                    err
                );
                break;
            }
        }
        assigned_any = true;

        log::printk!(
            "[kernel-start][dtb]   BAR{} @ {} -> {:#x} size={:#x} (64bit={})",
            idx,
            pci.pnp_id(),
            addr,
            size,
            is_64
        );

        idx += if is_64 { 2 } else { 1 };
    }

    // 恢复探测前 command 状态，再为成功整理过资源的设备打开必要能力。
    let _ = pci.try_set_command(original_command);
    if assigned_any {
        let _ = pci.try_enable_mmio();
        let _ = pci.try_enable_bus_master();
    }
}

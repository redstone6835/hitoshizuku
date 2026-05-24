//! DTB 路径下的 PCIe host bridge 发现 + ECAM 配置空间访问 + BAR 资源分配。

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use general::dev::pci::{PciConfigAccess, PciDevice, pci_scan_raw, set_pci_config_access};
use general::dtb::DtbNode;

const DTB_COMPAT_PCI_ECAM: &[u8] = b"pci-host-ecam-generic";
const DTB_COMPAT_PCIE_ECAM: &[u8] = b"pcie-host-ecam-generic";

/// ECAM 全局状态:base(虚拟地址)+ 总线范围。
///
/// `ECAM_VBASE == 0` 表示尚未初始化,config access 函数会安全地返回默认值。
static ECAM_VBASE: AtomicU64 = AtomicU64::new(0);
static ECAM_SIZE: AtomicU64 = AtomicU64::new(0);
static BUS_START: AtomicU32 = AtomicU32::new(0);
static BUS_END: AtomicU32 = AtomicU32::new(0);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// ECAM 配置空间地址计算。越界返回 `None`。
#[inline]
fn ecam_addr(bus: u8, device: u8, function: u8, offset: u16) -> Option<usize> {
    let base = ECAM_VBASE.load(Ordering::Acquire);
    if base == 0 {
        return None;
    }
    let size = ECAM_SIZE.load(Ordering::Acquire);
    let start = BUS_START.load(Ordering::Acquire) as u8;
    let end = BUS_END.load(Ordering::Acquire) as u8;
    if bus < start || bus > end || device >= 32 || function >= 8 || offset >= 0x1000 {
        return None;
    }
    let rel_bus = (bus - start) as u64;
    let off = (rel_bus << 20) | ((device as u64) << 15) | ((function as u64) << 12) | offset as u64;
    if off >= size {
        return None;
    }
    Some((base + off) as usize)
}

fn ecam_read_u8(_seg: u16, bus: u8, dev: u8, func: u8, offset: u16) -> u8 {
    match ecam_addr(bus, dev, func, offset) {
        Some(a) => unsafe { core::ptr::read_volatile(a as *const u8) },
        None => 0xff,
    }
}
fn ecam_read_u16(_seg: u16, bus: u8, dev: u8, func: u8, offset: u16) -> u16 {
    match ecam_addr(bus, dev, func, offset) {
        Some(a) => unsafe { core::ptr::read_volatile(a as *const u16) },
        None => 0xffff,
    }
}
fn ecam_read_u32(_seg: u16, bus: u8, dev: u8, func: u8, offset: u16) -> u32 {
    match ecam_addr(bus, dev, func, offset) {
        Some(a) => unsafe { core::ptr::read_volatile(a as *const u32) },
        None => 0xffff_ffff,
    }
}
fn ecam_write_u8(_seg: u16, bus: u8, dev: u8, func: u8, offset: u16, v: u8) {
    if let Some(a) = ecam_addr(bus, dev, func, offset) {
        unsafe { core::ptr::write_volatile(a as *mut u8, v) }
    }
}
fn ecam_write_u16(_seg: u16, bus: u8, dev: u8, func: u8, offset: u16, v: u16) {
    if let Some(a) = ecam_addr(bus, dev, func, offset) {
        unsafe { core::ptr::write_volatile(a as *mut u16, v) }
    }
}
fn ecam_write_u32(_seg: u16, bus: u8, dev: u8, func: u8, offset: u16, v: u32) {
    if let Some(a) = ecam_addr(bus, dev, func, offset) {
        unsafe { core::ptr::write_volatile(a as *mut u32, v) }
    }
}

/// 扫描 DTB 根找 `pci-host-ecam-generic`(或 `pcie-host-ecam-generic`),解析
/// 出 ECAM 的物理基址/大小与 bus-range。
///
/// 成功时返回 `Some((phys_base, size, bus_start, bus_end))`;DTB 里没有
/// pcie 节点时返回 `None`。
pub(crate) fn parse_pcie_node(dtb: general::dtb::Dtb<'static>) -> Option<(u64, u64, u8, u8)> {
    let root = dtb.root()?;
    walk_for_pcie(root)
}

fn walk_for_pcie(node: DtbNode<'static>) -> Option<(u64, u64, u8, u8)> {
    if node_compatible_is(node, DTB_COMPAT_PCI_ECAM)
        || node_compatible_is(node, DTB_COMPAT_PCIE_ECAM)
    {
        let reg = node.find_property("reg")?;
        let (base, size) = parse_two_cells_reg(reg.value())?;
        let (bstart, bend) = match node.find_property("bus-range") {
            Some(p) => {
                let v = p.value();
                if v.len() < 8 {
                    (0u8, 0xffu8)
                } else {
                    let s = u32::from_be_bytes([v[0], v[1], v[2], v[3]]) as u8;
                    let e = u32::from_be_bytes([v[4], v[5], v[6], v[7]]) as u8;
                    (s, e)
                }
            }
            None => (0u8, 0xffu8),
        };
        return Some((base, size, bstart, bend));
    }
    for child in node.children() {
        if let Some(r) = walk_for_pcie(child) {
            return Some(r);
        }
    }
    None
}

fn node_compatible_is(node: DtbNode<'static>, needle: &[u8]) -> bool {
    let Some(prop) = node.find_property("compatible") else {
        return false;
    };
    prop.value().split(|&b| b == 0).any(|entry| entry == needle)
}

fn parse_two_cells_reg(reg: &[u8]) -> Option<(u64, u64)> {
    if reg.len() >= 16 {
        let base = u64::from_be_bytes(reg[0..8].try_into().ok()?);
        let size = u64::from_be_bytes(reg[8..16].try_into().ok()?);
        Some((base, size))
    } else if reg.len() >= 8 {
        let base = u32::from_be_bytes(reg[0..4].try_into().ok()?) as u64;
        let size = u32::from_be_bytes(reg[4..8].try_into().ok()?) as u64;
        Some((base, size))
    } else {
        None
    }
}

// ECAM config access 转发的 device_mmio_to_virt —— 装载时由启动上下文传入,
// 用于把 BAR 物理地址转成当前平台可访问的内核虚拟地址。
// 默认 identity(装载前不会被用到,只是为了让类型检查通过)。
static DEVICE_MMIO_TO_VIRT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

fn mmio_to_virt_via_stored(phys: usize) -> usize {
    let f = DEVICE_MMIO_TO_VIRT.load(Ordering::Acquire);
    if f == 0 {
        return phys;
    }
    // Safety: 存入的是合法的 fn 指针(由 install_ecam 只写一次)。
    let f: fn(usize) -> usize = unsafe { core::mem::transmute(f) };
    f(phys)
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
        });
    }
    true
}

// ── BAR 资源分配 ────────────────────────────────────────────────────────
//
// 直接 `-kernel` 引导下没有 UEFI/SeaBIOS,PCI BAR 基址都是 0。本模块用一个
// 简单的 bump allocator 按顺序切片 PCI MMIO 窗口给每个设备的每个 BAR。
//
/// 扫一遍 PCI 总线并给每个 MMIO BAR 分配一段物理地址。必须在
/// [`install_ecam`] 之后、`pci_scan_and_register` 之前调用。
pub(crate) fn assign_bars(bus_start: u8, bus_end: u8) {
    let Some(mmio_window) = hal::platform::default_pci_mmio_window() else {
        log::printk!("[kernel-start][dtb] no fallback PCI MMIO window for this platform");
        return;
    };
    let devices = pci_scan_raw(0, bus_start, bus_end);
    let mut next: u64 = mmio_window.start;
    for d in devices.iter() {
        // bridge header(type 1)的 BAR 只有两个,暂不处理 bridge;
        // 当前 virt 平台只暴露一个 host bridge,bus 0 上都是 endpoint。
        if d.header_type != 0 {
            continue;
        }
        let pnp = general::dev::pnp::PnpDevice::new(
            general::dev::pnp::PnpId::Pci {
                segment: d.segment,
                bus: d.bus,
                device: d.device,
                function: d.function,
            },
            alloc::format!("pci-{:02x}:{:02x}.{}", d.bus, d.device, d.function).into(),
            alloc::boxed::Box::new(general::dev::pci::PciInfo {
                vendor: d.vendor,
                device_id: d.device_id,
                revision: 0,
                class: d.class,
                subclass: 0,
                prog_if: 0,
                subsystem_vendor: 0,
                subsystem_id: 0,
                header_type: d.header_type,
                multi_function: d.multi_function,
            }),
        );
        let pci = match PciDevice::from_pnp(&pnp) {
            Some(p) => p,
            None => continue,
        };
        assign_device_bars(&pci, &mut next, mmio_window.end);
    }
    log::printk!("[kernel-start][dtb] assigned PCI BARs up to {:#x}", next);
}

/// 给一个 PCI 设备的所有 BAR 分配地址。`next` 是 bump allocator 游标。
fn assign_device_bars(pci: &PciDevice, next: &mut u64, mmio_limit: u64) {
    let mut idx: u16 = 0;
    while idx < 6 {
        let offset = 0x10u16 + idx * 4;
        let bar_val = pci.read_config_u32(offset);
        // 判 type
        let is_mmio = bar_val & 0x1 == 0;
        if !is_mmio {
            idx += 1;
            continue;
        }
        let is_64 = (bar_val >> 1) & 0x3 == 2;

        // 用 0xFFFFFFFF 探测 size
        pci.write_config_u32(offset, 0xffff_ffff);
        let lo_size_raw = pci.read_config_u32(offset);
        let (size, hi_offset): (u64, Option<u16>) = if is_64 {
            let hi_offset = offset + 4;
            let hi_bar_val = pci.read_config_u32(hi_offset);
            pci.write_config_u32(hi_offset, 0xffff_ffff);
            let hi_size_raw = pci.read_config_u32(hi_offset);
            let combined = ((hi_size_raw as u64) << 32) | ((lo_size_raw & 0xffff_fff0) as u64);
            let sz = (!combined).wrapping_add(1);
            // 恢复低 / 高 的 BAR 原值(很可能都是 0 但也要正确恢复)
            pci.write_config_u32(offset, bar_val);
            pci.write_config_u32(hi_offset, hi_bar_val);
            (sz, Some(hi_offset))
        } else {
            let masked = lo_size_raw & 0xffff_fff0;
            let sz = (!(masked as u64) as u32).wrapping_add(1) as u64;
            pci.write_config_u32(offset, bar_val);
            (sz, None)
        };

        if size == 0 {
            idx += if is_64 { 2 } else { 1 };
            continue;
        }

        // 对齐分配
        let align = size.max(0x10);
        let addr = (*next + align - 1) & !(align - 1);
        if addr + size > mmio_limit {
            log::printk!(
                "[kernel-start][dtb] PCI BAR pool exhausted (next={:#x})",
                *next
            );
            return;
        }
        *next = addr + size;

        // 写回分配到的基址(保留 type bits:bit 0/2/3)
        let type_bits = bar_val & 0xf;
        let lo_val = (addr as u32 & 0xffff_fff0) | type_bits;
        pci.write_config_u32(offset, lo_val);
        if let Some(hi) = hi_offset {
            pci.write_config_u32(hi, (addr >> 32) as u32);
        }

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

    // 开 bus master + memory decode
    pci.enable_mmio();
    pci.enable_bus_master();
}

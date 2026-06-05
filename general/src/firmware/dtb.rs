//! Platform-neutral DTB firmware parser.
//!
//! The low-level [`crate::dtb`] module exposes raw FDT nodes and properties.
//! This module turns that raw tree into normalized firmware descriptors that
//! kernel startup code can consume without hardcoding paths such as `/soc`.

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::str;

use allocator::MemorySegment;

use crate::dtb::{Dtb, DtbNode};

use super::power::{
    PowerAccessWidth, PowerControlInfo, PowerControlMethod, PowerRegister, PowerRegisterSpace,
};
use super::{SerialPortInfo, normalize_segments};

const COMPAT_SYSCON_POWEROFF: &[u8] = b"syscon-poweroff";
const COMPAT_SYSCON_REBOOT: &[u8] = b"syscon-reboot";
const COMPAT_VIRTIO_MMIO: &[u8] = b"virtio,mmio";
const COMPAT_NS16550: &[u8] = b"ns16550";
const COMPAT_NS16550A: &[u8] = b"ns16550a";
const COMPAT_PCI_ECAM: &[u8] = b"pci-host-ecam-generic";
const COMPAT_PCIE_ECAM: &[u8] = b"pcie-host-ecam-generic";
const COMPAT_SIMPLE_BUS: &[u8] = b"simple-bus";
const COMPAT_SIMPLE_MFD: &[u8] = b"simple-mfd";

pub type NodeId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbMmioDeviceInfo {
    pub name: &'static str,
    pub path: &'static str,
    pub phys_addr: usize,
    pub size: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbPcieHostInfo {
    pub name: &'static str,
    pub path: &'static str,
    pub ecam_phys: usize,
    pub ecam_size: usize,
    pub bus_start: u8,
    pub bus_end: u8,
}

#[derive(Debug)]
pub struct DtbFirmwareInfo {
    pub cpu_count: usize,
    pub memory_segments: Vec<MemorySegment>,
    pub reserved_segments: Vec<MemorySegment>,
    pub external_initramfs_range: Option<(usize, usize)>,
    pub stdout_serial: Option<SerialPortInfo>,
    pub power_controls: PowerControlInfo,
    pub serial_ports: Vec<SerialPortInfo>,
    pub virtio_mmio_devices: Vec<DtbMmioDeviceInfo>,
    pub pcie_hosts: Vec<DtbPcieHostInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbFirmwareError {
    MissingRoot,
    NoUsableMemory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AddressRange {
    start: usize,
    size: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DtbAddressError {
    MissingReg,
    InvalidReg,
    UnsupportedCells,
    UnsupportedBus,
    UnmatchedRange,
    Overflow,
}

struct DtbNodeInfo {
    node: DtbNode<'static>,
    path: &'static str,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    reg_addr_cells: usize,
    reg_size_cells: usize,
    child_addr_cells: usize,
    child_size_cells: usize,
    enabled: bool,
}

#[derive(Clone, Copy)]
struct DtbAlias {
    name: &'static str,
    target: &'static str,
}

#[derive(Clone, Copy)]
struct DtbPhandle {
    value: u32,
    node_id: NodeId,
}

struct DtbTree {
    nodes: Vec<DtbNodeInfo>,
    aliases: Vec<DtbAlias>,
    phandles: Vec<DtbPhandle>,
}

pub fn parse(dtb: Dtb<'static>) -> Result<DtbFirmwareInfo, DtbFirmwareError> {
    let tree = DtbTree::new(dtb)?;

    let cpu_count = tree.cpu_count();
    let stdout_serial = tree.stdout_serial();
    let power_controls = tree.power_controls();
    let external_initramfs_range = tree.external_initramfs_range();

    let raw_memory_segments =
        normalize_segments(tree.memory_segments()).ok_or(DtbFirmwareError::NoUsableMemory)?;
    let reserved_segments = normalize_segments(tree.reserved_segments(dtb)).unwrap_or_default();
    let memory_segments = subtract_reserved_segments(raw_memory_segments, &reserved_segments)
        .ok_or(DtbFirmwareError::NoUsableMemory)?;

    Ok(DtbFirmwareInfo {
        cpu_count,
        memory_segments,
        reserved_segments,
        external_initramfs_range,
        stdout_serial,
        power_controls,
        serial_ports: tree.serial_ports(),
        virtio_mmio_devices: tree.virtio_mmio_devices(),
        pcie_hosts: tree.pcie_hosts(),
    })
}

impl DtbTree {
    fn new(dtb: Dtb<'static>) -> Result<Self, DtbFirmwareError> {
        let root = dtb.root().ok_or(DtbFirmwareError::MissingRoot)?;
        let root_addr_cells = read_cells_count(root, "#address-cells").unwrap_or(2);
        let root_size_cells = read_cells_count(root, "#size-cells").unwrap_or(1);

        let mut tree = Self {
            nodes: Vec::new(),
            aliases: Vec::new(),
            phandles: Vec::new(),
        };

        tree.nodes.push(DtbNodeInfo {
            node: root,
            path: "/",
            parent: None,
            children: Vec::new(),
            reg_addr_cells: 0,
            reg_size_cells: 0,
            child_addr_cells: root_addr_cells,
            child_size_cells: root_size_cells,
            enabled: true,
        });

        let mut stack: Vec<(DtbNode<'static>, NodeId)> = Vec::new();
        let mut root_children: Vec<DtbNode<'static>> = root.children().collect();
        root_children.reverse();
        for child in root_children {
            stack.push((child, 0));
        }

        while let Some((node, parent)) = stack.pop() {
            let parent_path = tree.nodes[parent].path;
            let reg_addr_cells = tree.nodes[parent].child_addr_cells;
            let reg_size_cells = tree.nodes[parent].child_size_cells;
            let name = node.name().unwrap_or("<invalid>");
            let path = if parent_path == "/" {
                leak_str(&format!("/{}", name))
            } else {
                leak_str(&format!("{}/{}", parent_path, name))
            };
            let child_addr_cells =
                read_cells_count(node, "#address-cells").unwrap_or(reg_addr_cells);
            let child_size_cells = read_cells_count(node, "#size-cells").unwrap_or(reg_size_cells);
            let enabled = tree.nodes[parent].enabled && node_enabled(node);
            let node_id = tree.nodes.len();

            tree.nodes[parent].children.push(node_id);
            tree.nodes.push(DtbNodeInfo {
                node,
                path,
                parent: Some(parent),
                children: Vec::new(),
                reg_addr_cells,
                reg_size_cells,
                child_addr_cells,
                child_size_cells,
                enabled,
            });

            let mut children: Vec<DtbNode<'static>> = node.children().collect();
            children.reverse();
            for child in children {
                stack.push((child, node_id));
            }
        }

        tree.build_alias_index();
        tree.build_phandle_index();
        Ok(tree)
    }

    fn build_alias_index(&mut self) {
        for entry in &self.nodes {
            if entry.node.base_name_bytes() != b"aliases" {
                continue;
            }
            for prop in entry.node.properties() {
                let Some(name) = prop.name() else {
                    continue;
                };
                let Some(target) = parse_path_reference(prop.value()) else {
                    continue;
                };
                self.aliases.push(DtbAlias { name, target });
            }
        }
    }

    fn build_phandle_index(&mut self) {
        for (node_id, entry) in self.nodes.iter().enumerate() {
            for name in ["phandle", "linux,phandle"] {
                let Some(value) = entry
                    .node
                    .find_property(name)
                    .and_then(|prop| read_be_u32_prop(prop.value()))
                else {
                    continue;
                };
                if value != 0 && !self.phandles.iter().any(|item| item.value == value) {
                    self.phandles.push(DtbPhandle { value, node_id });
                }
            }
        }
    }

    fn cpu_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|entry| entry.enabled && entry.node.base_name_bytes() == b"cpu")
            .count()
            .max(1)
    }

    fn stdout_serial(&self) -> Option<SerialPortInfo> {
        let chosen = self
            .nodes
            .iter()
            .find(|entry| entry.node.base_name_bytes() == b"chosen")?;
        let stdout_path = chosen.node.find_property("stdout-path")?;
        let node_id = self.resolve_path_or_alias(stdout_path.value())?;
        let entry = &self.nodes[node_id];
        if !entry.enabled || !self.node_is_serial(node_id) {
            return None;
        }
        let range = self.first_reg_range(node_id)?;
        Some(SerialPortInfo {
            name: self.node_name_or_path(node_id),
            phys_addr: range.start,
            clock_hz: read_clock_hz(entry.node),
        })
    }

    fn power_controls(&self) -> PowerControlInfo {
        PowerControlInfo {
            shutdown: self.syscon_power_action(COMPAT_SYSCON_POWEROFF),
            reboot: self.syscon_power_action(COMPAT_SYSCON_REBOOT),
        }
    }

    fn syscon_power_action(&self, compatible: &[u8]) -> Option<PowerControlMethod> {
        let action_id = self
            .nodes
            .iter()
            .position(|entry| entry.enabled && compatible_contains(entry.node, compatible))?;
        let action_node = self.nodes[action_id].node;
        let regmap_phandle = read_be_u32_prop(action_node.find_property("regmap")?.value())?;
        let offset = read_usize_scalar(action_node.find_property("offset")?.value())?;
        let value = read_u64_scalar(action_node.find_property("value")?.value())?;
        let regmap_id = self.lookup_phandle(regmap_phandle)?;
        if !self.nodes[regmap_id].enabled {
            return None;
        }

        let regmap_node = self.nodes[regmap_id].node;
        let range = self.first_reg_range(regmap_id)?;
        let width_bytes = regmap_node
            .find_property("reg-io-width")
            .and_then(|prop| read_usize_scalar(prop.value()))
            .or_else(|| {
                if range.size != 0 && offset < range.size && range.size - offset < 4 {
                    Some(1)
                } else {
                    Some(4)
                }
            })?;
        let access_width = PowerAccessWidth::from_bytes(width_bytes)?;
        let address = range.start.checked_add(offset)?;

        Some(PowerControlMethod::RegisterWrite {
            register: PowerRegister {
                space: PowerRegisterSpace::SystemMemory,
                address,
                access_width,
            },
            value,
        })
    }

    fn external_initramfs_range(&self) -> Option<(usize, usize)> {
        let chosen = self
            .nodes
            .iter()
            .find(|entry| entry.node.base_name_bytes() == b"chosen")?;
        let start = read_usize_scalar(chosen.node.find_property("linux,initrd-start")?.value())?;
        let end = read_usize_scalar(chosen.node.find_property("linux,initrd-end")?.value())?;
        (end > start).then_some((start, end))
    }

    fn memory_segments(&self) -> Vec<MemorySegment> {
        let mut segments = Vec::new();
        for node_id in 0..self.nodes.len() {
            let entry = &self.nodes[node_id];
            if !entry.enabled || !self.node_is_memory(node_id) {
                continue;
            }
            if let Ok(ranges) = self.reg_ranges(node_id) {
                for range in ranges {
                    segments.push(MemorySegment {
                        start: range.start,
                        size: range.size,
                    });
                }
            }
        }
        segments
    }

    fn reserved_segments(&self, dtb: Dtb<'static>) -> Vec<MemorySegment> {
        let mut reserved = Vec::new();
        if let Some(entries) = dtb.mem_reservations() {
            for entry in entries {
                if entry.size != 0 {
                    reserved.push(MemorySegment {
                        start: entry.address,
                        size: entry.size,
                    });
                }
            }
        }

        for node_id in 0..self.nodes.len() {
            let Some(parent_id) = self.nodes[node_id].parent else {
                continue;
            };
            if !self.nodes[node_id].enabled {
                continue;
            }
            if self.nodes[parent_id].node.base_name_bytes() != b"reserved-memory" {
                continue;
            }
            if let Ok(ranges) = self.reg_ranges(node_id) {
                for range in ranges {
                    reserved.push(MemorySegment {
                        start: range.start,
                        size: range.size,
                    });
                }
            }
        }
        reserved
    }

    fn serial_ports(&self) -> Vec<SerialPortInfo> {
        let mut ports = Vec::new();
        for node_id in 0..self.nodes.len() {
            if !self.nodes[node_id].enabled || !self.node_is_serial(node_id) {
                continue;
            }
            let Some(range) = self.first_reg_range(node_id) else {
                continue;
            };
            if ports
                .iter()
                .any(|port: &SerialPortInfo| port.phys_addr == range.start)
            {
                continue;
            }
            ports.push(SerialPortInfo {
                name: self.node_name_or_path(node_id),
                phys_addr: range.start,
                clock_hz: read_clock_hz(self.nodes[node_id].node),
            });
        }
        ports
    }

    fn virtio_mmio_devices(&self) -> Vec<DtbMmioDeviceInfo> {
        let mut devices = Vec::new();
        for node_id in 0..self.nodes.len() {
            let entry = &self.nodes[node_id];
            if !entry.enabled || !compatible_contains(entry.node, COMPAT_VIRTIO_MMIO) {
                continue;
            }
            let Some(range) = self.first_reg_range(node_id) else {
                continue;
            };
            if devices
                .iter()
                .any(|dev: &DtbMmioDeviceInfo| dev.phys_addr == range.start)
            {
                continue;
            }
            devices.push(DtbMmioDeviceInfo {
                name: self.node_name_or_path(node_id),
                path: entry.path,
                phys_addr: range.start,
                size: range.size,
            });
        }
        devices
    }

    fn pcie_hosts(&self) -> Vec<DtbPcieHostInfo> {
        let mut hosts = Vec::new();
        for node_id in 0..self.nodes.len() {
            let entry = &self.nodes[node_id];
            if !entry.enabled || !self.node_is_pcie_host(node_id) {
                continue;
            }
            let Some(range) = self.first_reg_range(node_id) else {
                continue;
            };
            let (bus_start, bus_end) = read_bus_range(entry.node).unwrap_or((0, 0xff));
            hosts.push(DtbPcieHostInfo {
                name: self.node_name_or_path(node_id),
                path: entry.path,
                ecam_phys: range.start,
                ecam_size: range.size,
                bus_start,
                bus_end,
            });
        }
        hosts
    }

    fn first_reg_range(&self, node_id: NodeId) -> Option<AddressRange> {
        self.reg_ranges(node_id).ok()?.into_iter().next()
    }

    fn reg_ranges(&self, node_id: NodeId) -> Result<Vec<AddressRange>, DtbAddressError> {
        let entry = self.nodes.get(node_id).ok_or(DtbAddressError::InvalidReg)?;
        let reg = entry
            .node
            .find_property("reg")
            .ok_or(DtbAddressError::MissingReg)?
            .value();
        let entry_cells = entry
            .reg_addr_cells
            .checked_add(entry.reg_size_cells)
            .ok_or(DtbAddressError::Overflow)?;
        let entry_bytes = entry_cells
            .checked_mul(4)
            .ok_or(DtbAddressError::Overflow)?;
        if entry.reg_addr_cells == 0
            || entry.reg_size_cells == 0
            || entry_bytes == 0
            || !reg.len().is_multiple_of(entry_bytes)
        {
            return Err(DtbAddressError::InvalidReg);
        }

        let mut ranges = Vec::new();
        for chunk in reg.chunks_exact(entry_bytes) {
            let addr_bytes = entry
                .reg_addr_cells
                .checked_mul(4)
                .ok_or(DtbAddressError::Overflow)?;
            let addr = read_cells_u128(&chunk[..addr_bytes], entry.reg_addr_cells)?;
            let size = read_cells_u128(&chunk[addr_bytes..], entry.reg_size_cells)?;
            if size == 0 {
                continue;
            }
            ranges.push(self.translate_reg_address(node_id, addr, size)?);
        }

        (!ranges.is_empty())
            .then_some(ranges)
            .ok_or(DtbAddressError::InvalidReg)
    }

    fn translate_reg_address(
        &self,
        node_id: NodeId,
        mut start: u128,
        size: u128,
    ) -> Result<AddressRange, DtbAddressError> {
        let mut current_parent = self.nodes[node_id].parent;
        while let Some(bus_id) = current_parent {
            let bus = &self.nodes[bus_id];
            if bus.parent.is_none() {
                break;
            }
            if self.node_is_pcie_host(bus_id) {
                return Err(DtbAddressError::UnsupportedBus);
            }

            if let Some(ranges_prop) = bus.node.find_property("ranges") {
                let ranges = ranges_prop.value();
                if !ranges.is_empty() {
                    start = self.translate_through_ranges(bus_id, start, size, ranges)?;
                }
            } else if !self.bus_allows_implicit_identity(bus_id) {
                return Err(DtbAddressError::UnsupportedBus);
            }

            current_parent = bus.parent;
        }

        let start = u128_to_usize(start)?;
        let size = u128_to_usize(size)?;
        start.checked_add(size).ok_or(DtbAddressError::Overflow)?;
        Ok(AddressRange { start, size })
    }

    fn translate_through_ranges(
        &self,
        bus_id: NodeId,
        start: u128,
        size: u128,
        ranges: &[u8],
    ) -> Result<u128, DtbAddressError> {
        let bus = &self.nodes[bus_id];
        let range_cells = bus
            .child_addr_cells
            .checked_add(bus.reg_addr_cells)
            .and_then(|cells| cells.checked_add(bus.child_size_cells))
            .ok_or(DtbAddressError::Overflow)?;
        let range_bytes = range_cells
            .checked_mul(4)
            .ok_or(DtbAddressError::Overflow)?;
        if bus.child_addr_cells == 0
            || bus.reg_addr_cells == 0
            || bus.child_size_cells == 0
            || range_bytes == 0
            || !ranges.len().is_multiple_of(range_bytes)
        {
            return Err(DtbAddressError::InvalidReg);
        }

        for chunk in ranges.chunks_exact(range_bytes) {
            let child_bytes = bus
                .child_addr_cells
                .checked_mul(4)
                .ok_or(DtbAddressError::Overflow)?;
            let parent_bytes = bus
                .reg_addr_cells
                .checked_mul(4)
                .ok_or(DtbAddressError::Overflow)?;
            let child_end = child_bytes;
            let parent_end = child_end + parent_bytes;
            let child_base = read_cells_u128(&chunk[..child_end], bus.child_addr_cells)?;
            let parent_base = read_cells_u128(&chunk[child_end..parent_end], bus.reg_addr_cells)?;
            let range_size = read_cells_u128(&chunk[parent_end..], bus.child_size_cells)?;
            let Some(offset) = start.checked_sub(child_base) else {
                continue;
            };
            if offset
                .checked_add(size)
                .is_some_and(|end| end <= range_size)
            {
                return parent_base
                    .checked_add(offset)
                    .ok_or(DtbAddressError::Overflow);
            }
        }

        Err(DtbAddressError::UnmatchedRange)
    }

    fn bus_allows_implicit_identity(&self, bus_id: NodeId) -> bool {
        let bus = &self.nodes[bus_id];
        bus.node.base_name_bytes() == b"reserved-memory"
            || compatible_contains(bus.node, COMPAT_SIMPLE_BUS)
            || compatible_contains(bus.node, COMPAT_SIMPLE_MFD)
    }

    fn node_is_memory(&self, node_id: NodeId) -> bool {
        let node = self.nodes[node_id].node;
        node.base_name_bytes() == b"memory"
            || property_first_string_eq(node, "device_type", "memory")
    }

    fn node_is_serial(&self, node_id: NodeId) -> bool {
        let node = self.nodes[node_id].node;
        node.base_name_bytes() == b"serial"
            || compatible_contains(node, COMPAT_NS16550)
            || compatible_contains(node, COMPAT_NS16550A)
    }

    fn node_is_pcie_host(&self, node_id: NodeId) -> bool {
        let node = self.nodes[node_id].node;
        compatible_contains(node, COMPAT_PCI_ECAM) || compatible_contains(node, COMPAT_PCIE_ECAM)
    }

    fn node_name_or_path(&self, node_id: NodeId) -> &'static str {
        self.nodes[node_id]
            .node
            .name()
            .unwrap_or(self.nodes[node_id].path)
    }

    fn resolve_path_or_alias(&self, value: &'static [u8]) -> Option<NodeId> {
        let name = parse_path_reference(value)?;
        if name.starts_with('/') {
            return self.find_node_by_path(name);
        }
        if let Some(alias) = self.aliases.iter().find(|alias| alias.name == name) {
            return self.find_node_by_path(alias.target);
        }
        self.find_node_by_path(name)
    }

    fn find_node_by_path(&self, path: &str) -> Option<NodeId> {
        if path == "/" {
            return Some(0);
        }
        let normalized = path.trim_start_matches('/');
        self.nodes
            .iter()
            .position(|entry| entry.path.trim_start_matches('/') == normalized)
    }

    fn lookup_phandle(&self, value: u32) -> Option<NodeId> {
        self.phandles
            .iter()
            .find(|entry| entry.value == value)
            .map(|entry| entry.node_id)
    }
}

fn subtract_reserved_segments(
    segments: Vec<MemorySegment>,
    reserved: &[MemorySegment],
) -> Option<Vec<MemorySegment>> {
    let segments = normalize_segments(segments)?;
    if reserved.is_empty() {
        return Some(segments);
    }

    let mut result = Vec::new();
    for segment in segments {
        let mut cursor = segment.start;
        let segment_end = segment.end();

        for hole in reserved {
            let hole_start = hole.start;
            let hole_end = hole.end();
            if hole_end <= cursor {
                continue;
            }
            if hole_start >= segment_end {
                break;
            }
            if cursor < hole_start {
                result.push(MemorySegment {
                    start: cursor,
                    size: hole_start - cursor,
                });
            }
            cursor = cursor.max(hole_end.min(segment_end));
            if cursor >= segment_end {
                break;
            }
        }

        if cursor < segment_end {
            result.push(MemorySegment {
                start: cursor,
                size: segment_end - cursor,
            });
        }
    }

    normalize_segments(result)
}

fn compatible_contains(node: DtbNode<'static>, compatible: &[u8]) -> bool {
    let Some(prop) = node.find_property("compatible") else {
        return false;
    };
    prop.value()
        .split(|&byte| byte == 0)
        .any(|entry| entry == compatible)
}

fn property_first_string_eq(node: DtbNode<'static>, name: &str, expected: &str) -> bool {
    node.find_property(name)
        .and_then(|prop| parse_dtb_string(prop.value()))
        == Some(expected)
}

fn node_enabled(node: DtbNode<'static>) -> bool {
    match node
        .find_property("status")
        .and_then(|prop| parse_dtb_string(prop.value()))
    {
        None | Some("okay") | Some("ok") => true,
        Some(_) => false,
    }
}

fn parse_path_reference(value: &'static [u8]) -> Option<&'static str> {
    let path = parse_dtb_string(value)?;
    Some(path.split(':').next().unwrap_or(path))
}

fn parse_dtb_string(value: &'static [u8]) -> Option<&'static str> {
    let end = value
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(value.len());
    str::from_utf8(value.get(..end)?).ok()
}

fn read_clock_hz(node: DtbNode<'static>) -> Option<u32> {
    read_be_u32_prop(node.find_property("clock-frequency")?.value())
}

fn read_bus_range(node: DtbNode<'static>) -> Option<(u8, u8)> {
    let value = node.find_property("bus-range")?.value();
    let start = read_be_u32_prop(value.get(..4)?)?;
    let end = read_be_u32_prop(value.get(4..8)?)?;
    if start <= u8::MAX as u32 && end <= u8::MAX as u32 && start <= end {
        Some((start as u8, end as u8))
    } else {
        None
    }
}

fn read_cells_count(node: DtbNode<'static>, name: &str) -> Option<usize> {
    node.find_property(name)
        .and_then(|prop| read_be_u32_prop(prop.value()))
        .map(|value| value as usize)
}

fn read_be_u32_prop(value: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(value.get(..4)?.try_into().ok()?))
}

fn read_usize_scalar(value: &[u8]) -> Option<usize> {
    let raw = if value.len() >= 8 {
        u64::from_be_bytes(value.get(..8)?.try_into().ok()?)
    } else {
        read_be_u32_prop(value)? as u64
    };
    (raw <= usize::MAX as u64).then_some(raw as usize)
}

fn read_u64_scalar(value: &[u8]) -> Option<u64> {
    if value.len() >= 8 {
        Some(u64::from_be_bytes(value.get(..8)?.try_into().ok()?))
    } else {
        read_be_u32_prop(value).map(|value| value as u64)
    }
}

fn read_cells_u128(bytes: &[u8], cells: usize) -> Result<u128, DtbAddressError> {
    if cells > 4 {
        return Err(DtbAddressError::UnsupportedCells);
    }
    let expected = cells.checked_mul(4).ok_or(DtbAddressError::Overflow)?;
    if bytes.len() != expected {
        return Err(DtbAddressError::InvalidReg);
    }

    let mut value = 0u128;
    for chunk in bytes.chunks_exact(4) {
        let cell = u32::from_be_bytes(chunk.try_into().map_err(|_| DtbAddressError::InvalidReg)?);
        value = (value << 32) | cell as u128;
    }
    Ok(value)
}

fn u128_to_usize(value: u128) -> Result<usize, DtbAddressError> {
    if value <= usize::MAX as u128 {
        Ok(value as usize)
    } else {
        Err(DtbAddressError::Overflow)
    }
}

fn leak_str(value: &str) -> &'static str {
    let boxed: Box<str> = value.into();
    Box::leak(boxed)
}

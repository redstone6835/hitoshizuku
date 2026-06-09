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
const COMPAT_NS16550: &[u8] = b"ns16550";
const COMPAT_NS16550A: &[u8] = b"ns16550a";
const COMPAT_PCI_ECAM: &[u8] = b"pci-host-ecam-generic";
const COMPAT_PCIE_ECAM: &[u8] = b"pcie-host-ecam-generic";
const COMPAT_SIMPLE_BUS: &[u8] = b"simple-bus";
const COMPAT_SIMPLE_MFD: &[u8] = b"simple-mfd";

pub type NodeId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbMmioRangeInfo {
    pub phys_addr: usize,
    pub size: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbInterruptInfo {
    pub parent: Option<u32>,
    pub specifier: Box<[u32]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DtbPropertyValue {
    Bool,
    U32(u32),
    U32List(Box<[u32]>),
    StringList(Vec<&'static str>),
    Bytes(Box<[u8]>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbDeviceProperty {
    pub name: &'static str,
    pub value: DtbPropertyValue,
}

#[derive(Debug)]
pub struct DtbPlatformDeviceInfo {
    pub name: &'static str,
    pub path: &'static str,
    pub parent_path: Option<&'static str>,
    pub phandle: Option<u32>,
    pub interrupt_parent: Option<u32>,
    pub address_cells: usize,
    pub size_cells: usize,
    pub parent_address_cells: usize,
    pub parent_size_cells: usize,
    pub compatible: Vec<&'static str>,
    pub reg_ranges: Vec<DtbMmioRangeInfo>,
    pub interrupts: Vec<DtbInterruptInfo>,
    pub interrupt_controller: bool,
    pub clock_hz: Option<u32>,
    pub properties: Vec<DtbDeviceProperty>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbPcieHostInfo {
    pub name: &'static str,
    pub path: &'static str,
    pub ecam_phys: usize,
    pub ecam_size: usize,
    pub domain: u16,
    pub bus_start: u8,
    pub bus_end: u8,
    pub dma_coherent: bool,
    pub address_cells: usize,
    pub interrupt_cells: usize,
    pub ranges: Vec<DtbPciRangeInfo>,
    pub interrupt_map_mask: Option<Box<[u32]>>,
    pub interrupt_map: Vec<DtbPciInterruptMapEntry>,
    pub msi_map: Vec<DtbPciMsiMapEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbPciRangeInfo {
    pub space: DtbPciAddressSpace,
    pub child_addr: u64,
    pub parent_addr: usize,
    pub size: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtbPciAddressSpace {
    Io,
    Memory,
    PrefetchableMemory,
    Unknown(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbPciMsiMapEntry {
    pub requester_base: u32,
    pub controller: u32,
    pub msi_base: u32,
    pub length: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbPciInterruptMapEntry {
    pub child_address: Box<[u32]>,
    pub child_interrupt: Box<[u32]>,
    pub parent: u32,
    pub parent_address: Box<[u32]>,
    pub parent_specifier: Box<[u32]>,
}

#[derive(Debug)]
pub struct DtbFirmwareInfo {
    pub root_compatible: Vec<&'static str>,
    pub cpu_count: usize,
    pub cpus: Vec<DtbCpuInfo>,
    pub memory_segments: Vec<MemorySegment>,
    pub reserved_segments: Vec<MemorySegment>,
    pub external_initramfs_range: Option<(usize, usize)>,
    pub rng_seed: Option<Box<[u8]>>,
    pub stdout_serial: Option<SerialPortInfo>,
    pub power_controls: PowerControlInfo,
    pub serial_ports: Vec<SerialPortInfo>,
    pub platform_devices: Vec<DtbPlatformDeviceInfo>,
    pub pcie_hosts: Vec<DtbPcieHostInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbCpuInfo {
    pub logical_id: u32,
    pub reg: u64,
    pub phandle: Option<u32>,
    pub compatible: Vec<&'static str>,
    pub socket_id: Option<u32>,
    pub core_id: Option<u32>,
    pub thread_id: Option<u32>,
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

#[derive(Clone, Copy)]
struct DtbCpuMapEntry {
    cpu: u32,
    socket_id: Option<u32>,
    core_id: Option<u32>,
    thread_id: Option<u32>,
}

struct DtbTree {
    nodes: Vec<DtbNodeInfo>,
    aliases: Vec<DtbAlias>,
    phandles: Vec<DtbPhandle>,
}

pub fn parse(dtb: Dtb<'static>) -> Result<DtbFirmwareInfo, DtbFirmwareError> {
    let tree = DtbTree::new(dtb)?;

    let root_compatible = tree.root_compatible();
    let cpu_count = tree.cpu_count();
    let cpus = tree.cpus();
    let stdout_serial = tree.stdout_serial();
    let power_controls = tree.power_controls();
    let external_initramfs_range = tree.external_initramfs_range();
    let rng_seed = tree.rng_seed();

    let raw_memory_segments =
        normalize_segments(tree.memory_segments()).ok_or(DtbFirmwareError::NoUsableMemory)?;
    let reserved_segments = normalize_segments(tree.reserved_segments(dtb)).unwrap_or_default();
    let memory_segments = subtract_reserved_segments(raw_memory_segments, &reserved_segments)
        .ok_or(DtbFirmwareError::NoUsableMemory)?;

    Ok(DtbFirmwareInfo {
        root_compatible,
        cpu_count,
        cpus,
        memory_segments,
        reserved_segments,
        external_initramfs_range,
        rng_seed,
        stdout_serial,
        power_controls,
        serial_ports: tree.serial_ports(),
        platform_devices: tree.platform_devices(),
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

    fn root_compatible(&self) -> Vec<&'static str> {
        compatible_strings(self.nodes[0].node)
    }

    fn cpu_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|entry| entry.enabled && entry.node.base_name_bytes() == b"cpu")
            .count()
            .max(1)
    }

    fn cpus(&self) -> Vec<DtbCpuInfo> {
        let topology = self.cpu_map_entries();
        let mut cpus = Vec::new();
        for node_id in 0..self.nodes.len() {
            let entry = &self.nodes[node_id];
            if !entry.enabled || !self.node_is_cpu(node_id) {
                continue;
            }
            let phandle = self.phandle_for_node(node_id);
            let topology = phandle
                .and_then(|phandle| topology.iter().find(|entry| entry.cpu == phandle).copied());
            let logical_id = u32::try_from(cpus.len()).unwrap_or(u32::MAX);
            cpus.push(DtbCpuInfo {
                logical_id,
                reg: self.read_cpu_reg(node_id).unwrap_or(u64::from(logical_id)),
                phandle,
                compatible: compatible_strings(entry.node),
                socket_id: topology.and_then(|entry| entry.socket_id),
                core_id: topology.and_then(|entry| entry.core_id),
                thread_id: topology.and_then(|entry| entry.thread_id),
            });
        }
        cpus
    }

    fn cpu_map_entries(&self) -> Vec<DtbCpuMapEntry> {
        let Some(root_id) = self
            .nodes
            .iter()
            .position(|entry| entry.enabled && entry.node.base_name_bytes() == b"cpu-map")
        else {
            return Vec::new();
        };

        let mut entries = Vec::new();
        let mut stack = Vec::new();
        stack.push((root_id, None, None, None));
        while let Some((node_id, socket_id, core_id, thread_id)) = stack.pop() {
            let node = self.nodes[node_id].node;
            let name = node.name().unwrap_or("");
            let socket_id = indexed_name_suffix(name, "socket").or(socket_id);
            let core_id = indexed_name_suffix(name, "core").or(core_id);
            let thread_id = indexed_name_suffix(name, "thread").or(thread_id);

            if let Some(cpu) = node
                .find_property("cpu")
                .and_then(|prop| read_be_u32_prop(prop.value()))
            {
                entries.push(DtbCpuMapEntry {
                    cpu,
                    socket_id,
                    core_id,
                    thread_id,
                });
            }

            for child in self.nodes[node_id].children.iter().rev() {
                if self.nodes[*child].enabled {
                    stack.push((*child, socket_id, core_id, thread_id));
                }
            }
        }
        entries
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
            reg_size: Some(range.size),
            clock_hz: read_clock_hz(entry.node),
            baud: read_current_speed(entry.node),
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

    fn rng_seed(&self) -> Option<Box<[u8]>> {
        let chosen = self
            .nodes
            .iter()
            .find(|entry| entry.node.base_name_bytes() == b"chosen")?;
        let seed = chosen.node.find_property("rng-seed")?.value();
        (!seed.is_empty()).then(|| seed.to_vec().into_boxed_slice())
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
                reg_size: Some(range.size),
                clock_hz: read_clock_hz(self.nodes[node_id].node),
                baud: read_current_speed(self.nodes[node_id].node),
            });
        }
        ports
    }

    fn platform_devices(&self) -> Vec<DtbPlatformDeviceInfo> {
        let mut devices = Vec::new();
        for node_id in 0..self.nodes.len() {
            let entry = &self.nodes[node_id];
            if !entry.enabled || node_id == 0 {
                continue;
            }
            let compatible = compatible_strings(entry.node);
            if compatible.is_empty() {
                continue;
            };
            let interrupt_controller = self.node_is_interrupt_controller(node_id);
            let ranges = match self.reg_ranges(node_id) {
                Ok(ranges) => ranges,
                // 有 compatible 的无寄存器节点仍是固件描述的一部分，例如
                // simple-bus、syscon-poweroff/reboot 这类功能节点。它们不提供
                // MMIO resource，但应进入 platform PnP，让总线/电源/诊断接口
                // 能看到完整固件拓扑。
                Err(DtbAddressError::MissingReg) => Vec::new(),
                Err(DtbAddressError::InvalidReg) if interrupt_controller => Vec::new(),
                Err(_) => continue,
            };
            let reg_ranges = ranges
                .into_iter()
                .map(|range| DtbMmioRangeInfo {
                    phys_addr: range.start,
                    size: range.size,
                })
                .collect();
            devices.push(DtbPlatformDeviceInfo {
                name: self.node_name_or_path(node_id),
                path: entry.path,
                parent_path: entry.parent.map(|parent| self.nodes[parent].path),
                phandle: self.phandle_for_node(node_id),
                interrupt_parent: self.interrupt_parent_phandle(node_id),
                address_cells: entry.child_addr_cells,
                size_cells: entry.child_size_cells,
                parent_address_cells: entry.reg_addr_cells,
                parent_size_cells: entry.reg_size_cells,
                compatible,
                reg_ranges,
                interrupts: self.interrupts(node_id),
                interrupt_controller,
                clock_hz: read_clock_hz(entry.node),
                properties: scalar_properties(entry.node),
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
            let domain = read_pci_domain(entry.node).unwrap_or(0);
            let address_cells = entry.child_addr_cells;
            let interrupt_cells = read_cells_count(entry.node, "#interrupt-cells").unwrap_or(1);
            let ranges = self.pci_ranges(node_id);
            let interrupt_map_mask =
                self.pci_interrupt_map_mask(node_id, address_cells, interrupt_cells);
            let interrupt_map = self.pci_interrupt_map(node_id, address_cells, interrupt_cells);
            let msi_map = self.pci_msi_map(node_id);
            hosts.push(DtbPcieHostInfo {
                name: self.node_name_or_path(node_id),
                path: entry.path,
                ecam_phys: range.start,
                ecam_size: range.size,
                domain,
                bus_start,
                bus_end,
                dma_coherent: entry.node.find_property("dma-coherent").is_some(),
                address_cells,
                interrupt_cells,
                ranges,
                interrupt_map_mask,
                interrupt_map,
                msi_map,
            });
        }
        hosts
    }

    fn pci_ranges(&self, node_id: NodeId) -> Vec<DtbPciRangeInfo> {
        let entry = &self.nodes[node_id];
        let Some(value) = entry.node.find_property("ranges").map(|prop| prop.value()) else {
            return Vec::new();
        };
        let range_cells = entry
            .child_addr_cells
            .checked_add(entry.reg_addr_cells)
            .and_then(|cells| cells.checked_add(entry.reg_size_cells));
        let Some(range_cells) = range_cells else {
            return Vec::new();
        };
        let Some(range_bytes) = range_cells.checked_mul(4) else {
            return Vec::new();
        };
        if entry.child_addr_cells < 3
            || entry.reg_addr_cells == 0
            || entry.reg_size_cells == 0
            || range_bytes == 0
            || !value.len().is_multiple_of(range_bytes)
        {
            return Vec::new();
        }

        let mut ranges = Vec::new();
        for chunk in value.chunks_exact(range_bytes) {
            let child_bytes = entry.child_addr_cells * 4;
            let parent_bytes = entry.reg_addr_cells * 4;
            let parent_start = child_bytes;
            let size_start = child_bytes + parent_bytes;
            let Some(child_cells) =
                read_fixed_u32_cells(&chunk[..child_bytes], entry.child_addr_cells)
            else {
                continue;
            };
            let Some(space) = pci_address_space(child_cells[0]) else {
                continue;
            };
            let Some(child_addr) = pci_child_range_address(&child_cells) else {
                continue;
            };
            let Ok(parent_raw) =
                read_cells_u128(&chunk[parent_start..size_start], entry.reg_addr_cells)
            else {
                continue;
            };
            let Ok(size_raw) = read_cells_u128(&chunk[size_start..], entry.reg_size_cells) else {
                continue;
            };
            let Ok(parent_addr) = u128_to_usize(parent_raw) else {
                continue;
            };
            let Ok(size) = u128_to_usize(size_raw) else {
                continue;
            };
            if size == 0 || parent_addr.checked_add(size).is_none() {
                continue;
            }
            ranges.push(DtbPciRangeInfo {
                space,
                child_addr,
                parent_addr,
                size,
            });
        }
        ranges
    }

    fn pci_msi_map(&self, node_id: NodeId) -> Vec<DtbPciMsiMapEntry> {
        let Some(value) = self.nodes[node_id]
            .node
            .find_property("msi-map")
            .map(|prop| prop.value())
        else {
            return Vec::new();
        };
        let Some(cells) = read_u32_cells(value) else {
            return Vec::new();
        };
        if !cells.len().is_multiple_of(4) {
            return Vec::new();
        }
        let mut entries = Vec::new();
        for chunk in cells.chunks_exact(4) {
            let [requester_base, controller, msi_base, length] = chunk else {
                continue;
            };
            if *length == 0 || self.lookup_phandle(*controller).is_none() {
                continue;
            }
            entries.push(DtbPciMsiMapEntry {
                requester_base: *requester_base,
                controller: *controller,
                msi_base: *msi_base,
                length: *length,
            });
        }
        entries
    }

    fn pci_interrupt_map_mask(
        &self,
        node_id: NodeId,
        address_cells: usize,
        interrupt_cells: usize,
    ) -> Option<Box<[u32]>> {
        let expected = address_cells.checked_add(interrupt_cells)?;
        let value = self.nodes[node_id]
            .node
            .find_property("interrupt-map-mask")?
            .value();
        let cells = read_u32_cells(value)?;
        (cells.len() == expected).then_some(cells)
    }

    fn pci_interrupt_map(
        &self,
        node_id: NodeId,
        address_cells: usize,
        interrupt_cells: usize,
    ) -> Vec<DtbPciInterruptMapEntry> {
        let Some(value) = self.nodes[node_id]
            .node
            .find_property("interrupt-map")
            .map(|prop| prop.value())
        else {
            return Vec::new();
        };
        let Some(child_address_bytes) = address_cells.checked_mul(4) else {
            return Vec::new();
        };
        let Some(child_interrupt_bytes) = interrupt_cells.checked_mul(4) else {
            return Vec::new();
        };
        if address_cells == 0 || interrupt_cells == 0 {
            return Vec::new();
        }

        let mut entries = Vec::new();
        let mut offset = 0usize;
        while offset < value.len() {
            let Some(child_address_raw) =
                value.get(offset..offset.saturating_add(child_address_bytes))
            else {
                break;
            };
            let Some(child_address) = read_fixed_u32_cells(child_address_raw, address_cells) else {
                break;
            };
            offset += child_address_bytes;

            let Some(child_interrupt_raw) =
                value.get(offset..offset.saturating_add(child_interrupt_bytes))
            else {
                break;
            };
            let Some(child_interrupt) = read_fixed_u32_cells(child_interrupt_raw, interrupt_cells)
            else {
                break;
            };
            offset += child_interrupt_bytes;

            let Some(parent) = value
                .get(offset..offset.saturating_add(4))
                .and_then(read_be_u32_prop)
            else {
                break;
            };
            offset += 4;

            let Some(parent_id) = self.lookup_phandle(parent) else {
                break;
            };
            let parent_node = self.nodes[parent_id].node;
            let parent_address_cells = read_cells_count(parent_node, "#address-cells").unwrap_or(0);
            let parent_interrupt_cells =
                read_cells_count(parent_node, "#interrupt-cells").unwrap_or(1);
            let Some(parent_address_bytes) = parent_address_cells.checked_mul(4) else {
                break;
            };
            let Some(parent_interrupt_bytes) = parent_interrupt_cells.checked_mul(4) else {
                break;
            };

            let Some(parent_address_raw) =
                value.get(offset..offset.saturating_add(parent_address_bytes))
            else {
                break;
            };
            let Some(parent_address) =
                read_fixed_u32_cells(parent_address_raw, parent_address_cells)
            else {
                break;
            };
            offset += parent_address_bytes;

            let Some(parent_specifier_raw) =
                value.get(offset..offset.saturating_add(parent_interrupt_bytes))
            else {
                break;
            };
            let Some(parent_specifier) =
                read_fixed_u32_cells(parent_specifier_raw, parent_interrupt_cells)
            else {
                break;
            };
            offset += parent_interrupt_bytes;

            entries.push(DtbPciInterruptMapEntry {
                child_address,
                child_interrupt,
                parent,
                parent_address,
                parent_specifier,
            });
        }
        entries
    }

    fn first_reg_range(&self, node_id: NodeId) -> Option<AddressRange> {
        self.reg_ranges(node_id).ok()?.into_iter().next()
    }

    fn interrupts(&self, node_id: NodeId) -> Vec<DtbInterruptInfo> {
        let extended = self.interrupts_extended(node_id);
        if !extended.is_empty() {
            return extended;
        }
        self.interrupts_inherited(node_id)
    }

    fn interrupts_extended(&self, node_id: NodeId) -> Vec<DtbInterruptInfo> {
        let Some(value) = self.nodes[node_id]
            .node
            .find_property("interrupts-extended")
            .map(|prop| prop.value())
        else {
            return Vec::new();
        };

        let mut interrupts = Vec::new();
        let mut offset = 0usize;
        while offset < value.len() {
            let Some(parent) = value
                .get(offset..offset.saturating_add(4))
                .and_then(read_be_u32_prop)
            else {
                break;
            };
            offset += 4;
            let Some(parent_id) = self.lookup_phandle(parent) else {
                break;
            };
            let cell_count =
                read_cells_count(self.nodes[parent_id].node, "#interrupt-cells").unwrap_or(1);
            let Some(byte_count) = cell_count.checked_mul(4) else {
                break;
            };
            if byte_count == 0 {
                break;
            }
            let Some(specifier_bytes) = value.get(offset..offset.saturating_add(byte_count)) else {
                break;
            };
            let Some(specifier) = read_u32_cells(specifier_bytes) else {
                break;
            };
            interrupts.push(DtbInterruptInfo {
                parent: Some(parent),
                specifier,
            });
            offset += byte_count;
        }
        interrupts
    }

    fn interrupts_inherited(&self, node_id: NodeId) -> Vec<DtbInterruptInfo> {
        let Some(value) = self.nodes[node_id]
            .node
            .find_property("interrupts")
            .map(|prop| prop.value())
        else {
            return Vec::new();
        };
        let parent = self.interrupt_parent_phandle(node_id);
        let cell_count = parent
            .and_then(|phandle| self.lookup_phandle(phandle))
            .and_then(|parent_id| read_cells_count(self.nodes[parent_id].node, "#interrupt-cells"))
            .unwrap_or(1);
        let Some(byte_count) = cell_count.checked_mul(4) else {
            return Vec::new();
        };
        if byte_count == 0 || !value.len().is_multiple_of(byte_count) {
            return Vec::new();
        }

        let mut interrupts = Vec::new();
        for chunk in value.chunks_exact(byte_count) {
            let Some(specifier) = read_u32_cells(chunk) else {
                continue;
            };
            interrupts.push(DtbInterruptInfo { parent, specifier });
        }
        interrupts
    }

    fn interrupt_parent_phandle(&self, node_id: NodeId) -> Option<u32> {
        let mut current = Some(node_id);
        while let Some(id) = current {
            if let Some(value) = self.nodes[id]
                .node
                .find_property("interrupt-parent")
                .and_then(|prop| read_be_u32_prop(prop.value()))
            {
                return Some(value);
            }
            current = self.nodes[id].parent;
        }
        None
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

    fn node_is_cpu(&self, node_id: NodeId) -> bool {
        let node = self.nodes[node_id].node;
        node.base_name_bytes() == b"cpu" || property_first_string_eq(node, "device_type", "cpu")
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

    fn node_is_interrupt_controller(&self, node_id: NodeId) -> bool {
        self.nodes[node_id]
            .node
            .find_property("interrupt-controller")
            .is_some()
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

    fn phandle_for_node(&self, node_id: NodeId) -> Option<u32> {
        self.phandles
            .iter()
            .find(|entry| entry.node_id == node_id)
            .map(|entry| entry.value)
    }

    fn read_cpu_reg(&self, node_id: NodeId) -> Option<u64> {
        let entry = &self.nodes[node_id];
        let value = entry.node.find_property("reg")?.value();
        let byte_count = entry.reg_addr_cells.checked_mul(4)?;
        if entry.reg_addr_cells == 0 || value.len() < byte_count {
            return None;
        }
        let raw = read_cells_u128(&value[..byte_count], entry.reg_addr_cells).ok()?;
        u64::try_from(raw).ok()
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

fn compatible_strings(node: DtbNode<'static>) -> Vec<&'static str> {
    let Some(prop) = node.find_property("compatible") else {
        return Vec::new();
    };
    prop.value()
        .split(|&byte| byte == 0)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| str::from_utf8(entry).ok())
        .collect()
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

fn read_current_speed(node: DtbNode<'static>) -> Option<u32> {
    read_be_u32_prop(node.find_property("current-speed")?.value())
}

fn scalar_properties(node: DtbNode<'static>) -> Vec<DtbDeviceProperty> {
    let mut properties = Vec::new();
    for property in node.properties() {
        let Some(name) = property.name() else {
            continue;
        };
        let value = property.value();
        let value = if value.is_empty() {
            DtbPropertyValue::Bool
        } else if string_property_name(name) {
            let values = string_list(value);
            if values.is_empty() {
                DtbPropertyValue::Bytes(value.to_vec().into_boxed_slice())
            } else {
                DtbPropertyValue::StringList(values)
            }
        } else if value.len() == 4 {
            let Some(value) = read_be_u32_prop(value) else {
                continue;
            };
            DtbPropertyValue::U32(value)
        } else if value.len().is_multiple_of(4) {
            let Some(values) = read_u32_cells(value) else {
                continue;
            };
            DtbPropertyValue::U32List(values)
        } else {
            DtbPropertyValue::Bytes(value.to_vec().into_boxed_slice())
        };
        properties.push(DtbDeviceProperty { name, value });
    }
    properties
}

fn string_property_name(name: &str) -> bool {
    matches!(
        name,
        "compatible" | "device_type" | "model" | "reg-names" | "status"
    )
}

fn read_pci_domain(node: DtbNode<'static>) -> Option<u16> {
    let value = read_be_u32_prop(node.find_property("linux,pci-domain")?.value())?;
    u16::try_from(value).ok()
}

fn pci_address_space(phys_hi: u32) -> Option<DtbPciAddressSpace> {
    const PCI_RANGE_SPACE_MASK: u32 = 0x0300_0000;
    const PCI_RANGE_IO: u32 = 0x0100_0000;
    const PCI_RANGE_MEM32: u32 = 0x0200_0000;
    const PCI_RANGE_MEM64: u32 = 0x0300_0000;
    const PCI_RANGE_PREFETCHABLE: u32 = 0x4000_0000;

    match phys_hi & PCI_RANGE_SPACE_MASK {
        PCI_RANGE_IO => Some(DtbPciAddressSpace::Io),
        PCI_RANGE_MEM32 | PCI_RANGE_MEM64 => {
            if phys_hi & PCI_RANGE_PREFETCHABLE != 0 {
                Some(DtbPciAddressSpace::PrefetchableMemory)
            } else {
                Some(DtbPciAddressSpace::Memory)
            }
        }
        0 => None,
        other => Some(DtbPciAddressSpace::Unknown(other)),
    }
}

fn pci_child_range_address(cells: &[u32]) -> Option<u64> {
    if cells.len() < 3 {
        return None;
    }
    Some(((cells[1] as u64) << 32) | cells[2] as u64)
}

fn string_list(value: &'static [u8]) -> Vec<&'static str> {
    value
        .split(|&byte| byte == 0)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| str::from_utf8(entry).ok())
        .collect()
}

fn indexed_name_suffix(name: &str, prefix: &str) -> Option<u32> {
    let suffix = name.strip_prefix(prefix)?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
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

fn read_u32_cells(value: &[u8]) -> Option<Box<[u32]>> {
    if value.is_empty() || !value.len().is_multiple_of(4) {
        return None;
    }
    let mut cells = Vec::new();
    for chunk in value.chunks_exact(4) {
        cells.push(read_be_u32_prop(chunk)?);
    }
    Some(cells.into_boxed_slice())
}

fn read_fixed_u32_cells(value: &[u8], cells: usize) -> Option<Box<[u32]>> {
    let expected = cells.checked_mul(4)?;
    if value.len() != expected {
        return None;
    }
    if cells == 0 {
        return Some(Vec::new().into_boxed_slice());
    }
    read_u32_cells(value)
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

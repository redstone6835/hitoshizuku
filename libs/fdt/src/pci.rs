//! Open Firmware PCI host binding 的无损、原子解码。

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::{AddressError, InterruptError, NodeId, Property, PropertyError, Tree};

const PCI_SPACE_MASK: u32 = 0x0300_0000;
const PCI_SPACE_IO: u32 = 0x0100_0000;
const PCI_SPACE_MEM32: u32 = 0x0200_0000;
const PCI_SPACE_MEM64: u32 = 0x0300_0000;
const PCI_PREFETCHABLE: u32 = 0x4000_0000;
const PCI_RELOCATABLE: u32 = 0x8000_0000;
const PCI_ALIASED: u32 = 0x2000_0000;
const COMPAT_LOONGSON_PCH_PIC: &str = "loongson,pch-pic-1.0";

/// PCI child address 的空间类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciAddressSpace {
    Io,
    Memory32,
    Memory64,
}

/// PCI host `ranges` 中的一个完整窗口。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciRange {
    pub space: PciAddressSpace,
    pub prefetchable: bool,
    pub relocatable: bool,
    pub aliased: bool,
    /// 未丢失定义位的 PCI phys.hi cell。
    pub phys_hi: u32,
    pub child_address: u64,
    /// 经普通父总线 `ranges` 递归翻译后的根地址。
    pub parent_address: u128,
    pub size: u128,
}

/// PCI interrupt-map 中的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciInterruptMapEntry {
    pub child_address: Vec<u32>,
    pub child_interrupt: Vec<u32>,
    pub parent: NodeId,
    pub parent_phandle: u32,
    pub parent_address: Vec<u32>,
    pub parent_specifier: Vec<u32>,
}

/// PCI child key 经 host map 与后续 interrupt nexus 递归翻译后的最终路由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciInterruptRoute {
    pub provider: NodeId,
    pub provider_phandle: u32,
    pub address: Vec<u32>,
    pub specifier: Vec<u32>,
}

/// 已规范化的 PCI interrupt-map；缺失 mask 时按 Linux 语义全一填充。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciInterruptMap {
    pub host: NodeId,
    pub child_address_cells: usize,
    pub child_interrupt_cells: usize,
    pub mask: Vec<u32>,
    /// `interrupt-map-pass-thru`；缺失时按规范填充全零。
    pub pass_thru: Vec<u32>,
    pub entries: Vec<PciInterruptMapEntry>,
}

/// PCI `msi-map` 中的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciMsiMapEntry {
    pub requester_base: u32,
    pub controller: NodeId,
    pub controller_phandle: u32,
    /// 按目标 `#msi-cells` 保留的输出 specifier。
    pub msi_specifier: Vec<u32>,
    pub length: u32,
}

/// 已规范化的 PCI msi-map。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciMsiMap {
    /// `msi-map-mask`；缺失时为 `0xffff_ffff`。
    pub mask: u32,
    pub entries: Vec<PciMsiMapEntry>,
}

/// PCI binding 解码错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PciError {
    InvalidNode(NodeId),
    MissingParent(NodeId),
    MissingRequired {
        node: NodeId,
        property: &'static str,
    },
    InvalidProperty {
        node: NodeId,
        property: &'static str,
        error: PropertyError,
    },
    InvalidAddress(AddressError),
    InvalidInterrupt(InterruptError),
    InvalidCellCount {
        node: NodeId,
        property: &'static str,
        expected: Option<u32>,
        actual: u32,
    },
    IncompleteEntry {
        node: NodeId,
        property: &'static str,
        entry: usize,
        remaining_cells: usize,
        required_cells: usize,
    },
    UnknownPhandle {
        node: NodeId,
        property: &'static str,
        entry: usize,
        phandle: u32,
    },
    InvalidValue {
        node: NodeId,
        property: &'static str,
        entry: usize,
        value: u128,
    },
    InvalidInterruptKey {
        node: NodeId,
        address_expected: usize,
        address_actual: usize,
        interrupt_expected: usize,
        interrupt_actual: usize,
    },
    Overflow {
        node: NodeId,
        property: &'static str,
        entry: usize,
    },
}

impl fmt::Display for PciError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT PCI error: {self:?}")
    }
}

impl From<AddressError> for PciError {
    fn from(value: AddressError) -> Self {
        Self::InvalidAddress(value)
    }
}

impl From<InterruptError> for PciError {
    fn from(value: InterruptError) -> Self {
        Self::InvalidInterrupt(value)
    }
}

impl Tree<'_> {
    /// 解码 PCI host 的 required `ranges`。
    ///
    /// `Ok(None)` 仅表示属性缺失；空属性会返回空列表，由具体
    /// host binding 决定是否允许。任意坏条目都会使整个属性失败。
    pub fn pci_ranges(&self, host: NodeId) -> Result<Option<Vec<PciRange>>, PciError> {
        let node = self.node(host).ok_or(PciError::InvalidNode(host))?;
        let parent = self.parent(host).ok_or(PciError::MissingParent(host))?;
        let Some(property) = node.property("ranges") else {
            return Ok(None);
        };
        let child_cells = self.exact_cell_count(host, "#address-cells", 3)?;
        let size_cells = self.exact_cell_count(host, "#size-cells", 2)?;
        let parent_cells = self.address_cells(parent)? as usize;
        let stride = child_cells
            .checked_add(parent_cells)
            .and_then(|value| value.checked_add(size_cells))
            .ok_or(PciError::Overflow {
                node: host,
                property: "ranges",
                entry: 0,
            })?;
        let values = property_cells(host, "ranges", property)?;
        if values.is_empty() {
            return Ok(Some(Vec::new()));
        }
        if !values.len().is_multiple_of(stride) {
            return Err(PciError::IncompleteEntry {
                node: host,
                property: "ranges",
                entry: values.len() / stride,
                remaining_cells: values.len() % stride,
                required_cells: stride,
            });
        }

        let mut result = Vec::with_capacity(values.len() / stride);
        for (entry, row) in values.chunks_exact(stride).enumerate() {
            let phys_hi = row[0];
            let space = match phys_hi & PCI_SPACE_MASK {
                PCI_SPACE_IO => PciAddressSpace::Io,
                PCI_SPACE_MEM32 => PciAddressSpace::Memory32,
                PCI_SPACE_MEM64 => PciAddressSpace::Memory64,
                value => {
                    return Err(PciError::InvalidValue {
                        node: host,
                        property: "ranges",
                        entry,
                        value: u128::from(value),
                    });
                }
            };
            let child_address = (u64::from(row[1]) << 32) | u64::from(row[2]);
            let parent_start = child_cells;
            let size_start = parent_start + parent_cells;
            let parent_address =
                cells_value(host, "ranges", entry, &row[parent_start..size_start])?;
            let size = cells_value(host, "ranges", entry, &row[size_start..])?;
            if size == 0 {
                return Err(PciError::InvalidValue {
                    node: host,
                    property: "ranges",
                    entry,
                    value: 0,
                });
            }
            child_address
                .checked_add(u64::try_from(size).map_err(|_| PciError::Overflow {
                    node: host,
                    property: "ranges",
                    entry,
                })?)
                .ok_or(PciError::Overflow {
                    node: host,
                    property: "ranges",
                    entry,
                })?;
            let parent_address = self.translate_address(parent, parent_address, Some(size))?;
            parent_address.checked_add(size).ok_or(PciError::Overflow {
                node: host,
                property: "ranges",
                entry,
            })?;
            result.push(PciRange {
                space,
                prefetchable: phys_hi & PCI_PREFETCHABLE != 0,
                relocatable: phys_hi & PCI_RELOCATABLE != 0,
                aliased: phys_hi & PCI_ALIASED != 0,
                phys_hi,
                child_address,
                parent_address,
                size,
            });
        }
        Ok(Some(result))
    }

    /// 解码 PCI host `interrupt-map[-mask]`。
    pub fn pci_interrupt_map(&self, host: NodeId) -> Result<Option<PciInterruptMap>, PciError> {
        let node = self.node(host).ok_or(PciError::InvalidNode(host))?;
        let Some(property) = node.property("interrupt-map") else {
            return Ok(None);
        };
        let child_address_cells = self.exact_cell_count(host, "#address-cells", 3)?;
        // PCI child interrupt specifier 按 binding 固定为一个 cell。Linux 与现有
        // QEMU LoongArch 固件允许 host 在提供 interrupt-map 时省略这项推荐声明。
        let child_interrupt_cells = if node.property("#interrupt-cells").is_some() {
            self.exact_cell_count(host, "#interrupt-cells", 1)?
        } else {
            1
        };
        let key_cells = child_address_cells
            .checked_add(child_interrupt_cells)
            .ok_or(PciError::Overflow {
                node: host,
                property: "interrupt-map",
                entry: 0,
            })?;
        let fixed = key_cells.checked_add(1).ok_or(PciError::Overflow {
            node: host,
            property: "interrupt-map",
            entry: 0,
        })?;
        let values = property_cells(host, "interrupt-map", property)?;
        require_remaining(host, "interrupt-map", 0, values.len(), fixed)?;
        let legacy_loongson_map = is_legacy_loongson_pci_interrupt_map(
            self,
            &values,
            child_address_cells,
            child_interrupt_cells,
        );
        let mask = match node.property("interrupt-map-mask") {
            None => vec![u32::MAX; key_cells],
            Some(mask) => {
                let values = property_cells(host, "interrupt-map-mask", mask)?;
                if values.len() != key_cells {
                    return Err(PciError::IncompleteEntry {
                        node: host,
                        property: "interrupt-map-mask",
                        entry: 0,
                        remaining_cells: values.len(),
                        required_cells: key_cells,
                    });
                }
                values
            }
        };
        let pass_thru = match node.property("interrupt-map-pass-thru") {
            None => vec![0; key_cells],
            Some(property) => {
                let values = property_cells(host, "interrupt-map-pass-thru", property)?;
                if values.len() != key_cells {
                    return Err(PciError::IncompleteEntry {
                        node: host,
                        property: "interrupt-map-pass-thru",
                        entry: 0,
                        remaining_cells: values.len(),
                        required_cells: key_cells,
                    });
                }
                values
            }
        };
        let mut entries = Vec::new();
        let mut offset = 0usize;
        let mut entry = 0usize;
        while offset < values.len() {
            require_remaining(host, "interrupt-map", entry, values.len() - offset, fixed)?;
            let child_address = values[offset..offset + child_address_cells].to_vec();
            offset += child_address_cells;
            let child_interrupt = values[offset..offset + child_interrupt_cells].to_vec();
            offset += child_interrupt_cells;
            let phandle = values[offset];
            offset += 1;
            let parent = self
                .node_by_phandle(phandle)
                .ok_or(PciError::UnknownPhandle {
                    node: host,
                    property: "interrupt-map",
                    entry,
                    phandle,
                })?;
            // Linux compatibility: absent parent #address-cells means zero in interrupt-map.
            let parent_address_cells = optional_count(self, parent, "#address-cells", 0)?;
            let parent_interrupt_cells = if legacy_loongson_map {
                1
            } else {
                required_count(self, parent, "#interrupt-cells")?
            };
            let variable = parent_address_cells
                .checked_add(parent_interrupt_cells)
                .ok_or(PciError::Overflow {
                    node: host,
                    property: "interrupt-map",
                    entry,
                })?;
            require_remaining(
                host,
                "interrupt-map",
                entry,
                values.len() - offset,
                variable,
            )?;
            let parent_address = values[offset..offset + parent_address_cells].to_vec();
            offset += parent_address_cells;
            let parent_specifier = values[offset..offset + parent_interrupt_cells].to_vec();
            offset += parent_interrupt_cells;
            if !self.pci_interrupt_parent_available(parent)? {
                entry += 1;
                continue;
            }
            entries.push(PciInterruptMapEntry {
                child_address,
                child_interrupt,
                parent,
                parent_phandle: phandle,
                parent_address,
                parent_specifier,
            });
            entry += 1;
        }
        Ok(Some(PciInterruptMap {
            host,
            child_address_cells,
            child_interrupt_cells,
            mask,
            pass_thru,
            entries,
        }))
    }

    /// 用实际 PCI child address/interrupt key 选择 map 行，并递归解析到最终 IRQ 域。
    ///
    /// `interrupt-map-pass-thru` 必须在直接父 nexus 的 key 上生效，因此不能在只解码
    /// map 表时提前折叠父链。该接口严格按 `mask -> pass-thru -> parent nexus` 顺序
    /// 完成运行时翻译。
    pub fn resolve_pci_interrupt(
        &self,
        map: &PciInterruptMap,
        child_address: &[u32],
        child_interrupt: &[u32],
    ) -> Result<Option<PciInterruptRoute>, PciError> {
        self.node(map.host).ok_or(PciError::InvalidNode(map.host))?;
        let key_cells = map
            .child_address_cells
            .checked_add(map.child_interrupt_cells)
            .ok_or(PciError::Overflow {
                node: map.host,
                property: "interrupt-map",
                entry: 0,
            })?;
        if child_address.len() != map.child_address_cells
            || child_interrupt.len() != map.child_interrupt_cells
        {
            return Err(PciError::InvalidInterruptKey {
                node: map.host,
                address_expected: map.child_address_cells,
                address_actual: child_address.len(),
                interrupt_expected: map.child_interrupt_cells,
                interrupt_actual: child_interrupt.len(),
            });
        }
        validate_runtime_map_width(map.host, "interrupt-map-mask", map.mask.len(), key_cells)?;
        validate_runtime_map_width(
            map.host,
            "interrupt-map-pass-thru",
            map.pass_thru.len(),
            key_cells,
        )?;
        for (index, entry) in map.entries.iter().enumerate() {
            let encoded_key_cells = entry
                .child_address
                .len()
                .checked_add(entry.child_interrupt.len())
                .ok_or(PciError::Overflow {
                    node: map.host,
                    property: "interrupt-map",
                    entry: index,
                })?;
            validate_runtime_map_entry_width(map.host, index, encoded_key_cells, key_cells)?;
        }

        let mut child_key = Vec::with_capacity(key_cells);
        child_key.extend_from_slice(child_address);
        child_key.extend_from_slice(child_interrupt);
        for entry in &map.entries {
            let matches = child_key
                .iter()
                .zip(entry.child_address.iter().chain(&entry.child_interrupt))
                .zip(&map.mask)
                .all(|((&actual, &expected), &mask)| (actual ^ expected) & mask == 0);
            if !matches {
                continue;
            }

            let mut parent_key =
                Vec::with_capacity(entry.parent_address.len() + entry.parent_specifier.len());
            parent_key.extend_from_slice(&entry.parent_address);
            parent_key.extend_from_slice(&entry.parent_specifier);
            for (index, value) in parent_key.iter_mut().enumerate() {
                let Some((&child, &pass)) = child_key.get(index).zip(map.pass_thru.get(index))
                else {
                    break;
                };
                *value = (*value & !pass) | (child & pass);
            }
            let parent_address_cells = entry.parent_address.len();
            let (provider, address, specifier) = if is_legacy_loongson_pch_pic_parent(
                self,
                entry.parent,
                parent_address_cells,
                parent_key.len() - parent_address_cells,
            ) {
                (
                    entry.parent,
                    parent_key[..parent_address_cells].to_vec(),
                    parent_key[parent_address_cells..].to_vec(),
                )
            } else {
                let translated = self
                    .translate_interrupt_route(
                        entry.parent,
                        parent_key[..parent_address_cells].to_vec(),
                        parent_key[parent_address_cells..].to_vec(),
                    )
                    .map_err(PciError::InvalidInterrupt)?;
                (
                    translated.provider,
                    translated.address,
                    translated.specifier,
                )
            };
            let provider_phandle = self.phandle(provider).ok_or(PciError::MissingRequired {
                node: provider,
                property: "phandle",
            })?;
            return Ok(Some(PciInterruptRoute {
                provider,
                provider_phandle,
                address,
                specifier,
            }));
        }
        Ok(None)
    }

    /// 解码 PCI host `msi-map[-mask]`，并保留 target `#msi-cells` 宽度。
    ///
    /// 当前 binding 的每项为 `<rid-base phandle msi-base... length>`。同时兼容
    /// Linux 接受的历史坏表：目标声明两个 cell，但整张表仍由四-cell、长度为 1
    /// 且指向同一 phandle 的旧格式条目组成，此时按单输出 cell 解码。
    pub fn pci_msi_map(&self, host: NodeId) -> Result<Option<PciMsiMap>, PciError> {
        let node = self.node(host).ok_or(PciError::InvalidNode(host))?;
        let Some(property) = node.property("msi-map") else {
            return Ok(None);
        };
        let mask = match node.property("msi-map-mask") {
            None => u32::MAX,
            Some(property) => property
                .as_u32()
                .map_err(|error| PciError::InvalidProperty {
                    node: host,
                    property: "msi-map-mask",
                    error,
                })?,
        };
        let values = property_cells(host, "msi-map", property)?;
        let mut entries = Vec::new();
        let mut offset = 0usize;
        let mut entry = 0usize;
        let mut legacy_one_cell = false;
        while offset < values.len() {
            require_remaining(host, "msi-map", entry, values.len() - offset, 2)?;
            let requester_base = values[offset];
            let phandle = values[offset + 1];
            offset += 2;
            if requester_base & !mask != 0 {
                return Err(PciError::InvalidValue {
                    node: host,
                    property: "msi-map",
                    entry,
                    value: u128::from(requester_base),
                });
            }
            let controller = self
                .node_by_phandle(phandle)
                .ok_or(PciError::UnknownPhandle {
                    node: host,
                    property: "msi-map",
                    entry,
                    phandle,
                })?;
            // Linux compatibility: legacy MSI domains commonly omit #msi-cells.
            let mut msi_cells = optional_count(self, controller, "#msi-cells", 1)?;
            if entry == 0 && msi_cells == 2 {
                legacy_one_cell = is_legacy_one_cell_map(&values);
            }
            if legacy_one_cell {
                msi_cells = 1;
            }
            let remaining = values.len() - offset;
            let required = msi_cells.checked_add(1).ok_or(PciError::Overflow {
                node: host,
                property: "msi-map",
                entry,
            })?;
            require_remaining(host, "msi-map", entry, remaining, required)?;
            let msi_specifier = values[offset..offset + msi_cells].to_vec();
            offset += msi_cells;
            let length = values[offset];
            offset += 1;
            if length == 0
                || requester_base.checked_add(length - 1).is_none()
                || msi_specifier
                    .iter()
                    .any(|base| base.checked_add(length - 1).is_none())
            {
                return Err(PciError::InvalidValue {
                    node: host,
                    property: "msi-map",
                    entry,
                    value: u128::from(length),
                });
            }
            entries.push(PciMsiMapEntry {
                requester_base,
                controller,
                controller_phandle: phandle,
                msi_specifier,
                length,
            });
            entry += 1;
        }
        Ok(Some(PciMsiMap { mask, entries }))
    }

    fn exact_cell_count(
        &self,
        node: NodeId,
        property: &'static str,
        expected: u32,
    ) -> Result<usize, PciError> {
        let actual = self
            .node(node)
            .ok_or(PciError::InvalidNode(node))?
            .property(property)
            .ok_or(PciError::MissingRequired { node, property })?
            .as_u32()
            .map_err(|error| PciError::InvalidProperty {
                node,
                property,
                error,
            })?;
        if actual != expected {
            return Err(PciError::InvalidCellCount {
                node,
                property,
                expected: Some(expected),
                actual,
            });
        }
        Ok(actual as usize)
    }

    /// 与 Linux `of_irq_parse_raw()` 一致，禁用的 interrupt-map 目标不参与路由。
    fn pci_interrupt_parent_available(&self, parent: NodeId) -> Result<bool, PciError> {
        let node = self.node(parent).ok_or(PciError::InvalidNode(parent))?;
        let Some(property) = node.property("status") else {
            return Ok(true);
        };
        let status = property
            .as_str()
            .map_err(|error| PciError::InvalidProperty {
                node: parent,
                property: "status",
                error,
            })?;
        Ok(matches!(status, "ok" | "okay"))
    }
}

/// 兼容 QEMU LoongArch 长期生成的 PCI interrupt-map：host 省略
/// `#interrupt-cells`，且指向声明为两 cell 的 PCH PIC 时只编码 source cell。
/// 只有整张表都严格符合这一已知布局才启用兼容路径，标准两 cell 表仍按声明解析。
fn is_legacy_loongson_pci_interrupt_map(
    tree: &Tree<'_>,
    values: &[u32],
    child_address_cells: usize,
    child_interrupt_cells: usize,
) -> bool {
    let Some(key_cells) = child_address_cells.checked_add(child_interrupt_cells) else {
        return false;
    };
    let Some(stride) = key_cells.checked_add(2) else {
        return false;
    };
    if values.is_empty() || !values.len().is_multiple_of(stride) {
        return false;
    }
    values.chunks_exact(stride).all(|row| {
        let Some(parent) = tree.node_by_phandle(row[key_cells]) else {
            return false;
        };
        is_legacy_loongson_pch_pic_parent(tree, parent, 0, 1)
    })
}

fn is_legacy_loongson_pch_pic_parent(
    tree: &Tree<'_>,
    parent: NodeId,
    encoded_address_cells: usize,
    encoded_interrupt_cells: usize,
) -> bool {
    if encoded_address_cells != 0
        || encoded_interrupt_cells != 1
        || optional_count(tree, parent, "#address-cells", 0) != Ok(0)
        || required_count(tree, parent, "#interrupt-cells") != Ok(2)
    {
        return false;
    }
    let Some(node) = tree.node(parent) else {
        return false;
    };
    if node.property("interrupt-map").is_some()
        || node
            .property("interrupt-controller")
            .and_then(|property| property.as_bool().ok())
            != Some(true)
    {
        return false;
    }
    node.property("compatible")
        .and_then(|property| property.as_string_list().ok())
        .is_some_and(|mut values| values.any(|value| value == COMPAT_LOONGSON_PCH_PIC))
}

fn property_cells(
    node: NodeId,
    property_name: &'static str,
    property: Property<'_>,
) -> Result<Vec<u32>, PciError> {
    property
        .cells()
        .map(|cells| cells.collect())
        .map_err(|error| PciError::InvalidProperty {
            node,
            property: property_name,
            error,
        })
}

fn validate_runtime_map_width(
    node: NodeId,
    property: &'static str,
    actual: usize,
    expected: usize,
) -> Result<(), PciError> {
    if actual == expected {
        return Ok(());
    }
    Err(PciError::IncompleteEntry {
        node,
        property,
        entry: 0,
        remaining_cells: actual,
        required_cells: expected,
    })
}

fn validate_runtime_map_entry_width(
    node: NodeId,
    entry: usize,
    actual: usize,
    expected: usize,
) -> Result<(), PciError> {
    if actual == expected {
        return Ok(());
    }
    Err(PciError::IncompleteEntry {
        node,
        property: "interrupt-map",
        entry,
        remaining_cells: actual,
        required_cells: expected,
    })
}

fn required_count(
    tree: &Tree<'_>,
    node: NodeId,
    property_name: &'static str,
) -> Result<usize, PciError> {
    let view = tree.node(node).ok_or(PciError::InvalidNode(node))?;
    let property = view
        .property(property_name)
        .ok_or(PciError::MissingRequired {
            node,
            property: property_name,
        })?;
    let value = property
        .as_u32()
        .map_err(|error| PciError::InvalidProperty {
            node,
            property: property_name,
            error,
        })?;
    usize::try_from(value).map_err(|_| PciError::InvalidCellCount {
        node,
        property: property_name,
        expected: None,
        actual: value,
    })
}

fn optional_count(
    tree: &Tree<'_>,
    node: NodeId,
    property_name: &'static str,
    default: usize,
) -> Result<usize, PciError> {
    let view = tree.node(node).ok_or(PciError::InvalidNode(node))?;
    let Some(property) = view.property(property_name) else {
        return Ok(default);
    };
    let value = property
        .as_u32()
        .map_err(|error| PciError::InvalidProperty {
            node,
            property: property_name,
            error,
        })?;
    usize::try_from(value).map_err(|_| PciError::InvalidCellCount {
        node,
        property: property_name,
        expected: None,
        actual: value,
    })
}

/// 兼容 Linux `of_check_bad_map()` 识别的历史四-cell map。
fn is_legacy_one_cell_map(values: &[u32]) -> bool {
    let Some(first) = values.chunks_exact(4).next() else {
        return false;
    };
    values.len().is_multiple_of(4)
        && values
            .chunks_exact(4)
            .all(|entry| entry[1] == first[1] && entry[3] == 1)
}

fn require_remaining(
    node: NodeId,
    property: &'static str,
    entry: usize,
    remaining: usize,
    required: usize,
) -> Result<(), PciError> {
    if remaining < required {
        Err(PciError::IncompleteEntry {
            node,
            property,
            entry,
            remaining_cells: remaining,
            required_cells: required,
        })
    } else {
        Ok(())
    }
}

fn cells_value(
    node: NodeId,
    property: &'static str,
    _entry: usize,
    cells: &[u32],
) -> Result<u128, PciError> {
    if cells.len() > 4 {
        return Err(PciError::InvalidCellCount {
            node,
            property,
            expected: None,
            actual: cells.len() as u32,
        });
    }
    Ok(cells
        .iter()
        .fold(0u128, |value, &cell| (value << 32) | u128::from(cell)))
}

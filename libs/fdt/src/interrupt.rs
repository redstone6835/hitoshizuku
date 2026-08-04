//! Devicetree interrupt-parent 与 interrupt specifier 的规范化解码。

use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::{NodeId, Property, PropertyError, Tree};

/// 一条已经绑定到具体 provider 节点的 interrupt specifier。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterruptSpecifier {
    /// interrupt provider 的稳定节点编号。
    pub provider: NodeId,
    /// provider 的规范 phandle；隐式父节点可以没有 phandle。
    pub phandle: Option<u32>,
    /// 按 provider `#interrupt-cells` 保留的原始大端 cell 值。
    pub cells: Vec<u32>,
}

/// interrupt binding 解码错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InterruptError {
    /// 节点编号不属于当前树。
    InvalidNode(NodeId),
    /// 属性不符合其 binding 类型。
    InvalidProperty {
        /// 属性所在节点。
        node: NodeId,
        /// 属性名。
        property: &'static str,
        /// 精确的底层解码错误。
        error: PropertyError,
    },
    /// interrupt specifier 无法找到最终 provider。
    MissingProvider(NodeId),
    /// phandle 没有对应节点。
    UnknownPhandle {
        /// 引用所在节点。
        node: NodeId,
        /// 引用属性。
        property: &'static str,
        /// 属性中的条目序号。
        entry: usize,
        /// 未解析的 phandle。
        phandle: u32,
    },
    /// provider 没有必需的 `#interrupt-cells`。
    MissingInterruptCells(NodeId),
    /// `#interrupt-cells` 无法在本机上形成切片，或零宽普通 `interrupts`
    /// 无法确定条目边界。
    InvalidInterruptCells { provider: NodeId, cells: u32 },
    /// `#address-cells` 无法在本机上形成切片。
    InvalidAddressCells { nexus: NodeId, cells: u32 },
    /// nexus 需要 unit address，但设备没有 `reg`。
    MissingUnitAddress {
        /// 产生中断的设备节点。
        node: NodeId,
        /// 需要 unit address 的 nexus。
        nexus: NodeId,
    },
    /// 当前 specifier 的宽度与 interrupt domain 不一致。
    InvalidSpecifierLength {
        /// 当前 interrupt domain provider。
        provider: NodeId,
        /// provider 声明的 cell 数。
        expected_cells: usize,
        /// 实际 cell 数。
        actual_cells: usize,
    },
    /// `interrupt-map` 没有可用的匹配行。
    MissingMapEntry(NodeId),
    /// cell 数求和溢出本机 `usize`。
    CellCountOverflow {
        /// 声明布局的节点。
        node: NodeId,
        /// 正在解码的属性。
        property: &'static str,
    },
    /// 属性末尾不足以组成完整条目。
    IncompleteEntry {
        /// 属性所在节点。
        node: NodeId,
        /// 属性名。
        property: &'static str,
        /// 正在解码的条目序号。
        entry: usize,
        /// 剩余 cell 数。
        remaining_cells: usize,
        /// 当前条目需要的 cell 数。
        required_cells: usize,
    },
    /// interrupt-parent 链成环或超过树的节点数。
    ParentCycle(NodeId),
}

impl fmt::Display for InterruptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT interrupt error: {self:?}")
    }
}

#[derive(Debug)]
pub(crate) struct MapTranslation {
    pub(crate) provider: NodeId,
    pub(crate) address: Vec<u32>,
    pub(crate) specifier: Vec<u32>,
}

impl Tree<'_> {
    /// 按 DTSpec 与 Linux `of_irq_find_parent()` 语义解析首个 interrupt domain。
    ///
    /// 每一层优先使用显式 `interrupt-parent`；缺失时走设备树父边。声明了合法
    /// `#interrupt-cells` 的节点可以是 interrupt controller，也可以是仍需经过
    /// `interrupt-map` 翻译的 nexus。
    pub fn interrupt_provider(&self, node: NodeId) -> Result<Option<NodeId>, InterruptError> {
        self.node(node).ok_or(InterruptError::InvalidNode(node))?;
        let mut current = node;
        for _ in 0..=self.len() {
            let view = self
                .node(current)
                .ok_or(InterruptError::InvalidNode(current))?;
            let next = if let Some(property) = view.property("interrupt-parent") {
                let phandle =
                    property
                        .as_u32()
                        .map_err(|error| InterruptError::InvalidProperty {
                            node: current,
                            property: "interrupt-parent",
                            error,
                        })?;
                self.node_by_phandle(phandle)
                    .ok_or(InterruptError::UnknownPhandle {
                        node: current,
                        property: "interrupt-parent",
                        entry: 0,
                        phandle,
                    })?
            } else if let Some(parent) = self.parent(current) {
                parent
            } else {
                return Ok(None);
            };

            let provider = self.node(next).ok_or(InterruptError::InvalidNode(next))?;
            if let Some(property) = provider.property("#interrupt-cells") {
                let cells = property
                    .as_u32()
                    .map_err(|error| InterruptError::InvalidProperty {
                        node: next,
                        property: "#interrupt-cells",
                        error,
                    })?;
                validate_interrupt_cells(next, cells)?;
                return Ok(Some(next));
            }
            current = next;
        }
        Err(InterruptError::ParentCycle(node))
    }

    /// 原子地解码并翻译节点的 `interrupts-extended` 或 `interrupts`。
    ///
    /// 解析会递归应用每一层 `interrupt-map[-mask]`，最终只返回实际声明了
    /// `interrupt-controller` 的 provider。`Ok(None)` 仅表示两个属性都缺失。
    /// 只要高优先级的 `interrupts-extended` 存在，任意坏条目都会返回 `Err`，
    /// 不会退化到 `interrupts` 或泄露已经解析的前缀。
    pub fn interrupts(
        &self,
        node: NodeId,
    ) -> Result<Option<Vec<InterruptSpecifier>>, InterruptError> {
        let view = self.node(node).ok_or(InterruptError::InvalidNode(node))?;
        if let Some(property) = view.property("interrupts-extended") {
            let values = property_cells(node, "interrupts-extended", property)?;
            let mut raw = Vec::new();
            let mut offset = 0usize;
            let mut entry = 0usize;
            while offset < values.len() {
                let phandle = values[offset];
                offset += 1;
                let provider =
                    self.node_by_phandle(phandle)
                        .ok_or(InterruptError::UnknownPhandle {
                            node,
                            property: "interrupts-extended",
                            entry,
                            phandle,
                        })?;
                let cells = self.required_interrupt_cells(provider)?;
                let end = offset
                    .checked_add(cells)
                    .ok_or(InterruptError::CellCountOverflow {
                        node: provider,
                        property: "#interrupt-cells",
                    })?;
                let specifier = values
                    .get(offset..end)
                    .ok_or(InterruptError::IncompleteEntry {
                        node,
                        property: "interrupts-extended",
                        entry,
                        remaining_cells: values.len() - offset,
                        required_cells: cells,
                    })?;
                raw.push((provider, specifier.to_vec()));
                offset = end;
                entry += 1;
            }

            let result = raw
                .into_iter()
                .map(|(provider, specifier)| self.translate_interrupt(node, provider, specifier))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Some(result));
        }

        let Some(property) = view.property("interrupts") else {
            return Ok(None);
        };
        let provider = self
            .interrupt_provider(node)?
            .ok_or(InterruptError::MissingProvider(node))?;
        let cells = self.required_interrupt_cells(provider)?;
        let values = property_cells(node, "interrupts", property)?;
        // `interrupts-extended` 和 interrupt-map 都有 phandle 可分隔零宽
        // specifier；普通 interrupts 没有任何信息可确定条目数量，必须拒绝。
        if cells == 0 {
            return Err(InterruptError::InvalidInterruptCells { provider, cells: 0 });
        }
        if !values.len().is_multiple_of(cells) {
            return Err(InterruptError::IncompleteEntry {
                node,
                property: "interrupts",
                entry: values.len() / cells,
                remaining_cells: values.len() % cells,
                required_cells: cells,
            });
        }
        let result = values
            .chunks_exact(cells)
            .map(|specifier| self.translate_interrupt(node, provider, specifier.to_vec()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(result))
    }

    fn translate_interrupt(
        &self,
        device: NodeId,
        provider: NodeId,
        specifier: Vec<u32>,
    ) -> Result<InterruptSpecifier, InterruptError> {
        let address_cells = self.initial_interrupt_address_cells(provider)?;
        let translated = self.translate_interrupt_route_inner(
            Some(device),
            provider,
            address_cells,
            None,
            specifier,
        )?;
        Ok(InterruptSpecifier {
            provider: translated.provider,
            phandle: self.phandle(translated.provider),
            cells: translated.specifier,
        })
    }

    /// 从专用总线 map 的父域继续执行通用 interrupt nexus 翻译。
    pub(crate) fn translate_interrupt_route(
        &self,
        provider: NodeId,
        address: Vec<u32>,
        specifier: Vec<u32>,
    ) -> Result<MapTranslation, InterruptError> {
        self.translate_interrupt_route_inner(
            None,
            provider,
            address.len(),
            Some(address),
            specifier,
        )
    }

    fn translate_interrupt_route_inner(
        &self,
        device: Option<NodeId>,
        mut provider: NodeId,
        mut address_cells: usize,
        mut address: Option<Vec<u32>>,
        mut specifier: Vec<u32>,
    ) -> Result<MapTranslation, InterruptError> {
        let mut visited = BTreeSet::new();

        loop {
            if !visited.insert(provider) {
                return Err(InterruptError::ParentCycle(provider));
            }

            let view = self
                .node(provider)
                .ok_or(InterruptError::InvalidNode(provider))?;
            let expected_cells = self.required_interrupt_cells(provider)?;
            if specifier.len() != expected_cells {
                return Err(InterruptError::InvalidSpecifierLength {
                    provider,
                    expected_cells,
                    actual_cells: specifier.len(),
                });
            }

            if let Some(interrupt_map) = view.property("interrupt-map") {
                if address.is_none() {
                    let device = device.expect("device-backed translation has no unit address yet");
                    address = self.interrupt_unit_address(device, address_cells)?;
                }
                if address_cells != 0 && address.is_none() {
                    return Err(InterruptError::MissingUnitAddress {
                        node: device.expect("only device-backed translation can lack reg"),
                        nexus: provider,
                    });
                }
                let translated = self.translate_interrupt_map(
                    provider,
                    address.as_deref().unwrap_or(&[]),
                    &specifier,
                    interrupt_map,
                )?;
                if translated.provider == provider {
                    if self.is_interrupt_controller(provider)? {
                        return Ok(MapTranslation {
                            provider,
                            address: translated.address,
                            specifier: translated.specifier,
                        });
                    }
                    return Err(InterruptError::ParentCycle(provider));
                }
                provider = translated.provider;
                address_cells = translated.address.len();
                address = Some(translated.address);
                specifier = translated.specifier;
                continue;
            }

            if self.is_interrupt_controller(provider)? {
                return Ok(MapTranslation {
                    provider,
                    address: address.unwrap_or_default(),
                    specifier,
                });
            }

            provider = self
                .interrupt_provider(provider)?
                .ok_or(InterruptError::MissingProvider(provider))?;
        }
    }

    fn translate_interrupt_map(
        &self,
        nexus: NodeId,
        address: &[u32],
        specifier: &[u32],
        property: Property<'_>,
    ) -> Result<MapTranslation, InterruptError> {
        let address_cells = address.len();
        let key_cells = address_cells.checked_add(specifier.len()).ok_or(
            InterruptError::CellCountOverflow {
                node: nexus,
                property: "interrupt-map",
            },
        )?;
        let node = self.node(nexus).ok_or(InterruptError::InvalidNode(nexus))?;
        let values = property_cells(nexus, "interrupt-map", property)?;
        if values.is_empty() {
            return Err(InterruptError::MissingMapEntry(nexus));
        }
        let minimum = key_cells
            .checked_add(1)
            .ok_or(InterruptError::CellCountOverflow {
                node: nexus,
                property: "interrupt-map",
            })?;
        require_remaining(nexus, "interrupt-map", 0, values.len(), minimum)?;

        let mask = match node.property("interrupt-map-mask") {
            None => vec![u32::MAX; key_cells],
            Some(property) => {
                let mask = property_cells(nexus, "interrupt-map-mask", property)?;
                if mask.len() != key_cells {
                    return Err(InterruptError::IncompleteEntry {
                        node: nexus,
                        property: "interrupt-map-mask",
                        entry: 0,
                        remaining_cells: mask.len(),
                        required_cells: key_cells,
                    });
                }
                mask
            }
        };
        let pass_thru = match node.property("interrupt-map-pass-thru") {
            None => vec![0; key_cells],
            Some(property) => {
                let pass_thru = property_cells(nexus, "interrupt-map-pass-thru", property)?;
                if pass_thru.len() != key_cells {
                    return Err(InterruptError::IncompleteEntry {
                        node: nexus,
                        property: "interrupt-map-pass-thru",
                        entry: 0,
                        remaining_cells: pass_thru.len(),
                        required_cells: key_cells,
                    });
                }
                pass_thru
            }
        };
        let mut match_key = Vec::with_capacity(key_cells);
        match_key.extend_from_slice(address);
        match_key.extend_from_slice(specifier);

        let mut selected = None;
        let mut offset = 0usize;
        let mut entry = 0usize;
        while offset < values.len() {
            require_remaining(
                nexus,
                "interrupt-map",
                entry,
                values.len() - offset,
                minimum,
            )?;
            let child_key = &values[offset..offset + key_cells];
            offset += key_cells;
            let phandle = values[offset];
            offset += 1;
            let parent = self
                .node_by_phandle(phandle)
                .ok_or(InterruptError::UnknownPhandle {
                    node: nexus,
                    property: "interrupt-map",
                    entry,
                    phandle,
                })?;
            let parent_address_cells = self.parent_interrupt_address_cells(parent)?;
            let parent_interrupt_cells = self.required_interrupt_cells(parent)?;
            let variable = parent_address_cells
                .checked_add(parent_interrupt_cells)
                .ok_or(InterruptError::CellCountOverflow {
                    node: nexus,
                    property: "interrupt-map",
                })?;
            require_remaining(
                nexus,
                "interrupt-map",
                entry,
                values.len() - offset,
                variable,
            )?;
            let parent_address = &values[offset..offset + parent_address_cells];
            offset += parent_address_cells;
            let parent_specifier = &values[offset..offset + parent_interrupt_cells];
            offset += parent_interrupt_cells;

            let matches = match_key
                .iter()
                .zip(child_key)
                .zip(&mask)
                .all(|((&actual, &expected), &mask)| (actual ^ expected) & mask == 0);
            if selected.is_none() && matches && self.interrupt_parent_available(parent)? {
                let mut parent_key = Vec::with_capacity(variable);
                parent_key.extend_from_slice(parent_address);
                parent_key.extend_from_slice(parent_specifier);
                for (index, value) in parent_key.iter_mut().enumerate() {
                    let Some((&child, &pass)) = match_key.get(index).zip(pass_thru.get(index))
                    else {
                        break;
                    };
                    *value = (*value & !pass) | (child & pass);
                }
                selected = Some(MapTranslation {
                    provider: parent,
                    address: parent_key[..parent_address_cells].to_vec(),
                    specifier: parent_key[parent_address_cells..].to_vec(),
                });
            }
            entry += 1;
        }

        selected.ok_or(InterruptError::MissingMapEntry(nexus))
    }

    fn initial_interrupt_address_cells(&self, nexus: NodeId) -> Result<usize, InterruptError> {
        let mut current = Some(nexus);
        while let Some(node) = current {
            let view = self.node(node).ok_or(InterruptError::InvalidNode(node))?;
            if let Some(property) = view.property("#address-cells") {
                let cells = property
                    .as_u32()
                    .map_err(|error| InterruptError::InvalidProperty {
                        node,
                        property: "#address-cells",
                        error,
                    })?;
                return validate_address_cells(nexus, cells);
            }
            current = self.parent(node);
        }

        // Linux `of_irq_parse_raw()` 的历史兼容默认值，而不是普通父地址的 2-cell
        // 推断：只有初始 nexus 沿物理父链都没有声明时才使用。
        Ok(2)
    }

    fn parent_interrupt_address_cells(&self, parent: NodeId) -> Result<usize, InterruptError> {
        let view = self
            .node(parent)
            .ok_or(InterruptError::InvalidNode(parent))?;
        let Some(property) = view.property("#address-cells") else {
            // Linux 对 interrupt-map 中目标 domain 的兼容默认值为零。
            return Ok(0);
        };
        let cells = property
            .as_u32()
            .map_err(|error| InterruptError::InvalidProperty {
                node: parent,
                property: "#address-cells",
                error,
            })?;
        validate_address_cells(parent, cells)
    }

    fn interrupt_unit_address(
        &self,
        device: NodeId,
        address_cells: usize,
    ) -> Result<Option<Vec<u32>>, InterruptError> {
        if address_cells == 0 {
            return Ok(Some(Vec::new()));
        }
        let view = self
            .node(device)
            .ok_or(InterruptError::InvalidNode(device))?;
        let Some(property) = view.property("reg") else {
            return Ok(None);
        };
        let values = property_cells(device, "reg", property)?;
        let address = values
            .get(..address_cells)
            .ok_or(InterruptError::IncompleteEntry {
                node: device,
                property: "reg",
                entry: 0,
                remaining_cells: values.len(),
                required_cells: address_cells,
            })?;
        Ok(Some(address.to_vec()))
    }

    fn interrupt_parent_available(&self, parent: NodeId) -> Result<bool, InterruptError> {
        let node = self
            .node(parent)
            .ok_or(InterruptError::InvalidNode(parent))?;
        let Some(property) = node.property("status") else {
            return Ok(true);
        };
        let status = property
            .as_str()
            .map_err(|error| InterruptError::InvalidProperty {
                node: parent,
                property: "status",
                error,
            })?;
        Ok(matches!(status, "ok" | "okay"))
    }

    fn is_interrupt_controller(&self, provider: NodeId) -> Result<bool, InterruptError> {
        let node = self
            .node(provider)
            .ok_or(InterruptError::InvalidNode(provider))?;
        let Some(property) = node.property("interrupt-controller") else {
            return Ok(false);
        };
        property
            .as_bool()
            .map_err(|error| InterruptError::InvalidProperty {
                node: provider,
                property: "interrupt-controller",
                error,
            })
    }

    fn required_interrupt_cells(&self, provider: NodeId) -> Result<usize, InterruptError> {
        let node = self
            .node(provider)
            .ok_or(InterruptError::InvalidNode(provider))?;
        let property = node
            .property("#interrupt-cells")
            .ok_or(InterruptError::MissingInterruptCells(provider))?;
        let cells = property
            .as_u32()
            .map_err(|error| InterruptError::InvalidProperty {
                node: provider,
                property: "#interrupt-cells",
                error,
            })?;
        validate_interrupt_cells(provider, cells)
    }
}

fn property_cells(
    node: NodeId,
    property_name: &'static str,
    property: Property<'_>,
) -> Result<Vec<u32>, InterruptError> {
    property
        .cells()
        .map(|cells| cells.collect())
        .map_err(|error| InterruptError::InvalidProperty {
            node,
            property: property_name,
            error,
        })
}

fn require_remaining(
    node: NodeId,
    property: &'static str,
    entry: usize,
    remaining: usize,
    required: usize,
) -> Result<(), InterruptError> {
    if remaining < required {
        Err(InterruptError::IncompleteEntry {
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

fn validate_interrupt_cells(provider: NodeId, cells: u32) -> Result<usize, InterruptError> {
    usize::try_from(cells).map_err(|_| InterruptError::InvalidInterruptCells { provider, cells })
}

fn validate_address_cells(nexus: NodeId, cells: u32) -> Result<usize, InterruptError> {
    usize::try_from(cells).map_err(|_| InterruptError::InvalidAddressCells { nexus, cells })
}

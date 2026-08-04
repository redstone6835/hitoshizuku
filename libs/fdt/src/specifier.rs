//! 通用 phandle、provider specifier 与 ID map 解码。
//!
//! clock、reset、DMA、IOMMU、GPIO、PHY 等 binding 使用相同的
//! `phandle + provider #*-cells` 编码。这里提供一次完整校验、原子返回的公共
//! 抽象；具体子系统只负责解释 `args` 的含义。

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

use crate::{NodeId, PropertyError, Tree};

/// phandle-array 中的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhandleArgs {
    /// phandle 为 0 的标准空槽没有 provider。
    pub provider: Option<NodeId>,
    /// 属性中的原始 phandle。
    pub phandle: u32,
    /// 按 provider `#*-cells` 保留的参数。
    pub args: Vec<u32>,
}

impl PhandleArgs {
    /// 是否为 phandle 0 编码的空槽。
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.provider.is_none()
    }
}

/// 带可选 `*-names` 名称的一项 provider specifier。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedPhandleArgs {
    pub name: Option<String>,
    pub specifier: PhandleArgs,
}

/// 通用 phandle-array 解码错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpecifierError {
    InvalidNode(NodeId),
    InvalidProperty {
        node: NodeId,
        property: String,
        error: PropertyError,
    },
    UnknownPhandle {
        node: NodeId,
        property: String,
        entry: usize,
        phandle: u32,
    },
    MissingProviderCells {
        provider: NodeId,
        property: String,
    },
    CellCountOverflow {
        provider: NodeId,
        property: String,
        count: u32,
    },
    IncompleteEntry {
        node: NodeId,
        property: String,
        entry: usize,
        remaining_cells: usize,
        required_cells: usize,
    },
    NameCountMismatch {
        node: NodeId,
        property: String,
        names: usize,
        entries: usize,
    },
}

impl fmt::Display for SpecifierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT provider specifier error: {self:?}")
    }
}

/// `iommu-map` 等 requester-ID map 中的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdMapEntry {
    pub input_base: u32,
    pub provider: NodeId,
    pub provider_phandle: u32,
    pub output_base: Vec<u32>,
    pub length: u32,
}

/// 已规范化的 requester-ID map。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdMap {
    pub mask: u32,
    pub entries: Vec<IdMapEntry>,
}

/// ID map 解码错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdMapError {
    InvalidNode(NodeId),
    InvalidProperty {
        node: NodeId,
        property: String,
        error: PropertyError,
    },
    UnknownPhandle {
        node: NodeId,
        property: String,
        entry: usize,
        phandle: u32,
    },
    MissingProviderCells {
        provider: NodeId,
        property: String,
    },
    CellCountOverflow {
        provider: NodeId,
        property: String,
        count: u32,
    },
    IncompleteEntry {
        node: NodeId,
        property: String,
        entry: usize,
        remaining_cells: usize,
        required_cells: usize,
    },
    InvalidRange {
        node: NodeId,
        property: String,
        entry: usize,
        input_base: u32,
        length: u32,
    },
    Overflow,
}

impl fmt::Display for IdMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT ID map error: {self:?}")
    }
}

impl Tree<'_> {
    /// 原子解码通用 phandle-array。provider 必须声明 `cells_property`。
    pub fn phandle_array(
        &self,
        node: NodeId,
        property: &str,
        cells_property: &str,
    ) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_array_inner(node, property, cells_property, None)
    }

    /// 解码允许 provider 省略 `cells_property` 的兼容 binding。
    pub fn phandle_array_with_default(
        &self,
        node: NodeId,
        property: &str,
        cells_property: &str,
        default_cells: u32,
    ) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_array_inner(node, property, cells_property, Some(default_cells))
    }

    /// 解码 phandle-only 列表，包括 phandle 0 空槽。
    #[inline]
    pub fn phandle_list(
        &self,
        node: NodeId,
        property: &str,
    ) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_array_inner(node, property, "", Some(0))
    }

    /// 解码 phandle-array，并严格关联可选的 `*-names` 字符串列表。
    pub fn named_phandle_array(
        &self,
        node: NodeId,
        property: &str,
        cells_property: &str,
        names_property: &str,
    ) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        let Some(entries) = self.phandle_array(node, property, cells_property)? else {
            return Ok(None);
        };
        let view = self.node(node).ok_or(SpecifierError::InvalidNode(node))?;
        let names = view
            .property(names_property)
            .map(|property| {
                property
                    .as_string_list()
                    .map(|names| names.map(String::from).collect::<Vec<_>>())
                    .map_err(|error| SpecifierError::InvalidProperty {
                        node,
                        property: names_property.to_string(),
                        error,
                    })
            })
            .transpose()?;
        if let Some(names) = names.as_ref()
            && names.len() != entries.len()
        {
            return Err(SpecifierError::NameCountMismatch {
                node,
                property: names_property.to_string(),
                names: names.len(),
                entries: entries.len(),
            });
        }
        Ok(Some(
            entries
                .into_iter()
                .enumerate()
                .map(|(index, specifier)| NamedPhandleArgs {
                    name: names.as_ref().map(|names| names[index].clone()),
                    specifier,
                })
                .collect(),
        ))
    }

    /// `clocks` / `clock-names`。
    pub fn clocks(&self, node: NodeId) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_phandle_array(node, "clocks", "#clock-cells", "clock-names")
    }

    /// `resets` / `reset-names`。
    pub fn resets(&self, node: NodeId) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_phandle_array(node, "resets", "#reset-cells", "reset-names")
    }

    /// `phys` / `phy-names`。
    pub fn phys(&self, node: NodeId) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_phandle_array(node, "phys", "#phy-cells", "phy-names")
    }

    /// `dmas` / `dma-names`。
    pub fn dmas(&self, node: NodeId) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_phandle_array(node, "dmas", "#dma-cells", "dma-names")
    }

    /// `mboxes` / `mbox-names`。
    pub fn mboxes(&self, node: NodeId) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_phandle_array(node, "mboxes", "#mbox-cells", "mbox-names")
    }

    /// `io-channels` / `io-channel-names`。
    pub fn io_channels(
        &self,
        node: NodeId,
    ) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_phandle_array(node, "io-channels", "#io-channel-cells", "io-channel-names")
    }

    /// `iommus`。
    pub fn iommus(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_array(node, "iommus", "#iommu-cells")
    }

    /// `power-domains`。
    pub fn power_domains(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_array(node, "power-domains", "#power-domain-cells")
    }

    /// `interconnects`。
    pub fn interconnects(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_array(node, "interconnects", "#interconnect-cells")
    }

    /// `pwms`。
    pub fn pwms(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_array(node, "pwms", "#pwm-cells")
    }

    /// `thermal-sensors`。
    pub fn thermal_sensors(
        &self,
        node: NodeId,
    ) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_array(node, "thermal-sensors", "#thermal-sensor-cells")
    }

    /// `sound-dai`。
    pub fn sound_dais(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_array(node, "sound-dai", "#sound-dai-cells")
    }

    /// `memory-region` phandle-only 列表。
    pub fn memory_regions(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_list(node, "memory-region")
    }

    /// `nvmem-cells` phandle-only 列表。
    pub fn nvmem_cells(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_list(node, "nvmem-cells")
    }

    /// `operating-points-v2` phandle-only 列表。
    pub fn operating_points(
        &self,
        node: NodeId,
    ) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_list(node, "operating-points-v2")
    }

    /// `interrupt-affinity` phandle-only 列表。
    pub fn interrupt_affinity(
        &self,
        node: NodeId,
    ) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_list(node, "interrupt-affinity")
    }

    /// 解码任意 `*-gpios` 属性。
    pub fn gpios(
        &self,
        node: NodeId,
        property: &str,
    ) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_array(node, property, "#gpio-cells")
    }

    /// 解码 `iommu-map[-mask]`。
    pub fn iommu_map(&self, node: NodeId) -> Result<Option<IdMap>, IdMapError> {
        self.id_map(node, "iommu-map", "#iommu-cells", "iommu-map-mask", None)
    }

    /// 原子解码 Linux `of_map_id()` 形式的通用 ID map。
    pub fn id_map(
        &self,
        node: NodeId,
        map_property: &str,
        cells_property: &str,
        mask_property: &str,
        default_cells: Option<u32>,
    ) -> Result<Option<IdMap>, IdMapError> {
        let view = self.node(node).ok_or(IdMapError::InvalidNode(node))?;
        let Some(property) = view.property(map_property) else {
            return Ok(None);
        };
        let values = property
            .cells()
            .map(|cells| cells.collect::<Vec<_>>())
            .map_err(|error| IdMapError::InvalidProperty {
                node,
                property: map_property.to_string(),
                error,
            })?;
        let mask = match view.property(mask_property) {
            None => u32::MAX,
            Some(property) => property
                .as_u32()
                .map_err(|error| IdMapError::InvalidProperty {
                    node,
                    property: mask_property.to_string(),
                    error,
                })?,
        };

        let mut entries = Vec::new();
        let mut offset = 0usize;
        let mut entry = 0usize;
        while offset < values.len() {
            if values.len() - offset < 2 {
                return Err(IdMapError::IncompleteEntry {
                    node,
                    property: map_property.to_string(),
                    entry,
                    remaining_cells: values.len() - offset,
                    required_cells: 2,
                });
            }
            let input_base = values[offset];
            let phandle = values[offset + 1];
            offset += 2;
            let provider = self
                .node_by_phandle(phandle)
                .ok_or(IdMapError::UnknownPhandle {
                    node,
                    property: map_property.to_string(),
                    entry,
                    phandle,
                })?;
            let cells = provider_cells_id_map(self, provider, cells_property, default_cells)?;
            let required = cells.checked_add(1).ok_or(IdMapError::Overflow)?;
            if values.len() - offset < required {
                return Err(IdMapError::IncompleteEntry {
                    node,
                    property: map_property.to_string(),
                    entry,
                    remaining_cells: values.len() - offset,
                    required_cells: required,
                });
            }
            let output_base = values[offset..offset + cells].to_vec();
            offset += cells;
            let length = values[offset];
            offset += 1;
            if input_base & !mask != 0
                || length == 0
                || input_base.checked_add(length - 1).is_none()
            {
                return Err(IdMapError::InvalidRange {
                    node,
                    property: map_property.to_string(),
                    entry,
                    input_base,
                    length,
                });
            }
            entries.push(IdMapEntry {
                input_base,
                provider,
                provider_phandle: phandle,
                output_base,
                length,
            });
            entry += 1;
        }
        Ok(Some(IdMap { mask, entries }))
    }

    fn phandle_array_inner(
        &self,
        node: NodeId,
        property_name: &str,
        cells_property: &str,
        default_cells: Option<u32>,
    ) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        let view = self.node(node).ok_or(SpecifierError::InvalidNode(node))?;
        let Some(property) = view.property(property_name) else {
            return Ok(None);
        };
        let values = property
            .cells()
            .map(|cells| cells.collect::<Vec<_>>())
            .map_err(|error| SpecifierError::InvalidProperty {
                node,
                property: property_name.to_string(),
                error,
            })?;
        let mut entries = Vec::new();
        let mut offset = 0usize;
        let mut entry = 0usize;
        while offset < values.len() {
            let phandle = values[offset];
            offset += 1;
            if phandle == 0 {
                entries.push(PhandleArgs {
                    provider: None,
                    phandle,
                    args: Vec::new(),
                });
                entry += 1;
                continue;
            }
            let provider = self
                .node_by_phandle(phandle)
                .ok_or(SpecifierError::UnknownPhandle {
                    node,
                    property: property_name.to_string(),
                    entry,
                    phandle,
                })?;
            let cells = provider_cells(self, provider, cells_property, default_cells)?;
            if values.len() - offset < cells {
                return Err(SpecifierError::IncompleteEntry {
                    node,
                    property: property_name.to_string(),
                    entry,
                    remaining_cells: values.len() - offset,
                    required_cells: cells,
                });
            }
            entries.push(PhandleArgs {
                provider: Some(provider),
                phandle,
                args: values[offset..offset + cells].to_vec(),
            });
            offset += cells;
            entry += 1;
        }
        Ok(Some(entries))
    }
}

fn provider_cells(
    tree: &Tree<'_>,
    provider: NodeId,
    property_name: &str,
    default: Option<u32>,
) -> Result<usize, SpecifierError> {
    let view = tree
        .node(provider)
        .ok_or(SpecifierError::InvalidNode(provider))?;
    let count = match view.property(property_name) {
        Some(property) => property
            .as_u32()
            .map_err(|error| SpecifierError::InvalidProperty {
                node: provider,
                property: property_name.to_string(),
                error,
            })?,
        None => default.ok_or(SpecifierError::MissingProviderCells {
            provider,
            property: property_name.to_string(),
        })?,
    };
    usize::try_from(count).map_err(|_| SpecifierError::CellCountOverflow {
        provider,
        property: property_name.to_string(),
        count,
    })
}

fn provider_cells_id_map(
    tree: &Tree<'_>,
    provider: NodeId,
    property_name: &str,
    default: Option<u32>,
) -> Result<usize, IdMapError> {
    let view = tree
        .node(provider)
        .ok_or(IdMapError::InvalidNode(provider))?;
    let count = match view.property(property_name) {
        Some(property) => property
            .as_u32()
            .map_err(|error| IdMapError::InvalidProperty {
                node: provider,
                property: property_name.to_string(),
                error,
            })?,
        None => default.ok_or(IdMapError::MissingProviderCells {
            provider,
            property: property_name.to_string(),
        })?,
    };
    usize::try_from(count).map_err(|_| IdMapError::CellCountOverflow {
        provider,
        property: property_name.to_string(),
        count,
    })
}

//! 通用 phandle、provider specifier 与 ID map 解码。
//!
//! clock、reset、DMA、IOMMU、GPIO、PHY 等 binding 使用相同的
//! `phandle + provider #*-cells` 编码。这里提供一次完整校验、原子返回的公共
//! 抽象；具体子系统只负责解释 `args` 的含义。

use alloc::{
    format,
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
    ArgumentCountMismatch {
        provider: NodeId,
        property: String,
        expected: usize,
        actual: usize,
    },
    NoMatchingMap {
        nexus: NodeId,
        property: String,
        args: Vec<u32>,
    },
    MapCycle {
        nexus: NodeId,
        property: String,
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

/// requester ID 经 [`IdMap`] 翻译后的 provider specifier。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappedId {
    pub provider: NodeId,
    pub provider_phandle: u32,
    pub args: Vec<u32>,
}

/// [`IdMap`] 中命中的原始条目及 requester ID 在区间内的偏移。
///
/// 该视图不解释 provider specifier 的 cell 编码，因而可用于实现
/// binding 自己定义的区间翻译规则。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdMapMatch<'a> {
    pub entry: &'a IdMapEntry,
    pub offset: u32,
}

/// requester-ID 命中后的 provider specifier 翻译错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdMapTranslationError {
    /// 单 cell specifier 加上区间偏移后溢出。
    OutputOverflow {
        provider: NodeId,
        provider_phandle: u32,
        output_base: u32,
        offset: u32,
    },
    /// 多 cell specifier 的区间算术没有通用 DTSpec 语义。
    AmbiguousMultiCellRange {
        provider: NodeId,
        provider_phandle: u32,
        cells: usize,
        length: u32,
        offset: u32,
    },
}

impl fmt::Display for IdMapTranslationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT ID map translation error: {self:?}")
    }
}

impl IdMap {
    /// 先应用 `*-map-mask`，再按半开区间查找第一个命中项。
    ///
    /// 返回值保留原始 [`IdMapEntry`] 及偏移，不猜测 provider
    /// specifier 的编码。无匹配项返回 `None`。
    pub fn match_id(&self, id: u32) -> Option<IdMapMatch<'_>> {
        let id = id & self.mask;
        self.entries.iter().find_map(|entry| {
            let offset = id.checked_sub(entry.input_base)?;
            if offset >= entry.length {
                return None;
            }
            Some(IdMapMatch { entry, offset })
        })
    }

    /// 按通用、无歧义的 cell 语义翻译 requester ID。
    ///
    /// 0-cell provider 对整个命中区间都返回空参数；单 cell
    /// provider 使用 checked addition；多 cell provider 仅在 `length == 1`
    /// 时原样返回。多 cell 区间没有通用的算术定义，调用方应改用
    /// [`Self::match_id`] 并按具体 binding 解释。
    ///
    /// 无匹配项返回 `Ok(None)`；该接口不内置 Linux `of_map_id()`
    /// 的 filter 或 bypass 策略。
    pub fn map_id(&self, id: u32) -> Result<Option<MappedId>, IdMapTranslationError> {
        let Some(matched) = self.match_id(id) else {
            return Ok(None);
        };
        let entry = matched.entry;
        let args = match entry.output_base.as_slice() {
            [] => Vec::new(),
            &[output_base] => alloc::vec![output_base.checked_add(matched.offset).ok_or(
                IdMapTranslationError::OutputOverflow {
                    provider: entry.provider,
                    provider_phandle: entry.provider_phandle,
                    output_base,
                    offset: matched.offset,
                },
            )?],
            _ if entry.length == 1 => entry.output_base.clone(),
            output_base => {
                return Err(IdMapTranslationError::AmbiguousMultiCellRange {
                    provider: entry.provider,
                    provider_phandle: entry.provider_phandle,
                    cells: output_base.len(),
                    length: entry.length,
                    offset: matched.offset,
                });
            }
        };
        Ok(Some(MappedId {
            provider: entry.provider,
            provider_phandle: entry.provider_phandle,
            args,
        }))
    }
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
        self.attach_names(node, names_property, entries).map(Some)
    }

    /// 原子解码 phandle-array，并按 DTSpec nexus `<stem>-map` 递归翻译。
    ///
    /// provider 的参数宽度由 `#<stem>-cells` 声明。phandle 0 空槽保持为空，
    /// 不参与 nexus 翻译。
    pub fn mapped_phandle_array(
        &self,
        node: NodeId,
        property: &str,
        stem: &str,
    ) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        let cells_property = format!("#{stem}-cells");
        let Some(entries) = self.phandle_array(node, property, &cells_property)? else {
            return Ok(None);
        };
        entries
            .iter()
            .map(|entry| self.resolve_phandle_args_map(entry, stem))
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }

    /// 解码并递归翻译 phandle-array，同时严格关联可选名称列表。
    pub fn named_mapped_phandle_array(
        &self,
        node: NodeId,
        property: &str,
        stem: &str,
        names_property: &str,
    ) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        let Some(entries) = self.mapped_phandle_array(node, property, stem)? else {
            return Ok(None);
        };
        self.attach_names(node, names_property, entries).map(Some)
    }

    /// 将单个 provider specifier 递归解析到不再声明 `<stem>-map` 的最终节点。
    ///
    /// 每一级 nexus map 都会先完整校验，再选择第一个匹配且可用的父
    /// provider；因此坏的未匹配行或表尾也会令整个解析原子失败。
    pub fn resolve_phandle_args_map(
        &self,
        specifier: &PhandleArgs,
        stem: &str,
    ) -> Result<PhandleArgs, SpecifierError> {
        let Some(_) = specifier.provider else {
            return Ok(specifier.clone());
        };

        let cells_property = format!("#{stem}-cells");
        let map_property = format!("{stem}-map");
        let mask_property = format!("{stem}-map-mask");
        let pass_property = format!("{stem}-map-pass-thru");
        let mut current = specifier.clone();
        let mut visited = Vec::new();

        loop {
            let provider = current
                .provider
                .expect("a nexus translation cannot turn into an empty slot");
            let cells = provider_cells(self, provider, &cells_property, None)?;
            if current.args.len() != cells {
                return Err(SpecifierError::ArgumentCountMismatch {
                    provider,
                    property: cells_property.clone(),
                    expected: cells,
                    actual: current.args.len(),
                });
            }
            if visited.contains(&provider) {
                return Err(SpecifierError::MapCycle {
                    nexus: provider,
                    property: map_property.clone(),
                });
            }
            visited.push(provider);

            let view = self
                .node(provider)
                .ok_or(SpecifierError::InvalidNode(provider))?;
            let Some(property) = view.property(&map_property) else {
                return Ok(current);
            };
            let values = property
                .cells()
                .map(|cells| cells.collect::<Vec<_>>())
                .map_err(|error| SpecifierError::InvalidProperty {
                    node: provider,
                    property: map_property.clone(),
                    error,
                })?;
            let mask = map_modifier(self, provider, &mask_property, cells, u32::MAX)?;
            let pass = map_modifier(self, provider, &pass_property, cells, 0)?;

            let mut matched = None;
            let mut offset = 0usize;
            let mut entry = 0usize;
            while offset < values.len() {
                let minimum = cells
                    .checked_add(1)
                    .ok_or(SpecifierError::CellCountOverflow {
                        provider,
                        property: cells_property.clone(),
                        count: u32::MAX,
                    })?;
                if values.len() - offset < minimum {
                    return Err(SpecifierError::IncompleteEntry {
                        node: provider,
                        property: map_property.clone(),
                        entry,
                        remaining_cells: values.len() - offset,
                        required_cells: minimum,
                    });
                }

                let child = &values[offset..offset + cells];
                let phandle = values[offset + cells];
                let parent =
                    self.node_by_phandle(phandle)
                        .ok_or(SpecifierError::UnknownPhandle {
                            node: provider,
                            property: map_property.clone(),
                            entry,
                            phandle,
                        })?;
                let parent_cells = provider_cells(self, parent, &cells_property, None)?;
                let row_cells =
                    minimum
                        .checked_add(parent_cells)
                        .ok_or(SpecifierError::CellCountOverflow {
                            provider: parent,
                            property: cells_property.clone(),
                            count: u32::MAX,
                        })?;
                if values.len() - offset < row_cells {
                    return Err(SpecifierError::IncompleteEntry {
                        node: provider,
                        property: map_property.clone(),
                        entry,
                        remaining_cells: values.len() - offset,
                        required_cells: row_cells,
                    });
                }
                let parent_args = &values[offset + minimum..offset + minimum + parent_cells];
                let parent_is_available = provider_available(self, parent)?;
                let is_match = matched.is_none()
                    && parent_is_available
                    && child
                        .iter()
                        .zip(&current.args)
                        .zip(&mask)
                        .all(|((&mapped, &actual), &mask)| (mapped ^ actual) & mask == 0);
                if is_match {
                    let mut args = parent_args.to_vec();
                    for index in 0..core::cmp::min(cells, parent_cells) {
                        args[index] =
                            (args[index] & !pass[index]) | (current.args[index] & pass[index]);
                    }
                    matched = Some(PhandleArgs {
                        provider: Some(parent),
                        phandle,
                        args,
                    });
                }

                offset += row_cells;
                entry += 1;
            }

            current = matched.ok_or_else(|| SpecifierError::NoMatchingMap {
                nexus: provider,
                property: map_property.clone(),
                args: current.args.clone(),
            })?;
        }
    }

    /// 解码 phandle-only 列表，并严格关联可选的 `*-names` 字符串列表。
    pub fn named_phandle_list(
        &self,
        node: NodeId,
        property: &str,
        names_property: &str,
    ) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        let Some(entries) = self.phandle_list(node, property)? else {
            return Ok(None);
        };
        self.attach_names(node, names_property, entries).map(Some)
    }

    fn attach_names(
        &self,
        node: NodeId,
        names_property: &str,
        entries: Vec<PhandleArgs>,
    ) -> Result<Vec<NamedPhandleArgs>, SpecifierError> {
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
        Ok(entries
            .into_iter()
            .enumerate()
            .map(|(index, specifier)| NamedPhandleArgs {
                name: names.as_ref().map(|names| names[index].clone()),
                specifier,
            })
            .collect())
    }

    /// `clocks` / `clock-names`。
    pub fn clocks(&self, node: NodeId) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_mapped_phandle_array(node, "clocks", "clock", "clock-names")
    }

    /// `resets` / `reset-names`。
    pub fn resets(&self, node: NodeId) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_mapped_phandle_array(node, "resets", "reset", "reset-names")
    }

    /// `phys` / `phy-names`。
    pub fn phys(&self, node: NodeId) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_mapped_phandle_array(node, "phys", "phy", "phy-names")
    }

    /// `dmas` / `dma-names`。
    pub fn dmas(&self, node: NodeId) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_mapped_phandle_array(node, "dmas", "dma", "dma-names")
    }

    /// `mboxes` / `mbox-names`。
    pub fn mboxes(&self, node: NodeId) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_mapped_phandle_array(node, "mboxes", "mbox", "mbox-names")
    }

    /// `io-channels` / `io-channel-names`。
    pub fn io_channels(
        &self,
        node: NodeId,
    ) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_mapped_phandle_array(node, "io-channels", "io-channel", "io-channel-names")
    }

    /// `iommus`。
    pub fn iommus(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.mapped_phandle_array(node, "iommus", "iommu")
    }

    /// `power-domains`。
    pub fn power_domains(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.mapped_phandle_array(node, "power-domains", "power-domain")
    }

    /// `interconnects`。
    pub fn interconnects(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.mapped_phandle_array(node, "interconnects", "interconnect")
    }

    /// `pwms`。
    pub fn pwms(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.mapped_phandle_array(node, "pwms", "pwm")
    }

    /// `thermal-sensors`。
    pub fn thermal_sensors(
        &self,
        node: NodeId,
    ) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.mapped_phandle_array(node, "thermal-sensors", "thermal-sensor")
    }

    /// `sound-dai`。
    pub fn sound_dais(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.mapped_phandle_array(node, "sound-dai", "sound-dai")
    }

    /// `memory-region` phandle-only 列表。
    pub fn memory_regions(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_list(node, "memory-region")
    }

    /// `memory-region` / `memory-region-names`。
    pub fn named_memory_regions(
        &self,
        node: NodeId,
    ) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_phandle_list(node, "memory-region", "memory-region-names")
    }

    /// `nvmem-cells` phandle-only 列表。
    pub fn nvmem_cells(&self, node: NodeId) -> Result<Option<Vec<PhandleArgs>>, SpecifierError> {
        self.phandle_list(node, "nvmem-cells")
    }

    /// `nvmem-cells` / `nvmem-cell-names`。
    pub fn named_nvmem_cells(
        &self,
        node: NodeId,
    ) -> Result<Option<Vec<NamedPhandleArgs>>, SpecifierError> {
        self.named_phandle_list(node, "nvmem-cells", "nvmem-cell-names")
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
        self.mapped_phandle_array(node, property, "gpio")
    }

    /// 解码 `iommu-map[-mask]`。
    pub fn iommu_map(&self, node: NodeId) -> Result<Option<IdMap>, IdMapError> {
        self.id_map(node, "iommu-map", "#iommu-cells", "iommu-map-mask", Some(1))
    }

    /// 原子解码 requester-ID map。
    ///
    /// 每项采用 `<input-base phandle output-specifier... length>`，输出宽度由目标
    /// provider 的 `cells_property` 决定。单 cell 表与 Linux `of_map_id()` 的四-cell
    /// ABI 完全一致；多 cell 表按最新 dt-schema 无损保留。空属性和不完整尾项均非法。
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
        if values.is_empty() {
            return Err(IdMapError::IncompleteEntry {
                node,
                property: map_property.to_string(),
                entry: 0,
                remaining_cells: 0,
                required_cells: 3,
            });
        }
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

fn map_modifier(
    tree: &Tree<'_>,
    node: NodeId,
    property_name: &str,
    cells: usize,
    default: u32,
) -> Result<Vec<u32>, SpecifierError> {
    let view = tree.node(node).ok_or(SpecifierError::InvalidNode(node))?;
    let Some(property) = view.property(property_name) else {
        return Ok(alloc::vec![default; cells]);
    };
    let values = property
        .cells()
        .map(|cells| cells.collect::<Vec<_>>())
        .map_err(|error| SpecifierError::InvalidProperty {
            node,
            property: property_name.to_string(),
            error,
        })?;
    if values.len() != cells {
        let expected = cells
            .checked_mul(4)
            .ok_or(SpecifierError::CellCountOverflow {
                provider: node,
                property: property_name.to_string(),
                count: u32::try_from(cells).unwrap_or(u32::MAX),
            })?;
        return Err(SpecifierError::InvalidProperty {
            node,
            property: property_name.to_string(),
            error: PropertyError::InvalidLength {
                actual: property.value().len(),
                expected: Some(expected),
            },
        });
    }
    Ok(values)
}

fn provider_available(tree: &Tree<'_>, provider: NodeId) -> Result<bool, SpecifierError> {
    let view = tree
        .node(provider)
        .ok_or(SpecifierError::InvalidNode(provider))?;
    let Some(property) = view.property("status") else {
        return Ok(true);
    };
    let status = property
        .as_str()
        .map_err(|error| SpecifierError::InvalidProperty {
            node: provider,
            property: "status".to_string(),
            error,
        })?;
    Ok(matches!(status, "ok" | "okay"))
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

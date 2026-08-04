//! Devicetree 内存描述的规范化语义层。
//!
//! 本模块只在 `alloc` feature 下提供。它保留 FDT 的稳定节点标识和完整
//! `u128` 地址宽度，把 `/memory`、`/chosen`、memory reservation block 与
//! `/reserved-memory` 分开表示；策略层可据启动协议决定如何组合这些来源。

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{fmt, str};

use crate::{AddressError, NodeId, Property, PropertyError, Tree, TreeError, decode_cells};

/// 根物理地址空间中的一段范围。
///
/// 地址和长度均使用 `u128`，因此所有规范允许的 0–4 个 cell 组合都不会在
/// 解析层发生平台相关的截断。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalRange {
    /// 根物理地址空间中的起始地址。
    pub address: u128,
    /// 范围长度（字节）。
    pub size: u128,
}

impl PhysicalRange {
    /// 返回半开区间末端；范围越过 `u128::MAX` 时返回 `None`。
    #[inline]
    pub const fn end(self) -> Option<u128> {
        self.address.checked_add(self.size)
    }

    /// 长度是否为零。
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.size == 0
    }
}

/// 一个可用内存节点及其规范化范围。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryBank {
    /// 对应 `/memory` 节点的稳定编号。
    pub node: NodeId,
    /// `linux,usable-memory`（若存在）或 `reg` 翻译到根地址空间后的范围。
    pub ranges: Vec<PhysicalRange>,
    /// 节点是否带有合法的空值 `hotpluggable` 属性。
    pub hotpluggable: bool,
}

/// `/reserved-memory` 子节点的放置方式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReservedMemoryPlacement {
    /// `reg` 指定的静态物理范围。
    Static(Vec<PhysicalRange>),
    /// 由操作系统完成的动态分配请求。
    Dynamic {
        /// 请求长度；按父节点 `#size-cells` 解码。
        size: u128,
        /// 可选的地址对齐；按父节点 `#size-cells` 解码。
        alignment: Option<u128>,
        /// 可选分配窗口；为空表示 binding 未限制可分配位置。
        alloc_ranges: Vec<PhysicalRange>,
    },
}

/// 一个 `/reserved-memory` 子节点的无损规范化描述。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservedMemory {
    /// 子节点稳定编号。
    pub node: NodeId,
    /// 子节点绝对路径。
    pub path: String,
    /// 去掉 `@unit-address` 后的节点名；DTSpec 规定该名称应反映用途。
    pub purpose: String,
    /// `phandle` 或兼容的 `linux,phandle`。
    pub phandle: Option<u32>,
    /// `compatible` 字符串列表，保持属性中的原始顺序。
    pub compatible: Vec<String>,
    /// 静态范围或尚待满足的动态分配约束。
    pub placement: ReservedMemoryPlacement,
    /// 是否禁止把该范围纳入操作系统的标准内存映射。
    pub no_map: bool,
    /// 是否允许操作系统临时复用该范围。
    pub reusable: bool,
}

/// DTB 中所有内存来源的规范化描述。
///
/// 本结构不会自行执行范围裁剪、合并或动态保留区分配，因而不会丢失固件输入；
/// 例如 UEFI 启动路径可以按规范忽略 `memory_banks`，但仍消费两类保留范围。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryDescription {
    /// 根节点直接子节点中合法且启用的 `device_type = "memory"` 节点。
    pub memory_banks: Vec<MemoryBank>,
    /// `/chosen/linux,usable-memory-range` 中的根物理范围。
    pub chosen_usable_ranges: Vec<PhysicalRange>,
    /// FDT memory reservation block 中的原始顺序范围。
    pub reservation_block_ranges: Vec<PhysicalRange>,
    /// `/reserved-memory` 的启用直接子节点。
    pub reserved_memory: Vec<ReservedMemory>,
}

/// 设备树内存 binding 的结构化语义错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemoryError {
    /// 节点状态等索引树语义不合法。
    InvalidTree(TreeError),
    /// 地址 cell、元组布局或普通总线翻译不合法。
    InvalidAddress(AddressError),
    /// DTSpec 要求的属性缺失。
    MissingProperty {
        /// 所属节点。
        node: NodeId,
        /// 缺失属性名。
        property: &'static str,
    },
    /// 属性存在，但 binding 至少要求一个值或一个范围元组。
    EmptyProperty {
        /// 所属节点。
        node: NodeId,
        /// 空属性名。
        property: &'static str,
    },
    /// 属性不符合 binding 规定的标量、字符串或布尔编码。
    InvalidProperty {
        /// 所属节点。
        node: NodeId,
        /// 属性名。
        property: &'static str,
        /// 具体解码错误。
        error: PropertyError,
    },
    /// 需要物理长度的地址元组由 `#size-cells = <0>` 编码，无法形成范围。
    MissingRangeSize {
        /// 所属节点。
        node: NodeId,
        /// 使用 `reg` 布局的属性名。
        property: &'static str,
    },
    /// 标量 binding 不允许零值。
    ZeroValue {
        /// 所属节点。
        node: NodeId,
        /// 属性名。
        property: &'static str,
    },
    /// 地址范围的长度为零。
    ZeroSizedRange {
        /// 所属节点；`None` 表示 memory reservation block。
        node: Option<NodeId>,
        /// 范围来源属性或块名称。
        property: &'static str,
        /// 属性或 reservation block 内的范围序号。
        index: usize,
        /// 范围起始地址。
        address: u128,
    },
    /// 地址与长度之和超出 `u128`，无法形成规范半开区间。
    RangeOverflow {
        /// 所属节点；`None` 表示 memory reservation block。
        node: Option<NodeId>,
        /// 范围来源属性或块名称。
        property: &'static str,
        /// 属性或 reservation block 内的范围序号。
        index: usize,
        /// 范围起始地址。
        address: u128,
        /// 范围长度。
        size: u128,
    },
    /// `/reserved-memory` 的 cell 数与根节点不一致。
    MismatchedCellCount {
        /// `/reserved-memory` 节点。
        node: NodeId,
        /// `#address-cells` 或 `#size-cells`。
        property: &'static str,
        /// 子节点声明值。
        declared: u32,
        /// 根节点声明值（或规范默认值）。
        expected: u32,
    },
    /// 具有 `device_type = "memory"` 的节点未使用规范 `memory` unit-name。
    InvalidMemoryUnitName {
        /// 所属节点。
        node: NodeId,
        /// 实际节点名。
        name: String,
    },
    /// 同一 reserved-memory 节点同时声明了互斥属性。
    MutuallyExclusiveProperties {
        /// 所属节点。
        node: NodeId,
        /// 第一个属性名。
        first: &'static str,
        /// 第二个属性名。
        second: &'static str,
    },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT memory error: {self:?}")
    }
}

impl From<TreeError> for MemoryError {
    #[inline]
    fn from(value: TreeError) -> Self {
        Self::InvalidTree(value)
    }
}

impl From<AddressError> for MemoryError {
    #[inline]
    fn from(value: AddressError) -> Self {
        Self::InvalidAddress(value)
    }
}

impl Tree<'_> {
    /// 按 DTSpec 和 Linux 通用 binding 解析完整内存描述。
    ///
    /// 仅根节点直接子节点中 `device_type` 精确为 `memory` 的可用节点会成为
    /// [`MemoryBank`]。`linux,usable-memory` 存在时优先于 `reg`；reserved-memory
    /// 子节点中 `reg` 同样优先于动态 `size` 请求。
    pub fn memory_description(&self) -> Result<MemoryDescription, MemoryError> {
        Ok(MemoryDescription {
            memory_banks: self.memory_banks()?,
            chosen_usable_ranges: self.chosen_usable_ranges()?,
            reservation_block_ranges: self.reservation_block_ranges()?,
            reserved_memory: self.reserved_memory_nodes()?,
        })
    }

    fn memory_banks(&self) -> Result<Vec<MemoryBank>, MemoryError> {
        let mut banks = Vec::new();
        for &node_id in self
            .children(self.root_id())
            .expect("a validated tree always contains its root")
        {
            let node = self
                .node(node_id)
                .expect("an indexed child node always exists");
            let Some(device_type) = node.property("device_type") else {
                continue;
            };
            let device_type =
                device_type
                    .as_str()
                    .map_err(|error| MemoryError::InvalidProperty {
                        node: node_id,
                        property: "device_type",
                        error,
                    })?;
            if device_type != "memory" {
                continue;
            }
            if !self.is_effectively_available(node_id)? {
                continue;
            }
            if node.base_name_bytes() != b"memory" {
                return Err(MemoryError::InvalidMemoryUnitName {
                    node: node_id,
                    name: node.name().to_string(),
                });
            }

            let property = if node.property("linux,usable-memory").is_some() {
                "linux,usable-memory"
            } else if node.property("reg").is_some() {
                "reg"
            } else {
                return Err(MemoryError::MissingProperty {
                    node: node_id,
                    property: "reg",
                });
            };
            let ranges = self.physical_ranges(node_id, property)?;
            let hotpluggable = self.boolean_property(node_id, "hotpluggable")?;
            banks.push(MemoryBank {
                node: node_id,
                ranges,
                hotpluggable,
            });
        }
        Ok(banks)
    }

    fn chosen_usable_ranges(&self) -> Result<Vec<PhysicalRange>, MemoryError> {
        let Some(chosen) = self
            .find_node("/chosen")
            .or_else(|| self.find_node("/chosen@0"))
        else {
            return Ok(Vec::new());
        };
        if self
            .node(chosen)
            .expect("an indexed chosen node always exists")
            .property("linux,usable-memory-range")
            .is_none()
        {
            return Ok(Vec::new());
        }
        self.physical_ranges(chosen, "linux,usable-memory-range")
    }

    fn reserved_memory_nodes(&self) -> Result<Vec<ReservedMemory>, MemoryError> {
        let Some(parent) = self.find_node("/reserved-memory") else {
            return Ok(Vec::new());
        };
        if !self.is_effectively_available(parent)? {
            return Ok(Vec::new());
        }
        self.validate_reserved_memory_parent(parent)?;

        let mut result = Vec::new();
        for &node_id in self
            .children(parent)
            .expect("an indexed reserved-memory node always exists")
        {
            if !self.is_effectively_available(node_id)? {
                continue;
            }
            let node = self
                .node(node_id)
                .expect("an indexed reserved-memory child always exists");
            let no_map = self.boolean_property(node_id, "no-map")?;
            let reusable = self.boolean_property(node_id, "reusable")?;
            if no_map && reusable {
                return Err(MemoryError::MutuallyExclusiveProperties {
                    node: node_id,
                    first: "no-map",
                    second: "reusable",
                });
            }

            let placement = if node.property("reg").is_some() {
                // Linux 与 libfdt 以 `reg` 是否存在区分静态/动态保留区；缺少
                // unit-address 只属于 dtc 命名诊断。部分已部署固件（包括
                // LS2K1000-DP-FACTORY）使用 `framebuffer { reg; no-map; }`，不能
                // 因节点名不规范而丢弃一段必须保留的物理内存。
                ReservedMemoryPlacement::Static(self.physical_ranges(node_id, "reg")?)
            } else {
                let size = self.required_size_scalar(node_id, parent, "size")?;
                if size == 0 {
                    return Err(MemoryError::ZeroValue {
                        node: node_id,
                        property: "size",
                    });
                }
                let alignment = node
                    .property("alignment")
                    .map(|property| self.size_scalar(node_id, parent, "alignment", property))
                    .transpose()?;
                let alloc_ranges = if node.property("alloc-ranges").is_some() {
                    self.physical_ranges(node_id, "alloc-ranges")?
                } else {
                    Vec::new()
                };
                ReservedMemoryPlacement::Dynamic {
                    size,
                    alignment,
                    alloc_ranges,
                }
            };

            let compatible = node
                .property("compatible")
                .map(|property| {
                    property
                        .as_string_list()
                        .map(|values| values.map(ToString::to_string).collect())
                        .map_err(|error| MemoryError::InvalidProperty {
                            node: node_id,
                            property: "compatible",
                            error,
                        })
                })
                .transpose()?
                .unwrap_or_default();
            let purpose = str::from_utf8(node.base_name_bytes())
                .expect("validated node names are ASCII")
                .to_string();
            result.push(ReservedMemory {
                node: node_id,
                path: self
                    .path(node_id)
                    .expect("an indexed reserved-memory child has a path"),
                purpose,
                phandle: self.phandle(node_id),
                compatible,
                placement,
                no_map,
                reusable,
            });
        }
        Ok(result)
    }

    fn physical_ranges(
        &self,
        node: NodeId,
        property: &'static str,
    ) -> Result<Vec<PhysicalRange>, MemoryError> {
        let decoded = self.translated_reg_property(node, property)?;
        if decoded.is_empty() {
            return Err(MemoryError::EmptyProperty { node, property });
        }
        decoded
            .into_iter()
            .enumerate()
            .map(|(index, range)| {
                let physical = PhysicalRange {
                    address: range.address,
                    size: range
                        .size
                        .ok_or(MemoryError::MissingRangeSize { node, property })?,
                };
                validate_physical_range(Some(node), property, index, physical)?;
                Ok(physical)
            })
            .collect()
    }

    fn reservation_block_ranges(&self) -> Result<Vec<PhysicalRange>, MemoryError> {
        self.fdt()
            .reservations()
            .enumerate()
            .map(|(index, entry)| {
                let range = PhysicalRange {
                    address: u128::from(entry.address),
                    size: u128::from(entry.size),
                };
                validate_physical_range(None, "memory-reservation-block", index, range)?;
                Ok(range)
            })
            .collect()
    }

    fn validate_reserved_memory_parent(&self, node: NodeId) -> Result<(), MemoryError> {
        let parent = self
            .node(node)
            .expect("an indexed reserved-memory node always exists");
        for property in ["#address-cells", "#size-cells", "ranges"] {
            if parent.property(property).is_none() {
                return Err(MemoryError::MissingProperty { node, property });
            }
        }

        let root = self.root_id();
        let address_cells = self.address_cells(node)?;
        let root_address_cells = self.address_cells(root)?;
        if address_cells != root_address_cells {
            return Err(MemoryError::MismatchedCellCount {
                node,
                property: "#address-cells",
                declared: address_cells,
                expected: root_address_cells,
            });
        }
        let size_cells = self.size_cells(node)?;
        let root_size_cells = self.size_cells(root)?;
        if size_cells != root_size_cells {
            return Err(MemoryError::MismatchedCellCount {
                node,
                property: "#size-cells",
                declared: size_cells,
                expected: root_size_cells,
            });
        }

        // 即使所有子节点都是纯动态请求，也要立即校验 required `ranges` 的布局，
        // 避免是否访问 `alloc-ranges` 改变父节点格式错误的可见性。
        self.ranges(node)?;
        Ok(())
    }

    fn required_size_scalar(
        &self,
        node: NodeId,
        parent: NodeId,
        property_name: &'static str,
    ) -> Result<u128, MemoryError> {
        let property = self
            .node(node)
            .expect("an indexed node always exists")
            .property(property_name)
            .ok_or(MemoryError::MissingProperty {
                node,
                property: property_name,
            })?;
        self.size_scalar(node, parent, property_name, property)
    }

    fn size_scalar(
        &self,
        node: NodeId,
        parent: NodeId,
        property_name: &'static str,
        property: Property<'_>,
    ) -> Result<u128, MemoryError> {
        let cells = self.size_cells(parent)? as usize;
        decode_cells(property.value(), cells).map_err(|error| MemoryError::InvalidProperty {
            node,
            property: property_name,
            error,
        })
    }

    fn boolean_property(
        &self,
        node: NodeId,
        property_name: &'static str,
    ) -> Result<bool, MemoryError> {
        let node_view = self.node(node).expect("an indexed node always exists");
        let Some(property) = node_view.property(property_name) else {
            return Ok(false);
        };
        property
            .as_bool()
            .map_err(|error| MemoryError::InvalidProperty {
                node,
                property: property_name,
                error,
            })
    }

    fn is_effectively_available(&self, node: NodeId) -> Result<bool, MemoryError> {
        let mut current = Some(node);
        while let Some(node_id) = current {
            if !self.is_available(node_id)? {
                return Ok(false);
            }
            current = self.parent(node_id);
        }
        Ok(true)
    }
}

fn validate_physical_range(
    node: Option<NodeId>,
    property: &'static str,
    index: usize,
    range: PhysicalRange,
) -> Result<(), MemoryError> {
    if range.size == 0 {
        return Err(MemoryError::ZeroSizedRange {
            node,
            property,
            index,
            address: range.address,
        });
    }
    if range.end().is_none() {
        return Err(MemoryError::RangeOverflow {
            node,
            property,
            index,
            address: range.address,
            size: range.size,
        });
    }
    Ok(())
}

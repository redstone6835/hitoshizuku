//! 需要 `alloc` 的索引化设备树与通用地址语义。

use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

use crate::{Error, Fdt, Node, Property, PropertyError};

/// 一棵 [`Tree`] 内稳定的节点编号。
///
/// 编号按 structure block 的先序顺序分配；只要输入 blob 不变，编号就不变。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct NodeId(usize);

impl NodeId {
    /// 返回从零开始的稳定索引。
    #[inline]
    pub const fn index(self) -> usize {
        self.0
    }
}

/// 索引树构造或 `/chosen/stdout-path` 解析错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TreeError {
    /// 底层 blob 不合法。
    InvalidFdt(Error),
    /// 两个节点具有相同绝对路径。
    DuplicatePath(String),
    /// 节点编号不属于当前树。
    InvalidNode(NodeId),
    /// phandle 属性不是一个合法大端 `u32`。
    InvalidPhandle {
        /// 所属节点。
        node: NodeId,
        /// 属性名。
        property: &'static str,
        /// 具体解码错误。
        error: PropertyError,
    },
    /// phandle 使用了规范保留值 0 或 `0xffff_ffff`。
    ReservedPhandle {
        /// 所属节点。
        node: NodeId,
        /// 非法值。
        value: u32,
    },
    /// 同一节点的 `phandle` 与 `linux,phandle` 不一致。
    ConflictingPhandle {
        /// 所属节点。
        node: NodeId,
        /// `phandle` 值。
        phandle: u32,
        /// `linux,phandle` 值。
        linux_phandle: u32,
    },
    /// 多个节点使用同一个 phandle。
    DuplicatePhandle {
        /// 重复值。
        value: u32,
        /// 首个节点。
        first: NodeId,
        /// 后续节点。
        second: NodeId,
    },
    /// `status` 不是规范字符串。
    InvalidStatus {
        /// 所属节点。
        node: NodeId,
        /// 具体解码错误。
        error: PropertyError,
    },
    /// chosen 控制台属性不是规范字符串。
    InvalidChosen(PropertyError),
    /// chosen 控制台路径或 alias 无法解析。
    UnresolvedChosen(String),
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT tree error: {self:?}")
    }
}

impl From<Error> for TreeError {
    fn from(value: Error) -> Self {
        Self::InvalidFdt(value)
    }
}

/// 节点 `status` 属性的规范化结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeStatus<'a> {
    /// 没有 `status` 属性。
    Absent,
    /// `okay` 或历史兼容拼写 `ok`。
    Okay,
    /// `disabled`。
    Disabled,
    /// `reserved`。
    Reserved,
    /// `fail` 或带原因后缀的 `fail-...`。
    Fail(&'a str),
    /// binding 定义的其他值。
    Other(&'a str),
}

impl NodeStatus<'_> {
    /// 与 Linux `of_device_is_available()` 一致的可用性判断。
    #[inline]
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Absent | Self::Okay)
    }
}

/// `reg` 中的一项。地址保持为无损的 `u128`，到平台边界再收窄。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegEntry {
    /// 父总线地址空间中的地址。
    pub address: u128,
    /// `#size-cells = <0>` 时为 `None`。
    pub size: Option<u128>,
}

/// 普通 `ranges` 中的一项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeMapping {
    /// 子总线地址空间起点。
    pub child_address: u128,
    /// 父总线地址空间起点。
    pub parent_address: u128,
    /// 映射窗口长度；`#size-cells = <0>` 时该字段从属性中省略。
    pub size: Option<u128>,
}

/// 已翻译地址及其长度。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddressRange {
    /// 根地址空间中的地址。
    pub address: u128,
    /// `#size-cells = <0>` 时为 `None`。
    pub size: Option<u128>,
}

/// `reg` / `ranges` 解码及普通总线地址翻译错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AddressError {
    /// 节点编号不属于当前树。
    InvalidNode(NodeId),
    /// 需要父总线的节点没有父节点。
    MissingParent(NodeId),
    /// cell 属性或地址数组格式错误。
    InvalidProperty {
        /// 所属节点。
        node: NodeId,
        /// 属性名。
        property: &'static str,
        /// 具体格式错误。
        error: PropertyError,
    },
    /// cell 数超出 `u128` 能无损表达的范围。
    UnsupportedCellCount {
        /// 定义 cell 数的总线节点。
        node: NodeId,
        /// 属性名。
        property: &'static str,
        /// 声明值。
        count: u32,
    },
    /// 数组长度不是一项所需 cell 数的整数倍。
    IncompleteEntry {
        /// 所属节点。
        node: NodeId,
        /// `reg` 或 `ranges`。
        property: &'static str,
        /// 实际 cell 数。
        cells: usize,
        /// 每项 cell 数。
        cells_per_entry: usize,
    },
    /// 总线未提供 `ranges`，不能假定为恒等映射。
    MissingRanges(NodeId),
    /// 地址不属于该总线的任何 `ranges` 窗口。
    UnmappedAddress {
        /// 总线节点。
        bus: NodeId,
        /// 当前总线地址。
        address: u128,
        /// 资源长度。
        size: Option<u128>,
    },
    /// 地址范围不能由当前总线声明的 `#address-cells` 无损表达。
    AddressOutOfRange {
        /// 地址所属的总线地址空间。
        bus: NodeId,
        /// 无法表达的地址。
        address: u128,
        /// 需要完整落在该地址空间中的范围长度。
        size: Option<u128>,
        /// 总线声明的地址 cell 数。
        cells: u32,
    },
    /// 地址加法或窗口边界计算溢出 `u128`。
    Overflow,
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT address error: {self:?}")
    }
}

/// `/chosen/stdout-path` 的规范化结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChosenStdout<'a> {
    /// 属性中的完整原始字符串（不含最后的 NUL）。
    pub raw: &'a str,
    /// alias 展开后的绝对节点路径。
    pub path: &'a str,
    /// 目标节点。
    pub node: NodeId,
    /// 第一个 `:` 后的串口选项，内容原样保留。
    pub options: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
struct Alias<'a> {
    path: &'a str,
    target: NodeId,
}

#[derive(Debug)]
struct NodeRecord<'a> {
    node: Node<'a>,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
}

/// 带稳定索引和常用 Devicetree 语义的树。
#[derive(Debug)]
pub struct Tree<'a> {
    fdt: Fdt<'a>,
    nodes: Vec<NodeRecord<'a>>,
    aliases: BTreeMap<&'a str, Alias<'a>>,
    phandles: BTreeMap<u32, NodeId>,
    node_phandles: BTreeMap<NodeId, u32>,
}

impl<'a> Tree<'a> {
    /// 校验 blob 并构建索引。
    #[inline]
    pub fn parse(bytes: &'a [u8]) -> Result<Self, TreeError> {
        Self::from_fdt(Fdt::parse(bytes)?)
    }

    /// 从已经校验的借用视图构建索引。
    pub fn from_fdt(fdt: Fdt<'a>) -> Result<Self, TreeError> {
        let mut nodes: Vec<NodeRecord<'a>> = Vec::new();
        let mut ancestors: Vec<NodeId> = Vec::new();

        for node in fdt.nodes() {
            let depth = node.depth();
            ancestors.truncate(depth);
            let parent = ancestors.last().copied();

            let id = NodeId(nodes.len());
            nodes.push(NodeRecord {
                node,
                parent,
                children: Vec::new(),
            });
            if let Some(parent) = parent {
                nodes[parent.index()].children.push(id);
            }
            ancestors.push(id);
        }

        let mut tree = Self {
            fdt,
            nodes,
            aliases: BTreeMap::new(),
            phandles: BTreeMap::new(),
            node_phandles: BTreeMap::new(),
        };
        tree.validate_names_and_legacy_paths()?;
        tree.index_phandles()?;
        tree.index_aliases();
        Ok(tree)
    }

    /// 返回底层无分配视图。
    #[inline]
    pub const fn fdt(&self) -> Fdt<'a> {
        self.fdt
    }

    /// 节点总数。
    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// 树是否为空。合法 FDT 恒为 `false`。
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 根节点编号。
    #[inline]
    pub const fn root_id(&self) -> NodeId {
        NodeId(0)
    }

    /// 取得借用节点视图。
    #[inline]
    pub fn node(&self, id: NodeId) -> Option<Node<'a>> {
        self.nodes.get(id.index()).map(|record| record.node)
    }

    /// 按需构造节点绝对路径。
    ///
    /// 索引不会为每个节点永久保存完整祖先路径，避免深层合法设备树产生二次方
    /// 内存占用。调用方确实需要展示或持久化路径时再承担与路径长度线性相关的分配。
    pub fn path(&self, id: NodeId) -> Option<String> {
        self.nodes.get(id.index())?;
        if id == self.root_id() {
            return Some("/".to_string());
        }

        let mut components = Vec::new();
        let mut current = Some(id);
        let mut length = 0usize;
        while let Some(node_id) = current {
            if node_id == self.root_id() {
                break;
            }
            let record = self.nodes.get(node_id.index())?;
            length = length.checked_add(record.node.name().len() + 1)?;
            components.push(record.node.name());
            current = record.parent;
        }

        let mut path = String::with_capacity(length);
        for component in components.into_iter().rev() {
            path.push('/');
            path.push_str(component);
        }
        Some(path)
    }

    /// 按绝对路径查找节点编号。
    ///
    /// 路径组件包含 unit-address 时执行完整名称匹配；省略 `@unit-address`
    /// 时，仅在同名基础名称无歧义时匹配。
    pub fn find_node(&self, path: &str) -> Option<NodeId> {
        if path == "/" {
            return Some(self.root_id());
        }
        let relative = path.strip_prefix('/')?;
        if relative.is_empty() || relative.ends_with('/') {
            return None;
        }

        let mut current = self.root_id();
        for component in relative.split('/') {
            if component.is_empty() {
                return None;
            }
            let children = self.children(current)?;
            if let Some(exact) = children
                .iter()
                .copied()
                .find(|&child| self.nodes[child.index()].node.name() == component)
            {
                current = exact;
                continue;
            }
            if component.contains('@') {
                return None;
            }
            let mut candidates = children.iter().copied().filter(|&child| {
                self.nodes[child.index()].node.base_name_bytes() == component.as_bytes()
            });
            current = candidates.next()?;
            if candidates.next().is_some() {
                return None;
            }
        }
        Some(current)
    }

    /// 返回父节点编号。
    #[inline]
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes.get(id.index()).and_then(|record| record.parent)
    }

    /// 返回直接子节点编号。
    #[inline]
    pub fn children(&self, id: NodeId) -> Option<&[NodeId]> {
        self.nodes
            .get(id.index())
            .map(|record| record.children.as_slice())
    }

    /// 按稳定编号顺序迭代所有节点编号。
    #[inline]
    pub fn node_ids(&self) -> impl ExactSizeIterator<Item = NodeId> + '_ {
        (0..self.nodes.len()).map(NodeId)
    }

    /// 解析绝对路径或 `/aliases` 中的 alias。
    ///
    /// 该接口不处理 `:options`；这是 chosen stdout binding 的专属语法。
    pub fn resolve_path_or_alias(&self, value: &str) -> Option<NodeId> {
        if value.starts_with('/') {
            self.find_node(value)
        } else {
            self.aliases.get(value).map(|alias| alias.target)
        }
    }

    /// 返回 alias 展开后的绝对路径。
    #[inline]
    pub fn alias_path(&self, name: &str) -> Option<&'a str> {
        self.aliases.get(name).map(|alias| alias.path)
    }

    /// 按 phandle 查找节点；0 和 `0xffff_ffff` 永远无效。
    #[inline]
    pub fn node_by_phandle(&self, phandle: u32) -> Option<NodeId> {
        self.phandles.get(&phandle).copied()
    }

    /// 返回节点规范 phandle（含 `linux,phandle` 兼容形式）。
    pub fn phandle(&self, id: NodeId) -> Option<u32> {
        self.node_phandles.get(&id).copied()
    }

    /// 解码节点的 `status`。
    pub fn status(&self, id: NodeId) -> Result<NodeStatus<'a>, TreeError> {
        let node = self.node(id).ok_or(TreeError::InvalidNode(id))?;
        let Some(property) = node.property("status") else {
            return Ok(NodeStatus::Absent);
        };
        let value = property
            .as_str()
            .map_err(|error| TreeError::InvalidStatus { node: id, error })?;
        Ok(match value {
            "ok" | "okay" => NodeStatus::Okay,
            "disabled" => NodeStatus::Disabled,
            "reserved" => NodeStatus::Reserved,
            value if value == "fail" || value.starts_with("fail-") => NodeStatus::Fail(value),
            value => NodeStatus::Other(value),
        })
    }

    /// 与 Linux `of_device_is_available()` 一致地判断节点是否可用。
    #[inline]
    pub fn is_available(&self, id: NodeId) -> Result<bool, TreeError> {
        self.status(id).map(NodeStatus::is_available)
    }

    /// 返回该总线用于其直接子节点地址的 `#address-cells`。
    pub fn address_cells(&self, bus: NodeId) -> Result<u32, AddressError> {
        self.cell_count(bus, "#address-cells", 2)
    }

    /// 返回该总线用于其直接子节点长度的 `#size-cells`。
    pub fn size_cells(&self, bus: NodeId) -> Result<u32, AddressError> {
        self.cell_count(bus, "#size-cells", 1)
    }

    /// 按父总线定义的 cell 数解码节点上具有 `reg` 布局的属性。
    ///
    /// `property_name` 必须由相应 binding 明确指定；本接口只复用 `reg` 的
    /// `(address, size)` 元组布局，不会根据属性长度猜测类型。属性缺失时返回空列表。
    pub fn reg_property(
        &self,
        id: NodeId,
        property_name: &'static str,
    ) -> Result<Vec<RegEntry>, AddressError> {
        let node = self.node_or_error(id)?;
        let Some(property) = node.property(property_name) else {
            return Ok(Vec::new());
        };
        let parent = self.parent(id).ok_or(AddressError::MissingParent(id))?;
        let address_cells = self.address_cells(parent)?;
        let size_cells = self.size_cells(parent)?;
        let entries = self.decode_reg(id, property_name, property, address_cells, size_cells)?;
        for entry in &entries {
            self.ensure_range_fits(parent, entry.address, entry.size)?;
        }
        Ok(entries)
    }

    /// 按父总线定义的 cell 数解码节点 `reg`。
    #[inline]
    pub fn reg(&self, id: NodeId) -> Result<Vec<RegEntry>, AddressError> {
        self.reg_property(id, "reg")
    }

    /// 解码总线的普通 `ranges`。
    ///
    /// `None` 表示缺少属性，不能推断映射；`Some([])` 表示规范规定的恒等映射。
    /// PCI、ISA 等在高位 cell 编码空间/标志的总线应由相应 binding 层解释，
    /// 不应直接使用这个普通数值映射接口。
    pub fn ranges(&self, bus: NodeId) -> Result<Option<Vec<RangeMapping>>, AddressError> {
        let node = self.node_or_error(bus)?;
        let Some(property) = node.property("ranges") else {
            return Ok(None);
        };
        let parent = self.parent(bus).ok_or(AddressError::MissingParent(bus))?;
        if property.value().is_empty() {
            return Ok(Some(Vec::new()));
        }

        let child_cells = self.address_cells(bus)?;
        let parent_cells = self.address_cells(parent)?;
        let size_cells = self.size_cells(bus)?;
        let stride = cell_stride(child_cells, parent_cells, size_cells).ok_or(
            AddressError::UnsupportedCellCount {
                node: bus,
                property: "ranges",
                count: child_cells.max(parent_cells).max(size_cells),
            },
        )?;
        let total_cells = checked_cells(bus, "ranges", property)?;
        if stride == 0 || !total_cells.is_multiple_of(stride) {
            return Err(AddressError::IncompleteEntry {
                node: bus,
                property: "ranges",
                cells: total_cells,
                cells_per_entry: stride,
            });
        }

        let mut cells = property
            .cells()
            .map_err(|error| AddressError::InvalidProperty {
                node: bus,
                property: "ranges",
                error,
            })?;
        let mut result = Vec::with_capacity(total_cells / stride);
        while cells.remaining() != 0 {
            let child_address = read_cell_value(&mut cells, child_cells, bus, "ranges")?;
            let parent_address = read_cell_value(&mut cells, parent_cells, bus, "ranges")?;
            let size = if size_cells == 0 {
                None
            } else {
                Some(read_cell_value(&mut cells, size_cells, bus, "ranges")?)
            };
            self.ensure_range_fits(bus, child_address, size)?;
            self.ensure_range_fits(parent, parent_address, size)?;
            result.push(RangeMapping {
                child_address,
                parent_address,
                size,
            });
        }
        Ok(Some(result))
    }

    /// 从 `bus` 地址空间逐级翻译到根地址空间。
    pub fn translate_address(
        &self,
        mut bus: NodeId,
        mut address: u128,
        size: Option<u128>,
    ) -> Result<u128, AddressError> {
        self.node_or_error(bus)?;
        self.ensure_range_fits(bus, address, size)?;
        while let Some(parent) = self.parent(bus) {
            let mappings = self.ranges(bus)?.ok_or(AddressError::MissingRanges(bus))?;
            if !mappings.is_empty() {
                let mapping = mappings
                    .iter()
                    .find(|mapping| mapping_contains(**mapping, address, size))
                    .ok_or(AddressError::UnmappedAddress { bus, address, size })?;
                let delta = address
                    .checked_sub(mapping.child_address)
                    .ok_or(AddressError::Overflow)?;
                address = mapping
                    .parent_address
                    .checked_add(delta)
                    .ok_or(AddressError::Overflow)?;
            }
            self.ensure_range_fits(parent, address, size)?;
            bus = parent;
        }
        Ok(address)
    }

    /// 解码节点上具有 `reg` 布局的属性，并把全部地址翻译到根地址空间。
    pub fn translated_reg_property(
        &self,
        id: NodeId,
        property_name: &'static str,
    ) -> Result<Vec<AddressRange>, AddressError> {
        let entries = self.reg_property(id, property_name)?;
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let bus = self.parent(id).ok_or(AddressError::MissingParent(id))?;
        entries
            .into_iter()
            .map(|entry| {
                Ok(AddressRange {
                    address: self.translate_address(bus, entry.address, entry.size)?,
                    size: entry.size,
                })
            })
            .collect()
    }

    /// 解码节点全部 `reg` 并翻译到根地址空间。
    #[inline]
    pub fn translated_reg(&self, id: NodeId) -> Result<Vec<AddressRange>, AddressError> {
        self.translated_reg_property(id, "reg")
    }

    /// 解析 `/chosen/stdout-path`，并兼容 `/chosen@0` 与历史
    /// `linux,stdout-path`。
    ///
    /// 第一个 `:` 后的全部内容不作改写，调用方可按 UART binding 继续解释。
    pub fn chosen_stdout(&self) -> Result<Option<ChosenStdout<'a>>, TreeError> {
        let Some(chosen_id) = self
            .find_node("/chosen")
            .or_else(|| self.find_node("/chosen@0"))
        else {
            return Ok(None);
        };
        let chosen = self
            .node(chosen_id)
            .expect("indexed chosen node must exist");
        let Some(property) = ["stdout-path", "linux,stdout-path"]
            .into_iter()
            .find_map(|name| chosen.property(name))
        else {
            return Ok(None);
        };
        let raw = property.as_str().map_err(TreeError::InvalidChosen)?;
        let (selector, options) = raw
            .split_once(':')
            .map_or((raw, None), |(path, options)| (path, Some(options)));

        let (path, node) = if selector.starts_with('/') {
            let node = self
                .find_node(selector)
                .ok_or_else(|| TreeError::UnresolvedChosen(raw.to_string()))?;
            (selector, node)
        } else {
            let alias = self
                .aliases
                .get(selector)
                .ok_or_else(|| TreeError::UnresolvedChosen(raw.to_string()))?;
            (alias.path, alias.target)
        };
        Ok(Some(ChosenStdout {
            raw,
            path,
            node,
            options,
        }))
    }

    fn node_or_error(&self, id: NodeId) -> Result<Node<'a>, AddressError> {
        self.node(id).ok_or(AddressError::InvalidNode(id))
    }

    fn ensure_range_fits(
        &self,
        bus: NodeId,
        address: u128,
        size: Option<u128>,
    ) -> Result<(), AddressError> {
        let cells = self.address_cells(bus)?;
        let fits = match cells {
            0 => address == 0,
            1..=3 => {
                let limit = 1u128 << (cells * 32);
                address < limit && size.is_none_or(|size| size <= limit - address)
            }
            _ => size.is_none_or(|size| address.checked_add(size).is_some()),
        };
        if fits {
            Ok(())
        } else {
            Err(AddressError::AddressOutOfRange {
                bus,
                address,
                size,
                cells,
            })
        }
    }

    fn cell_count(
        &self,
        bus: NodeId,
        property_name: &'static str,
        default: u32,
    ) -> Result<u32, AddressError> {
        let node = self.node_or_error(bus)?;
        let count = match node.property(property_name) {
            None => default,
            Some(property) => property
                .as_u32()
                .map_err(|error| AddressError::InvalidProperty {
                    node: bus,
                    property: property_name,
                    error,
                })?,
        };
        if count > 4 {
            Err(AddressError::UnsupportedCellCount {
                node: bus,
                property: property_name,
                count,
            })
        } else {
            Ok(count)
        }
    }

    fn decode_reg(
        &self,
        node: NodeId,
        property_name: &'static str,
        property: Property<'a>,
        address_cells: u32,
        size_cells: u32,
    ) -> Result<Vec<RegEntry>, AddressError> {
        let stride = (address_cells as usize)
            .checked_add(size_cells as usize)
            .ok_or(AddressError::Overflow)?;
        let total_cells = checked_cells(node, property_name, property)?;
        if stride == 0 || !total_cells.is_multiple_of(stride) {
            return Err(AddressError::IncompleteEntry {
                node,
                property: property_name,
                cells: total_cells,
                cells_per_entry: stride,
            });
        }
        let mut cells = property
            .cells()
            .map_err(|error| AddressError::InvalidProperty {
                node,
                property: property_name,
                error,
            })?;
        let mut result = Vec::with_capacity(total_cells / stride);
        while cells.remaining() != 0 {
            let address = read_cell_value(&mut cells, address_cells, node, property_name)?;
            let size = if size_cells == 0 {
                None
            } else {
                Some(read_cell_value(
                    &mut cells,
                    size_cells,
                    node,
                    property_name,
                )?)
            };
            result.push(RegEntry { address, size });
        }
        Ok(result)
    }

    fn index_phandles(&mut self) -> Result<(), TreeError> {
        for index in 0..self.nodes.len() {
            let id = NodeId(index);
            let node = self.nodes[index].node;
            let phandle = decode_phandle(node.property("phandle"), id, "phandle")?;
            let linux = decode_phandle(node.property("linux,phandle"), id, "linux,phandle")?;
            if let (Some(phandle), Some(linux_phandle)) = (phandle, linux)
                && phandle != linux_phandle
            {
                return Err(TreeError::ConflictingPhandle {
                    node: id,
                    phandle,
                    linux_phandle,
                });
            }
            let Some(value) = phandle.or(linux) else {
                continue;
            };
            if value == 0 || value == u32::MAX {
                return Err(TreeError::ReservedPhandle { node: id, value });
            }
            if let Some(first) = self.phandles.insert(value, id) {
                return Err(TreeError::DuplicatePhandle {
                    value,
                    first,
                    second: id,
                });
            }
            self.node_phandles.insert(id, value);
        }
        Ok(())
    }

    fn validate_names_and_legacy_paths(&self) -> Result<(), TreeError> {
        for (index, record) in self.nodes.iter().enumerate() {
            let mut properties = BTreeSet::new();
            for property in record.node.properties() {
                if !properties.insert(property.name_bytes()) {
                    return Err(TreeError::InvalidFdt(Error::DuplicatePropertyName {
                        offset: property.structure_offset(),
                    }));
                }
            }

            let mut children = BTreeSet::new();
            for &child_id in &record.children {
                let child = self.nodes[child_id.index()].node;
                if !children.insert(child.name_bytes()) {
                    return Err(TreeError::InvalidFdt(Error::DuplicateNodeName {
                        offset: child.structure_offset(),
                    }));
                }

                if self.fdt.header().version < 16 {
                    let parent_path = record.node.raw_name_bytes();
                    let child_path = child.raw_name_bytes();
                    let valid = if index == self.root_id().index() {
                        child_path.starts_with(b"/") && !child_path[1..].contains(&b'/')
                    } else {
                        child_path.strip_prefix(parent_path).is_some_and(|suffix| {
                            suffix.starts_with(b"/") && !suffix[1..].contains(&b'/')
                        })
                    };
                    if !valid {
                        return Err(TreeError::InvalidFdt(Error::InvalidNodeName {
                            offset: child.structure_offset(),
                        }));
                    }
                }
            }
        }
        Ok(())
    }

    fn index_aliases(&mut self) {
        let Some(aliases_id) = self.find_node("/aliases") else {
            return;
        };
        let aliases = self.nodes[aliases_id.index()].node;
        for property in aliases.properties() {
            if matches!(property.name(), "name" | "phandle" | "linux,phandle") {
                continue;
            }
            let Ok(path) = property.as_str() else {
                continue;
            };
            if !path.starts_with('/') || path.contains(':') {
                continue;
            }
            let Some(target) = self.find_node(path) else {
                continue;
            };
            self.aliases.insert(property.name(), Alias { path, target });
        }
    }
}

fn decode_phandle(
    property: Option<Property<'_>>,
    node: NodeId,
    property_name: &'static str,
) -> Result<Option<u32>, TreeError> {
    property
        .map(|property| {
            property
                .as_u32()
                .map_err(|error| TreeError::InvalidPhandle {
                    node,
                    property: property_name,
                    error,
                })
        })
        .transpose()
}

fn checked_cells(
    node: NodeId,
    property_name: &'static str,
    property: Property<'_>,
) -> Result<usize, AddressError> {
    property
        .cells()
        .map(|cells| cells.len())
        .map_err(|error| AddressError::InvalidProperty {
            node,
            property: property_name,
            error,
        })
}

fn cell_stride(child: u32, parent: u32, size: u32) -> Option<usize> {
    (child as usize)
        .checked_add(parent as usize)?
        .checked_add(size as usize)
}

fn read_cell_value(
    cells: &mut crate::Cells<'_>,
    count: u32,
    node: NodeId,
    property: &'static str,
) -> Result<u128, AddressError> {
    cells
        .read_value(count as usize)
        .map_err(|error| AddressError::InvalidProperty {
            node,
            property,
            error,
        })
}

fn mapping_contains(mapping: RangeMapping, address: u128, size: Option<u128>) -> bool {
    let Some(delta) = address.checked_sub(mapping.child_address) else {
        return false;
    };
    let Some(window_size) = mapping.size else {
        return delta == 0 && size.is_none_or(|size| size == 0);
    };
    let requested = size.unwrap_or(0);
    if requested == 0 {
        delta < window_size
    } else {
        delta < window_size && requested <= window_size - delta
    }
}

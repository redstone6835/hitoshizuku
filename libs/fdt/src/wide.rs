//! 任意宽度 cell 数值与无损地址翻译。
//!
//! 设备树允许 binding 自行声明 `#address-cells`、`#size-cells`。内核最终只能
//! 消费本机地址宽度，但解析层不能因此截断或拒绝一棵结构合法的树。本模块保留
//! 完整的大端 cell 序列，并为 `reg`、`ranges`、`dma-ranges` 提供任意精度运算。

use alloc::{vec, vec::Vec};
use core::{cmp::Ordering, fmt};

use crate::{NodeId, Property, PropertyError, Tree};

/// 一个按高位 cell 在前保存的无符号任意精度数值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellValue {
    cells: Vec<u32>,
}

impl CellValue {
    /// 保留输入 cell 的精确宽度和前导零。
    #[inline]
    pub fn from_cells(cells: &[u32]) -> Self {
        Self {
            cells: cells.to_vec(),
        }
    }

    /// 返回原始大端 cell 序列。
    #[inline]
    pub fn cells(&self) -> &[u32] {
        &self.cells
    }

    /// 返回编码宽度。
    #[inline]
    pub fn width(&self) -> usize {
        self.cells.len()
    }

    /// 数值是否为零。
    #[inline]
    pub fn is_zero(&self) -> bool {
        self.significant_cells().is_empty()
    }

    /// 数值能否由指定数量的 cell 无损表示。
    #[inline]
    pub fn fits_cells(&self, width: usize) -> bool {
        self.significant_cells().len() <= width
    }

    /// 转换为 `u128`；超过 128 位的非零高位会返回 `None`。
    pub fn to_u128(&self) -> Option<u128> {
        let significant = self.significant_cells();
        if significant.len() > 4 {
            return None;
        }
        let mut value = 0u128;
        for &cell in significant {
            value = (value << 32) | u128::from(cell);
        }
        Some(value)
    }

    /// 按数值比较，忽略前导零和编码宽度。
    pub fn numeric_cmp(&self, other: &Self) -> Ordering {
        let left = self.significant_cells();
        let right = other.significant_cells();
        left.len().cmp(&right.len()).then_with(|| left.cmp(right))
    }

    /// 任意精度加法。仅当结果 vector 长度计算溢出时返回 `None`。
    pub fn checked_add(&self, other: &Self) -> Option<Self> {
        let width = self.cells.len().max(other.cells.len());
        let capacity = width.checked_add(1)?;
        let mut result = vec![0; capacity];
        let mut carry = 0u64;
        for index in 0..width {
            let left = u64::from(cell_from_end(&self.cells, index));
            let right = u64::from(cell_from_end(&other.cells, index));
            let sum = left + right + carry;
            result[capacity - 1 - index] = sum as u32;
            carry = sum >> 32;
        }
        result[0] = carry as u32;
        Some(Self::canonical(result))
    }

    /// 任意精度减法；下溢时返回 `None`。
    pub fn checked_sub(&self, other: &Self) -> Option<Self> {
        if self.numeric_cmp(other) == Ordering::Less {
            return None;
        }
        let width = self.cells.len().max(other.cells.len());
        let mut result = vec![0; width];
        let mut borrow = 0i64;
        for index in 0..width {
            let left = i64::from(cell_from_end(&self.cells, index));
            let right = i64::from(cell_from_end(&other.cells, index));
            let mut difference = left - right - borrow;
            if difference < 0 {
                difference += 1i64 << 32;
                borrow = 1;
            } else {
                borrow = 0;
            }
            result[width - 1 - index] = difference as u32;
        }
        debug_assert_eq!(borrow, 0);
        Some(Self::canonical(result))
    }

    fn significant_cells(&self) -> &[u32] {
        let first = self
            .cells
            .iter()
            .position(|&cell| cell != 0)
            .unwrap_or(self.cells.len());
        &self.cells[first..]
    }

    fn canonical(mut cells: Vec<u32>) -> Self {
        let leading = cells
            .iter()
            .position(|&cell| cell != 0)
            .unwrap_or(cells.len());
        if leading != 0 {
            cells.drain(..leading);
        }
        Self { cells }
    }
}

impl PartialOrd for CellValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CellValue {
    fn cmp(&self, other: &Self) -> Ordering {
        self.numeric_cmp(other)
    }
}

/// 任意宽度 `reg` 条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellRegEntry {
    pub address: CellValue,
    pub size: Option<CellValue>,
}

/// 任意宽度 `ranges` 或 `dma-ranges` 条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellRangeMapping {
    pub child_address: CellValue,
    pub parent_address: CellValue,
    pub size: Option<CellValue>,
}

/// 翻译到根地址空间后的任意宽度范围。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellAddressRange {
    pub address: CellValue,
    pub size: Option<CellValue>,
}

/// 任意宽度地址解码错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CellAddressError {
    InvalidNode(NodeId),
    MissingParent(NodeId),
    InvalidProperty {
        node: NodeId,
        property: &'static str,
        error: PropertyError,
    },
    CellCountOverflow {
        node: NodeId,
        property: &'static str,
        count: u32,
    },
    IncompleteEntry {
        node: NodeId,
        property: &'static str,
        cells: usize,
        cells_per_entry: usize,
    },
    MissingRanges {
        node: NodeId,
        property: &'static str,
    },
    UnmappedAddress {
        bus: NodeId,
        property: &'static str,
    },
    AddressOutOfRange {
        bus: NodeId,
        cells: usize,
    },
    Overflow,
}

impl fmt::Display for CellAddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT wide address error: {self:?}")
    }
}

impl Tree<'_> {
    /// 无损解码节点的 `reg`。
    #[inline]
    pub fn reg_cells(&self, node: NodeId) -> Result<Vec<CellRegEntry>, CellAddressError> {
        self.reg_property_cells(node, "reg")
    }

    /// 无损解码具有 `reg` 元组布局的属性。
    pub fn reg_property_cells(
        &self,
        node: NodeId,
        property_name: &'static str,
    ) -> Result<Vec<CellRegEntry>, CellAddressError> {
        let view = self.node(node).ok_or(CellAddressError::InvalidNode(node))?;
        let Some(property) = view.property(property_name) else {
            return Ok(Vec::new());
        };
        let parent = self
            .parent(node)
            .ok_or(CellAddressError::MissingParent(node))?;
        let address_cells = wide_cell_count(self, parent, "#address-cells", 2)?;
        let size_cells = wide_cell_count(self, parent, "#size-cells", 1)?;
        decode_wide_reg(node, property_name, property, address_cells, size_cells)
    }

    /// 无损解码普通 `ranges`。
    #[inline]
    pub fn ranges_cells(
        &self,
        bus: NodeId,
    ) -> Result<Option<Vec<CellRangeMapping>>, CellAddressError> {
        self.ranges_property_cells(bus, "ranges")
    }

    /// 无损解码 `dma-ranges`。
    #[inline]
    pub fn dma_ranges_cells(
        &self,
        bus: NodeId,
    ) -> Result<Option<Vec<CellRangeMapping>>, CellAddressError> {
        self.ranges_property_cells(bus, "dma-ranges")
    }

    /// 无损解码具有普通 `ranges` 元组布局的属性。
    pub fn ranges_property_cells(
        &self,
        bus: NodeId,
        property_name: &'static str,
    ) -> Result<Option<Vec<CellRangeMapping>>, CellAddressError> {
        let view = self.node(bus).ok_or(CellAddressError::InvalidNode(bus))?;
        let Some(property) = view.property(property_name) else {
            return Ok(None);
        };
        let parent = self
            .parent(bus)
            .ok_or(CellAddressError::MissingParent(bus))?;
        if property.value().is_empty() {
            return Ok(Some(Vec::new()));
        }

        let child_cells = wide_cell_count(self, bus, "#address-cells", 2)?;
        let parent_cells = wide_cell_count(self, parent, "#address-cells", 2)?;
        let size_cells = wide_cell_count(self, bus, "#size-cells", 1)?;
        let stride = child_cells
            .checked_add(parent_cells)
            .and_then(|value| value.checked_add(size_cells))
            .ok_or(CellAddressError::Overflow)?;
        let values = property_values(bus, property_name, property)?;
        if stride == 0 || !values.len().is_multiple_of(stride) {
            return Err(CellAddressError::IncompleteEntry {
                node: bus,
                property: property_name,
                cells: values.len(),
                cells_per_entry: stride,
            });
        }

        let mut result = Vec::with_capacity(values.len() / stride);
        for row in values.chunks_exact(stride) {
            let parent_start = child_cells;
            let size_start = parent_start + parent_cells;
            result.push(CellRangeMapping {
                child_address: CellValue::from_cells(&row[..parent_start]),
                parent_address: CellValue::from_cells(&row[parent_start..size_start]),
                size: (size_cells != 0).then(|| CellValue::from_cells(&row[size_start..])),
            });
        }
        Ok(Some(result))
    }

    /// 通过普通 `ranges` 把任意宽度地址翻译到根地址空间。
    pub fn translate_address_cells(
        &self,
        bus: NodeId,
        address: &CellValue,
        size: Option<&CellValue>,
    ) -> Result<CellValue, CellAddressError> {
        self.translate_address_cells_with(bus, address, size, "ranges")
    }

    /// 通过 `dma-ranges` 把任意宽度 DMA 地址翻译到根地址空间。
    pub fn translate_dma_address_cells(
        &self,
        bus: NodeId,
        address: &CellValue,
        size: Option<&CellValue>,
    ) -> Result<CellValue, CellAddressError> {
        self.translate_address_cells_with(bus, address, size, "dma-ranges")
    }

    /// 无损解码并翻译节点的全部 `reg` 条目。
    pub fn translated_reg_cells(
        &self,
        node: NodeId,
    ) -> Result<Vec<CellAddressRange>, CellAddressError> {
        let entries = self.reg_cells(node)?;
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let bus = self
            .parent(node)
            .ok_or(CellAddressError::MissingParent(node))?;
        entries
            .into_iter()
            .map(|entry| {
                let address =
                    self.translate_address_cells(bus, &entry.address, entry.size.as_ref())?;
                Ok(CellAddressRange {
                    address,
                    size: entry.size,
                })
            })
            .collect()
    }

    fn translate_address_cells_with(
        &self,
        mut bus: NodeId,
        address: &CellValue,
        size: Option<&CellValue>,
        property_name: &'static str,
    ) -> Result<CellValue, CellAddressError> {
        self.node(bus).ok_or(CellAddressError::InvalidNode(bus))?;
        let mut translated = address.clone();
        ensure_wide_range_fits(self, bus, &translated, size)?;
        while let Some(parent) = self.parent(bus) {
            let mappings = self.ranges_property_cells(bus, property_name)?.ok_or(
                CellAddressError::MissingRanges {
                    node: bus,
                    property: property_name,
                },
            )?;
            if !mappings.is_empty() {
                let mapping = mappings
                    .iter()
                    .find(|mapping| wide_mapping_contains(mapping, &translated, size))
                    .ok_or(CellAddressError::UnmappedAddress {
                        bus,
                        property: property_name,
                    })?;
                let delta = translated
                    .checked_sub(&mapping.child_address)
                    .ok_or(CellAddressError::Overflow)?;
                translated = mapping
                    .parent_address
                    .checked_add(&delta)
                    .ok_or(CellAddressError::Overflow)?;
            }
            ensure_wide_range_fits(self, parent, &translated, size)?;
            bus = parent;
        }
        Ok(translated)
    }
}

fn wide_cell_count(
    tree: &Tree<'_>,
    node: NodeId,
    property_name: &'static str,
    default: u32,
) -> Result<usize, CellAddressError> {
    let view = tree.node(node).ok_or(CellAddressError::InvalidNode(node))?;
    let count = match view.property(property_name) {
        None => default,
        Some(property) => property
            .as_u32()
            .map_err(|error| CellAddressError::InvalidProperty {
                node,
                property: property_name,
                error,
            })?,
    };
    usize::try_from(count).map_err(|_| CellAddressError::CellCountOverflow {
        node,
        property: property_name,
        count,
    })
}

fn decode_wide_reg(
    node: NodeId,
    property_name: &'static str,
    property: Property<'_>,
    address_cells: usize,
    size_cells: usize,
) -> Result<Vec<CellRegEntry>, CellAddressError> {
    let stride = address_cells
        .checked_add(size_cells)
        .ok_or(CellAddressError::Overflow)?;
    let values = property_values(node, property_name, property)?;
    if stride == 0 || !values.len().is_multiple_of(stride) {
        return Err(CellAddressError::IncompleteEntry {
            node,
            property: property_name,
            cells: values.len(),
            cells_per_entry: stride,
        });
    }
    let mut result = Vec::with_capacity(values.len() / stride);
    for row in values.chunks_exact(stride) {
        result.push(CellRegEntry {
            address: CellValue::from_cells(&row[..address_cells]),
            size: (size_cells != 0).then(|| CellValue::from_cells(&row[address_cells..])),
        });
    }
    Ok(result)
}

fn property_values(
    node: NodeId,
    property_name: &'static str,
    property: Property<'_>,
) -> Result<Vec<u32>, CellAddressError> {
    property
        .cells()
        .map(|cells| cells.collect())
        .map_err(|error| CellAddressError::InvalidProperty {
            node,
            property: property_name,
            error,
        })
}

fn ensure_wide_range_fits(
    tree: &Tree<'_>,
    bus: NodeId,
    address: &CellValue,
    size: Option<&CellValue>,
) -> Result<(), CellAddressError> {
    let width = wide_cell_count(tree, bus, "#address-cells", 2)?;
    let last = match size {
        Some(size) if !size.is_zero() => {
            let one = CellValue::from_cells(&[1]);
            let tail = size.checked_sub(&one).ok_or(CellAddressError::Overflow)?;
            Some(
                address
                    .checked_add(&tail)
                    .ok_or(CellAddressError::Overflow)?,
            )
        }
        _ => None,
    };
    if address.fits_cells(width) && last.as_ref().is_none_or(|last| last.fits_cells(width)) {
        Ok(())
    } else {
        Err(CellAddressError::AddressOutOfRange { bus, cells: width })
    }
}

fn wide_mapping_contains(
    mapping: &CellRangeMapping,
    address: &CellValue,
    size: Option<&CellValue>,
) -> bool {
    let Some(delta) = address.checked_sub(&mapping.child_address) else {
        return false;
    };
    let Some(window_size) = mapping.size.as_ref() else {
        return delta.is_zero() && size.is_none_or(CellValue::is_zero);
    };
    if size.is_none_or(CellValue::is_zero) {
        return delta.numeric_cmp(window_size) == Ordering::Less;
    }
    delta
        .checked_add(size.expect("non-zero size was checked"))
        .is_some_and(|end| end.numeric_cmp(window_size) != Ordering::Greater)
}

fn cell_from_end(cells: &[u32], index: usize) -> u32 {
    cells
        .len()
        .checked_sub(index + 1)
        .and_then(|index| cells.get(index).copied())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::CellValue;
    use core::cmp::Ordering;

    #[test]
    fn arbitrary_precision_arithmetic_ignores_leading_zero_width() {
        let wide = CellValue::from_cells(&[0, 1, 0, 0, 0]);
        let narrow = CellValue::from_cells(&[1, 0, 0, 0]);
        assert_eq!(wide.numeric_cmp(&narrow), Ordering::Equal);
        assert_eq!(wide.to_u128(), Some(1u128 << 96));

        let maximum = CellValue::from_cells(&[u32::MAX, u32::MAX, u32::MAX, u32::MAX]);
        let one = CellValue::from_cells(&[1]);
        let sum = maximum.checked_add(&one).unwrap();
        assert_eq!(sum.cells(), &[1, 0, 0, 0, 0]);
        assert_eq!(
            sum.checked_sub(&one).unwrap().numeric_cmp(&maximum),
            Ordering::Equal
        );
    }
}

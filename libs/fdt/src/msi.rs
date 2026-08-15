//! Devicetree `msi-parent` phandle 列表的规范化解码。

use alloc::vec::Vec;
use core::fmt;

use crate::{NodeId, PropertyError, Tree};

/// 一条已经绑定到具体 MSI controller 的 `msi-parent` 项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MsiParent {
    /// MSI controller 的稳定节点编号。
    pub controller: NodeId,
    /// 属性中使用的规范 phandle。
    pub controller_phandle: u32,
    /// 按 controller `#msi-cells` 保留的原始 cell 值。
    pub msi_specifier: Vec<u32>,
}

/// `msi-parent` binding 解码错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MsiError {
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
    /// phandle 没有对应节点。
    UnknownPhandle {
        /// 引用所在节点。
        node: NodeId,
        /// 属性中的条目序号。
        entry: usize,
        /// 未解析的 phandle。
        phandle: u32,
    },
    /// `#msi-cells` 无法在本机上形成切片。
    InvalidMsiCells {
        /// 声明 cell 数的 controller。
        controller: NodeId,
        /// 声明值。
        cells: u32,
    },
    /// 属性存在但没有任何 phandle 项。
    EmptyProperty(NodeId),
    /// 属性末尾不足以组成完整条目。
    IncompleteEntry {
        /// 属性所在节点。
        node: NodeId,
        /// 正在解码的条目序号。
        entry: usize,
        /// phandle 后剩余的 cell 数。
        remaining_cells: usize,
        /// 当前条目所需的 specifier cell 数。
        required_cells: usize,
    },
}

impl fmt::Display for MsiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT MSI error: {self:?}")
    }
}

impl Tree<'_> {
    /// 原子地解码节点的 `msi-parent` phandle + specifier 列表。
    ///
    /// 目标省略 `#msi-cells` 时按规范和 Linux optional-args 语义视为零。
    /// `Ok(None)` 仅表示属性缺失；存在属性时任意坏条目都会使整个列表失败。
    pub fn msi_parents(&self, node: NodeId) -> Result<Option<Vec<MsiParent>>, MsiError> {
        let view = self.node(node).ok_or(MsiError::InvalidNode(node))?;
        let Some(property) = view.property("msi-parent") else {
            return Ok(None);
        };
        let values = property
            .cells()
            .map(|cells| cells.collect::<Vec<_>>())
            .map_err(|error| MsiError::InvalidProperty {
                node,
                property: "msi-parent",
                error,
            })?;
        if values.is_empty() {
            return Err(MsiError::EmptyProperty(node));
        }

        let mut parents = Vec::new();
        let mut offset = 0usize;
        let mut entry = 0usize;
        while offset < values.len() {
            let phandle = values[offset];
            offset += 1;
            let controller = self
                .node_by_phandle(phandle)
                .ok_or(MsiError::UnknownPhandle {
                    node,
                    entry,
                    phandle,
                })?;
            let controller_view = self
                .node(controller)
                .ok_or(MsiError::InvalidNode(controller))?;
            let cells = match controller_view.property("#msi-cells") {
                None => 0,
                Some(property) => property
                    .as_u32()
                    .map_err(|error| MsiError::InvalidProperty {
                        node: controller,
                        property: "#msi-cells",
                        error,
                    })?,
            };
            let cells = usize::try_from(cells)
                .map_err(|_| MsiError::InvalidMsiCells { controller, cells })?;
            let remaining = values.len() - offset;
            if remaining < cells {
                return Err(MsiError::IncompleteEntry {
                    node,
                    entry,
                    remaining_cells: remaining,
                    required_cells: cells,
                });
            }
            let end = offset + cells;
            parents.push(MsiParent {
                controller,
                controller_phandle: phandle,
                msi_specifier: values[offset..end].to_vec(),
            });
            offset = end;
            entry += 1;
        }
        Ok(Some(parents))
    }
}

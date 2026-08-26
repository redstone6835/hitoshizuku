//! Devicetree NUMA binding 的规范化语义层。
//!
//! 本模块解析任意节点上的 `numa-node-id`，以及
//! `compatible = "numa-distance-map-v1"` 的距离矩阵。节点归属保持显式来源；
//! 普通设备查询可以使用父链继承接口，距离查询则按 binding 语义接受只声明一侧
//! 的对称矩阵。

use alloc::vec::Vec;
use core::fmt;

use crate::{NodeId, PropertyError, Tree, TreeError};

/// NUMA 本地访问距离。`numa-distance-map-v1` 要求同节点项使用该值。
pub const NUMA_LOCAL_DISTANCE: u32 = 10;

/// 一个节点直接声明的 NUMA 归属。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumaNodeAssignment {
    pub node: NodeId,
    pub node_id: u32,
}

/// NUMA 节点之间的一条固件距离。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumaDistance {
    pub from: u32,
    pub to: u32,
    pub distance: u32,
}

/// 完整 NUMA 固件描述。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NumaDescription {
    /// 所有启用节点上直接声明的 `numa-node-id`。
    pub assignments: Vec<NumaNodeAssignment>,
    /// `distance-matrix` 的原始顺序条目。
    pub distances: Vec<NumaDistance>,
}

impl NumaDescription {
    /// 查询一对节点的距离。
    ///
    /// v1 binding 的矩阵是对称的，因此只提供反向项时仍可求值。同节点且固件未
    /// 显式列出时返回规范本地距离；不同节点未描述时保持 `None`。
    pub fn distance(&self, from: u32, to: u32) -> Option<u32> {
        self.distances
            .iter()
            .find(|entry| entry.from == from && entry.to == to)
            .or_else(|| {
                self.distances
                    .iter()
                    .find(|entry| entry.from == to && entry.to == from)
            })
            .map(|entry| entry.distance)
            .or_else(|| (from == to).then_some(NUMA_LOCAL_DISTANCE))
    }
}

/// NUMA binding 解码错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NumaError {
    InvalidTree(TreeError),
    InvalidProperty {
        node: NodeId,
        property: &'static str,
        error: PropertyError,
    },
    MultipleDistanceMaps {
        first: NodeId,
        second: NodeId,
    },
    DistanceMapOutsideRoot {
        node: NodeId,
    },
    InvalidDistanceMapName {
        node: NodeId,
    },
    MissingDistanceMatrix {
        node: NodeId,
    },
    InvalidDistanceMatrixLength {
        node: NodeId,
        cells: usize,
    },
    InvalidDistance {
        node: NodeId,
        from: u32,
        to: u32,
        distance: u32,
    },
    DuplicateDistance {
        node: NodeId,
        from: u32,
        to: u32,
    },
    UnorderedDistance {
        node: NodeId,
        previous_from: u32,
        previous_to: u32,
        from: u32,
        to: u32,
    },
    AsymmetricDistance {
        node: NodeId,
        from: u32,
        to: u32,
        forward: u32,
        reverse: u32,
    },
}

impl fmt::Display for NumaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT NUMA error: {self:?}")
    }
}

impl From<TreeError> for NumaError {
    fn from(value: TreeError) -> Self {
        Self::InvalidTree(value)
    }
}

impl Tree<'_> {
    /// 解码节点直接声明的 `numa-node-id`。
    pub fn numa_node_id(&self, node: NodeId) -> Result<Option<u32>, NumaError> {
        let node_view = self.node(node).ok_or(TreeError::InvalidNode(node))?;
        node_view
            .property("numa-node-id")
            .map(|property| {
                property
                    .as_u32()
                    .map_err(|error| NumaError::InvalidProperty {
                        node,
                        property: "numa-node-id",
                        error,
                    })
            })
            .transpose()
    }

    /// 按 Linux OF 语义沿父链查询设备的有效 NUMA 归属。
    pub fn effective_numa_node_id(&self, mut node: NodeId) -> Result<Option<u32>, NumaError> {
        loop {
            if let Some(node_id) = self.numa_node_id(node)? {
                return Ok(Some(node_id));
            }
            let Some(parent) = self.parent(node) else {
                return Ok(None);
            };
            node = parent;
        }
    }

    /// 解析整棵树的 NUMA 节点归属与 v1 距离矩阵。
    pub fn numa_description(&self) -> Result<NumaDescription, NumaError> {
        let mut assignments = Vec::new();
        let mut distance_map = None;

        for node in self.node_ids() {
            if !self.numa_node_is_effectively_available(node)? {
                continue;
            }
            if let Some(node_id) = self.numa_node_id(node)? {
                assignments.push(NumaNodeAssignment { node, node_id });
            }

            let node_view = self.node(node).expect("indexed node must exist");
            let Some(compatible) = node_view.property("compatible") else {
                continue;
            };
            let compatible =
                compatible
                    .as_string_list()
                    .map_err(|error| NumaError::InvalidProperty {
                        node,
                        property: "compatible",
                        error,
                    })?;
            if !compatible
                .into_iter()
                .any(|value| value == "numa-distance-map-v1")
            {
                continue;
            }
            if self.parent(node) != Some(self.root_id()) {
                return Err(NumaError::DistanceMapOutsideRoot { node });
            }
            if node_view.name() != "distance-map" {
                return Err(NumaError::InvalidDistanceMapName { node });
            }
            if let Some(first) = distance_map.replace(node) {
                return Err(NumaError::MultipleDistanceMaps {
                    first,
                    second: node,
                });
            }
        }

        let distances = match distance_map {
            Some(node) => self.parse_numa_distances(node)?,
            None => Vec::new(),
        };
        Ok(NumaDescription {
            assignments,
            distances,
        })
    }

    fn parse_numa_distances(&self, node: NodeId) -> Result<Vec<NumaDistance>, NumaError> {
        let node_view = self.node(node).expect("indexed node must exist");
        let property = node_view
            .property("distance-matrix")
            .ok_or(NumaError::MissingDistanceMatrix { node })?;
        let cells = property
            .cells()
            .map_err(|error| NumaError::InvalidProperty {
                node,
                property: "distance-matrix",
                error,
            })?
            .collect::<Vec<_>>();
        if cells.is_empty() || !cells.len().is_multiple_of(3) {
            return Err(NumaError::InvalidDistanceMatrixLength {
                node,
                cells: cells.len(),
            });
        }

        let mut distances = Vec::with_capacity(cells.len() / 3);
        let mut previous = None;
        for tuple in cells.chunks_exact(3) {
            let entry = NumaDistance {
                from: tuple[0],
                to: tuple[1],
                distance: tuple[2],
            };
            if let Some((previous_from, previous_to)) = previous
                && (entry.from, entry.to) <= (previous_from, previous_to)
            {
                return Err(NumaError::UnorderedDistance {
                    node,
                    previous_from,
                    previous_to,
                    from: entry.from,
                    to: entry.to,
                });
            }
            let valid = if entry.from == entry.to {
                entry.distance == NUMA_LOCAL_DISTANCE
            } else {
                entry.distance > NUMA_LOCAL_DISTANCE
            };
            if !valid {
                return Err(NumaError::InvalidDistance {
                    node,
                    from: entry.from,
                    to: entry.to,
                    distance: entry.distance,
                });
            }
            if distances.iter().any(|existing: &NumaDistance| {
                existing.from == entry.from && existing.to == entry.to
            }) {
                return Err(NumaError::DuplicateDistance {
                    node,
                    from: entry.from,
                    to: entry.to,
                });
            }
            if let Some(reverse) = distances
                .iter()
                .find(|existing| existing.from == entry.to && existing.to == entry.from)
                && reverse.distance != entry.distance
            {
                return Err(NumaError::AsymmetricDistance {
                    node,
                    from: entry.from,
                    to: entry.to,
                    forward: entry.distance,
                    reverse: reverse.distance,
                });
            }
            distances.push(entry);
            previous = Some((entry.from, entry.to));
        }
        Ok(distances)
    }

    fn numa_node_is_effectively_available(&self, mut node: NodeId) -> Result<bool, NumaError> {
        loop {
            if !self.is_available(node)? {
                return Ok(false);
            }
            let Some(parent) = self.parent(node) else {
                return Ok(true);
            };
            node = parent;
        }
    }
}

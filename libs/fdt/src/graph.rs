//! Devicetree graph/endpoint 通用 binding。

use alloc::vec::Vec;
use core::fmt;

use crate::{NodeId, PropertyError, Tree};

/// 一个 graph endpoint 及其远端连接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphEndpoint {
    pub node: NodeId,
    pub port: NodeId,
    pub port_id: Option<u32>,
    pub endpoint_id: Option<u32>,
    pub phandle: Option<u32>,
    pub remote: Option<NodeId>,
    pub remote_phandle: Option<u32>,
}

/// graph binding 解码错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GraphError {
    InvalidNode(NodeId),
    InvalidProperty {
        node: NodeId,
        property: &'static str,
        error: PropertyError,
    },
    UnknownRemote {
        node: NodeId,
        phandle: u32,
    },
    RemoteIsNotEndpoint {
        node: NodeId,
        remote: NodeId,
    },
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT graph error: {self:?}")
    }
}

impl Tree<'_> {
    /// 枚举设备节点下 direct `port` 或 `ports/port` 的全部 endpoint。
    pub fn graph_endpoints(&self, device: NodeId) -> Result<Vec<GraphEndpoint>, GraphError> {
        self.node(device).ok_or(GraphError::InvalidNode(device))?;
        let mut ports = Vec::new();
        for &child in self
            .children(device)
            .ok_or(GraphError::InvalidNode(device))?
        {
            let view = self.node(child).ok_or(GraphError::InvalidNode(child))?;
            if view.base_name_bytes() == b"port" {
                ports.push(child);
            } else if view.base_name_bytes() == b"ports" {
                for &port in self.children(child).ok_or(GraphError::InvalidNode(child))? {
                    let port_view = self.node(port).ok_or(GraphError::InvalidNode(port))?;
                    if port_view.base_name_bytes() == b"port" {
                        ports.push(port);
                    }
                }
            }
        }

        let mut endpoints = Vec::new();
        for port in ports {
            let port_id = graph_reg(self, port)?;
            for &endpoint in self.children(port).ok_or(GraphError::InvalidNode(port))? {
                let view = self
                    .node(endpoint)
                    .ok_or(GraphError::InvalidNode(endpoint))?;
                if view.base_name_bytes() != b"endpoint" {
                    continue;
                }
                let endpoint_id = graph_reg(self, endpoint)?;
                let (remote, remote_phandle) = match view.property("remote-endpoint") {
                    None => (None, None),
                    Some(property) => {
                        let phandle =
                            property
                                .as_u32()
                                .map_err(|error| GraphError::InvalidProperty {
                                    node: endpoint,
                                    property: "remote-endpoint",
                                    error,
                                })?;
                        let remote =
                            self.node_by_phandle(phandle)
                                .ok_or(GraphError::UnknownRemote {
                                    node: endpoint,
                                    phandle,
                                })?;
                        let remote_view =
                            self.node(remote).ok_or(GraphError::InvalidNode(remote))?;
                        if remote_view.base_name_bytes() != b"endpoint" {
                            return Err(GraphError::RemoteIsNotEndpoint {
                                node: endpoint,
                                remote,
                            });
                        }
                        (Some(remote), Some(phandle))
                    }
                };
                endpoints.push(GraphEndpoint {
                    node: endpoint,
                    port,
                    port_id,
                    endpoint_id,
                    phandle: self.phandle(endpoint),
                    remote,
                    remote_phandle,
                });
            }
        }
        Ok(endpoints)
    }
}

fn graph_reg(tree: &Tree<'_>, node: NodeId) -> Result<Option<u32>, GraphError> {
    let view = tree.node(node).ok_or(GraphError::InvalidNode(node))?;
    view.property("reg")
        .map(|property| {
            property
                .as_u32()
                .map_err(|error| GraphError::InvalidProperty {
                    node,
                    property: "reg",
                    error,
                })
        })
        .transpose()
}

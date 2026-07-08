//! 绑定图模型。

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{ElmError, ElmResult};
use crate::ids::{BindingId, ElmId, Generation, LeaseId, PortId};
use crate::manifest::ElmManifest;
use crate::nexus::FlowContract;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentEdge {
    pub child: ElmId,
    pub parent: ElmId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyEdge {
    pub consumer: ElmId,
    pub provider: ElmId,
    pub contract: FlowContract,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionPoint {
    pub owner: ElmId,
    pub name: String,
    pub contract: FlowContract,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionEdge {
    pub extension: ElmId,
    pub target: ElmId,
    pub point: String,
    pub contract: FlowContract,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityBindingEdge {
    pub id: BindingId,
    pub consumer: ElmId,
    pub port: PortId,
    pub contract: FlowContract,
    pub generation: Generation,
    pub lease: Option<LeaseId>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GraphValidationReport {
    pub cells: usize,
    pub parent_edges: usize,
    pub dependency_edges: usize,
    pub extension_edges: usize,
    pub capability_bindings: usize,
}

#[derive(Debug, Default)]
pub struct BindingGraph {
    cells: BTreeMap<ElmId, ElmManifest>,
    parents: BTreeMap<ElmId, ElmId>,
    dependencies: Vec<DependencyEdge>,
    extension_points: BTreeMap<(ElmId, String), ExtensionPoint>,
    extensions: Vec<ExtensionEdge>,
    capability_bindings: Vec<CapabilityBindingEdge>,
}

impl BindingGraph {
    pub const fn new() -> Self {
        Self {
            cells: BTreeMap::new(),
            parents: BTreeMap::new(),
            dependencies: Vec::new(),
            extension_points: BTreeMap::new(),
            extensions: Vec::new(),
            capability_bindings: Vec::new(),
        }
    }

    pub fn insert_cell(&mut self, id: ElmId, manifest: ElmManifest) -> ElmResult<()> {
        if self.cells.contains_key(&id) {
            return Err(ElmError::DuplicateCell);
        }
        self.cells.insert(id, manifest);
        Ok(())
    }

    pub fn set_parent(&mut self, child: ElmId, parent: ElmId) -> ElmResult<()> {
        self.require_cell(child)?;
        self.require_cell(parent)?;
        if child == parent {
            return Err(ElmError::ParentCycle);
        }
        self.parents.insert(child, parent);
        if self.has_parent_cycle(child) {
            self.parents.remove(&child);
            return Err(ElmError::ParentCycle);
        }
        Ok(())
    }

    pub fn add_dependency(
        &mut self,
        consumer: ElmId,
        provider: ElmId,
        contract: FlowContract,
    ) -> ElmResult<()> {
        self.require_cell(consumer)?;
        self.require_cell(provider)?;
        self.dependencies.push(DependencyEdge {
            consumer,
            provider,
            contract,
        });
        if self.has_dependency_cycle() {
            self.dependencies.pop();
            return Err(ElmError::DependencyCycle);
        }
        Ok(())
    }

    pub fn add_extension_point(
        &mut self,
        owner: ElmId,
        name: impl Into<String>,
        contract: FlowContract,
    ) -> ElmResult<()> {
        self.require_cell(owner)?;
        let name = name.into();
        let key = (owner, name.clone());
        if self.extension_points.contains_key(&key) {
            return Err(ElmError::DuplicateExtensionPoint);
        }
        self.extension_points.insert(
            key,
            ExtensionPoint {
                owner,
                name,
                contract,
            },
        );
        Ok(())
    }

    pub fn add_extension(
        &mut self,
        extension: ElmId,
        target: ElmId,
        point: impl Into<String>,
        contract: FlowContract,
    ) -> ElmResult<()> {
        self.require_cell(extension)?;
        self.require_cell(target)?;
        let point = point.into();
        let Some(extension_point) = self.extension_points.get(&(target, point.clone())) else {
            return Err(ElmError::ExtensionPointNotFound);
        };
        if extension_point.contract != contract {
            return Err(ElmError::ContractMismatch);
        }
        self.extensions.push(ExtensionEdge {
            extension,
            target,
            point,
            contract,
        });
        if self.has_extension_cycle() {
            self.extensions.pop();
            return Err(ElmError::ExtensionCycle);
        }
        Ok(())
    }

    pub fn add_capability_binding(
        &mut self,
        id: BindingId,
        consumer: ElmId,
        port: PortId,
        contract: FlowContract,
        generation: Generation,
        lease: Option<LeaseId>,
    ) -> ElmResult<()> {
        self.require_cell(consumer)?;
        if self.capability_bindings.iter().any(|edge| edge.id == id) {
            return Err(ElmError::DuplicateBinding);
        }
        if self.capability_bindings.iter().any(|edge| {
            edge.active
                && edge.consumer == consumer
                && edge.port == port
                && edge.contract == contract
        }) {
            return Err(ElmError::DuplicateBinding);
        }
        self.capability_bindings.push(CapabilityBindingEdge {
            id,
            consumer,
            port,
            contract,
            generation,
            lease,
            active: true,
        });
        Ok(())
    }

    pub fn validate(&self) -> ElmResult<GraphValidationReport> {
        for (child, parent) in &self.parents {
            self.require_cell(*child)?;
            self.require_cell(*parent)?;
        }
        let mut seen_bindings = BTreeSet::new();
        for edge in &self.capability_bindings {
            self.require_cell(edge.consumer)?;
            if !seen_bindings.insert(edge.id) {
                return Err(ElmError::DuplicateBinding);
            }
        }
        if self.has_dependency_cycle() {
            return Err(ElmError::DependencyCycle);
        }
        if self.has_extension_cycle() {
            return Err(ElmError::ExtensionCycle);
        }
        Ok(GraphValidationReport {
            cells: self.cells.len(),
            parent_edges: self.parents.len(),
            dependency_edges: self.dependencies.len(),
            extension_edges: self.extensions.len(),
            capability_bindings: self.capability_bindings.len(),
        })
    }

    pub fn cell(&self, id: ElmId) -> Option<&ElmManifest> {
        self.cells.get(&id)
    }

    pub fn remove_cell(&mut self, id: ElmId) -> ElmResult<GraphRemovalReport> {
        self.require_cell(id)?;
        if self.parents.values().any(|parent| *parent == id) {
            return Err(ElmError::LeaseBusy);
        }

        let parent_edges = usize::from(self.parents.remove(&id).is_some());
        let before_dependencies = self.dependencies.len();
        self.dependencies
            .retain(|edge| edge.consumer != id && edge.provider != id);
        let dependency_edges = before_dependencies - self.dependencies.len();

        let before_extensions = self.extensions.len();
        self.extensions
            .retain(|edge| edge.extension != id && edge.target != id);
        let extension_edges = before_extensions - self.extensions.len();

        let before_bindings = self.capability_bindings.len();
        self.capability_bindings.retain(|edge| edge.consumer != id);
        let capability_bindings = before_bindings - self.capability_bindings.len();

        let extension_points: Vec<_> = self
            .extension_points
            .keys()
            .filter(|(owner, _)| *owner == id)
            .cloned()
            .collect();
        let extension_point_count = extension_points.len();
        for key in extension_points {
            self.extension_points.remove(&key);
        }

        self.cells.remove(&id);
        Ok(GraphRemovalReport {
            parent_edges,
            dependency_edges,
            extension_edges,
            extension_points: extension_point_count,
            capability_bindings,
        })
    }

    pub fn remove_cell_relations(&mut self, id: ElmId) -> ElmResult<GraphRemovalReport> {
        self.require_cell(id)?;

        let before_dependencies = self.dependencies.len();
        self.dependencies
            .retain(|edge| edge.consumer != id && edge.provider != id);
        let dependency_edges = before_dependencies - self.dependencies.len();

        let before_extensions = self.extensions.len();
        self.extensions
            .retain(|edge| edge.extension != id && edge.target != id);
        let extension_edges = before_extensions - self.extensions.len();

        let before_bindings = self.capability_bindings.len();
        self.capability_bindings.retain(|edge| edge.consumer != id);
        let capability_bindings = before_bindings - self.capability_bindings.len();

        let extension_points: Vec<_> = self
            .extension_points
            .keys()
            .filter(|(owner, _)| *owner == id)
            .cloned()
            .collect();
        let extension_point_count = extension_points.len();
        for key in extension_points {
            self.extension_points.remove(&key);
        }

        Ok(GraphRemovalReport {
            parent_edges: 0,
            dependency_edges,
            extension_edges,
            extension_points: extension_point_count,
            capability_bindings,
        })
    }

    pub fn parent(&self, child: ElmId) -> Option<ElmId> {
        self.parents.get(&child).copied()
    }

    pub fn parent_edges(&self) -> Vec<ParentEdge> {
        self.parents
            .iter()
            .map(|(child, parent)| ParentEdge {
                child: *child,
                parent: *parent,
            })
            .collect()
    }

    pub fn children_of(&self, parent: ElmId) -> Vec<ElmId> {
        self.parents
            .iter()
            .filter_map(|(child, current_parent)| {
                if *current_parent == parent {
                    Some(*child)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn dependencies(&self) -> &[DependencyEdge] {
        &self.dependencies
    }

    pub fn dependents_of(&self, provider: ElmId) -> Vec<ElmId> {
        self.dependencies
            .iter()
            .filter_map(|edge| {
                if edge.provider == provider {
                    Some(edge.consumer)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn extension_points(&self) -> Vec<ExtensionPoint> {
        self.extension_points.values().cloned().collect()
    }

    pub fn extensions(&self) -> &[ExtensionEdge] {
        &self.extensions
    }

    pub fn extensions_targeting(&self, target: ElmId) -> Vec<ElmId> {
        self.extensions
            .iter()
            .filter_map(|edge| {
                if edge.target == target {
                    Some(edge.extension)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn capability_bindings(&self) -> &[CapabilityBindingEdge] {
        &self.capability_bindings
    }

    pub fn capability_binding(&self, id: BindingId) -> Option<&CapabilityBindingEdge> {
        self.capability_bindings.iter().find(|edge| edge.id == id)
    }

    pub fn capability_binding_for(
        &self,
        consumer: ElmId,
        port: PortId,
        contract: &FlowContract,
    ) -> Option<&CapabilityBindingEdge> {
        self.capability_bindings.iter().find(|edge| {
            edge.active
                && edge.consumer == consumer
                && edge.port == port
                && edge.contract == *contract
        })
    }

    pub fn capability_bindings_for_cell(&self, consumer: ElmId) -> Vec<BindingId> {
        self.capability_bindings
            .iter()
            .filter_map(|edge| {
                if edge.consumer == consumer {
                    Some(edge.id)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn capability_bindings_mut_for_cell(
        &mut self,
        consumer: ElmId,
    ) -> impl Iterator<Item = &mut CapabilityBindingEdge> {
        self.capability_bindings
            .iter_mut()
            .filter(move |edge| edge.consumer == consumer)
    }

    pub fn remove_capability_binding(&mut self, id: BindingId) -> ElmResult<CapabilityBindingEdge> {
        let Some(index) = self
            .capability_bindings
            .iter()
            .position(|edge| edge.id == id)
        else {
            return Err(ElmError::BindingNotFound);
        };
        Ok(self.capability_bindings.remove(index))
    }

    fn require_cell(&self, id: ElmId) -> ElmResult<()> {
        if self.cells.contains_key(&id) {
            Ok(())
        } else {
            Err(ElmError::CellNotFound)
        }
    }

    fn has_parent_cycle(&self, start: ElmId) -> bool {
        let mut seen = BTreeSet::new();
        let mut current = start;
        while let Some(parent) = self.parents.get(&current).copied() {
            if !seen.insert(parent) {
                return true;
            }
            current = parent;
        }
        false
    }

    fn has_dependency_cycle(&self) -> bool {
        let mut adjacency: BTreeMap<ElmId, Vec<ElmId>> = BTreeMap::new();
        for edge in &self.dependencies {
            adjacency
                .entry(edge.consumer)
                .or_default()
                .push(edge.provider);
        }
        has_cycle(&adjacency)
    }

    fn has_extension_cycle(&self) -> bool {
        let mut adjacency: BTreeMap<ElmId, Vec<ElmId>> = BTreeMap::new();
        for edge in &self.extensions {
            adjacency
                .entry(edge.extension)
                .or_default()
                .push(edge.target);
        }
        has_cycle(&adjacency)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GraphRemovalReport {
    pub parent_edges: usize,
    pub dependency_edges: usize,
    pub extension_edges: usize,
    pub extension_points: usize,
    pub capability_bindings: usize,
}

fn has_cycle(adjacency: &BTreeMap<ElmId, Vec<ElmId>>) -> bool {
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for node in adjacency.keys().copied() {
        if visit(node, adjacency, &mut visiting, &mut visited) {
            return true;
        }
    }
    false
}

fn visit(
    node: ElmId,
    adjacency: &BTreeMap<ElmId, Vec<ElmId>>,
    visiting: &mut BTreeSet<ElmId>,
    visited: &mut BTreeSet<ElmId>,
) -> bool {
    if visited.contains(&node) {
        return false;
    }
    if !visiting.insert(node) {
        return true;
    }
    if let Some(next_nodes) = adjacency.get(&node) {
        for next in next_nodes {
            if visit(*next, adjacency, visiting, visited) {
                return true;
            }
        }
    }
    visiting.remove(&node);
    visited.insert(node);
    false
}

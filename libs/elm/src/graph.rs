//! ELM cell、依赖、能力绑定与 extension 的一致性图模型。
//!
//! [`BindingGraph`] 同时维护 parent tree、dependency edge、Nexus binding、补缀点和 extension
//! edge。所有新增和移除都必须保持端点存在、generation 匹配、契约一致、id 唯一以及禁止的
//! 环不存在。管理预检使用只读验证报告，提交阶段再在核心事务锁内重新检查。
//!
//! graph 只表达关系，不执行 provider 调用或资源回收。移除报告列出受影响的子单元、依赖者、
//! extension、binding 和 lease，供生命周期事务决定是否阻断、级联或排空。

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{ElmError, ElmResult};
use crate::ids::{BindingId, ElmId, Generation, LeaseId, PortId};
use crate::manifest::ElmManifest;
use crate::nexus::FlowContract;
pub use crate::wire::ElmMixinMode;

#[derive(Debug, Clone, PartialEq, Eq)]
/// child cell 到唯一 parent cell 的有向层级边。
pub struct ParentEdge {
    /// 子对象或子 cell 的标识符。
    pub child: ElmId,
    /// 父对象或父 cell 的标识符，用于建立层级关系。
    pub parent: ElmId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// consumer cell 对 provider cell 的必需或可选依赖边。
pub struct DependencyEdge {
    /// 消费该能力、export 或资源的 cell id。
    pub consumer: ElmId,
    /// 提供该能力或处理入口的 cell/port 引用。
    pub provider: ElmId,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: FlowContract,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 由目标 cell 拥有、允许 extension 附着的命名补缀点。
pub struct ExtensionPoint {
    /// 拥有该对象的 cell id；所有生命周期和权限检查都归属于该 owner。
    pub owner: ElmId,
    /// 对象的固定长度名称缓冲区；实际字符串以首个零字节结束。
    pub name: String,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: FlowContract,
    /// 端口、绑定或扩展点采用的并发/分发模式编码。
    pub mode: ElmMixinMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// extension cell 到目标补缀点的已验证附着关系。
pub struct ExtensionEdge {
    /// `extension` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub extension: ElmId,
    /// 关系、重定位或调用的目标对象。
    pub target: ElmId,
    /// 补缀点的完整 identifier，通常包含阶段后缀。
    pub point: String,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: FlowContract,
    /// mixin/provider 处理器自身的调用契约。
    pub handler_contract: FlowContract,
    /// 同一扩展点中的调度优先级；排序规则由扩展运行时定义。
    pub priority: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 两个端口之间携带 generation 与 lease 的已提交能力绑定边。
pub struct CapabilityBindingEdge {
    /// 该对象在所属表或运行时注册表中的稳定标识符。
    pub id: BindingId,
    /// 消费该能力、export 或资源的 cell id。
    pub consumer: ElmId,
    /// 该记录关联的 port id。
    pub port: PortId,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: FlowContract,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: Generation,
    /// 该记录关联的 lease id。
    pub lease: Option<LeaseId>,
    /// `active` 表示该条件在当前快照或计划中是否成立。
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
/// 对整个 binding graph 执行一致性检查得到的错误计数和诊断摘要。
pub struct GraphValidationReport {
    /// 当前快照或移除报告包含的 cell 集合。
    pub cells: usize,
    /// 当前图中的父子关系边集合。
    pub parent_edges: usize,
    /// 当前图中的依赖关系边集合。
    pub dependency_edges: usize,
    /// 当前图中的 extension 附着边集合。
    pub extension_edges: usize,
    /// 当前图中的能力 binding 边集合。
    pub capability_bindings: usize,
}

#[derive(Debug, Clone, Default)]
/// 保存全部 cell、关系边、端口和补缀点索引的 ELM 一致性图。
pub struct BindingGraph {
    cells: BTreeMap<ElmId, ElmManifest>,
    parents: BTreeMap<ElmId, ElmId>,
    dependencies: Vec<DependencyEdge>,
    extension_points: BTreeMap<(ElmId, String), ExtensionPoint>,
    extensions: Vec<ExtensionEdge>,
    capability_bindings: Vec<CapabilityBindingEdge>,
}

impl BindingGraph {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
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

    /// 执行 `insert_cell` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn insert_cell(&mut self, id: ElmId, manifest: ElmManifest) -> ElmResult<()> {
        if self.cells.contains_key(&id) {
            return Err(ElmError::DuplicateCell);
        }
        self.cells.insert(id, manifest);
        Ok(())
    }

    /// 执行 `try_reserve_edges` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn try_reserve_edges(
        &mut self,
        dependencies: usize,
        extensions: usize,
        capability_bindings: usize,
    ) -> ElmResult<()> {
        self.dependencies
            .try_reserve(dependencies)
            .map_err(|_| ElmError::LeaseBusy)?;
        self.extensions
            .try_reserve(extensions)
            .map_err(|_| ElmError::LeaseBusy)?;
        self.capability_bindings
            .try_reserve(capability_bindings)
            .map_err(|_| ElmError::LeaseBusy)?;
        Ok(())
    }

    /// 更新 `parent`，同时保持所属类型定义的不变量。
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

    /// 向模型注册 `dependency`，并拒绝重复 id、非法关系或环。
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

    /// 向模型注册 `extension_point`，并拒绝重复 id、非法关系或环。
    pub fn add_extension_point(
        &mut self,
        owner: ElmId,
        name: impl Into<String>,
        contract: FlowContract,
    ) -> ElmResult<()> {
        self.add_extension_point_with_mode(owner, name, contract, ElmMixinMode::Chain)
    }

    /// 向模型注册 `extension_point_with_mode`，并拒绝重复 id、非法关系或环。
    pub fn add_extension_point_with_mode(
        &mut self,
        owner: ElmId,
        name: impl Into<String>,
        contract: FlowContract,
        mode: ElmMixinMode,
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
                mode,
            },
        );
        Ok(())
    }

    /// 向模型注册 `extension`，并拒绝重复 id、非法关系或环。
    pub fn add_extension(
        &mut self,
        extension: ElmId,
        target: ElmId,
        point: impl Into<String>,
        contract: FlowContract,
    ) -> ElmResult<()> {
        let handler_contract = contract.clone();
        self.add_extension_with_dispatch(extension, target, point, contract, handler_contract, 0)
    }

    /// 向模型注册 `extension_with_dispatch`，并拒绝重复 id、非法关系或环。
    pub fn add_extension_with_dispatch(
        &mut self,
        extension: ElmId,
        target: ElmId,
        point: impl Into<String>,
        contract: FlowContract,
        handler_contract: FlowContract,
        priority: i32,
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
        if extension_point.mode == ElmMixinMode::Exclusive
            && self
                .extensions
                .iter()
                .any(|edge| edge.target == target && edge.point == point)
        {
            return Err(ElmError::DuplicateBinding);
        }
        if self.extensions.iter().any(|edge| {
            edge.extension == extension
                && edge.target == target
                && edge.point == point
                && edge.contract == contract
        }) {
            return Err(ElmError::DuplicateBinding);
        }
        self.extensions.push(ExtensionEdge {
            extension,
            target,
            point,
            contract,
            handler_contract,
            priority,
        });
        if self.has_extension_cycle() {
            self.extensions.pop();
            return Err(ElmError::ExtensionCycle);
        }
        Ok(())
    }

    /// 向模型注册 `capability_binding`，并拒绝重复 id、非法关系或环。
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

    /// 验证当前对象及其关联记录满足全部结构、范围和关系不变量。
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
        for edge in &self.extensions {
            self.require_cell(edge.extension)?;
            self.require_cell(edge.target)?;
            let Some(point) = self
                .extension_points
                .get(&(edge.target, edge.point.clone()))
            else {
                return Err(ElmError::ExtensionPointNotFound);
            };
            if point.contract != edge.contract {
                return Err(ElmError::ContractMismatch);
            }
            if point.mode == ElmMixinMode::Exclusive
                && self
                    .extensions
                    .iter()
                    .filter(|candidate| {
                        candidate.target == edge.target && candidate.point == edge.point
                    })
                    .count()
                    != 1
            {
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

    /// 执行 `cell` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn cell(&self, id: ElmId) -> Option<&ElmManifest> {
        self.cells.get(&id)
    }

    /// 从模型移除 `cell`，并返回或检查受影响关系。
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

        let before_extension_points = self.extension_points.len();
        self.extension_points.retain(|(owner, _), _| *owner != id);
        let extension_point_count = before_extension_points - self.extension_points.len();

        self.cells.remove(&id);
        Ok(GraphRemovalReport {
            parent_edges,
            dependency_edges,
            extension_edges,
            extension_points: extension_point_count,
            capability_bindings,
        })
    }

    /// 从模型移除 `cell_relations`，并返回或检查受影响关系。
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

        let before_extension_points = self.extension_points.len();
        self.extension_points.retain(|(owner, _), _| *owner != id);
        let extension_point_count = before_extension_points - self.extension_points.len();

        Ok(GraphRemovalReport {
            parent_edges: 0,
            dependency_edges,
            extension_edges,
            extension_points: extension_point_count,
            capability_bindings,
        })
    }

    /// 执行 `parent` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn parent(&self, child: ElmId) -> Option<ElmId> {
        self.parents.get(&child).copied()
    }

    /// 执行 `parent_edges` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn parent_edges(&self) -> Vec<ParentEdge> {
        self.parents
            .iter()
            .map(|(child, parent)| ParentEdge {
                child: *child,
                parent: *parent,
            })
            .collect()
    }

    /// 执行 `children_of` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `dependencies` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn dependencies(&self) -> &[DependencyEdge] {
        &self.dependencies
    }

    /// 执行 `dependents_of` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `dependent_count` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn dependent_count(&self, provider: ElmId) -> usize {
        self.dependencies
            .iter()
            .filter(|edge| edge.provider == provider)
            .count()
    }

    /// 执行 `extension_points` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn extension_points(&self) -> Vec<ExtensionPoint> {
        self.extension_points.values().cloned().collect()
    }

    /// 执行 `extension_points_iter` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn extension_points_iter(&self) -> impl Iterator<Item = &ExtensionPoint> {
        self.extension_points.values()
    }

    /// 执行 `extension_point` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn extension_point(&self, owner: ElmId, name: &str) -> Option<&ExtensionPoint> {
        self.extension_points
            .iter()
            .find(|((current_owner, current_name), _)| {
                *current_owner == owner && current_name.as_str() == name
            })
            .map(|(_, point)| point)
    }

    /// 执行 `extensions` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn extensions(&self) -> &[ExtensionEdge] {
        &self.extensions
    }

    /// 执行 `extensions_targeting` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `extension_target_count` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn extension_target_count(&self, target: ElmId) -> usize {
        self.extensions
            .iter()
            .filter(|edge| edge.target == target)
            .count()
    }

    /// 执行 `extension_exists` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn extension_exists(
        &self,
        extension: ElmId,
        target: ElmId,
        point: &str,
        contract: &FlowContract,
    ) -> bool {
        self.extensions.iter().any(|edge| {
            edge.extension == extension
                && edge.target == target
                && edge.point == point
                && &edge.contract == contract
        })
    }

    /// 从模型移除 `extension`，并返回或检查受影响关系。
    pub fn remove_extension(
        &mut self,
        extension: ElmId,
        target: ElmId,
        point: &str,
    ) -> Option<ExtensionEdge> {
        self.extensions
            .iter()
            .position(|edge| {
                edge.extension == extension && edge.target == target && edge.point == point
            })
            .map(|index| self.extensions.remove(index))
    }

    /// 执行 `capability_bindings` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn capability_bindings(&self) -> &[CapabilityBindingEdge] {
        &self.capability_bindings
    }

    /// 执行 `capability_binding` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn capability_binding(&self, id: BindingId) -> Option<&CapabilityBindingEdge> {
        self.capability_bindings.iter().find(|edge| edge.id == id)
    }

    /// 执行 `capability_binding_for` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `capability_bindings_for_cell` 定义的模型或协议操作；返回值反映校验后的结果。
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

    /// 执行 `capability_bindings_mut_for_cell` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn capability_bindings_mut_for_cell(
        &mut self,
        consumer: ElmId,
    ) -> impl Iterator<Item = &mut CapabilityBindingEdge> {
        self.capability_bindings
            .iter_mut()
            .filter(move |edge| edge.consumer == consumer)
    }

    /// 从模型移除 `capability_binding`，并返回或检查受影响关系。
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
/// 移除 cell 前枚举子单元、依赖者、extension、binding、lease 和菜单影响的报告。
pub struct GraphRemovalReport {
    /// 当前图中的父子关系边集合。
    pub parent_edges: usize,
    /// 当前图中的依赖关系边集合。
    pub dependency_edges: usize,
    /// 当前图中的 extension 附着边集合。
    pub extension_edges: usize,
    /// 该单元允许其他 ELM 附着的补缀点集合。
    pub extension_points: usize,
    /// 当前图中的能力 binding 边集合。
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

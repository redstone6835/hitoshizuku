//! ELM 核心全局状态。

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use elm_model::{
    ActionId, BindingGraph, BindingId, ELM_LIFECYCLE_REASON_BUILTIN_PROTECTED,
    ELM_LIFECYCLE_REASON_CELL_NOT_FOUND, ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
    ELM_LIFECYCLE_REASON_HAS_CHILDREN, ELM_LIFECYCLE_REASON_INVALID_STATE,
    ELM_LIFECYCLE_REASON_LEASE_BUSY, ELM_LIFECYCLE_REASON_NATIVE_TODO, ELM_LIFECYCLE_REASON_NONE,
    ELM_MENU_FLAG_REQUIRES_SYS_ADMIN, ELM_MENU_FLAG_TODO, ELM_MGR_STATUS_BUSY,
    ELM_MGR_STATUS_INVALID, ELM_MGR_STATUS_NOT_FOUND, ELM_MGR_STATUS_OK, ELM_MGR_STATUS_PERMISSION,
    ELM_MGR_STATUS_TODO, ElmCoreInfo, ElmEbiArch, ElmEbiLoadStatus, ElmEbiUnit, ElmError,
    ElmEventRecord, ElmEventSequence, ElmId, ElmKind, ElmLifecycleResponse, ElmLoadCellResponse,
    ElmManifest, ElmMenuItemKind, ElmName, ElmState, ElmVersion, FlowContract, FlowMode,
    Generation, IntentKind, LeaseId, LeaseKind, LeaseRegistry, LeaseRights, NexusIntent,
    NexusOffer, PortDescriptor, PortId, ResourceLease, TopologyEventKind, builtin_port_descriptors,
    state_code,
};
use sched::sync::Spinlock;

use super::menu::MenuItemRuntime;
use super::ports::PortRuntime;

pub(crate) const ELM_MGR_ID: ElmId = ElmId(1);
pub(crate) const ELM_MENU_DEMO_ID: ElmId = ElmId(2);
const ELM_MENU_DEMO_ACTION_ID: ActionId = ActionId(1);
const ELM_MENU_DEMO_BINDING_ID: BindingId = BindingId(1);
const ELM_MENU_DEMO_LEASE_ID: LeaseId = LeaseId(1);
const FIRST_DYNAMIC_CELL_ID: u64 = 100;
const EVENT_RING_LIMIT: usize = 128;

static CORE: Spinlock<ElmCore> = Spinlock::new(ElmCore::new());

#[derive(Debug, Clone)]
pub(crate) struct CellRuntime {
    pub id: ElmId,
    pub parent: Option<ElmId>,
    pub state: ElmState,
    pub kind: ElmKind,
    pub generation: Generation,
    pub name: String,
    pub ebi_arch: ElmEbiArch,
    pub ebi_status: ElmEbiLoadStatus,
    pub has_native_code: bool,
    pub owned_bindings: Vec<BindingId>,
    pub owned_menu_items: Vec<u64>,
}

pub(crate) struct ElmCore {
    initialized: bool,
    graph: BindingGraph,
    cells: Vec<CellRuntime>,
    ports: Vec<PortRuntime>,
    menu_items: Vec<MenuItemRuntime>,
    menu_generation: Generation,
    leases: LeaseRegistry,
    events: Vec<ElmEventRecord>,
    next_event_sequence: ElmEventSequence,
    acknowledged_event_sequence: u64,
    #[allow(dead_code)]
    next_cell_id: u64,
    #[allow(dead_code)]
    next_binding_id: u64,
    #[allow(dead_code)]
    next_lease_id: u64,
    #[allow(dead_code)]
    next_action_id: u64,
    #[allow(dead_code)]
    next_menu_item_id: u64,
}

impl ElmCore {
    pub const fn new() -> Self {
        Self {
            initialized: false,
            graph: BindingGraph::new(),
            cells: Vec::new(),
            ports: Vec::new(),
            menu_items: Vec::new(),
            menu_generation: Generation::FIRST,
            leases: LeaseRegistry::new(),
            events: Vec::new(),
            next_event_sequence: ElmEventSequence::FIRST,
            acknowledged_event_sequence: 0,
            next_cell_id: FIRST_DYNAMIC_CELL_ID,
            next_binding_id: 100,
            next_lease_id: 100,
            next_action_id: 100,
            next_menu_item_id: 100,
        }
    }

    pub fn init_builtin_mgr(&mut self) -> Result<(), ElmError> {
        if self.initialized {
            return Ok(());
        }

        let manifest = ElmManifest::new(
            ElmName::new("elm-mgr")?,
            ElmVersion::new("0.1.0")?,
            ElmKind::Manager,
        )
        .with_offer(NexusOffer::new(
            FlowContract::new("mgr.menu.item@1")?,
            FlowMode::Ordered,
        ));
        self.graph.insert_cell(ELM_MGR_ID, manifest)?;
        self.graph.add_extension_point(
            ELM_MGR_ID,
            "menu.item",
            FlowContract::new("mgr.menu.item@1")?,
        )?;
        self.cells.push(CellRuntime {
            id: ELM_MGR_ID,
            parent: None,
            state: ElmState::Active,
            kind: ElmKind::Manager,
            generation: Generation::FIRST,
            name: "elm-mgr".to_string(),
            ebi_arch: ElmEbiArch::Any,
            ebi_status: ElmEbiLoadStatus::Ok,
            has_native_code: false,
            owned_bindings: Vec::new(),
            owned_menu_items: Vec::new(),
        });
        self.emit(TopologyEventKind::CellAdded, Some(ELM_MGR_ID));
        self.emit(TopologyEventKind::CellStateChanged, Some(ELM_MGR_ID));
        self.register_builtin_ports();
        self.register_builtin_menu_demo()?;
        self.initialized = true;
        log::info!("[elm] Core initialized with builtin elm-mgr and menu extension");
        Ok(())
    }

    pub fn core_info(&self) -> ElmCoreInfo {
        ElmCoreInfo::new(
            self.cells.len() as u32,
            self.ports.len() as u32,
            self.lease_count() as u32,
            self.last_event_sequence(),
        )
    }

    pub fn cells(&self) -> &[CellRuntime] {
        &self.cells
    }

    pub fn ports(&self) -> &[PortRuntime] {
        &self.ports
    }

    pub fn menu_items(&self) -> &[MenuItemRuntime] {
        &self.menu_items
    }

    pub fn menu_generation(&self) -> Generation {
        self.menu_generation
    }

    pub fn lease_count(&self) -> usize {
        self.leases.len()
    }

    pub fn last_event_sequence(&self) -> u64 {
        self.next_event_sequence.0.saturating_sub(1)
    }

    pub fn read_next_event(&self) -> Option<ElmEventRecord> {
        self.events
            .iter()
            .find(|event| event.sequence > self.acknowledged_event_sequence)
            .copied()
    }

    pub fn ack_event(&mut self, sequence: u64) {
        self.acknowledged_event_sequence = self.acknowledged_event_sequence.max(sequence);
    }

    // TODO(elm): soyo 解析器接入后由容器转换层调用该协议装载入口。
    #[allow(dead_code)]
    pub fn load_ebi_unit(&mut self, unit: ElmEbiUnit, arch: ElmEbiArch) -> ElmLoadCellResponse {
        if let Err(status) = unit.validate(arch) {
            return ElmLoadCellResponse::failed(status);
        }
        let manifest = unit.manifest.clone();
        let name = manifest.name.as_str().to_string();
        let image_arch = unit.target.arch;
        let id = self.alloc_cell_id();

        if let Err(err) = self.insert_loaded_cell(id, manifest, name, image_arch, &unit) {
            log::error!("[elm] EBI cell rejected by runtime: {:?}", err);
            return ElmLoadCellResponse::failed(ElmEbiLoadStatus::RuntimeRejected);
        }

        if unit.has_native_code() {
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::NativeCodeTodo,
                id.0,
                state_code(ElmState::Loaded),
                0,
            );
        }

        if let Err(err) = self.activate_loaded_cell(id, &unit) {
            log::error!("[elm] EBI cell activation rejected by runtime: {:?}", err);
            return ElmLoadCellResponse::new(
                ElmEbiLoadStatus::RuntimeRejected,
                id.0,
                state_code(self.cell_state(id).unwrap_or(ElmState::Loaded)),
                0,
            );
        }

        ElmLoadCellResponse::new(ElmEbiLoadStatus::Ok, id.0, state_code(ElmState::Active), 0)
    }

    pub fn pause_cell(&mut self, id: ElmId) -> ElmLifecycleResponse {
        if let Some(response) = self.reject_common_lifecycle_target(id, true) {
            return response;
        }

        match self.cell_state(id).unwrap_or(ElmState::Retired) {
            ElmState::Active => {
                if self.transition_cell_state(id, ElmState::Quiescing).is_err()
                    || self.transition_cell_state(id, ElmState::Paused).is_err()
                {
                    return self.lifecycle_response(
                        id,
                        ELM_MGR_STATUS_INVALID,
                        ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                        0,
                        0,
                    );
                }
                self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0)
            }
            ElmState::Paused => {
                self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0)
            }
            _ => self.lifecycle_response(
                id,
                ELM_MGR_STATUS_INVALID,
                ELM_LIFECYCLE_REASON_INVALID_STATE,
                0,
                0,
            ),
        }
    }

    pub fn resume_cell(&mut self, id: ElmId) -> ElmLifecycleResponse {
        if let Some(response) = self.reject_common_lifecycle_target(id, true) {
            return response;
        }

        match self.cell_state(id).unwrap_or(ElmState::Retired) {
            ElmState::Paused => {
                if self.transition_cell_state(id, ElmState::Active).is_err() {
                    return self.lifecycle_response(
                        id,
                        ELM_MGR_STATUS_INVALID,
                        ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                        0,
                        0,
                    );
                }
                self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0)
            }
            ElmState::Active => {
                self.lifecycle_response(id, ELM_MGR_STATUS_OK, ELM_LIFECYCLE_REASON_NONE, 0, 0)
            }
            _ => self.lifecycle_response(
                id,
                ELM_MGR_STATUS_INVALID,
                ELM_LIFECYCLE_REASON_INVALID_STATE,
                0,
                0,
            ),
        }
    }

    pub fn detach_cell(&mut self, id: ElmId) -> ElmLifecycleResponse {
        if self.cell_index(id).is_none() {
            return ElmLifecycleResponse::new(
                id.0,
                ELM_MGR_STATUS_NOT_FOUND,
                0,
                0,
                0,
                ELM_LIFECYCLE_REASON_CELL_NOT_FOUND,
            );
        }
        if self.is_builtin_cell(id) {
            return self.lifecycle_response(
                id,
                ELM_MGR_STATUS_PERMISSION,
                ELM_LIFECYCLE_REASON_BUILTIN_PROTECTED,
                0,
                0,
            );
        }
        if self.has_live_children(id) {
            return self.lifecycle_response(
                id,
                ELM_MGR_STATUS_BUSY,
                ELM_LIFECYCLE_REASON_HAS_CHILDREN,
                0,
                0,
            );
        }

        match self.cell_state(id).unwrap_or(ElmState::Retired) {
            ElmState::Active => {
                if self.transition_cell_state(id, ElmState::Quiescing).is_err() {
                    return self.lifecycle_response(
                        id,
                        ELM_MGR_STATUS_INVALID,
                        ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                        0,
                        0,
                    );
                }
            }
            ElmState::Quiescing
            | ElmState::Paused
            | ElmState::Loaded
            | ElmState::Quarantined
            | ElmState::Detached => {}
            _ => {
                return self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    ELM_LIFECYCLE_REASON_INVALID_STATE,
                    0,
                    0,
                );
            }
        }

        let revoked_leases = match self.leases.revoke_and_remove_owned_by(id) {
            Ok(ids) => ids,
            Err(ElmError::LeaseBusy) => {
                return self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_BUSY,
                    ELM_LIFECYCLE_REASON_LEASE_BUSY,
                    0,
                    0,
                );
            }
            Err(_) => {
                return self.lifecycle_response(
                    id,
                    ELM_MGR_STATUS_INVALID,
                    ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                    0,
                    0,
                );
            }
        };
        for lease in &revoked_leases {
            self.emit_lease(TopologyEventKind::LeaseRevoked, *lease);
        }

        let removed_menu_items = self.remove_menu_items_owned_by(id);
        let removed_bindings = self.take_owned_bindings(id);
        for binding in removed_bindings {
            self.emit_binding(TopologyEventKind::BindingRemoved, binding);
        }

        if self.graph.remove_cell(id).is_err() {
            return self.lifecycle_response(
                id,
                ELM_MGR_STATUS_INVALID,
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                revoked_leases.len() as u32,
                removed_menu_items as u32,
            );
        }

        let detach_result = match self.cell_state(id).unwrap_or(ElmState::Retired) {
            ElmState::Loaded => self.transition_cell_state(id, ElmState::Detached),
            ElmState::Paused => self.transition_cell_state(id, ElmState::Detached),
            ElmState::Quiescing => self.transition_cell_state(id, ElmState::Detached),
            ElmState::Quarantined => self.transition_cell_state(id, ElmState::Detached),
            ElmState::Detached => Ok(()),
            _ => Err(ElmError::InvalidTransition),
        };
        if detach_result.is_err() || self.transition_cell_state(id, ElmState::Retired).is_err() {
            return self.lifecycle_response(
                id,
                ELM_MGR_STATUS_INVALID,
                ELM_LIFECYCLE_REASON_GRAPH_INCONSISTENT,
                revoked_leases.len() as u32,
                removed_menu_items as u32,
            );
        }
        self.remove_cell_runtime(id);
        self.emit(TopologyEventKind::CellRemoved, Some(id));

        ElmLifecycleResponse::new(
            id.0,
            ELM_MGR_STATUS_OK,
            state_code(ElmState::Retired),
            revoked_leases.len() as u32,
            removed_menu_items as u32,
            ELM_LIFECYCLE_REASON_NONE,
        )
    }

    pub fn debug_dump_bytes(&self) -> Vec<u8> {
        // TODO(elm): 后续输出完整绑定图、租约表、端口提供者健康状态和故障隔离原因。
        let mut out = format!(
            "ELM Core 诊断\ncells={}\nports={}\nleases={}\nmenu_items={}\nlast_event_sequence={}\nTODO(elm): 尚未实现完整诊断明细\n",
            self.cells.len(),
            self.ports.len(),
            self.lease_count(),
            self.menu_items.len(),
            self.last_event_sequence(),
        );
        for cell in &self.cells {
            out.push_str(
                format!(
                    "cell id={} name={} state={:?} ebi_arch={:?} ebi_status={:?} native_code={}\n",
                    cell.id.0,
                    cell.name,
                    cell.state,
                    cell.ebi_arch,
                    cell.ebi_status,
                    cell.has_native_code,
                )
                .as_str(),
            );
        }
        out.into_bytes()
    }

    fn register_builtin_ports(&mut self) {
        for desc in builtin_port_descriptors() {
            self.register_port(desc);
        }
        log::info!("[elm] registered {} Nexus ports", self.ports.len());
    }

    fn register_port(&mut self, desc: PortDescriptor) {
        let port = desc.id;
        self.ports.push(PortRuntime::new(desc));
        self.emit_port(TopologyEventKind::PortAdded, port);
        if !desc.implemented {
            log::debug!(
                "[elm] port {} registered as TODO(elm) 提供者",
                desc.contract
            );
        }
    }

    fn register_builtin_menu_demo(&mut self) -> Result<(), ElmError> {
        let menu_contract = FlowContract::new("mgr.menu.item@1")?;
        let manifest = ElmManifest::new(
            ElmName::new("elm-menu-demo")?,
            ElmVersion::new("0.1.0")?,
            ElmKind::Extension,
        )
        .with_intent(NexusIntent::new(IntentKind::Extend, menu_contract.clone()));

        self.graph.insert_cell(ELM_MENU_DEMO_ID, manifest)?;
        self.graph.set_parent(ELM_MENU_DEMO_ID, ELM_MGR_ID)?;
        self.graph
            .add_extension(ELM_MENU_DEMO_ID, ELM_MGR_ID, "menu.item", menu_contract)?;
        self.cells.push(CellRuntime {
            id: ELM_MENU_DEMO_ID,
            parent: Some(ELM_MGR_ID),
            state: ElmState::Active,
            kind: ElmKind::Extension,
            generation: Generation::FIRST,
            name: "elm-menu-demo".to_string(),
            ebi_arch: ElmEbiArch::Any,
            ebi_status: ElmEbiLoadStatus::Ok,
            has_native_code: false,
            owned_bindings: vec![ELM_MENU_DEMO_BINDING_ID],
            owned_menu_items: vec![1],
        });
        self.leases.insert(ResourceLease::new(
            ELM_MENU_DEMO_LEASE_ID,
            ELM_MENU_DEMO_ID,
            LeaseKind::MenuItem,
            LeaseRights::CONTROL,
            Generation::FIRST,
        ))?;
        self.menu_items.push(MenuItemRuntime::new(
            1,
            ELM_MENU_DEMO_ID,
            ELM_MENU_DEMO_ACTION_ID,
            ElmMenuItemKind::Action,
            ELM_MENU_FLAG_TODO | ELM_MENU_FLAG_REQUIRES_SYS_ADMIN,
            "ELM 状态",
            "查看 ELM Core 拓扑、事件和菜单状态",
            "elm/status",
        ));
        self.menu_generation = self.menu_generation.next();
        self.emit(TopologyEventKind::CellAdded, Some(ELM_MENU_DEMO_ID));
        self.emit(TopologyEventKind::CellStateChanged, Some(ELM_MENU_DEMO_ID));
        self.emit_binding(TopologyEventKind::BindingAdded, ELM_MENU_DEMO_BINDING_ID);
        self.emit_lease(TopologyEventKind::LeaseAdded, ELM_MENU_DEMO_LEASE_ID);
        self.emit(TopologyEventKind::MenuItemAdded, Some(ELM_MENU_DEMO_ID));
        Ok(())
    }

    #[allow(dead_code)]
    fn insert_loaded_cell(
        &mut self,
        id: ElmId,
        manifest: ElmManifest,
        name: String,
        ebi_arch: ElmEbiArch,
        unit: &ElmEbiUnit,
    ) -> Result<(), ElmError> {
        let kind = manifest.kind;
        self.graph.insert_cell(id, manifest)?;
        self.graph.set_parent(id, ELM_MGR_ID)?;
        self.cells.push(CellRuntime {
            id,
            parent: Some(ELM_MGR_ID),
            state: ElmState::Discovered,
            kind,
            generation: Generation::FIRST,
            name,
            ebi_arch,
            ebi_status: if unit.has_native_code() {
                ElmEbiLoadStatus::NativeCodeTodo
            } else {
                ElmEbiLoadStatus::Ok
            },
            has_native_code: unit.has_native_code(),
            owned_bindings: Vec::new(),
            owned_menu_items: Vec::new(),
        });
        self.emit(TopologyEventKind::CellAdded, Some(id));
        self.transition_cell_state(id, ElmState::Verified)?;
        self.transition_cell_state(id, ElmState::Loaded)?;
        Ok(())
    }

    #[allow(dead_code)]
    fn activate_loaded_cell(&mut self, id: ElmId, unit: &ElmEbiUnit) -> Result<(), ElmError> {
        if let Some(menu) = &unit.menu {
            let menu_contract = FlowContract::new("mgr.menu.item@1")?;
            self.graph
                .add_extension(id, ELM_MGR_ID, "menu.item", menu_contract)?;
            let binding = self.alloc_binding_id();
            self.emit_binding(TopologyEventKind::BindingAdded, binding);

            let lease = self.alloc_lease_id();
            self.leases.insert(ResourceLease::new(
                lease,
                id,
                LeaseKind::MenuItem,
                LeaseRights::CONTROL,
                Generation::FIRST,
            ))?;
            self.emit_lease(TopologyEventKind::LeaseAdded, lease);

            let action = self.alloc_action_id();
            let menu_id = self.alloc_menu_item_id();
            self.menu_items.push(MenuItemRuntime::new(
                menu_id,
                id,
                action,
                menu.kind,
                menu.flags | ELM_MENU_FLAG_TODO,
                &menu.label,
                &menu.description,
                &menu.route,
            ));
            let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) else {
                return Err(ElmError::CellNotFound);
            };
            cell.owned_bindings.push(binding);
            cell.owned_menu_items.push(menu_id);
            self.menu_generation = self.menu_generation.next();
            self.emit(TopologyEventKind::MenuItemAdded, Some(id));
        }

        self.transition_cell_state(id, ElmState::Linked)?;
        self.transition_cell_state(id, ElmState::Ready)?;
        self.transition_cell_state(id, ElmState::Active)?;
        Ok(())
    }

    fn transition_cell_state(&mut self, id: ElmId, to: ElmState) -> Result<(), ElmError> {
        let Some(cell) = self.cells.iter_mut().find(|cell| cell.id == id) else {
            return Err(ElmError::CellNotFound);
        };
        cell.state.transition_to(to)?;
        cell.state = to;
        self.emit(TopologyEventKind::CellStateChanged, Some(id));
        Ok(())
    }

    fn cell_state(&self, id: ElmId) -> Option<ElmState> {
        self.cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.state)
    }

    fn cell_index(&self, id: ElmId) -> Option<usize> {
        self.cells.iter().position(|cell| cell.id == id)
    }

    fn is_builtin_cell(&self, id: ElmId) -> bool {
        id.0 < FIRST_DYNAMIC_CELL_ID
    }

    fn has_live_children(&self, id: ElmId) -> bool {
        self.cells
            .iter()
            .any(|cell| cell.parent == Some(id) && cell.state != ElmState::Retired)
    }

    fn cell_has_native_code(&self, id: ElmId) -> bool {
        self.cells
            .iter()
            .find(|cell| cell.id == id)
            .map(|cell| cell.has_native_code)
            .unwrap_or(false)
    }

    fn reject_common_lifecycle_target(
        &self,
        id: ElmId,
        reject_native: bool,
    ) -> Option<ElmLifecycleResponse> {
        if self.cell_index(id).is_none() {
            return Some(ElmLifecycleResponse::new(
                id.0,
                ELM_MGR_STATUS_NOT_FOUND,
                0,
                0,
                0,
                ELM_LIFECYCLE_REASON_CELL_NOT_FOUND,
            ));
        }
        if self.is_builtin_cell(id) {
            return Some(self.lifecycle_response(
                id,
                ELM_MGR_STATUS_PERMISSION,
                ELM_LIFECYCLE_REASON_BUILTIN_PROTECTED,
                0,
                0,
            ));
        }
        if reject_native && self.cell_has_native_code(id) {
            return Some(self.lifecycle_response(
                id,
                ELM_MGR_STATUS_TODO,
                ELM_LIFECYCLE_REASON_NATIVE_TODO,
                0,
                0,
            ));
        }
        None
    }

    fn lifecycle_response(
        &self,
        id: ElmId,
        status: i32,
        reason: u32,
        revoked_leases: u32,
        removed_menu_items: u32,
    ) -> ElmLifecycleResponse {
        ElmLifecycleResponse::new(
            id.0,
            status,
            self.cell_state(id).map(state_code).unwrap_or(0),
            revoked_leases,
            removed_menu_items,
            reason,
        )
    }

    fn remove_menu_items_owned_by(&mut self, id: ElmId) -> usize {
        let Some(index) = self.cell_index(id) else {
            return 0;
        };
        let owned = core::mem::take(&mut self.cells[index].owned_menu_items);
        if owned.is_empty() {
            return 0;
        }

        let before = self.menu_items.len();
        self.menu_items
            .retain(|item| !owned.iter().any(|owned_id| *owned_id == item.id));
        let removed = before - self.menu_items.len();
        if removed != 0 {
            self.menu_generation = self.menu_generation.next();
            self.emit(TopologyEventKind::MenuItemRemoved, Some(id));
        }
        removed
    }

    fn take_owned_bindings(&mut self, id: ElmId) -> Vec<BindingId> {
        let Some(index) = self.cell_index(id) else {
            return Vec::new();
        };
        core::mem::take(&mut self.cells[index].owned_bindings)
    }

    fn remove_cell_runtime(&mut self, id: ElmId) {
        self.cells.retain(|cell| cell.id != id);
    }

    #[allow(dead_code)]
    fn alloc_cell_id(&mut self) -> ElmId {
        let id = ElmId(self.next_cell_id);
        self.next_cell_id += 1;
        id
    }

    #[allow(dead_code)]
    fn alloc_binding_id(&mut self) -> BindingId {
        let id = BindingId(self.next_binding_id);
        self.next_binding_id += 1;
        id
    }

    #[allow(dead_code)]
    fn alloc_lease_id(&mut self) -> LeaseId {
        let id = LeaseId(self.next_lease_id);
        self.next_lease_id += 1;
        id
    }

    #[allow(dead_code)]
    fn alloc_action_id(&mut self) -> ActionId {
        let id = ActionId(self.next_action_id);
        self.next_action_id += 1;
        id
    }

    #[allow(dead_code)]
    fn alloc_menu_item_id(&mut self) -> u64 {
        let id = self.next_menu_item_id;
        self.next_menu_item_id += 1;
        id
    }

    fn emit(&mut self, kind: TopologyEventKind, cell: Option<ElmId>) {
        let record = ElmEventRecord::new(self.next_event_sequence, kind, cell, None, None, None);
        self.push_event(record);
    }

    fn emit_port(&mut self, kind: TopologyEventKind, port: PortId) {
        let record =
            ElmEventRecord::new(self.next_event_sequence, kind, None, Some(port), None, None);
        self.push_event(record);
    }

    fn emit_binding(&mut self, kind: TopologyEventKind, binding: BindingId) {
        let record = ElmEventRecord::new(
            self.next_event_sequence,
            kind,
            None,
            None,
            Some(binding),
            None,
        );
        self.push_event(record);
    }

    fn emit_lease(&mut self, kind: TopologyEventKind, lease: LeaseId) {
        let record = ElmEventRecord::new(
            self.next_event_sequence,
            kind,
            None,
            None,
            None,
            Some(lease),
        );
        self.push_event(record);
    }

    fn push_event(&mut self, record: ElmEventRecord) {
        self.next_event_sequence = self.next_event_sequence.next();
        if self.events.len() >= EVENT_RING_LIMIT {
            self.events.remove(0);
        }
        self.events.push(record);
    }
}

pub(crate) fn with_core<R>(f: impl FnOnce(&mut ElmCore) -> R) -> R {
    let mut core = CORE.lock();
    f(&mut core)
}

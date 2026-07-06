use crate::{
    ActionId, BindingGraph, BindingId, ELM_LIFECYCLE_REASON_HAS_DEPENDENTS,
    ELM_LIFECYCLE_REASON_HAS_EXTENSIONS, ELM_LIFECYCLE_REASON_NONE, ELM_MGR_ACTION_BIND,
    ELM_MGR_ACTION_DETACH, ELM_MGR_ACTION_UNBIND, ELM_MGR_POLICY_AUDIT,
    ELM_MGR_POLICY_MENU_BINDING, ELM_MGR_POLICY_NEXUS_BINDING, ELM_MGR_POLICY_PREFLIGHT,
    ELM_MGR_STATUS_BUSY, ELM_MGR_STATUS_INVALID, ELM_MGR_STATUS_OK, ELM_MGR_STATUS_TODO,
    ELM_NEXUS_CONTRACT_LEN, ELM_POLICY_BLOCK_CONTRACT_MISMATCH, ELM_POLICY_BLOCK_DUPLICATE_BINDING,
    ELM_POLICY_BLOCK_HAS_DEPENDENTS, ELM_POLICY_BLOCK_HAS_EXTENSIONS, ELM_POLICY_BLOCK_PORT_TODO,
    ElmCellSnapshot, ElmCoreInfo, ElmCtlCommand, ElmEbiArch, ElmEbiEntry, ElmEbiLoadStatus,
    ElmEbiMenuDecl, ElmEbiSegment, ElmEbiSegmentKind, ElmEbiTarget, ElmEbiUnit, ElmError,
    ElmEventRecord, ElmId, ElmKind, ElmLifecycleAction, ElmLifecyclePlanRequest,
    ElmLifecyclePlanResponse, ElmLifecycleRequest, ElmLifecycleResponse, ElmManifest,
    ElmMenuItemKind, ElmMenuItemSnapshot, ElmMenuSnapshotHeader, ElmMgrAuditHeader,
    ElmMgrAuditRecord, ElmMgrPolicyInfo, ElmMgrRelationKind, ElmMgrRelationRecord,
    ElmMgrResponseHeader, ElmMgrTopologyHeader, ElmName, ElmNexusBindPlanResponse,
    ElmNexusBindRequest, ElmNexusBindingRecord, ElmNexusBindingSnapshotHeader,
    ElmNexusUnbindRequest, ElmPortSnapshot, ElmSnapshotHeader, ElmState, ElmVersion, FlowContract,
    FlowMode, Generation, LeaseId, LeaseKind, LeaseRegistry, LeaseRights, LeaseState, PortId,
    ResourceLease, TopologyEventKind, builtin_port_descriptors, first_lifecycle_reason,
    planned_final_state, state_code, status_from_blockers,
};

fn manifest(name: &str) -> ElmManifest {
    ElmManifest::new(
        ElmName::new(name).unwrap(),
        ElmVersion::new("0.1.0").unwrap(),
        ElmKind::Service,
    )
}

fn contract(name: &str) -> FlowContract {
    FlowContract::new(name).unwrap()
}

#[test]
fn state_machine_accepts_normal_start_path() {
    let transition = ElmState::Discovered
        .transition_to(ElmState::Verified)
        .unwrap();
    assert_eq!(transition.from, ElmState::Discovered);
    assert_eq!(transition.to, ElmState::Verified);
    assert!(ElmState::Ready.transition_to(ElmState::Active).is_ok());
}

#[test]
fn state_machine_rejects_skipping_binding() {
    assert!(matches!(
        ElmState::Loaded.transition_to(ElmState::Active),
        Err(ElmError::InvalidTransition)
    ));
}

#[test]
fn state_machine_accepts_unplug_paths() {
    assert!(ElmState::Loaded.transition_to(ElmState::Detached).is_ok());
    assert!(ElmState::Paused.transition_to(ElmState::Detached).is_ok());
    assert!(ElmState::Detached.transition_to(ElmState::Retired).is_ok());
}

#[test]
fn binding_graph_tracks_parent_dependency_and_extension() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(1), manifest("elm-mgr")).unwrap();
    graph.insert_cell(ElmId(2), manifest("menu-demo")).unwrap();
    graph.set_parent(ElmId(2), ElmId(1)).unwrap();
    graph
        .add_extension_point(ElmId(1), "menu.item", contract("mgr.menu.item@1"))
        .unwrap();
    graph
        .add_extension(ElmId(2), ElmId(1), "menu.item", contract("mgr.menu.item@1"))
        .unwrap();
    graph
        .add_dependency(ElmId(2), ElmId(1), contract("core.event@1"))
        .unwrap();

    let report = graph.validate().unwrap();
    assert_eq!(report.cells, 2);
    assert_eq!(report.parent_edges, 1);
    assert_eq!(report.dependency_edges, 1);
    assert_eq!(report.extension_edges, 1);

    let parent_edges = graph.parent_edges();
    assert_eq!(parent_edges[0].child, ElmId(2));
    assert_eq!(parent_edges[0].parent, ElmId(1));
    assert_eq!(graph.children_of(ElmId(1)), alloc::vec![ElmId(2)]);
    assert_eq!(graph.dependents_of(ElmId(1)), alloc::vec![ElmId(2)]);
    assert_eq!(graph.extensions_targeting(ElmId(1)), alloc::vec![ElmId(2)]);

    let extension_points = graph.extension_points();
    assert_eq!(extension_points[0].owner, ElmId(1));
    assert_eq!(extension_points[0].name, "menu.item");
}

#[test]
fn binding_graph_tracks_capability_bindings() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(1), manifest("elm-mgr")).unwrap();
    graph.insert_cell(ElmId(2), manifest("menu-demo")).unwrap();
    graph
        .add_capability_binding(
            BindingId(7),
            ElmId(2),
            PortId(3),
            contract("mgr.menu.item@1"),
            Generation::FIRST,
            Some(LeaseId(9)),
        )
        .unwrap();

    let report = graph.validate().unwrap();
    assert_eq!(report.capability_bindings, 1);

    let binding = graph.capability_binding(BindingId(7)).unwrap();
    assert_eq!(binding.consumer, ElmId(2));
    assert_eq!(binding.port, PortId(3));
    assert_eq!(binding.lease, Some(LeaseId(9)));
    assert!(binding.active);
    assert_eq!(
        graph.capability_bindings_for_cell(ElmId(2)),
        alloc::vec![BindingId(7)]
    );
    assert!(
        graph
            .capability_binding_for(ElmId(2), PortId(3), &contract("mgr.menu.item@1"))
            .is_some()
    );
}

#[test]
fn binding_graph_rejects_duplicate_capability_binding() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(2), manifest("menu-demo")).unwrap();
    graph
        .add_capability_binding(
            BindingId(7),
            ElmId(2),
            PortId(3),
            contract("mgr.menu.item@1"),
            Generation::FIRST,
            Some(LeaseId(9)),
        )
        .unwrap();

    assert!(matches!(
        graph.add_capability_binding(
            BindingId(8),
            ElmId(2),
            PortId(3),
            contract("mgr.menu.item@1"),
            Generation::FIRST,
            Some(LeaseId(10)),
        ),
        Err(ElmError::DuplicateBinding)
    ));
}

#[test]
fn binding_graph_rejects_parent_cycle() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(1), manifest("root")).unwrap();
    graph.insert_cell(ElmId(2), manifest("child")).unwrap();
    graph.set_parent(ElmId(2), ElmId(1)).unwrap();
    assert!(matches!(
        graph.set_parent(ElmId(1), ElmId(2)),
        Err(ElmError::ParentCycle)
    ));
}

#[test]
fn binding_graph_rejects_dependency_cycle() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(1), manifest("a")).unwrap();
    graph.insert_cell(ElmId(2), manifest("b")).unwrap();
    graph
        .add_dependency(ElmId(1), ElmId(2), contract("test.dep@1"))
        .unwrap();
    assert!(matches!(
        graph.add_dependency(ElmId(2), ElmId(1), contract("test.dep@1")),
        Err(ElmError::DependencyCycle)
    ));
}

#[test]
fn binding_graph_rejects_invalid_extension_contract() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(1), manifest("target")).unwrap();
    graph.insert_cell(ElmId(2), manifest("extension")).unwrap();
    graph
        .add_extension_point(ElmId(1), "menu.item", contract("mgr.menu.item@1"))
        .unwrap();
    assert!(matches!(
        graph.add_extension(ElmId(2), ElmId(1), "menu.item", contract("mgr.menu.item@2")),
        Err(ElmError::ContractMismatch)
    ));
}

#[test]
fn binding_graph_removes_leaf_cell_edges() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(1), manifest("elm-mgr")).unwrap();
    graph.insert_cell(ElmId(2), manifest("menu-demo")).unwrap();
    graph.set_parent(ElmId(2), ElmId(1)).unwrap();
    graph
        .add_extension_point(ElmId(1), "menu.item", contract("mgr.menu.item@1"))
        .unwrap();
    graph
        .add_extension(ElmId(2), ElmId(1), "menu.item", contract("mgr.menu.item@1"))
        .unwrap();

    let report = graph.remove_cell(ElmId(2)).unwrap();
    assert_eq!(report.parent_edges, 1);
    assert_eq!(report.extension_edges, 1);
    assert!(graph.cell(ElmId(2)).is_none());
    assert!(graph.validate().is_ok());
}

#[test]
fn binding_graph_removes_capability_binding() {
    let mut graph = BindingGraph::new();
    graph.insert_cell(ElmId(2), manifest("menu-demo")).unwrap();
    graph
        .add_capability_binding(
            BindingId(7),
            ElmId(2),
            PortId(3),
            contract("mgr.menu.item@1"),
            Generation::FIRST,
            Some(LeaseId(9)),
        )
        .unwrap();

    let removed = graph.remove_capability_binding(BindingId(7)).unwrap();
    assert_eq!(removed.consumer, ElmId(2));
    assert!(graph.capability_binding(BindingId(7)).is_none());
    assert!(matches!(
        graph.remove_capability_binding(BindingId(7)),
        Err(ElmError::BindingNotFound)
    ));
}

#[test]
fn lease_registry_revokes_owned_leases() {
    let mut registry = LeaseRegistry::new();
    registry
        .insert(ResourceLease::new(
            LeaseId(1),
            ElmId(7),
            LeaseKind::MenuItem,
            LeaseRights::CONTROL,
            Generation::FIRST,
        ))
        .unwrap();
    assert_eq!(registry.revoke_all_owned_by(ElmId(7)).unwrap(), 1);
    assert_eq!(
        registry.get(LeaseId(1)).unwrap().state,
        LeaseState::Revoking
    );
}

#[test]
fn lease_revoke_waits_for_active_refs() {
    let mut lease = ResourceLease::new(
        LeaseId(1),
        ElmId(7),
        LeaseKind::Provider,
        LeaseRights::READ,
        Generation::FIRST,
    );
    lease.active_refs = 1;
    lease.begin_revoke().unwrap();
    assert!(matches!(lease.finish_revoke(), Err(ElmError::LeaseBusy)));
    lease.active_refs = 0;
    lease.finish_revoke().unwrap();
    assert_eq!(lease.state, LeaseState::Revoked);
}

#[test]
fn lease_registry_revokes_and_removes_owned_leases() {
    let mut registry = LeaseRegistry::new();
    registry
        .insert(ResourceLease::new(
            LeaseId(1),
            ElmId(7),
            LeaseKind::MenuItem,
            LeaseRights::CONTROL,
            Generation::FIRST,
        ))
        .unwrap();

    let revoked = registry.revoke_and_remove_owned_by(ElmId(7)).unwrap();
    assert_eq!(revoked, alloc::vec![LeaseId(1)]);
    assert!(registry.get(LeaseId(1)).is_none());
    assert!(registry.is_empty());
}

#[test]
fn lease_registry_reports_busy_owned_leases() {
    let mut registry = LeaseRegistry::new();
    registry
        .insert(ResourceLease::new(
            LeaseId(1),
            ElmId(7),
            LeaseKind::Provider,
            LeaseRights::READ,
            Generation::FIRST,
        ))
        .unwrap();

    assert_eq!(registry.busy_owned_by(ElmId(7)), 0);
    registry.get_mut(LeaseId(1)).unwrap().active_refs = 2;
    assert_eq!(registry.busy_owned_by(ElmId(7)), 1);
    assert_eq!(registry.busy_owned_by(ElmId(8)), 0);
}

#[test]
fn lease_registry_tracks_binding_owner() {
    let mut registry = LeaseRegistry::new();
    registry
        .insert(
            ResourceLease::new(
                LeaseId(1),
                ElmId(7),
                LeaseKind::MenuItem,
                LeaseRights::CONTROL,
                Generation::FIRST,
            )
            .with_binding(BindingId(9)),
        )
        .unwrap();

    assert_eq!(
        registry.get_by_binding(BindingId(9)).map(|lease| lease.id),
        Some(LeaseId(1))
    );
    assert_eq!(registry.revoke_and_remove(LeaseId(1)).unwrap(), LeaseId(1));
    assert!(registry.get(LeaseId(1)).is_none());
}

#[test]
fn ctl_command_rejects_unknown_command() {
    assert_eq!(ElmCtlCommand::from_raw(1), Some(ElmCtlCommand::CoreQuery));
    assert_eq!(ElmCtlCommand::from_raw(509), None);
}

#[test]
fn core_info_reports_capabilities_and_counts() {
    let info = ElmCoreInfo::new(1, 15, 0, 7);
    assert_eq!(info.cell_count, 1);
    assert_eq!(info.port_count, 15);
    assert_eq!(info.event_sequence, 7);
    assert_ne!(info.capabilities, 0);
}

#[test]
fn snapshot_entries_truncate_names_safely() {
    let cell = ElmCellSnapshot::new(
        ElmId(1),
        None,
        ElmState::Active,
        ElmKind::Manager,
        Generation::FIRST,
        "elm-mgr",
        ElmEbiArch::Any,
        ElmEbiLoadStatus::Ok,
        false,
    );
    assert_eq!(cell.id, 1);
    assert_eq!(cell.parent, 0);
    assert_eq!(cell.name_len, 7);
    assert_eq!(&cell.name[..7], b"elm-mgr");

    let port = ElmPortSnapshot::new(
        crate::PortId(1),
        None,
        "core.log@1",
        crate::FlowDirection::Sink,
        FlowMode::Shared,
        true,
    );
    assert_eq!(port.contract_len, 10);
    assert_eq!(&port.contract[..10], b"core.log@1");
}

#[test]
fn snapshot_header_uses_fixed_entry_sizes() {
    let header = ElmSnapshotHeader::new(1, 15, 0, 9);
    assert_eq!(header.cell_count, 1);
    assert_eq!(header.port_count, 15);
    assert!(header.cell_entry_size as usize >= core::mem::size_of::<ElmCellSnapshot>());
    assert!(header.port_entry_size as usize >= core::mem::size_of::<ElmPortSnapshot>());
}

#[test]
fn builtin_ports_include_mgr_menu_and_todo_ports() {
    let ports = builtin_port_descriptors();
    assert!(ports.iter().any(|port| port.contract == "mgr.menu.item@1"));
    assert!(ports.iter().any(|port| !port.implemented));
}

#[test]
fn event_record_uses_zero_for_absent_objects() {
    let event = ElmEventRecord::new(
        crate::ElmEventSequence(3),
        TopologyEventKind::CellAdded,
        Some(ElmId(9)),
        None,
        None,
        None,
    );
    assert_eq!(event.sequence, 3);
    assert_eq!(event.cell, 9);
    assert_eq!(event.port, 0);
    assert_eq!(event.binding, 0);
    assert_eq!(event.lease, 0);
}

#[test]
fn mgr_response_header_reports_payload_status() {
    let ok = ElmMgrResponseHeader::ok(16);
    assert_eq!(ok.status, ELM_MGR_STATUS_OK);
    assert_eq!(ok.payload_len, 16);

    let invalid = ElmMgrResponseHeader::invalid();
    assert_eq!(invalid.status, ELM_MGR_STATUS_INVALID);
    assert_eq!(invalid.payload_len, 0);
}

#[test]
fn lifecycle_request_and_response_are_fixed_layout() {
    let request = ElmLifecycleRequest::new(7);
    assert_eq!(request.cell_id, 7);
    assert_eq!(core::mem::size_of::<ElmLifecycleRequest>(), 16);

    let response = ElmLifecycleResponse::new(
        7,
        ELM_MGR_STATUS_OK,
        state_code(ElmState::Paused),
        1,
        2,
        ELM_LIFECYCLE_REASON_NONE,
    );
    assert_eq!(response.cell_id, 7);
    assert_eq!(response.final_state, state_code(ElmState::Paused));
    assert_eq!(response.revoked_leases, 1);
    assert_eq!(response.removed_menu_items, 2);
    assert_eq!(core::mem::size_of::<ElmLifecycleResponse>(), 32);
}

#[test]
fn lifecycle_plan_and_mgr_policy_are_fixed_layout() {
    assert_eq!(
        ElmLifecycleAction::from_raw(ElmLifecycleAction::Detach as u32),
        Some(ElmLifecycleAction::Detach)
    );
    assert_eq!(ElmLifecycleAction::Detach.bit(), ELM_MGR_ACTION_DETACH);

    let request = ElmLifecyclePlanRequest::new(7, ElmLifecycleAction::Pause);
    assert_eq!(request.cell_id, 7);
    assert_eq!(request.action, ElmLifecycleAction::Pause as u32);
    assert_eq!(core::mem::size_of::<ElmLifecyclePlanRequest>(), 16);

    let response = ElmLifecyclePlanResponse::new(
        7,
        ElmLifecycleAction::Detach,
        false,
        ELM_MGR_STATUS_BUSY,
        state_code(ElmState::Active),
        ELM_POLICY_BLOCK_HAS_DEPENDENTS,
    )
    .with_affected(0, 1, 0);
    assert_eq!(response.allowed, 0);
    assert_eq!(response.affected_dependents, 1);
    assert_eq!(
        first_lifecycle_reason(response.blockers),
        ELM_LIFECYCLE_REASON_HAS_DEPENDENTS
    );
    assert_eq!(status_from_blockers(response.blockers), ELM_MGR_STATUS_BUSY);
    assert_eq!(core::mem::size_of::<ElmLifecyclePlanResponse>(), 48);

    let policy = ElmMgrPolicyInfo::new(128);
    assert_eq!(policy.audit_capacity, 128);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_PREFLIGHT, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_AUDIT, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_NEXUS_BINDING, 0);
    assert_ne!(policy.policy_flags & ELM_MGR_POLICY_MENU_BINDING, 0);
    assert_ne!(policy.supported_actions & ELM_MGR_ACTION_BIND, 0);
    assert_ne!(policy.supported_actions & ELM_MGR_ACTION_UNBIND, 0);
}

#[test]
fn nexus_binding_abi_records_are_fixed_layout() {
    let request = ElmNexusBindRequest::new(7, 3, "mgr.menu.item@1");
    assert_eq!(request.cell_id, 7);
    assert_eq!(request.port_id, 3);
    assert_eq!(request.contract_len, "mgr.menu.item@1".len() as u16);
    assert_eq!(request.contract.len(), ELM_NEXUS_CONTRACT_LEN);
    assert_eq!(core::mem::size_of::<ElmNexusBindRequest>(), 88);

    let response = ElmNexusBindPlanResponse::new(7, 3, 11, 12, 1, true, ELM_MGR_STATUS_OK, 0);
    assert_eq!(response.allowed, 1);
    assert_eq!(response.binding_id, 11);
    assert_eq!(response.lease_id, 12);
    assert_eq!(core::mem::size_of::<ElmNexusBindPlanResponse>(), 64);

    let unbind = ElmNexusUnbindRequest::new(11);
    assert_eq!(unbind.binding_id, 11);
    assert_eq!(core::mem::size_of::<ElmNexusUnbindRequest>(), 16);

    let header = ElmNexusBindingSnapshotHeader::new(1, 9);
    assert_eq!(header.binding_count, 1);
    assert_eq!(header.event_sequence, 9);
    assert_eq!(core::mem::size_of::<ElmNexusBindingSnapshotHeader>(), 16);

    let record = ElmNexusBindingRecord::new(11, 7, 3, 12, 1, true, "mgr.menu.item@1");
    assert_eq!(record.active, 1);
    assert_eq!(record.contract_len, "mgr.menu.item@1".len() as u16);
    assert_eq!(core::mem::size_of::<ElmNexusBindingRecord>(), 120);
}

#[test]
fn mgr_topology_and_audit_records_are_fixed_layout() {
    let relation = ElmMgrRelationRecord::new(
        ElmMgrRelationKind::Extension,
        2,
        1,
        "mgr.menu.item@1",
        "menu.item",
    );
    assert_eq!(relation.source, 2);
    assert_eq!(relation.target, 1);
    assert_eq!(relation.contract_len, "mgr.menu.item@1".len() as u16);
    assert_eq!(relation.point_len, "menu.item".len() as u16);
    assert_eq!(core::mem::size_of::<ElmMgrRelationRecord>(), 128);

    let topology = ElmMgrTopologyHeader::new(1, 2, 9);
    assert_eq!(topology.relation_count, 1);
    assert_eq!(topology.cell_count, 2);
    assert_eq!(topology.event_sequence, 9);
    assert_eq!(core::mem::size_of::<ElmMgrTopologyHeader>(), 24);

    let audit = ElmMgrAuditRecord::new(
        3,
        ElmLifecycleAction::Detach as u32,
        ELM_MGR_STATUS_BUSY,
        7,
        ELM_POLICY_BLOCK_HAS_EXTENSIONS,
        state_code(ElmState::Active),
    );
    assert_eq!(audit.sequence, 3);
    assert_eq!(
        first_lifecycle_reason(audit.blockers),
        ELM_LIFECYCLE_REASON_HAS_EXTENSIONS
    );
    assert_eq!(core::mem::size_of::<ElmMgrAuditRecord>(), 40);

    let audit_header = ElmMgrAuditHeader::new(1, 0, 3);
    assert_eq!(audit_header.record_count, 1);
    assert_eq!(audit_header.last_sequence, 3);
    assert_eq!(core::mem::size_of::<ElmMgrAuditHeader>(), 24);
}

#[test]
fn lifecycle_status_helpers_report_planned_result() {
    assert_eq!(
        planned_final_state(ElmLifecycleAction::Pause, ElmState::Active),
        state_code(ElmState::Paused)
    );
    assert_eq!(
        planned_final_state(ElmLifecycleAction::Replace, ElmState::Active),
        state_code(ElmState::Active)
    );
    assert_eq!(status_from_blockers(0), ELM_MGR_STATUS_OK);
    assert_eq!(
        status_from_blockers(ELM_POLICY_BLOCK_PORT_TODO),
        ELM_MGR_STATUS_TODO
    );
    assert_eq!(
        status_from_blockers(ELM_POLICY_BLOCK_DUPLICATE_BINDING),
        ELM_MGR_STATUS_BUSY
    );
    assert_eq!(
        status_from_blockers(ELM_POLICY_BLOCK_CONTRACT_MISMATCH),
        ELM_MGR_STATUS_INVALID
    );
}

#[test]
fn menu_snapshot_entries_are_fixed_layout() {
    let header = ElmMenuSnapshotHeader::new(1, 3);
    assert_eq!(header.item_count, 1);
    assert_eq!(header.generation, 3);
    assert!(header.item_entry_size as usize >= core::mem::size_of::<ElmMenuItemSnapshot>());

    let item = ElmMenuItemSnapshot::new(
        7,
        ElmId(2),
        ActionId(9),
        ElmMenuItemKind::Action,
        crate::ELM_MENU_FLAG_TODO,
        "ELM 状态",
        "查看当前 ELM 拓扑",
        "elm/status",
    );
    assert_eq!(item.id, 7);
    assert_eq!(item.owner, 2);
    assert_eq!(item.action, 9);
    assert_eq!(item.label_len, "ELM 状态".len() as u16);
    assert_eq!(&item.route[..10], b"elm/status");
}

#[test]
fn ebi_protocol_accepts_menu_extension_unit() {
    let unit = ElmEbiUnit::new(
        ElmManifest::new(
            ElmName::new("demo-menu").unwrap(),
            ElmVersion::new("0.1.0").unwrap(),
            ElmKind::Extension,
        ),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_menu(ElmEbiMenuDecl::new(
        ElmMenuItemKind::Action,
        crate::ELM_MENU_FLAG_TODO,
        "Demo",
        "demo item",
        "demo/run",
    ));

    assert!(unit.validate(ElmEbiArch::Riscv64).is_ok());
    assert_eq!(unit.manifest.name.as_str(), "demo-menu");
    assert!(unit.menu.is_some());
    assert!(!unit.has_native_code());
}

#[test]
fn ebi_protocol_rejects_wrong_architecture() {
    let unit = ElmEbiUnit::new(
        manifest("wrong-arch"),
        ElmEbiTarget::new(ElmEbiArch::LoongArch64),
    );
    assert_eq!(
        unit.validate(ElmEbiArch::Riscv64),
        Err(ElmEbiLoadStatus::ArchMismatch)
    );
}

#[test]
fn ebi_protocol_marks_native_code_as_todo_boundary() {
    let unit = ElmEbiUnit::new(manifest("native-cell"), ElmEbiTarget::new(ElmEbiArch::Any))
        .with_segment(ElmEbiSegment::new(ElmEbiSegmentKind::Code, 4096, 0));

    assert!(unit.validate(ElmEbiArch::Riscv64).is_ok());
    assert!(unit.has_native_code());
}

#[test]
fn ebi_protocol_marks_entry_as_native_boundary() {
    let unit = ElmEbiUnit::new(manifest("entry-cell"), ElmEbiTarget::new(ElmEbiArch::Any))
        .with_entry(ElmEbiEntry::new("elm_main"));

    assert!(unit.validate(ElmEbiArch::LoongArch64).is_ok());
    assert!(unit.has_native_code());
}

#[test]
fn ebi_protocol_rejects_invalid_menu_decl() {
    let unit = ElmEbiUnit::new(manifest("bad-menu"), ElmEbiTarget::new(ElmEbiArch::Any)).with_menu(
        ElmEbiMenuDecl::new(ElmMenuItemKind::Action, 0, "", "missing label", "bad/menu"),
    );

    assert_eq!(
        unit.validate(ElmEbiArch::Riscv64),
        Err(ElmEbiLoadStatus::InvalidMenu)
    );
}

#[test]
fn ebi_protocol_rejects_unsupported_abi() {
    let mut target = ElmEbiTarget::new(ElmEbiArch::Any);
    target.abi_version = crate::ELM_EBI_ABI_VERSION + 1;
    let unit = ElmEbiUnit::new(manifest("future-unit"), target);

    assert_eq!(
        unit.validate(ElmEbiArch::Riscv64),
        Err(ElmEbiLoadStatus::UnsupportedAbi)
    );
}

use crate::{
    ActionId, BindingGraph, ELM_LIFECYCLE_REASON_NONE, ELM_MGR_STATUS_INVALID, ELM_MGR_STATUS_OK,
    ElmCellSnapshot, ElmCoreInfo, ElmCtlCommand, ElmEbiArch, ElmEbiEntry, ElmEbiLoadStatus,
    ElmEbiMenuDecl, ElmEbiSegment, ElmEbiSegmentKind, ElmEbiTarget, ElmEbiUnit, ElmError,
    ElmEventRecord, ElmId, ElmKind, ElmLifecycleRequest, ElmLifecycleResponse, ElmManifest,
    ElmMenuItemKind, ElmMenuItemSnapshot, ElmMenuSnapshotHeader, ElmMgrResponseHeader, ElmName,
    ElmPortSnapshot, ElmSnapshotHeader, ElmState, ElmVersion, FlowContract, FlowMode, Generation,
    LeaseId, LeaseKind, LeaseRegistry, LeaseRights, LeaseState, ResourceLease, TopologyEventKind,
    builtin_port_descriptors, state_code,
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

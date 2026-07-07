use ktest::ktest;

use elm_model::{
    ELM_HEALTH_CHECK_AUDITS, ELM_HEALTH_CHECK_BINDINGS, ELM_HEALTH_CHECK_CELLS,
    ELM_HEALTH_CHECK_EVENTS, ELM_HEALTH_CHECK_GRAPH, ELM_HEALTH_CHECK_MENU, ELM_HEALTH_CHECK_PORTS,
    ELM_HEALTH_CHECK_PROVIDERS, ELM_HEALTH_CHECK_RUNTIME_PORTS, ELM_MGR_STATUS_OK,
    ELM_MGR_STATUS_TODO, ELM_NEXUS_CONTRACT_LEN, ELM_POLICY_BLOCK_PORT_TODO,
    ELM_PROVIDER_FLAG_DYNAMIC, ELM_PROVIDER_FLAG_TODO_BACKEND, ElmCoreHealthRecord, ElmEbiArch,
    ElmEbiLoadStatus, ElmEbiMenuDecl, ElmEbiSegment, ElmEbiSegmentKind, ElmEbiTarget, ElmEbiUnit,
    ElmId, ElmKind, ElmManifest, ElmMenuItemKind, ElmName, ElmNexusBindRequest,
    ElmPortAccessPolicy, ElmProviderPortRegisterRequest, ElmProviderPortStatsHeader, ElmState,
    ElmVersion, FlowDirection, FlowMode, state_code,
};

use super::core::{ELM_MGR_ID, ElmCore};

fn manifest(name: &str, kind: ElmKind) -> ElmManifest {
    ElmManifest::new(
        ElmName::new(name).unwrap(),
        ElmVersion::new("0.1.0").unwrap(),
        kind,
    )
}

fn menu_unit(name: &str) -> ElmEbiUnit {
    ElmEbiUnit::new(
        manifest(name, ElmKind::Extension),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_menu(ElmEbiMenuDecl::new(
        ElmMenuItemKind::Action,
        0,
        "诊断入口",
        "ELM 第一阶段菜单单元",
        "elm/test/menu",
    ))
}

fn native_unit(name: &str) -> ElmEbiUnit {
    ElmEbiUnit::new(
        manifest(name, ElmKind::Service),
        ElmEbiTarget::new(ElmEbiArch::Any),
    )
    .with_segment(ElmEbiSegment::new(ElmEbiSegmentKind::Code, 4096, 0))
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

#[ktest]
fn elm_builtin_mgr_init_health_is_clean() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    assert_eq!(core.cells().len(), 1);
    assert_eq!(core.cells()[0].id, ELM_MGR_ID);
    assert_eq!(core.cells()[0].state, ElmState::Active);

    let health = core.health_bytes();
    assert_eq!(read_i32(&health, 8), ELM_MGR_STATUS_OK);
    assert_eq!(read_u32(&health, 12), 0);
    assert_eq!(read_u32(&health, 4), 9);

    let record_size = read_u16(&health, 2) as usize;
    assert_eq!(record_size, core::mem::size_of::<ElmCoreHealthRecord>());
    let mut checks = 0u32;
    for index in 0..read_u32(&health, 4) as usize {
        let offset = core::mem::size_of::<elm_model::ElmCoreHealthHeader>() + index * record_size;
        let check_kind = read_u32(&health, offset);
        assert_eq!(read_i32(&health, offset + 4), ELM_MGR_STATUS_OK);
        checks |= 1 << check_kind;
    }
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_GRAPH), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_CELLS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_PORTS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_PROVIDERS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_BINDINGS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_RUNTIME_PORTS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_MENU), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_EVENTS), 0);
    assert_ne!(checks & (1 << ELM_HEALTH_CHECK_AUDITS), 0);
}

#[ktest]
fn elm_menu_ebi_unit_reaches_active_state() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let response = core.load_ebi_unit(menu_unit("elm-test-menu"), ElmEbiArch::Riscv64);
    assert_eq!(response.status, ElmEbiLoadStatus::Ok as i32);
    assert_eq!(response.final_state, state_code(ElmState::Active));
    assert_eq!(core.menu_items().len(), 1);

    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(response.cell_id))
        .unwrap();
    assert_eq!(cell.parent, Some(ELM_MGR_ID));
    assert_eq!(cell.state, ElmState::Active);
}

#[ktest]
fn elm_native_ebi_unit_stays_loaded_until_native_loader_exists() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let response = core.load_ebi_unit(native_unit("elm-native-todo"), ElmEbiArch::Riscv64);
    assert_eq!(response.status, ElmEbiLoadStatus::NativeCodeTodo as i32);
    assert_eq!(response.final_state, state_code(ElmState::Loaded));

    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(response.cell_id))
        .unwrap();
    assert_eq!(cell.state, ElmState::Loaded);
    assert_eq!(cell.ebi_status, ElmEbiLoadStatus::NativeCodeTodo);
    assert!(cell.has_native_code);
}

#[ktest]
fn elm_dynamic_provider_is_queryable_and_bind_preflight_is_todo() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let register = ElmProviderPortRegisterRequest::new(
        ELM_MGR_ID.0,
        "test.dynamic.provider@1",
        ElmPortAccessPolicy::Public,
        FlowDirection::Control,
        FlowMode::Shared,
        0,
    );
    let response = core.register_provider_port(register);
    assert_eq!(response.status, ELM_MGR_STATUS_OK);

    let provider_bytes = core.provider_ports_bytes();
    let header_size = core::mem::size_of::<ElmProviderPortStatsHeader>();
    let record_size = read_u16(&provider_bytes, 2) as usize;
    let record_count = read_u32(&provider_bytes, 4) as usize;
    let mut found = false;
    for index in 0..record_count {
        let offset = header_size + index * record_size;
        if read_u64(&provider_bytes, offset) != response.port_id {
            continue;
        }
        found = true;
        assert_eq!(read_u64(&provider_bytes, offset + 8), ELM_MGR_ID.0);
        assert_eq!(read_u32(&provider_bytes, offset + 28), 0);
        assert_eq!(read_u32(&provider_bytes, offset + 32), 0);
        assert_eq!(
            read_u16(&provider_bytes, offset + 42),
            ELM_PROVIDER_FLAG_DYNAMIC | ELM_PROVIDER_FLAG_TODO_BACKEND
        );
    }
    assert!(found);

    let mut bind =
        ElmNexusBindRequest::new(ELM_MGR_ID.0, response.port_id, "test.dynamic.provider@1");
    assert_eq!(bind.contract.len(), ELM_NEXUS_CONTRACT_LEN);
    bind.flags = 0;
    let plan = core.preflight_bind(bind);
    assert_eq!(plan.allowed, 0);
    assert_eq!(plan.status, ELM_MGR_STATUS_TODO);
    assert_ne!(plan.blockers & ELM_POLICY_BLOCK_PORT_TODO, 0);
}

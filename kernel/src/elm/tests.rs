use ktest::ktest;

use alloc::vec::Vec;

use elm_model::{
    ELM_ACTION_OPCODE_INVOKE, ELM_ACTION_RESULT_HEALTH, ELM_CALL_STATUS_INVALID,
    ELM_CALL_STATUS_NOT_FOUND, ELM_CALL_STATUS_OK, ELM_CALL_STATUS_UNSUPPORTED,
    ELM_HEALTH_CHECK_AUDITS, ELM_HEALTH_CHECK_BINDINGS, ELM_HEALTH_CHECK_CELLS,
    ELM_HEALTH_CHECK_EVENTS, ELM_HEALTH_CHECK_GRAPH, ELM_HEALTH_CHECK_MENU, ELM_HEALTH_CHECK_PORTS,
    ELM_HEALTH_CHECK_PROVIDERS, ELM_HEALTH_CHECK_RUNTIME_PORTS, ELM_MENU_FLAG_TODO,
    ELM_MGR_ACTION_PROVIDER_ASYNC, ELM_MGR_ACTION_PROVIDER_INVOKE, ELM_MGR_STATUS_BUSY,
    ELM_MGR_STATUS_INVALID, ELM_MGR_STATUS_OK, ELM_MGR_STATUS_TODO, ELM_MGR_STATUS_UNSUPPORTED,
    ELM_NEXUS_CONTRACT_LEN, ELM_POLICY_BLOCK_LEASE_BUSY, ELM_POLICY_BLOCK_PORT_TODO,
    ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED, ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED,
    ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL, ELM_PROVIDER_ASYNC_QUEUE_LIMIT,
    ELM_PROVIDER_FLAG_DYNAMIC, ELM_PROVIDER_FLAG_KERNEL_BACKEND, ELM_PROVIDER_FLAG_TODO_BACKEND,
    ElmActionInvokeRequest, ElmCallFrame, ElmCoreHealthHeader, ElmCoreHealthRecord, ElmEbiArch,
    ElmEbiLoadStatus, ElmEbiMenuDecl, ElmEbiSegment, ElmEbiSegmentKind, ElmEbiTarget, ElmEbiUnit,
    ElmId, ElmKind, ElmManifest, ElmMenuItemKind, ElmMgrAuditHeader, ElmMgrCallHeader,
    ElmMgrCallKind, ElmMgrPolicyInfo, ElmMgrResponseHeader, ElmName, ElmNexusBindPlanResponse,
    ElmNexusBindRequest, ElmNexusUnbindRequest, ElmPortAccessPolicy, ElmProviderAsyncCancelRequest,
    ElmProviderAsyncPollRequest, ElmProviderAsyncState, ElmProviderAsyncSubmitRequest,
    ElmProviderInvokeRequest, ElmProviderInvokeResponse, ElmProviderPortRegisterRequest,
    ElmProviderPortRegisterResponse, ElmProviderPortStatsHeader, ElmProviderQueueStatsHeader,
    ElmState, ElmVersion, FlowDirection, FlowMode, state_code,
};

use super::core::{ELM_MGR_ID, ElmCore};
use super::mgr_channel::dispatch_mgr_call_on_core;

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

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn raw_mgr_call(kind: u32, flags: u32, reserved: u32, payload: &[u8]) -> Vec<u8> {
    let header = ElmMgrCallHeader {
        kind,
        flags,
        payload_len: payload.len() as u32,
        reserved,
    };
    let mut out = Vec::new();
    push_u32(&mut out, header.kind);
    push_u32(&mut out, header.flags);
    push_u32(&mut out, header.payload_len);
    push_u32(&mut out, header.reserved);
    out.extend_from_slice(payload);
    out
}

fn mgr_call(kind: ElmMgrCallKind, payload: &[u8]) -> Vec<u8> {
    raw_mgr_call(kind as u32, 0, 0, payload)
}

fn provider_register_payload(request: &ElmProviderPortRegisterRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.owner_cell_id);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.access_policy);
    push_u32(&mut out, request.direction);
    push_u32(&mut out, request.mode);
    push_u16(&mut out, request.contract_len);
    push_u16(&mut out, request.reserved0);
    push_u32(&mut out, request.reserved1);
    out.extend_from_slice(&request.contract);
    out
}

fn nexus_bind_payload(request: &ElmNexusBindRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.cell_id);
    push_u64(&mut out, request.port_id);
    push_u32(&mut out, request.flags);
    push_u16(&mut out, request.contract_len);
    push_u16(&mut out, request.reserved);
    out.extend_from_slice(&request.contract);
    out
}

fn action_invoke_payload(request: &ElmActionInvokeRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.action_id);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.reserved);
    out
}

fn provider_invoke_payload(request: &ElmProviderInvokeRequest) -> Vec<u8> {
    let frame = request.frame;
    let mut out = Vec::new();
    push_u64(&mut out, frame.binding_id);
    push_u64(&mut out, frame.call_id);
    push_u32(&mut out, frame.opcode);
    push_u32(&mut out, frame.flags);
    push_u16(&mut out, frame.payload_len);
    push_u16(&mut out, frame.reserved0);
    push_u32(&mut out, frame.reserved1);
    out.extend_from_slice(&frame.payload);
    out
}

fn provider_async_submit_payload(request: &ElmProviderAsyncSubmitRequest) -> Vec<u8> {
    let mut out = provider_invoke_payload(&ElmProviderInvokeRequest::new(request.frame));
    push_u32(&mut out, request.timeout_ms);
    push_u32(&mut out, request.result_ttl_ms);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.reserved);
    out
}

fn provider_async_poll_payload(request: &ElmProviderAsyncPollRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.ticket_id);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.reserved);
    out
}

fn provider_async_cancel_payload(request: &ElmProviderAsyncCancelRequest) -> Vec<u8> {
    let mut out = Vec::new();
    push_u64(&mut out, request.ticket_id);
    push_u32(&mut out, request.flags);
    push_u32(&mut out, request.reserved);
    out
}

fn response_status(bytes: &[u8]) -> i32 {
    read_i32(bytes, 0)
}

fn response_payload_len(bytes: &[u8]) -> usize {
    read_u32(bytes, 4) as usize
}

fn response_payload(bytes: &[u8]) -> &[u8] {
    let start = core::mem::size_of::<ElmMgrResponseHeader>();
    let end = start + response_payload_len(bytes);
    &bytes[start..end]
}

fn bind_mgr_action_provider(core: &mut ElmCore) -> (u64, ElmNexusBindPlanResponse) {
    let action = core
        .menu_items()
        .iter()
        .find(|item| item.route == "elm/mgr/health")
        .unwrap()
        .action;
    let bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, 4, "mgr.action.invoke@1");
    let response = core.commit_bind(bind);
    assert_eq!(response.allowed, 1);
    assert_eq!(response.status, ELM_MGR_STATUS_OK);
    (action.0, response)
}

#[ktest]
fn elm_builtin_mgr_init_health_is_clean() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    assert_eq!(core.cells().len(), 1);
    assert_eq!(core.cells()[0].id, ELM_MGR_ID);
    assert_eq!(core.cells()[0].state, ElmState::Active);
    assert_eq!(core.menu_items().len(), 1);
    assert_eq!(core.menu_items()[0].owner, ELM_MGR_ID);
    assert_eq!(core.menu_items()[0].route, "elm/mgr/health");
    assert_eq!(core.menu_items()[0].flags & ELM_MENU_FLAG_TODO, 0);

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
    assert_eq!(core.menu_items().len(), 2);

    let cell = core
        .cells()
        .iter()
        .find(|cell| cell.id == ElmId(response.cell_id))
        .unwrap();
    assert_eq!(cell.parent, Some(ELM_MGR_ID));
    assert_eq!(cell.state, ElmState::Active);
}

#[ktest]
fn elm_mgr_action_provider_invokes_builtin_health_action() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let action = core
        .menu_items()
        .iter()
        .find(|item| item.route == "elm/mgr/health")
        .unwrap()
        .action;
    let bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, 4, "mgr.action.invoke@1");
    let plan = core.preflight_bind(bind);
    assert_eq!(plan.allowed, 1);
    assert_eq!(plan.status, ELM_MGR_STATUS_OK);

    let bind_response = core.commit_bind(bind);
    assert_eq!(bind_response.allowed, 1);
    assert_eq!(bind_response.status, ELM_MGR_STATUS_OK);

    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(action.0));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        1,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let response = core
        .invoke_provider(ElmProviderInvokeRequest::new(frame))
        .unwrap();
    assert_eq!(response.reply.status, ELM_CALL_STATUS_OK);
    assert_eq!(
        response.reply.payload_len as usize,
        core::mem::size_of::<elm_model::ElmActionInvokeReply>()
    );
    assert_eq!(read_u64(&response.reply.payload, 0), action.0);
    assert_eq!(read_u64(&response.reply.payload, 16), ELM_MGR_ID.0);
    assert_eq!(
        read_u32(&response.reply.payload, 24),
        ELM_ACTION_RESULT_HEALTH
    );
    assert_eq!(read_i32(&response.reply.payload, 28), ELM_MGR_STATUS_OK);

    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(999_999));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        2,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let response = core
        .invoke_provider(ElmProviderInvokeRequest::new(frame))
        .unwrap();
    assert_eq!(response.reply.status, ELM_CALL_STATUS_NOT_FOUND);

    let frame = ElmCallFrame::new(bind_response.binding_id, 3, 0xffff, &[]);
    let response = core
        .invoke_provider(ElmProviderInvokeRequest::new(frame))
        .unwrap();
    assert_eq!(response.reply.status, ELM_CALL_STATUS_UNSUPPORTED);

    let frame = ElmCallFrame::new(bind_response.binding_id, 4, ELM_ACTION_OPCODE_INVOKE, b"x");
    let response = core
        .invoke_provider(ElmProviderInvokeRequest::new(frame))
        .unwrap();
    assert_eq!(response.reply.status, ELM_CALL_STATUS_INVALID);

    let stats = core.provider_stats_bytes();
    let header_size = core::mem::size_of::<ElmProviderPortStatsHeader>();
    let record_size = read_u16(&stats, 2) as usize;
    let record_count = read_u32(&stats, 4) as usize;
    let mut found = false;
    for index in 0..record_count {
        let offset = header_size + index * record_size;
        if read_u64(&stats, offset) != 4 {
            continue;
        }
        found = true;
        assert_eq!(read_u64(&stats, offset + 8), ELM_MGR_ID.0);
        assert_eq!(
            read_u32(&stats, offset + 20),
            u32::from(ELM_PROVIDER_FLAG_KERNEL_BACKEND)
        );
        assert_eq!(read_u64(&stats, offset + 24), 1);
        assert_eq!(read_u64(&stats, offset + 32), 3);
    }
    assert!(found);
}

#[ktest]
fn elm_provider_async_core_completes_and_releases_lease_on_poll() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let (action, bind_response) = bind_mgr_action_provider(&mut core);

    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(action));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        100,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let now = sched::now_ns_public();
    let submit =
        core.submit_provider_call(ElmProviderAsyncSubmitRequest::new(frame, 1_000, 1_000), now);
    assert_eq!(submit.status, ELM_MGR_STATUS_OK);
    assert_eq!(submit.state, ElmProviderAsyncState::Queued as u32);

    let unbind = core.preflight_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_BUSY);
    assert_ne!(unbind.blockers & ELM_POLICY_BLOCK_LEASE_BUSY, 0);

    assert!(core.run_one_async_provider_job_at(now.saturating_add(100_000)));
    let poll = core.poll_provider_reply(
        ElmProviderAsyncPollRequest::new(submit.ticket_id),
        now.saturating_add(200_000),
    );
    assert_eq!(poll.state, ElmProviderAsyncState::Completed as u32);
    assert_eq!(poll.status, ELM_MGR_STATUS_OK);
    assert_eq!(poll.reply.status, ELM_CALL_STATUS_OK);
    assert_eq!(
        poll.reply.payload_len as usize,
        core::mem::size_of::<elm_model::ElmActionInvokeReply>()
    );

    let unbind = core.preflight_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_OK);
    assert_eq!(unbind.allowed, 1);
}

#[ktest]
fn elm_provider_async_cancel_queued_job_releases_lease() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let (action, bind_response) = bind_mgr_action_provider(&mut core);

    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(action));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        101,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let now = sched::now_ns_public();
    let submit = core.submit_provider_call(ElmProviderAsyncSubmitRequest::new(frame, 0, 0), now);
    assert_eq!(submit.status, ELM_MGR_STATUS_OK);

    let cancel = core.cancel_provider_call(
        ElmProviderAsyncCancelRequest::new(submit.ticket_id),
        now.saturating_add(1),
    );
    assert_eq!(cancel.state, ElmProviderAsyncState::Canceled as u32);
    assert_eq!(cancel.status, ELM_MGR_STATUS_OK);

    let unbind = core.preflight_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_OK);
    assert_eq!(unbind.allowed, 1);
}

#[ktest]
fn elm_provider_async_queued_timeout_is_retained_until_poll() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let (action, bind_response) = bind_mgr_action_provider(&mut core);

    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(action));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        102,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let now = sched::now_ns_public();
    let submit =
        core.submit_provider_call(ElmProviderAsyncSubmitRequest::new(frame, 1, 1_000), now);
    assert_eq!(submit.status, ELM_MGR_STATUS_OK);
    assert_eq!(
        core.expire_provider_jobs_at(now.saturating_add(2_000_000)),
        1
    );

    let unbind = core.preflight_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_BUSY);
    assert_ne!(unbind.blockers & ELM_POLICY_BLOCK_LEASE_BUSY, 0);

    let poll = core.poll_provider_reply(
        ElmProviderAsyncPollRequest::new(submit.ticket_id),
        now.saturating_add(2_100_000),
    );
    assert_eq!(poll.state, ElmProviderAsyncState::Expired as u32);
    assert_eq!(poll.status, ELM_MGR_STATUS_BUSY);
    assert_ne!(poll.blockers & ELM_POLICY_BLOCK_PROVIDER_CALL_EXPIRED, 0);

    let unbind = core.preflight_unbind(ElmNexusUnbindRequest::new(bind_response.binding_id));
    assert_eq!(unbind.status, ELM_MGR_STATUS_OK);
    assert_eq!(unbind.allowed, 1);
}

#[ktest]
fn elm_provider_async_queue_full_rejects_without_holding_lease() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let (action, bind_response) = bind_mgr_action_provider(&mut core);
    let payload = action_invoke_payload(&ElmActionInvokeRequest::new(action));
    let now = sched::now_ns_public();

    for call_id in 0..ELM_PROVIDER_ASYNC_QUEUE_LIMIT {
        let frame = ElmCallFrame::new(
            bind_response.binding_id,
            200 + u64::from(call_id),
            ELM_ACTION_OPCODE_INVOKE,
            &payload,
        );
        let submit =
            core.submit_provider_call(ElmProviderAsyncSubmitRequest::new(frame, 0, 0), now);
        assert_eq!(submit.status, ELM_MGR_STATUS_OK);
    }

    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        999,
        ELM_ACTION_OPCODE_INVOKE,
        &payload,
    );
    let submit = core.submit_provider_call(ElmProviderAsyncSubmitRequest::new(frame, 0, 0), now);
    assert_eq!(submit.status, ELM_MGR_STATUS_BUSY);
    assert_ne!(submit.blockers & ELM_POLICY_BLOCK_PROVIDER_QUEUE_FULL, 0);
    assert_eq!(submit.queue_depth, ELM_PROVIDER_ASYNC_QUEUE_LIMIT);
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

#[ktest]
fn elm_mgr_channel_dispatches_core_queries_and_provider_flow() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let policy = dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::QueryPolicy, &[]));
    assert_eq!(response_status(&policy), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&policy),
        core::mem::size_of::<ElmMgrPolicyInfo>()
    );

    let health = dispatch_mgr_call_on_core(&mut core, &mgr_call(ElmMgrCallKind::QueryHealth, &[]));
    assert_eq!(response_status(&health), ELM_MGR_STATUS_OK);
    assert!(response_payload_len(&health) >= core::mem::size_of::<ElmCoreHealthHeader>());
    let health_payload = response_payload(&health);
    assert_eq!(read_i32(health_payload, 8), ELM_MGR_STATUS_OK);

    let action = core
        .menu_items()
        .iter()
        .find(|item| item.route == "elm/mgr/health")
        .unwrap()
        .action;
    let action_bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, 4, "mgr.action.invoke@1");
    let action_bind_payload = nexus_bind_payload(&action_bind);
    let action_bind_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::CommitBind, &action_bind_payload),
    );
    assert_eq!(response_status(&action_bind_response), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&action_bind_response),
        core::mem::size_of::<ElmNexusBindPlanResponse>()
    );
    let action_bind_response_payload = response_payload(&action_bind_response);
    assert_eq!(
        read_i32(action_bind_response_payload, 40),
        ELM_MGR_STATUS_OK
    );
    assert_eq!(read_u32(action_bind_response_payload, 44), 1);
    let action_binding = read_u64(action_bind_response_payload, 16);

    let action_payload = action_invoke_payload(&ElmActionInvokeRequest::new(action.0));
    let frame = ElmCallFrame::new(
        action_binding,
        10,
        ELM_ACTION_OPCODE_INVOKE,
        &action_payload,
    );
    let invoke_payload = provider_invoke_payload(&ElmProviderInvokeRequest::new(frame));
    let invoke_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::InvokeProvider, &invoke_payload),
    );
    assert_eq!(response_status(&invoke_response), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&invoke_response),
        core::mem::size_of::<ElmProviderInvokeResponse>()
    );
    let invoke_response_payload = response_payload(&invoke_response);
    assert_eq!(read_i32(invoke_response_payload, 16), ELM_CALL_STATUS_OK);
    assert_eq!(
        read_u16(invoke_response_payload, 24) as usize,
        core::mem::size_of::<elm_model::ElmActionInvokeReply>()
    );
    assert_eq!(
        read_u32(invoke_response_payload, 32 + 24),
        ELM_ACTION_RESULT_HEALTH
    );
    assert_eq!(
        read_i32(invoke_response_payload, 32 + 28),
        ELM_MGR_STATUS_OK
    );

    let missing_action_payload = action_invoke_payload(&ElmActionInvokeRequest::new(999_999));
    let frame = ElmCallFrame::new(
        action_binding,
        11,
        ELM_ACTION_OPCODE_INVOKE,
        &missing_action_payload,
    );
    let invoke_payload = provider_invoke_payload(&ElmProviderInvokeRequest::new(frame));
    let invoke_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::InvokeProvider, &invoke_payload),
    );
    assert_eq!(response_status(&invoke_response), ELM_MGR_STATUS_OK);
    let invoke_response_payload = response_payload(&invoke_response);
    assert_eq!(
        read_i32(invoke_response_payload, 16),
        ELM_CALL_STATUS_NOT_FOUND
    );

    let audits = core.audit_bytes();
    let audit_header_size = core::mem::size_of::<ElmMgrAuditHeader>();
    let audit_record_size = read_u16(&audits, 2) as usize;
    let audit_record_count = read_u32(&audits, 4) as usize;
    let last_audit = audit_header_size + (audit_record_count - 1) * audit_record_size;
    assert_eq!(
        read_u32(&audits, last_audit + 8),
        ELM_MGR_ACTION_PROVIDER_INVOKE
    );
    assert_eq!(read_i32(&audits, last_audit + 12), ELM_MGR_STATUS_INVALID);
    assert_ne!(
        read_u64(&audits, last_audit + 24) & ELM_POLICY_BLOCK_PROVIDER_CALL_FAILED,
        0
    );

    let register = ElmProviderPortRegisterRequest::new(
        ELM_MGR_ID.0,
        "test.channel.provider@1",
        ElmPortAccessPolicy::Public,
        FlowDirection::Control,
        FlowMode::Shared,
        0,
    );
    let register_payload = provider_register_payload(&register);
    let register_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::RegisterProviderPort, &register_payload),
    );
    assert_eq!(response_status(&register_response), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&register_response),
        core::mem::size_of::<ElmProviderPortRegisterResponse>()
    );
    let register_response_payload = response_payload(&register_response);
    assert_eq!(read_i32(register_response_payload, 16), ELM_MGR_STATUS_OK);
    let port_id = read_u64(register_response_payload, 8);

    let providers = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryProviderPorts, &[]),
    );
    assert_eq!(response_status(&providers), ELM_MGR_STATUS_OK);
    let provider_payload = response_payload(&providers);
    let header_size = core::mem::size_of::<ElmProviderPortStatsHeader>();
    let record_size = read_u16(provider_payload, 2) as usize;
    let record_count = read_u32(provider_payload, 4) as usize;
    let mut found = false;
    for index in 0..record_count {
        let offset = header_size + index * record_size;
        if read_u64(provider_payload, offset) != port_id {
            continue;
        }
        found = true;
        assert_eq!(read_u64(provider_payload, offset + 8), ELM_MGR_ID.0);
        assert_eq!(
            read_u16(provider_payload, offset + 42),
            ELM_PROVIDER_FLAG_DYNAMIC | ELM_PROVIDER_FLAG_TODO_BACKEND
        );
    }
    assert!(found);

    let bind = ElmNexusBindRequest::new(ELM_MGR_ID.0, port_id, "test.channel.provider@1");
    let bind_payload = nexus_bind_payload(&bind);
    let plan = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::PreflightBind, &bind_payload),
    );
    assert_eq!(response_status(&plan), ELM_MGR_STATUS_OK);
    assert_eq!(
        response_payload_len(&plan),
        core::mem::size_of::<ElmNexusBindPlanResponse>()
    );
    let plan_payload = response_payload(&plan);
    assert_eq!(read_i32(plan_payload, 40), ELM_MGR_STATUS_TODO);
    assert_eq!(read_u32(plan_payload, 44), 0);
    assert_ne!(read_u64(plan_payload, 48) & ELM_POLICY_BLOCK_PORT_TODO, 0);
}

#[ktest]
fn elm_mgr_channel_dispatches_async_provider_queue_flow() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();
    let (action, bind_response) = bind_mgr_action_provider(&mut core);

    let action_payload = action_invoke_payload(&ElmActionInvokeRequest::new(action));
    let frame = ElmCallFrame::new(
        bind_response.binding_id,
        700,
        ELM_ACTION_OPCODE_INVOKE,
        &action_payload,
    );
    let submit_payload =
        provider_async_submit_payload(&ElmProviderAsyncSubmitRequest::new(frame, 1_000, 1_000));
    let submit_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::SubmitProviderCall, &submit_payload),
    );
    assert_eq!(response_status(&submit_response), ELM_MGR_STATUS_OK);
    let submit_response_payload = response_payload(&submit_response);
    assert_eq!(read_i32(submit_response_payload, 24), ELM_MGR_STATUS_OK);
    assert_eq!(
        read_u32(submit_response_payload, 28),
        ElmProviderAsyncState::Queued as u32
    );
    let ticket_id = read_u64(submit_response_payload, 0);
    assert_ne!(ticket_id, 0);

    let queue_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryProviderQueue, &[]),
    );
    assert_eq!(response_status(&queue_response), ELM_MGR_STATUS_OK);
    let queue_payload = response_payload(&queue_response);
    assert!(queue_payload.len() >= core::mem::size_of::<ElmProviderQueueStatsHeader>());
    let header_size = core::mem::size_of::<ElmProviderQueueStatsHeader>();
    let record_size = read_u16(queue_payload, 2) as usize;
    let record_count = read_u32(queue_payload, 4) as usize;
    let mut found = false;
    for index in 0..record_count {
        let offset = header_size + index * record_size;
        if read_u64(queue_payload, offset) != 4 {
            continue;
        }
        found = true;
        assert_eq!(read_u32(queue_payload, offset + 8), 1);
        assert_eq!(read_u32(queue_payload, offset + 16), 0);
    }
    assert!(found);

    assert!(core.run_one_async_provider_job_at(sched::now_ns_public().saturating_add(100_000)));
    let poll_payload = provider_async_poll_payload(&ElmProviderAsyncPollRequest::new(ticket_id));
    let poll_response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::PollProviderReply, &poll_payload),
    );
    assert_eq!(response_status(&poll_response), ELM_MGR_STATUS_OK);
    let poll_response_payload = response_payload(&poll_response);
    assert_eq!(
        read_u32(poll_response_payload, 8),
        ElmProviderAsyncState::Completed as u32
    );
    assert_eq!(read_i32(poll_response_payload, 12), ELM_MGR_STATUS_OK);
    assert_eq!(read_i32(poll_response_payload, 32), ELM_CALL_STATUS_OK);

    let audits = core.audit_bytes();
    let audit_header_size = core::mem::size_of::<ElmMgrAuditHeader>();
    let audit_record_size = read_u16(&audits, 2) as usize;
    let audit_record_count = read_u32(&audits, 4) as usize;
    let last_audit = audit_header_size + (audit_record_count - 1) * audit_record_size;
    assert_eq!(
        read_u32(&audits, last_audit + 8),
        ELM_MGR_ACTION_PROVIDER_ASYNC
    );
}

#[ktest]
fn elm_mgr_channel_rejects_malformed_byte_requests() {
    let mut core = ElmCore::new();
    core.init_builtin_mgr().unwrap();

    let truncated = [0u8; 3];
    let response = dispatch_mgr_call_on_core(&mut core, &truncated);
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let header_flags = raw_mgr_call(ElmMgrCallKind::QueryPolicy as u32, 1, 0, &[]);
    let response = dispatch_mgr_call_on_core(&mut core, &header_flags);
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let header_reserved = raw_mgr_call(ElmMgrCallKind::QueryPolicy as u32, 0, 1, &[]);
    let response = dispatch_mgr_call_on_core(&mut core, &header_reserved);
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let unsupported = raw_mgr_call(0xffff, 0, 0, &[]);
    let response = dispatch_mgr_call_on_core(&mut core, &unsupported);
    assert_eq!(response_status(&response), ELM_MGR_STATUS_UNSUPPORTED);

    let non_empty_query_payload = [1u8];
    let response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::QueryPolicy, &non_empty_query_payload),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let register = ElmProviderPortRegisterRequest::new(
        ELM_MGR_ID.0,
        "test.invalid.provider@1",
        ElmPortAccessPolicy::Public,
        FlowDirection::Control,
        FlowMode::Shared,
        0,
    );
    let mut register_payload = provider_register_payload(&register);
    register_payload[26] = 1;
    let response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::RegisterProviderPort, &register_payload),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let mut register_payload = provider_register_payload(&register);
    register_payload[28] = 1;
    let response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::RegisterProviderPort, &register_payload),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let mut register_payload = provider_register_payload(&register);
    register_payload[24..26].copy_from_slice(&((ELM_NEXUS_CONTRACT_LEN as u16) + 1).to_le_bytes());
    let response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::RegisterProviderPort, &register_payload),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);

    let cancel_payload = provider_async_cancel_payload(&ElmProviderAsyncCancelRequest::new(1));
    let mut malformed_cancel = cancel_payload;
    malformed_cancel[8] = 1;
    let response = dispatch_mgr_call_on_core(
        &mut core,
        &mgr_call(ElmMgrCallKind::CancelProviderCall, &malformed_cancel),
    );
    assert_eq!(response_status(&response), ELM_MGR_STATUS_INVALID);
}

//! `elm-mgr` 管理通道。

use alloc::vec::Vec;

use elm_model::{
    ELM_EBI_SOURCE_ABI_VERSION, ELM_MGR_MAX_INPUT, ELM_MGR_STATUS_OK,
    ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE, ELM_REPLACE_CELL_ABI_VERSION, ElmEbiArch,
    ElmEbiSourceKind, ElmEbiSourceRequest, ElmId, ElmLifecycleAction, ElmLifecyclePlanRequest,
    ElmLifecycleRequest, ElmMgrCallHeader, ElmMgrCallKind, ElmMgrEventSubscribeRequest,
    ElmMgrEventUnsubscribeRequest, ElmMgrResponseHeader, ElmMgrSubscribedEventReadRequest,
    ElmNexusBindRequest, ElmNexusUnbindRequest, ElmProviderAsyncCancelRequest,
    ElmProviderAsyncPollRequest, ElmProviderAsyncSubmitRequest, ElmProviderInvokeRequest,
    ElmProviderPortRegisterRequest, ElmProviderPortUnregisterRequest, ElmProviderSnapshotRequest,
    ElmReplaceCellRequestV1, ElmRuntimeEventRequest, ElmRuntimeLogRequest,
};

use super::{core::ElmCore, executor, menu, with_core};

pub(crate) fn dispatch_mgr_call(input: &[u8]) -> Vec<u8> {
    with_core(|core| dispatch_mgr_call_on_core(core, input))
}

pub(crate) fn dispatch_mgr_call_on_core(core: &mut ElmCore, input: &[u8]) -> Vec<u8> {
    let Some(header) = read_call_header(input) else {
        return response_only(ElmMgrResponseHeader::invalid());
    };
    let Some(kind) = ElmMgrCallKind::from_raw(header.kind) else {
        return response_only(ElmMgrResponseHeader::unsupported());
    };
    match kind {
        ElmMgrCallKind::QueryMenu => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = menu::menu_snapshot_bytes(core.menu_items(), core.menu_generation());
            response_with_payload(payload)
        }
        ElmMgrCallKind::LoadCell => {
            let payload = call_payload(input, header);
            if payload.is_empty() {
                core.record_mgr_audit(0, ElmId(0), ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE, 0);
                return response_only(ElmMgrResponseHeader::todo());
            }
            let Some((request, source_payload)) = read_ebi_source_request(payload) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let Some(source_kind) = ElmEbiSourceKind::from_raw(request.source_kind) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match source_kind {
                ElmEbiSourceKind::Eki => match elm_model::parse_eki_image(source_payload) {
                    Ok(image) => {
                        let response = core.load_ebi_image(image, current_ebi_arch());
                        response_with_plain_payload(&response)
                    }
                    Err(_) => response_only(ElmMgrResponseHeader::invalid()),
                },
                _ => {
                    core.record_mgr_audit(
                        0,
                        ElmId(0),
                        ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE,
                        0,
                    );
                    response_only(ElmMgrResponseHeader::todo())
                }
            }
        }
        ElmMgrCallKind::PauseCell => {
            let Some(request) = read_lifecycle_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.pause_cell(ElmId(request.cell_id));
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::ResumeCell => {
            let Some(request) = read_lifecycle_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.resume_cell(ElmId(request.cell_id));
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::DetachCell => {
            let Some(request) = read_lifecycle_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.detach_cell(ElmId(request.cell_id));
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::ReplaceCell => {
            let Some((request, source_payload)) =
                read_replace_cell_request(call_payload(input, header))
            else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let Some(source_kind) = ElmEbiSourceKind::from_raw(request.source_kind) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match source_kind {
                ElmEbiSourceKind::Eki => match elm_model::parse_eki_image(source_payload) {
                    Ok(image) => {
                        let response = core.replace_cell_from_ebi_image(
                            ElmId(request.target_cell_id),
                            image,
                            current_ebi_arch(),
                            request.migration_limit,
                        );
                        response_with_plain_payload(&response)
                    }
                    Err(_) => response_only(ElmMgrResponseHeader::invalid()),
                },
                _ => {
                    core.record_mgr_audit(
                        ElmLifecycleAction::Replace as u32,
                        ElmId(request.target_cell_id),
                        ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE,
                        0,
                    );
                    response_only(ElmMgrResponseHeader::todo())
                }
            }
        }
        ElmMgrCallKind::QueryTopology => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.topology_bytes();
            response_with_payload(payload)
        }
        ElmMgrCallKind::QueryPolicy => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let policy = core.policy_info();
            response_with_plain_payload(&policy)
        }
        ElmMgrCallKind::PreflightLifecycle => {
            let Some(request) = read_lifecycle_plan_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let plan = core.preflight_lifecycle(request);
            response_with_plain_payload(&plan)
        }
        ElmMgrCallKind::QueryAudit => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.audit_bytes();
            response_with_payload(payload)
        }
        ElmMgrCallKind::QueryNexusBindings => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.nexus_bindings_bytes();
            response_with_payload(payload)
        }
        ElmMgrCallKind::PreflightBind => {
            let Some(request) = read_nexus_bind_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let plan = core.preflight_bind(request);
            response_with_plain_payload(&plan)
        }
        ElmMgrCallKind::CommitBind => {
            let Some(request) = read_nexus_bind_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.commit_bind(request);
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::PreflightUnbind => {
            let Some(request) = read_nexus_unbind_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let plan = core.preflight_unbind(request);
            response_with_plain_payload(&plan)
        }
        ElmMgrCallKind::CommitUnbind => {
            let Some(request) = read_nexus_unbind_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.commit_unbind(request);
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::SubmitRuntimeLog => {
            let Some(request) = read_runtime_log_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match core.submit_runtime_log(request) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            }
        }
        ElmMgrCallKind::ReadRuntimeEvent => {
            let Some(request) = read_runtime_event_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match core.read_runtime_event(request) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            }
        }
        ElmMgrCallKind::AckRuntimeEvent => {
            let Some(request) = read_runtime_event_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match core.ack_runtime_event(request) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            }
        }
        ElmMgrCallKind::QueryRuntimePorts => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.runtime_ports_bytes();
            response_with_payload(payload)
        }
        ElmMgrCallKind::RegisterProviderPort => {
            let Some(request) = read_provider_port_register_request(call_payload(input, header))
            else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.register_provider_port(request);
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::UnregisterProviderPort => {
            let Some(request) = read_provider_port_unregister_request(call_payload(input, header))
            else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.unregister_provider_port(request);
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::QueryProviderPorts => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.provider_ports_bytes();
            response_with_payload(payload)
        }
        ElmMgrCallKind::InvokeProvider => {
            let Some(request) = read_provider_invoke_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match core.invoke_provider(request) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            }
        }
        ElmMgrCallKind::SubmitProviderCall => {
            let Some(request) = read_provider_async_submit_request(call_payload(input, header))
            else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.submit_provider_call(request, sched::now_ns_public());
            let should_wake = response.status == ELM_MGR_STATUS_OK;
            let out = response_with_plain_payload(&response);
            if should_wake {
                executor::wake_provider_worker();
            }
            out
        }
        ElmMgrCallKind::PollProviderReply => {
            let Some(request) = read_provider_async_poll_request(call_payload(input, header))
            else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.poll_provider_reply(request, sched::now_ns_public());
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::CancelProviderCall => {
            let Some(request) = read_provider_async_cancel_request(call_payload(input, header))
            else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.cancel_provider_call(request, sched::now_ns_public());
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::QueryProviderQueue => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.provider_queue_bytes(sched::now_ns_public());
            response_with_payload(payload)
        }
        ElmMgrCallKind::QueryProviderStats => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.provider_stats_bytes();
            response_with_payload(payload)
        }
        ElmMgrCallKind::QueryHealth => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.health_bytes();
            response_with_payload(payload)
        }
        ElmMgrCallKind::QueryApiRegistry => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.api_registry_bytes();
            response_with_payload(payload)
        }
        ElmMgrCallKind::SubscribeEvent => {
            let Some(request) = read_event_subscribe_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.subscribe_event(request);
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::UnsubscribeEvent => {
            let Some(request) = read_event_unsubscribe_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.unsubscribe_event(request);
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::QueryEventSubscriptions => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.event_subscriptions_bytes();
            response_with_payload(payload)
        }
        ElmMgrCallKind::ReadSubscribedEvents => {
            let Some(request) = read_subscribed_event_read_request(call_payload(input, header))
            else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match core.read_subscribed_events(request) {
                Ok(payload) => response_with_payload(payload),
                Err(status) => response_only(response_header_from_status(status)),
            }
        }
        ElmMgrCallKind::QueryProviderSnapshot => {
            let Some(request) = read_provider_snapshot_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match core.provider_snapshot_bytes(request) {
                Ok(payload) => response_with_payload(payload),
                Err(status) => response_only(response_header_from_status(status)),
            }
        }
        ElmMgrCallKind::QueryNativeCapabilities => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.native_capabilities_bytes();
            response_with_payload(payload)
        }
        ElmMgrCallKind::QueryTodoRegistry => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.todo_registry_bytes();
            response_with_payload(payload)
        }
    }
}

fn call_payload(input: &[u8], header: ElmMgrCallHeader) -> &[u8] {
    let start = core::mem::size_of::<ElmMgrCallHeader>();
    let end = start + header.payload_len as usize;
    &input[start..end]
}

fn payload_is_empty(header: ElmMgrCallHeader) -> bool {
    header.payload_len == 0
}

fn read_call_header(input: &[u8]) -> Option<ElmMgrCallHeader> {
    if input.len() > ELM_MGR_MAX_INPUT {
        return None;
    }
    let raw = input.get(..core::mem::size_of::<ElmMgrCallHeader>())?;
    let header = ElmMgrCallHeader {
        kind: u32::from_le_bytes(raw[0..4].try_into().ok()?),
        flags: u32::from_le_bytes(raw[4..8].try_into().ok()?),
        payload_len: u32::from_le_bytes(raw[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(raw[12..16].try_into().ok()?),
    };
    if header.flags != 0 || header.reserved != 0 {
        return None;
    }
    let expected =
        core::mem::size_of::<ElmMgrCallHeader>().checked_add(header.payload_len as usize)?;
    if input.len() == expected {
        Some(header)
    } else {
        None
    }
}

fn read_lifecycle_request(payload: &[u8]) -> Option<ElmLifecycleRequest> {
    if payload.len() != core::mem::size_of::<ElmLifecycleRequest>() {
        return None;
    }
    let request = ElmLifecycleRequest {
        cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn read_ebi_source_request(payload: &[u8]) -> Option<(ElmEbiSourceRequest, &[u8])> {
    let request_size = core::mem::size_of::<ElmEbiSourceRequest>();
    if payload.len() < request_size {
        return None;
    }
    let request = ElmEbiSourceRequest {
        abi_version: u16::from_le_bytes(payload[0..2].try_into().ok()?),
        source_kind: u16::from_le_bytes(payload[2..4].try_into().ok()?),
        flags: u32::from_le_bytes(payload[4..8].try_into().ok()?),
        payload_len: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    };
    if request.abi_version != ELM_EBI_SOURCE_ABI_VERSION
        || request.flags != 0
        || request.reserved != 0
    {
        return None;
    }
    let end = request_size.checked_add(request.payload_len as usize)?;
    if payload.len() != end {
        return None;
    }
    Some((request, &payload[request_size..end]))
}

fn read_replace_cell_request(payload: &[u8]) -> Option<(ElmReplaceCellRequestV1, &[u8])> {
    let request_size = core::mem::size_of::<ElmReplaceCellRequestV1>();
    if payload.len() < request_size {
        return None;
    }
    let request = ElmReplaceCellRequestV1 {
        abi_version: u16::from_le_bytes(payload[0..2].try_into().ok()?),
        flags: u16::from_le_bytes(payload[2..4].try_into().ok()?),
        source_kind: u16::from_le_bytes(payload[4..6].try_into().ok()?),
        reserved0: u16::from_le_bytes(payload[6..8].try_into().ok()?),
        target_cell_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        migration_limit: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        source_payload_len: u32::from_le_bytes(payload[20..24].try_into().ok()?),
        reserved1: u64::from_le_bytes(payload[24..32].try_into().ok()?),
    };
    if request.abi_version != ELM_REPLACE_CELL_ABI_VERSION
        || request.flags != 0
        || request.reserved0 != 0
        || request.reserved1 != 0
        || request.target_cell_id == 0
    {
        return None;
    }
    let end = request_size.checked_add(request.source_payload_len as usize)?;
    if payload.len() != end {
        return None;
    }
    Some((request, &payload[request_size..end]))
}

fn read_lifecycle_plan_request(payload: &[u8]) -> Option<ElmLifecyclePlanRequest> {
    if payload.len() != core::mem::size_of::<ElmLifecyclePlanRequest>() {
        return None;
    }
    let request = ElmLifecyclePlanRequest {
        cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        action: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        flags: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    };
    if request.flags != 0 {
        return None;
    }
    Some(request)
}

fn read_nexus_bind_request(payload: &[u8]) -> Option<ElmNexusBindRequest> {
    if payload.len() != core::mem::size_of::<ElmNexusBindRequest>() {
        return None;
    }
    let mut contract = [0u8; elm_model::ELM_NEXUS_CONTRACT_LEN];
    contract.copy_from_slice(&payload[24..24 + elm_model::ELM_NEXUS_CONTRACT_LEN]);
    let request = ElmNexusBindRequest {
        cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        port_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        flags: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        contract_len: u16::from_le_bytes(payload[20..22].try_into().ok()?),
        reserved: u16::from_le_bytes(payload[22..24].try_into().ok()?),
        contract,
    };
    if request.flags != 0
        || request.reserved != 0
        || usize::from(request.contract_len) > elm_model::ELM_NEXUS_CONTRACT_LEN
    {
        return None;
    }
    Some(request)
}

fn read_nexus_unbind_request(payload: &[u8]) -> Option<ElmNexusUnbindRequest> {
    if payload.len() != core::mem::size_of::<ElmNexusUnbindRequest>() {
        return None;
    }
    let request = ElmNexusUnbindRequest {
        binding_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn read_runtime_log_request(payload: &[u8]) -> Option<ElmRuntimeLogRequest> {
    if payload.len() != core::mem::size_of::<ElmRuntimeLogRequest>() {
        return None;
    }
    let mut message = [0u8; elm_model::ELM_RUNTIME_LOG_MESSAGE_LEN];
    message.copy_from_slice(&payload[24..24 + elm_model::ELM_RUNTIME_LOG_MESSAGE_LEN]);
    let request = ElmRuntimeLogRequest {
        binding_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        level: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        flags: u32::from_le_bytes(payload[12..16].try_into().ok()?),
        message_len: u16::from_le_bytes(payload[16..18].try_into().ok()?),
        reserved0: u16::from_le_bytes(payload[18..20].try_into().ok()?),
        reserved1: u32::from_le_bytes(payload[20..24].try_into().ok()?),
        message,
    };
    if request.flags != 0
        || request.reserved0 != 0
        || request.reserved1 != 0
        || usize::from(request.message_len) > elm_model::ELM_RUNTIME_LOG_MESSAGE_LEN
    {
        return None;
    }
    Some(request)
}

fn read_runtime_event_request(payload: &[u8]) -> Option<ElmRuntimeEventRequest> {
    if payload.len() != core::mem::size_of::<ElmRuntimeEventRequest>() {
        return None;
    }
    let request = ElmRuntimeEventRequest {
        binding_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        cursor: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        flags: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[20..24].try_into().ok()?),
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn read_event_subscribe_request(payload: &[u8]) -> Option<ElmMgrEventSubscribeRequest> {
    if payload.len() != core::mem::size_of::<ElmMgrEventSubscribeRequest>() {
        return None;
    }
    let request = ElmMgrEventSubscribeRequest {
        owner_cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        kind_filter: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        flags: u32::from_le_bytes(payload[12..16].try_into().ok()?),
        cell_filter: u64::from_le_bytes(payload[16..24].try_into().ok()?),
        port_filter: u64::from_le_bytes(payload[24..32].try_into().ok()?),
        binding_filter: u64::from_le_bytes(payload[32..40].try_into().ok()?),
        lease_filter: u64::from_le_bytes(payload[40..48].try_into().ok()?),
    };
    if request.flags != 0 || request.owner_cell_id == 0 {
        return None;
    }
    Some(request)
}

fn read_event_unsubscribe_request(payload: &[u8]) -> Option<ElmMgrEventUnsubscribeRequest> {
    if payload.len() != core::mem::size_of::<ElmMgrEventUnsubscribeRequest>() {
        return None;
    }
    let request = ElmMgrEventUnsubscribeRequest {
        subscription_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        owner_cell_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        flags: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[20..24].try_into().ok()?),
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn read_subscribed_event_read_request(payload: &[u8]) -> Option<ElmMgrSubscribedEventReadRequest> {
    if payload.len() != core::mem::size_of::<ElmMgrSubscribedEventReadRequest>() {
        return None;
    }
    let request = ElmMgrSubscribedEventReadRequest {
        subscription_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        cursor: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        max_records: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        flags: u32::from_le_bytes(payload[20..24].try_into().ok()?),
    };
    if request.subscription_id == 0 {
        return None;
    }
    Some(request)
}

fn read_provider_port_register_request(payload: &[u8]) -> Option<ElmProviderPortRegisterRequest> {
    if payload.len() != core::mem::size_of::<ElmProviderPortRegisterRequest>() {
        return None;
    }
    let mut contract = [0u8; elm_model::ELM_NEXUS_CONTRACT_LEN];
    contract.copy_from_slice(&payload[32..32 + elm_model::ELM_NEXUS_CONTRACT_LEN]);
    let request = ElmProviderPortRegisterRequest {
        owner_cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        access_policy: u32::from_le_bytes(payload[12..16].try_into().ok()?),
        direction: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        mode: u32::from_le_bytes(payload[20..24].try_into().ok()?),
        contract_len: u16::from_le_bytes(payload[24..26].try_into().ok()?),
        reserved0: u16::from_le_bytes(payload[26..28].try_into().ok()?),
        reserved1: u32::from_le_bytes(payload[28..32].try_into().ok()?),
        contract,
    };
    if request.flags != 0
        || request.reserved0 != 0
        || request.reserved1 != 0
        || usize::from(request.contract_len) > elm_model::ELM_NEXUS_CONTRACT_LEN
    {
        return None;
    }
    Some(request)
}

fn read_provider_port_unregister_request(
    payload: &[u8],
) -> Option<ElmProviderPortUnregisterRequest> {
    if payload.len() != core::mem::size_of::<ElmProviderPortUnregisterRequest>() {
        return None;
    }
    let request = ElmProviderPortUnregisterRequest {
        port_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn read_provider_invoke_request(payload: &[u8]) -> Option<ElmProviderInvokeRequest> {
    if payload.len() != core::mem::size_of::<ElmProviderInvokeRequest>() {
        return None;
    }
    let frame = read_call_frame(payload)?;
    Some(ElmProviderInvokeRequest { frame })
}

fn read_provider_snapshot_request(payload: &[u8]) -> Option<ElmProviderSnapshotRequest> {
    if payload.len() != core::mem::size_of::<ElmProviderSnapshotRequest>() {
        return None;
    }
    let request = ElmProviderSnapshotRequest {
        port_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        binding_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        flags: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[20..24].try_into().ok()?),
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn read_provider_async_submit_request(payload: &[u8]) -> Option<ElmProviderAsyncSubmitRequest> {
    if payload.len() != core::mem::size_of::<ElmProviderAsyncSubmitRequest>() {
        return None;
    }
    let frame_size = core::mem::size_of::<elm_model::ElmCallFrame>();
    let frame = read_call_frame(&payload[..frame_size])?;
    let request = ElmProviderAsyncSubmitRequest {
        frame,
        timeout_ms: u32::from_le_bytes(payload[frame_size..frame_size + 4].try_into().ok()?),
        result_ttl_ms: u32::from_le_bytes(payload[frame_size + 4..frame_size + 8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[frame_size + 8..frame_size + 12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[frame_size + 12..frame_size + 16].try_into().ok()?),
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn read_provider_async_poll_request(payload: &[u8]) -> Option<ElmProviderAsyncPollRequest> {
    if payload.len() != core::mem::size_of::<ElmProviderAsyncPollRequest>() {
        return None;
    }
    let request = ElmProviderAsyncPollRequest {
        ticket_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn read_provider_async_cancel_request(payload: &[u8]) -> Option<ElmProviderAsyncCancelRequest> {
    if payload.len() != core::mem::size_of::<ElmProviderAsyncCancelRequest>() {
        return None;
    }
    let request = ElmProviderAsyncCancelRequest {
        ticket_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn read_call_frame(payload: &[u8]) -> Option<elm_model::ElmCallFrame> {
    if payload.len() != core::mem::size_of::<elm_model::ElmCallFrame>() {
        return None;
    }
    let frame = elm_model::ElmCallFrame {
        binding_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        call_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        opcode: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        flags: u32::from_le_bytes(payload[20..24].try_into().ok()?),
        payload_len: u16::from_le_bytes(payload[24..26].try_into().ok()?),
        reserved0: u16::from_le_bytes(payload[26..28].try_into().ok()?),
        reserved1: u32::from_le_bytes(payload[28..32].try_into().ok()?),
        payload: {
            let mut payload_out = [0u8; elm_model::ELM_FRAME_PAYLOAD_LEN];
            payload_out.copy_from_slice(&payload[32..32 + elm_model::ELM_FRAME_PAYLOAD_LEN]);
            payload_out
        },
    };
    if frame.flags != 0
        || frame.reserved0 != 0
        || frame.reserved1 != 0
        || usize::from(frame.payload_len) > elm_model::ELM_FRAME_PAYLOAD_LEN
    {
        return None;
    }
    Some(frame)
}

fn response_header_from_status(status: i32) -> ElmMgrResponseHeader {
    match status {
        elm_model::ELM_MGR_STATUS_NOT_FOUND => ElmMgrResponseHeader::not_found(),
        elm_model::ELM_MGR_STATUS_BUSY => ElmMgrResponseHeader::busy(),
        elm_model::ELM_MGR_STATUS_TODO => ElmMgrResponseHeader::todo(),
        elm_model::ELM_MGR_STATUS_UNSUPPORTED => ElmMgrResponseHeader::unsupported(),
        _ => ElmMgrResponseHeader::invalid(),
    }
}

fn response_with_payload(payload: Vec<u8>) -> Vec<u8> {
    let header = ElmMgrResponseHeader::ok(payload.len() as u32);
    let mut out = Vec::new();
    push_plain(&mut out, &header);
    out.extend_from_slice(&payload);
    out
}

fn response_with_plain_payload<T>(value: &T) -> Vec<u8> {
    response_with_payload(plain_bytes(value).to_vec())
}

fn response_only(header: ElmMgrResponseHeader) -> Vec<u8> {
    let mut out = Vec::new();
    push_plain(&mut out, &header);
    out
}

fn push_plain<T>(out: &mut Vec<u8>, value: &T) {
    out.extend_from_slice(plain_bytes(value));
}

fn plain_bytes<T>(value: &T) -> &[u8] {
    // 安全性：管理通道响应头为 `#[repr(C)]` 固定布局，不包含内核指针。
    unsafe {
        core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>())
    }
}

#[cfg(target_arch = "riscv64")]
fn current_ebi_arch() -> ElmEbiArch {
    ElmEbiArch::Riscv64
}

#[cfg(target_arch = "loongarch64")]
fn current_ebi_arch() -> ElmEbiArch {
    ElmEbiArch::LoongArch64
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
fn current_ebi_arch() -> ElmEbiArch {
    ElmEbiArch::Any
}

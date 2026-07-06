//! `elm-mgr` 管理通道。

use alloc::vec::Vec;

use elm_model::{
    ELM_POLICY_BLOCK_LOAD_REQUIRES_SOYO, ElmId, ElmLifecycleAction, ElmLifecyclePlanRequest,
    ElmLifecycleRequest, ElmMgrCallHeader, ElmMgrCallKind, ElmMgrResponseHeader,
    ElmNexusBindRequest, ElmNexusUnbindRequest, ElmProviderInvokeRequest,
    ElmProviderPortRegisterRequest, ElmProviderPortUnregisterRequest, ElmRuntimeEventRequest,
    ElmRuntimeLogRequest,
};

use super::{menu, with_core};

pub(crate) fn dispatch_mgr_call(input: &[u8]) -> Vec<u8> {
    let Some(header) = read_call_header(input) else {
        return response_only(ElmMgrResponseHeader::invalid());
    };
    let Some(kind) = ElmMgrCallKind::from_raw(header.kind) else {
        return response_only(ElmMgrResponseHeader::unsupported());
    };
    match kind {
        ElmMgrCallKind::QueryMenu => {
            let payload = with_core(|core| {
                menu::menu_snapshot_bytes(core.menu_items(), core.menu_generation())
            });
            response_with_payload(payload)
        }
        ElmMgrCallKind::LoadCell => {
            // TODO(elm): 未来由 soyo 解析器把文件转换为 EBI 协议对象后再装载。
            with_core(|core| {
                core.record_mgr_audit(0, ElmId(0), ELM_POLICY_BLOCK_LOAD_REQUIRES_SOYO, 0);
            });
            response_only(ElmMgrResponseHeader::todo())
        }
        ElmMgrCallKind::PauseCell => {
            let Some(request) = read_lifecycle_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = with_core(|core| core.pause_cell(ElmId(request.cell_id)));
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::ResumeCell => {
            let Some(request) = read_lifecycle_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = with_core(|core| core.resume_cell(ElmId(request.cell_id)));
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::DetachCell => {
            let Some(request) = read_lifecycle_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = with_core(|core| core.detach_cell(ElmId(request.cell_id)));
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::ReplaceCell => {
            // TODO(elm): 热替换需要影子绑定、状态迁移和切换代回滚协议。
            let Some(request) = read_lifecycle_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let plan = with_core(|core| {
                let plan = core.preflight_lifecycle(ElmLifecyclePlanRequest::new(
                    request.cell_id,
                    ElmLifecycleAction::Replace,
                ));
                core.record_mgr_audit(
                    ElmLifecycleAction::Replace as u32,
                    ElmId(request.cell_id),
                    plan.blockers,
                    plan.final_state,
                );
                plan
            });
            response_with_plain_payload(&plan)
        }
        ElmMgrCallKind::QueryTopology => {
            let payload = with_core(|core| core.topology_bytes());
            response_with_payload(payload)
        }
        ElmMgrCallKind::QueryPolicy => {
            let policy = with_core(|core| core.policy_info());
            response_with_plain_payload(&policy)
        }
        ElmMgrCallKind::PreflightLifecycle => {
            let Some(request) = read_lifecycle_plan_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let plan = with_core(|core| core.preflight_lifecycle(request));
            response_with_plain_payload(&plan)
        }
        ElmMgrCallKind::QueryAudit => {
            let payload = with_core(|core| core.audit_bytes());
            response_with_payload(payload)
        }
        ElmMgrCallKind::QueryNexusBindings => {
            let payload = with_core(|core| core.nexus_bindings_bytes());
            response_with_payload(payload)
        }
        ElmMgrCallKind::PreflightBind => {
            let Some(request) = read_nexus_bind_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let plan = with_core(|core| core.preflight_bind(request));
            response_with_plain_payload(&plan)
        }
        ElmMgrCallKind::CommitBind => {
            let Some(request) = read_nexus_bind_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = with_core(|core| core.commit_bind(request));
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::PreflightUnbind => {
            let Some(request) = read_nexus_unbind_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let plan = with_core(|core| core.preflight_unbind(request));
            response_with_plain_payload(&plan)
        }
        ElmMgrCallKind::CommitUnbind => {
            let Some(request) = read_nexus_unbind_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = with_core(|core| core.commit_unbind(request));
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::SubmitRuntimeLog => {
            let Some(request) = read_runtime_log_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match with_core(|core| core.submit_runtime_log(request)) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            }
        }
        ElmMgrCallKind::ReadRuntimeEvent => {
            let Some(request) = read_runtime_event_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match with_core(|core| core.read_runtime_event(request)) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            }
        }
        ElmMgrCallKind::AckRuntimeEvent => {
            let Some(request) = read_runtime_event_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match with_core(|core| core.ack_runtime_event(request)) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            }
        }
        ElmMgrCallKind::QueryRuntimePorts => {
            let payload = with_core(|core| core.runtime_ports_bytes());
            response_with_payload(payload)
        }
        ElmMgrCallKind::RegisterProviderPort => {
            let Some(request) = read_provider_port_register_request(call_payload(input, header))
            else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = with_core(|core| core.register_provider_port(request));
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::UnregisterProviderPort => {
            let Some(request) = read_provider_port_unregister_request(call_payload(input, header))
            else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = with_core(|core| core.unregister_provider_port(request));
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::QueryProviderPorts => {
            let payload = with_core(|core| core.provider_ports_bytes());
            response_with_payload(payload)
        }
        ElmMgrCallKind::InvokeProvider => {
            let Some(request) = read_provider_invoke_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match with_core(|core| core.invoke_provider(request)) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            }
        }
        ElmMgrCallKind::QueryProviderStats => {
            let payload = with_core(|core| core.provider_stats_bytes());
            response_with_payload(payload)
        }
    }
}

fn call_payload(input: &[u8], header: ElmMgrCallHeader) -> &[u8] {
    let start = core::mem::size_of::<ElmMgrCallHeader>();
    let end = start + header.payload_len as usize;
    &input[start..end]
}

fn read_call_header(input: &[u8]) -> Option<ElmMgrCallHeader> {
    let raw = input.get(..core::mem::size_of::<ElmMgrCallHeader>())?;
    let header = ElmMgrCallHeader {
        kind: u32::from_le_bytes(raw[0..4].try_into().ok()?),
        flags: u32::from_le_bytes(raw[4..8].try_into().ok()?),
        payload_len: u32::from_le_bytes(raw[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(raw[12..16].try_into().ok()?),
    };
    let expected = core::mem::size_of::<ElmMgrCallHeader>() + header.payload_len as usize;
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
    Some(ElmLifecycleRequest {
        cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    })
}

fn read_lifecycle_plan_request(payload: &[u8]) -> Option<ElmLifecyclePlanRequest> {
    if payload.len() != core::mem::size_of::<ElmLifecyclePlanRequest>() {
        return None;
    }
    Some(ElmLifecyclePlanRequest {
        cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        action: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        flags: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    })
}

fn read_nexus_bind_request(payload: &[u8]) -> Option<ElmNexusBindRequest> {
    if payload.len() != core::mem::size_of::<ElmNexusBindRequest>() {
        return None;
    }
    let mut contract = [0u8; elm_model::ELM_NEXUS_CONTRACT_LEN];
    contract.copy_from_slice(&payload[24..24 + elm_model::ELM_NEXUS_CONTRACT_LEN]);
    Some(ElmNexusBindRequest {
        cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        port_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        flags: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        contract_len: u16::from_le_bytes(payload[20..22].try_into().ok()?),
        reserved: u16::from_le_bytes(payload[22..24].try_into().ok()?),
        contract,
    })
}

fn read_nexus_unbind_request(payload: &[u8]) -> Option<ElmNexusUnbindRequest> {
    if payload.len() != core::mem::size_of::<ElmNexusUnbindRequest>() {
        return None;
    }
    Some(ElmNexusUnbindRequest {
        binding_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    })
}

fn read_runtime_log_request(payload: &[u8]) -> Option<ElmRuntimeLogRequest> {
    if payload.len() != core::mem::size_of::<ElmRuntimeLogRequest>() {
        return None;
    }
    let mut message = [0u8; elm_model::ELM_RUNTIME_LOG_MESSAGE_LEN];
    message.copy_from_slice(&payload[24..24 + elm_model::ELM_RUNTIME_LOG_MESSAGE_LEN]);
    Some(ElmRuntimeLogRequest {
        binding_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        level: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        flags: u32::from_le_bytes(payload[12..16].try_into().ok()?),
        message_len: u16::from_le_bytes(payload[16..18].try_into().ok()?),
        reserved0: u16::from_le_bytes(payload[18..20].try_into().ok()?),
        reserved1: u32::from_le_bytes(payload[20..24].try_into().ok()?),
        message,
    })
}

fn read_runtime_event_request(payload: &[u8]) -> Option<ElmRuntimeEventRequest> {
    if payload.len() != core::mem::size_of::<ElmRuntimeEventRequest>() {
        return None;
    }
    Some(ElmRuntimeEventRequest {
        binding_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        cursor: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        flags: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[20..24].try_into().ok()?),
    })
}

fn read_provider_port_register_request(payload: &[u8]) -> Option<ElmProviderPortRegisterRequest> {
    if payload.len() != core::mem::size_of::<ElmProviderPortRegisterRequest>() {
        return None;
    }
    let mut contract = [0u8; elm_model::ELM_NEXUS_CONTRACT_LEN];
    contract.copy_from_slice(&payload[32..32 + elm_model::ELM_NEXUS_CONTRACT_LEN]);
    Some(ElmProviderPortRegisterRequest {
        owner_cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        access_policy: u32::from_le_bytes(payload[12..16].try_into().ok()?),
        direction: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        mode: u32::from_le_bytes(payload[20..24].try_into().ok()?),
        contract_len: u16::from_le_bytes(payload[24..26].try_into().ok()?),
        reserved0: u16::from_le_bytes(payload[26..28].try_into().ok()?),
        reserved1: u32::from_le_bytes(payload[28..32].try_into().ok()?),
        contract,
    })
}

fn read_provider_port_unregister_request(
    payload: &[u8],
) -> Option<ElmProviderPortUnregisterRequest> {
    if payload.len() != core::mem::size_of::<ElmProviderPortUnregisterRequest>() {
        return None;
    }
    Some(ElmProviderPortUnregisterRequest {
        port_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    })
}

fn read_provider_invoke_request(payload: &[u8]) -> Option<ElmProviderInvokeRequest> {
    if payload.len() != core::mem::size_of::<ElmProviderInvokeRequest>() {
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
    Some(ElmProviderInvokeRequest { frame })
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

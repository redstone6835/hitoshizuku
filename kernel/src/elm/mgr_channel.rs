//! `elm-mgr` 管理通道。

use alloc::vec::Vec;

use elm_model::{
    ELM_POLICY_BLOCK_LOAD_REQUIRES_SOYO, ElmId, ElmLifecycleAction, ElmLifecyclePlanRequest,
    ElmLifecycleRequest, ElmMgrCallHeader, ElmMgrCallKind, ElmMgrResponseHeader,
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

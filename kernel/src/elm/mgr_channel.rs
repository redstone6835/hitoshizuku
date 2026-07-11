//! `elm-mgr` 管理通道。

use alloc::vec::Vec;

use elm_model::{
    ELM_EBI_PROJECTION_SOURCE_ABI_VERSION, ELM_EBI_PROJECTION_SOURCE_FLAG_IMAGE_SESSION,
    ELM_EBI_PROJECTION_SOURCE_FLAGS_MASK, ELM_EBI_PROJECTION_SOURCE_REQUEST_SIZE,
    ELM_EBI_SOURCE_ABI_VERSION, ELM_EBI_SOURCE_REQUEST_SIZE, ELM_IMAGE_SESSION_ABI_VERSION,
    ELM_IMAGE_SESSION_REFERENCE_ABI_VERSION, ELM_MGR_MAX_INPUT, ELM_MGR_STATUS_OK,
    ELM_MGR_STATUS_PERMISSION, ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE,
    ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED, ELM_PROVIDER_SNAPSHOT_REQUEST_FLAGS_MASK,
    ELM_REPLACE_CELL_ABI_VERSION, ElmCellPolicyRequest, ElmCellPolicyV1, ElmEbiArch,
    ElmEbiSourceKind, ElmEbiSourceRequest, ElmExtensionAttachRequest, ElmExtensionDetachRequest,
    ElmExtensionDispatchRequest, ElmId, ElmImageSessionBeginRequestV1, ElmImageSessionReferenceV1,
    ElmImageSessionRequestV1, ElmImageSessionWriteRequestV1, ElmLifecyclePlanRequest,
    ElmLifecycleRequest, ElmMgrCallHeader, ElmMgrCallKind, ElmMgrEventSubscribeRequest,
    ElmMgrEventUnsubscribeRequest, ElmMgrResponseHeader, ElmMgrSubscribedEventReadRequest,
    ElmNexusBindRequest, ElmNexusUnbindRequest, ElmPrincipal, ElmProjectionSourceRequest,
    ElmProviderAsyncCancelRequest, ElmProviderAsyncPollRequest, ElmProviderAsyncSubmitRequest,
    ElmProviderInvokeRequest, ElmProviderPortRegisterRequest, ElmProviderPortUnregisterRequest,
    ElmProviderSnapshotRequest, ElmReplaceCellRequestV1, ElmResourceBudgetRequest,
    ElmResourceBudgetUpdateRequest, ElmRuntimeEventRequest, ElmRuntimeLogRequest,
    ElmSliceImageReader,
};

use super::{
    core::{
        ElmCore, ElmMgrAccessTarget, ElmMgrAuthorization, detach_cell_unlocked,
        dispatch_extension_unlocked, invoke_provider_unlocked, load_ebi_image_unlocked,
        pause_cell_unlocked, provider_snapshot_unlocked, replace_cell_unlocked,
        resume_cell_unlocked,
    },
    executor, menu, source, with_core,
};

pub(crate) fn dispatch_mgr_call(input: &[u8]) -> Vec<u8> {
    dispatch_mgr_call_as(ElmPrincipal::kernel(), input)
}

pub(crate) fn dispatch_mgr_call_as(principal: ElmPrincipal, input: &[u8]) -> Vec<u8> {
    let Some(header) = read_call_header(input) else {
        return response_only(ElmMgrResponseHeader::invalid());
    };
    let Some(kind) = ElmMgrCallKind::from_raw(header.kind) else {
        return response_only(ElmMgrResponseHeader::unsupported());
    };
    match kind {
        ElmMgrCallKind::LoadCell => {
            let payload = call_payload(input, header);
            if payload.is_empty() {
                with_core(|core| {
                    core.record_mgr_audit(0, ElmId(0), ELM_POLICY_BLOCK_LOAD_REQUIRES_EBI_SOURCE, 0)
                });
                return response_only(ElmMgrResponseHeader::todo());
            }
            let Some((request, source_payload)) = read_ebi_source_request(payload) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let Some(source_kind) = ElmEbiSourceKind::from_raw(request.source_kind) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let parent = ElmId(request.parent_cell_id);
            let Some(mut authorization) = authorize_unlocked_call(
                principal,
                kind,
                ElmMgrAccessTarget::Load(parent, request.budget),
            ) else {
                return response_only(ElmMgrResponseHeader::permission());
            };
            let image = match source_kind {
                ElmEbiSourceKind::Projection => {
                    load_projection_image(principal, source_payload, current_ebi_arch())
                }
                ElmEbiSourceKind::Builtin | ElmEbiSourceKind::Memory => {
                    return response_only(ElmMgrResponseHeader::unsupported());
                }
            };
            return match image {
                Ok(image) => {
                    let response = match load_ebi_image_unlocked(
                        image,
                        current_ebi_arch(),
                        source_kind,
                        parent,
                        request.budget,
                        &mut authorization,
                    ) {
                        Ok(response) => response_with_plain_payload(&response),
                        Err(status) => response_only(response_header_from_status(status)),
                    };
                    finish_unlocked_call(kind, authorization, response)
                }
                Err(status) => response_only(response_header_from_status(status)),
            };
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
            let Some(mut authorization) = authorize_unlocked_call(
                principal,
                kind,
                ElmMgrAccessTarget::Cell(ElmId(request.target_cell_id)),
            ) else {
                return response_only(ElmMgrResponseHeader::permission());
            };
            let image = match source_kind {
                ElmEbiSourceKind::Projection => {
                    load_projection_image(principal, source_payload, current_ebi_arch())
                }
                ElmEbiSourceKind::Builtin | ElmEbiSourceKind::Memory => {
                    return response_only(ElmMgrResponseHeader::unsupported());
                }
            };
            return match image {
                Ok(image) => {
                    let response = match replace_cell_unlocked(
                        ElmId(request.target_cell_id),
                        image,
                        current_ebi_arch(),
                        request.migration_limit,
                        source_kind,
                        &mut authorization,
                    ) {
                        Ok(response) => response_with_plain_payload(&response),
                        Err(status) => response_only(response_header_from_status(status)),
                    };
                    finish_unlocked_call(kind, authorization, response)
                }
                Err(status) => response_only(response_header_from_status(status)),
            };
        }
        ElmMgrCallKind::PauseCell => {
            let Some(request) = read_lifecycle_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let Some(mut authorization) = authorize_unlocked_call(
                principal,
                kind,
                ElmMgrAccessTarget::Cell(ElmId(request.cell_id)),
            ) else {
                return response_only(ElmMgrResponseHeader::permission());
            };
            let response = match pause_cell_unlocked(ElmId(request.cell_id), &mut authorization) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            };
            return finish_unlocked_call(kind, authorization, response);
        }
        ElmMgrCallKind::ResumeCell => {
            let Some(request) = read_lifecycle_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let Some(mut authorization) = authorize_unlocked_call(
                principal,
                kind,
                ElmMgrAccessTarget::Cell(ElmId(request.cell_id)),
            ) else {
                return response_only(ElmMgrResponseHeader::permission());
            };
            let response = match resume_cell_unlocked(ElmId(request.cell_id), &mut authorization) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            };
            return finish_unlocked_call(kind, authorization, response);
        }
        ElmMgrCallKind::DetachCell => {
            let Some(request) = read_lifecycle_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let Some(mut authorization) = authorize_unlocked_call(
                principal,
                kind,
                ElmMgrAccessTarget::Cell(ElmId(request.cell_id)),
            ) else {
                return response_only(ElmMgrResponseHeader::permission());
            };
            let response = match detach_cell_unlocked(ElmId(request.cell_id), &mut authorization) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            };
            return finish_unlocked_call(kind, authorization, response);
        }
        ElmMgrCallKind::InvokeProvider => {
            let Some(request) = read_provider_invoke_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let Some(mut authorization) = authorize_unlocked_call(
                principal,
                kind,
                ElmMgrAccessTarget::Binding(elm_model::BindingId(request.frame.binding_id)),
            ) else {
                return response_only(ElmMgrResponseHeader::permission());
            };
            let response = match invoke_provider_unlocked(request, &mut authorization) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            };
            return finish_unlocked_call(kind, authorization, response);
        }
        ElmMgrCallKind::QueryProviderSnapshot => {
            let Some(request) = read_provider_snapshot_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let target = if request.binding_id != 0 {
                ElmMgrAccessTarget::Binding(elm_model::BindingId(request.binding_id))
            } else {
                ElmMgrAccessTarget::Port(elm_model::PortId(request.port_id))
            };
            let Some(mut authorization) = authorize_unlocked_call(principal, kind, target) else {
                return response_only(ElmMgrResponseHeader::permission());
            };
            let response = match provider_snapshot_unlocked(request, &mut authorization) {
                Ok(payload) => response_with_payload(payload),
                Err(status) => response_only(response_header_from_status(status)),
            };
            return finish_unlocked_call(kind, authorization, response);
        }
        ElmMgrCallKind::DispatchExtension => {
            let Some(request) = read_extension_dispatch_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let Some(mut authorization) = authorize_unlocked_call(
                principal,
                kind,
                ElmMgrAccessTarget::Cells(
                    ElmId(request.target_cell_id),
                    ElmId(request.extension_cell_id),
                ),
            ) else {
                return response_only(ElmMgrResponseHeader::permission());
            };
            let response = match dispatch_extension_unlocked(request, &mut authorization) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            };
            return finish_unlocked_call(kind, authorization, response);
        }
        ElmMgrCallKind::BeginImageSession
        | ElmMgrCallKind::WriteImageSession
        | ElmMgrCallKind::SealImageSession
        | ElmMgrCallKind::AbortImageSession
        | ElmMgrCallKind::QueryImageSession => {
            let Some(authorization) =
                authorize_unlocked_call(principal, kind, ElmMgrAccessTarget::Global)
            else {
                return response_only(ElmMgrResponseHeader::permission());
            };
            let response = dispatch_image_session_call(
                principal,
                kind,
                call_payload(input, header),
                sched::now_ns_public(),
            );
            return finish_unlocked_call(kind, authorization, response);
        }
        _ => {}
    }
    with_core(|core| dispatch_mgr_call_on_core_as(core, principal, input))
}

pub(crate) fn dispatch_mgr_call_on_core(core: &mut ElmCore, input: &[u8]) -> Vec<u8> {
    dispatch_mgr_call_on_core_as(core, ElmPrincipal::kernel(), input)
}

pub(crate) fn dispatch_mgr_call_on_core_as(
    core: &mut ElmCore,
    principal: ElmPrincipal,
    input: &[u8],
) -> Vec<u8> {
    let Some(header) = read_call_header(input) else {
        return response_only(ElmMgrResponseHeader::invalid());
    };
    let Some(kind) = ElmMgrCallKind::from_raw(header.kind) else {
        return response_only(ElmMgrResponseHeader::unsupported());
    };
    let Some(target) = mgr_access_target(kind, call_payload(input, header)) else {
        return response_only(ElmMgrResponseHeader::invalid());
    };
    let authorization = core.authorize_mgr_call(principal, kind, target);
    if !authorization.allowed() {
        if principal.kind != elm_model::ElmPrincipalKind::Kernel {
            core.record_mgr_authorization(kind, authorization, ELM_MGR_STATUS_PERMISSION);
        }
        return response_only(ElmMgrResponseHeader::permission());
    }
    let response = dispatch_mgr_call_on_core_unchecked(core, principal, input);
    if principal.kind != elm_model::ElmPrincipalKind::Kernel {
        core.record_mgr_authorization(kind, authorization, response_status(&response));
    }
    response
}

fn dispatch_mgr_call_on_core_unchecked(
    core: &mut ElmCore,
    principal: ElmPrincipal,
    input: &[u8],
) -> Vec<u8> {
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
            let parent = ElmId(request.parent_cell_id);
            match source_kind {
                ElmEbiSourceKind::Projection => {
                    match load_projection_image(principal, source_payload, current_ebi_arch()) {
                        Ok(image) => {
                            // 局部 Core 不持有全局自旋锁，可以完整执行原生装载事务。
                            let response = core.load_ebi_image_in_detached_core(
                                image,
                                current_ebi_arch(),
                                source_kind,
                                parent,
                                request.budget,
                            );
                            response_with_plain_payload(&response)
                        }
                        Err(status) => response_only(response_header_from_status(status)),
                    }
                }
                ElmEbiSourceKind::Builtin | ElmEbiSourceKind::Memory => {
                    response_only(ElmMgrResponseHeader::unsupported())
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
                ElmEbiSourceKind::Projection => {
                    match load_projection_image(principal, source_payload, current_ebi_arch()) {
                        Ok(image) => {
                            let response = core
                                .replace_declarative_cell_from_ebi_image_with_source(
                                    ElmId(request.target_cell_id),
                                    image,
                                    current_ebi_arch(),
                                    request.migration_limit,
                                    ElmEbiSourceKind::Projection,
                                );
                            response_with_plain_payload(&response)
                        }
                        Err(status) => response_only(response_header_from_status(status)),
                    }
                }
                ElmEbiSourceKind::Builtin | ElmEbiSourceKind::Memory => {
                    response_only(ElmMgrResponseHeader::unsupported())
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
        ElmMgrCallKind::QueryFaultDump => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.fault_dump_bytes();
            response_with_payload(payload)
        }
        ElmMgrCallKind::QueryLifecycleTrace => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            response_with_payload(core.lifecycle_trace_bytes())
        }
        ElmMgrCallKind::QueryProviderCallTrace => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            response_with_payload(core.provider_call_trace_bytes())
        }
        ElmMgrCallKind::QueryMixinTrace => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            response_with_payload(core.mixin_trace_bytes())
        }
        ElmMgrCallKind::QueryReplaceTrace => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            response_with_payload(core.replace_trace_bytes())
        }
        ElmMgrCallKind::QueryPolicyTrace => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            response_with_payload(core.policy_trace_bytes())
        }
        ElmMgrCallKind::QueryResourceDiagnostics => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            response_with_payload(core.resource_diagnostics_bytes())
        }
        ElmMgrCallKind::QueryRuntimeJournal => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            response_with_payload(core.runtime_journal_bytes())
        }
        ElmMgrCallKind::QueryTrustState => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            response_with_plain_payload(&core.trust_runtime_info())
        }
        ElmMgrCallKind::QueryCellPolicy => {
            let Some(request) = read_cell_policy_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.query_cell_policy(request);
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::UpdateCellPolicy => {
            let Some(policy) = read_cell_policy(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.update_cell_policy(policy);
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::QueryResourceBudget => {
            let Some(request) = read_resource_budget_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.query_resource_budget(request);
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::UpdateResourceBudget => {
            let Some(request) = read_resource_budget_update_request(call_payload(input, header))
            else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.update_resource_budget(request);
            response_with_plain_payload(&response)
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
        ElmMgrCallKind::QueryExtensions => {
            if !payload_is_empty(header) {
                return response_only(ElmMgrResponseHeader::invalid());
            }
            let payload = core.extensions_bytes();
            response_with_payload(payload)
        }
        ElmMgrCallKind::PreflightExtensionAttach => {
            let Some(request) = read_extension_attach_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.preflight_extension_attach(request);
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::CommitExtensionAttach => {
            let Some(request) = read_extension_attach_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.commit_extension_attach(request);
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::CommitExtensionDetach => {
            let Some(request) = read_extension_detach_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            let response = core.commit_extension_detach(request);
            response_with_plain_payload(&response)
        }
        ElmMgrCallKind::DispatchExtension => {
            let Some(request) = read_extension_dispatch_request(call_payload(input, header)) else {
                return response_only(ElmMgrResponseHeader::invalid());
            };
            match core.dispatch_extension_on_local_core(request) {
                Ok(response) => response_with_plain_payload(&response),
                Err(status) => response_only(response_header_from_status(status)),
            }
        }
        ElmMgrCallKind::BeginImageSession
        | ElmMgrCallKind::WriteImageSession
        | ElmMgrCallKind::SealImageSession
        | ElmMgrCallKind::AbortImageSession
        | ElmMgrCallKind::QueryImageSession => dispatch_image_session_call(
            principal,
            kind,
            call_payload(input, header),
            sched::now_ns_public(),
        ),
    }
}

fn authorize_unlocked_call(
    principal: ElmPrincipal,
    kind: ElmMgrCallKind,
    target: ElmMgrAccessTarget,
) -> Option<ElmMgrAuthorization> {
    with_core(|core| {
        let authorization = core.authorize_mgr_call(principal, kind, target);
        if authorization.allowed() {
            Some(authorization)
        } else {
            if principal.kind != elm_model::ElmPrincipalKind::Kernel {
                core.record_mgr_authorization(kind, authorization, ELM_MGR_STATUS_PERMISSION);
            }
            None
        }
    })
}

fn finish_unlocked_call(
    kind: ElmMgrCallKind,
    authorization: ElmMgrAuthorization,
    response: Vec<u8>,
) -> Vec<u8> {
    let status = response_status(&response);
    if authorization.principal.kind != elm_model::ElmPrincipalKind::Kernel {
        with_core(|core| core.record_mgr_authorization(kind, authorization, status));
    }
    response
}

fn response_status(response: &[u8]) -> i32 {
    response
        .get(..4)
        .and_then(|raw| raw.try_into().ok())
        .map(i32::from_le_bytes)
        .unwrap_or(ELM_MGR_STATUS_PERMISSION)
}

fn mgr_access_target(kind: ElmMgrCallKind, payload: &[u8]) -> Option<ElmMgrAccessTarget> {
    let target = match kind {
        ElmMgrCallKind::LoadCell => {
            if payload.is_empty() {
                ElmMgrAccessTarget::Load(
                    elm_model::ELM_MGR_BUILTIN_ID,
                    elm_model::ElmResourceBudget::DEFAULT,
                )
            } else {
                let (request, _) = read_ebi_source_request(payload)?;
                ElmMgrAccessTarget::Load(ElmId(request.parent_cell_id), request.budget)
            }
        }
        ElmMgrCallKind::ReplaceCell => {
            let (request, _) = read_replace_cell_request(payload)?;
            ElmMgrAccessTarget::Cell(ElmId(request.target_cell_id))
        }
        ElmMgrCallKind::PauseCell | ElmMgrCallKind::ResumeCell | ElmMgrCallKind::DetachCell => {
            let request = read_lifecycle_request(payload)?;
            ElmMgrAccessTarget::Cell(ElmId(request.cell_id))
        }
        ElmMgrCallKind::PreflightLifecycle => {
            let request = read_lifecycle_plan_request(payload)?;
            ElmMgrAccessTarget::Cell(ElmId(request.cell_id))
        }
        ElmMgrCallKind::QueryCellPolicy => {
            let request = read_cell_policy_request(payload)?;
            ElmMgrAccessTarget::Cell(ElmId(request.cell_id))
        }
        ElmMgrCallKind::UpdateCellPolicy => {
            ElmMgrAccessTarget::PolicyUpdate(read_cell_policy(payload)?)
        }
        ElmMgrCallKind::QueryResourceBudget => {
            let request = read_resource_budget_request(payload)?;
            ElmMgrAccessTarget::Cell(ElmId(request.cell_id))
        }
        ElmMgrCallKind::UpdateResourceBudget => {
            ElmMgrAccessTarget::ResourceUpdate(read_resource_budget_update_request(payload)?)
        }
        ElmMgrCallKind::PreflightBind | ElmMgrCallKind::CommitBind => {
            let request = read_nexus_bind_request(payload)?;
            ElmMgrAccessTarget::Cell(ElmId(request.cell_id))
        }
        ElmMgrCallKind::PreflightUnbind | ElmMgrCallKind::CommitUnbind => {
            let request = read_nexus_unbind_request(payload)?;
            ElmMgrAccessTarget::Binding(elm_model::BindingId(request.binding_id))
        }
        ElmMgrCallKind::SubmitRuntimeLog => {
            let request = read_runtime_log_request(payload)?;
            ElmMgrAccessTarget::Binding(elm_model::BindingId(request.binding_id))
        }
        ElmMgrCallKind::ReadRuntimeEvent | ElmMgrCallKind::AckRuntimeEvent => {
            let request = read_runtime_event_request(payload)?;
            ElmMgrAccessTarget::Binding(elm_model::BindingId(request.binding_id))
        }
        ElmMgrCallKind::RegisterProviderPort => {
            let request = read_provider_port_register_request(payload)?;
            ElmMgrAccessTarget::Cell(ElmId(request.owner_cell_id))
        }
        ElmMgrCallKind::UnregisterProviderPort => {
            let request = read_provider_port_unregister_request(payload)?;
            ElmMgrAccessTarget::Port(elm_model::PortId(request.port_id))
        }
        ElmMgrCallKind::InvokeProvider => {
            let request = read_provider_invoke_request(payload)?;
            ElmMgrAccessTarget::Binding(elm_model::BindingId(request.frame.binding_id))
        }
        ElmMgrCallKind::SubmitProviderCall => {
            let request = read_provider_async_submit_request(payload)?;
            ElmMgrAccessTarget::Binding(elm_model::BindingId(request.frame.binding_id))
        }
        ElmMgrCallKind::PollProviderReply => {
            let request = read_provider_async_poll_request(payload)?;
            ElmMgrAccessTarget::ProviderTicket(request.ticket_id)
        }
        ElmMgrCallKind::CancelProviderCall => {
            let request = read_provider_async_cancel_request(payload)?;
            ElmMgrAccessTarget::ProviderTicket(request.ticket_id)
        }
        ElmMgrCallKind::QueryProviderSnapshot => {
            let request = read_provider_snapshot_request(payload)?;
            if request.binding_id != 0 {
                ElmMgrAccessTarget::Binding(elm_model::BindingId(request.binding_id))
            } else {
                ElmMgrAccessTarget::Port(elm_model::PortId(request.port_id))
            }
        }
        ElmMgrCallKind::SubscribeEvent => {
            let request = read_event_subscribe_request(payload)?;
            ElmMgrAccessTarget::Cell(ElmId(request.owner_cell_id))
        }
        ElmMgrCallKind::UnsubscribeEvent => {
            let request = read_event_unsubscribe_request(payload)?;
            ElmMgrAccessTarget::Subscription(request.subscription_id)
        }
        ElmMgrCallKind::ReadSubscribedEvents => {
            let request = read_subscribed_event_read_request(payload)?;
            ElmMgrAccessTarget::Subscription(request.subscription_id)
        }
        ElmMgrCallKind::PreflightExtensionAttach | ElmMgrCallKind::CommitExtensionAttach => {
            let request = read_extension_attach_request(payload)?;
            ElmMgrAccessTarget::Cells(
                ElmId(request.extension_cell_id),
                ElmId(request.target_cell_id),
            )
        }
        ElmMgrCallKind::CommitExtensionDetach => {
            let request = read_extension_detach_request(payload)?;
            ElmMgrAccessTarget::Cells(
                ElmId(request.extension_cell_id),
                ElmId(request.target_cell_id),
            )
        }
        ElmMgrCallKind::DispatchExtension => {
            let request = read_extension_dispatch_request(payload)?;
            ElmMgrAccessTarget::Cells(
                ElmId(request.target_cell_id),
                ElmId(request.extension_cell_id),
            )
        }
        ElmMgrCallKind::BeginImageSession => {
            read_image_session_begin_request(payload)?;
            ElmMgrAccessTarget::Global
        }
        ElmMgrCallKind::WriteImageSession => {
            read_image_session_write_request(payload)?;
            ElmMgrAccessTarget::Global
        }
        ElmMgrCallKind::SealImageSession
        | ElmMgrCallKind::AbortImageSession
        | ElmMgrCallKind::QueryImageSession => {
            read_image_session_request(payload)?;
            ElmMgrAccessTarget::Global
        }
        ElmMgrCallKind::QueryMenu
        | ElmMgrCallKind::QueryTopology
        | ElmMgrCallKind::QueryPolicy
        | ElmMgrCallKind::QueryAudit
        | ElmMgrCallKind::QueryNexusBindings
        | ElmMgrCallKind::QueryRuntimePorts
        | ElmMgrCallKind::QueryProviderPorts
        | ElmMgrCallKind::QueryProviderStats
        | ElmMgrCallKind::QueryHealth
        | ElmMgrCallKind::QueryProviderQueue
        | ElmMgrCallKind::QueryApiRegistry
        | ElmMgrCallKind::QueryEventSubscriptions
        | ElmMgrCallKind::QueryNativeCapabilities
        | ElmMgrCallKind::QueryTodoRegistry
        | ElmMgrCallKind::QueryExtensions
        | ElmMgrCallKind::QueryFaultDump
        | ElmMgrCallKind::QueryLifecycleTrace
        | ElmMgrCallKind::QueryProviderCallTrace
        | ElmMgrCallKind::QueryMixinTrace
        | ElmMgrCallKind::QueryReplaceTrace
        | ElmMgrCallKind::QueryPolicyTrace
        | ElmMgrCallKind::QueryResourceDiagnostics
        | ElmMgrCallKind::QueryRuntimeJournal
        | ElmMgrCallKind::QueryTrustState => {
            if !payload.is_empty() {
                return None;
            }
            ElmMgrAccessTarget::Global
        }
    };
    Some(target)
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

fn read_cell_policy_request(payload: &[u8]) -> Option<ElmCellPolicyRequest> {
    if payload.len() != core::mem::size_of::<ElmCellPolicyRequest>() {
        return None;
    }
    let request = ElmCellPolicyRequest {
        cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn read_cell_policy(payload: &[u8]) -> Option<ElmCellPolicyV1> {
    if payload.len() != core::mem::size_of::<ElmCellPolicyV1>() {
        return None;
    }
    let policy = ElmCellPolicyV1 {
        cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        generation: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        policy_epoch: u64::from_le_bytes(payload[16..24].try_into().ok()?),
        flags: u32::from_le_bytes(payload[24..28].try_into().ok()?),
        allowed_actions: u32::from_le_bytes(payload[28..32].try_into().ok()?),
        provider_flags: u32::from_le_bytes(payload[32..36].try_into().ok()?),
        extension_flags: u32::from_le_bytes(payload[36..40].try_into().ok()?),
        native_flags: u32::from_le_bytes(payload[40..44].try_into().ok()?),
        resource_flags: u32::from_le_bytes(payload[44..48].try_into().ok()?),
        status: i32::from_le_bytes(payload[48..52].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[52..56].try_into().ok()?),
        blockers: u64::from_le_bytes(payload[56..64].try_into().ok()?),
    };
    if policy.reserved != 0 {
        return None;
    }
    Some(policy)
}

fn read_resource_budget_request(payload: &[u8]) -> Option<ElmResourceBudgetRequest> {
    if payload.len() != core::mem::size_of::<ElmResourceBudgetRequest>() {
        return None;
    }
    let request = ElmResourceBudgetRequest {
        cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn read_resource_budget_update_request(payload: &[u8]) -> Option<ElmResourceBudgetUpdateRequest> {
    if payload.len() != core::mem::size_of::<ElmResourceBudgetUpdateRequest>() {
        return None;
    }
    let budget = read_resource_budget_at(payload, 16)?;
    let request = ElmResourceBudgetUpdateRequest {
        cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        flags: u32::from_le_bytes(payload[8..12].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[12..16].try_into().ok()?),
        budget,
    };
    if request.flags != 0 || request.reserved != 0 {
        return None;
    }
    Some(request)
}

fn read_image_session_begin_request(payload: &[u8]) -> Option<ElmImageSessionBeginRequestV1> {
    if payload.len() != core::mem::size_of::<ElmImageSessionBeginRequestV1>() {
        return None;
    }
    let mut expected_digest = [0u8; elm_model::ELM_IMAGE_SESSION_DIGEST_LEN];
    expected_digest.copy_from_slice(&payload[24..56]);
    let request = ElmImageSessionBeginRequestV1 {
        abi_version: u16::from_le_bytes(payload[0..2].try_into().ok()?),
        hash_alg: u16::from_le_bytes(payload[2..4].try_into().ok()?),
        flags: u32::from_le_bytes(payload[4..8].try_into().ok()?),
        total_len: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        ttl_ms: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        digest_len: u16::from_le_bytes(payload[20..22].try_into().ok()?),
        reserved0: u16::from_le_bytes(payload[22..24].try_into().ok()?),
        expected_digest,
        reserved1: u64::from_le_bytes(payload[56..64].try_into().ok()?),
    };
    if request.abi_version != ELM_IMAGE_SESSION_ABI_VERSION
        || request.flags != 0
        || request.reserved0 != 0
        || request.reserved1 != 0
    {
        return None;
    }
    Some(request)
}

fn read_image_session_write_request(
    payload: &[u8],
) -> Option<(ElmImageSessionWriteRequestV1, &[u8])> {
    let request_size = core::mem::size_of::<ElmImageSessionWriteRequestV1>();
    if payload.len() < request_size {
        return None;
    }
    let request = ElmImageSessionWriteRequestV1 {
        abi_version: u16::from_le_bytes(payload[0..2].try_into().ok()?),
        flags: u16::from_le_bytes(payload[2..4].try_into().ok()?),
        reserved0: u32::from_le_bytes(payload[4..8].try_into().ok()?),
        session_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        offset: u64::from_le_bytes(payload[16..24].try_into().ok()?),
        chunk_len: u32::from_le_bytes(payload[24..28].try_into().ok()?),
        reserved1: u32::from_le_bytes(payload[28..32].try_into().ok()?),
    };
    if request.abi_version != ELM_IMAGE_SESSION_ABI_VERSION
        || request.flags != 0
        || request.reserved0 != 0
        || request.reserved1 != 0
        || request.session_id == 0
        || request.chunk_len == 0
        || request.chunk_len as usize > elm_model::ELM_IMAGE_SESSION_MAX_CHUNK
    {
        return None;
    }
    let end = request_size.checked_add(request.chunk_len as usize)?;
    if payload.len() != end {
        return None;
    }
    Some((request, &payload[request_size..end]))
}

fn read_image_session_request(payload: &[u8]) -> Option<ElmImageSessionRequestV1> {
    if payload.len() != core::mem::size_of::<ElmImageSessionRequestV1>() {
        return None;
    }
    let request = ElmImageSessionRequestV1 {
        abi_version: u16::from_le_bytes(payload[0..2].try_into().ok()?),
        flags: u16::from_le_bytes(payload[2..4].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[4..8].try_into().ok()?),
        session_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
    };
    if request.abi_version != ELM_IMAGE_SESSION_ABI_VERSION
        || request.flags != 0
        || request.reserved != 0
        || request.session_id == 0
    {
        return None;
    }
    Some(request)
}

fn dispatch_image_session_call(
    principal: ElmPrincipal,
    kind: ElmMgrCallKind,
    payload: &[u8],
    now_ns: u64,
) -> Vec<u8> {
    let result = match kind {
        ElmMgrCallKind::BeginImageSession => read_image_session_begin_request(payload)
            .ok_or(elm_model::ELM_MGR_STATUS_INVALID)
            .and_then(|request| source::begin_image_session(principal, request, now_ns)),
        ElmMgrCallKind::WriteImageSession => read_image_session_write_request(payload)
            .ok_or(elm_model::ELM_MGR_STATUS_INVALID)
            .and_then(|(request, chunk)| {
                source::write_image_session(
                    principal,
                    request.session_id,
                    request.offset,
                    chunk,
                    now_ns,
                )
            }),
        ElmMgrCallKind::SealImageSession => read_image_session_request(payload)
            .ok_or(elm_model::ELM_MGR_STATUS_INVALID)
            .and_then(|request| source::seal_image_session(principal, request.session_id, now_ns)),
        ElmMgrCallKind::AbortImageSession => read_image_session_request(payload)
            .ok_or(elm_model::ELM_MGR_STATUS_INVALID)
            .and_then(|request| source::abort_image_session(principal, request.session_id, now_ns)),
        ElmMgrCallKind::QueryImageSession => read_image_session_request(payload)
            .ok_or(elm_model::ELM_MGR_STATUS_INVALID)
            .and_then(|request| source::query_image_session(principal, request.session_id, now_ns)),
        _ => Err(elm_model::ELM_MGR_STATUS_UNSUPPORTED),
    };
    match result {
        Ok(info) => response_with_plain_payload(&info),
        Err(status) => response_only(response_header_from_status(status)),
    }
}

fn read_ebi_source_request(payload: &[u8]) -> Option<(ElmEbiSourceRequest, &[u8])> {
    let request_size = ELM_EBI_SOURCE_REQUEST_SIZE;
    if payload.len() < request_size {
        return None;
    }
    let request = ElmEbiSourceRequest {
        abi_version: u16::from_le_bytes(payload[0..2].try_into().ok()?),
        source_kind: u16::from_le_bytes(payload[2..4].try_into().ok()?),
        flags: u32::from_le_bytes(payload[4..8].try_into().ok()?),
        parent_cell_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        budget: read_resource_budget_at(payload, 16)?,
        reserved0: u16::from_le_bytes(payload[80..82].try_into().ok()?),
        payload_len: u32::from_le_bytes(payload[84..88].try_into().ok()?),
        reserved1: u32::from_le_bytes(payload[88..92].try_into().ok()?),
    };
    if request.abi_version != ELM_EBI_SOURCE_ABI_VERSION
        || request.flags != 0
        || request.parent_cell_id == 0
        || request.reserved0 != 0
        || request.reserved1 != 0
        || payload[82..84].iter().any(|byte| *byte != 0)
        || payload[92..request_size].iter().any(|byte| *byte != 0)
    {
        return None;
    }
    let end = request_size.checked_add(request.payload_len as usize)?;
    if payload.len() != end {
        return None;
    }
    Some((request, &payload[request_size..end]))
}

fn read_resource_budget_at(payload: &[u8], offset: usize) -> Option<elm_model::ElmResourceBudget> {
    let end = offset.checked_add(core::mem::size_of::<elm_model::ElmResourceBudget>())?;
    let bytes = payload.get(offset..end)?;
    Some(elm_model::ElmResourceBudget {
        max_provider_ports: u16::from_le_bytes(bytes[0..2].try_into().ok()?),
        max_provider_queue: u16::from_le_bytes(bytes[2..4].try_into().ok()?),
        max_event_subscriptions: u16::from_le_bytes(bytes[4..6].try_into().ok()?),
        max_pending_loads: u16::from_le_bytes(bytes[6..8].try_into().ok()?),
        max_native_images: u16::from_le_bytes(bytes[8..10].try_into().ok()?),
        max_native_faults: u16::from_le_bytes(bytes[10..12].try_into().ok()?),
        max_audit_records: u16::from_le_bytes(bytes[12..14].try_into().ok()?),
        max_concurrent_calls: u16::from_le_bytes(bytes[14..16].try_into().ok()?),
        max_native_image_bytes: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
        max_native_stack_bytes: u64::from_le_bytes(bytes[24..32].try_into().ok()?),
        max_dynamic_alloc_bytes: u64::from_le_bytes(bytes[32..40].try_into().ok()?),
        max_cpu_time_ns_per_call: u64::from_le_bytes(bytes[40..48].try_into().ok()?),
        cpu_budget_ns_per_period: u64::from_le_bytes(bytes[48..56].try_into().ok()?),
        cpu_period_ns: u64::from_le_bytes(bytes[56..64].try_into().ok()?),
    })
}

fn load_projection_image(
    principal: ElmPrincipal,
    payload: &[u8],
    arch: ElmEbiArch,
) -> Result<elm_model::ElmEbiImage, i32> {
    let Some((request, provider_payload)) = read_projection_source_request(payload) else {
        return Err(elm_model::ELM_MGR_STATUS_INVALID);
    };
    if request.flags & ELM_EBI_PROJECTION_SOURCE_FLAG_IMAGE_SESSION != 0 {
        let reference = read_image_session_reference(provider_payload)
            .ok_or(elm_model::ELM_MGR_STATUS_INVALID)?;
        let reader =
            source::consume_image_session(principal, reference.session_id, sched::now_ns_public())?;
        source::project_ebi_image(request.provider_id, &reader, arch)
            .map_err(|_| elm_model::ELM_MGR_STATUS_INVALID)
    } else {
        let reader = ElmSliceImageReader::new(provider_payload);
        source::project_ebi_image(request.provider_id, &reader, arch)
            .map_err(|_| elm_model::ELM_MGR_STATUS_INVALID)
    }
}

fn read_projection_source_request(payload: &[u8]) -> Option<(ElmProjectionSourceRequest, &[u8])> {
    let request_size = ELM_EBI_PROJECTION_SOURCE_REQUEST_SIZE;
    if payload.len() < request_size {
        return None;
    }
    let request = ElmProjectionSourceRequest {
        abi_version: u16::from_le_bytes(payload[0..2].try_into().ok()?),
        flags: u16::from_le_bytes(payload[2..4].try_into().ok()?),
        reserved0: u32::from_le_bytes(payload[4..8].try_into().ok()?),
        provider_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        payload_len: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        reserved1: u32::from_le_bytes(payload[20..24].try_into().ok()?),
    };
    if request.abi_version != ELM_EBI_PROJECTION_SOURCE_ABI_VERSION
        || request.flags & !ELM_EBI_PROJECTION_SOURCE_FLAGS_MASK != 0
        || request.reserved0 != 0
        || request.provider_id == 0
        || request.reserved1 != 0
    {
        return None;
    }
    let end = request_size.checked_add(request.payload_len as usize)?;
    if payload.len() != end {
        return None;
    }
    Some((request, &payload[request_size..end]))
}

fn read_image_session_reference(payload: &[u8]) -> Option<ElmImageSessionReferenceV1> {
    if payload.len() != core::mem::size_of::<ElmImageSessionReferenceV1>() {
        return None;
    }
    let reference = ElmImageSessionReferenceV1 {
        abi_version: u16::from_le_bytes(payload[0..2].try_into().ok()?),
        flags: u16::from_le_bytes(payload[2..4].try_into().ok()?),
        reserved: u32::from_le_bytes(payload[4..8].try_into().ok()?),
        session_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
    };
    if reference.abi_version != ELM_IMAGE_SESSION_REFERENCE_ABI_VERSION
        || reference.flags != 0
        || reference.reserved != 0
        || reference.session_id == 0
    {
        return None;
    }
    Some(reference)
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
    if request.flags & !ELM_PROVIDER_SNAPSHOT_REQUEST_FLAGS_MASK != 0
        || (request.flags & ELM_PROVIDER_SNAPSHOT_REQUEST_FLAG_PAGED == 0 && request.reserved != 0)
    {
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

fn read_extension_attach_request(payload: &[u8]) -> Option<ElmExtensionAttachRequest> {
    if payload.len() != core::mem::size_of::<ElmExtensionAttachRequest>() {
        return None;
    }
    let mut point = [0u8; elm_model::ELM_MGR_EXTENSION_POINT_LEN];
    let mut contract = [0u8; elm_model::ELM_MGR_EXTENSION_CONTRACT_LEN];
    let mut handler_contract = [0u8; elm_model::ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN];
    point.copy_from_slice(&payload[32..32 + elm_model::ELM_MGR_EXTENSION_POINT_LEN]);
    contract.copy_from_slice(
        &payload[32 + elm_model::ELM_MGR_EXTENSION_POINT_LEN
            ..32 + elm_model::ELM_MGR_EXTENSION_POINT_LEN
                + elm_model::ELM_MGR_EXTENSION_CONTRACT_LEN],
    );
    let handler_start =
        32 + elm_model::ELM_MGR_EXTENSION_POINT_LEN + elm_model::ELM_MGR_EXTENSION_CONTRACT_LEN;
    handler_contract.copy_from_slice(
        &payload[handler_start..handler_start + elm_model::ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN],
    );
    let request = ElmExtensionAttachRequest {
        extension_cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        target_cell_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        flags: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        priority: i32::from_le_bytes(payload[20..24].try_into().ok()?),
        point_len: u16::from_le_bytes(payload[24..26].try_into().ok()?),
        contract_len: u16::from_le_bytes(payload[26..28].try_into().ok()?),
        handler_contract_len: u16::from_le_bytes(payload[28..30].try_into().ok()?),
        reserved: u16::from_le_bytes(payload[30..32].try_into().ok()?),
        point,
        contract,
        handler_contract,
    };
    if request.flags != 0
        || request.reserved != 0
        || request.extension_cell_id == 0
        || request.target_cell_id == 0
        || usize::from(request.point_len) > elm_model::ELM_MGR_EXTENSION_POINT_LEN
        || usize::from(request.contract_len) > elm_model::ELM_MGR_EXTENSION_CONTRACT_LEN
        || usize::from(request.handler_contract_len)
            > elm_model::ELM_MGR_EXTENSION_HANDLER_CONTRACT_LEN
    {
        return None;
    }
    Some(request)
}

fn read_extension_detach_request(payload: &[u8]) -> Option<ElmExtensionDetachRequest> {
    if payload.len() != core::mem::size_of::<ElmExtensionDetachRequest>() {
        return None;
    }
    let mut point = [0u8; elm_model::ELM_MGR_EXTENSION_POINT_LEN];
    point.copy_from_slice(&payload[24..24 + elm_model::ELM_MGR_EXTENSION_POINT_LEN]);
    let request = ElmExtensionDetachRequest {
        extension_cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        target_cell_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        flags: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        point_len: u16::from_le_bytes(payload[20..22].try_into().ok()?),
        reserved: u16::from_le_bytes(payload[22..24].try_into().ok()?),
        point,
    };
    if request.flags != 0
        || request.reserved != 0
        || request.extension_cell_id == 0
        || request.target_cell_id == 0
        || usize::from(request.point_len) > elm_model::ELM_MGR_EXTENSION_POINT_LEN
    {
        return None;
    }
    Some(request)
}

fn read_extension_dispatch_request(payload: &[u8]) -> Option<ElmExtensionDispatchRequest> {
    if payload.len() != core::mem::size_of::<ElmExtensionDispatchRequest>() {
        return None;
    }
    let mut point = [0u8; elm_model::ELM_MGR_EXTENSION_POINT_LEN];
    let mut contract = [0u8; elm_model::ELM_MGR_EXTENSION_CONTRACT_LEN];
    let mut dispatch_payload = [0u8; elm_model::ELM_MGR_EXTENSION_PAYLOAD_LEN];
    let point_start = 36;
    let contract_start = point_start + elm_model::ELM_MGR_EXTENSION_POINT_LEN;
    let payload_start = contract_start + elm_model::ELM_MGR_EXTENSION_CONTRACT_LEN;
    point.copy_from_slice(&payload[point_start..contract_start]);
    contract.copy_from_slice(&payload[contract_start..payload_start]);
    dispatch_payload.copy_from_slice(
        &payload[payload_start..payload_start + elm_model::ELM_MGR_EXTENSION_PAYLOAD_LEN],
    );
    let request = ElmExtensionDispatchRequest {
        target_cell_id: u64::from_le_bytes(payload[0..8].try_into().ok()?),
        extension_cell_id: u64::from_le_bytes(payload[8..16].try_into().ok()?),
        opcode: u32::from_le_bytes(payload[16..20].try_into().ok()?),
        flags: u32::from_le_bytes(payload[20..24].try_into().ok()?),
        point_len: u16::from_le_bytes(payload[24..26].try_into().ok()?),
        contract_len: u16::from_le_bytes(payload[26..28].try_into().ok()?),
        payload_len: u16::from_le_bytes(payload[28..30].try_into().ok()?),
        reserved0: u16::from_le_bytes(payload[30..32].try_into().ok()?),
        reserved1: u32::from_le_bytes(payload[32..36].try_into().ok()?),
        point,
        contract,
        payload: dispatch_payload,
    };
    if request.flags & !elm_model::ELM_EXTENSION_DISPATCH_FLAGS_MASK != 0
        || request.reserved0 != 0
        || request.reserved1 != 0
        || request.target_cell_id == 0
        || usize::from(request.point_len) > elm_model::ELM_MGR_EXTENSION_POINT_LEN
        || usize::from(request.contract_len) > elm_model::ELM_MGR_EXTENSION_CONTRACT_LEN
        || usize::from(request.payload_len) > elm_model::ELM_MGR_EXTENSION_PAYLOAD_LEN
    {
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
        elm_model::ELM_MGR_STATUS_PERMISSION => ElmMgrResponseHeader::permission(),
        elm_model::ELM_MGR_STATUS_NOT_FOUND => ElmMgrResponseHeader::not_found(),
        elm_model::ELM_MGR_STATUS_BUSY => ElmMgrResponseHeader::busy(),
        elm_model::ELM_MGR_STATUS_TODO => ElmMgrResponseHeader::todo(),
        elm_model::ELM_MGR_STATUS_UNSUPPORTED => ElmMgrResponseHeader::unsupported(),
        elm_model::ELM_MGR_STATUS_INVALID => ElmMgrResponseHeader::invalid(),
        _ => ElmMgrResponseHeader::error(status),
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

//! 受授权 Manager ELM 使用的类型化管理 API。
//!
//! 本模块只在 `management` feature 下公开。调用方不能取得裸分发入口，所有管理命令
//! 都必须经过这里的固定请求、固定回复或分页回复校验。

use core::marker::PhantomData;

use crate::*;

const RESPONSE_HEADER_SIZE: usize = core::mem::size_of::<ElmMgrResponseHeader>();
const FIXED_RESPONSE_CAPACITY: usize = 4096;

/// 可直接作为管理协议请求发送的固定布局类型。
///
/// # 安全性
///
/// 实现类型必须是 `repr(C)`、不含引用和指针、所有字节均由字段覆盖，并且每个字段在
/// 构造后都已初始化。管理协议当前只支持小端目标。
unsafe trait ManagementRequest: Copy {}

macro_rules! management_requests {
    ($($request:ty),+ $(,)?) => {
        $(
            // 安全性：这些请求类型都经过固定布局测试，且显式保留字段覆盖全部对齐字节。
            unsafe impl ManagementRequest for $request {}
        )+
    };
}

management_requests!(
    ElmEbiSourceRequest,
    ElmReplaceCellRequestV1,
    ElmLifecycleRequest,
    ElmLifecyclePlanRequest,
    ElmNexusBindRequest,
    ElmNexusUnbindRequest,
    ElmRuntimeLogRequest,
    ElmRuntimeEventRequest,
    ElmProviderPortRegisterRequest,
    ElmProviderPortUnregisterRequest,
    ElmProviderInvokeRequest,
    ElmProviderAsyncSubmitRequest,
    ElmProviderAsyncPollRequest,
    ElmProviderAsyncCancelRequest,
    ElmMgrEventSubscribeRequest,
    ElmMgrEventUnsubscribeRequest,
    ElmMgrSubscribedEventReadRequest,
    ElmExtensionAttachRequest,
    ElmExtensionDetachRequest,
    ElmExtensionDispatchRequest,
    ElmCellPolicyRequest,
    ElmCellPolicyV1,
    ElmResourceBudgetRequest,
    ElmResourceBudgetUpdateRequest,
    ElmImageSessionBeginRequestV1,
    ElmImageSessionWriteRequestV1,
    ElmImageSessionRequestV1,
    ElmProviderSnapshotRequest,
);

macro_rules! fixed_method {
    ($name:ident, $kind:ident, $request:ty, $response:ty) => {
        pub fn $name(&self, request: &$request) -> Result<$response, Error> {
            self.call_fixed(ElmMgrCallKind::$kind, wire_bytes(request))
        }
    };
}

macro_rules! empty_fixed_method {
    ($name:ident, $kind:ident, $response:ty) => {
        pub fn $name(&self) -> Result<$response, Error> {
            self.call_fixed(ElmMgrCallKind::$kind, &[])
        }
    };
}

macro_rules! page_method {
    ($name:ident, $kind:ident, $alias:ident, $header:ty, $record:ty) => {
        pub fn $name<'a>(&self, output: &'a mut [u8]) -> Result<$alias<'a>, Error> {
            let payload = self.call(ElmMgrCallKind::$kind, &[], output)?;
            RecordPage::<$header, $record>::parse(payload)
        }
    };
}

macro_rules! fixed_page_method {
    ($name:ident, $kind:ident, $request:ty, $alias:ident, $header:ty, $record:ty) => {
        pub fn $name<'a>(
            &self,
            request: &$request,
            output: &'a mut [u8],
        ) -> Result<$alias<'a>, Error> {
            let payload = self.call(ElmMgrCallKind::$kind, wire_bytes(request), output)?;
            RecordPage::<$header, $record>::parse(payload)
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Runtime(RuntimeApiError),
    BufferTooSmall(usize),
    Status(i32),
    MalformedResponse,
}

impl From<RuntimeApiError> for Error {
    fn from(value: RuntimeApiError) -> Self {
        Self::Runtime(value)
    }
}

#[derive(Clone, Copy)]
pub struct Client {
    table: &'static ElmManagementApiV1,
}

impl Client {
    pub fn acquire() -> Result<Self, Error> {
        let namespace = crate::developer::runtime_api::query_namespace(
            ELM_API_MANAGEMENT_IDENTIFIER,
            &[ELM_API_VERSION_V1],
        )?;
        Self::from_namespace(namespace)
    }

    fn from_namespace(namespace: ElmApiNamespaceV1) -> Result<Self, Error> {
        if namespace.struct_size < core::mem::size_of::<ElmApiNamespaceV1>() as u32
            || namespace.flags != 0
            || namespace.selected_version != ELM_API_VERSION_V1
            || namespace.reserved0 != 0
            || namespace.table_size < core::mem::size_of::<ElmManagementApiV1>() as u32
            || namespace.table_address == 0
            || namespace.generation == 0
            || namespace.capabilities & u64::from(ELM_CELL_POLICY_ALLOW_MANAGEMENT) == 0
        {
            return Err(Error::MalformedResponse);
        }
        // 安全性：命名空间由内核根表返回，表地址指向与内核同寿命的只读静态对象；
        // 上面已经验证版本、最小尺寸和非空地址。
        let table = unsafe { &*(namespace.table_address as *const ElmManagementApiV1) };
        if table.struct_size < core::mem::size_of::<ElmManagementApiV1>() as u32
            || table.abi_version != ELM_API_VERSION_V1
            || table.reserved0 != 0
        {
            return Err(Error::MalformedResponse);
        }
        Ok(Self { table })
    }

    page_method!(
        query_menu,
        QueryMenu,
        MenuPage,
        ElmMenuSnapshotHeader,
        ElmMenuItemSnapshot
    );
    page_method!(
        query_topology,
        QueryTopology,
        TopologyPage,
        ElmMgrTopologyHeader,
        ElmMgrRelationRecord
    );
    empty_fixed_method!(query_policy, QueryPolicy, ElmMgrPolicyInfo);
    page_method!(
        query_audit,
        QueryAudit,
        AuditPage,
        ElmMgrAuditHeader,
        ElmMgrAuditRecord
    );
    page_method!(
        query_nexus_bindings,
        QueryNexusBindings,
        NexusBindingPage,
        ElmNexusBindingSnapshotHeader,
        ElmNexusBindingRecord
    );
    page_method!(
        query_runtime_ports,
        QueryRuntimePorts,
        RuntimePortPage,
        ElmRuntimePortStatsHeader,
        ElmRuntimePortStatsRecord
    );
    page_method!(
        query_provider_ports,
        QueryProviderPorts,
        ProviderPortPage,
        ElmProviderPortStatsHeader,
        ElmProviderPortRecord
    );
    page_method!(
        query_provider_stats,
        QueryProviderStats,
        ProviderStatsPage,
        ElmProviderPortStatsHeader,
        ElmProviderPortStatsRecord
    );
    page_method!(
        query_health,
        QueryHealth,
        HealthPage,
        ElmCoreHealthHeader,
        ElmCoreHealthRecord
    );
    page_method!(
        query_provider_queue,
        QueryProviderQueue,
        ProviderQueuePage,
        ElmProviderQueueStatsHeader,
        ElmProviderQueueStatsRecord
    );
    page_method!(
        query_api_registry,
        QueryApiRegistry,
        ApiRegistryPage,
        ElmMgrApiRegistryHeader,
        ElmMgrApiDescriptor
    );
    page_method!(
        query_event_subscriptions,
        QueryEventSubscriptions,
        EventSubscriptionPage,
        ElmMgrEventSubscriptionHeader,
        ElmMgrEventSubscriptionRecord
    );
    page_method!(
        query_native_capabilities,
        QueryNativeCapabilities,
        NativeCapabilityPage,
        ElmNativeCapabilityHeader,
        ElmNativeCapabilityRecord
    );
    page_method!(
        query_todo_registry,
        QueryTodoRegistry,
        TodoRegistryPage,
        ElmTodoRegistryHeader,
        ElmTodoRegistryRecord
    );
    page_method!(
        query_fault_dump,
        QueryFaultDump,
        FaultDumpPage,
        ElmFaultDumpHeader,
        ElmFaultDumpRecord
    );
    page_method!(
        query_lifecycle_trace,
        QueryLifecycleTrace,
        TracePage,
        ElmRuntimeTraceHeader,
        ElmRuntimeTraceRecord
    );
    page_method!(
        query_provider_call_trace,
        QueryProviderCallTrace,
        TracePage,
        ElmRuntimeTraceHeader,
        ElmRuntimeTraceRecord
    );
    page_method!(
        query_mixin_trace,
        QueryMixinTrace,
        TracePage,
        ElmRuntimeTraceHeader,
        ElmRuntimeTraceRecord
    );
    page_method!(
        query_replace_trace,
        QueryReplaceTrace,
        TracePage,
        ElmRuntimeTraceHeader,
        ElmRuntimeTraceRecord
    );
    page_method!(
        query_policy_trace,
        QueryPolicyTrace,
        TracePage,
        ElmRuntimeTraceHeader,
        ElmRuntimeTraceRecord
    );
    page_method!(
        query_resource_diagnostics,
        QueryResourceDiagnostics,
        TracePage,
        ElmRuntimeTraceHeader,
        ElmRuntimeTraceRecord
    );
    page_method!(
        query_runtime_journal,
        QueryRuntimeJournal,
        TracePage,
        ElmRuntimeTraceHeader,
        ElmRuntimeTraceRecord
    );

    fixed_method!(
        detach_cell,
        DetachCell,
        ElmLifecycleRequest,
        ElmLifecycleResponse
    );
    fixed_method!(
        pause_cell,
        PauseCell,
        ElmLifecycleRequest,
        ElmLifecycleResponse
    );
    fixed_method!(
        resume_cell,
        ResumeCell,
        ElmLifecycleRequest,
        ElmLifecycleResponse
    );
    fixed_method!(
        preflight_lifecycle,
        PreflightLifecycle,
        ElmLifecyclePlanRequest,
        ElmLifecyclePlanResponse
    );
    fixed_method!(
        preflight_bind,
        PreflightBind,
        ElmNexusBindRequest,
        ElmNexusBindPlanResponse
    );
    fixed_method!(
        commit_bind,
        CommitBind,
        ElmNexusBindRequest,
        ElmNexusBindPlanResponse
    );
    fixed_method!(
        preflight_unbind,
        PreflightUnbind,
        ElmNexusUnbindRequest,
        ElmNexusBindPlanResponse
    );
    fixed_method!(
        commit_unbind,
        CommitUnbind,
        ElmNexusUnbindRequest,
        ElmNexusBindPlanResponse
    );
    fixed_method!(
        submit_runtime_log,
        SubmitRuntimeLog,
        ElmRuntimeLogRequest,
        ElmRuntimeLogResponse
    );
    fixed_method!(
        read_runtime_event,
        ReadRuntimeEvent,
        ElmRuntimeEventRequest,
        ElmRuntimeEventResponse
    );
    fixed_method!(
        ack_runtime_event,
        AckRuntimeEvent,
        ElmRuntimeEventRequest,
        ElmRuntimeEventResponse
    );
    fixed_method!(
        register_provider_port,
        RegisterProviderPort,
        ElmProviderPortRegisterRequest,
        ElmProviderPortRegisterResponse
    );
    fixed_method!(
        unregister_provider_port,
        UnregisterProviderPort,
        ElmProviderPortUnregisterRequest,
        ElmProviderPortRegisterResponse
    );
    fixed_method!(
        invoke_provider,
        InvokeProvider,
        ElmProviderInvokeRequest,
        ElmProviderInvokeResponse
    );
    fixed_method!(
        submit_provider_call,
        SubmitProviderCall,
        ElmProviderAsyncSubmitRequest,
        ElmProviderAsyncSubmitResponse
    );
    fixed_method!(
        poll_provider_reply,
        PollProviderReply,
        ElmProviderAsyncPollRequest,
        ElmProviderAsyncPollResponse
    );
    fixed_method!(
        cancel_provider_call,
        CancelProviderCall,
        ElmProviderAsyncCancelRequest,
        ElmProviderAsyncCancelResponse
    );
    fixed_method!(
        subscribe_event,
        SubscribeEvent,
        ElmMgrEventSubscribeRequest,
        ElmMgrEventSubscribeResponse
    );
    fixed_method!(
        unsubscribe_event,
        UnsubscribeEvent,
        ElmMgrEventUnsubscribeRequest,
        ElmMgrEventUnsubscribeResponse
    );
    fixed_page_method!(
        read_subscribed_events,
        ReadSubscribedEvents,
        ElmMgrSubscribedEventReadRequest,
        SubscribedEventPage,
        ElmMgrSubscribedEventReadHeader,
        ElmEventRecord
    );
    fixed_method!(
        preflight_extension_attach,
        PreflightExtensionAttach,
        ElmExtensionAttachRequest,
        ElmExtensionAttachResponse
    );
    fixed_method!(
        commit_extension_attach,
        CommitExtensionAttach,
        ElmExtensionAttachRequest,
        ElmExtensionAttachResponse
    );
    fixed_method!(
        commit_extension_detach,
        CommitExtensionDetach,
        ElmExtensionDetachRequest,
        ElmExtensionDetachResponse
    );
    fixed_method!(
        dispatch_extension,
        DispatchExtension,
        ElmExtensionDispatchRequest,
        ElmExtensionDispatchResponse
    );
    fixed_method!(
        query_cell_policy,
        QueryCellPolicy,
        ElmCellPolicyRequest,
        ElmCellPolicyV1
    );
    fixed_method!(
        update_cell_policy,
        UpdateCellPolicy,
        ElmCellPolicyV1,
        ElmCellPolicyV1
    );
    fixed_method!(
        query_resource_budget,
        QueryResourceBudget,
        ElmResourceBudgetRequest,
        ElmResourceBudgetResponse
    );
    fixed_method!(
        update_resource_budget,
        UpdateResourceBudget,
        ElmResourceBudgetUpdateRequest,
        ElmResourceBudgetResponse
    );
    empty_fixed_method!(query_trust_state, QueryTrustState, ElmTrustRuntimeInfoV1);
    fixed_method!(
        begin_image_session,
        BeginImageSession,
        ElmImageSessionBeginRequestV1,
        ElmImageSessionInfoV1
    );
    fixed_method!(
        seal_image_session,
        SealImageSession,
        ElmImageSessionRequestV1,
        ElmImageSessionInfoV1
    );
    fixed_method!(
        abort_image_session,
        AbortImageSession,
        ElmImageSessionRequestV1,
        ElmImageSessionInfoV1
    );
    fixed_method!(
        query_image_session,
        QueryImageSession,
        ElmImageSessionRequestV1,
        ElmImageSessionInfoV1
    );

    pub fn load_cell(
        &self,
        request: &ElmEbiSourceRequest,
        source_payload: &[u8],
        input: &mut [u8],
    ) -> Result<ElmLoadCellResponse, Error> {
        if request.payload_len as usize != source_payload.len() {
            return Err(Error::MalformedResponse);
        }
        let input = join_request(request, source_payload, input)?;
        self.call_fixed(ElmMgrCallKind::LoadCell, input)
    }

    pub fn replace_cell(
        &self,
        request: &ElmReplaceCellRequestV1,
        source_payload: &[u8],
        input: &mut [u8],
    ) -> Result<ElmReplaceCellResponseV1, Error> {
        if request.source_payload_len as usize != source_payload.len() {
            return Err(Error::MalformedResponse);
        }
        let input = join_request(request, source_payload, input)?;
        self.call_fixed(ElmMgrCallKind::ReplaceCell, input)
    }

    pub fn query_provider_snapshot<'a>(
        &self,
        request: &ElmProviderSnapshotRequest,
        output: &'a mut [u8],
    ) -> Result<ProviderSnapshot<'a>, Error> {
        let payload = self.call(
            ElmMgrCallKind::QueryProviderSnapshot,
            wire_bytes(request),
            output,
        )?;
        ProviderSnapshot::parse(payload)
    }

    pub fn query_extensions<'a>(&self, output: &'a mut [u8]) -> Result<ExtensionPage<'a>, Error> {
        let payload = self.call(ElmMgrCallKind::QueryExtensions, &[], output)?;
        if payload.len() < core::mem::size_of::<ElmExtensionSnapshotHeader>() {
            return Err(Error::MalformedResponse);
        }
        let point_count = read_u32(payload, 4)?;
        let edge_count = read_u32(payload, 8)?;
        let count = point_count
            .checked_add(edge_count)
            .ok_or(Error::MalformedResponse)?;
        RecordPage::parse_with_count(payload, count)
    }

    pub fn write_image_session(
        &self,
        request: &ElmImageSessionWriteRequestV1,
        chunk: &[u8],
        input: &mut [u8],
    ) -> Result<ElmImageSessionInfoV1, Error> {
        if request.chunk_len as usize != chunk.len() {
            return Err(Error::MalformedResponse);
        }
        let input = join_request(request, chunk, input)?;
        self.call_fixed(ElmMgrCallKind::WriteImageSession, input)
    }

    fn call_fixed<R: Copy>(&self, kind: ElmMgrCallKind, input: &[u8]) -> Result<R, Error> {
        let mut output = [0u8; FIXED_RESPONSE_CAPACITY];
        let payload = self.call(kind, input, &mut output)?;
        if payload.len() != core::mem::size_of::<R>() {
            return Err(Error::MalformedResponse);
        }
        // 安全性：长度已经精确校验；回复缓冲可能未按 R 对齐，因此使用非对齐读取。
        Ok(unsafe { (payload.as_ptr() as *const R).read_unaligned() })
    }

    fn call<'a>(
        &self,
        kind: ElmMgrCallKind,
        input: &[u8],
        output: &'a mut [u8],
    ) -> Result<&'a [u8], Error> {
        let mut output_len = 0usize;
        let status = (self.table.dispatch)(
            kind as u32,
            input.as_ptr(),
            input.len(),
            output.as_mut_ptr(),
            output.len(),
            &mut output_len,
        );
        if status == ELM_API_STATUS_BUFFER_TOO_SMALL {
            return Err(Error::BufferTooSmall(output_len));
        }
        if status != ELM_API_STATUS_OK {
            return Err(Error::Status(status));
        }
        if output_len > output.len() || output_len < RESPONSE_HEADER_SIZE {
            return Err(Error::MalformedResponse);
        }
        let response = &output[..output_len];
        let header = read_response_header(response)?;
        if header.reserved != 0
            || header.status != ELM_MGR_STATUS_OK
            || header.payload_len as usize != output_len - RESPONSE_HEADER_SIZE
        {
            return Err(if header.status == ELM_MGR_STATUS_OK {
                Error::MalformedResponse
            } else {
                Error::Status(header.status)
            });
        }
        Ok(&response[RESPONSE_HEADER_SIZE..])
    }
}

pub struct RecordPage<'a, H: Copy, R: Copy> {
    header: H,
    records: &'a [u8],
    count: usize,
    marker: PhantomData<R>,
}

impl<'a, H: Copy, R: Copy> RecordPage<'a, H, R> {
    fn parse(payload: &'a [u8]) -> Result<Self, Error> {
        let count = read_u32(payload, 4)?;
        Self::parse_with_count(payload, count)
    }

    fn parse_with_count(payload: &'a [u8], count: u32) -> Result<Self, Error> {
        let header_size = core::mem::size_of::<H>();
        let record_size = core::mem::size_of::<R>();
        if payload.len() < header_size
            || read_u16(payload, 0)? != ELM_CTL_ABI_VERSION
            || usize::from(read_u16(payload, 2)?) != record_size
        {
            return Err(Error::MalformedResponse);
        }
        let count = count as usize;
        let records_len = record_size
            .checked_mul(count)
            .ok_or(Error::MalformedResponse)?;
        if header_size.checked_add(records_len) != Some(payload.len()) {
            return Err(Error::MalformedResponse);
        }
        // 安全性：头部长度已经校验；回复缓冲可能未按 H 对齐，因此使用非对齐读取。
        let header = unsafe { (payload.as_ptr() as *const H).read_unaligned() };
        Ok(Self {
            header,
            records: &payload[header_size..],
            count,
            marker: PhantomData,
        })
    }

    pub const fn header(&self) -> H {
        self.header
    }

    pub fn records(&self) -> RecordIter<'_, R> {
        RecordIter {
            bytes: self.records,
            index: 0,
            count: self.count,
            marker: PhantomData,
        }
    }

    pub const fn len(&self) -> usize {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }
}

pub struct RecordIter<'a, R: Copy> {
    bytes: &'a [u8],
    index: usize,
    count: usize,
    marker: PhantomData<R>,
}

impl<R: Copy> Iterator for RecordIter<'_, R> {
    type Item = R;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.count {
            return None;
        }
        let offset = self.index.checked_mul(core::mem::size_of::<R>())?;
        self.index += 1;
        // 安全性：RecordPage 已验证完整记录区间，且这里使用非对齐读取。
        Some(unsafe { (self.bytes.as_ptr().add(offset) as *const R).read_unaligned() })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.count.saturating_sub(self.index);
        (remaining, Some(remaining))
    }
}

impl<R: Copy> ExactSizeIterator for RecordIter<'_, R> {}

pub struct ProviderSnapshot<'a> {
    header: ElmProviderSnapshotHeader,
    payload: &'a [u8],
}

impl<'a> ProviderSnapshot<'a> {
    fn parse(payload: &'a [u8]) -> Result<Self, Error> {
        let header_size = core::mem::size_of::<ElmProviderSnapshotHeader>();
        if payload.len() < header_size {
            return Err(Error::MalformedResponse);
        }
        // 安全性：长度已经校验；回复缓冲可能未对齐，因此使用非对齐读取。
        let header =
            unsafe { (payload.as_ptr() as *const ElmProviderSnapshotHeader).read_unaligned() };
        if header.abi_version != ELM_CTL_ABI_VERSION
            || usize::from(header.header_size) != header_size
            || header.status != ELM_MGR_STATUS_OK
            || header.flags & !ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAGS_MASK != 0
            || header.payload_len as usize != payload.len() - header_size
        {
            return Err(Error::MalformedResponse);
        }
        Ok(Self {
            header,
            payload: &payload[header_size..],
        })
    }

    pub const fn header(&self) -> ElmProviderSnapshotHeader {
        self.header
    }

    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }
}

pub type MenuPage<'a> = RecordPage<'a, ElmMenuSnapshotHeader, ElmMenuItemSnapshot>;
pub type TopologyPage<'a> = RecordPage<'a, ElmMgrTopologyHeader, ElmMgrRelationRecord>;
pub type AuditPage<'a> = RecordPage<'a, ElmMgrAuditHeader, ElmMgrAuditRecord>;
pub type NexusBindingPage<'a> =
    RecordPage<'a, ElmNexusBindingSnapshotHeader, ElmNexusBindingRecord>;
pub type RuntimePortPage<'a> = RecordPage<'a, ElmRuntimePortStatsHeader, ElmRuntimePortStatsRecord>;
pub type ProviderPortPage<'a> = RecordPage<'a, ElmProviderPortStatsHeader, ElmProviderPortRecord>;
pub type ProviderStatsPage<'a> =
    RecordPage<'a, ElmProviderPortStatsHeader, ElmProviderPortStatsRecord>;
pub type HealthPage<'a> = RecordPage<'a, ElmCoreHealthHeader, ElmCoreHealthRecord>;
pub type ProviderQueuePage<'a> =
    RecordPage<'a, ElmProviderQueueStatsHeader, ElmProviderQueueStatsRecord>;
pub type ApiRegistryPage<'a> = RecordPage<'a, ElmMgrApiRegistryHeader, ElmMgrApiDescriptor>;
pub type EventSubscriptionPage<'a> =
    RecordPage<'a, ElmMgrEventSubscriptionHeader, ElmMgrEventSubscriptionRecord>;
pub type SubscribedEventPage<'a> = RecordPage<'a, ElmMgrSubscribedEventReadHeader, ElmEventRecord>;
pub type NativeCapabilityPage<'a> =
    RecordPage<'a, ElmNativeCapabilityHeader, ElmNativeCapabilityRecord>;
pub type TodoRegistryPage<'a> = RecordPage<'a, ElmTodoRegistryHeader, ElmTodoRegistryRecord>;
pub type ExtensionPage<'a> = RecordPage<'a, ElmExtensionSnapshotHeader, ElmExtensionSnapshotRecord>;
pub type FaultDumpPage<'a> = RecordPage<'a, ElmFaultDumpHeader, ElmFaultDumpRecord>;
pub type TracePage<'a> = RecordPage<'a, ElmRuntimeTraceHeader, ElmRuntimeTraceRecord>;

fn join_request<'a, T: ManagementRequest>(
    request: &T,
    tail: &[u8],
    output: &'a mut [u8],
) -> Result<&'a [u8], Error> {
    let request = wire_bytes(request);
    let required = request
        .len()
        .checked_add(tail.len())
        .ok_or(Error::MalformedResponse)?;
    if output.len() < required {
        return Err(Error::BufferTooSmall(required));
    }
    output[..request.len()].copy_from_slice(request);
    output[request.len()..required].copy_from_slice(tail);
    Ok(&output[..required])
}

fn wire_bytes<T: ManagementRequest>(value: &T) -> &[u8] {
    // 安全性：ManagementRequest 保证固定布局没有隐式填充，且全部字节均已初始化。
    unsafe {
        core::slice::from_raw_parts(value as *const T as *const u8, core::mem::size_of::<T>())
    }
}

fn read_response_header(bytes: &[u8]) -> Result<ElmMgrResponseHeader, Error> {
    if bytes.len() < RESPONSE_HEADER_SIZE {
        return Err(Error::MalformedResponse);
    }
    Ok(ElmMgrResponseHeader {
        status: i32::from_le_bytes(
            bytes[0..4]
                .try_into()
                .map_err(|_| Error::MalformedResponse)?,
        ),
        payload_len: u32::from_le_bytes(
            bytes[4..8]
                .try_into()
                .map_err(|_| Error::MalformedResponse)?,
        ),
        reserved: u64::from_le_bytes(
            bytes[8..16]
                .try_into()
                .map_err(|_| Error::MalformedResponse)?,
        ),
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, Error> {
    let end = offset.checked_add(2).ok_or(Error::MalformedResponse)?;
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or(Error::MalformedResponse)?
            .try_into()
            .map_err(|_| Error::MalformedResponse)?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let end = offset.checked_add(4).ok_or(Error::MalformedResponse)?;
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or(Error::MalformedResponse)?
            .try_into()
            .map_err(|_| Error::MalformedResponse)?,
    ))
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    use super::*;

    static LAST_KIND: AtomicU32 = AtomicU32::new(0);
    static LAST_INPUT_LEN: AtomicUsize = AtomicUsize::new(0);
    static MOCK_MODE: AtomicU32 = AtomicU32::new(0);

    #[derive(Clone, Copy)]
    enum MockShape {
        Fixed(usize),
        Page { header: usize, record: usize },
        ProviderSnapshot,
    }

    macro_rules! fixed_shape {
        ($response:ty) => {
            MockShape::Fixed(core::mem::size_of::<$response>())
        };
    }

    macro_rules! page_shape {
        ($header:ty, $record:ty) => {
            MockShape::Page {
                header: core::mem::size_of::<$header>(),
                record: core::mem::size_of::<$record>(),
            }
        };
    }

    fn mock_shape(kind: ElmMgrCallKind) -> MockShape {
        match kind {
            ElmMgrCallKind::QueryMenu => page_shape!(ElmMenuSnapshotHeader, ElmMenuItemSnapshot),
            ElmMgrCallKind::LoadCell => fixed_shape!(ElmLoadCellResponse),
            ElmMgrCallKind::DetachCell | ElmMgrCallKind::PauseCell | ElmMgrCallKind::ResumeCell => {
                fixed_shape!(ElmLifecycleResponse)
            }
            ElmMgrCallKind::ReplaceCell => fixed_shape!(ElmReplaceCellResponseV1),
            ElmMgrCallKind::QueryTopology => {
                page_shape!(ElmMgrTopologyHeader, ElmMgrRelationRecord)
            }
            ElmMgrCallKind::QueryPolicy => fixed_shape!(ElmMgrPolicyInfo),
            ElmMgrCallKind::PreflightLifecycle => fixed_shape!(ElmLifecyclePlanResponse),
            ElmMgrCallKind::QueryAudit => page_shape!(ElmMgrAuditHeader, ElmMgrAuditRecord),
            ElmMgrCallKind::QueryNexusBindings => {
                page_shape!(ElmNexusBindingSnapshotHeader, ElmNexusBindingRecord)
            }
            ElmMgrCallKind::PreflightBind | ElmMgrCallKind::CommitBind => {
                fixed_shape!(ElmNexusBindPlanResponse)
            }
            ElmMgrCallKind::PreflightUnbind | ElmMgrCallKind::CommitUnbind => {
                fixed_shape!(ElmNexusBindPlanResponse)
            }
            ElmMgrCallKind::SubmitRuntimeLog => fixed_shape!(ElmRuntimeLogResponse),
            ElmMgrCallKind::ReadRuntimeEvent | ElmMgrCallKind::AckRuntimeEvent => {
                fixed_shape!(ElmRuntimeEventResponse)
            }
            ElmMgrCallKind::QueryRuntimePorts => {
                page_shape!(ElmRuntimePortStatsHeader, ElmRuntimePortStatsRecord)
            }
            ElmMgrCallKind::RegisterProviderPort | ElmMgrCallKind::UnregisterProviderPort => {
                fixed_shape!(ElmProviderPortRegisterResponse)
            }
            ElmMgrCallKind::QueryProviderPorts => {
                page_shape!(ElmProviderPortStatsHeader, ElmProviderPortRecord)
            }
            ElmMgrCallKind::InvokeProvider => fixed_shape!(ElmProviderInvokeResponse),
            ElmMgrCallKind::QueryProviderStats => {
                page_shape!(ElmProviderPortStatsHeader, ElmProviderPortStatsRecord)
            }
            ElmMgrCallKind::QueryHealth => page_shape!(ElmCoreHealthHeader, ElmCoreHealthRecord),
            ElmMgrCallKind::SubmitProviderCall => fixed_shape!(ElmProviderAsyncSubmitResponse),
            ElmMgrCallKind::PollProviderReply => fixed_shape!(ElmProviderAsyncPollResponse),
            ElmMgrCallKind::CancelProviderCall => fixed_shape!(ElmProviderAsyncCancelResponse),
            ElmMgrCallKind::QueryProviderQueue => {
                page_shape!(ElmProviderQueueStatsHeader, ElmProviderQueueStatsRecord)
            }
            ElmMgrCallKind::QueryApiRegistry => {
                page_shape!(ElmMgrApiRegistryHeader, ElmMgrApiDescriptor)
            }
            ElmMgrCallKind::SubscribeEvent => fixed_shape!(ElmMgrEventSubscribeResponse),
            ElmMgrCallKind::UnsubscribeEvent => fixed_shape!(ElmMgrEventUnsubscribeResponse),
            ElmMgrCallKind::QueryEventSubscriptions => {
                page_shape!(ElmMgrEventSubscriptionHeader, ElmMgrEventSubscriptionRecord)
            }
            ElmMgrCallKind::ReadSubscribedEvents => {
                page_shape!(ElmMgrSubscribedEventReadHeader, ElmEventRecord)
            }
            ElmMgrCallKind::QueryProviderSnapshot => MockShape::ProviderSnapshot,
            ElmMgrCallKind::QueryNativeCapabilities => {
                page_shape!(ElmNativeCapabilityHeader, ElmNativeCapabilityRecord)
            }
            ElmMgrCallKind::QueryTodoRegistry => {
                page_shape!(ElmTodoRegistryHeader, ElmTodoRegistryRecord)
            }
            ElmMgrCallKind::QueryExtensions => {
                page_shape!(ElmExtensionSnapshotHeader, ElmExtensionSnapshotRecord)
            }
            ElmMgrCallKind::PreflightExtensionAttach | ElmMgrCallKind::CommitExtensionAttach => {
                fixed_shape!(ElmExtensionAttachResponse)
            }
            ElmMgrCallKind::CommitExtensionDetach => fixed_shape!(ElmExtensionDetachResponse),
            ElmMgrCallKind::DispatchExtension => fixed_shape!(ElmExtensionDispatchResponse),
            ElmMgrCallKind::QueryFaultDump => page_shape!(ElmFaultDumpHeader, ElmFaultDumpRecord),
            ElmMgrCallKind::QueryLifecycleTrace
            | ElmMgrCallKind::QueryProviderCallTrace
            | ElmMgrCallKind::QueryMixinTrace
            | ElmMgrCallKind::QueryReplaceTrace
            | ElmMgrCallKind::QueryPolicyTrace
            | ElmMgrCallKind::QueryResourceDiagnostics
            | ElmMgrCallKind::QueryRuntimeJournal => {
                page_shape!(ElmRuntimeTraceHeader, ElmRuntimeTraceRecord)
            }
            ElmMgrCallKind::QueryCellPolicy | ElmMgrCallKind::UpdateCellPolicy => {
                fixed_shape!(ElmCellPolicyV1)
            }
            ElmMgrCallKind::QueryResourceBudget | ElmMgrCallKind::UpdateResourceBudget => {
                fixed_shape!(ElmResourceBudgetResponse)
            }
            ElmMgrCallKind::QueryTrustState => fixed_shape!(ElmTrustRuntimeInfoV1),
            ElmMgrCallKind::BeginImageSession
            | ElmMgrCallKind::WriteImageSession
            | ElmMgrCallKind::SealImageSession
            | ElmMgrCallKind::AbortImageSession
            | ElmMgrCallKind::QueryImageSession => fixed_shape!(ElmImageSessionInfoV1),
        }
    }

    extern "C" fn mock_dispatch(
        raw_kind: u32,
        _input: *const u8,
        input_len: usize,
        output: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        let Some(kind) = ElmMgrCallKind::from_raw(raw_kind) else {
            return ELM_API_STATUS_UNSUPPORTED;
        };
        LAST_KIND.store(raw_kind, Ordering::Release);
        LAST_INPUT_LEN.store(input_len, Ordering::Release);
        let shape = mock_shape(kind);
        let payload_len = match shape {
            MockShape::Fixed(size) => size,
            MockShape::Page { header, .. } => header,
            MockShape::ProviderSnapshot => core::mem::size_of::<ElmProviderSnapshotHeader>(),
        };
        let total_len = RESPONSE_HEADER_SIZE + payload_len;
        if output_len.is_null() {
            return ELM_API_STATUS_INVALID;
        }
        let mode = MOCK_MODE.load(Ordering::Acquire);
        let reported_len = if mode == 4 {
            total_len.saturating_add(7)
        } else {
            total_len
        };
        // 安全性：测试调用方总是传入有效的 usize 输出槽。
        unsafe { output_len.write(reported_len) };
        if mode == 4 {
            return ELM_API_STATUS_BUFFER_TOO_SMALL;
        }
        if output.is_null() || output_capacity < total_len {
            return ELM_API_STATUS_BUFFER_TOO_SMALL;
        }
        // 安全性：上面已经验证指针非空且容量覆盖完整模拟回复。
        let output = unsafe { core::slice::from_raw_parts_mut(output, total_len) };
        output.fill(0);
        output[4..8].copy_from_slice(&(payload_len as u32).to_le_bytes());
        let payload = &mut output[RESPONSE_HEADER_SIZE..];
        match shape {
            MockShape::Fixed(_) => {}
            MockShape::Page { record, .. } => {
                payload[0..2].copy_from_slice(&ELM_CTL_ABI_VERSION.to_le_bytes());
                payload[2..4].copy_from_slice(&(record as u16).to_le_bytes());
            }
            MockShape::ProviderSnapshot => {
                payload[0..2].copy_from_slice(&ELM_CTL_ABI_VERSION.to_le_bytes());
                payload[2..4].copy_from_slice(&(payload_len as u16).to_le_bytes());
            }
        }
        match mode {
            1 => output[8..16].copy_from_slice(&1u64.to_le_bytes()),
            2 => output[4..8].copy_from_slice(&((payload_len + 1) as u32).to_le_bytes()),
            3 => payload[2..4].copy_from_slice(&0u16.to_le_bytes()),
            5 => output[0..4].copy_from_slice(&ELM_MGR_STATUS_PERMISSION.to_le_bytes()),
            _ => {}
        }
        ELM_API_STATUS_OK
    }

    static MOCK_TABLE: ElmManagementApiV1 = ElmManagementApiV1 {
        struct_size: core::mem::size_of::<ElmManagementApiV1>() as u32,
        abi_version: ELM_API_VERSION_V1,
        reserved0: 0,
        dispatch: mock_dispatch,
    };

    static INVALID_TABLE: ElmManagementApiV1 = ElmManagementApiV1 {
        struct_size: core::mem::size_of::<ElmManagementApiV1>() as u32,
        abi_version: ELM_API_VERSION_V1,
        reserved0: 1,
        dispatch: mock_dispatch,
    };

    fn management_namespace(table: &'static ElmManagementApiV1) -> ElmApiNamespaceV1 {
        ElmApiNamespaceV1 {
            struct_size: core::mem::size_of::<ElmApiNamespaceV1>() as u32,
            flags: 0,
            selected_version: ELM_API_VERSION_V1,
            reserved0: 0,
            table_size: core::mem::size_of::<ElmManagementApiV1>() as u32,
            table_address: table as *const ElmManagementApiV1 as usize,
            generation: 1,
            capabilities: u64::from(ELM_CELL_POLICY_ALLOW_MANAGEMENT),
        }
    }

    #[test]
    fn management_client_validates_namespace_and_table_metadata() {
        let valid = management_namespace(&MOCK_TABLE);
        assert!(Client::from_namespace(valid).is_ok());

        let mut invalid = valid;
        invalid.flags = 1;
        assert!(matches!(
            Client::from_namespace(invalid),
            Err(Error::MalformedResponse)
        ));
        invalid = valid;
        invalid.generation = 0;
        assert!(matches!(
            Client::from_namespace(invalid),
            Err(Error::MalformedResponse)
        ));
        invalid = valid;
        invalid.capabilities = 0;
        assert!(matches!(
            Client::from_namespace(invalid),
            Err(Error::MalformedResponse)
        ));
        invalid = valid;
        invalid.table_size = 0;
        assert!(matches!(
            Client::from_namespace(invalid),
            Err(Error::MalformedResponse)
        ));
        invalid = management_namespace(&INVALID_TABLE);
        assert!(matches!(
            Client::from_namespace(invalid),
            Err(Error::MalformedResponse)
        ));
    }

    #[test]
    fn management_request_layouts_have_no_implicit_padding() {
        macro_rules! assert_size {
            ($type:ty, $size:expr) => {
                assert_eq!(core::mem::size_of::<$type>(), $size, stringify!($type));
            };
        }

        assert_size!(ElmEbiSourceRequest, 96);
        assert_size!(ElmReplaceCellRequestV1, 32);
        assert_size!(ElmLifecycleRequest, 16);
        assert_size!(ElmLifecyclePlanRequest, 16);
        assert_size!(ElmNexusBindRequest, 88);
        assert_size!(ElmNexusUnbindRequest, 16);
        assert_size!(ElmRuntimeLogRequest, 280);
        assert_size!(ElmRuntimeEventRequest, 24);
        assert_size!(ElmProviderPortRegisterRequest, 96);
        assert_size!(ElmProviderPortUnregisterRequest, 16);
        assert_size!(ElmProviderInvokeRequest, 288);
        assert_size!(ElmProviderAsyncSubmitRequest, 304);
        assert_size!(ElmProviderAsyncPollRequest, 16);
        assert_size!(ElmProviderAsyncCancelRequest, 16);
        assert_size!(ElmMgrEventSubscribeRequest, 48);
        assert_size!(ElmMgrEventUnsubscribeRequest, 24);
        assert_size!(ElmMgrSubscribedEventReadRequest, 24);
        assert_size!(ElmExtensionAttachRequest, 192);
        assert_size!(ElmExtensionDetachRequest, 56);
        assert_size!(ElmExtensionDispatchRequest, 392);
        assert_size!(ElmCellPolicyRequest, 16);
        assert_size!(ElmCellPolicyV1, 64);
        assert_size!(ElmResourceBudgetRequest, 16);
        assert_size!(ElmResourceBudgetUpdateRequest, 80);
        assert_size!(ElmImageSessionBeginRequestV1, 64);
        assert_size!(ElmImageSessionWriteRequestV1, 32);
        assert_size!(ElmImageSessionRequestV1, 16);
        assert_size!(ElmProviderSnapshotRequest, 24);

        assert_eq!(core::mem::offset_of!(ElmEbiSourceRequest, reserved1), 82);
        assert_eq!(core::mem::offset_of!(ElmEbiSourceRequest, payload_len), 84);
        assert_eq!(core::mem::offset_of!(ElmEbiSourceRequest, reserved3), 92);
        assert_eq!(
            core::mem::offset_of!(ElmExtensionDispatchRequest, reserved2),
            388
        );

        let source = ElmEbiSourceRequest::new(ElmEbiSourceKind::Memory, 0);
        assert!(wire_bytes(&source)[80..96].iter().all(|byte| *byte == 0));
        let extension = ElmExtensionDispatchRequest::new(1, 2, 0, "point", "contract@1");
        assert_eq!(&wire_bytes(&extension)[388..392], &[0; 4]);
    }

    #[test]
    fn management_client_maps_all_v1_calls_and_rejects_malformed_replies() {
        let client = Client { table: &MOCK_TABLE };
        let mut seen = 0u64;
        let mut output = [0u8; 1024];
        let frame = ElmCallFrame::empty(1, 2, 3);

        macro_rules! call {
            ($kind:ident, $input_len:expr, $expression:expr) => {{
                MOCK_MODE.store(0, Ordering::Release);
                $expression.expect(stringify!($kind));
                assert_eq!(
                    LAST_KIND.load(Ordering::Acquire),
                    ElmMgrCallKind::$kind as u32
                );
                assert_eq!(LAST_INPUT_LEN.load(Ordering::Acquire), $input_len);
                seen |= 1u64 << (ElmMgrCallKind::$kind as u32 - 1);
            }};
        }

        call!(QueryMenu, 0, client.query_menu(&mut output));
        let source_payload = [1u8, 2];
        let source =
            ElmEbiSourceRequest::new(ElmEbiSourceKind::Memory, source_payload.len() as u32);
        let mut joined = [0u8; 512];
        call!(
            LoadCell,
            core::mem::size_of::<ElmEbiSourceRequest>() + source_payload.len(),
            client.load_cell(&source, &source_payload, &mut joined)
        );
        let lifecycle = ElmLifecycleRequest::new(9);
        call!(
            DetachCell,
            core::mem::size_of_val(&lifecycle),
            client.detach_cell(&lifecycle)
        );
        call!(
            PauseCell,
            core::mem::size_of_val(&lifecycle),
            client.pause_cell(&lifecycle)
        );
        call!(
            ResumeCell,
            core::mem::size_of_val(&lifecycle),
            client.resume_cell(&lifecycle)
        );
        let replacement_payload = [3u8, 4, 5];
        let replacement = ElmReplaceCellRequestV1::new(
            9,
            ElmEbiSourceKind::Memory as u16,
            replacement_payload.len() as u32,
        );
        call!(
            ReplaceCell,
            core::mem::size_of_val(&replacement) + replacement_payload.len(),
            client.replace_cell(&replacement, &replacement_payload, &mut joined)
        );
        call!(QueryTopology, 0, client.query_topology(&mut output));
        call!(QueryPolicy, 0, client.query_policy());
        let lifecycle_plan = ElmLifecyclePlanRequest::new(9, ElmLifecycleAction::Pause);
        call!(
            PreflightLifecycle,
            core::mem::size_of_val(&lifecycle_plan),
            client.preflight_lifecycle(&lifecycle_plan)
        );
        call!(QueryAudit, 0, client.query_audit(&mut output));
        call!(
            QueryNexusBindings,
            0,
            client.query_nexus_bindings(&mut output)
        );
        let bind = ElmNexusBindRequest::new(9, 10, "test.contract@1");
        call!(
            PreflightBind,
            core::mem::size_of_val(&bind),
            client.preflight_bind(&bind)
        );
        call!(
            CommitBind,
            core::mem::size_of_val(&bind),
            client.commit_bind(&bind)
        );
        let unbind = ElmNexusUnbindRequest::new(11);
        call!(
            PreflightUnbind,
            core::mem::size_of_val(&unbind),
            client.preflight_unbind(&unbind)
        );
        call!(
            CommitUnbind,
            core::mem::size_of_val(&unbind),
            client.commit_unbind(&unbind)
        );
        let runtime_log = ElmRuntimeLogRequest::new(11, 6, "test");
        call!(
            SubmitRuntimeLog,
            core::mem::size_of_val(&runtime_log),
            client.submit_runtime_log(&runtime_log)
        );
        let runtime_event = ElmRuntimeEventRequest::new(11, 0);
        call!(
            ReadRuntimeEvent,
            core::mem::size_of_val(&runtime_event),
            client.read_runtime_event(&runtime_event)
        );
        call!(
            AckRuntimeEvent,
            core::mem::size_of_val(&runtime_event),
            client.ack_runtime_event(&runtime_event)
        );
        call!(
            QueryRuntimePorts,
            0,
            client.query_runtime_ports(&mut output)
        );
        let register = ElmProviderPortRegisterRequest::new(
            9,
            "test.provider@1",
            ElmPortAccessPolicy::Public,
            FlowDirection::Duplex,
            FlowMode::Shared,
            0,
        );
        call!(
            RegisterProviderPort,
            core::mem::size_of_val(&register),
            client.register_provider_port(&register)
        );
        let unregister = ElmProviderPortUnregisterRequest::new(10);
        call!(
            UnregisterProviderPort,
            core::mem::size_of_val(&unregister),
            client.unregister_provider_port(&unregister)
        );
        call!(
            QueryProviderPorts,
            0,
            client.query_provider_ports(&mut output)
        );
        let invoke = ElmProviderInvokeRequest::new(frame);
        call!(
            InvokeProvider,
            core::mem::size_of_val(&invoke),
            client.invoke_provider(&invoke)
        );
        call!(
            QueryProviderStats,
            0,
            client.query_provider_stats(&mut output)
        );
        call!(QueryHealth, 0, client.query_health(&mut output));
        let submit = ElmProviderAsyncSubmitRequest::new(frame, 100, 1000);
        call!(
            SubmitProviderCall,
            core::mem::size_of_val(&submit),
            client.submit_provider_call(&submit)
        );
        let poll = ElmProviderAsyncPollRequest::new(12);
        call!(
            PollProviderReply,
            core::mem::size_of_val(&poll),
            client.poll_provider_reply(&poll)
        );
        let cancel = ElmProviderAsyncCancelRequest::new(12);
        call!(
            CancelProviderCall,
            core::mem::size_of_val(&cancel),
            client.cancel_provider_call(&cancel)
        );
        call!(
            QueryProviderQueue,
            0,
            client.query_provider_queue(&mut output)
        );
        call!(QueryApiRegistry, 0, client.query_api_registry(&mut output));
        let subscribe = ElmMgrEventSubscribeRequest::new(9);
        call!(
            SubscribeEvent,
            core::mem::size_of_val(&subscribe),
            client.subscribe_event(&subscribe)
        );
        let unsubscribe = ElmMgrEventUnsubscribeRequest::new(13, 9);
        call!(
            UnsubscribeEvent,
            core::mem::size_of_val(&unsubscribe),
            client.unsubscribe_event(&unsubscribe)
        );
        call!(
            QueryEventSubscriptions,
            0,
            client.query_event_subscriptions(&mut output)
        );
        let event_read = ElmMgrSubscribedEventReadRequest::new(13, 0, 8);
        call!(
            ReadSubscribedEvents,
            core::mem::size_of_val(&event_read),
            client.read_subscribed_events(&event_read, &mut output)
        );
        let snapshot = ElmProviderSnapshotRequest::by_port(10);
        call!(
            QueryProviderSnapshot,
            core::mem::size_of_val(&snapshot),
            client.query_provider_snapshot(&snapshot, &mut output)
        );
        call!(
            QueryNativeCapabilities,
            0,
            client.query_native_capabilities(&mut output)
        );
        call!(
            QueryTodoRegistry,
            0,
            client.query_todo_registry(&mut output)
        );
        call!(QueryExtensions, 0, client.query_extensions(&mut output));
        let attach = ElmExtensionAttachRequest::new(14, 9, "point", "contract@1");
        call!(
            PreflightExtensionAttach,
            core::mem::size_of_val(&attach),
            client.preflight_extension_attach(&attach)
        );
        call!(
            CommitExtensionAttach,
            core::mem::size_of_val(&attach),
            client.commit_extension_attach(&attach)
        );
        let detach = ElmExtensionDetachRequest::new(14, 9, "point");
        call!(
            CommitExtensionDetach,
            core::mem::size_of_val(&detach),
            client.commit_extension_detach(&detach)
        );
        let dispatch = ElmExtensionDispatchRequest::new(9, 14, 1, "point", "contract@1");
        call!(
            DispatchExtension,
            core::mem::size_of_val(&dispatch),
            client.dispatch_extension(&dispatch)
        );
        call!(QueryFaultDump, 0, client.query_fault_dump(&mut output));
        call!(
            QueryLifecycleTrace,
            0,
            client.query_lifecycle_trace(&mut output)
        );
        call!(
            QueryProviderCallTrace,
            0,
            client.query_provider_call_trace(&mut output)
        );
        call!(QueryMixinTrace, 0, client.query_mixin_trace(&mut output));
        call!(
            QueryReplaceTrace,
            0,
            client.query_replace_trace(&mut output)
        );
        call!(QueryPolicyTrace, 0, client.query_policy_trace(&mut output));
        call!(
            QueryResourceDiagnostics,
            0,
            client.query_resource_diagnostics(&mut output)
        );
        call!(
            QueryRuntimeJournal,
            0,
            client.query_runtime_journal(&mut output)
        );
        let cell_policy_request = ElmCellPolicyRequest::new(9);
        call!(
            QueryCellPolicy,
            core::mem::size_of_val(&cell_policy_request),
            client.query_cell_policy(&cell_policy_request)
        );
        let cell_policy = ElmCellPolicyV1::new(9, 1, ELM_CELL_POLICY_ALLOW_ALL, 0, 0);
        call!(
            UpdateCellPolicy,
            core::mem::size_of_val(&cell_policy),
            client.update_cell_policy(&cell_policy)
        );
        let budget_request = ElmResourceBudgetRequest::new(9);
        call!(
            QueryResourceBudget,
            core::mem::size_of_val(&budget_request),
            client.query_resource_budget(&budget_request)
        );
        let budget_update = ElmResourceBudgetUpdateRequest::new(9, ElmResourceBudget::DEFAULT);
        call!(
            UpdateResourceBudget,
            core::mem::size_of_val(&budget_update),
            client.update_resource_budget(&budget_update)
        );
        call!(QueryTrustState, 0, client.query_trust_state());
        let begin = ElmImageSessionBeginRequestV1::new(1024, 1000, [0; 32]);
        call!(
            BeginImageSession,
            core::mem::size_of_val(&begin),
            client.begin_image_session(&begin)
        );
        let chunk = [7u8, 8, 9];
        let write = ElmImageSessionWriteRequestV1::new(15, 0, chunk.len() as u32);
        call!(
            WriteImageSession,
            core::mem::size_of_val(&write) + chunk.len(),
            client.write_image_session(&write, &chunk, &mut joined)
        );
        let session = ElmImageSessionRequestV1::new(15);
        call!(
            SealImageSession,
            core::mem::size_of_val(&session),
            client.seal_image_session(&session)
        );
        call!(
            AbortImageSession,
            core::mem::size_of_val(&session),
            client.abort_image_session(&session)
        );
        call!(
            QueryImageSession,
            core::mem::size_of_val(&session),
            client.query_image_session(&session)
        );

        assert_eq!(seen, (1u64 << 60) - 1);

        MOCK_MODE.store(1, Ordering::Release);
        assert_eq!(client.query_policy(), Err(Error::MalformedResponse));
        MOCK_MODE.store(2, Ordering::Release);
        assert_eq!(client.query_policy(), Err(Error::MalformedResponse));
        MOCK_MODE.store(3, Ordering::Release);
        assert!(matches!(
            client.query_menu(&mut output),
            Err(Error::MalformedResponse)
        ));
        MOCK_MODE.store(4, Ordering::Release);
        assert!(matches!(
            client.query_policy(),
            Err(Error::BufferTooSmall(_))
        ));
        MOCK_MODE.store(5, Ordering::Release);
        assert_eq!(
            client.query_policy(),
            Err(Error::Status(ELM_MGR_STATUS_PERMISSION))
        );
        MOCK_MODE.store(0, Ordering::Release);
    }
}

//! 受授权 Manager ELM 使用的类型化管理 API。
//!
//! 本模块只在 `management` feature 下公开。调用方不能取得裸分发入口，所有管理命令
//! 都必须经过这里的固定请求、固定回复或分页回复校验。
//!
//! # 权限边界
//!
//! [`Client::acquire`] 通过根 API 查询 `org.elm.management` 命名空间。内核只有在当前调用
//! 来自 `Manager` kind、镜像证明可信、cell 处于可管理状态、generation 当前且策略显式授予
//! management capability 时才返回函数表。取得 `Client` 不是永久授权：每次 dispatch 都会
//! 在内核重新鉴权，因此热替换、暂停、策略收紧或信任撤销会立即影响后续调用。
//!
//! 普通 ELM 不应启用此 feature。事件注册、日志、受管 import 和 mixin 分发等普通运行时
//! 能力位于 [`crate::runtime`] 或对应安全包装，不需要管理命名空间。
//!
//! # 回复校验
//!
//! `Client` 从不把裸 output 指针或管理函数表暴露给业务代码。固定回复必须精确等于目标类型
//! 尺寸；分页回复必须同时满足 ABI 版本、记录尺寸、记录数量和总缓冲区长度；provider 快照
//! 还会校验 header size、flags 和 payload length。任何保留字段、长度或状态不一致都返回
//! [`Error::MalformedResponse`]。
//!
//! 对可变长查询，调用方提供工作缓冲区。容量不足时返回 [`Error::BufferTooSmall`]，其中携带
//! 运行时报告的所需尺寸，调用方可以扩大缓冲区后重试。所有页对象借用该缓冲区，页或迭代器
//! 存活期间不能再次把同一缓冲区交给其他管理调用。
//!
//! # 示例
//!
//! ```no_run
//! use elm::management::{Client, Error};
//!
//! let manager = Client::acquire()?;
//! let policy = manager.query_policy()?;
//! let mut storage = [0_u8; 4096];
//! let health = manager.query_health(&mut storage)?;
//! for record in health.records() {
//!     if record.status != elm::ELM_MGR_STATUS_OK {
//!         // 根据 record.check 和 record.detail 输出诊断。
//!     }
//! }
//! # Ok::<(), Error>(())
//! ```

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
        #[doc = concat!("执行管理动作 `", stringify!($kind), "`，并返回固定布局回复。")]
        #[doc = ""]
        #[doc = concat!("请求类型为 `", stringify!($request), "`，回复类型为 `", stringify!($response), "`。")]
        #[doc = "客户端会把请求按当前小端固定布局发送，验证通用回复头、管理状态和精确 payload 尺寸后才构造返回值。"]
        #[doc = ""]
        #[doc = "# 错误"]
        #[doc = ""]
        #[doc = "权限或策略拒绝、对象不存在、状态冲突等返回 `Error::Status`；固定回复超过内部 4096 字节容量时返回 `Error::BufferTooSmall`；任何布局不一致返回 `Error::MalformedResponse`。"]
        pub fn $name(&self, request: &$request) -> Result<$response, Error> {
            self.call_fixed(ElmMgrCallKind::$kind, wire_bytes(request))
        }
    };
}

macro_rules! empty_fixed_method {
    ($name:ident, $kind:ident, $response:ty) => {
        #[doc = concat!("执行无请求载荷的管理动作 `", stringify!($kind), "`。")]
        #[doc = ""]
        #[doc = concat!("成功时返回已经过状态、保留字段和尺寸校验的 `", stringify!($response), "`。")]
        #[doc = "失败可能是重新鉴权拒绝、运行时业务状态错误、内部固定回复容量不足或畸形回复。"]
        pub fn $name(&self) -> Result<$response, Error> {
            self.call_fixed(ElmMgrCallKind::$kind, &[])
        }
    };
}

macro_rules! page_method {
    ($name:ident, $kind:ident, $alias:ident, $header:ty, $record:ty) => {
        #[doc = concat!("查询管理快照 `", stringify!($kind), "`。")]
        #[doc = ""]
        #[doc = concat!("`output` 同时承载通用回复头、`", stringify!($header), "` 和零条或多条 `", stringify!($record), "`。成功后返回 `", stringify!($alias), "`，其生命周期受 `output` 约束。")]
        #[doc = "客户端会验证 ABI 版本、单条记录尺寸、记录数量乘法溢出和总长度，不会接受截断页或尾随字节。缓冲区不足时错误携带所需尺寸，可扩大后重试。"]
        pub fn $name<'a>(&self, output: &'a mut [u8]) -> Result<$alias<'a>, Error> {
            let payload = self.call(ElmMgrCallKind::$kind, &[], output)?;
            RecordPage::<$header, $record>::parse(payload)
        }
    };
}

macro_rules! fixed_page_method {
    ($name:ident, $kind:ident, $request:ty, $alias:ident, $header:ty, $record:ty) => {
        #[doc = concat!("使用 `", stringify!($request), "` 查询分页管理快照 `", stringify!($kind), "`。")]
        #[doc = ""]
        #[doc = concat!("回复由 `", stringify!($header), "` 和零条或多条 `", stringify!($record), "` 构成；返回的 `", stringify!($alias), "` 借用 `output`。")]
        #[doc = "客户端会验证游标请求的固定布局、通用回复状态、记录尺寸、数量和总长度。容量不足时返回所需字节数，畸形分页数据不会部分暴露。"]
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
/// 类型化管理客户端可观察的失败。
///
/// 此枚举保留运行时链接错误、输出容量问题、管理业务状态和协议破坏之间的区别。调用方只应
/// 对明确可重试的 `BufferTooSmall` 或特定 `Status` 重试；`MalformedResponse` 表示当前内核
/// 与框架存在 ABI 不一致或内核回复已损坏。
pub enum Error {
    /// 查询管理命名空间或访问根 API 时发生的错误。
    Runtime(RuntimeApiError),
    /// 调用方输出或拼接输入缓冲区不足，携带所需最小字节数。
    BufferTooSmall(usize),
    /// ELM API 或 elm-mgr 返回非成功状态码。
    Status(i32),
    /// 回复的版本、尺寸、保留字段、标志、数量或载荷边界违反 v1 协议。
    MalformedResponse,
}

impl From<RuntimeApiError> for Error {
    fn from(value: RuntimeApiError) -> Self {
        Self::Runtime(value)
    }
}

#[derive(Clone, Copy)]
/// 受授权 Manager ELM 使用的类型化管理客户端。
///
/// `Client` 只保存由根 API 发布、与内核同寿命的只读 dispatch 表引用。它可以按值复制，
/// 但复制不会冻结权限或 generation；内核仍会在每次调用时根据当前上下文重新鉴权。
///
/// 该类型不能从裸地址公开构造，唯一入口是 [`Client::acquire`]。
pub struct Client {
    table: &'static ElmManagementApiV1,
}

impl Client {
    /// 为当前 Manager ELM 取得 v1 管理命名空间。
    ///
    /// 方法要求根表支持命名空间查询，并只请求 [`ELM_API_VERSION_V1`]。返回前会验证命名空间
    /// 结构尺寸、选择版本、表地址、表尺寸、generation、保留字段以及 management capability。
    ///
    /// # 错误
    ///
    /// 当前单元不是获授权 Manager、镜像不可信、根表不可用或命名空间不存在时返回相应
    /// `Runtime`/`Status` 错误；任何表布局不一致返回 [`Error::MalformedResponse`]。
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

    /// 从任意实现 EBI projection source 协议的数据源装载一个 ELM cell。
    ///
    /// `request` 描述 source kind、来源标志和尾随 payload 长度，`source_payload` 是该来源的
    /// 不透明请求数据；例如 image-session 引用并不等价于把镜像字节直接塞入请求。方法先验证
    /// 两处长度一致，再把固定头和尾随数据拼入 `input` 后提交 `LoadCell`。
    ///
    /// 装载仍会经过来源解析、证明链、ABI 指纹、依赖、策略、资源预算、原生镜像执行和
    /// `on_initialize` 事务。返回成功只表示整个事务已经提交。
    ///
    /// # 缓冲区
    ///
    /// `input` 至少需要 `size_of::<ElmEbiSourceRequest>() + source_payload.len()` 字节；不足时
    /// 返回包含所需尺寸的 [`Error::BufferTooSmall`]。
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

    /// 使用新的 EBI projection source 对现有 cell 执行强一致热替换。
    ///
    /// `request` 指定目标 cell、预期 generation、来源信息和迁移约束，`source_payload` 是来源
    /// 私有数据。客户端只负责安全拼接和协议校验；内核会执行影子装载、兼容性选择、调用排空、
    /// 状态迁移、generation 切换、import rebind、旧代终结和失败回滚。
    ///
    /// 请求中的 `source_payload_len` 必须与切片长度精确一致。`input` 容量不足时不会提交任何
    /// 替换步骤；运行时返回失败时应结合 replace trace 和 response 中的 rollback 状态诊断。
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

    /// 查询一个 provider 或 binding 暴露的契约特定快照。
    ///
    /// 回复载荷不是 elm-mgr 统一记录数组，而是 provider 自己定义的字节协议，因此返回
    /// [`ProviderSnapshot`]：通用 provider snapshot header 已验证，内部 payload 仍须由调用方
    /// 按 provider 契约解析。分页请求和下一游标由 `ElmProviderSnapshotRequest` 与回复 flags
    /// 表达。
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

    /// 查询全部补缀点和已附着 extension 的统一快照。
    ///
    /// header 分别给出 point 与 edge 数量，随后两类对象都编码为
    /// [`ElmExtensionSnapshotRecord`]。客户端使用 checked addition 合并数量，再验证记录区长度，
    /// 因而不会接受计数溢出、缺失记录或尾随字节。
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

    /// 向尚未封口的 image session 写入一段镜像字节。
    ///
    /// `request` 指定 session、偏移和 chunk 长度，`chunk` 是实际数据。方法要求两处长度完全
    /// 一致，并使用 `input` 拼接固定头和数据。运行时负责所有者、generation、session 状态、
    /// 区间重叠、总长度、TTL 和资源配额检查；写入成功不等于镜像已验证，仍需 seal。
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

/// 已完成边界校验的管理记录页。
///
/// `H` 是按值复制的页头，`R` 是每条固定记录的类型。记录字节继续借用调用方 output 缓冲区，
/// 并通过 [`RecordIter`] 使用非对齐读取按值产生记录，避免把任意 `u8` 缓冲区错误转换为对齐
/// slice。`RecordPage` 只能由 [`Client`] 的查询方法构造。
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

    /// 按值返回经过 ABI 版本和记录尺寸校验的页头。
    pub const fn header(&self) -> H {
        self.header
    }

    /// 返回按值遍历全部记录的精确长度迭代器。
    pub fn records(&self) -> RecordIter<'_, R> {
        RecordIter {
            bytes: self.records,
            index: 0,
            count: self.count,
            marker: PhantomData,
        }
    }

    /// 返回页中的记录数量。
    pub const fn len(&self) -> usize {
        self.count
    }

    /// 判断页中是否没有记录。
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// [`RecordPage`] 的按值记录迭代器。
///
/// 迭代器不会返回对 output 缓冲区的潜在未对齐引用。它实现 `ExactSizeIterator`，每次
/// `next` 都在已经验证的记录区内执行一次 `read_unaligned`。
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

/// 已验证通用头部、但保留契约私有 payload 的 provider 快照。
///
/// 该视图借用调用方 output 缓冲区。header 中的状态、flags、header size 和 payload length
/// 已经验证；`payload` 的内部结构由被查询 provider 的契约定义，elm-mgr 不替它解释。
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

    /// 返回已经过通用 snapshot 规则校验的头部。
    pub const fn header(&self) -> ElmProviderSnapshotHeader {
        self.header
    }

    /// 返回 provider 写入的契约私有有效载荷。
    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }
}

/// elm-mgr 模组菜单项快照页。
pub type MenuPage<'a> = RecordPage<'a, ElmMenuSnapshotHeader, ElmMenuItemSnapshot>;
/// cell 父子、依赖、binding 和 extension 关系拓扑页。
pub type TopologyPage<'a> = RecordPage<'a, ElmMgrTopologyHeader, ElmMgrRelationRecord>;
/// 管理授权与操作审计记录页。
pub type AuditPage<'a> = RecordPage<'a, ElmMgrAuditHeader, ElmMgrAuditRecord>;
/// 当前 Nexus 能力绑定快照页。
pub type NexusBindingPage<'a> =
    RecordPage<'a, ElmNexusBindingSnapshotHeader, ElmNexusBindingRecord>;
/// elm-mgr 内建运行时端口统计页。
pub type RuntimePortPage<'a> = RecordPage<'a, ElmRuntimePortStatsHeader, ElmRuntimePortStatsRecord>;
/// 已注册 provider 端口清单页。
pub type ProviderPortPage<'a> = RecordPage<'a, ElmProviderPortStatsHeader, ElmProviderPortRecord>;
/// provider 端口调用与故障统计页。
pub type ProviderStatsPage<'a> =
    RecordPage<'a, ElmProviderPortStatsHeader, ElmProviderPortStatsRecord>;
/// ELM 运行时各不变量检查结果页。
pub type HealthPage<'a> = RecordPage<'a, ElmCoreHealthHeader, ElmCoreHealthRecord>;
/// provider 异步队列容量、积压和过期统计页。
pub type ProviderQueuePage<'a> =
    RecordPage<'a, ElmProviderQueueStatsHeader, ElmProviderQueueStatsRecord>;
/// elm-mgr 已公开管理 API 描述符注册表页。
pub type ApiRegistryPage<'a> = RecordPage<'a, ElmMgrApiRegistryHeader, ElmMgrApiDescriptor>;
/// 当前事件订阅及其游标状态页。
pub type EventSubscriptionPage<'a> =
    RecordPage<'a, ElmMgrEventSubscriptionHeader, ElmMgrEventSubscriptionRecord>;
/// 某个订阅读取返回的事件记录页。
pub type SubscribedEventPage<'a> = RecordPage<'a, ElmMgrSubscribedEventReadHeader, ElmEventRecord>;
/// 当前原生 import/export 能力及授权信息页。
pub type NativeCapabilityPage<'a> =
    RecordPage<'a, ElmNativeCapabilityHeader, ElmNativeCapabilityRecord>;
/// 明确未实现且会阻断功能的运行时 TODO 注册表页。
pub type TodoRegistryPage<'a> = RecordPage<'a, ElmTodoRegistryHeader, ElmTodoRegistryRecord>;
/// 补缀点和 extension edge 的合并快照页。
pub type ExtensionPage<'a> = RecordPage<'a, ElmExtensionSnapshotHeader, ElmExtensionSnapshotRecord>;
/// 原生故障、恢复出口和调用现场摘要页。
pub type FaultDumpPage<'a> = RecordPage<'a, ElmFaultDumpHeader, ElmFaultDumpRecord>;
/// 生命周期、provider、mixin、替换、策略、资源或 journal 结构化追踪页。
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

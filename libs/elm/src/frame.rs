//! ELM 通用调用帧。
//!
//! 调用帧是 Nexus、provider 和受管 import/export 同步调用的固定布局边界。它只描述调用
//! 关联、操作编号、状态和最多 256 字节的契约载荷，不描述底层文件格式，也不暴露内核指针。
//!
//! `binding_id` 与 `call_id` 必须在回复中原样返回，运行时依靠它们检测错误路由和陈旧回复。
//! 所有保留字段必须为零，`payload_len` 不得超过固定数组容量。模块业务代码应优先使用
//! [`ProviderRequest`](crate::ProviderRequest)、[`ManagedImport`](crate::ManagedImport) 和
//! [`ProviderReply`](crate::ProviderReply)，而不是直接操作原生 frame。

/// 通用请求和回复帧可携带的最大载荷字节数。
pub const ELM_FRAME_PAYLOAD_LEN: usize = 256;
/// [`ElmNativeEntryFrameV1`] 的 ABI 版本。
pub const ELM_NATIVE_ENTRY_ABI_VERSION: u16 = 1;
/// [`ElmNativeProviderCallV1`] 的 ABI 版本。
pub const ELM_NATIVE_PROVIDER_CALL_ABI_VERSION: u16 = 1;
/// [`ElmNativeManagedCallV1`] 的 ABI 版本。
pub const ELM_NATIVE_MANAGED_CALL_ABI_VERSION: u16 = 1;
/// [`ElmNativeProviderSnapshotV1`] 的 ABI 版本。
pub const ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION: u16 = 1;
/// 快照请求启用分页；此时 `reserved2` 在线格式中承载当前或下一游标。
pub const ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED: u16 = 1 << 0;
/// 快照回复仍有后续页面，且返回了有效下一游标。
pub const ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE: u16 = 1 << 1;
/// `ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK` 定义当前版本认可的全部标志位；输入包含掩码外位时必须拒绝或按调用契约报错。
pub const ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK: u16 =
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED | ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE;

/// v1 通用调用未设置任何可选标志。
pub const ELM_CALL_FLAG_NONE: u32 = 0;

/// elm-mgr action provider 的“执行动作”操作码。
pub const ELM_ACTION_OPCODE_INVOKE: u32 = 1;
/// action 回复携带健康检查结果。
pub const ELM_ACTION_RESULT_HEALTH: u32 = 1;

/// 调用成功。
pub const ELM_CALL_STATUS_OK: i32 = 0;
/// 目标、操作码或对象不存在。
pub const ELM_CALL_STATUS_NOT_FOUND: i32 = -2;
/// 目标正被调用、排空、替换或租约保护，当前不能完成操作。
pub const ELM_CALL_STATUS_BUSY: i32 = -16;
/// 请求布局、参数、状态或载荷无效。
pub const ELM_CALL_STATUS_INVALID: i32 = -22;
/// 目标不实现该操作或当前 ABI 不支持该能力。
pub const ELM_CALL_STATUS_UNSUPPORTED: i32 = -95;
/// provider 原生执行发生故障，或调用尚未写入有效回复时使用的故障默认值。
pub const ELM_CALL_STATUS_PROVIDER_FAULT: i32 = -4098;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 调用 elm-mgr 菜单 action provider 的固定请求。
pub struct ElmActionInvokeRequest {
    /// 要执行的 action id，必须来自当前菜单或 API 注册表快照。
    pub action_id: u64,
    /// v1 必须为 [`ELM_CALL_FLAG_NONE`]。
    pub flags: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmActionInvokeRequest {
    /// 构造零标志和零保留字段的 action 请求。
    pub const fn new(action_id: u64) -> Self {
        Self {
            action_id,
            flags: ELM_CALL_FLAG_NONE,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// action provider 返回的固定结果摘要。
pub struct ElmActionInvokeReply {
    /// `action_id` 所指对象的稳定运行时标识符。
    pub action_id: u64,
    /// 触发该 action 的菜单项 id。
    pub menu_item_id: u64,
    /// 拥有该对象或资源的 ELM 单元标识符。
    pub owner_cell_id: u64,
    /// 结果载荷类别，当前可为 [`ELM_ACTION_RESULT_HEALTH`]。
    pub result_kind: u32,
    /// action 自身的结果状态码。
    pub result_code: i32,
    /// 运行时当前事件序列，用于建立读取游标。
    pub event_sequence: u64,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u64,
}

impl ElmActionInvokeReply {
    /// 构造健康检查 action 的规范回复。
    pub const fn health(
        action_id: u64,
        menu_item_id: u64,
        owner_cell_id: u64,
        result_code: i32,
        event_sequence: u64,
    ) -> Self {
        Self {
            action_id,
            menu_item_id,
            owner_cell_id,
            result_kind: ELM_ACTION_RESULT_HEALTH,
            result_code,
            event_sequence,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// ELM 同步调用的固定请求帧。
///
/// `opcode` 和 payload 由绑定契约解释。`call_id` 应在调用方作用域内保持唯一且非零；受管
/// import 包装会自动生成。结构可以按值跨 ABI 复制，不包含引用或宿主字长字段。
pub struct ElmCallFrame {
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// 一次调用的关联标识符，回复必须原样返回。
    pub call_id: u64,
    /// 契约内部的操作编号。
    pub opcode: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 有效载荷的实际字节数；不得超过相邻载荷缓冲区容量。
    pub payload_len: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u32,
    /// 固定容量的线格式载荷缓冲区；仅前 `payload_len` 字节有效。
    pub payload: [u8; ELM_FRAME_PAYLOAD_LEN],
}

impl ElmCallFrame {
    /// 构造零标志、零保留字段且不携带载荷的请求帧。
    pub const fn empty(binding_id: u64, call_id: u64, opcode: u32) -> Self {
        Self {
            binding_id,
            call_id,
            opcode,
            flags: ELM_CALL_FLAG_NONE,
            payload_len: 0,
            reserved0: 0,
            reserved1: 0,
            payload: [0; ELM_FRAME_PAYLOAD_LEN],
        }
    }

    /// 从原始载荷构造请求帧。
    ///
    /// 当前低层构造器会把超过 256 字节的输入截断到固定容量。需要禁止截断的业务代码应先
    /// 检查长度，或使用 [`ManagedImport::call_bytes`](crate::ManagedImport::call_bytes) 和
    /// [`ProviderReply::bytes`](crate::ProviderReply::bytes) 等返回错误的安全包装。
    pub fn new(binding_id: u64, call_id: u64, opcode: u32, payload: &[u8]) -> Self {
        let mut out = Self::empty(binding_id, call_id, opcode);
        let n = payload.len().min(ELM_FRAME_PAYLOAD_LEN);
        out.payload[..n].copy_from_slice(&payload[..n]);
        out.payload_len = n as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// ELM 同步调用的固定回复帧。
///
/// 回复必须复制请求的 `binding_id` 和 `call_id`。状态为零只说明传输和业务处理成功，payload
/// 的具体类型仍由操作契约决定。
pub struct ElmReplyFrame {
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// 一次调用的关联标识符，回复必须原样返回。
    pub call_id: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 有效载荷的实际字节数；不得超过相邻载荷缓冲区容量。
    pub payload_len: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u32,
    /// 固定容量的线格式载荷缓冲区；仅前 `payload_len` 字节有效。
    pub payload: [u8; ELM_FRAME_PAYLOAD_LEN],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 内核调用原生 provider trampoline 使用的 ABI v1 frame。
///
/// `request` 由内核填写，`reply` 预置为 provider fault。trampoline 校验版本、标志、保留字段
/// 和 binding 关联后调用安全 Rust handler，并只在成功构造回复后覆盖 `reply`。
pub struct ElmNativeProviderCallV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u32,
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// 保护对应调用或资源生命周期的租约标识符。
    pub lease_id: u64,
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// 待处理请求帧。
    pub request: ElmCallFrame,
    /// handler 写回的回复帧，进入 trampoline 前为故障默认值。
    pub reply: ElmReplyFrame,
}

/// 受管 import/export 的固定原生调用帧。
///
/// import 槽只保存 `import_handle`；实际目标、代际和权限由运行时调用门解析。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNativeManagedCallV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u32,
    /// 运行时解析并授权的受管 import handle。
    pub import_handle: u64,
    /// 发起调用的 ELM 单元标识符。
    pub caller_cell_id: u64,
    /// 调用方代际，用于在分发前检测陈旧调用。
    pub caller_generation: u64,
    /// 接收调用的 ELM 单元标识符。
    pub callee_cell_id: u64,
    /// 被调用方代际，用于将调用路由到正确的热替换版本。
    pub callee_generation: u64,
    /// 发送给 export 的请求帧。
    pub request: ElmCallFrame,
    /// export handler 写回的回复帧。
    pub reply: ElmReplyFrame,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 内核调用原生 `#[elm::entry]` trampoline 使用的 ABI v1 frame。
pub struct ElmNativeEntryFrameV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u32,
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 父 ELM 单元标识符；零通常表示没有父单元。
    pub parent_id: u64,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: u64,
    /// 对象或单元的当前状态编码。
    pub state: u32,
    /// entry 返回状态；成功为零，失败为 `HookError` 状态码。
    pub exit_code: i32,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u64,
}

impl ElmNativeEntryFrameV1 {
    /// 构造版本、标志、保留字段和退出码均规范化的 entry frame。
    pub const fn new(cell_id: u64, parent_id: u64, generation: u64, state: u32) -> Self {
        Self {
            abi_version: ELM_NATIVE_ENTRY_ABI_VERSION,
            flags: 0,
            reserved0: 0,
            cell_id,
            parent_id,
            generation,
            state,
            exit_code: 0,
            reserved1: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 内核调用原生 provider snapshot trampoline 使用的 ABI v1 frame。
///
/// `payload_addr` 和 `capacity` 描述调用期输出缓冲区。handler 只能写入该范围，trampoline
/// 会验证返回长度、记录数和分页游标，再更新 `status`、`payload_len` 与 flags。
pub struct ElmNativeProviderSnapshotV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 请求/回复分页标志，只允许 `PAGED` 和 `MORE`。
    pub flags: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u32,
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// Nexus 或 provider 端口的运行时标识符。
    pub port_id: u64,
    /// 能力绑定的运行时标识符。
    pub binding_id: u64,
    /// 保护对应调用或资源生命周期的租约标识符。
    pub lease_id: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 第二保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved1: u32,
    /// `payload_addr` 指向缓冲区的总容量，单位为字节。
    pub capacity: u32,
    /// 有效载荷的实际字节数；不得超过相邻载荷缓冲区容量。
    pub payload_len: u32,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 非分页时必须为零；分页请求中是当前游标，分页回复中是下一游标。
    pub reserved2: u32,
    /// 调用方提供的输出缓冲区地址；只在本次 snapshot 调用期间有效。
    pub payload_addr: u64,
}

impl ElmNativeProviderSnapshotV1 {
    /// 构造未分页、空载荷且状态预置为 provider fault 的 snapshot frame。
    pub const fn new(
        cell_id: u64,
        port_id: u64,
        binding_id: u64,
        lease_id: u64,
        payload_addr: u64,
        capacity: u32,
    ) -> Self {
        Self {
            abi_version: ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION,
            flags: 0,
            reserved0: 0,
            cell_id,
            port_id,
            binding_id,
            lease_id,
            status: ELM_CALL_STATUS_PROVIDER_FAULT,
            reserved1: 0,
            capacity,
            payload_len: 0,
            record_count: 0,
            reserved2: 0,
            payload_addr,
        }
    }
}

impl ElmNativeProviderCallV1 {
    /// 从已分配的 cell、port、lease 和请求构造 provider 调用 frame。
    ///
    /// `binding_id` 自动复制自请求，回复预置为同一 binding/call id 的 provider fault。
    pub const fn new(cell_id: u64, port_id: u64, lease_id: u64, request: ElmCallFrame) -> Self {
        Self {
            abi_version: ELM_NATIVE_PROVIDER_CALL_ABI_VERSION,
            flags: 0,
            reserved0: 0,
            cell_id,
            port_id,
            lease_id,
            binding_id: request.binding_id,
            request,
            reply: ElmReplyFrame::empty(
                request.binding_id,
                request.call_id,
                ELM_CALL_STATUS_PROVIDER_FAULT,
            ),
        }
    }
}

impl ElmNativeManagedCallV1 {
    /// 构造带完整调用方/被调用方 generation 的受管 export 调用 frame。
    ///
    /// 回复预置为 provider fault；只有 export trampoline 成功后才会覆盖。
    pub const fn new(
        import_handle: u64,
        caller_cell_id: u64,
        caller_generation: u64,
        callee_cell_id: u64,
        callee_generation: u64,
        request: ElmCallFrame,
    ) -> Self {
        Self {
            abi_version: ELM_NATIVE_MANAGED_CALL_ABI_VERSION,
            flags: 0,
            reserved0: 0,
            import_handle,
            caller_cell_id,
            caller_generation,
            callee_cell_id,
            callee_generation,
            request,
            reply: ElmReplyFrame::empty(
                request.binding_id,
                request.call_id,
                ELM_CALL_STATUS_PROVIDER_FAULT,
            ),
        }
    }
}

impl ElmReplyFrame {
    /// 构造零标志、零保留字段且不携带载荷的回复帧。
    pub const fn empty(binding_id: u64, call_id: u64, status: i32) -> Self {
        Self {
            binding_id,
            call_id,
            status,
            flags: ELM_CALL_FLAG_NONE,
            payload_len: 0,
            reserved0: 0,
            reserved1: 0,
            payload: [0; ELM_FRAME_PAYLOAD_LEN],
        }
    }

    /// 从原始载荷构造回复帧。
    ///
    /// 与 [`ElmCallFrame::new`] 相同，低层构造器会把超长输入截断到 256 字节。模块业务代码
    /// 应优先使用返回 [`PayloadError`](crate::PayloadError) 的 [`ProviderReply`](crate::ProviderReply)。
    pub fn new(binding_id: u64, call_id: u64, status: i32, payload: &[u8]) -> Self {
        let mut out = Self::empty(binding_id, call_id, status);
        let n = payload.len().min(ELM_FRAME_PAYLOAD_LEN);
        out.payload[..n].copy_from_slice(&payload[..n]);
        out.payload_len = n as u16;
        out
    }
}

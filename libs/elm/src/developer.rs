//! Rust ELM 开发侧安全边界。
//!
//! 本模块把 EBI v1 的裸函数指针、原始地址和固定布局帧收敛到少量内部调用门。
//! ELM 业务代码只处理借用、结果类型和显式固定线编码载荷。这里的公开项会在 crate 根
//! 重导出；模块作者通常直接使用 `elm::LifecycleContext`、`elm::ManagedImport` 等路径。
//!
//! attribute 生成的 trampoline 是唯一应接触原生 ABI frame 的代码。业务函数不得保存
//! 请求借用、迁移缓冲区、当前上下文或原始回复帧内部地址，也不得让 panic 穿过 trampoline。
//! 跨 ELM 的长期关系应使用受管 import、binding、lease 和运行时登记资源表达。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::context::{
    ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION, ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION,
    ElmNativeHookContextV1, ElmNativeMigrationContextV1,
};
use crate::elmapi::{
    ELM_API_ABORT_REASON_PANIC, ELM_API_ROOT_MAGIC, ELM_API_STATUS_BUFFER_TOO_SMALL,
    ELM_API_VERSION_V1, ElmApiContextV1, ElmApiNamespaceV1, ElmApiRootV1, ElmRuntimeApiV1,
};
use crate::frame::{
    ELM_CALL_STATUS_INVALID, ELM_CALL_STATUS_OK, ELM_CALL_STATUS_PROVIDER_FAULT,
    ELM_FRAME_PAYLOAD_LEN, ELM_NATIVE_ENTRY_ABI_VERSION, ELM_NATIVE_MANAGED_CALL_ABI_VERSION,
    ELM_NATIVE_PROVIDER_CALL_ABI_VERSION, ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE, ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK, ElmCallFrame, ElmNativeEntryFrameV1,
    ElmNativeManagedCallV1, ElmNativeProviderCallV1, ElmNativeProviderSnapshotV1, ElmReplyFrame,
};
use crate::module_wire::{
    MGR_EXTENSION_DISPATCH_RESPONSE_SIZE, MGR_EXTENSION_PAYLOAD_LEN, MGR_RESPONSE_HEADER_SIZE,
    MGR_STATUS_OK, MIXIN_REPLY_CONTINUE, MIXIN_REPLY_DENY, MIXIN_REPLY_REPLACE, MIXIN_REPLY_STOP,
    ModuleExtensionDispatchRequest, ModuleExtensionDispatchResponse, ModuleMgrResponseHeader,
};

/// 装载器注入 ELM 根 API 表地址时使用的固定导入槽符号。
///
/// 每个由 Rust 框架构建的原生 ELM 都包含此槽。打包器把它投影为受运行时管理的特殊重定位，
/// 装载器在执行任何模块代码前写入 [`ElmApiRootV1`] 地址。模块不得自行定义同名符号。
pub const ELM_API_ROOT_SLOT_SYMBOL: &str = "__elm_api_root_slot_v1";
/// 启用 mixin 的 ingress 阶段，即原始函数执行前的输入补缀。
pub const ELM_MIXIN_STAGE_INGRESS: u32 = 1 << 0;
/// 启用 mixin 的 substitute 阶段，该阶段可替换帧并跳过原始函数。
pub const ELM_MIXIN_STAGE_SUBSTITUTE: u32 = 1 << 1;
/// 启用 mixin 的 egress 阶段，即原始函数或替代逻辑完成后的输出补缀。
pub const ELM_MIXIN_STAGE_EGRESS: u32 = 1 << 2;
/// 启用 mixin 的 observe 阶段；该阶段最后执行，主要用于只读观察和审计。
pub const ELM_MIXIN_STAGE_OBSERVE: u32 = 1 << 3;
/// 当前 ABI 支持的全部 mixin 阶段位集合。
pub const ELM_MIXIN_STAGES_ALL: u32 = ELM_MIXIN_STAGE_INGRESS
    | ELM_MIXIN_STAGE_SUBSTITUTE
    | ELM_MIXIN_STAGE_EGRESS
    | ELM_MIXIN_STAGE_OBSERVE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 生命周期、entry、provider 和补缀点业务函数返回的稳定错误。
///
/// 错误只携带一个非零状态码，以便 trampoline 无需分配即可把失败传播给运行时。状态码的
/// 具体命名空间由调用契约决定；框架保留 `ELM_CALL_STATUS_*` 作为通用调用错误。
pub struct HookError {
    status: i32,
}

impl HookError {
    /// 从状态码构造错误。
    ///
    /// 零表示成功，不能用于错误；传入零时会归一化为
    /// [`ELM_CALL_STATUS_INVALID`](crate::ELM_CALL_STATUS_INVALID)，从而保证 `HookError`
    /// 永远表示失败。
    pub const fn new(status: i32) -> Self {
        Self {
            status: if status == 0 {
                ELM_CALL_STATUS_INVALID
            } else {
                status
            },
        }
    }

    /// 返回将由 ABI trampoline 传播给运行时的非零状态码。
    pub const fn status(self) -> i32 {
        self.status
    }
}

/// 生命周期钩子使用的结果类型；成功不携带载荷，失败携带稳定状态码。
pub type HookResult = Result<(), HookError>;
/// 设备 IRQ 业务回调使用的结果类型；成功值表示本处理器是否消费了该中断。
pub type DeviceIrqResult = Result<bool, HookError>;
/// [`entry`](crate::entry) 业务函数使用的结果类型。
pub type EntryResult = HookResult;
/// [`mixin_point`](crate::mixin_point) 原始函数使用的结果类型。
pub type PointResult = HookResult;
/// 迁移状态导出钩子的结果类型；成功值是实际写入迁移缓冲区的字节数。
pub type MigrationExportResult = Result<usize, HookError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 固定 ELM 载荷编码或解码失败。
pub enum PayloadError {
    /// 调用方提供的输出缓冲区小于 [`ElmPayload::WIRE_SIZE`]。
    BufferTooSmall,
    /// 输入长度不等于该契约的固定线格式尺寸，或编码器写入长度不一致。
    SizeMismatch,
    /// `bool` 字段在线格式中的字节既不是 0 也不是 1。
    InvalidBoolean,
}

/// 可跨 provider、受管 import/export 或 mixin 边界传输的固定线格式载荷。
///
/// 实现必须与 Rust 内存布局无关，并对同一值产生确定的小端字节串。推荐始终通过
/// [`payload`](crate::payload) 派生；手工实现者必须保证 `WIRE_SIZE` 固定、`encode` 恰好
/// 写入该长度、`decode` 拒绝任何其他长度，并验证所有受限字段。
///
/// # 示例
///
/// ```
/// use elm::ElmPayload;
///
/// #[elm::payload("example.counter@1")]
/// #[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// struct Counter {
///     value: u32,
///     enabled: bool,
/// }
///
/// let value = Counter { value: 0x1122_3344, enabled: true };
/// let mut bytes = [0_u8; Counter::WIRE_SIZE];
/// assert_eq!(value.encode(&mut bytes), Ok(5));
/// assert_eq!(bytes, [0x44, 0x33, 0x22, 0x11, 1]);
/// assert_eq!(Counter::decode(&bytes), Ok(value));
/// ```
pub trait ElmPayload: Sized {
    /// 载荷的完整 `identifier@version` 契约。
    ///
    /// 绑定和 mixin 分发必须按完整字节串匹配该值，不能只比较哈希或 Rust 类型名。
    const CONTRACT: &'static str;
    /// 该载荷在线格式中的精确字节数。
    const WIRE_SIZE: usize;

    /// 按稳定线格式编码到 `output`，成功时返回写入字节数。
    ///
    /// 输出容量不足时返回 [`PayloadError::BufferTooSmall`]；成功长度必须等于 `WIRE_SIZE`。
    fn encode(&self, output: &mut [u8]) -> Result<usize, PayloadError>;
    /// 从完整固定载荷解码一个值。
    ///
    /// 实现必须拒绝长度不等于 `WIRE_SIZE` 的输入和任何非规范编码。
    fn decode(input: &[u8]) -> Result<Self, PayloadError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 生命周期钩子看到的当前单元只读上下文。
///
/// 该类型从原生 ABI frame 复制稳定标量，不暴露内核指针。值只描述本次钩子调用，不能作为
/// 后续操作的授权凭据；运行时会在每个调用边界重新校验 generation、状态和策略。
pub struct LifecycleContext {
    cell_id: u64,
    parent_id: u64,
    generation: u64,
    state: u32,
    phase: u16,
    flags: u32,
}

impl LifecycleContext {
    const fn from_raw(raw: ElmNativeHookContextV1) -> Self {
        Self {
            cell_id: raw.cell_id,
            parent_id: raw.parent_id,
            generation: raw.generation,
            state: raw.state,
            phase: raw.phase,
            flags: raw.flags,
        }
    }

    /// 返回正在执行生命周期钩子的 cell id。
    pub const fn cell_id(self) -> u64 {
        self.cell_id
    }

    /// 返回父 ELM 的 cell id；根单元的原始值为零并映射为 `None`。
    pub const fn parent_id(self) -> Option<u64> {
        if self.parent_id == 0 {
            None
        } else {
            Some(self.parent_id)
        }
    }

    /// 返回当前 cell generation。
    ///
    /// 热替换提交后旧 generation 立即陈旧，不得把此值缓存为永久身份。
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// 返回进入钩子时的 [`ElmState`](crate::ElmState) 原始编码。
    pub const fn state(self) -> u32 {
        self.state
    }

    /// 返回当前 [`ElmLifecyclePhase`](crate::ElmLifecyclePhase) 的原始编码。
    pub const fn phase(self) -> u16 {
        self.phase
    }

    /// 返回本次生命周期调用的附加标志；v1 未定义的位必须由 trampoline 拒绝。
    pub const fn flags(self) -> u32 {
        self.flags
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 热替换迁移钩子看到的代际上下文。
///
/// 同一个替换事务会把等价的上下文传给旧代导出、新代导入和新代回滚钩子。迁移缓冲区不在
/// 此结构中暴露，而是作为受调用期约束的切片单独传给业务函数。
pub struct MigrationContext {
    cell_id: u64,
    old_generation: u64,
    new_generation: u64,
    phase: u16,
}

impl MigrationContext {
    const fn from_raw(raw: &ElmNativeMigrationContextV1) -> Self {
        Self {
            cell_id: raw.cell_id,
            old_generation: raw.old_generation,
            new_generation: raw.new_generation,
            phase: raw.phase,
        }
    }

    /// 返回被替换逻辑单元的稳定 cell id。
    pub const fn cell_id(self) -> u64 {
        self.cell_id
    }

    /// 返回替换事务开始时对外服务的旧 generation。
    pub const fn old_generation(self) -> u64 {
        self.old_generation
    }

    /// 返回影子装载的新 generation；仅在提交成功后才成为公开 generation。
    pub const fn new_generation(self) -> u64 {
        self.new_generation
    }

    /// 返回当前迁移阶段的原始编码，用于区分 export、import 和 abort。
    pub const fn phase(self) -> u16 {
        self.phase
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 可选 entry 函数在单元激活后收到的只读上下文。
///
/// entry 在初始化和声明式拓扑激活完成后执行。该上下文不包含生命周期 phase，因为 entry
/// 不是生命周期提交钩子。
pub struct EntryContext {
    cell_id: u64,
    parent_id: u64,
    generation: u64,
    state: u32,
}

impl EntryContext {
    const fn from_raw(raw: ElmNativeEntryFrameV1) -> Self {
        Self {
            cell_id: raw.cell_id,
            parent_id: raw.parent_id,
            generation: raw.generation,
            state: raw.state,
        }
    }

    /// 返回执行 entry 的 cell id。
    pub const fn cell_id(self) -> u64 {
        self.cell_id
    }

    /// 返回父 ELM 的 cell id；根单元返回 `None`。
    pub const fn parent_id(self) -> Option<u64> {
        if self.parent_id == 0 {
            None
        } else {
            Some(self.parent_id)
        }
    }

    /// 返回 entry 所属的当前 generation。
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// 返回调用 entry 时的 [`ElmState`](crate::ElmState) 原始编码，通常为 `Active`。
    pub const fn state(self) -> u32 {
        self.state
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `#[elm::provider]` 业务函数收到的已验证请求视图。
///
/// trampoline 已经检查原生 frame 的 ABI 版本、保留字段、binding 关联和载荷边界。此结构
/// 按值复制固定调用帧，业务代码仍不应把 id 当作对象指针，也不能绕过 lease 去长期使用对应
/// 内核资源。
pub struct ProviderRequest {
    /// 实现该 provider 的 cell id。
    pub cell_id: u64,
    /// 本次调用命中的 provider port id。
    pub port_id: u64,
    /// 覆盖本次调用生命周期的 lease id。
    pub lease_id: u64,
    /// 请求的通用固定调用帧，包含 binding、call id、opcode、flags 和载荷。
    pub frame: ElmCallFrame,
}

impl ProviderRequest {
    /// 返回调用帧中前 `payload_len` 字节的有效载荷。
    pub fn payload(&self) -> &[u8] {
        &self.frame.payload[..usize::from(self.frame.payload_len)]
    }

    /// 使用 `T` 的固定载荷契约解码请求。
    ///
    /// 此方法只负责线格式校验；provider 实现仍应确认端口契约和 opcode 是否允许该类型。
    pub fn decode<T: ElmPayload>(&self) -> Result<T, PayloadError> {
        T::decode(self.payload())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `#[elm::export]` 业务函数收到的已验证受管调用。
///
/// 运行时已经完成 import handle 解析、作用域授权、版本选择和 generation 路由。调用方与被
/// 调用方信息用于审计和细粒度策略，不代表业务函数可以访问对应 cell 的内部内存。
pub struct ManagedRequest {
    /// 解析到本 export 的受管 import handle。
    pub import_handle: u64,
    /// 发起调用的 ELM 单元标识符。
    pub caller_cell_id: u64,
    /// 调用方代际，用于在分发前检测陈旧调用。
    pub caller_generation: u64,
    /// 接收调用的 ELM 单元标识符。
    pub callee_cell_id: u64,
    /// 被调用方代际，用于将调用路由到正确的热替换版本。
    pub callee_generation: u64,
    /// 请求的通用固定调用帧。
    pub frame: ElmCallFrame,
}

impl ManagedRequest {
    /// 返回调用帧中前 `payload_len` 字节的有效载荷。
    pub fn payload(&self) -> &[u8] {
        &self.frame.payload[..usize::from(self.frame.payload_len)]
    }

    /// 使用 `T` 的固定载荷契约解码请求。
    pub fn decode<T: ElmPayload>(&self) -> Result<T, PayloadError> {
        T::decode(self.payload())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// provider、受管 export 和 mixin trampoline 使用的安全回复构造器。
///
/// 该类型隐藏固定容量数组和长度维护，避免业务代码构造 `payload_len` 越界的
/// [`ElmReplyFrame`]。普通 provider 与 export 应返回零状态表示成功；mixin trampoline 会
/// 额外设置控制标志。
pub struct ProviderReply {
    status: i32,
    flags: u32,
    payload_len: u16,
    payload: [u8; ELM_FRAME_PAYLOAD_LEN],
}

impl ProviderReply {
    /// 构造指定状态且不携带载荷的回复。
    pub const fn empty(status: i32) -> Self {
        Self {
            status,
            flags: 0,
            payload_len: 0,
            payload: [0; ELM_FRAME_PAYLOAD_LEN],
        }
    }

    /// 构造状态为 [`ELM_CALL_STATUS_OK`](crate::ELM_CALL_STATUS_OK) 的空成功回复。
    pub const fn ok() -> Self {
        Self::empty(ELM_CALL_STATUS_OK)
    }

    /// 从原始字节构造回复。
    ///
    /// `payload` 超过 [`ELM_FRAME_PAYLOAD_LEN`](crate::ELM_FRAME_PAYLOAD_LEN) 时返回
    /// [`PayloadError::BufferTooSmall`]，不会截断数据。
    pub fn bytes(status: i32, payload: &[u8]) -> Result<Self, PayloadError> {
        if payload.len() > ELM_FRAME_PAYLOAD_LEN {
            return Err(PayloadError::BufferTooSmall);
        }
        let mut reply = Self::empty(status);
        reply.payload[..payload.len()].copy_from_slice(payload);
        reply.payload_len = payload.len() as u16;
        Ok(reply)
    }

    /// 编码类型化载荷并构造回复。
    ///
    /// `T::WIRE_SIZE` 必须能放入固定回复帧；编码器返回的实际长度会成为 `payload_len`。
    pub fn payload<T: ElmPayload>(status: i32, payload: &T) -> Result<Self, PayloadError> {
        if T::WIRE_SIZE > ELM_FRAME_PAYLOAD_LEN {
            return Err(PayloadError::BufferTooSmall);
        }
        let mut reply = Self::empty(status);
        let len = payload.encode(&mut reply.payload)?;
        reply.payload_len = len as u16;
        Ok(reply)
    }

    /// 设置协议回复标志并返回更新后的构造器值。
    ///
    /// 普通业务代码不应随意设置未知位。当前主要由 mixin trampoline 写入
    /// `CONTINUE`、`STOP`、`REPLACE` 或 `DENY` 控制标志。
    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    fn into_frame(self, binding_id: u64, call_id: u64) -> ElmReplyFrame {
        let mut frame = ElmReplyFrame::empty(binding_id, call_id, self.status);
        frame.flags = self.flags;
        frame.payload_len = self.payload_len;
        frame.payload = self.payload;
        frame
    }
}

/// provider 处理函数的规范结果类型。
pub type ProviderResult = Result<ProviderReply, HookError>;
/// 受管 export 处理函数的规范结果类型。
pub type ManagedResult = ProviderResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// [`ManagedImport`] 调用返回的已验证回复包装。
///
/// 构造阶段已经验证保留字段和载荷边界，并核对底层 reply 的 binding/call id。业务代码可先
/// 检查状态，再按契约读取字节或解码固定载荷。
pub struct ManagedReply {
    frame: ElmReplyFrame,
}

impl ManagedReply {
    fn from_frame(frame: ElmReplyFrame) -> Result<Self, RuntimeApiError> {
        if frame.reserved0 != 0
            || frame.reserved1 != 0
            || usize::from(frame.payload_len) > frame.payload.len()
        {
            return Err(RuntimeApiError::MalformedResponse);
        }
        Ok(Self { frame })
    }

    /// 返回被调用 export 写入的业务状态码。
    pub const fn status(self) -> i32 {
        self.frame.status
    }

    /// 返回回复标志；调用契约未定义的位应视为不兼容。
    pub const fn flags(self) -> u32 {
        self.frame.flags
    }

    /// 返回回复中前 `payload_len` 字节的有效载荷。
    pub fn payload(&self) -> &[u8] {
        &self.frame.payload[..usize::from(self.frame.payload_len)]
    }

    /// 使用 `T` 的固定载荷契约解码回复。
    pub fn decode<T: ElmPayload>(&self) -> Result<T, RuntimeApiError> {
        T::decode(self.payload()).map_err(RuntimeApiError::Payload)
    }

    /// 消费安全包装并取得底层固定布局回复帧。
    ///
    /// 只有需要转交给其他框架 API 时才应使用此方法；普通业务代码优先使用 `status`、
    /// `payload` 和 `decode`。
    pub const fn into_frame(self) -> ElmReplyFrame {
        self.frame
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `#[elm::provider_snapshot]` 业务函数收到的快照请求。
///
/// 快照调用由运行时用 lease 保护。分页请求的 `cursor` 只对同一 provider、binding 和快照
/// 契约有意义；实现不得把它解释为可直接解引用的地址。
pub struct SnapshotRequest {
    /// 实现该快照入口的 cell id。
    pub cell_id: u64,
    /// 被查询的 provider port id。
    pub port_id: u64,
    /// 发起快照查询的 binding id。
    pub binding_id: u64,
    /// 覆盖本次快照生成过程的 lease id。
    pub lease_id: u64,
    /// 是否请求分页快照。
    pub paged: bool,
    /// 当前分页游标；非分页请求恒为零。
    pub cursor: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// provider 快照函数对输出缓冲区的描述。
///
/// 实际字节由处理函数写入 trampoline 提供的切片，本结构只报告状态、有效前缀、记录数量和
/// 分页游标。`payload_len` 不能大于输出容量；存在下一页时 `next_cursor` 必须非零且不同于
/// 请求游标。
pub struct SnapshotReply {
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 有效载荷的实际字节数；不得超过相邻载荷缓冲区容量。
    pub payload_len: usize,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 下一页游标；`None` 表示本次回复已经完整结束。
    pub next_cursor: Option<u32>,
}

impl SnapshotReply {
    /// 构造不再有后续页面的成功回复。
    pub const fn complete(payload_len: usize, record_count: u32) -> Self {
        Self {
            status: MGR_STATUS_OK,
            payload_len,
            record_count,
            next_cursor: None,
        }
    }

    /// 构造仍有后续页面的成功回复。
    ///
    /// 调用方必须确保当前请求启用了分页，且 `next_cursor` 非零并向前推进。
    pub const fn more(payload_len: usize, record_count: u32, next_cursor: u32) -> Self {
        Self {
            status: MGR_STATUS_OK,
            payload_len,
            record_count,
            next_cursor: Some(next_cursor),
        }
    }

    /// 构造不携带载荷和记录的失败回复。
    pub const fn error(status: i32) -> Self {
        Self {
            status,
            payload_len: 0,
            record_count: 0,
            next_cursor: None,
        }
    }
}

/// provider 快照处理函数的规范结果类型。
pub type SnapshotResult = Result<SnapshotReply, HookError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// mixin 处理器对当前补缀阶段的控制结果。
pub enum MixinControl {
    /// 不替换帧，继续执行当前阶段的后续处理器。
    Continue,
    /// 不替换帧，但停止当前阶段的后续处理器。
    Stop,
    /// 用处理器修改后的帧替换当前帧，然后继续当前阶段。
    Replace,
    /// 替换当前帧并停止当前阶段的后续处理器。
    ReplaceAndStop,
    /// 拒绝整个补缀点调用；包装函数返回失败且不再执行原函数或后续阶段。
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 一个分阶段 mixin 补缀点的静态描述。
///
/// 此类型主要由 [`mixin_point`](crate::mixin_point) 展开代码构造。每个非空点名已经包含阶段
/// 后缀，例如 `scheduler.select.ingress`；`None` 表示该阶段未启用。
pub struct MixinPointDescriptor {
    /// 各阶段共享的固定载荷契约。
    pub contract: &'static str,
    /// 原函数执行前的 ingress 点名。
    pub ingress: Option<&'static str>,
    /// 可替代原函数的 substitute 点名。
    pub substitute: Option<&'static str>,
    /// 原函数或替代逻辑完成后的 egress 点名。
    pub egress: Option<&'static str>,
    /// 最后执行的 observe 点名。
    pub observe: Option<&'static str>,
}

#[repr(transparent)]
#[derive(Debug)]
/// 由装载器写入受管句柄的安全 import 槽。
///
/// 该类型必须作为不可变 `static` 并由 [`import`](crate::import) 标记。装载器只写入不透明
/// handle；每次调用都回到运行时执行代际路由、授权、并发和回复关联校验，因此它是支持热替换
/// 的默认跨 ELM 调用方式。
///
/// # 示例
///
/// ```no_run
/// use elm::ManagedImport;
///
/// #[elm::import(
///     name = "example.echo",
///     contract = "example.echo@1",
///     version = 1,
///     optional = true
/// )]
/// static ECHO: ManagedImport = ManagedImport::new();
///
/// let reply = ECHO.call_bytes(1, b"hello")?;
/// if reply.status() != elm::ELM_CALL_STATUS_OK {
///     return Err(elm::RuntimeApiError::Status(reply.status()));
/// }
/// # Ok::<(), elm::RuntimeApiError>(())
/// ```
pub struct ManagedImport {
    slot: ImportSlot,
}

impl ManagedImport {
    /// 构造尚未绑定的零值 import 槽。
    ///
    /// 只有装载器可以在模块激活前写入该槽；业务代码不能自行绑定 handle。
    pub const fn new() -> Self {
        Self {
            slot: ImportSlot::new(),
        }
    }

    /// 返回装载器写入的不透明 handle；可选 import 未解析时返回 `None`。
    ///
    /// handle 不是地址，不能解引用，也不应持久化到镜像外部。
    pub fn handle(&self) -> Option<u64> {
        let value = self.slot.read();
        (value != 0).then_some(value as u64)
    }

    /// 使用已经构造的固定调用帧执行一次受管调用。
    ///
    /// 运行时会覆盖路由语义并验证返回的 binding/call id。多数业务代码应使用
    /// [`call_bytes`](Self::call_bytes)、[`call_payload`](Self::call_payload) 或
    /// [`call`](Self::call)，避免自行维护 call id。
    pub fn invoke(&self, request: &ElmCallFrame) -> Result<ElmReplyFrame, RuntimeApiError> {
        let handle = self.handle().ok_or(RuntimeApiError::ImportUnavailable)?;
        runtime_api::invoke_managed(handle, request)
    }

    /// 用原始载荷执行受管调用。
    ///
    /// 框架自动生成非零 call id，并以 binding id 0 请求运行时按 import handle 路由。载荷
    /// 超过固定帧容量时不会截断，而是返回 [`PayloadError::BufferTooSmall`]。
    pub fn call_bytes(&self, opcode: u32, payload: &[u8]) -> Result<ManagedReply, RuntimeApiError> {
        if payload.len() > ELM_FRAME_PAYLOAD_LEN {
            return Err(RuntimeApiError::Payload(PayloadError::BufferTooSmall));
        }
        let request = ElmCallFrame::new(0, next_managed_call_id(), opcode, payload);
        ManagedReply::from_frame(self.invoke(&request)?)
    }

    /// 编码一个 [`ElmPayload`] 请求并执行受管调用。
    ///
    /// 此方法不自动要求回复状态成功，也不解码回复，适合一个操作可能返回多种载荷契约的场景。
    pub fn call_payload<T: ElmPayload>(
        &self,
        opcode: u32,
        payload: &T,
    ) -> Result<ManagedReply, RuntimeApiError> {
        if T::WIRE_SIZE > ELM_FRAME_PAYLOAD_LEN {
            return Err(RuntimeApiError::Payload(PayloadError::BufferTooSmall));
        }
        let mut bytes = [0u8; ELM_FRAME_PAYLOAD_LEN];
        let len = payload.encode(&mut bytes)?;
        if len > bytes.len() {
            return Err(RuntimeApiError::MalformedResponse);
        }
        self.call_bytes(opcode, &bytes[..len])
    }

    /// 执行完整的类型化请求/回复调用。
    ///
    /// 请求使用 `T` 编码；只有回复状态为 `ELM_CALL_STATUS_OK` 时才使用 `R` 解码。状态失败、
    /// 线格式错误和运行时错误都通过 [`RuntimeApiError`] 返回。
    pub fn call<T: ElmPayload, R: ElmPayload>(
        &self,
        opcode: u32,
        payload: &T,
    ) -> Result<R, RuntimeApiError> {
        let reply = self.call_payload(opcode, payload)?;
        if reply.status() != ELM_CALL_STATUS_OK {
            return Err(RuntimeApiError::Status(reply.status()));
        }
        reply.decode()
    }
}

impl Default for ManagedImport {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(transparent)]
#[derive(Debug)]
/// 由装载器写入原生地址的直接固定 import 槽。
///
/// 该路径绕过受管调用帧，只适合已由运行时授予 native import 能力、ABI 指纹完全匹配且目标
/// generation 被固定的低层场景。直接导入会限制或阻止目标热替换；普通 ELM 应使用
/// [`ManagedImport`]。
pub struct UnsafeDirectImport {
    slot: ImportSlot,
}

impl UnsafeDirectImport {
    /// 构造尚未绑定的零值直接导入槽。
    pub const fn new() -> Self {
        Self {
            slot: ImportSlot::new(),
        }
    }

    /// 返回装载器写入的原生目标地址。
    ///
    /// # 安全性
    ///
    /// 调用方必须自行证明地址来自与声明完全一致的函数签名和 Rust ABI 指纹，目标 generation
    /// 在整个调用期间被 pin，目标代码和数据仍映射，调用权限仍有效，并且 panic 不会跨 ABI
    /// 展开。把地址转换为错误函数类型或在解绑、卸载、替换后调用会导致未定义行为。
    pub unsafe fn address(&self) -> Option<usize> {
        let value = self.slot.read();
        (value != 0).then_some(value)
    }
}

impl Default for UnsafeDirectImport {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(transparent)]
#[derive(Debug)]
struct ImportSlot(UnsafeCell<usize>);

impl ImportSlot {
    const fn new() -> Self {
        Self(UnsafeCell::new(0))
    }

    fn read(&self) -> usize {
        // 安全性：装载器只在激活前写入槽位；运行期只做易失只读访问。
        unsafe { core::ptr::read_volatile(self.0.get()) }
    }
}

unsafe impl Sync for ImportSlot {}

#[repr(transparent)]
struct RootImportSlot(UnsafeCell<usize>);

unsafe impl Sync for RootImportSlot {}

static NEXT_MANAGED_CALL_ID: AtomicU64 = AtomicU64::new(1);

fn next_managed_call_id() -> u64 {
    let id = NEXT_MANAGED_CALL_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        NEXT_MANAGED_CALL_ID.fetch_add(1, Ordering::Relaxed)
    } else {
        id
    }
}

#[unsafe(export_name = "__elm_api_root_slot_v1")]
#[unsafe(link_section = ".data.elm_imports")]
#[used]
static ELM_API_ROOT_SLOT: RootImportSlot = RootImportSlot(UnsafeCell::new(0));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 安全开发包装访问 ELM 根 API、运行时表或受管 import 时的错误。
///
/// 该枚举区分装载链接问题、协议布局问题、业务状态和固定载荷问题，便于模块决定是降级、
/// 返回 `HookError` 还是主动中止。它不包含内核内部错误类型，因而可以稳定存在于外部框架。
pub enum RuntimeApiError {
    /// 根 API 导入槽仍为零，通常表示模块没有通过合规装载器启动。
    RootUnavailable,
    /// 根表魔数、版本、选择版本或最小结构尺寸与当前框架不兼容。
    IncompatibleRoot,
    /// 根表没有提供兼容的普通运行时函数表。
    RuntimeUnavailable,
    /// 受管 import 槽未绑定；可选 import 在没有匹配 export 时会产生此错误。
    ImportUnavailable,
    /// 调用方缓冲区不足；携带运行时报告的所需最小字节数。
    BufferTooSmall(usize),
    /// 运行时回复违反结构尺寸、保留字段、载荷边界或调用关联不变量。
    MalformedResponse,
    /// 运行时或被调用 export 返回了非零稳定状态码。
    Status(i32),
    /// 请求编码或回复解码失败。
    Payload(PayloadError),
}

impl From<PayloadError> for RuntimeApiError {
    fn from(value: PayloadError) -> Self {
        Self::Payload(value)
    }
}

pub(crate) mod runtime_api {
    use super::*;

    pub fn features() -> Result<u64, RuntimeApiError> {
        Ok(root()?.features)
    }

    pub fn log(level: u32, message: &str) -> Result<(), RuntimeApiError> {
        let status = (runtime()?.log)(level, message.as_ptr(), message.len());
        status_result(status)
    }

    pub fn abort_current(reason: u32) -> ! {
        match runtime() {
            Ok(runtime) => (runtime.abort_current)(reason),
            Err(_) => loop {
                core::hint::spin_loop();
            },
        }
    }

    pub fn abort_panic() -> ! {
        abort_current(ELM_API_ABORT_REASON_PANIC)
    }

    pub fn current_context() -> Result<ElmApiContextV1, RuntimeApiError> {
        let mut output = ElmApiContextV1::empty();
        let status = (runtime()?.current_context)(&mut output);
        status_result(status)?;
        Ok(output)
    }

    pub fn dispatch_mixin(input: &[u8], output: &mut [u8]) -> Result<usize, RuntimeApiError> {
        let mut output_len = 0usize;
        let status = (runtime()?.dispatch_mixin)(
            input.as_ptr(),
            input.len(),
            output.as_mut_ptr(),
            output.len(),
            &mut output_len,
        );
        if status == ELM_API_STATUS_BUFFER_TOO_SMALL {
            return Err(RuntimeApiError::BufferTooSmall(output_len));
        }
        status_result(status)?;
        if output_len > output.len() {
            return Err(RuntimeApiError::MalformedResponse);
        }
        Ok(output_len)
    }

    pub fn invoke_managed(
        import_handle: u64,
        request: &ElmCallFrame,
    ) -> Result<ElmReplyFrame, RuntimeApiError> {
        let mut reply = ElmReplyFrame::empty(
            request.binding_id,
            request.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
        let status = (runtime()?.invoke_managed)(import_handle, request, &mut reply);
        status_result(status)?;
        if reply.binding_id != request.binding_id || reply.call_id != request.call_id {
            return Err(RuntimeApiError::MalformedResponse);
        }
        Ok(reply)
    }

    pub fn query_namespace(
        identifier: &str,
        versions: &[u16],
    ) -> Result<ElmApiNamespaceV1, RuntimeApiError> {
        let mut output = ElmApiNamespaceV1::empty();
        let status = (root()?.query_namespace)(
            identifier.as_ptr(),
            identifier.len(),
            versions.as_ptr(),
            versions.len(),
            &mut output,
        );
        status_result(status)?;
        Ok(output)
    }

    pub(crate) fn ensure_linked() {
        let _ = root_address();
    }

    fn root() -> Result<&'static ElmApiRootV1, RuntimeApiError> {
        let address = root_address();
        if address == 0 {
            return Err(RuntimeApiError::RootUnavailable);
        }
        // 安全性：槽位只由 ELM 装载器写入经过 ABI 校验的静态根表地址。
        let root = unsafe { &*(address as *const ElmApiRootV1) };
        if root.magic != ELM_API_ROOT_MAGIC
            || root.abi_version != ELM_API_VERSION_V1
            || root.selected_version != ELM_API_VERSION_V1
            || root.struct_size < core::mem::size_of::<ElmApiRootV1>() as u32
        {
            return Err(RuntimeApiError::IncompatibleRoot);
        }
        Ok(root)
    }

    fn runtime() -> Result<&'static ElmRuntimeApiV1, RuntimeApiError> {
        let root = root()?;
        if root.runtime_table.is_null()
            || root.runtime_table_size < core::mem::size_of::<ElmRuntimeApiV1>() as u32
        {
            return Err(RuntimeApiError::RuntimeUnavailable);
        }
        // 安全性：根表由内核发布，且已验证表地址和最小尺寸。
        let runtime = unsafe { &*root.runtime_table };
        if runtime.abi_version != ELM_API_VERSION_V1
            || runtime.struct_size < core::mem::size_of::<ElmRuntimeApiV1>() as u32
        {
            return Err(RuntimeApiError::RuntimeUnavailable);
        }
        Ok(runtime)
    }

    fn root_address() -> usize {
        // 安全性：装载阶段完成单次槽位重定位，运行阶段只做易失读取。
        unsafe { core::ptr::read_volatile(ELM_API_ROOT_SLOT.0.get()) }
    }

    fn status_result(status: i32) -> Result<(), RuntimeApiError> {
        if status == 0 {
            Ok(())
        } else {
            Err(RuntimeApiError::Status(status))
        }
    }
}

/// 按固定阶段顺序执行一个 mixin 补缀点。
///
/// 此函数主要供 [`mixin_point`](crate::mixin_point) 展开代码调用。它把 `frame` 编码后交给
/// elm-mgr 的 extension dispatcher，并严格按 ingress、substitute、原实现、egress、observe
/// 顺序推进。substitute 返回替换帧时跳过 `original`；observe 阶段返回的替换标志会被忽略，
/// 但拒绝和协议错误仍会使调用失败。
///
/// `descriptor.contract` 必须与 `T::CONTRACT` 以及所有 attached mixin 声明一致，且
/// `T::WIRE_SIZE` 不得超过运行时扩展载荷容量。任何阶段返回 deny、blocker、非成功状态、
/// 错误长度或不可解码替换载荷时返回 [`HookError`]。
///
/// 普通模块不应手工拼装描述符；使用 attribute 可以同时生成正确的阶段名称和 `.elm.meta`
/// 扩展点声明。
pub fn run_mixin_point<T: ElmPayload>(
    descriptor: MixinPointDescriptor,
    frame: &mut T,
    original: fn(&mut T) -> PointResult,
) -> PointResult {
    if let Some(point) = descriptor.ingress {
        dispatch_mixin_stage(point, descriptor.contract, frame)?;
    }
    let substituted = match descriptor.substitute {
        Some(point) => dispatch_mixin_stage(point, descriptor.contract, frame)?,
        None => false,
    };
    if !substituted {
        original(frame)?;
    }
    if let Some(point) = descriptor.egress {
        dispatch_mixin_stage(point, descriptor.contract, frame)?;
    }
    if let Some(point) = descriptor.observe {
        let _ = dispatch_mixin_stage(point, descriptor.contract, frame)?;
    }
    Ok(())
}

fn dispatch_mixin_stage<T: ElmPayload>(
    point: &str,
    contract: &str,
    frame: &mut T,
) -> Result<bool, HookError> {
    if T::WIRE_SIZE > MGR_EXTENSION_PAYLOAD_LEN {
        return Err(HookError::new(ELM_CALL_STATUS_INVALID));
    }
    let context = runtime_api::current_context().map_err(runtime_error_to_hook)?;
    let mut request = ModuleExtensionDispatchRequest::new(context.cell_id, point, contract)
        .ok_or_else(|| HookError::new(ELM_CALL_STATUS_INVALID))?;
    let payload_len = frame
        .encode(&mut request.payload)
        .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?;
    request.payload_len = payload_len as u16;
    let input = request.encode();
    let mut output = [0u8; MGR_RESPONSE_HEADER_SIZE + MGR_EXTENSION_DISPATCH_RESPONSE_SIZE];
    let output_len =
        runtime_api::dispatch_mixin(&input, &mut output).map_err(runtime_error_to_hook)?;
    let header_size = MGR_RESPONSE_HEADER_SIZE;
    let response_size = MGR_EXTENSION_DISPATCH_RESPONSE_SIZE;
    if output_len != header_size + response_size {
        return Err(HookError::new(ELM_CALL_STATUS_INVALID));
    }
    let header = ModuleMgrResponseHeader::decode(&output[..header_size])
        .ok_or_else(|| HookError::new(ELM_CALL_STATUS_INVALID))?;
    if header.status != MGR_STATUS_OK
        || header.reserved != 0
        || header.payload_len as usize != response_size
    {
        return Err(HookError::new(header.status));
    }
    let response = ModuleExtensionDispatchResponse::decode(&output[header_size..])
        .ok_or_else(|| HookError::new(ELM_CALL_STATUS_INVALID))?;
    if response.status != MGR_STATUS_OK || response.blockers != 0 {
        return Err(HookError::new(response.status));
    }
    if response.reply.flags & MIXIN_REPLY_DENY != 0 {
        return Err(HookError::new(ELM_CALL_STATUS_INVALID));
    }
    let replaced = response.reply.flags & MIXIN_REPLY_REPLACE != 0;
    if replaced {
        let len = usize::from(response.reply.payload_len);
        if len > response.reply.payload.len() {
            return Err(HookError::new(ELM_CALL_STATUS_INVALID));
        }
        *frame = T::decode(&response.reply.payload[..len])
            .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?;
    }
    Ok(replaced)
}

fn runtime_error_to_hook(error: RuntimeApiError) -> HookError {
    match error {
        RuntimeApiError::Status(status) => HookError::new(status),
        _ => HookError::new(ELM_CALL_STATUS_INVALID),
    }
}

#[doc(hidden)]
pub mod __private {
    use super::*;

    pub unsafe fn lifecycle_trampoline(
        raw: *mut ElmNativeHookContextV1,
        expected_phase: u16,
        handler: fn(&LifecycleContext) -> HookResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_ref() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION
            || raw.phase != expected_phase
            || raw.reserved != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        match handler(&LifecycleContext::from_raw(*raw)) {
            Ok(()) => 0,
            Err(error) => error.status(),
        }
    }

    pub unsafe fn migration_export_trampoline(
        raw: *mut ElmNativeMigrationContextV1,
        handler: fn(&MigrationContext, &mut [u8]) -> MigrationExportResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if !migration_context_valid(raw, 6) {
            return ELM_CALL_STATUS_INVALID;
        }
        let Ok(capacity) = usize::try_from(raw.buffer_capacity) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.buffer_ptr == 0 && capacity != 0 {
            return ELM_CALL_STATUS_INVALID;
        }
        let output = if capacity == 0 {
            &mut []
        } else {
            unsafe { core::slice::from_raw_parts_mut(raw.buffer_ptr as *mut u8, capacity) }
        };
        match handler(&MigrationContext::from_raw(raw), output) {
            Ok(len) if len <= capacity => {
                raw.buffer_len = len as u64;
                raw.status = 0;
                0
            }
            Ok(_) => ELM_CALL_STATUS_INVALID,
            Err(error) => error.status(),
        }
    }

    pub unsafe fn migration_input_trampoline(
        raw: *mut ElmNativeMigrationContextV1,
        expected_phase: u16,
        handler: fn(&MigrationContext, &[u8]) -> HookResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if !migration_context_valid(raw, expected_phase)
            || raw.buffer_len > raw.buffer_capacity
            || raw.buffer_ptr == 0 && raw.buffer_len != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let Ok(len) = usize::try_from(raw.buffer_len) else {
            return ELM_CALL_STATUS_INVALID;
        };
        let input = if len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(raw.buffer_ptr as *const u8, len) }
        };
        match handler(&MigrationContext::from_raw(raw), input) {
            Ok(()) => {
                raw.status = 0;
                0
            }
            Err(error) => error.status(),
        }
    }

    pub unsafe fn entry_trampoline(
        raw: *mut ElmNativeEntryFrameV1,
        handler: fn(&EntryContext) -> EntryResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_ENTRY_ABI_VERSION
            || raw.flags != 0
            || raw.reserved0 != 0
            || raw.reserved1 != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        match handler(&EntryContext::from_raw(*raw)) {
            Ok(()) => {
                raw.exit_code = 0;
                0
            }
            Err(error) => {
                raw.exit_code = error.status();
                error.status()
            }
        }
    }

    pub unsafe fn provider_trampoline<F>(raw: *mut ElmNativeProviderCallV1, handler: F) -> i32
    where
        F: FnOnce(&ProviderRequest) -> ProviderResult,
    {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_PROVIDER_CALL_ABI_VERSION
            || raw.flags != 0
            || raw.reserved0 != 0
            || raw.binding_id != raw.request.binding_id
            || usize::from(raw.request.payload_len) > raw.request.payload.len()
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let request = ProviderRequest {
            cell_id: raw.cell_id,
            port_id: raw.port_id,
            lease_id: raw.lease_id,
            frame: raw.request,
        };
        match handler(&request) {
            Ok(reply) => {
                raw.reply = reply.into_frame(raw.request.binding_id, raw.request.call_id);
                0
            }
            Err(error) => error.status(),
        }
    }

    pub unsafe fn managed_trampoline(
        raw: *mut ElmNativeManagedCallV1,
        handler: fn(&ManagedRequest) -> ManagedResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_MANAGED_CALL_ABI_VERSION
            || raw.flags != 0
            || raw.reserved0 != 0
            || usize::from(raw.request.payload_len) > raw.request.payload.len()
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let request = ManagedRequest {
            import_handle: raw.import_handle,
            caller_cell_id: raw.caller_cell_id,
            caller_generation: raw.caller_generation,
            callee_cell_id: raw.callee_cell_id,
            callee_generation: raw.callee_generation,
            frame: raw.request,
        };
        match handler(&request) {
            Ok(reply) => {
                raw.reply = reply.into_frame(raw.request.binding_id, raw.request.call_id);
                0
            }
            Err(error) => error.status(),
        }
    }

    pub unsafe fn snapshot_trampoline(
        raw: *mut ElmNativeProviderSnapshotV1,
        handler: fn(&SnapshotRequest, &mut [u8]) -> SnapshotResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION
            || raw.flags & !ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED != 0
            || raw.reserved0 != 0
            || raw.reserved1 != 0
            || raw.payload_addr == 0 && raw.capacity != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let paged = raw.flags & ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED != 0;
        let request = SnapshotRequest {
            cell_id: raw.cell_id,
            port_id: raw.port_id,
            binding_id: raw.binding_id,
            lease_id: raw.lease_id,
            paged,
            cursor: if paged { raw.reserved2 } else { 0 },
        };
        let output = if raw.capacity == 0 {
            &mut []
        } else {
            unsafe {
                core::slice::from_raw_parts_mut(raw.payload_addr as *mut u8, raw.capacity as usize)
            }
        };
        match handler(&request, output) {
            Ok(reply) if reply.payload_len <= output.len() => {
                raw.status = reply.status;
                raw.payload_len = reply.payload_len as u32;
                raw.record_count = reply.record_count;
                raw.flags = if paged {
                    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED
                } else {
                    0
                };
                if let Some(next) = reply.next_cursor {
                    if !paged || next == 0 || next == request.cursor {
                        return ELM_CALL_STATUS_INVALID;
                    }
                    raw.flags |= ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE;
                    raw.reserved2 = next;
                } else {
                    raw.reserved2 = 0;
                }
                if raw.flags & !ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK != 0 {
                    return ELM_CALL_STATUS_INVALID;
                }
                0
            }
            Ok(_) => ELM_CALL_STATUS_INVALID,
            Err(error) => error.status(),
        }
    }

    pub unsafe fn mixin_trampoline<T: ElmPayload>(
        raw: *mut ElmNativeProviderCallV1,
        handler: fn(&mut T) -> MixinControl,
    ) -> i32 {
        unsafe {
            provider_trampoline(raw, |request| {
                let mut frame = request
                    .decode::<T>()
                    .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?;
                let control = handler(&mut frame);
                let flags = match control {
                    MixinControl::Continue => MIXIN_REPLY_CONTINUE,
                    MixinControl::Stop => MIXIN_REPLY_STOP,
                    MixinControl::Replace => MIXIN_REPLY_REPLACE,
                    MixinControl::ReplaceAndStop => MIXIN_REPLY_REPLACE | MIXIN_REPLY_STOP,
                    MixinControl::Deny => MIXIN_REPLY_DENY,
                };
                let reply = if flags & MIXIN_REPLY_REPLACE != 0 {
                    ProviderReply::payload(ELM_CALL_STATUS_OK, &frame)
                        .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?
                } else {
                    ProviderReply::ok()
                };
                Ok(reply.with_flags(flags))
            })
        }
    }

    pub fn write_bytes(
        output: &mut [u8],
        offset: &mut usize,
        bytes: &[u8],
    ) -> Result<(), PayloadError> {
        let end = offset
            .checked_add(bytes.len())
            .ok_or(PayloadError::BufferTooSmall)?;
        let target = output
            .get_mut(*offset..end)
            .ok_or(PayloadError::BufferTooSmall)?;
        target.copy_from_slice(bytes);
        *offset = end;
        Ok(())
    }

    pub fn read_array<const N: usize>(
        input: &[u8],
        offset: &mut usize,
    ) -> Result<[u8; N], PayloadError> {
        let end = offset.checked_add(N).ok_or(PayloadError::SizeMismatch)?;
        let source = input.get(*offset..end).ok_or(PayloadError::SizeMismatch)?;
        let mut output = [0u8; N];
        output.copy_from_slice(source);
        *offset = end;
        Ok(output)
    }

    pub fn read_bool(input: &[u8], offset: &mut usize) -> Result<bool, PayloadError> {
        match read_array::<1>(input, offset)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(PayloadError::InvalidBoolean),
        }
    }

    fn migration_context_valid(raw: &ElmNativeMigrationContextV1, phase: u16) -> bool {
        raw.abi_version == ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION
            && raw.phase == phase
            && raw.flags == 0
            && raw.status == 0
            && raw.reserved == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ElmLifecyclePhase, ElmNativeMigrationContextV1};
    use crate::ids::{ElmId, Generation};

    fn export_empty(_context: &MigrationContext, output: &mut [u8]) -> MigrationExportResult {
        assert!(output.is_empty());
        Ok(0)
    }

    fn snapshot_empty(_request: &SnapshotRequest, output: &mut [u8]) -> SnapshotResult {
        assert!(output.is_empty());
        Ok(SnapshotReply::complete(0, 0))
    }

    #[test]
    fn import_wrappers_have_one_word_layout() {
        assert_eq!(
            core::mem::size_of::<ManagedImport>(),
            core::mem::size_of::<usize>()
        );
        assert_eq!(
            core::mem::size_of::<UnsafeDirectImport>(),
            core::mem::size_of::<usize>()
        );
    }

    #[test]
    fn zero_length_native_buffers_do_not_require_non_null_pointer() {
        let mut migration = ElmNativeMigrationContextV1::new(
            ElmLifecyclePhase::MigrateExport,
            ElmId(7),
            Generation(1),
            Generation(2),
            0,
            0,
            0,
        );
        let migration_status =
            unsafe { __private::migration_export_trampoline(&mut migration, export_empty) };
        assert_eq!(migration_status, 0);
        assert_eq!(migration.buffer_len, 0);

        let mut snapshot = ElmNativeProviderSnapshotV1::new(7, 8, 9, 10, 0, 0);
        let snapshot_status =
            unsafe { __private::snapshot_trampoline(&mut snapshot, snapshot_empty) };
        assert_eq!(snapshot_status, 0);
        assert_eq!(snapshot.payload_len, 0);
    }
}

//! ELM 生命周期上下文和当前执行上下文。
//!
//! 内核进入任何原生 ELM 代码前都会建立一个按栈嵌套的当前上下文，离开时由 RAII guard
//! 恢复上一层。子系统可以通过 [`current_context`] 取得当前 cell、generation、状态、kind
//! 和允许动作，从而执行 per-cell 策略检查，而不需要反向依赖 elm-mgr。
//!
//! 正式内核运行时使用 [`register_current_context_ops`] 把普通上下文绑定到调度任务，保证任务
//! 迁移 CPU 后仍得到正确身份。按 CPU 固定栈用于硬中断上下文，以及独立测试和无调度器环境
//! 的后备实现。

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::ids::{ElmId, Generation};
use crate::kind::ElmKind;
use crate::state::ElmState;

const ELM_CONTEXT_ALLOWED_ACTIONS_ALL: u32 = (1 << 9) - 1;

/// [`ElmNativeHookContextV1`] 的 ABI 版本。
pub const ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION: u16 = 1;
/// [`ElmNativeMigrationContextV1`] 的 ABI 版本。
pub const ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION: u16 = 1;
/// 后备按 CPU 上下文栈可区分的最大 CPU 数。
///
/// 正式任务级后端不受此值限制；未注册 CPU resolver 时统一使用 CPU 0。
pub const ELM_CONTEXT_MAX_CPUS: usize = 12;
/// 单个 CPU 后备上下文栈允许的最大嵌套深度。
pub const ELM_CONTEXT_MAX_DEPTH: usize = 16;

const ELM_CONTEXT_SLOT_COUNT: usize = ELM_CONTEXT_MAX_CPUS * ELM_CONTEXT_MAX_DEPTH;

type CurrentCpuIdFn = fn() -> usize;

/// 由上层运行时注入的任务级当前上下文存储。
///
/// `elm` crate 不依赖调度器；内核通过这张静态表把上下文绑定到当前任务。未注册时
/// 保留按 CPU 的固定栈，仅供独立 crate 测试和不带调度器的宿主使用。
pub struct ElmCurrentContextOps {
    /// 把上下文压入当前任务并返回非零或后端定义的退出 token；失败返回 `None`。
    pub enter: fn(ElmCurrentContext) -> Option<u64>,
    /// 使用 `enter` 返回的 token 按栈顺序退出上下文。
    pub leave: fn(u64),
    /// 压入一个“暂时没有 ELM owner”的边界，并返回恢复 token。
    pub suspend: fn() -> Option<u64>,
    /// 使用 `suspend` 返回的 token 按栈顺序恢复外层上下文。
    pub resume: fn(u64),
    /// 返回当前任务最内层 ELM 上下文；任务不在 ELM 中时返回 `None`。
    pub current: fn() -> Option<ElmCurrentContext>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 当前原生调用所处的生命周期或迁移阶段。
pub enum ElmLifecyclePhase {
    /// 新单元公开前执行必需初始化。
    Initialize,
    /// 撤销拓扑和释放镜像前执行必需终结。
    Finalize,
    /// 停止产生新工作并准备排空。
    Quiesce,
    /// 排空完成后进入暂停状态。
    Pause,
    /// 从暂停状态恢复服务前执行。
    Resume,
    /// 旧 generation 向迁移缓冲区导出状态。
    MigrateExport,
    /// 新 generation 从迁移缓冲区导入状态。
    MigrateImport,
    /// 替换失败后撤销新 generation 的迁移状态。
    MigrateAbort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 内核侧建立当前 ELM 身份时使用的强类型上下文。
///
/// 该类型不是跨 ABI 结构，而是内核运行时和可复用子系统之间的纯 Rust 值。创建后通过
/// [`enter_current_context`] 进入作用域，guard drop 时恢复上一层。
pub struct ElmContext {
    cell_id: ElmId,
    parent_id: Option<ElmId>,
    generation: Generation,
    state: ElmState,
    phase: ElmLifecyclePhase,
    kind: ElmKind,
    flags: u32,
    allowed_actions: u32,
}

impl ElmContext {
    /// 构造上下文，默认 kind 为 [`ElmKind::Other`] 且允许全部已定义动作。
    ///
    /// 内核在进入不可信模块前应使用 [`with_kind`](Self::with_kind) 和
    /// [`with_allowed_actions`](Self::with_allowed_actions) 收紧实际值。
    pub const fn new(
        cell_id: ElmId,
        parent_id: Option<ElmId>,
        generation: Generation,
        state: ElmState,
        phase: ElmLifecyclePhase,
        flags: u32,
    ) -> Self {
        Self {
            cell_id,
            parent_id,
            generation,
            state,
            phase,
            kind: ElmKind::Other,
            flags,
            allowed_actions: ELM_CONTEXT_ALLOWED_ACTIONS_ALL,
        }
    }

    /// 返回当前调用关联的 ELM 单元标识符。
    pub const fn cell_id(&self) -> ElmId {
        self.cell_id
    }

    /// 返回父 ELM 单元标识符；没有父单元时返回 `None`。
    pub const fn parent_id(&self) -> Option<ElmId> {
        self.parent_id
    }

    /// 返回用于陈旧引用检测的当前代际。
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// 返回当前 ELM 状态编码或强类型状态。
    pub const fn state(&self) -> ElmState {
        self.state
    }

    /// 返回当前生命周期或迁移阶段。
    pub const fn phase(&self) -> ElmLifecyclePhase {
        self.phase
    }

    /// 返回该对象的协议类别。
    pub const fn kind(&self) -> ElmKind {
        self.kind
    }

    /// 返回该对象当前设置的协议标志位。
    pub const fn flags(&self) -> u32 {
        self.flags
    }

    /// 返回当前上下文允许的动作位集合。
    pub const fn allowed_actions(&self) -> u32 {
        self.allowed_actions
    }

    /// 设置动作位快照并返回更新后的上下文。
    pub const fn with_allowed_actions(mut self, allowed_actions: u32) -> Self {
        self.allowed_actions = allowed_actions;
        self
    }

    /// 设置当前 cell kind 并返回更新后的上下文。
    pub const fn with_kind(mut self, kind: ElmKind) -> Self {
        self.kind = kind;
        self
    }

    /// 更新进入下一次调用时要观察到的 cell 状态。
    ///
    /// 此 setter 不验证生命周期状态机；调用方必须先通过 [`ElmState::transition_to`] 或运行时
    /// 事务完成状态迁移校验。
    pub fn set_state(&mut self, state: ElmState) {
        self.state = state;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 可复制的当前 ELM 身份快照。
///
/// 子系统用它执行无锁快速策略检查。快照只对取得它的调用时刻有效，不能在异步工作中长期
/// 保存；异步资源必须显式记录 owner cell 和 generation，并在执行时重新校验。
pub struct ElmCurrentContext {
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: ElmId,
    /// 父 ELM 单元；根单元为 `None`。
    pub parent_id: Option<ElmId>,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: Generation,
    /// 对象或单元的当前状态编码。
    pub state: ElmState,
    /// 当前生命周期或迁移阶段编码。
    pub phase: ElmLifecyclePhase,
    /// 当前 cell 的角色分类。
    pub kind: ElmKind,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 当前上下文允许执行的管理动作位集合。
    pub allowed_actions: u32,
}

impl ElmCurrentContext {
    /// 从内核侧 [`ElmContext`] 复制当前身份快照。
    pub const fn from_context(context: &ElmContext) -> Self {
        Self {
            cell_id: context.cell_id(),
            parent_id: context.parent_id(),
            generation: context.generation(),
            state: context.state(),
            phase: context.phase(),
            kind: context.kind(),
            flags: context.flags(),
            allowed_actions: context.allowed_actions(),
        }
    }
}

static CURRENT_CPU_ID_FN: AtomicUsize = AtomicUsize::new(0);
static CURRENT_CONTEXT_OPS: AtomicUsize = AtomicUsize::new(0);
static CURRENT_DEPTH: [AtomicUsize; ELM_CONTEXT_MAX_CPUS] =
    [const { AtomicUsize::new(0) }; ELM_CONTEXT_MAX_CPUS];
static CURRENT_CELL_ID: [AtomicU64; ELM_CONTEXT_SLOT_COUNT] =
    [const { AtomicU64::new(0) }; ELM_CONTEXT_SLOT_COUNT];
static CURRENT_PARENT_ID: [AtomicU64; ELM_CONTEXT_SLOT_COUNT] =
    [const { AtomicU64::new(0) }; ELM_CONTEXT_SLOT_COUNT];
static CURRENT_GENERATION: [AtomicU64; ELM_CONTEXT_SLOT_COUNT] =
    [const { AtomicU64::new(0) }; ELM_CONTEXT_SLOT_COUNT];
static CURRENT_STATE: [AtomicU32; ELM_CONTEXT_SLOT_COUNT] =
    [const { AtomicU32::new(0) }; ELM_CONTEXT_SLOT_COUNT];
static CURRENT_PHASE: [AtomicU32; ELM_CONTEXT_SLOT_COUNT] =
    [const { AtomicU32::new(0) }; ELM_CONTEXT_SLOT_COUNT];
static CURRENT_KIND: [AtomicU32; ELM_CONTEXT_SLOT_COUNT] =
    [const { AtomicU32::new(ElmKind::Other as u32) }; ELM_CONTEXT_SLOT_COUNT];
static CURRENT_FLAGS: [AtomicU32; ELM_CONTEXT_SLOT_COUNT] =
    [const { AtomicU32::new(0) }; ELM_CONTEXT_SLOT_COUNT];
static CURRENT_ALLOWED_ACTIONS: [AtomicU32; ELM_CONTEXT_SLOT_COUNT] =
    [const { AtomicU32::new(ELM_CONTEXT_ALLOWED_ACTIONS_ALL) }; ELM_CONTEXT_SLOT_COUNT];

#[derive(Debug)]
/// 当前 ELM 上下文作用域的 RAII guard。
///
/// guard 必须按后进先出顺序 drop。任务级后端收到原 `enter` token；后备实现清空对应栈槽。
/// 故意泄漏 guard 会让后续代码继续被识别为该 ELM，属于严重的调用边界错误。
#[must_use = "必须持有 guard 直到本次 ELM 调用结束"]
pub struct ElmCurrentContextGuard {
    cpu_id: usize,
    depth: usize,
    backend_token: u64,
    task_backed: bool,
}

#[derive(Debug)]
/// 暂停当前 ELM owner 的 RAII guard。
///
/// guard 存活期间 [`current_context`] 返回 `None`，但外层上下文仍保留在栈中；
/// guard drop 后按 LIFO 顺序恢复。该边界供常驻 provider 在动态 consumer 回调中
/// 创建自身运行期资源，避免资源被错误归属给 consumer。
#[must_use = "必须持有 guard 直到常驻 provider 回调结束"]
pub struct ElmCurrentContextSuspensionGuard {
    cpu_id: usize,
    depth: usize,
    backend_token: u64,
    task_backed: bool,
}

impl Drop for ElmCurrentContextGuard {
    fn drop(&mut self) {
        if self.task_backed {
            if let Some(ops) = current_context_ops() {
                (ops.leave)(self.backend_token);
            }
            return;
        }
        let current = CURRENT_DEPTH[self.cpu_id].load(Ordering::Acquire);
        debug_assert_eq!(current, self.depth + 1, "ELM 当前上下文必须按栈顺序退出");
        if current != self.depth + 1 {
            return;
        }
        clear_context_slot(context_slot(self.cpu_id, self.depth));
        CURRENT_DEPTH[self.cpu_id].store(self.depth, Ordering::Release);
    }
}

impl Drop for ElmCurrentContextSuspensionGuard {
    fn drop(&mut self) {
        if self.task_backed {
            if let Some(ops) = current_context_ops() {
                (ops.resume)(self.backend_token);
            }
            return;
        }
        let current = CURRENT_DEPTH[self.cpu_id].load(Ordering::Acquire);
        debug_assert_eq!(
            current,
            self.depth + 1,
            "ELM 当前上下文暂停边界必须按栈顺序退出"
        );
        if current != self.depth + 1 {
            return;
        }
        clear_context_slot(context_slot(self.cpu_id, self.depth));
        CURRENT_DEPTH[self.cpu_id].store(self.depth, Ordering::Release);
    }
}

/// 进入一个可嵌套的当前 ELM 上下文。
///
/// 当前实现等价于 [`try_enter_current_context`]。返回 `None` 表示任务后端拒绝进入，或后备栈
/// 达到最大深度。成功后必须把 guard 保持到原生调用、provider 回调或生命周期钩子结束。
pub fn enter_current_context(context: &ElmContext) -> Option<ElmCurrentContextGuard> {
    try_enter_current_context(context)
}

/// 尝试进入当前 ELM 上下文而不分配内存。
///
/// 已注册任务级后端时委托给后端；否则使用按 CPU 原子固定栈。后备模式不适合可迁移任务，
/// 正式内核必须在启动早期注册任务级后端。
pub fn try_enter_current_context(context: &ElmContext) -> Option<ElmCurrentContextGuard> {
    let cpu_id = current_cpu_id();
    if cpu_context_depth(cpu_id) != 0 {
        return try_enter_cpu_context(cpu_id, ElmCurrentContext::from_context(context));
    }
    if let Some(ops) = current_context_ops() {
        let backend_token = (ops.enter)(ElmCurrentContext::from_context(context))?;
        return Some(ElmCurrentContextGuard {
            cpu_id: 0,
            depth: 0,
            backend_token,
            task_backed: true,
        });
    }
    try_enter_cpu_context(cpu_id, ElmCurrentContext::from_context(context))
}

/// 在当前 CPU 上进入硬中断专用的 ELM 上下文。
///
/// 此入口始终使用按 CPU 固定栈，不访问任务级后端。IRQ 回调可能打断持有任务扩展锁的
/// 内核路径，因此不能通过当前任务取得或创建 ELM 执行状态。guard 存活期间的嵌套进入、
/// 暂停和查询也会继续使用同一个固定栈；退出最外层 IRQ guard 后恢复被打断任务的上下文。
pub fn try_enter_interrupt_context(context: &ElmContext) -> Option<ElmCurrentContextGuard> {
    try_enter_cpu_context(current_cpu_id(), ElmCurrentContext::from_context(context))
}

/// 暂时隐藏当前 ELM owner，并在 guard drop 时恢复。
///
/// 已注册任务级后端时，暂停标记跟随当前任务迁移；否则使用按 CPU 固定栈。实现只
/// 压入一个空标记，不分配内存，也不会丢弃外层上下文。暂停边界内仍可通过
/// [`enter_current_context`] 进入一个显式 provider 上下文。
pub fn suspend_current_context() -> Option<ElmCurrentContextSuspensionGuard> {
    let cpu_id = current_cpu_id();
    let depth = cpu_context_depth(cpu_id);
    if depth != 0 {
        return try_suspend_cpu_context(cpu_id, depth);
    }
    if let Some(ops) = current_context_ops() {
        let backend_token = (ops.suspend)()?;
        return Some(ElmCurrentContextSuspensionGuard {
            cpu_id: 0,
            depth: 0,
            backend_token,
            task_backed: true,
        });
    }
    try_suspend_cpu_context(cpu_id, depth)
}

/// 返回当前任务最内层 ELM 身份快照。
///
/// 不在 ELM 调用边界内、栈为空或后端无上下文时返回 `None`。此函数不获取 elm-mgr 核心锁，
/// 可供 allocator、VFS、调度和设备子系统执行快速 per-cell 检查。
pub fn current_context() -> Option<ElmCurrentContext> {
    let cpu_id = current_cpu_id();
    let depth = cpu_context_depth(cpu_id);
    if depth != 0 {
        return cpu_context(cpu_id, depth);
    }
    if let Some(ops) = current_context_ops() {
        return (ops.current)();
    }
    None
}

fn try_enter_cpu_context(
    cpu_id: usize,
    context: ElmCurrentContext,
) -> Option<ElmCurrentContextGuard> {
    let depth = cpu_context_depth(cpu_id);
    if depth >= ELM_CONTEXT_MAX_DEPTH {
        return None;
    }
    store_context_slot(context_slot(cpu_id, depth), context);
    CURRENT_DEPTH[cpu_id].store(depth + 1, Ordering::Release);
    Some(ElmCurrentContextGuard {
        cpu_id,
        depth,
        backend_token: 0,
        task_backed: false,
    })
}

fn try_suspend_cpu_context(
    cpu_id: usize,
    depth: usize,
) -> Option<ElmCurrentContextSuspensionGuard> {
    if depth >= ELM_CONTEXT_MAX_DEPTH {
        return None;
    }
    clear_context_slot(context_slot(cpu_id, depth));
    CURRENT_DEPTH[cpu_id].store(depth + 1, Ordering::Release);
    Some(ElmCurrentContextSuspensionGuard {
        cpu_id,
        depth,
        backend_token: 0,
        task_backed: false,
    })
}

fn cpu_context(cpu_id: usize, depth: usize) -> Option<ElmCurrentContext> {
    if depth > ELM_CONTEXT_MAX_DEPTH {
        return None;
    }
    let slot = context_slot(cpu_id, depth - 1);
    let cell_id = CURRENT_CELL_ID[slot].load(Ordering::Acquire);
    if cell_id == 0 {
        return None;
    }
    let parent_id = match CURRENT_PARENT_ID[slot].load(Ordering::Acquire) {
        0 => None,
        id => Some(ElmId(id)),
    };
    Some(ElmCurrentContext {
        cell_id: ElmId(cell_id),
        parent_id,
        generation: Generation(CURRENT_GENERATION[slot].load(Ordering::Acquire)),
        state: state_from_raw(CURRENT_STATE[slot].load(Ordering::Acquire)),
        phase: phase_from_raw(CURRENT_PHASE[slot].load(Ordering::Acquire)),
        kind: ElmKind::from_raw(CURRENT_KIND[slot].load(Ordering::Acquire))
            .unwrap_or(ElmKind::Other),
        flags: CURRENT_FLAGS[slot].load(Ordering::Acquire),
        allowed_actions: CURRENT_ALLOWED_ACTIONS[slot].load(Ordering::Acquire),
    })
}

fn cpu_context_depth(cpu_id: usize) -> usize {
    CURRENT_DEPTH[cpu_id].load(Ordering::Acquire)
}

/// 返回当前 ELM cell id；不在 ELM 上下文中时返回 `None`。
pub fn current_cell() -> Option<ElmId> {
    current_context().map(|context| context.cell_id)
}

/// 为后备按 CPU 上下文栈注册 CPU id 解析函数。
///
/// 只能成功注册一次。解析结果大于支持上限时会夹到最后一个槽，因此正式多核内核不应依赖
/// 该后备路径，而应注册任务级 [`ElmCurrentContextOps`]。
pub fn register_current_cpu_id(resolver: CurrentCpuIdFn) -> bool {
    CURRENT_CPU_ID_FN
        .compare_exchange(0, resolver as usize, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// 注册任务级上下文后端。
///
/// 运行时只能在首次进入 ELM 之前调用一次，`ops` 必须具有静态生命周期且在内核运行期间
/// 地址稳定。重复注册返回 `false`，不会替换已经发布的后端。
pub fn register_current_context_ops(ops: &'static ElmCurrentContextOps) -> bool {
    CURRENT_CONTEXT_OPS
        .compare_exchange(
            0,
            ops as *const ElmCurrentContextOps as usize,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

fn current_context_ops() -> Option<&'static ElmCurrentContextOps> {
    let raw = CURRENT_CONTEXT_OPS.load(Ordering::Acquire);
    if raw == 0 {
        return None;
    }
    // 安全性：注册入口只接受 `'static` 表，且只允许从 0 发布一次。
    Some(unsafe { &*(raw as *const ElmCurrentContextOps) })
}

fn current_cpu_id() -> usize {
    let resolver = CURRENT_CPU_ID_FN.load(Ordering::Acquire);
    if resolver == 0 {
        return 0;
    }
    // 安全性：该地址只由 `register_current_cpu_id` 写入有效的函数指针。
    let resolver: CurrentCpuIdFn = unsafe { core::mem::transmute(resolver) };
    resolver().min(ELM_CONTEXT_MAX_CPUS - 1)
}

const fn context_slot(cpu_id: usize, depth: usize) -> usize {
    cpu_id * ELM_CONTEXT_MAX_DEPTH + depth
}

fn store_context_slot(slot: usize, context: ElmCurrentContext) {
    CURRENT_PARENT_ID[slot].store(
        context.parent_id.map(|id| id.0).unwrap_or(0),
        Ordering::Release,
    );
    CURRENT_GENERATION[slot].store(context.generation.0, Ordering::Release);
    CURRENT_STATE[slot].store(context.state as u32, Ordering::Release);
    CURRENT_PHASE[slot].store(phase_to_raw(context.phase), Ordering::Release);
    CURRENT_KIND[slot].store(context.kind as u32, Ordering::Release);
    CURRENT_FLAGS[slot].store(context.flags, Ordering::Release);
    CURRENT_ALLOWED_ACTIONS[slot].store(context.allowed_actions, Ordering::Release);
    CURRENT_CELL_ID[slot].store(context.cell_id.0, Ordering::Release);
}

fn clear_context_slot(slot: usize) {
    CURRENT_CELL_ID[slot].store(0, Ordering::Release);
    CURRENT_PARENT_ID[slot].store(0, Ordering::Release);
    CURRENT_GENERATION[slot].store(0, Ordering::Release);
    CURRENT_STATE[slot].store(0, Ordering::Release);
    CURRENT_PHASE[slot].store(0, Ordering::Release);
    CURRENT_KIND[slot].store(ElmKind::Other as u32, Ordering::Release);
    CURRENT_FLAGS[slot].store(0, Ordering::Release);
    CURRENT_ALLOWED_ACTIONS[slot].store(ELM_CONTEXT_ALLOWED_ACTIONS_ALL, Ordering::Release);
}

fn phase_to_raw(phase: ElmLifecyclePhase) -> u32 {
    match phase {
        ElmLifecyclePhase::Initialize => 1,
        ElmLifecyclePhase::Finalize => 2,
        ElmLifecyclePhase::Quiesce => 3,
        ElmLifecyclePhase::Pause => 4,
        ElmLifecyclePhase::Resume => 5,
        ElmLifecyclePhase::MigrateExport => 6,
        ElmLifecyclePhase::MigrateImport => 7,
        ElmLifecyclePhase::MigrateAbort => 8,
    }
}

fn phase_from_raw(raw: u32) -> ElmLifecyclePhase {
    match raw {
        2 => ElmLifecyclePhase::Finalize,
        3 => ElmLifecyclePhase::Quiesce,
        4 => ElmLifecyclePhase::Pause,
        5 => ElmLifecyclePhase::Resume,
        6 => ElmLifecyclePhase::MigrateExport,
        7 => ElmLifecyclePhase::MigrateImport,
        8 => ElmLifecyclePhase::MigrateAbort,
        _ => ElmLifecyclePhase::Initialize,
    }
}

fn state_from_raw(raw: u32) -> ElmState {
    match raw {
        1 => ElmState::Verified,
        2 => ElmState::Loaded,
        3 => ElmState::Linked,
        4 => ElmState::Ready,
        5 => ElmState::Active,
        6 => ElmState::Quiescing,
        7 => ElmState::Paused,
        8 => ElmState::Detached,
        9 => ElmState::Retired,
        10 => ElmState::Faulted,
        11 => ElmState::Quarantined,
        _ => ElmState::Discovered,
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 生命周期钩子的 EBI Rust ABI v1 固定布局上下文。
///
/// 该结构位于内核与原生镜像边界，只包含固定宽度标量。业务模块不直接接收它；attribute
/// trampoline 验证版本、phase 和保留字段后转换为 [`LifecycleContext`](crate::LifecycleContext)。
pub struct ElmNativeHookContextV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `ElmLifecyclePhase` 的稳定编码，必须与被调用钩子相符。
    pub phase: u16,
    /// 当前调用标志；v1 仅接受运行时明确声明的位。
    pub flags: u32,
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 父 cell id；零表示没有父单元。
    pub parent_id: u64,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: u64,
    /// 进入钩子时的 `ElmState` 稳定编码。
    pub state: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmNativeHookContextV1 {
    /// 从内核侧上下文构造规范 v1 原生钩子 frame。
    ///
    /// 构造器自动写入 ABI 版本、phase 编码和零保留字段。
    pub const fn from_context(context: &ElmContext) -> Self {
        Self {
            abi_version: ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION,
            phase: match context.phase() {
                ElmLifecyclePhase::Initialize => 1,
                ElmLifecyclePhase::Finalize => 2,
                ElmLifecyclePhase::Quiesce => 3,
                ElmLifecyclePhase::Pause => 4,
                ElmLifecyclePhase::Resume => 5,
                ElmLifecyclePhase::MigrateExport => 6,
                ElmLifecyclePhase::MigrateImport => 7,
                ElmLifecyclePhase::MigrateAbort => 8,
            },
            flags: context.flags(),
            cell_id: context.cell_id().0,
            parent_id: match context.parent_id() {
                Some(parent) => parent.0,
                None => 0,
            },
            generation: context.generation().0,
            state: context.state() as u32,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 热替换迁移钩子的 EBI Rust ABI v1 固定布局上下文。
///
/// `buffer_ptr` 指向运行时拥有的迁移缓冲区。export 阶段可写至 `buffer_capacity` 并由
/// trampoline 更新 `buffer_len`；import/abort 阶段只能读取前 `buffer_len` 字节。模块不得
/// 保存该地址或在钩子返回后访问。
pub struct ElmNativeMigrationContextV1 {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 仅允许 migrate-export、migrate-import 或 migrate-abort 编码。
    pub phase: u16,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 热替换事务中旧单元的代际。
    pub old_generation: u64,
    /// 热替换事务中新单元的代际。
    pub new_generation: u64,
    /// 迁移缓冲区地址；仅可在对应生命周期调用期间按容量访问。
    pub buffer_ptr: u64,
    /// 迁移缓冲区可写入的最大字节数。
    pub buffer_capacity: u64,
    /// 迁移缓冲区当前包含的有效字节数。
    pub buffer_len: u64,
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmNativeMigrationContextV1 {
    /// 构造规范迁移 frame。
    ///
    /// 非迁移 phase 会编码为零并在 trampoline 校验时被拒绝。调用方还必须保证
    /// `buffer_len <= buffer_capacity`，且非零长度对应有效地址。
    pub const fn new(
        phase: ElmLifecyclePhase,
        cell_id: ElmId,
        old_generation: Generation,
        new_generation: Generation,
        buffer_ptr: u64,
        buffer_capacity: u64,
        buffer_len: u64,
    ) -> Self {
        Self {
            abi_version: ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION,
            phase: match phase {
                ElmLifecyclePhase::MigrateExport => 6,
                ElmLifecyclePhase::MigrateImport => 7,
                ElmLifecyclePhase::MigrateAbort => 8,
                _ => 0,
            },
            flags: 0,
            cell_id: cell_id.0,
            old_generation: old_generation.0,
            new_generation: new_generation.0,
            buffer_ptr,
            buffer_capacity,
            buffer_len,
            status: 0,
            reserved: 0,
        }
    }
}

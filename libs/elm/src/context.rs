//! ELM 生命周期上下文和当前执行上下文。

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::ids::{ElmId, Generation};
use crate::kind::ElmKind;
use crate::state::ElmState;

const ELM_CONTEXT_ALLOWED_ACTIONS_ALL: u32 = (1 << 9) - 1;

pub const ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION: u16 = 1;
pub const ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION: u16 = 1;
pub const ELM_CONTEXT_MAX_CPUS: usize = 8;
pub const ELM_CONTEXT_MAX_DEPTH: usize = 16;

const ELM_CONTEXT_SLOT_COUNT: usize = ELM_CONTEXT_MAX_CPUS * ELM_CONTEXT_MAX_DEPTH;

type CurrentCpuIdFn = fn() -> usize;

/// 由上层运行时注入的任务级当前上下文存储。
///
/// `elm` crate 不依赖调度器；内核通过这张静态表把上下文绑定到当前任务。未注册时
/// 保留按 CPU 的固定栈，仅供独立 crate 测试和不带调度器的宿主使用。
pub struct ElmCurrentContextOps {
    pub enter: fn(ElmCurrentContext) -> Option<u64>,
    pub leave: fn(u64),
    pub current: fn() -> Option<ElmCurrentContext>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElmLifecyclePhase {
    Initialize,
    Finalize,
    Quiesce,
    Pause,
    Resume,
    MigrateExport,
    MigrateImport,
    MigrateAbort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    pub const fn cell_id(&self) -> ElmId {
        self.cell_id
    }

    pub const fn parent_id(&self) -> Option<ElmId> {
        self.parent_id
    }

    pub const fn generation(&self) -> Generation {
        self.generation
    }

    pub const fn state(&self) -> ElmState {
        self.state
    }

    pub const fn phase(&self) -> ElmLifecyclePhase {
        self.phase
    }

    pub const fn kind(&self) -> ElmKind {
        self.kind
    }

    pub const fn flags(&self) -> u32 {
        self.flags
    }

    pub const fn allowed_actions(&self) -> u32 {
        self.allowed_actions
    }

    pub const fn with_allowed_actions(mut self, allowed_actions: u32) -> Self {
        self.allowed_actions = allowed_actions;
        self
    }

    pub const fn with_kind(mut self, kind: ElmKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn set_state(&mut self, state: ElmState) {
        self.state = state;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmCurrentContext {
    pub cell_id: ElmId,
    pub parent_id: Option<ElmId>,
    pub generation: Generation,
    pub state: ElmState,
    pub phase: ElmLifecyclePhase,
    pub kind: ElmKind,
    pub flags: u32,
    pub allowed_actions: u32,
}

impl ElmCurrentContext {
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
pub struct ElmCurrentContextGuard {
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

pub fn enter_current_context(context: &ElmContext) -> Option<ElmCurrentContextGuard> {
    try_enter_current_context(context)
}

pub fn try_enter_current_context(context: &ElmContext) -> Option<ElmCurrentContextGuard> {
    if let Some(ops) = current_context_ops() {
        let backend_token = (ops.enter)(ElmCurrentContext::from_context(context))?;
        return Some(ElmCurrentContextGuard {
            cpu_id: 0,
            depth: 0,
            backend_token,
            task_backed: true,
        });
    }
    let cpu_id = current_cpu_id();
    let depth = CURRENT_DEPTH[cpu_id].load(Ordering::Acquire);
    if depth >= ELM_CONTEXT_MAX_DEPTH {
        return None;
    }
    store_context_slot(
        context_slot(cpu_id, depth),
        ElmCurrentContext::from_context(context),
    );
    CURRENT_DEPTH[cpu_id].store(depth + 1, Ordering::Release);
    Some(ElmCurrentContextGuard {
        cpu_id,
        depth,
        backend_token: 0,
        task_backed: false,
    })
}

pub fn current_context() -> Option<ElmCurrentContext> {
    if let Some(ops) = current_context_ops() {
        return (ops.current)();
    }
    let cpu_id = current_cpu_id();
    let depth = CURRENT_DEPTH[cpu_id].load(Ordering::Acquire);
    if depth == 0 || depth > ELM_CONTEXT_MAX_DEPTH {
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

pub fn current_cell() -> Option<ElmId> {
    current_context().map(|context| context.cell_id)
}

pub fn register_current_cpu_id(resolver: CurrentCpuIdFn) -> bool {
    CURRENT_CPU_ID_FN
        .compare_exchange(0, resolver as usize, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// 注册任务级上下文后端。运行时只能在首次进入 ELM 之前注册一次。
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
pub struct ElmNativeHookContextV1 {
    pub abi_version: u16,
    pub phase: u16,
    pub flags: u32,
    pub cell_id: u64,
    pub parent_id: u64,
    pub generation: u64,
    pub state: u32,
    pub reserved: u32,
}

impl ElmNativeHookContextV1 {
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
pub struct ElmNativeMigrationContextV1 {
    pub abi_version: u16,
    pub phase: u16,
    pub flags: u32,
    pub cell_id: u64,
    pub old_generation: u64,
    pub new_generation: u64,
    pub buffer_ptr: u64,
    pub buffer_capacity: u64,
    pub buffer_len: u64,
    pub status: i32,
    pub reserved: u32,
}

impl ElmNativeMigrationContextV1 {
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

//! ELM 原生调用的任务级执行域与故障恢复边界。
//!
//! 执行状态挂在当前 [`sched::Task`] 上，因此原生 ELM 可以在内核 API 中阻塞、被调度
//! 并迁移到其它 CPU。trap 热路径通过 Task 内发布的只读裸指针无锁取得状态；指针所有权
//! 始终由 `TASKEXT_ELM_EXECUTION` 对应的 `Arc` 持有。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use elm_model::{
    ELM_CONTEXT_MAX_DEPTH, ElmCurrentContext, ElmCurrentContextOps, register_current_context_ops,
};
use sched::sync::Spinlock;

pub const ELM_GUARD_PHASE_NONE: u32 = 0;
pub const ELM_GUARD_PHASE_HOOK: u32 = 1;
pub const ELM_GUARD_PHASE_MIGRATION: u32 = 2;
pub const ELM_GUARD_PHASE_ENTRY: u32 = 3;
pub const ELM_GUARD_PHASE_PROVIDER_CALL: u32 = 4;
pub const ELM_GUARD_PHASE_PROVIDER_SNAPSHOT: u32 = 5;
pub const ELM_GUARD_PHASE_MANAGED_CALL: u32 = 6;
pub const ELM_GUARD_PHASE_DEVICE_MATCH: u32 = 7;
pub const ELM_GUARD_PHASE_DEVICE_PROBE: u32 = 8;
pub const ELM_GUARD_PHASE_DEVICE_REMOVE: u32 = 9;
pub const ELM_GUARD_PHASE_DEVICE_IO: u32 = 10;
pub const ELM_GUARD_PHASE_DEVICE_IRQ: u32 = 11;
pub const ELM_GUARD_PHASE_DEVICE_DISCOVERY: u32 = 12;

pub const ELM_GUARD_ABORT_NONE: usize = 0;
pub const ELM_GUARD_ABORT_CANCEL: usize = 1;
pub const ELM_GUARD_ABORT_TIMEOUT: usize = 2;
pub const ELM_GUARD_ABORT_TRAP: usize = 3;
pub const ELM_GUARD_ABORT_PANIC: usize = 4;

pub const ELM_GUARD_MAX_DEPTH: usize = 16;
pub const ELM_GUARD_MAX_HOST_RANGES: usize = 4;
pub const ELM_GUARD_FAULT_RING_PER_CPU: usize = 16;

const ELM_GUARD_MAX_CPUS: usize = sched::NR_CPUS;
const ELM_GUARD_FAULT_SLOT_COUNT: usize = ELM_GUARD_MAX_CPUS * ELM_GUARD_FAULT_RING_PER_CPU;

/// 原生 ELM fault 恢复时写入 ABI 返回寄存器的通用错误值。
pub const ELM_GUARD_NATIVE_FAULT_RETURN: usize = (-4098isize) as usize;

/// 当前原生调用所处的可信边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum ElmExecutionDomain {
    Runtime = 1,
    ElmCode = 2,
    KernelCall = 3,
    Interrupt = 4,
}

impl ElmExecutionDomain {
    fn from_raw(raw: usize) -> Option<Self> {
        match raw {
            1 => Some(Self::Runtime),
            2 => Some(Self::ElmCode),
            3 => Some(Self::KernelCall),
            4 => Some(Self::Interrupt),
            _ => None,
        }
    }
}

const fn domain_transition_allowed(current: ElmExecutionDomain, next: ElmExecutionDomain) -> bool {
    // top-half 运行时不能进入可能持锁、分配或调度的普通内核 API。中断回调只允许
    // 操作预先映射的设备寄存器和自身内存；复杂工作必须提交给延后回调。
    !matches!(current, ElmExecutionDomain::Interrupt)
        || matches!(next, ElmExecutionDomain::Interrupt)
}

struct ElmGuardFrame {
    cell: AtomicU64,
    phase: AtomicUsize,
    deadline_ns: AtomicU64,
    abort_reason: AtomicUsize,
    recovery_pc: AtomicUsize,
    recovery_sp: AtomicUsize,
    domain: AtomicUsize,
    code_start: AtomicUsize,
    code_end: AtomicUsize,
    image_start: AtomicUsize,
    image_end: AtomicUsize,
    stack_start: AtomicUsize,
    stack_end: AtomicUsize,
    host_range_count: AtomicUsize,
    host_start: [AtomicUsize; ELM_GUARD_MAX_HOST_RANGES],
    host_end: [AtomicUsize; ELM_GUARD_MAX_HOST_RANGES],
}

impl ElmGuardFrame {
    const fn new() -> Self {
        Self {
            cell: AtomicU64::new(0),
            phase: AtomicUsize::new(ELM_GUARD_PHASE_NONE as usize),
            deadline_ns: AtomicU64::new(0),
            abort_reason: AtomicUsize::new(ELM_GUARD_ABORT_NONE),
            recovery_pc: AtomicUsize::new(0),
            recovery_sp: AtomicUsize::new(0),
            domain: AtomicUsize::new(0),
            code_start: AtomicUsize::new(0),
            code_end: AtomicUsize::new(0),
            image_start: AtomicUsize::new(0),
            image_end: AtomicUsize::new(0),
            stack_start: AtomicUsize::new(0),
            stack_end: AtomicUsize::new(0),
            host_range_count: AtomicUsize::new(0),
            host_start: [const { AtomicUsize::new(0) }; ELM_GUARD_MAX_HOST_RANGES],
            host_end: [const { AtomicUsize::new(0) }; ELM_GUARD_MAX_HOST_RANGES],
        }
    }

    fn clear(&self) {
        self.recovery_pc.store(0, Ordering::Release);
        self.recovery_sp.store(0, Ordering::Release);
        self.abort_reason
            .store(ELM_GUARD_ABORT_NONE, Ordering::Release);
        self.deadline_ns.store(0, Ordering::Release);
        self.domain.store(0, Ordering::Release);
        self.code_start.store(0, Ordering::Release);
        self.code_end.store(0, Ordering::Release);
        self.image_start.store(0, Ordering::Release);
        self.image_end.store(0, Ordering::Release);
        self.stack_start.store(0, Ordering::Release);
        self.stack_end.store(0, Ordering::Release);
        self.host_range_count.store(0, Ordering::Release);
        for index in 0..ELM_GUARD_MAX_HOST_RANGES {
            self.host_start[index].store(0, Ordering::Release);
            self.host_end[index].store(0, Ordering::Release);
        }
        self.phase
            .store(ELM_GUARD_PHASE_NONE as usize, Ordering::Release);
        self.cell.store(0, Ordering::Release);
    }
}

struct ElmContextStack {
    depth: usize,
    entries: [Option<ElmCurrentContext>; ELM_CONTEXT_MAX_DEPTH],
}

impl ElmContextStack {
    const fn new() -> Self {
        Self {
            depth: 0,
            entries: [None; ELM_CONTEXT_MAX_DEPTH],
        }
    }
}

/// 单个任务拥有的完整 ELM 执行状态。
pub struct ElmTaskExecutionState {
    guard_depth: AtomicUsize,
    frames: [ElmGuardFrame; ELM_GUARD_MAX_DEPTH],
    contexts: Spinlock<ElmContextStack>,
    registered: AtomicBool,
}

impl ElmTaskExecutionState {
    pub const fn new() -> Self {
        Self {
            guard_depth: AtomicUsize::new(0),
            frames: [const { ElmGuardFrame::new() }; ELM_GUARD_MAX_DEPTH],
            contexts: Spinlock::new(ElmContextStack::new()),
            registered: AtomicBool::new(false),
        }
    }

    fn current_frame(&self) -> Option<(usize, &ElmGuardFrame)> {
        let depth = self.guard_depth.load(Ordering::Acquire);
        if depth == 0 || depth > ELM_GUARD_MAX_DEPTH {
            None
        } else {
            Some((depth - 1, &self.frames[depth - 1]))
        }
    }

    fn push_context(&self, context: ElmCurrentContext) -> Option<u64> {
        let mut stack = self.contexts.lock();
        if stack.depth >= ELM_CONTEXT_MAX_DEPTH {
            return None;
        }
        let depth = stack.depth;
        stack.entries[depth] = Some(context);
        stack.depth = depth + 1;
        Some((depth + 1) as u64)
    }

    fn pop_context(&self, token: u64) {
        let Ok(expected_depth) = usize::try_from(token) else {
            return;
        };
        let mut stack = self.contexts.lock();
        debug_assert_eq!(
            stack.depth, expected_depth,
            "ELM 当前上下文必须按栈顺序退出"
        );
        if expected_depth == 0 || stack.depth != expected_depth {
            return;
        }
        stack.entries[expected_depth - 1] = None;
        stack.depth -= 1;
    }

    fn current_context(&self) -> Option<ElmCurrentContext> {
        let stack = self.contexts.lock();
        stack
            .depth
            .checked_sub(1)
            .and_then(|index| stack.entries[index])
    }
}

impl Default for ElmTaskExecutionState {
    fn default() -> Self {
        Self::new()
    }
}

struct RegisteredExecutionState {
    state: Weak<ElmTaskExecutionState>,
    task: Weak<sched::Task>,
}

static ACTIVE_STATES: Spinlock<Vec<RegisteredExecutionState>> = Spinlock::new(Vec::new());

static LAST_FAULT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static FAULT_RING_NEXT: [AtomicUsize; ELM_GUARD_MAX_CPUS] =
    [const { AtomicUsize::new(0) }; ELM_GUARD_MAX_CPUS];
static FAULT_RING_SEQUENCE: [AtomicU64; ELM_GUARD_FAULT_SLOT_COUNT] =
    [const { AtomicU64::new(0) }; ELM_GUARD_FAULT_SLOT_COUNT];
static FAULT_RING_DEPTH: [AtomicUsize; ELM_GUARD_FAULT_SLOT_COUNT] =
    [const { AtomicUsize::new(0) }; ELM_GUARD_FAULT_SLOT_COUNT];
static FAULT_RING_CELL: [AtomicU64; ELM_GUARD_FAULT_SLOT_COUNT] =
    [const { AtomicU64::new(0) }; ELM_GUARD_FAULT_SLOT_COUNT];
static FAULT_RING_PHASE: [AtomicUsize; ELM_GUARD_FAULT_SLOT_COUNT] =
    [const { AtomicUsize::new(ELM_GUARD_PHASE_NONE as usize) }; ELM_GUARD_FAULT_SLOT_COUNT];
static FAULT_RING_REASON: [AtomicUsize; ELM_GUARD_FAULT_SLOT_COUNT] =
    [const { AtomicUsize::new(ELM_GUARD_ABORT_NONE) }; ELM_GUARD_FAULT_SLOT_COUNT];
static FAULT_RING_PC: [AtomicUsize; ELM_GUARD_FAULT_SLOT_COUNT] =
    [const { AtomicUsize::new(0) }; ELM_GUARD_FAULT_SLOT_COUNT];
static FAULT_RING_ADDR: [AtomicUsize; ELM_GUARD_FAULT_SLOT_COUNT] =
    [const { AtomicUsize::new(0) }; ELM_GUARD_FAULT_SLOT_COUNT];
static FAULT_RING_CODE: [AtomicUsize; ELM_GUARD_FAULT_SLOT_COUNT] =
    [const { AtomicUsize::new(0) }; ELM_GUARD_FAULT_SLOT_COUNT];
static FAULT_RING_RETURN_PC: [AtomicUsize; ELM_GUARD_FAULT_SLOT_COUNT] =
    [const { AtomicUsize::new(0) }; ELM_GUARD_FAULT_SLOT_COUNT];
static FAULT_RING_RETURN_SP: [AtomicUsize; ELM_GUARD_FAULT_SLOT_COUNT] =
    [const { AtomicUsize::new(0) }; ELM_GUARD_FAULT_SLOT_COUNT];
static FAULT_RING_DROPPED: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmTrapRecovery {
    pub cell: u64,
    pub phase: u32,
    pub reason: usize,
    pub return_pc: usize,
    pub return_sp: usize,
    pub return_value: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmGuardFaultSnapshot {
    pub sequence: u64,
    pub cpu_id: u32,
    pub depth: u32,
    pub cell: u64,
    pub phase: u32,
    pub reason: usize,
    pub pc: usize,
    pub addr: usize,
    pub code: usize,
    pub return_pc: usize,
    pub return_sp: usize,
}

/// 当前原生 ELM 调用的可执行镜像边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmNativeBounds {
    pub code_start: usize,
    pub code_end: usize,
    pub image_start: usize,
    pub image_end: usize,
}

pub struct ElmGuard {
    state: Arc<ElmTaskExecutionState>,
    depth: usize,
    cell: u64,
    entry_cpu: usize,
}

impl ElmGuard {
    pub fn enter(cell: u64, phase: u32, deadline_ns: u64) -> Option<Self> {
        if cell == 0 || phase == ELM_GUARD_PHASE_NONE {
            return None;
        }
        let state = ensure_current_state()?;
        let depth = state.guard_depth.load(Ordering::Acquire);
        if depth >= ELM_GUARD_MAX_DEPTH {
            return None;
        }
        let frame = &state.frames[depth];
        frame.phase.store(phase as usize, Ordering::Release);
        frame.deadline_ns.store(deadline_ns, Ordering::Release);
        frame
            .abort_reason
            .store(ELM_GUARD_ABORT_NONE, Ordering::Release);
        frame
            .domain
            .store(ElmExecutionDomain::Runtime as usize, Ordering::Release);
        frame.cell.store(cell, Ordering::Release);
        state.guard_depth.store(depth + 1, Ordering::Release);
        Some(Self {
            state,
            depth,
            cell,
            entry_cpu: current_cpu_id(),
        })
    }

    pub fn configure_native_bounds(
        &self,
        code_start: usize,
        code_end: usize,
        image_start: usize,
        image_end: usize,
        stack_start: usize,
        stack_end: usize,
        host_ranges: &[(usize, usize)],
    ) -> bool {
        if code_start == 0
            || code_start >= code_end
            || image_start == 0
            || image_start >= image_end
            || code_start < image_start
            || code_end > image_end
            || stack_start == 0
            || stack_start >= stack_end
            || host_ranges.len() > ELM_GUARD_MAX_HOST_RANGES
            || self.state.guard_depth.load(Ordering::Acquire) != self.depth + 1
        {
            return false;
        }
        let frame = &self.state.frames[self.depth];
        for (index, &(start, end)) in host_ranges.iter().enumerate() {
            if start == 0 || start >= end {
                return false;
            }
            frame.host_start[index].store(start, Ordering::Relaxed);
            frame.host_end[index].store(end, Ordering::Relaxed);
        }
        frame
            .host_range_count
            .store(host_ranges.len(), Ordering::Release);
        frame.code_start.store(code_start, Ordering::Relaxed);
        frame.code_end.store(code_end, Ordering::Relaxed);
        frame.image_start.store(image_start, Ordering::Relaxed);
        frame.image_end.store(image_end, Ordering::Relaxed);
        frame.stack_start.store(stack_start, Ordering::Relaxed);
        frame.stack_end.store(stack_end, Ordering::Release);
        true
    }

    pub fn enter_domain(&self, domain: ElmExecutionDomain) -> Option<ElmExecutionDomainGuard> {
        if self.state.guard_depth.load(Ordering::Acquire) != self.depth + 1 {
            return None;
        }
        let frame = &self.state.frames[self.depth];
        let previous = frame.domain.load(Ordering::Acquire);
        let current = ElmExecutionDomain::from_raw(previous)?;
        if !domain_transition_allowed(current, domain) {
            return None;
        }
        frame.domain.store(domain as usize, Ordering::Release);
        Some(ElmExecutionDomainGuard {
            state: Arc::clone(&self.state),
            depth: self.depth,
            previous,
        })
    }

    pub fn abort_reason(&self) -> usize {
        self.state.frames[self.depth]
            .abort_reason
            .load(Ordering::Acquire)
    }

    pub fn aborted(&self) -> bool {
        self.abort_reason() != ELM_GUARD_ABORT_NONE
    }

    pub const fn cpu_id(&self) -> usize {
        self.entry_cpu
    }

    pub const fn depth(&self) -> usize {
        self.depth
    }
}

impl Drop for ElmGuard {
    fn drop(&mut self) {
        let current = self.state.guard_depth.load(Ordering::Acquire);
        let frame = &self.state.frames[self.depth];
        debug_assert_eq!(current, self.depth + 1, "ELM guard 必须按栈顺序退出");
        debug_assert_eq!(frame.cell.load(Ordering::Acquire), self.cell);
        if current != self.depth + 1 {
            return;
        }
        frame.clear();
        self.state.guard_depth.store(self.depth, Ordering::Release);
    }
}

pub struct ElmExecutionDomainGuard {
    state: Arc<ElmTaskExecutionState>,
    depth: usize,
    previous: usize,
}

impl Drop for ElmExecutionDomainGuard {
    fn drop(&mut self) {
        if self.state.guard_depth.load(Ordering::Acquire) == self.depth + 1 {
            self.state.frames[self.depth]
                .domain
                .store(self.previous, Ordering::Release);
        }
    }
}

/// 注册 `elm` crate 使用的任务级上下文后端。
pub fn register_task_context_backend() -> bool {
    register_current_context_ops(&TASK_CONTEXT_OPS)
}

pub fn enter_current_domain(domain: ElmExecutionDomain) -> Option<ElmExecutionDomainGuard> {
    let state = current_state_arc()?;
    let (depth, frame) = state.current_frame()?;
    let previous = frame.domain.load(Ordering::Acquire);
    let current = ElmExecutionDomain::from_raw(previous)?;
    if !domain_transition_allowed(current, domain) {
        return None;
    }
    frame.domain.store(domain as usize, Ordering::Release);
    Some(ElmExecutionDomainGuard {
        state,
        depth,
        previous,
    })
}

pub fn current_execution_domain() -> Option<ElmExecutionDomain> {
    let state = current_state_ref()?;
    let (_, frame) = state.current_frame()?;
    ElmExecutionDomain::from_raw(frame.domain.load(Ordering::Acquire))
}

pub fn active_cell() -> u64 {
    current_state_ref()
        .and_then(|state| state.current_frame().map(|(_, frame)| frame))
        .map(|frame| frame.cell.load(Ordering::Acquire))
        .unwrap_or(0)
}

pub fn active_phase() -> u32 {
    current_state_ref()
        .and_then(|state| state.current_frame().map(|(_, frame)| frame))
        .map(|frame| frame.phase.load(Ordering::Acquire) as u32)
        .unwrap_or(ELM_GUARD_PHASE_NONE)
}

pub fn active_deadline_ns() -> u64 {
    current_state_ref()
        .and_then(|state| state.current_frame().map(|(_, frame)| frame))
        .map(|frame| frame.deadline_ns.load(Ordering::Acquire))
        .unwrap_or(0)
}

pub fn request_abort(cell: u64, reason: usize) -> bool {
    if cell == 0 || reason == ELM_GUARD_ABORT_NONE {
        return false;
    }
    let mut requested = false;
    let mut registry = ACTIVE_STATES.lock();
    registry.retain(|entry| entry.state.strong_count() != 0 && entry.task.strong_count() != 0);
    for entry in registry.iter() {
        let Some(state) = entry.state.upgrade() else {
            continue;
        };
        let depth = state
            .guard_depth
            .load(Ordering::Acquire)
            .min(ELM_GUARD_MAX_DEPTH);
        let mut matched = false;
        for frame in (0..depth).rev() {
            if state.frames[frame].cell.load(Ordering::Acquire) == cell {
                state.frames[frame]
                    .abort_reason
                    .store(reason, Ordering::Release);
                requested = true;
                matched = true;
            }
        }
        if matched && let Some(task) = entry.task.upgrade() {
            sched::request_resched(task.current_cpu().min(ELM_GUARD_MAX_CPUS - 1));
        }
    }
    requested
}

pub fn request_timeout_if_expired(now_ns: u64) -> bool {
    let Some(state) = current_state_ref() else {
        return false;
    };
    let Some((_, frame)) = state.current_frame() else {
        return false;
    };
    let deadline = frame.deadline_ns.load(Ordering::Acquire);
    if deadline == 0 || now_ns < deadline {
        return false;
    }
    frame
        .abort_reason
        .store(ELM_GUARD_ABORT_TIMEOUT, Ordering::Release);
    true
}

pub fn request_trap_recovery(cell: u64) -> bool {
    request_abort(cell, ELM_GUARD_ABORT_TRAP)
}

pub fn request_panic_recovery(cell: u64) -> bool {
    request_abort(cell, ELM_GUARD_ABORT_PANIC)
}

/// 为当前保护栈顶登记架构调用门的受控恢复出口。
pub fn arm_current_recovery(return_pc: usize, return_sp: usize) -> bool {
    if return_pc == 0 || return_sp == 0 || return_sp & 0xf != 0 {
        return false;
    }
    let Some(state) = current_state_ref() else {
        return false;
    };
    let Some((_, frame)) = state.current_frame() else {
        return false;
    };
    if frame.code_start.load(Ordering::Acquire) == 0
        || frame.stack_end.load(Ordering::Acquire) == 0
        || frame.recovery_pc.load(Ordering::Acquire) != 0
    {
        return false;
    }
    frame.recovery_sp.store(return_sp, Ordering::Release);
    frame.recovery_pc.store(return_pc, Ordering::Release);
    true
}

pub fn disarm_current_recovery() -> bool {
    let Some(state) = current_state_ref() else {
        return false;
    };
    let Some((_, frame)) = state.current_frame() else {
        return false;
    };
    let armed = frame.recovery_pc.swap(0, Ordering::AcqRel) != 0;
    frame.recovery_sp.store(0, Ordering::Release);
    armed
}

pub fn try_recover_kernel_fault(
    fault_pc: usize,
    fault_addr: usize,
    fault_code: usize,
) -> Option<ElmTrapRecovery> {
    let state = current_state_ref()?;
    let (_, frame) = state.current_frame()?;
    if !matches!(
        ElmExecutionDomain::from_raw(frame.domain.load(Ordering::Acquire)),
        Some(ElmExecutionDomain::ElmCode | ElmExecutionDomain::Interrupt)
    ) || !pc_in_current_code(frame, fault_pc)
    {
        return None;
    }
    consume_current_recovery(
        state,
        ELM_GUARD_ABORT_TRAP,
        fault_pc,
        fault_addr,
        fault_code,
    )
}

/// timer/IRQ 返回前尝试落实已经投递的取消或超时。
///
/// 只有中断现场位于 ELM 可执行段且执行域为普通代码或 top-half 时才允许改写
/// trap frame。
pub fn try_recover_requested_abort(interrupted_pc: usize) -> Option<ElmTrapRecovery> {
    let state = current_state_ref()?;
    let (_, frame) = state.current_frame()?;
    let reason = frame.abort_reason.load(Ordering::Acquire);
    if !matches!(reason, ELM_GUARD_ABORT_CANCEL | ELM_GUARD_ABORT_TIMEOUT)
        || !matches!(
            ElmExecutionDomain::from_raw(frame.domain.load(Ordering::Acquire)),
            Some(ElmExecutionDomain::ElmCode | ElmExecutionDomain::Interrupt)
        )
        || !pc_in_current_code(frame, interrupted_pc)
    {
        return None;
    }
    consume_current_recovery(state, reason, interrupted_pc, 0, reason)
}

/// 显式 `elmapi::abort_current` 使用的恢复出口。
pub fn try_recover_explicit_abort(reason: usize) -> Option<ElmTrapRecovery> {
    if !matches!(
        reason,
        ELM_GUARD_ABORT_CANCEL | ELM_GUARD_ABORT_TIMEOUT | ELM_GUARD_ABORT_PANIC
    ) {
        return None;
    }
    let state = current_state_ref()?;
    consume_current_recovery(state, reason, 0, 0, reason)
}

/// 验证原生 ELM 交给运行时的内存范围。
pub fn validate_current_memory_range(address: usize, len: usize, write: bool) -> bool {
    if len == 0 {
        return true;
    }
    let Some(end) = address.checked_add(len) else {
        return false;
    };
    if address == 0 || end <= address {
        return false;
    }
    let Some(state) = current_state_ref() else {
        return false;
    };
    let Some((_, frame)) = state.current_frame() else {
        return false;
    };
    if range_within(
        address,
        end,
        frame.stack_start.load(Ordering::Acquire),
        frame.stack_end.load(Ordering::Acquire),
    ) {
        return true;
    }
    let host_count = frame
        .host_range_count
        .load(Ordering::Acquire)
        .min(ELM_GUARD_MAX_HOST_RANGES);
    for index in 0..host_count {
        if range_within(
            address,
            end,
            frame.host_start[index].load(Ordering::Acquire),
            frame.host_end[index].load(Ordering::Acquire),
        ) {
            return true;
        }
    }
    if range_within(
        address,
        end,
        frame.image_start.load(Ordering::Acquire),
        frame.image_end.load(Ordering::Acquire),
    ) {
        return crate::elm_image::validate_elm_image_range(address, len, true, write, false);
    }
    let cell = frame.cell.load(Ordering::Acquire);
    allocator::KERNEL_ALLOCATOR
        .query_containing_allocation(address, len)
        .is_ok_and(|record| record.accounting_owner() == cell)
}

/// 返回当前 ELM 调用已经由装载器验证的代码和镜像边界。
pub fn current_native_bounds() -> Option<ElmNativeBounds> {
    let state = current_state_ref()?;
    let (_, frame) = state.current_frame()?;
    let bounds = ElmNativeBounds {
        code_start: frame.code_start.load(Ordering::Acquire),
        code_end: frame.code_end.load(Ordering::Acquire),
        image_start: frame.image_start.load(Ordering::Acquire),
        image_end: frame.image_end.load(Ordering::Acquire),
    };
    if bounds.code_start == 0
        || bounds.code_start >= bounds.code_end
        || bounds.image_start == 0
        || bounds.image_start >= bounds.image_end
        || bounds.code_start < bounds.image_start
        || bounds.code_end > bounds.image_end
    {
        return None;
    }
    Some(bounds)
}

/// 验证一个回调入口是否落在当前 ELM 已封闭的可执行段。
pub fn validate_current_code_address(address: usize) -> bool {
    current_native_bounds().is_some_and(|bounds| {
        address >= bounds.code_start
            && address < bounds.code_end
            && crate::elm_image::validate_elm_image_range(address, 1, true, false, true)
    })
}

fn consume_current_recovery(
    state: &ElmTaskExecutionState,
    reason: usize,
    fault_pc: usize,
    fault_addr: usize,
    fault_code: usize,
) -> Option<ElmTrapRecovery> {
    let (depth_index, frame) = state.current_frame()?;
    let cell = frame.cell.load(Ordering::Acquire);
    let phase = frame.phase.load(Ordering::Acquire);
    if cell == 0 || phase == ELM_GUARD_PHASE_NONE as usize {
        return None;
    }
    let return_pc = frame.recovery_pc.swap(0, Ordering::AcqRel);
    let return_sp = frame.recovery_sp.swap(0, Ordering::AcqRel);
    if return_pc == 0 || return_sp == 0 || return_sp & 0xf != 0 {
        return None;
    }
    frame.abort_reason.store(reason, Ordering::Release);
    let sequence =
        match LAST_FAULT_SEQUENCE.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        }) {
            Ok(previous) => previous + 1,
            Err(_) => {
                FAULT_RING_DROPPED.fetch_add(1, Ordering::Relaxed);
                0
            }
        };
    if sequence != 0 {
        push_fault_snapshot(
            sequence,
            current_cpu_id(),
            depth_index + 1,
            cell,
            phase,
            reason,
            fault_pc,
            fault_addr,
            fault_code,
            return_pc,
            return_sp,
        );
    }
    Some(ElmTrapRecovery {
        cell,
        phase: phase as u32,
        reason,
        return_pc,
        return_sp,
        return_value: ELM_GUARD_NATIVE_FAULT_RETURN,
    })
}

pub fn last_fault_snapshot() -> Option<ElmGuardFaultSnapshot> {
    let mut selected = None;
    visit_fault_snapshots(|snapshot| {
        if selected
            .is_none_or(|current: ElmGuardFaultSnapshot| snapshot.sequence > current.sequence)
        {
            selected = Some(snapshot);
        }
    });
    selected
}

pub fn visit_fault_snapshots(mut visitor: impl FnMut(ElmGuardFaultSnapshot)) {
    for slot in 0..ELM_GUARD_FAULT_SLOT_COUNT {
        if let Some(snapshot) = read_fault_snapshot(slot) {
            visitor(snapshot);
        }
    }
}

pub fn fault_snapshot_count() -> usize {
    let mut count = 0usize;
    visit_fault_snapshots(|_| count += 1);
    count
}

pub fn dropped_fault_snapshot_count() -> u64 {
    FAULT_RING_DROPPED.load(Ordering::Acquire)
}

#[allow(clippy::too_many_arguments)]
fn push_fault_snapshot(
    sequence: u64,
    cpu_id: usize,
    depth: usize,
    cell: u64,
    phase: usize,
    reason: usize,
    pc: usize,
    addr: usize,
    code: usize,
    return_pc: usize,
    return_sp: usize,
) {
    let position = FAULT_RING_NEXT[cpu_id].fetch_add(1, Ordering::AcqRel);
    let slot = fault_slot(cpu_id, position % ELM_GUARD_FAULT_RING_PER_CPU);
    if FAULT_RING_SEQUENCE[slot].load(Ordering::Acquire) != 0 {
        FAULT_RING_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    FAULT_RING_SEQUENCE[slot].store(0, Ordering::Release);
    FAULT_RING_DEPTH[slot].store(depth, Ordering::Relaxed);
    FAULT_RING_CELL[slot].store(cell, Ordering::Relaxed);
    FAULT_RING_PHASE[slot].store(phase, Ordering::Relaxed);
    FAULT_RING_REASON[slot].store(reason, Ordering::Relaxed);
    FAULT_RING_PC[slot].store(pc, Ordering::Relaxed);
    FAULT_RING_ADDR[slot].store(addr, Ordering::Relaxed);
    FAULT_RING_CODE[slot].store(code, Ordering::Relaxed);
    FAULT_RING_RETURN_PC[slot].store(return_pc, Ordering::Relaxed);
    FAULT_RING_RETURN_SP[slot].store(return_sp, Ordering::Relaxed);
    FAULT_RING_SEQUENCE[slot].store(sequence, Ordering::Release);
}

fn read_fault_snapshot(slot: usize) -> Option<ElmGuardFaultSnapshot> {
    let sequence = FAULT_RING_SEQUENCE[slot].load(Ordering::Acquire);
    if sequence == 0 {
        return None;
    }
    let snapshot = ElmGuardFaultSnapshot {
        sequence,
        cpu_id: (slot / ELM_GUARD_FAULT_RING_PER_CPU) as u32,
        depth: FAULT_RING_DEPTH[slot].load(Ordering::Relaxed) as u32,
        cell: FAULT_RING_CELL[slot].load(Ordering::Relaxed),
        phase: FAULT_RING_PHASE[slot].load(Ordering::Relaxed) as u32,
        reason: FAULT_RING_REASON[slot].load(Ordering::Relaxed),
        pc: FAULT_RING_PC[slot].load(Ordering::Relaxed),
        addr: FAULT_RING_ADDR[slot].load(Ordering::Relaxed),
        code: FAULT_RING_CODE[slot].load(Ordering::Relaxed),
        return_pc: FAULT_RING_RETURN_PC[slot].load(Ordering::Relaxed),
        return_sp: FAULT_RING_RETURN_SP[slot].load(Ordering::Relaxed),
    };
    if FAULT_RING_SEQUENCE[slot].load(Ordering::Acquire) == sequence {
        Some(snapshot)
    } else {
        None
    }
}

fn ensure_current_state() -> Option<Arc<ElmTaskExecutionState>> {
    let task = sched::current_task_fast();
    let state = if let Some(payload) = task.ext_lookup(sched::TASKEXT_ELM_EXECUTION) {
        payload.downcast::<ElmTaskExecutionState>().ok()?
    } else {
        let state = Arc::new(ElmTaskExecutionState::new());
        task.ext_install(sched::TASKEXT_ELM_EXECUTION, state.clone());
        state
    };
    if !state.registered.swap(true, Ordering::AcqRel) {
        let mut registry = ACTIVE_STATES.lock();
        registry.retain(|entry| entry.state.strong_count() != 0 && entry.task.strong_count() != 0);
        if registry.try_reserve(1).is_err() {
            state.registered.store(false, Ordering::Release);
            return None;
        }
        registry.push(RegisteredExecutionState {
            state: Arc::downgrade(&state),
            task: Arc::downgrade(&task),
        });
    }
    Some(state)
}

fn current_state_arc() -> Option<Arc<ElmTaskExecutionState>> {
    sched::current_task_fast()
        .ext_lookup(sched::TASKEXT_ELM_EXECUTION)?
        .downcast::<ElmTaskExecutionState>()
        .ok()
}

fn current_state_ref() -> Option<&'static ElmTaskExecutionState> {
    let raw = sched::current_task_ref().elm_execution_ptr();
    if raw == 0 {
        return None;
    }
    // 安全性：Task 先发布持有该对象的 Arc，再发布此地址；移除时先清地址再释放 Arc。
    Some(unsafe { &*(raw as *const ElmTaskExecutionState) })
}

fn task_context_enter(context: ElmCurrentContext) -> Option<u64> {
    ensure_current_state()?.push_context(context)
}

fn task_context_leave(token: u64) {
    if let Some(state) = current_state_ref() {
        state.pop_context(token);
    }
}

fn task_current_context() -> Option<ElmCurrentContext> {
    current_state_ref()?.current_context()
}

static TASK_CONTEXT_OPS: ElmCurrentContextOps = ElmCurrentContextOps {
    enter: task_context_enter,
    leave: task_context_leave,
    current: task_current_context,
};

fn pc_in_current_code(frame: &ElmGuardFrame, pc: usize) -> bool {
    let start = frame.code_start.load(Ordering::Acquire);
    let end = frame.code_end.load(Ordering::Acquire);
    start != 0 && pc >= start && pc < end
}

const fn range_within(start: usize, end: usize, owner_start: usize, owner_end: usize) -> bool {
    owner_start != 0 && owner_start <= start && end <= owner_end && owner_start < owner_end
}

fn current_cpu_id() -> usize {
    sched::current_cpu_id().min(ELM_GUARD_MAX_CPUS - 1)
}

const fn fault_slot(cpu_id: usize, index: usize) -> usize {
    cpu_id * ELM_GUARD_FAULT_RING_PER_CPU + index
}

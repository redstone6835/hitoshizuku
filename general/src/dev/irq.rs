//! 通用 IRQ 注册与分发层。
//!
//! 架构 trap 入口只知道“发生了哪条规范化中断线”，不应该理解具体设备。
//! 本模块提供一个小型共享 IRQ registry：设备驱动注册 [`IrqHandler`]，架构层
//! 在非 timer 中断到达时调用 [`dispatch_interrupt`]。真正的 IRQ domain/控制器
//! 驱动以后可以在此基础上把固件中断 specifier 翻译成 [`IrqLine`]。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use vfs::sync::Spinlock;

use super::registry_id;
use crate::Interrupt;
use crate::dev::pnp::{self, PnpDependency, PnpHandleResource, PnpResourceKind};

#[derive(Clone, Copy)]
pub struct IrqLineOps {
    /// 使能一条已经规范化的 IRQ line。
    ///
    /// 架构层通常只处理 [`IrqLine::Hardware`]；级联控制器自己的子线应由对应
    /// controller driver 在 demux/ack 逻辑里管理。
    pub enable: fn(IrqLine) -> bool,
    /// 禁用一条已经规范化的 IRQ line。
    pub disable: fn(IrqLine) -> bool,
}

#[derive(Clone, Copy)]
pub struct IocsrOps {
    pub read32: fn(usize) -> u32,
    pub write32: fn(usize, u32),
    pub read64: fn(usize) -> u64,
    pub write64: fn(usize, u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqLine {
    Ipi,
    Hardware(usize),
    /// interrupt-controller domain 内部的硬件线。
    ///
    /// 级联控制器（例如 PCH PIC）在固件 IRQ specifier 翻译阶段可以返回这个
    /// 规范化子线；其 demux handler 读取控制器 pending 状态后，再用同一个
    /// [`IrqLine`] 调 [`dispatch_irq_line`] 分发给设备驱动。这样设备驱动不需要
    /// 知道上游 CPU HWI 或控制器寄存器布局。
    Controller {
        controller: u32,
        hwirq: u32,
    },
    Other(usize),
}

impl IrqLine {
    pub const fn from_interrupt(interrupt: Interrupt) -> Option<Self> {
        match interrupt {
            Interrupt::Ipi => Some(Self::Ipi),
            Interrupt::Hardware(line) => Some(Self::Hardware(line)),
            Interrupt::Other(line) => Some(Self::Other(line)),
            // Timer 在架构入口有严格的 acknowledge/调度时钟语义，不走通用设备 IRQ registry。
            Interrupt::Timer => None,
            Interrupt::UserSoftware
            | Interrupt::SupervisorSoftware
            | Interrupt::MachineMode
            | Interrupt::UserTimer
            | Interrupt::SupervisorTimer
            | Interrupt::MachineTimer
            | Interrupt::UserExternal
            | Interrupt::SupervisorExternal
            | Interrupt::MachineExternal => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqStatus {
    Handled,
    Unhandled,
}

impl IrqStatus {
    pub const fn is_handled(self) -> bool {
        matches!(self, Self::Handled)
    }
}

pub trait IrqHandler: Send + Sync {
    /// 处理中断。
    ///
    /// 调用发生在 IRQ registry 的短临界区内，handler 必须快速返回，不能睡眠，
    /// 也不能在回调内注册或注销 IRQ handler。后续如果引入 RCU/epoch registry，
    /// 可以放宽这个约束。
    fn handle_irq(&self, line: IrqLine) -> IrqStatus;
}

/// IRQ 触发模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqTrigger {
    Edge,
    Level,
}

/// IRQ 有效电平/边沿极性。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqPolarity {
    High,
    Low,
}

/// IRQ handler 共享策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqSharing {
    /// 该 line 只能有一个 handler。
    Exclusive,
    /// 该 line 可以挂多个 handler，dispatch 时逐个调用。
    Shared,
}

/// IRQ bottom-half 回调。
///
/// 当前 registry 会先在锁外运行 top-half；当 top-half 返回 `Handled` 后，再同步
/// 调用该回调，用于把较重的设备侧收尾逻辑从 fast handler 里拆出来。控制器线的
/// mask/ack/unmask 生命周期由对应 [`IrqDomain`] 的 `set_line_enabled`、
/// `configure_line` 以及控制器驱动的 demux/ack 过程表达；如果后续接入可调度的
/// threaded IRQ，仍可复用当前 request 结构，仅替换 bottom-half 调度策略。
pub trait IrqBottomHalf: Send + Sync {
    fn run_bottom_half(&self, line: IrqLine);
}

struct ElmIrqHandlerProxy {
    context: elm_model::ElmCurrentContext,
    owner: &'static str,
    handler: Option<Arc<dyn IrqHandler>>,
}

impl IrqHandler for ElmIrqHandlerProxy {
    fn handle_irq(&self, line: IrqLine) -> IrqStatus {
        let Some(_guard) = super::pnp::enter_elm_snapshot(self.context) else {
            log::error!(
                "[irq] cannot enter ELM handler context: owner={} cell={} generation={}",
                self.owner,
                self.context.cell_id.0,
                self.context.generation.0,
            );
            super::elm_lifecycle::mark_context_failed(self.context);
            return IrqStatus::Handled;
        };
        self.handler
            .as_ref()
            .expect("ELM IRQ handler proxy used after drop")
            .handle_irq(line)
    }
}

impl Drop for ElmIrqHandlerProxy {
    fn drop(&mut self) {
        let Some(handler) = self.handler.take() else {
            return;
        };
        let Some(_guard) = super::pnp::enter_elm_snapshot(self.context) else {
            super::elm_lifecycle::mark_context_failed(self.context);
            core::mem::forget(handler);
            return;
        };
        drop(handler);
    }
}

struct ElmIrqBottomHalfProxy {
    context: elm_model::ElmCurrentContext,
    owner: &'static str,
    bottom_half: Option<Arc<dyn IrqBottomHalf>>,
}

impl IrqBottomHalf for ElmIrqBottomHalfProxy {
    fn run_bottom_half(&self, line: IrqLine) {
        let Some(_guard) = super::pnp::enter_elm_snapshot(self.context) else {
            log::error!(
                "[irq] cannot enter ELM bottom-half context: owner={} cell={} generation={}",
                self.owner,
                self.context.cell_id.0,
                self.context.generation.0,
            );
            super::elm_lifecycle::mark_context_failed(self.context);
            return;
        };
        self.bottom_half
            .as_ref()
            .expect("ELM IRQ bottom-half proxy used after drop")
            .run_bottom_half(line);
    }
}

impl Drop for ElmIrqBottomHalfProxy {
    fn drop(&mut self) {
        let Some(bottom_half) = self.bottom_half.take() else {
            return;
        };
        let Some(_guard) = super::pnp::enter_elm_snapshot(self.context) else {
            super::elm_lifecycle::mark_context_failed(self.context);
            core::mem::forget(bottom_half);
            return;
        };
        drop(bottom_half);
    }
}

fn wrap_elm_irq_callbacks(request: &mut IrqRequest) -> Result<(), IrqError> {
    let Some(context) = elm_model::current_context() else {
        return Ok(());
    };
    let _accounting =
        allocator::suspend_implicit_allocation_accounting().ok_or(IrqError::OutOfMemory)?;
    request.handler = Arc::new(ElmIrqHandlerProxy {
        context,
        owner: request.owner,
        handler: Some(Arc::clone(&request.handler)),
    });
    if let Some(bottom_half) = request.bottom_half.take() {
        request.bottom_half = Some(Arc::new(ElmIrqBottomHalfProxy {
            context,
            owner: request.owner,
            bottom_half: Some(bottom_half),
        }));
    }
    Ok(())
}

/// IRQ 注册请求。
///
/// 这是设备驱动面向 IRQ core 的完整声明：要消费哪条 line、是否允许共享、
/// 固件解析出的触发/极性，以及资源所有者的诊断标签。旧入口会构造 shared
/// request，保持既有多 handler 行为。
pub struct IrqRequest {
    pub line: IrqLine,
    pub handler: Arc<dyn IrqHandler>,
    pub owner: &'static str,
    pub sharing: IrqSharing,
    pub trigger: Option<IrqTrigger>,
    pub polarity: Option<IrqPolarity>,
    pub bottom_half: Option<Arc<dyn IrqBottomHalf>>,
}

impl IrqRequest {
    pub fn shared(line: IrqLine, owner: &'static str, handler: Arc<dyn IrqHandler>) -> Self {
        Self {
            line,
            handler,
            owner,
            sharing: IrqSharing::Shared,
            trigger: None,
            polarity: None,
            bottom_half: None,
        }
    }

    pub fn exclusive(line: IrqLine, owner: &'static str, handler: Arc<dyn IrqHandler>) -> Self {
        Self {
            sharing: IrqSharing::Exclusive,
            ..Self::shared(line, owner, handler)
        }
    }

    pub const fn with_trigger(mut self, trigger: IrqTrigger, polarity: IrqPolarity) -> Self {
        self.trigger = Some(trigger);
        self.polarity = Some(polarity);
        self
    }

    pub fn with_bottom_half(mut self, bottom_half: Arc<dyn IrqBottomHalf>) -> Self {
        self.bottom_half = Some(bottom_half);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqError {
    OutOfMemory,
    NotFound,
    AlreadyRegistered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqHandle {
    id: u64,
    line: IrqLine,
}

impl IrqHandle {
    pub const fn id(self) -> u64 {
        self.id
    }

    pub const fn line(self) -> IrqLine {
        self.line
    }
}

struct IrqRegistration {
    id: u64,
    proc_irq: u32,
    line: IrqLine,
    owner: &'static str,
    sharing: IrqSharing,
    _trigger: Option<IrqTrigger>,
    _polarity: Option<IrqPolarity>,
    handler: Arc<dyn IrqHandler>,
    bottom_half: Option<Arc<dyn IrqBottomHalf>>,
    counts: [u64; sched::NR_CPUS],
    calls_in_flight: usize,
    retiring: bool,
}

pub struct IrqRegistry {
    next_id: AtomicU64,
    next_proc_irq: AtomicU64,
    handlers: Spinlock<Vec<IrqRegistration>>,
}

impl IrqRegistry {
    pub const fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            next_proc_irq: AtomicU64::new(1),
            handlers: Spinlock::new(Vec::new()),
        }
    }

    pub fn register_request(&self, request: IrqRequest) -> Result<IrqHandle, IrqError> {
        if !configure_irq_line(request.line, request.trigger, request.polarity) {
            return Err(IrqError::NotFound);
        }
        let _irq_guard = sched::arch_hooks::disable_local_interrupts();
        let mut handlers = self.handlers.lock();
        if handlers.iter().any(|entry| {
            entry.line == request.line
                && (entry.sharing == IrqSharing::Exclusive
                    || request.sharing == IrqSharing::Exclusive)
        }) {
            return Err(IrqError::AlreadyRegistered);
        }
        {
            // handler 表容量在注销单个 handler 后仍由内核复用，不归动态单元所有。
            let _accounting =
                allocator::suspend_implicit_allocation_accounting().ok_or(IrqError::OutOfMemory)?;
            handlers.try_reserve(1).map_err(|_| IrqError::OutOfMemory)?;
        }
        let id = registry_id::alloc_atomic_id(&self.next_id).map_err(|_| IrqError::OutOfMemory)?;
        let proc_irq = handlers
            .iter()
            .find(|entry| entry.line == request.line)
            .map(|entry| entry.proc_irq)
            .map(Ok)
            .unwrap_or_else(|| {
                registry_id::alloc_atomic_id(&self.next_proc_irq)
                    .map_err(|_| IrqError::OutOfMemory)
                    .and_then(|value| u32::try_from(value).map_err(|_| IrqError::OutOfMemory))
            })?;
        let line = request.line;
        handlers.push(IrqRegistration {
            id,
            proc_irq,
            line,
            owner: request.owner,
            sharing: request.sharing,
            _trigger: request.trigger,
            _polarity: request.polarity,
            handler: request.handler,
            bottom_half: request.bottom_half,
            counts: [0; sched::NR_CPUS],
            calls_in_flight: 0,
            retiring: false,
        });
        drop(handlers);
        enable_irq_line(line);
        Ok(IrqHandle { id, line })
    }

    pub fn register(
        &self,
        line: IrqLine,
        handler: Arc<dyn IrqHandler>,
    ) -> Result<IrqHandle, IrqError> {
        self.register_request(IrqRequest::shared(line, "legacy-irq-handler", handler))
    }

    fn unregister_inner(&self, handle: IrqHandle, allow_prepared: bool) -> Result<bool, IrqError> {
        let _irq_guard = sched::arch_hooks::disable_local_interrupts();
        let mut handlers = self.handlers.lock();
        let Some(index) = handlers
            .iter()
            .position(|entry| entry.id == handle.id && entry.line == handle.line)
        else {
            return Err(IrqError::NotFound);
        };
        if handlers[index].calls_in_flight != 0 || (handlers[index].retiring && !allow_prepared) {
            return Err(IrqError::AlreadyRegistered);
        }
        let removed = handlers.remove(index);
        let still_used = if let Some(remaining) =
            handlers.iter_mut().find(|entry| entry.line == handle.line)
        {
            for cpu in 0..sched::NR_CPUS {
                remaining.counts[cpu] = remaining.counts[cpu].saturating_add(removed.counts[cpu]);
            }
            true
        } else {
            false
        };
        drop(handlers);
        if !still_used {
            disable_irq_line(handle.line);
        }
        Ok(removed.retiring)
    }

    pub fn unregister(&self, handle: IrqHandle) -> Result<(), IrqError> {
        self.unregister_inner(handle, false).map(|_| ())
    }

    fn prepare_unregister(&self, handle: IrqHandle) -> Option<IrqLine> {
        let _irq_guard = sched::arch_hooks::disable_local_interrupts();
        let mut handlers = self.handlers.lock();
        let entry = handlers
            .iter_mut()
            .find(|entry| entry.id == handle.id && entry.line == handle.line)?;
        if entry.retiring || entry.calls_in_flight != 0 {
            return None;
        }
        entry.retiring = true;
        Some(entry.line)
    }

    fn cancel_unregister(&self, handle: IrqHandle) {
        let _irq_guard = sched::arch_hooks::disable_local_interrupts();
        let mut handlers = self.handlers.lock();
        if let Some(entry) = handlers
            .iter_mut()
            .find(|entry| entry.id == handle.id && entry.line == handle.line)
        {
            entry.retiring = false;
        }
    }

    fn finish_call(&self, id: u64, line: IrqLine) {
        let _irq_guard = sched::arch_hooks::disable_local_interrupts();
        let mut handlers = self.handlers.lock();
        if let Some(entry) = handlers
            .iter_mut()
            .find(|entry| entry.id == id && entry.line == line)
        {
            entry.calls_in_flight = entry.calls_in_flight.saturating_sub(1);
        }
    }

    pub fn dispatch_line(&self, line: IrqLine) -> bool {
        let mut handled = false;
        let mut last_id = 0;

        loop {
            let next = {
                let _irq_guard = sched::arch_hooks::disable_local_interrupts();
                let mut handlers = self.handlers.lock();
                handlers
                    .iter_mut()
                    .filter(|entry| entry.line == line && entry.id > last_id && !entry.retiring)
                    .min_by_key(|entry| entry.id)
                    .and_then(|entry| {
                        entry.calls_in_flight = entry.calls_in_flight.checked_add(1)?;
                        Some((
                            entry.id,
                            Arc::clone(&entry.handler),
                            entry.bottom_half.as_ref().map(Arc::clone),
                        ))
                    })
            };
            let Some((id, handler, bottom_half)) = next else {
                break;
            };

            // 级联 interrupt-controller handler 会继续调用 dispatch_irq_line()
            // 分发子线。handler 调用必须发生在 registry 锁外，否则 PCH/EIOINTC
            // 这类树形 IRQ 拓扑会递归拿同一把 Spinlock 并死锁。
            last_id = id;
            let status = handler.handle_irq(line);
            if status.is_handled() {
                handled = true;
                if let Some(bottom_half) = bottom_half {
                    bottom_half.run_bottom_half(line);
                }
            }
            self.finish_call(id, line);
        }

        if handled {
            let cpu = sched::current_cpu_id().min(sched::NR_CPUS - 1);
            let _irq_guard = sched::arch_hooks::disable_local_interrupts();
            if let Some(entry) = self
                .handlers
                .lock()
                .iter_mut()
                .find(|entry| entry.line == line)
            {
                entry.counts[cpu] = entry.counts[cpu].saturating_add(1);
            }
        }

        handled
    }

    fn snapshot(&self) -> Vec<IrqLineSnapshot> {
        let _irq_guard = sched::arch_hooks::disable_local_interrupts();
        let handlers = self.handlers.lock();
        let mut snapshot: Vec<IrqLineSnapshot> = Vec::new();
        if snapshot.try_reserve(handlers.len()).is_err() {
            return snapshot;
        }
        for entry in handlers.iter() {
            if let Some(existing) = snapshot
                .iter_mut()
                .find(|item| item.proc_irq == entry.proc_irq)
            {
                for cpu in 0..sched::NR_CPUS {
                    existing.counts[cpu] = existing.counts[cpu].saturating_add(entry.counts[cpu]);
                }
                if !existing.owners.contains(&entry.owner) && existing.owners.try_reserve(1).is_ok()
                {
                    existing.owners.push(entry.owner);
                }
                continue;
            }
            let mut owners = Vec::new();
            if owners.try_reserve(1).is_err() {
                continue;
            }
            owners.push(entry.owner);
            snapshot.push(IrqLineSnapshot {
                proc_irq: entry.proc_irq,
                line: entry.line,
                counts: entry.counts,
                owners,
            });
        }
        snapshot.sort_unstable_by_key(|entry| entry.proc_irq);
        snapshot
    }
}

impl Default for IrqRegistry {
    fn default() -> Self {
        Self::new()
    }
}

static IRQ_REGISTRY: IrqRegistry = IrqRegistry::new();
static TIMER_INTERRUPT_COUNTS: [AtomicU64; sched::NR_CPUS] =
    [const { AtomicU64::new(0) }; sched::NR_CPUS];
static IRQ_LINE_OPS: Spinlock<Option<IrqLineOps>> = Spinlock::new(None);
static IOCSR_OPS: Spinlock<Option<IocsrOps>> = Spinlock::new(None);
static DEFAULT_IRQ_DOMAIN: Spinlock<Option<DefaultIrqDomainRegistration>> = Spinlock::new(None);
static NEXT_DEFAULT_IRQ_DOMAIN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct IrqLineSnapshot {
    pub proc_irq: u32,
    pub line: IrqLine,
    pub counts: [u64; sched::NR_CPUS],
    pub owners: Vec<&'static str>,
}

/// 记录当前 CPU 收到的一次调度定时器中断。
pub fn record_timer_interrupt() {
    let cpu = sched::current_cpu_id().min(sched::NR_CPUS - 1);
    TIMER_INTERRUPT_COUNTS[cpu].fetch_add(1, Ordering::Relaxed);
}

/// 返回每个 CPU 已处理的调度定时器中断数。
pub fn timer_interrupt_counts() -> [u64; sched::NR_CPUS] {
    core::array::from_fn(|cpu| TIMER_INTERRUPT_COUNTS[cpu].load(Ordering::Relaxed))
}

/// 返回当前已注册 IRQ line 的稳定诊断快照。
pub fn snapshot_irq_lines() -> Vec<IrqLineSnapshot> {
    IRQ_REGISTRY.snapshot()
}

pub trait IrqDomain: Send + Sync {
    /// 将固件中断 specifier 翻译成规范化 [`IrqLine`]。
    ///
    /// 在一些层级中断控制器上，翻译过程也等价于为该 specifier 分配一个稳定
    /// parent vector。实现可以更新 controller-private 映射表，但不能解析具体
    /// 设备语义或写设备驱动特判。
    fn translate(&self, cells: &[u32]) -> Option<IrqLine>;

    /// 使能或禁用 domain 内部的一条硬件线。
    ///
    /// 架构层只能直接控制 CPU 本地中断线；级联控制器的 mask/unmask 必须回到
    /// 对应 IRQ domain 执行。默认返回 `false` 表示该 domain 不支持运行期门控。
    fn set_line_enabled(&self, _hwirq: u32, _enabled: bool) -> bool {
        false
    }

    /// 配置 domain 内部硬件线的触发/极性。
    ///
    /// 固件已提供该信息时，IRQ registry 在 handler 注册前调用。默认返回 `true`
    /// 表示该 domain 接受默认配置；需要真实落寄存器的 controller 应覆盖它。
    fn configure_line(
        &self,
        _hwirq: u32,
        _trigger: Option<IrqTrigger>,
        _polarity: Option<IrqPolarity>,
    ) -> bool {
        true
    }
}

struct ElmIrqDomainProxy {
    context: elm_model::ElmCurrentContext,
    controller: u32,
    domain: Option<Arc<dyn IrqDomain>>,
}

impl ElmIrqDomainProxy {
    fn domain(&self) -> &dyn IrqDomain {
        self.domain
            .as_deref()
            .expect("ELM IRQ domain proxy used after drop")
    }

    fn enter(&self, operation: &'static str) -> Option<elm_model::ElmCurrentContextGuard> {
        let guard = super::pnp::enter_elm_snapshot(self.context);
        if guard.is_none() {
            log::error!(
                "[irq] cannot enter ELM domain context: controller={} operation={} cell={} generation={}",
                self.controller,
                operation,
                self.context.cell_id.0,
                self.context.generation.0,
            );
            super::elm_lifecycle::mark_context_failed(self.context);
        }
        guard
    }
}

impl IrqDomain for ElmIrqDomainProxy {
    fn translate(&self, cells: &[u32]) -> Option<IrqLine> {
        let _guard = self.enter("translate")?;
        self.domain().translate(cells)
    }

    fn set_line_enabled(&self, hwirq: u32, enabled: bool) -> bool {
        let Some(_guard) = self.enter("set_line_enabled") else {
            return false;
        };
        self.domain().set_line_enabled(hwirq, enabled)
    }

    fn configure_line(
        &self,
        hwirq: u32,
        trigger: Option<IrqTrigger>,
        polarity: Option<IrqPolarity>,
    ) -> bool {
        let Some(_guard) = self.enter("configure_line") else {
            return false;
        };
        self.domain().configure_line(hwirq, trigger, polarity)
    }
}

impl Drop for ElmIrqDomainProxy {
    fn drop(&mut self) {
        let Some(domain) = self.domain.take() else {
            return;
        };
        let Some(_guard) = super::pnp::enter_elm_snapshot(self.context) else {
            super::elm_lifecycle::mark_context_failed(self.context);
            core::mem::forget(domain);
            return;
        };
        drop(domain);
    }
}

fn wrap_elm_irq_domain(
    controller: u32,
    domain: Arc<dyn IrqDomain>,
) -> Result<Arc<dyn IrqDomain>, IrqError> {
    let Some(context) = elm_model::current_context() else {
        return Ok(domain);
    };
    let _accounting =
        allocator::suspend_implicit_allocation_accounting().ok_or(IrqError::OutOfMemory)?;
    Ok(Arc::new(ElmIrqDomainProxy {
        context,
        controller,
        domain: Some(domain),
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqDomainHandle {
    controller: u32,
    id: u64,
}

impl IrqDomainHandle {
    pub const fn controller(self) -> u32 {
        self.controller
    }

    pub const fn id(self) -> u64 {
        self.id
    }
}

struct IrqDomainRegistration {
    controller: u32,
    // 同一个 controller 可以在热移除后重新注册。句柄必须带注册代次，旧句柄
    // 不能注销后来安装的新 domain。
    id: u64,
    domain: Arc<dyn IrqDomain>,
    active_handlers: usize,
    prepared_handlers: usize,
    calls_in_flight: usize,
    retiring: bool,
}

pub struct IrqDomainRegistry {
    next_id: AtomicU64,
    domains: Spinlock<Vec<IrqDomainRegistration>>,
}

impl IrqDomainRegistry {
    pub const fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            domains: Spinlock::new(Vec::new()),
        }
    }

    pub fn register(
        &self,
        controller: u32,
        domain: Arc<dyn IrqDomain>,
    ) -> Result<IrqDomainHandle, IrqError> {
        let mut domains = self.domains.lock();
        if domains.iter().any(|entry| entry.controller == controller) {
            return Err(IrqError::AlreadyRegistered);
        }
        domains.try_reserve(1).map_err(|_| IrqError::OutOfMemory)?;
        let id = registry_id::alloc_atomic_id(&self.next_id).map_err(|_| IrqError::OutOfMemory)?;
        domains.push(IrqDomainRegistration {
            controller,
            id,
            domain,
            active_handlers: 0,
            prepared_handlers: 0,
            calls_in_flight: 0,
            retiring: false,
        });
        let handle = IrqDomainHandle { controller, id };
        drop(domains);
        pnp::notify_dependency_ready(PnpDependency::IrqController(controller));
        Ok(handle)
    }

    pub fn unregister(&self, handle: IrqDomainHandle) -> Result<(), IrqError> {
        let mut domains = self.domains.lock();
        let Some(index) = domains
            .iter()
            .position(|entry| entry.controller == handle.controller && entry.id == handle.id)
        else {
            return Err(IrqError::NotFound);
        };
        if domains[index].active_handlers != 0
            || domains[index].prepared_handlers != 0
            || domains[index].calls_in_flight != 0
        {
            return Err(IrqError::AlreadyRegistered);
        }
        domains.remove(index);
        Ok(())
    }

    fn begin_call(&self, controller: u32) -> Option<(u64, Arc<dyn IrqDomain>)> {
        let mut domains = self.domains.lock();
        let entry = domains
            .iter_mut()
            .find(|entry| entry.controller == controller && !entry.retiring)?;
        entry.calls_in_flight = entry.calls_in_flight.checked_add(1)?;
        Some((entry.id, Arc::clone(&entry.domain)))
    }

    fn finish_call(&self, controller: u32, id: u64) {
        let mut domains = self.domains.lock();
        if let Some(entry) = domains
            .iter_mut()
            .find(|entry| entry.controller == controller && entry.id == id)
        {
            entry.calls_in_flight = entry.calls_in_flight.saturating_sub(1);
        }
    }

    fn acquire_handler(&self, controller: u32) -> bool {
        let mut domains = self.domains.lock();
        let Some(entry) = domains
            .iter_mut()
            .find(|entry| entry.controller == controller && !entry.retiring)
        else {
            return false;
        };
        let Some(next) = entry.active_handlers.checked_add(1) else {
            return false;
        };
        entry.active_handlers = next;
        true
    }

    fn prepare_handler(&self, controller: u32) -> bool {
        let mut domains = self.domains.lock();
        let Some(entry) = domains
            .iter_mut()
            .find(|entry| entry.controller == controller && !entry.retiring)
        else {
            return false;
        };
        if entry.prepared_handlers >= entry.active_handlers {
            return false;
        }
        entry.prepared_handlers += 1;
        true
    }

    fn cancel_handler(&self, controller: u32) {
        let mut domains = self.domains.lock();
        if let Some(entry) = domains
            .iter_mut()
            .find(|entry| entry.controller == controller)
        {
            entry.prepared_handlers = entry.prepared_handlers.saturating_sub(1);
        }
    }

    fn release_handler(&self, controller: u32, prepared: bool) {
        let mut domains = self.domains.lock();
        if let Some(entry) = domains
            .iter_mut()
            .find(|entry| entry.controller == controller)
        {
            entry.active_handlers = entry.active_handlers.saturating_sub(1);
            if prepared {
                entry.prepared_handlers = entry.prepared_handlers.saturating_sub(1);
            }
        }
    }

    fn prepare_unregister(&self, handle: IrqDomainHandle) -> bool {
        let mut domains = self.domains.lock();
        let Some(entry) = domains
            .iter_mut()
            .find(|entry| entry.controller == handle.controller && entry.id == handle.id)
        else {
            return false;
        };
        entry.retiring = true;
        if entry.active_handlers == entry.prepared_handlers && entry.calls_in_flight == 0 {
            true
        } else {
            entry.retiring = false;
            false
        }
    }

    fn cancel_unregister(&self, handle: IrqDomainHandle) {
        let mut domains = self.domains.lock();
        if let Some(entry) = domains
            .iter_mut()
            .find(|entry| entry.controller == handle.controller && entry.id == handle.id)
        {
            entry.retiring = false;
        }
    }

    pub fn translate(&self, controller: u32, cells: &[u32]) -> Option<IrqLine> {
        let (id, domain) = self.begin_call(controller)?;
        let translated = domain.translate(cells);
        self.finish_call(controller, id);
        translated
    }

    pub fn set_line_enabled(&self, controller: u32, hwirq: u32, enabled: bool) -> bool {
        let Some((id, domain)) = self.begin_call(controller) else {
            return false;
        };
        let result = domain.set_line_enabled(hwirq, enabled);
        self.finish_call(controller, id);
        result
    }

    pub fn configure_line(
        &self,
        controller: u32,
        hwirq: u32,
        trigger: Option<IrqTrigger>,
        polarity: Option<IrqPolarity>,
    ) -> bool {
        let Some((id, domain)) = self.begin_call(controller) else {
            return false;
        };
        let result = domain.configure_line(hwirq, trigger, polarity);
        self.finish_call(controller, id);
        result
    }
}

impl Default for IrqDomainRegistry {
    fn default() -> Self {
        Self::new()
    }
}

static IRQ_DOMAINS: IrqDomainRegistry = IrqDomainRegistry::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultIrqDomainHandle {
    id: u64,
}

impl DefaultIrqDomainHandle {
    pub const fn id(self) -> u64 {
        self.id
    }
}

struct DefaultIrqDomainRegistration {
    id: u64,
    domain: Arc<dyn IrqDomain>,
}

#[kernel_symbols::export(
    name = "general.dev.irq.register_irq_handler",
    contract = "kernel.general.irq-handler@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn register_irq_handler(
    line: IrqLine,
    handler: Arc<dyn IrqHandler>,
) -> Result<IrqHandle, IrqError> {
    register_irq_request(IrqRequest::shared(line, "legacy-irq-handler", handler))
}

#[kernel_symbols::export(
    name = "general.dev.irq.register_irq_request",
    contract = "kernel.general.irq-handler@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn register_irq_request(mut request: IrqRequest) -> Result<IrqHandle, IrqError> {
    wrap_elm_irq_callbacks(&mut request)?;
    let domain_controller = match request.line {
        IrqLine::Controller { controller, .. } => {
            if !IRQ_DOMAINS.acquire_handler(controller) {
                return Err(IrqError::NotFound);
            }
            Some(controller)
        }
        _ => None,
    };
    let handle = match IRQ_REGISTRY.register_request(request) {
        Ok(handle) => handle,
        Err(error) => {
            if let Some(controller) = domain_controller {
                IRQ_DOMAINS.release_handler(controller, false);
            }
            return Err(error);
        }
    };
    if super::elm_lifecycle::track_irq_handler(handle).is_err() {
        let _ = IRQ_REGISTRY.unregister(handle);
        if let Some(controller) = domain_controller {
            IRQ_DOMAINS.release_handler(controller, false);
        }
        return Err(IrqError::OutOfMemory);
    }
    Ok(handle)
}

#[kernel_symbols::export(
    name = "general.dev.irq.unregister_irq_handler",
    contract = "kernel.general.irq-handler@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_irq_handler(handle: IrqHandle) -> Result<(), IrqError> {
    unregister_irq_handler_inner(handle, false)
}

fn unregister_irq_handler_inner(handle: IrqHandle, allow_prepared: bool) -> Result<(), IrqError> {
    let prepared = IRQ_REGISTRY.unregister_inner(handle, allow_prepared)?;
    if let IrqLine::Controller { controller, .. } = handle.line {
        IRQ_DOMAINS.release_handler(controller, prepared);
    }
    super::elm_lifecycle::forget_irq_handler(handle);
    Ok(())
}

fn release_irq_handler_resource(handle: IrqHandle) -> bool {
    unregister_irq_handler_inner(handle, true).is_ok()
}

fn prepare_irq_handler_resource(handle: IrqHandle) -> bool {
    let Some(line) = IRQ_REGISTRY.prepare_unregister(handle) else {
        return false;
    };
    if let IrqLine::Controller { controller, .. } = line
        && !IRQ_DOMAINS.prepare_handler(controller)
    {
        IRQ_REGISTRY.cancel_unregister(handle);
        return false;
    }
    true
}

fn cancel_irq_handler_resource(handle: IrqHandle) {
    if let IrqLine::Controller { controller, .. } = handle.line {
        IRQ_DOMAINS.cancel_handler(controller);
    }
    IRQ_REGISTRY.cancel_unregister(handle);
}

/// 将 IRQ handler 注册 handle 包装成 PnP-owned resource。
#[kernel_symbols::export(
    name = "general.dev.irq.irq_handler_pnp_resource",
    contract = "kernel.general.device-resource@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn irq_handler_pnp_resource(
    handle: IrqHandle,
    label: &'static str,
) -> PnpHandleResource<IrqHandle> {
    let resource = PnpHandleResource::new_checked(
        PnpResourceKind::Irq,
        label,
        handle,
        prepare_irq_handler_resource,
        cancel_irq_handler_resource,
        crate::dev::pnp::PnpResourceReleaseOrder::Consumer,
        release_irq_handler_resource,
    );
    match handle.line {
        IrqLine::Controller { controller, .. } => {
            resource.with_consumed_dependency(PnpDependency::IrqController(controller))
        }
        _ => resource,
    }
}

#[kernel_symbols::export(
    name = "general.dev.irq.install_irq_line_ops",
    contract = "kernel.general.irq-admin@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn install_irq_line_ops(ops: IrqLineOps) {
    if super::elm_lifecycle::install_irq_line_ops(ops).is_err() {
        log::error!("[irq] ELM IRQ line 操作安装失败，原操作保持不变");
    }
}

pub(crate) fn replace_irq_line_ops(ops: Option<IrqLineOps>) -> Option<IrqLineOps> {
    let mut current = IRQ_LINE_OPS.lock();
    core::mem::replace(&mut *current, ops)
}

#[kernel_symbols::export(
    name = "general.dev.irq.install_iocsr_ops",
    contract = "kernel.general.irq-admin@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn install_iocsr_ops(ops: IocsrOps) {
    if super::elm_lifecycle::install_iocsr_ops(ops).is_err() {
        log::error!("[irq] ELM IOCSR 操作安装失败，原操作保持不变");
    }
}

pub(crate) fn replace_iocsr_ops(ops: Option<IocsrOps>) -> Option<IocsrOps> {
    let mut current = IOCSR_OPS.lock();
    core::mem::replace(&mut *current, ops)
}

fn enable_irq_line(line: IrqLine) {
    let _ = set_irq_line_enabled(line, true);
}

fn disable_irq_line(line: IrqLine) {
    let _ = set_irq_line_enabled(line, false);
}

fn configure_irq_line(
    line: IrqLine,
    trigger: Option<IrqTrigger>,
    polarity: Option<IrqPolarity>,
) -> bool {
    if trigger.is_none() && polarity.is_none() {
        return true;
    }
    if let IrqLine::Controller { controller, hwirq } = line {
        return IRQ_DOMAINS.configure_line(controller, hwirq, trigger, polarity);
    }
    true
}

#[kernel_symbols::export(
    name = "general.dev.irq.set_irq_line_enabled",
    contract = "kernel.general.irq-control@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn set_irq_line_enabled(line: IrqLine, enabled: bool) -> bool {
    if let IrqLine::Controller { controller, hwirq } = line
        && IRQ_DOMAINS.set_line_enabled(controller, hwirq, enabled)
    {
        return true;
    }
    let Some(ops) = *IRQ_LINE_OPS.lock() else {
        return false;
    };
    if enabled {
        (ops.enable)(line)
    } else {
        (ops.disable)(line)
    }
}

#[kernel_symbols::export(
    name = "general.dev.irq.iocsr_read32",
    contract = "kernel.general.iocsr@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
)]
pub fn iocsr_read32(offset: usize) -> Option<u32> {
    let ops = (*IOCSR_OPS.lock())?;
    Some((ops.read32)(offset))
}

#[kernel_symbols::export(
    name = "general.dev.irq.iocsr_write32",
    contract = "kernel.general.iocsr@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn iocsr_write32(offset: usize, value: u32) -> bool {
    let Some(ops) = *IOCSR_OPS.lock() else {
        return false;
    };
    (ops.write32)(offset, value);
    true
}

#[kernel_symbols::export(
    name = "general.dev.irq.iocsr_read64",
    contract = "kernel.general.iocsr@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
)]
pub fn iocsr_read64(offset: usize) -> Option<u64> {
    let ops = (*IOCSR_OPS.lock())?;
    Some((ops.read64)(offset))
}

#[kernel_symbols::export(
    name = "general.dev.irq.iocsr_write64",
    contract = "kernel.general.iocsr@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn iocsr_write64(offset: usize, value: u64) -> bool {
    let Some(ops) = *IOCSR_OPS.lock() else {
        return false;
    };
    (ops.write64)(offset, value);
    true
}

#[kernel_symbols::export(
    name = "general.dev.irq.dispatch_interrupt",
    contract = "kernel.general.irq-dispatch@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn dispatch_interrupt(interrupt: Interrupt) -> bool {
    let Some(line) = IrqLine::from_interrupt(interrupt) else {
        return false;
    };
    #[cfg(feature = "performance-profile")]
    let mut profile =
        profiling::scope(profiling::Event::IrqDispatch).trace_args(profile_irq_line(line), 0);
    let handled = dispatch_irq_line(line);
    #[cfg(feature = "performance-profile")]
    profile.set_trace_args(profile_irq_line(line), u64::from(handled));
    handled
}

#[cfg(feature = "performance-profile")]
fn profile_irq_line(line: IrqLine) -> u64 {
    match line {
        IrqLine::Ipi => 0,
        IrqLine::Hardware(hwirq) => (1u64 << 56) | hwirq as u64,
        IrqLine::Controller { controller, hwirq } => {
            (2u64 << 56) | ((controller as u64) << 32) | hwirq as u64
        }
        IrqLine::Other(value) => (3u64 << 56) | value as u64,
    }
}

/// 分发一条已经规范化的 IRQ line。
///
/// 架构 trap 入口使用 [`dispatch_interrupt`] 从 CPU interrupt 编码进入；级联
/// interrupt-controller 驱动在完成寄存器级 demux/ack 后使用本函数分发子线。
#[kernel_symbols::export(
    name = "general.dev.irq.dispatch_irq_line",
    contract = "kernel.general.irq-dispatch@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn dispatch_irq_line(line: IrqLine) -> bool {
    IRQ_REGISTRY.dispatch_line(line)
}

#[kernel_symbols::export(
    name = "general.dev.irq.register_irq_domain",
    contract = "kernel.general.irq-domain@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn register_irq_domain(
    controller: u32,
    domain: Arc<dyn IrqDomain>,
) -> Result<IrqDomainHandle, IrqError> {
    let domain = wrap_elm_irq_domain(controller, domain)?;
    let handle = IRQ_DOMAINS.register(controller, domain)?;
    if super::elm_lifecycle::track_irq_domain(handle).is_err() {
        let _ = IRQ_DOMAINS.unregister(handle);
        return Err(IrqError::OutOfMemory);
    }
    Ok(handle)
}

#[kernel_symbols::export(
    name = "general.dev.irq.unregister_irq_domain",
    contract = "kernel.general.irq-domain@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_irq_domain(handle: IrqDomainHandle) -> Result<(), IrqError> {
    IRQ_DOMAINS.unregister(handle)?;
    super::elm_lifecycle::forget_irq_domain(handle);
    Ok(())
}

fn release_irq_domain_resource(handle: IrqDomainHandle) -> bool {
    unregister_irq_domain(handle).is_ok()
}

fn prepare_irq_domain_resource(handle: IrqDomainHandle) -> bool {
    IRQ_DOMAINS.prepare_unregister(handle)
}

fn cancel_irq_domain_resource(handle: IrqDomainHandle) {
    IRQ_DOMAINS.cancel_unregister(handle);
}

/// 将 IRQ domain 注册 handle 包装成 PnP-owned resource。
#[kernel_symbols::export(
    name = "general.dev.irq.irq_domain_pnp_resource",
    contract = "kernel.general.device-resource@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn irq_domain_pnp_resource(
    handle: IrqDomainHandle,
    label: &'static str,
) -> PnpHandleResource<IrqDomainHandle> {
    PnpHandleResource::new_checked(
        PnpResourceKind::IrqDomain,
        label,
        handle,
        prepare_irq_domain_resource,
        cancel_irq_domain_resource,
        crate::dev::pnp::PnpResourceReleaseOrder::Provider,
        release_irq_domain_resource,
    )
    .with_provided_dependency(PnpDependency::IrqController(handle.controller))
}

/// 安装当前固件模型的默认 IRQ domain。
///
/// DTB 设备通常通过 phandle 指向具体 interrupt-controller；ACPI 这类固件模型
/// 可能直接给出全局中断号，此时 `DeviceResource::Irq { controller: None, .. }`
/// 会交给这里注册的默认 domain 翻译。该接口只表达资源解释策略，不把任何平台
/// 的中断号布局硬编码进设备 core。
#[kernel_symbols::export(
    name = "general.dev.irq.register_default_irq_domain",
    contract = "kernel.general.irq-domain@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn register_default_irq_domain(
    domain: Arc<dyn IrqDomain>,
) -> Result<DefaultIrqDomainHandle, IrqError> {
    let domain = wrap_elm_irq_domain(u32::MAX, domain)?;
    let mut default = DEFAULT_IRQ_DOMAIN.lock();
    if default.is_some() {
        return Err(IrqError::AlreadyRegistered);
    }
    let id = registry_id::alloc_atomic_id(&NEXT_DEFAULT_IRQ_DOMAIN_ID)
        .map_err(|_| IrqError::OutOfMemory)?;
    *default = Some(DefaultIrqDomainRegistration { id, domain });
    drop(default);
    let handle = DefaultIrqDomainHandle { id };
    if super::elm_lifecycle::track_default_irq_domain(handle).is_err() {
        let _ = unregister_default_irq_domain(handle);
        return Err(IrqError::OutOfMemory);
    }
    pnp::notify_dependency_ready(PnpDependency::DefaultIrqDomain);
    Ok(handle)
}

#[kernel_symbols::export(
    name = "general.dev.irq.unregister_default_irq_domain",
    contract = "kernel.general.irq-domain@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_default_irq_domain(handle: DefaultIrqDomainHandle) -> Result<(), IrqError> {
    let mut default = DEFAULT_IRQ_DOMAIN.lock();
    let Some(registration) = default.as_ref() else {
        return Err(IrqError::NotFound);
    };
    // 默认 IRQ domain 和其它 domain 一样使用注册句柄表达所有权。旧 handle
    // 不能注销后续重新安装的 domain，否则 ACPI/DTB 启动路径的恢复逻辑会互相踩踏。
    if registration.id != handle.id {
        return Err(IrqError::NotFound);
    }
    *default = None;
    drop(default);
    super::elm_lifecycle::forget_default_irq_domain(handle);
    Ok(())
}

fn release_default_irq_domain_resource(handle: DefaultIrqDomainHandle) -> bool {
    unregister_default_irq_domain(handle).is_ok()
}

/// 将默认 IRQ domain 注册 handle 包装成 PnP-owned resource。
#[kernel_symbols::export(
    name = "general.dev.irq.default_irq_domain_pnp_resource",
    contract = "kernel.general.device-resource@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn default_irq_domain_pnp_resource(
    handle: DefaultIrqDomainHandle,
    label: &'static str,
) -> PnpHandleResource<DefaultIrqDomainHandle> {
    PnpHandleResource::new(
        PnpResourceKind::IrqDomain,
        label,
        handle,
        release_default_irq_domain_resource,
    )
}

#[kernel_symbols::export(
    name = "general.dev.irq.translate_firmware_irq",
    contract = "kernel.general.irq-domain@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
)]
pub fn translate_firmware_irq(controller: Option<u32>, cells: &[u32]) -> Option<IrqLine> {
    match controller {
        Some(controller) => IRQ_DOMAINS.translate(controller, cells),
        None => DEFAULT_IRQ_DOMAIN
            .lock()
            .as_ref()
            .and_then(|registration| registration.domain.translate(cells)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    struct CountingHandler(Arc<AtomicUsize>);

    impl IrqHandler for CountingHandler {
        fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
            self.0.fetch_add(1, Ordering::Relaxed);
            IrqStatus::Handled
        }
    }

    struct IdentityDomain;

    impl IrqDomain for IdentityDomain {
        fn translate(&self, cells: &[u32]) -> Option<IrqLine> {
            let [hwirq] = cells else {
                return None;
            };
            Some(IrqLine::Controller {
                controller: 77,
                hwirq: *hwirq,
            })
        }
    }

    #[test]
    fn prepared_handler_is_hidden_from_dispatch_until_cancel() {
        let registry = IrqRegistry::new();
        let count = Arc::new(AtomicUsize::new(0));
        let line = IrqLine::Hardware(0x7fff);
        let handle = registry
            .register(line, Arc::new(CountingHandler(Arc::clone(&count))))
            .unwrap();
        assert!(registry.dispatch_line(line));
        assert_eq!(count.load(Ordering::Relaxed), 1);

        assert_eq!(registry.prepare_unregister(handle), Some(line));
        assert!(!registry.dispatch_line(line));
        assert_eq!(
            registry.unregister(handle),
            Err(IrqError::AlreadyRegistered)
        );

        registry.cancel_unregister(handle);
        assert!(registry.dispatch_line(line));
        assert_eq!(count.load(Ordering::Relaxed), 2);
        registry.unregister(handle).unwrap();
    }

    #[test]
    fn planned_consumers_allow_domain_provider_prepare() {
        let registry = IrqDomainRegistry::new();
        let handle = registry.register(77, Arc::new(IdentityDomain)).unwrap();
        assert!(registry.acquire_handler(77));
        assert!(!registry.prepare_unregister(handle));

        assert!(registry.prepare_handler(77));
        assert!(registry.prepare_unregister(handle));
        assert!(!registry.acquire_handler(77));
        registry.release_handler(77, true);
        registry.unregister(handle).unwrap();
    }
}

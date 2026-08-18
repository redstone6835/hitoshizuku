//! 通用 IRQ 注册与分发层。
//!
//! 架构 trap 入口只知道“发生了哪条规范化中断线”，不应该理解具体设备。
//! 本模块提供一个小型共享 IRQ registry：设备驱动注册 [`IrqHandler`]，架构层
//! 在非 timer 中断到达时调用 [`dispatch_interrupt`]。真正的 IRQ domain/控制器
//! 驱动以后可以在此基础上把固件中断 specifier 翻译成 [`IrqLine`]。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use hashbrown::HashTable;
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
    /// registry 只在短临界区内取得 handler 快照，实际回调在释放 registry 锁后
    /// 执行。handler 仍必须快速返回、不能睡眠，也不能在回调内注册或注销同一
    /// registry；这样可以避免回调与注册路径互相等待。
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
    owner: &'static str,
    sharing: IrqSharing,
    _trigger: Option<IrqTrigger>,
    _polarity: Option<IrqPolarity>,
    handler: Arc<dyn IrqHandler>,
    bottom_half: Option<Arc<dyn IrqBottomHalf>>,
}

/// IRQ 回调的只读热路径快照。
///
/// 注册/注销只在冷路径重建该数组；dispatch 只需取得一次 bucket 锁并持有
/// 一个 Arc，避免每个共享 handler 都重新查 HashTable、克隆 Arc 和竞争全局
/// registry 锁。旧快照在并发注销期间仍保持有效，符合 IRQ 回调不能睡眠的约束。
struct IrqDispatchRegistration {
    handler: Arc<dyn IrqHandler>,
    bottom_half: Option<Arc<dyn IrqBottomHalf>>,
}

struct IrqBucket {
    line: IrqLine,
    proc_irq: u32,
    handlers: Vec<IrqRegistration>,
    dispatch: Arc<[IrqDispatchRegistration]>,
    counts: [u64; sched::NR_CPUS],
}

pub struct IrqRegistry {
    next_id: AtomicU64,
    next_proc_irq: AtomicU64,
    lines: Spinlock<HashTable<IrqBucket>>,
}

/// 为 IRQ line 生成稳定的 SwissTable 哈希。
///
/// controller phandle 和 hwirq 通常都是连续小整数；一次奇数乘法同时打散 bucket
/// 低位和 7-bit control tag，避免相邻设备线聚集在同一 probe group。
#[inline]
fn irq_line_hash(line: IrqLine) -> u64 {
    let value = match line {
        IrqLine::Ipi => 0x243f_6a88_85a3_08d3,
        IrqLine::Hardware(hwirq) => 0x1319_8a2e_0370_7344 ^ hwirq as u64,
        IrqLine::Controller { controller, hwirq } => {
            0xa409_3822_299f_31d0 ^ ((controller as u64) << 32) ^ hwirq as u64
        }
        IrqLine::Other(value) => 0x082e_fa98_ec4e_6c89 ^ value as u64,
    };
    let mixed = value ^ value.rotate_left(25) ^ value.rotate_right(17);
    mixed.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

#[inline]
fn irq_bucket_hash(bucket: &IrqBucket) -> u64 {
    irq_line_hash(bucket.line)
}

fn build_dispatch_snapshot(
    handlers: &[IrqRegistration],
) -> Result<Arc<[IrqDispatchRegistration]>, IrqError> {
    let mut snapshot = Vec::new();
    snapshot
        .try_reserve(handlers.len())
        .map_err(|_| IrqError::OutOfMemory)?;
    for entry in handlers {
        snapshot.push(IrqDispatchRegistration {
            handler: Arc::clone(&entry.handler),
            bottom_half: entry.bottom_half.as_ref().map(Arc::clone),
        });
    }
    Ok(Arc::from(snapshot.into_boxed_slice()))
}

impl IrqRegistry {
    pub const fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            next_proc_irq: AtomicU64::new(1),
            lines: Spinlock::new(HashTable::new()),
        }
    }

    pub fn register_request(&self, request: IrqRequest) -> Result<IrqHandle, IrqError> {
        if !configure_irq_line(request.line, request.trigger, request.polarity) {
            return Err(IrqError::NotFound);
        }
        let line = request.line;
        let hash = irq_line_hash(line);
        let mut lines = self.lines.lock();
        if let Some(bucket) = lines.find_mut(hash, |bucket| bucket.line == line) {
            if bucket.handlers.iter().any(|entry| {
                entry.sharing == IrqSharing::Exclusive || request.sharing == IrqSharing::Exclusive
            }) {
                return Err(IrqError::AlreadyRegistered);
            }
            {
                // handler 表容量在注销单个 handler 后仍由内核复用，不归动态单元所有。
                let _accounting = allocator::suspend_implicit_allocation_accounting()
                    .ok_or(IrqError::OutOfMemory)?;
                bucket
                    .handlers
                    .try_reserve(1)
                    .map_err(|_| IrqError::OutOfMemory)?;
            }
            let id =
                registry_id::alloc_atomic_id(&self.next_id).map_err(|_| IrqError::OutOfMemory)?;
            bucket.handlers.push(IrqRegistration {
                id,
                owner: request.owner,
                sharing: request.sharing,
                _trigger: request.trigger,
                _polarity: request.polarity,
                handler: request.handler,
                bottom_half: request.bottom_half,
            });
            let dispatch = match build_dispatch_snapshot(&bucket.handlers) {
                Ok(dispatch) => dispatch,
                Err(error) => {
                    bucket.handlers.pop();
                    return Err(error);
                }
            };
            bucket.dispatch = dispatch;
            drop(lines);
            enable_irq_line(line);
            return Ok(IrqHandle { id, line });
        }

        let mut handlers = Vec::new();
        {
            let _accounting =
                allocator::suspend_implicit_allocation_accounting().ok_or(IrqError::OutOfMemory)?;
            lines
                .try_reserve(1, irq_bucket_hash)
                .map_err(|_| IrqError::OutOfMemory)?;
            handlers.try_reserve(1).map_err(|_| IrqError::OutOfMemory)?;
        }
        let id = registry_id::alloc_atomic_id(&self.next_id).map_err(|_| IrqError::OutOfMemory)?;
        let proc_irq = registry_id::alloc_atomic_id(&self.next_proc_irq)
            .map_err(|_| IrqError::OutOfMemory)
            .and_then(|value| u32::try_from(value).map_err(|_| IrqError::OutOfMemory))?;
        handlers.push(IrqRegistration {
            id,
            owner: request.owner,
            sharing: request.sharing,
            _trigger: request.trigger,
            _polarity: request.polarity,
            handler: request.handler,
            bottom_half: request.bottom_half,
        });
        let dispatch = build_dispatch_snapshot(&handlers)?;
        lines.insert_unique(
            hash,
            IrqBucket {
                line,
                proc_irq,
                handlers,
                dispatch,
                counts: [0; sched::NR_CPUS],
            },
            irq_bucket_hash,
        );
        drop(lines);
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

    pub fn unregister(&self, handle: IrqHandle) -> Result<(), IrqError> {
        let hash = irq_line_hash(handle.line);
        let mut lines = self.lines.lock();
        let Some(bucket) = lines.find_mut(hash, |bucket| bucket.line == handle.line) else {
            return Err(IrqError::NotFound);
        };
        let Some(index) = bucket
            .handlers
            .iter()
            .position(|entry| entry.id == handle.id)
        else {
            return Err(IrqError::NotFound);
        };
        let removed = bucket.handlers.remove(index);
        let still_used = !bucket.handlers.is_empty();
        if still_used {
            match build_dispatch_snapshot(&bucket.handlers) {
                Ok(dispatch) => bucket.dispatch = dispatch,
                Err(error) => {
                    bucket.handlers.insert(index, removed);
                    return Err(error);
                }
            }
        } else {
            let entry = lines
                .find_entry(hash, |bucket| bucket.line == handle.line)
                .map_err(|_| IrqError::NotFound)?;
            let _ = entry.remove();
        }
        drop(lines);
        if !still_used {
            disable_irq_line(handle.line);
        }
        Ok(())
    }

    pub fn dispatch_line(&self, line: IrqLine) -> bool {
        let hash = irq_line_hash(line);
        let dispatch = {
            let lines = self.lines.lock();
            lines
                .find(hash, |bucket| bucket.line == line)
                .map(|bucket| Arc::clone(&bucket.dispatch))
        };
        let Some(dispatch) = dispatch else {
            return false;
        };
        let mut handled = false;
        for entry in dispatch.iter() {
            // 级联 interrupt-controller handler 会继续调用 dispatch_irq_line()
            // 分发子线。handler 调用必须发生在 registry 锁外，否则 PCH/EIOINTC
            // 这类树形 IRQ 拓扑会递归拿同一把 Spinlock 并死锁。
            let status = entry.handler.handle_irq(line);
            if status.is_handled() {
                handled = true;
                if let Some(bottom_half) = entry.bottom_half.as_ref() {
                    bottom_half.run_bottom_half(line);
                }
            }
        }

        if handled {
            let cpu = sched::current_cpu_id().min(sched::NR_CPUS - 1);
            if let Some(bucket) = self
                .lines
                .lock()
                .find_mut(hash, |bucket| bucket.line == line)
            {
                bucket.counts[cpu] = bucket.counts[cpu].saturating_add(1);
            }
        }

        handled
    }

    fn snapshot(&self) -> Vec<IrqLineSnapshot> {
        let lines = self.lines.lock();
        let mut snapshot: Vec<IrqLineSnapshot> = Vec::new();
        if snapshot.try_reserve(lines.len()).is_err() {
            return snapshot;
        }
        for bucket in lines.iter() {
            let mut owners = Vec::new();
            if owners.try_reserve(bucket.handlers.len()).is_err() {
                continue;
            }
            for entry in &bucket.handlers {
                if !owners.contains(&entry.owner) {
                    owners.push(entry.owner);
                }
            }
            snapshot.push(IrqLineSnapshot {
                proc_irq: bucket.proc_irq,
                line: bucket.line,
                counts: bucket.counts,
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
        domains.remove(index);
        Ok(())
    }

    fn domain(&self, controller: u32) -> Option<Arc<dyn IrqDomain>> {
        let domains = self.domains.lock();
        domains
            .iter()
            .find(|entry| entry.controller == controller)
            .map(|entry| Arc::clone(&entry.domain))
    }

    pub fn translate(&self, controller: u32, cells: &[u32]) -> Option<IrqLine> {
        self.domain(controller)?.translate(cells)
    }

    pub fn set_line_enabled(&self, controller: u32, hwirq: u32, enabled: bool) -> bool {
        self.domain(controller)
            .is_some_and(|domain| domain.set_line_enabled(hwirq, enabled))
    }

    pub fn configure_line(
        &self,
        controller: u32,
        hwirq: u32,
        trigger: Option<IrqTrigger>,
        polarity: Option<IrqPolarity>,
    ) -> bool {
        self.domain(controller)
            .is_some_and(|domain| domain.configure_line(hwirq, trigger, polarity))
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
pub fn register_irq_request(request: IrqRequest) -> Result<IrqHandle, IrqError> {
    let handle = IRQ_REGISTRY.register_request(request)?;
    if super::elm_lifecycle::track_irq_handler(handle).is_err() {
        let _ = IRQ_REGISTRY.unregister(handle);
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
    IRQ_REGISTRY.unregister(handle)?;
    super::elm_lifecycle::forget_irq_handler(handle);
    Ok(())
}

fn release_irq_handler_resource(handle: IrqHandle) -> bool {
    unregister_irq_handler(handle).is_ok()
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
    PnpHandleResource::new(
        PnpResourceKind::Irq,
        label,
        handle,
        release_irq_handler_resource,
    )
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
    PnpHandleResource::new(
        PnpResourceKind::IrqDomain,
        label,
        handle,
        release_irq_domain_resource,
    )
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
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    use vfs::sync::Spinlock;

    use super::{IrqError, IrqHandler, IrqLine, IrqRegistry, IrqRequest, IrqStatus};

    struct RecordingHandler {
        marker: usize,
        status: IrqStatus,
        calls: Arc<Spinlock<Vec<usize>>>,
    }

    impl IrqHandler for RecordingHandler {
        fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
            self.calls.lock().push(self.marker);
            self.status
        }
    }

    fn handler(
        marker: usize,
        status: IrqStatus,
        calls: &Arc<Spinlock<Vec<usize>>>,
    ) -> Arc<dyn IrqHandler> {
        Arc::new(RecordingHandler {
            marker,
            status,
            calls: Arc::clone(calls),
        })
    }

    #[test]
    fn shared_line_keeps_registration_order_counts_and_lifetime() {
        let registry = IrqRegistry::new();
        let calls = Arc::new(Spinlock::new(Vec::new()));
        let line = IrqLine::Controller {
            controller: 17,
            hwirq: 32,
        };
        let first = registry
            .register_request(IrqRequest::shared(
                line,
                "first",
                handler(1, IrqStatus::Handled, &calls),
            ))
            .expect("first shared handler should register");
        let second = registry
            .register_request(IrqRequest::shared(
                line,
                "second",
                handler(2, IrqStatus::Unhandled, &calls),
            ))
            .expect("second shared handler should register");

        assert!(registry.dispatch_line(line));
        assert_eq!(&*calls.lock(), &[1, 2]);
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].line, line);
        assert_eq!(snapshot[0].counts.iter().sum::<u64>(), 1);
        assert_eq!(snapshot[0].owners, ["first", "second"]);

        registry
            .unregister(first)
            .expect("first shared handler should unregister");
        calls.lock().clear();
        assert!(!registry.dispatch_line(line));
        assert_eq!(&*calls.lock(), &[2]);
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].counts.iter().sum::<u64>(), 1);
        assert_eq!(snapshot[0].owners, ["second"]);

        registry
            .unregister(second)
            .expect("last shared handler should unregister");
        assert!(!registry.dispatch_line(line));
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn exclusive_line_rejects_all_other_handlers() {
        let registry = IrqRegistry::new();
        let calls = Arc::new(Spinlock::new(Vec::new()));
        let line = IrqLine::Hardware(5);
        let exclusive = registry
            .register_request(IrqRequest::exclusive(
                line,
                "exclusive",
                handler(1, IrqStatus::Handled, &calls),
            ))
            .expect("exclusive handler should register on an empty line");
        assert_eq!(
            registry.register_request(IrqRequest::shared(
                line,
                "shared",
                handler(2, IrqStatus::Handled, &calls),
            )),
            Err(IrqError::AlreadyRegistered)
        );
        registry
            .unregister(exclusive)
            .expect("exclusive handler should unregister");

        let shared = registry
            .register_request(IrqRequest::shared(
                line,
                "shared",
                handler(2, IrqStatus::Handled, &calls),
            ))
            .expect("shared handler should register on an empty line");
        assert_eq!(
            registry.register_request(IrqRequest::exclusive(
                line,
                "exclusive",
                handler(1, IrqStatus::Handled, &calls),
            )),
            Err(IrqError::AlreadyRegistered)
        );
        registry
            .unregister(shared)
            .expect("shared handler should unregister");
    }
}

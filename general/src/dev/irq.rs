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

use crate::Interrupt;

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
    line: IrqLine,
    handler: Arc<dyn IrqHandler>,
}

pub struct IrqRegistry {
    next_id: AtomicU64,
    handlers: Spinlock<Vec<IrqRegistration>>,
}

impl IrqRegistry {
    pub const fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            handlers: Spinlock::new(Vec::new()),
        }
    }

    pub fn register(
        &self,
        line: IrqLine,
        handler: Arc<dyn IrqHandler>,
    ) -> Result<IrqHandle, IrqError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut handlers = self.handlers.lock();
        handlers.try_reserve(1).map_err(|_| IrqError::OutOfMemory)?;
        handlers.push(IrqRegistration { id, line, handler });
        drop(handlers);
        enable_irq_line(line);
        Ok(IrqHandle { id, line })
    }

    pub fn unregister(&self, handle: IrqHandle) -> Result<(), IrqError> {
        let mut handlers = self.handlers.lock();
        let Some(index) = handlers
            .iter()
            .position(|entry| entry.id == handle.id && entry.line == handle.line)
        else {
            return Err(IrqError::NotFound);
        };
        handlers.remove(index);
        let still_used = handlers.iter().any(|entry| entry.line == handle.line);
        drop(handlers);
        if !still_used {
            disable_irq_line(handle.line);
        }
        Ok(())
    }

    pub fn dispatch_line(&self, line: IrqLine) -> bool {
        let mut handled = false;
        let mut last_id = 0;

        loop {
            let next = {
                let handlers = self.handlers.lock();
                handlers
                    .iter()
                    .filter(|entry| entry.line == line && entry.id > last_id)
                    .min_by_key(|entry| entry.id)
                    .map(|entry| (entry.id, Arc::clone(&entry.handler)))
            };
            let Some((id, handler)) = next else {
                break;
            };

            // 级联 interrupt-controller handler 会继续调用 dispatch_irq_line()
            // 分发子线。handler 调用必须发生在 registry 锁外，否则 PCH/EIOINTC
            // 这类树形 IRQ 拓扑会递归拿同一把 Spinlock 并死锁。
            last_id = id;
            handled |= handler.handle_irq(line).is_handled();
        }

        handled
    }
}

impl Default for IrqRegistry {
    fn default() -> Self {
        Self::new()
    }
}

static IRQ_REGISTRY: IrqRegistry = IrqRegistry::new();
static IRQ_LINE_OPS: Spinlock<Option<IrqLineOps>> = Spinlock::new(None);
static IOCSR_OPS: Spinlock<Option<IocsrOps>> = Spinlock::new(None);
static DEFAULT_IRQ_DOMAIN: Spinlock<Option<Arc<dyn IrqDomain>>> = Spinlock::new(None);

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqDomainHandle {
    controller: u32,
}

impl IrqDomainHandle {
    pub const fn controller(self) -> u32 {
        self.controller
    }
}

struct IrqDomainRegistration {
    controller: u32,
    domain: Arc<dyn IrqDomain>,
}

pub struct IrqDomainRegistry {
    domains: Spinlock<Vec<IrqDomainRegistration>>,
}

impl IrqDomainRegistry {
    pub const fn new() -> Self {
        Self {
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
        domains.push(IrqDomainRegistration { controller, domain });
        Ok(IrqDomainHandle { controller })
    }

    pub fn unregister(&self, handle: IrqDomainHandle) -> Result<(), IrqError> {
        let mut domains = self.domains.lock();
        let Some(index) = domains
            .iter()
            .position(|entry| entry.controller == handle.controller)
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
}

impl Default for IrqDomainRegistry {
    fn default() -> Self {
        Self::new()
    }
}

static IRQ_DOMAINS: IrqDomainRegistry = IrqDomainRegistry::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultIrqDomainHandle;

pub fn register_irq_handler(
    line: IrqLine,
    handler: Arc<dyn IrqHandler>,
) -> Result<IrqHandle, IrqError> {
    IRQ_REGISTRY.register(line, handler)
}

pub fn unregister_irq_handler(handle: IrqHandle) -> Result<(), IrqError> {
    IRQ_REGISTRY.unregister(handle)
}

pub fn install_irq_line_ops(ops: IrqLineOps) {
    *IRQ_LINE_OPS.lock() = Some(ops);
}

pub fn install_iocsr_ops(ops: IocsrOps) {
    *IOCSR_OPS.lock() = Some(ops);
}

fn enable_irq_line(line: IrqLine) {
    let _ = set_irq_line_enabled(line, true);
}

fn disable_irq_line(line: IrqLine) {
    let _ = set_irq_line_enabled(line, false);
}

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

pub fn iocsr_read32(offset: usize) -> Option<u32> {
    let ops = (*IOCSR_OPS.lock())?;
    Some((ops.read32)(offset))
}

pub fn iocsr_write32(offset: usize, value: u32) -> bool {
    let Some(ops) = *IOCSR_OPS.lock() else {
        return false;
    };
    (ops.write32)(offset, value);
    true
}

pub fn iocsr_read64(offset: usize) -> Option<u64> {
    let ops = (*IOCSR_OPS.lock())?;
    Some((ops.read64)(offset))
}

pub fn iocsr_write64(offset: usize, value: u64) -> bool {
    let Some(ops) = *IOCSR_OPS.lock() else {
        return false;
    };
    (ops.write64)(offset, value);
    true
}

pub fn dispatch_interrupt(interrupt: Interrupt) -> bool {
    let Some(line) = IrqLine::from_interrupt(interrupt) else {
        return false;
    };
    dispatch_irq_line(line)
}

/// 分发一条已经规范化的 IRQ line。
///
/// 架构 trap 入口使用 [`dispatch_interrupt`] 从 CPU interrupt 编码进入；级联
/// interrupt-controller 驱动在完成寄存器级 demux/ack 后使用本函数分发子线。
pub fn dispatch_irq_line(line: IrqLine) -> bool {
    IRQ_REGISTRY.dispatch_line(line)
}

pub fn register_irq_domain(
    controller: u32,
    domain: Arc<dyn IrqDomain>,
) -> Result<IrqDomainHandle, IrqError> {
    IRQ_DOMAINS.register(controller, domain)
}

pub fn unregister_irq_domain(handle: IrqDomainHandle) -> Result<(), IrqError> {
    IRQ_DOMAINS.unregister(handle)
}

/// 安装当前固件模型的默认 IRQ domain。
///
/// DTB 设备通常通过 phandle 指向具体 interrupt-controller；ACPI 这类固件模型
/// 可能直接给出全局中断号，此时 `DeviceResource::Irq { controller: None, .. }`
/// 会交给这里注册的默认 domain 翻译。该接口只表达资源解释策略，不把任何平台
/// 的中断号布局硬编码进设备 core。
pub fn register_default_irq_domain(
    domain: Arc<dyn IrqDomain>,
) -> Result<DefaultIrqDomainHandle, IrqError> {
    let mut default = DEFAULT_IRQ_DOMAIN.lock();
    if default.is_some() {
        return Err(IrqError::AlreadyRegistered);
    }
    *default = Some(domain);
    Ok(DefaultIrqDomainHandle)
}

pub fn unregister_default_irq_domain(_handle: DefaultIrqDomainHandle) -> Result<(), IrqError> {
    let mut default = DEFAULT_IRQ_DOMAIN.lock();
    if default.take().is_some() {
        Ok(())
    } else {
        Err(IrqError::NotFound)
    }
}

pub fn translate_firmware_irq(controller: Option<u32>, cells: &[u32]) -> Option<IrqLine> {
    match controller {
        Some(controller) => IRQ_DOMAINS.translate(controller, cells),
        None => DEFAULT_IRQ_DOMAIN
            .lock()
            .as_ref()
            .and_then(|domain| domain.translate(cells)),
    }
}

//! 架构无关的性能监控单元（PMU）描述与注册表。
//!
//! 固件驱动负责把平台特定的事件编码解析为 [`PmuEventCounterRange`]；性能计数器
//! 子系统只通过本模块查询“一个事件允许使用哪些逻辑 counter”，不直接读取 DTB
//! 属性，也不依赖 RISC-V SBI、ACPI 或具体 CSR 编码。

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::marker::PhantomData;

use vfs::sync::Spinlock;

use crate::dev::pnp::{self, PnpDependency, PnpHandleResource, PnpResourceKind};

use super::registry_id;

/// 一段连续事件到逻辑性能计数器位图的映射。
///
/// 位 `n` 对应 PMU 对外公布的逻辑 counter `n`。具体 counter 是否是固定计数器、
/// `MHPMCOUNTERn` 或固件计数器，由注册该映射的平台驱动和后续 PMU backend 解释。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmuEventCounterRange {
    first_event: u32,
    last_event: u32,
    counter_mask: u32,
}

#[kernel_symbols::export]
impl PmuEventCounterRange {
    /// 构造一个闭区间事件映射。
    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuEventCounterRange.new",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
    pub fn new(first_event: u32, last_event: u32, counter_mask: u32) -> Option<Self> {
        if first_event > last_event {
            return None;
        }
        Some(Self {
            first_event,
            last_event,
            counter_mask,
        })
    }

    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuEventCounterRange.first_event",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
    pub fn first_event(self) -> u32 {
        self.first_event
    }

    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuEventCounterRange.last_event",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
    pub fn last_event(self) -> u32 {
        self.last_event
    }

    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuEventCounterRange.counter_mask",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
    pub fn counter_mask(self) -> u32 {
        self.counter_mask
    }

    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuEventCounterRange.contains",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
    pub fn contains(self, event: u32) -> bool {
        event >= self.first_event && event <= self.last_event
    }
}

/// 一个固件 PMU 实例的不可变描述。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PmuDescriptor {
    name: Box<str>,
    firmware_path: Option<Box<str>>,
    event_counter_ranges: Box<[PmuEventCounterRange]>,
}

#[kernel_symbols::export]
impl PmuDescriptor {
    /// 构造并完整校验一个 PMU 描述。
    ///
    /// 同一实例内的事件区间不能重叠，否则按事件查询时无法得到唯一的 counter
    /// 位图。空映射是合法的，表示平台存在 PMU 节点，但固件没有公布静态映射。
    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuDescriptor.new",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn new(
        name: Box<str>,
        firmware_path: Option<Box<str>>,
        event_counter_ranges: Vec<PmuEventCounterRange>,
    ) -> Result<Self, PmuError> {
        if name.is_empty() {
            return Err(PmuError::Invalid);
        }
        validate_ranges(&event_counter_ranges)?;
        Ok(Self {
            name,
            firmware_path,
            event_counter_ranges: event_counter_ranges.into_boxed_slice(),
        })
    }

    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuDescriptor.name",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
    )]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuDescriptor.firmware_path",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
    )]
    pub fn firmware_path(&self) -> Option<&str> {
        self.firmware_path.as_deref()
    }

    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuDescriptor.event_counter_ranges",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
    )]
    pub fn event_counter_ranges(&self) -> &[PmuEventCounterRange] {
        &self.event_counter_ranges
    }

    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuDescriptor.event_counter_mask",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn event_counter_mask(&self, event: u32) -> Option<u32> {
        self.event_counter_ranges
            .iter()
            .find(|range| range.contains(event))
            .map(|range| range.counter_mask())
    }
}

/// 一次 PMU 注册生命周期的不可复用句柄。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmuHandle {
    id: u64,
}

#[kernel_symbols::export]
impl PmuHandle {
    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuHandle.id",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
    )]
    pub fn id(self) -> u64 {
        self.id
    }
}

/// 用于诊断和 PMU backend 发现的稳定快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PmuSnapshot {
    pub handle: PmuHandle,
    pub descriptor: PmuDescriptor,
}

/// PMU 逻辑 counter 的底层类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PmuCounterKind {
    /// 可由 S-mode 直接读取的硬件 CSR counter。
    Hardware { csr: u16, width: u8 },
    /// 必须通过固件接口读取的 counter。
    Firmware,
}

/// PMU backend 公布的一个逻辑 counter 描述。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmuCounterInfo {
    pub index: usize,
    pub kind: PmuCounterKind,
}

impl PmuCounterInfo {
    pub const fn hardware(index: usize, csr: u16, width: u8) -> Option<Self> {
        if width == 0 || width > 64 {
            return None;
        }
        Some(Self {
            index,
            kind: PmuCounterKind::Hardware { csr, width },
        })
    }

    pub const fn firmware(index: usize) -> Self {
        Self {
            index,
            kind: PmuCounterKind::Firmware,
        }
    }
}

/// 架构层安装的 PMU 执行入口。
///
/// DT/ACPI 驱动只注册事件到 counter 的固件约束；实际配置、启停和读取由本表
/// 指向的 SBI、CSR 或其它架构 backend 完成。所有操作都针对当前 CPU，因此
/// session 会记录创建它的 CPU，并拒绝跨 CPU 使用。
#[derive(Clone, Copy)]
pub struct PmuBackendOps {
    pub current_cpu_id: fn() -> usize,
    /// 读取当前 hart 上实际可用的逻辑 counter 位图。
    ///
    /// SBI 的 `num_counters` 是上界而不是连续有效性保证；backend 必须通过
    /// `counter_get_info` 过滤掉洞（例如逻辑 counter 1 对应不可用于 PMU 的 time
    /// counter）。
    pub valid_counter_mask: fn() -> Result<usize, PmuError>,
    pub counter_info: fn(usize) -> Result<PmuCounterInfo, PmuError>,
    pub configure: fn(usize, u32, u64) -> Result<usize, PmuError>,
    pub start: fn(usize, Option<u64>) -> Result<(), PmuError>,
    pub stop: fn(usize, bool) -> Result<(), PmuError>,
    pub read: fn(PmuCounterInfo) -> Result<u64, PmuError>,
    /// 进入不会发生本地抢占/迁移的短临界区，并返回待恢复的架构状态。
    pub enter_critical: fn() -> usize,
    pub exit_critical: fn(usize),
}

static PMU_BACKEND: Spinlock<Option<PmuBackendOps>> = Spinlock::new(None);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PmuError {
    Invalid,
    InvalidEncoding,
    OverlappingRanges,
    AlreadyRegistered,
    NotFound,
    OutOfMemory,
    NoBackend,
    Unsupported,
    Busy,
    WrongCpu,
    AlreadyRunning,
    NotRunning,
    Backend(isize),
}

struct PmuRegistration {
    handle: PmuHandle,
    /// `None` 表示该 generation 已从发现面摘除，只为既有 session 保留 tombstone。
    descriptor: Option<PmuDescriptor>,
    active_sessions: usize,
    reservations: Vec<PmuReservation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PmuReservation {
    owner_cpu: usize,
    counter: usize,
}

struct PmuRegistry {
    next_id: u64,
    registrations: Vec<PmuRegistration>,
}

impl PmuRegistry {
    const fn new() -> Self {
        Self {
            next_id: 1,
            registrations: Vec::new(),
        }
    }

    fn register(&mut self, descriptor: PmuDescriptor) -> Result<PmuHandle, PmuError> {
        let duplicate = self
            .registrations
            .iter()
            .filter_map(|registration| registration.descriptor.as_ref())
            .any(
                |registered| match (registered.firmware_path(), descriptor.firmware_path()) {
                    (Some(left), Some(right)) => left == right,
                    (None, None) => registered.name() == descriptor.name(),
                    _ => false,
                },
            );
        if duplicate {
            return Err(PmuError::AlreadyRegistered);
        }
        {
            // 注册表容量属于常驻内核；不能把 Vec 的剩余 capacity 记到发起注册的
            // 动态 ELM，否则即使设备已注销，模块仍会被分配账本永久钉住。
            let _accounting =
                allocator::suspend_implicit_allocation_accounting().ok_or(PmuError::OutOfMemory)?;
            self.registrations
                .try_reserve(1)
                .map_err(|_| PmuError::OutOfMemory)?;
        }
        let id =
            registry_id::alloc_locked_id(&mut self.next_id).map_err(|_| PmuError::OutOfMemory)?;
        let handle = PmuHandle { id };
        self.registrations.push(PmuRegistration {
            handle,
            descriptor: Some(descriptor),
            active_sessions: 0,
            reservations: Vec::new(),
        });
        Ok(handle)
    }

    fn unregister(&mut self, handle: PmuHandle) -> Result<(), PmuError> {
        let index = self
            .registrations
            .iter()
            .position(|registration| registration.handle == handle)
            .ok_or(PmuError::NotFound)?;
        if self.registrations[index].active_sessions != 0
            || !self.registrations[index].reservations.is_empty()
        {
            return Err(PmuError::Busy);
        }
        // 默认查询按注册先后选择 PMU；删除中间项时必须保持其余实例的顺序。
        self.registrations.remove(index);
        Ok(())
    }

    /// 停止一个 PMU 实例接收新会话，并在最后一个既有会话结束后回收它。
    ///
    /// PnP 解绑已经从设备对象中取出了资源集合，释放回调不能再把失败的资源放
    /// 回去。因此驱动热卸载不能直接使用可能返回 `Busy` 的 [`Self::unregister`]；
    /// retiring 状态把解绑转换为可完成的两阶段回收，同时允许同一路径的新驱动
    /// 注册一个新 generation。
    fn retire(
        &mut self,
        handle: PmuHandle,
    ) -> Result<(PmuDescriptor, Option<PmuRegistration>), PmuError> {
        let index = self
            .registrations
            .iter()
            .position(|registration| registration.handle == handle)
            .ok_or(PmuError::NotFound)?;
        let descriptor = self.registrations[index]
            .descriptor
            .take()
            .ok_or(PmuError::NotFound)?;
        let retired = self.take_retired_if_idle(index);
        Ok((descriptor, retired))
    }

    fn take_retired_if_idle(&mut self, index: usize) -> Option<PmuRegistration> {
        let registration = &self.registrations[index];
        if registration.descriptor.is_none()
            && registration.active_sessions == 0
            && registration.reservations.is_empty()
        {
            Some(self.registrations.remove(index))
        } else {
            None
        }
    }

    fn reserve_session(
        &mut self,
        handle: PmuHandle,
        event: u32,
        valid_counter_mask: usize,
        owner_cpu: usize,
    ) -> Result<usize, PmuError> {
        let registration_index = self
            .registrations
            .iter()
            .position(|registration| registration.handle == handle)
            .ok_or(PmuError::NotFound)?;
        let descriptor = self.registrations[registration_index]
            .descriptor
            .as_ref()
            .ok_or(PmuError::NotFound)?;
        let mask = if descriptor.event_counter_ranges().is_empty() {
            valid_counter_mask
        } else {
            descriptor
                .event_counter_mask(event)
                .map(|mask| mask as usize)
                .ok_or(PmuError::Unsupported)?
                & valid_counter_mask
        };
        let used = self
            .registrations
            .iter()
            .flat_map(|registration| registration.reservations.iter())
            .filter(|reservation| reservation.owner_cpu == owner_cpu)
            .try_fold(0usize, |used, reservation| {
                if reservation.counter >= usize::BITS as usize {
                    return Err(PmuError::Unsupported);
                }
                Ok(used | (1usize << reservation.counter))
            })?;
        let mask = mask & !used;
        if mask == 0 {
            return Err(PmuError::Unsupported);
        }
        let registration = &mut self.registrations[registration_index];
        registration.active_sessions = registration
            .active_sessions
            .checked_add(1)
            .ok_or(PmuError::OutOfMemory)?;
        Ok(mask)
    }

    fn release_pending_session(
        &mut self,
        handle: PmuHandle,
    ) -> Result<Option<PmuRegistration>, PmuError> {
        let index = self
            .registrations
            .iter()
            .position(|registration| registration.handle == handle)
            .ok_or(PmuError::NotFound)?;
        {
            let registration = &mut self.registrations[index];
            registration.active_sessions = registration
                .active_sessions
                .checked_sub(1)
                .ok_or(PmuError::Invalid)?;
        }
        Ok(self.take_retired_if_idle(index))
    }

    fn commit_session(
        &mut self,
        handle: PmuHandle,
        owner_cpu: usize,
        counter: usize,
    ) -> Result<(), PmuError> {
        if counter >= usize::BITS as usize {
            return Err(PmuError::Unsupported);
        }
        let registration_index = self
            .registrations
            .iter()
            .position(|registration| registration.handle == handle)
            .ok_or(PmuError::NotFound)?;
        if self
            .registrations
            .iter()
            .flat_map(|registration| registration.reservations.iter())
            .any(|reservation| reservation.owner_cpu == owner_cpu && reservation.counter == counter)
        {
            return Err(PmuError::Busy);
        }
        let registration = &mut self.registrations[registration_index];
        {
            // reservation capacity 由常驻 PMU registry 持有，不能归属于打开 session
            // 的动态 ELM；会话关闭后 Vec 仍可能保留这段 capacity。
            let _accounting =
                allocator::suspend_implicit_allocation_accounting().ok_or(PmuError::OutOfMemory)?;
            registration
                .reservations
                .try_reserve(1)
                .map_err(|_| PmuError::OutOfMemory)?;
        }
        registration
            .reservations
            .push(PmuReservation { owner_cpu, counter });
        Ok(())
    }

    fn release_session(
        &mut self,
        handle: PmuHandle,
        owner_cpu: usize,
        counter: usize,
    ) -> Result<Option<PmuRegistration>, PmuError> {
        let registration_index = self
            .registrations
            .iter()
            .position(|registration| registration.handle == handle)
            .ok_or(PmuError::NotFound)?;
        let registration = &mut self.registrations[registration_index];
        let Some(index) = registration.reservations.iter().position(|reservation| {
            reservation.owner_cpu == owner_cpu && reservation.counter == counter
        }) else {
            return Err(PmuError::NotFound);
        };
        registration.reservations.swap_remove(index);
        registration.active_sessions = registration
            .active_sessions
            .checked_sub(1)
            .ok_or(PmuError::Invalid)?;
        Ok(self.take_retired_if_idle(registration_index))
    }

    fn event_counter_mask_for(&self, handle: PmuHandle, event: u32) -> Option<u32> {
        self.registrations
            .iter()
            .find(|registration| registration.handle == handle)
            .and_then(|registration| registration.descriptor.as_ref())
            .and_then(|descriptor| descriptor.event_counter_mask(event))
    }

    fn event_counter_mask(&self, event: u32) -> Option<u32> {
        self.registrations
            .iter()
            .filter_map(|registration| registration.descriptor.as_ref())
            .find_map(|descriptor| descriptor.event_counter_mask(event))
    }

    fn snapshot(&self) -> Vec<PmuSnapshot> {
        self.registrations
            .iter()
            .filter_map(|registration| {
                registration
                    .descriptor
                    .as_ref()
                    .map(|descriptor| PmuSnapshot {
                        handle: registration.handle,
                        descriptor: descriptor.clone(),
                    })
            })
            .collect()
    }
}

static PMUS: Spinlock<PmuRegistry> = Spinlock::new(PmuRegistry::new());

/// 安装当前架构的 PMU backend。
///
/// 架构初始化只能安装一次；驱动热插拔不会替换这组 CPU 指令/固件入口。
pub fn install_backend(ops: PmuBackendOps) -> Result<(), PmuError> {
    let mut backend = PMU_BACKEND.lock();
    if backend.is_some() {
        return Err(PmuError::AlreadyRegistered);
    }
    *backend = Some(ops);
    Ok(())
}

#[kernel_symbols::export(
    name = "general.dev.pmu.backend_available",
    contract = "kernel.general.pmu@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
)]
pub fn backend_available() -> bool {
    PMU_BACKEND.lock().is_some()
}

fn backend() -> Result<PmuBackendOps, PmuError> {
    PMU_BACKEND
        .lock()
        .as_ref()
        .copied()
        .ok_or(PmuError::NoBackend)
}

fn with_backend_critical<T>(ops: PmuBackendOps, action: impl FnOnce() -> T) -> T {
    let state = (ops.enter_critical)();
    let result = action();
    (ops.exit_critical)(state);
    result
}

fn release_pending_reservation(handle: PmuHandle) {
    let retired = match PMUS.lock().release_pending_session(handle) {
        Ok(retired) => retired,
        Err(error) => {
            log::error!(
                "[pmu] failed to release pending session for handle {}: {:?}",
                handle.id(),
                error
            );
            return;
        }
    };
    // tombstone 内的常驻 Vec 在离开 PMUS 锁后销毁。
    drop(retired);
}

/// 一次绑定到创建 CPU 的 PMU 计数会话。
///
/// SBI PMU counter 是 per-hart 资源。即使该值被放入可迁移任务，所有操作仍会
/// 检查当前 CPU；调用方应在同一 CPU 上完成 `start/read/stop/close`。
pub struct PmuSession {
    pmu: PmuHandle,
    event: u32,
    counter: PmuCounterInfo,
    owner_cpu: usize,
    running: bool,
    closed: bool,
    _not_sync: PhantomData<*mut ()>,
}

#[kernel_symbols::export]
impl PmuSession {
    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuSession.event",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn event(&self) -> u32 {
        self.event
    }

    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuSession.counter",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn counter(&self) -> PmuCounterInfo {
        self.counter
    }

    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuSession.owner_cpu",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn owner_cpu(&self) -> usize {
        self.owner_cpu
    }

    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuSession.is_running",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn is_running(&self) -> bool {
        self.running
    }

    fn ensure_owner(&self, ops: PmuBackendOps) -> Result<(), PmuError> {
        if self.closed {
            return Err(PmuError::NotFound);
        }
        if (ops.current_cpu_id)() != self.owner_cpu {
            return Err(PmuError::WrongCpu);
        }
        Ok(())
    }

    /// 从当前值或给定初值启动 counter。
    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuSession.start",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn start(&mut self, initial_value: Option<u64>) -> Result<(), PmuError> {
        let ops = backend()?;
        with_backend_critical(ops, || {
            self.ensure_owner(ops)?;
            if self.running {
                return Err(PmuError::AlreadyRunning);
            }
            (ops.start)(self.counter.index, initial_value)?;
            self.running = true;
            Ok(())
        })
    }

    /// 停止 counter，但保留事件映射以便再次启动或读取。
    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuSession.stop",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn stop(&mut self) -> Result<(), PmuError> {
        let ops = backend()?;
        with_backend_critical(ops, || {
            self.ensure_owner(ops)?;
            if !self.running {
                return Err(PmuError::NotRunning);
            }
            (ops.stop)(self.counter.index, false)?;
            self.running = false;
            Ok(())
        })
    }

    /// 读取当前硬件 CSR 或固件 counter。
    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuSession.read",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
    )]
    pub fn read(&self) -> Result<u64, PmuError> {
        let ops = backend()?;
        with_backend_critical(ops, || {
            self.ensure_owner(ops)?;
            (ops.read)(self.counter)
        })
    }

    /// 停止 counter、解除事件映射并释放 session。
    #[kernel_symbols::export(
        name = "general.dev.pmu.PmuSession.close",
        contract = "kernel.general.pmu@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn close(&mut self) -> Result<(), PmuError> {
        let ops = backend()?;
        with_backend_critical(ops, || {
            self.ensure_owner(ops)?;
            (ops.stop)(self.counter.index, true)?;
            self.running = false;
            let retired =
                PMUS.lock()
                    .release_session(self.pmu, self.owner_cpu, self.counter.index)?;
            drop(retired);
            self.closed = true;
            Ok(())
        })
    }
}

impl Drop for PmuSession {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        let Ok(ops) = backend() else {
            return;
        };
        with_backend_critical(ops, || {
            let current_cpu = (ops.current_cpu_id)();
            if current_cpu != self.owner_cpu {
                log::error!(
                    "[pmu] session for CPU {} dropped on CPU {}; counter {} remains reserved",
                    self.owner_cpu,
                    current_cpu,
                    self.counter.index
                );
                return;
            }
            if let Err(error) = (ops.stop)(self.counter.index, true) {
                log::error!(
                    "[pmu] failed to reset counter {} on CPU {} during drop: {:?}; reservation retained",
                    self.counter.index,
                    self.owner_cpu,
                    error
                );
                return;
            }
            let retired = match PMUS.lock().release_session(
                self.pmu,
                self.owner_cpu,
                self.counter.index,
            ) {
                Ok(retired) => retired,
                Err(error) => {
                    log::error!(
                        "[pmu] failed to release counter {} reservation on CPU {} during drop: {:?}",
                        self.counter.index,
                        self.owner_cpu,
                        error
                    );
                    return;
                }
            };
            drop(retired);
            self.closed = true;
        });
    }
}

/// 按固件约束选择并配置一个当前 CPU 上的 counter。
#[kernel_symbols::export(
    name = "general.dev.pmu.open_session",
    contract = "kernel.general.pmu@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn open_session(
    handle: PmuHandle,
    event: u32,
    event_data: u64,
) -> Result<PmuSession, PmuError> {
    let ops = backend()?;
    with_backend_critical(ops, || {
        let owner_cpu = (ops.current_cpu_id)();
        let valid_counter_mask = (ops.valid_counter_mask)()?;
        if valid_counter_mask == 0 {
            return Err(PmuError::Unsupported);
        }
        let mask = PMUS
            .lock()
            .reserve_session(handle, event, valid_counter_mask, owner_cpu)?;
        let counter_index = match (ops.configure)(mask, event, event_data) {
            Ok(counter) => counter,
            Err(error) => {
                release_pending_reservation(handle);
                return Err(error);
            }
        };
        if counter_index >= usize::BITS as usize
            || valid_counter_mask & (1usize << counter_index) == 0
            || mask & (1usize << counter_index) == 0
        {
            log::error!(
                "[pmu] backend selected counter {} outside requested mask {:#x} on CPU {}; PMU retained busy",
                counter_index,
                mask,
                owner_cpu
            );
            return Err(PmuError::Invalid);
        }
        let counter = match (ops.counter_info)(counter_index) {
            Ok(info) if info.index == counter_index => info,
            Ok(_) => {
                if (ops.stop)(counter_index, true).is_ok() {
                    release_pending_reservation(handle);
                } else {
                    log::error!(
                        "[pmu] failed to reset counter {} after invalid counter info; PMU retained busy",
                        counter_index
                    );
                }
                return Err(PmuError::Invalid);
            }
            Err(error) => {
                if (ops.stop)(counter_index, true).is_ok() {
                    release_pending_reservation(handle);
                } else {
                    log::error!(
                        "[pmu] failed to reset counter {} after counter-info error {:?}; PMU retained busy",
                        counter_index,
                        error
                    );
                }
                return Err(error);
            }
        };
        if let Err(error) = PMUS.lock().commit_session(handle, owner_cpu, counter_index) {
            if error != PmuError::Busy && (ops.stop)(counter_index, true).is_ok() {
                release_pending_reservation(handle);
            } else {
                log::error!(
                    "[pmu] failed to commit counter {} reservation on CPU {}: {:?}; PMU retained busy",
                    counter_index,
                    owner_cpu,
                    error
                );
            }
            return Err(error);
        }
        Ok(PmuSession {
            pmu: handle,
            event,
            counter,
            owner_cpu,
            running: false,
            closed: false,
            _not_sync: PhantomData,
        })
    })
}

/// 解码通用的三-cell `<first-event last-event counter-mask>` 矩阵。
///
/// 固件驱动应在调用后继续校验架构专属的 event type 范围。本函数负责公共的
/// stride、闭区间方向和区间重叠检查，并保持固件中的条目顺序。
#[kernel_symbols::export(
    name = "general.dev.pmu.decode_event_counter_ranges",
    contract = "kernel.general.pmu@1",
    version = 1,
    capabilities = kernel_symbols::capability::CORE_SAFE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn decode_event_counter_ranges(cells: &[u32]) -> Result<Vec<PmuEventCounterRange>, PmuError> {
    const CELLS_PER_ENTRY: usize = 3;
    if cells.is_empty() || !cells.len().is_multiple_of(CELLS_PER_ENTRY) {
        return Err(PmuError::InvalidEncoding);
    }

    let mut ranges = Vec::new();
    ranges
        .try_reserve(cells.len() / CELLS_PER_ENTRY)
        .map_err(|_| PmuError::OutOfMemory)?;
    for entry in cells.chunks_exact(CELLS_PER_ENTRY) {
        let range = PmuEventCounterRange::new(entry[0], entry[1], entry[2])
            .ok_or(PmuError::InvalidEncoding)?;
        ranges.push(range);
    }
    validate_ranges(&ranges)?;
    Ok(ranges)
}

fn validate_ranges(ranges: &[PmuEventCounterRange]) -> Result<(), PmuError> {
    for (index, range) in ranges.iter().enumerate() {
        if range.first_event() > range.last_event() {
            return Err(PmuError::InvalidEncoding);
        }
        if ranges[..index].iter().any(|other| {
            range.first_event() <= other.last_event() && other.first_event() <= range.last_event()
        }) {
            return Err(PmuError::OverlappingRanges);
        }
    }
    Ok(())
}

#[kernel_symbols::export(
    name = "general.dev.pmu.register",
    contract = "kernel.general.pmu@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn register(descriptor: PmuDescriptor) -> Result<PmuHandle, PmuError> {
    let handle = PMUS.lock().register(descriptor)?;
    pnp::notify_dependency_ready(PnpDependency::Other("pmu"));
    Ok(handle)
}

#[kernel_symbols::export(
    name = "general.dev.pmu.unregister",
    contract = "kernel.general.pmu@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister(handle: PmuHandle) -> Result<(), PmuError> {
    PMUS.lock().unregister(handle)
}

fn release_pmu_resource(handle: PmuHandle) -> bool {
    let (descriptor, retired) = match PMUS.lock().retire(handle) {
        Ok(retired) => retired,
        Err(_) => return false,
    };
    // descriptor 的字符串与事件表由驱动 ELM 分配；必须在锁外及时销毁，才能让
    // 模块分配账本归零。retired tombstone 仅含常驻内核分配的 Vec capacity。
    drop(descriptor);
    drop(retired);
    true
}

/// 把 PMU 注册句柄交给 PnP 设备管理，确保 probe 回滚和 ELM 卸载时自动注销。
#[kernel_symbols::export(
    name = "general.dev.pmu.pnp_resource",
    contract = "kernel.general.pmu@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn pnp_resource(handle: PmuHandle, label: &'static str) -> PnpHandleResource<PmuHandle> {
    PnpHandleResource::new(
        PnpResourceKind::Other("pmu"),
        label,
        handle,
        release_pmu_resource,
    )
}

/// 查询指定 PMU 对一个事件公布的逻辑 counter 位图。
#[kernel_symbols::export(
    name = "general.dev.pmu.event_counter_mask_for",
    contract = "kernel.general.pmu@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE
)]
pub fn event_counter_mask_for(handle: PmuHandle, event: u32) -> Option<u32> {
    PMUS.lock().event_counter_mask_for(handle, event)
}

/// 查询最早注册且覆盖该事件的 PMU counter 位图。
///
/// 单 PMU 平台的性能子系统可以直接使用本入口；多 PMU 平台应先通过 [`snapshot`]
/// 选择实例，再调用 [`event_counter_mask_for`]。
#[kernel_symbols::export(
    name = "general.dev.pmu.event_counter_mask",
    contract = "kernel.general.pmu@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE
)]
pub fn event_counter_mask(event: u32) -> Option<u32> {
    PMUS.lock().event_counter_mask(event)
}

#[kernel_symbols::export(
    name = "general.dev.pmu.snapshot",
    contract = "kernel.general.pmu@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn snapshot() -> Vec<PmuSnapshot> {
    PMUS.lock().snapshot()
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn descriptor(name: &str, path: &str, cells: &[u32]) -> PmuDescriptor {
        PmuDescriptor::new(
            name.into(),
            Some(path.into()),
            decode_event_counter_ranges(cells).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn decodes_event_counter_matrix_and_rejects_ambiguous_ranges() {
        let ranges = decode_event_counter_ranges(&[
            1,
            1,
            0x1,
            2,
            2,
            0x4,
            3,
            10,
            0x0ff8,
            0x10000,
            0x10033,
            0x000f_f000,
        ])
        .unwrap();
        assert_eq!(ranges.len(), 4);
        assert_eq!(ranges[2].first_event(), 3);
        assert_eq!(ranges[2].last_event(), 10);
        assert_eq!(ranges[2].counter_mask(), 0x0ff8);
        assert!(ranges[3].contains(0x10020));

        assert_eq!(
            decode_event_counter_ranges(&[1, 2]),
            Err(PmuError::InvalidEncoding)
        );
        assert_eq!(
            decode_event_counter_ranges(&[2, 1, 1]),
            Err(PmuError::InvalidEncoding)
        );
        assert_eq!(
            decode_event_counter_ranges(&[1, 3, 1, 3, 4, 2]),
            Err(PmuError::OverlappingRanges)
        );
    }

    #[test]
    fn registry_uses_generation_handles_and_stable_per_pmu_queries() {
        let mut registry = PmuRegistry::new();
        let first = registry
            .register(descriptor("riscv-pmu", "/pmu", &[3, 6, 0x18]))
            .unwrap();
        let second = registry
            .register(descriptor("aux-pmu", "/soc/pmu", &[3, 3, 0x80]))
            .unwrap();

        assert_eq!(registry.event_counter_mask_for(first, 4), Some(0x18));
        assert_eq!(registry.event_counter_mask_for(second, 3), Some(0x80));
        assert_eq!(registry.event_counter_mask(3), Some(0x18));
        assert_eq!(registry.snapshot().len(), 2);
        assert_eq!(
            registry.register(descriptor("duplicate", "/pmu", &[7, 7, 1])),
            Err(PmuError::AlreadyRegistered)
        );

        registry.unregister(first).unwrap();
        assert_eq!(registry.event_counter_mask_for(first, 4), None);
        let replacement = registry
            .register(descriptor("riscv-pmu", "/pmu", &[4, 4, 0x20]))
            .unwrap();
        assert_ne!(replacement, first);
        assert_eq!(registry.event_counter_mask_for(replacement, 4), Some(0x20));
        assert_eq!(registry.unregister(first), Err(PmuError::NotFound));
    }

    #[test]
    fn descriptor_accepts_an_explicit_empty_mapping() {
        let descriptor = PmuDescriptor::new("firmware-pmu".into(), None, vec![]).unwrap();
        assert!(descriptor.event_counter_ranges().is_empty());
        assert_eq!(descriptor.event_counter_mask(1), None);
    }

    #[test]
    fn unregister_preserves_default_query_registration_order() {
        let mut registry = PmuRegistry::new();
        let removed = registry
            .register(descriptor("removed", "/pmu0", &[1, 1, 0x1]))
            .unwrap();
        let earliest_remaining = registry
            .register(descriptor("earliest", "/pmu1", &[1, 1, 0x2]))
            .unwrap();
        let later = registry
            .register(descriptor("later", "/pmu2", &[1, 1, 0x4]))
            .unwrap();

        registry.unregister(removed).unwrap();

        assert_eq!(registry.event_counter_mask(1), Some(0x2));
        assert_eq!(
            registry.event_counter_mask_for(earliest_remaining, 1),
            Some(0x2)
        );
        assert_eq!(registry.event_counter_mask_for(later, 1), Some(0x4));
    }

    #[test]
    fn session_reservation_intersects_firmware_mask_and_blocks_unload() {
        let mut registry = PmuRegistry::new();
        let handle = registry
            .register(descriptor("riscv-pmu", "/pmu", &[1, 1, 0b1_1001]))
            .unwrap();

        // backend 的有效位图没有 counter 4，因此固件位 4 必须在配置前被裁掉。
        assert_eq!(registry.reserve_session(handle, 1, 0b1111, 0), Ok(0b1001));
        registry.commit_session(handle, 0, 3).unwrap();
        assert_eq!(registry.unregister(handle), Err(PmuError::Busy));
        assert_eq!(registry.reserve_session(handle, 1, 0b1111, 0), Ok(0b0001));
        assert!(registry.release_pending_session(handle).unwrap().is_none());
        assert_eq!(registry.reserve_session(handle, 1, 0b1111, 1), Ok(0b1001));
        assert!(registry.release_pending_session(handle).unwrap().is_none());
        assert!(registry.release_session(handle, 0, 3).unwrap().is_none());
        assert_eq!(registry.unregister(handle), Ok(()));
    }

    #[test]
    fn empty_mapping_uses_all_backend_counters_and_unknown_event_fails_closed() {
        let mut registry = PmuRegistry::new();
        let unrestricted = registry
            .register(PmuDescriptor::new("firmware-pmu".into(), None, vec![]).unwrap())
            .unwrap();
        assert_eq!(
            registry.reserve_session(unrestricted, 0xf0005, 0b101, 0),
            Ok(0b101)
        );
        assert!(
            registry
                .release_pending_session(unrestricted)
                .unwrap()
                .is_none()
        );

        let constrained = registry
            .register(descriptor("mapped-pmu", "/pmu1", &[1, 2, 0x3]))
            .unwrap();
        assert_eq!(
            registry.reserve_session(constrained, 3, usize::MAX, 0),
            Err(PmuError::Unsupported)
        );
    }

    #[test]
    fn counter_reservations_are_global_per_cpu_and_independent_between_cpus() {
        let mut registry = PmuRegistry::new();
        let first = registry
            .register(PmuDescriptor::new("first".into(), Some("/pmu0".into()), vec![]).unwrap())
            .unwrap();
        let second = registry
            .register(PmuDescriptor::new("second".into(), Some("/pmu1".into()), vec![]).unwrap())
            .unwrap();

        assert_eq!(registry.reserve_session(first, 1, 0b101, 2), Ok(0b101));
        registry.commit_session(first, 2, 2).unwrap();
        assert_eq!(registry.reserve_session(second, 1, 0b101, 2), Ok(0b001));
        assert!(registry.release_pending_session(second).unwrap().is_none());
        assert_eq!(registry.reserve_session(second, 1, 0b101, 3), Ok(0b101));
        assert!(registry.release_pending_session(second).unwrap().is_none());
        assert_eq!(registry.commit_session(second, 2, 2), Err(PmuError::Busy));

        assert!(registry.release_session(first, 2, 2).unwrap().is_none());
        assert_eq!(registry.unregister(first), Ok(()));
        assert_eq!(registry.unregister(second), Ok(()));
    }

    #[test]
    fn retirement_hides_descriptor_and_reclaims_after_active_session() {
        let mut registry = PmuRegistry::new();
        let old = registry
            .register(descriptor("riscv-pmu", "/pmu", &[1, 1, 0b100]))
            .unwrap();
        assert_eq!(registry.reserve_session(old, 1, 0b100, 0), Ok(0b100));
        registry.commit_session(old, 0, 2).unwrap();

        let (retired_descriptor, tombstone) = registry.retire(old).unwrap();
        assert_eq!(retired_descriptor.firmware_path(), Some("/pmu"));
        assert!(tombstone.is_none());
        assert!(registry.snapshot().is_empty());
        assert_eq!(registry.event_counter_mask_for(old, 1), None);
        assert_eq!(
            registry.reserve_session(old, 1, 0b100, 0),
            Err(PmuError::NotFound)
        );

        // 新 generation 可以立即接管同一固件路径，但旧 tombstone 的 counter
        // reservation 在 session 关闭前仍参与全局排他。
        let replacement = registry
            .register(descriptor("riscv-pmu", "/pmu", &[1, 1, 0b100]))
            .unwrap();
        assert_ne!(replacement, old);
        assert_eq!(
            registry.reserve_session(replacement, 1, 0b100, 0),
            Err(PmuError::Unsupported)
        );

        let tombstone = registry.release_session(old, 0, 2).unwrap();
        assert!(tombstone.is_some());
        assert_eq!(registry.unregister(old), Err(PmuError::NotFound));
        assert_eq!(
            registry.reserve_session(replacement, 1, 0b100, 0),
            Ok(0b100)
        );
        assert!(
            registry
                .release_pending_session(replacement)
                .unwrap()
                .is_none()
        );
        registry.unregister(replacement).unwrap();
    }

    #[test]
    fn retirement_handles_idle_and_pending_sessions() {
        let mut registry = PmuRegistry::new();
        let idle = registry
            .register(descriptor("idle", "/pmu-idle", &[1, 1, 1]))
            .unwrap();
        let (_, retired) = registry.retire(idle).unwrap();
        assert!(retired.is_some());
        assert_eq!(registry.unregister(idle), Err(PmuError::NotFound));

        let pending = registry
            .register(descriptor("pending", "/pmu-pending", &[1, 1, 1]))
            .unwrap();
        assert_eq!(registry.reserve_session(pending, 1, 1, 0), Ok(1));
        let (_, retired) = registry.retire(pending).unwrap();
        assert!(retired.is_none());
        // retire 与 configure 之间已获 admission 的 open 可以提交完成。
        registry.commit_session(pending, 0, 0).unwrap();
        assert!(registry.release_session(pending, 0, 0).unwrap().is_some());

        let failed_pending = registry
            .register(descriptor("failed", "/pmu-failed", &[1, 1, 1]))
            .unwrap();
        assert_eq!(registry.reserve_session(failed_pending, 1, 1, 0), Ok(1));
        let (_, retired) = registry.retire(failed_pending).unwrap();
        assert!(retired.is_none());
        assert!(
            registry
                .release_pending_session(failed_pending)
                .unwrap()
                .is_some()
        );
    }
}

//! 常驻网络协议栈 broker。
//!
//! 常驻 host 负责 `net.stack` generation 生命周期和 worker-turn pinned batch 调用；
//! packet ownership 只在完整 sidecar 通过校验后由调用方移动。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use net::stack::{
    NET_STACK_SHARD_TURN_RUST_ABI, NET_STACK_SHARD_TURN_STATUS_BUSY,
    NET_STACK_SHARD_TURN_STATUS_OK, NetStackControlCommand,
    NetStackEndpoint, NetStackFlowCommand, NetStackHandle, NetStackLifecycle,
    NetStackRegisterError, NetStackRegisterErrorKind, NetStackRegistrar, NetStackRegistration,
    NetStackRemoveError, NetStackShardTurn, NetStackSnapshot, NetStackState,
};
use sched::sync::Spinlock;

static BROKER: Spinlock<KernelNetStackBroker> = Spinlock::new(KernelNetStackBroker::new());
pub(crate) const NET_STACK_EXECUTION_ACTION: u64 = 1;

#[cfg(feature = "performance-profile")]
fn observe_duplicate_stack_request(kind: Option<sched::ExecutionScopeKind>) {
    profiling::observe(profiling::Metric::NetStackDuplicateRequests, 1);
    match kind {
        Some(sched::ExecutionScopeKind::Syscall) => {
            profiling::observe(profiling::Metric::NetStackDuplicateSyscall, 1);
        }
        Some(sched::ExecutionScopeKind::NetworkWorker) => {
            profiling::observe(profiling::Metric::NetStackDuplicateWorker, 1);
        }
        None => {}
    }
}

fn claim_stack_call() -> bool {
    let claim = sched::current_task_fast().claim_execution_action(NET_STACK_EXECUTION_ACTION);
    #[cfg(feature = "performance-profile")]
    match claim {
        sched::ExecutionActionClaim::Claimed(sched::ExecutionScopeKind::Syscall) => {
            profiling::observe(profiling::Metric::NetStackSyscallCalls, 1);
        }
        sched::ExecutionActionClaim::Claimed(sched::ExecutionScopeKind::NetworkWorker) => {
            profiling::observe(profiling::Metric::NetStackWorkerCalls, 1);
        }
        sched::ExecutionActionClaim::OutsideScope => {
            profiling::observe(profiling::Metric::NetStackUnscopedCalls, 1);
        }
        sched::ExecutionActionClaim::AlreadyClaimed(kind) => {
            observe_duplicate_stack_request(Some(kind));
        }
    }
    !matches!(claim, sched::ExecutionActionClaim::AlreadyClaimed(_))
}

pub(crate) fn stack_call_budget_exhausted() -> bool {
    let task = sched::current_task_fast();
    let exhausted = task.execution_action_claimed(NET_STACK_EXECUTION_ACTION);
    #[cfg(feature = "performance-profile")]
    if exhausted {
        observe_duplicate_stack_request(task.execution_scope_kind());
    }
    exhausted
}

struct KernelNetStackRegistrar;

enum StackCall {
    Integrated(net::stack::IntegratedNetStackShardTurn),
    Pinned(Arc<PinnedStackCall>),
}

struct PinnedCallSlot {
    call: Spinlock<crate::elm::PinnedNativeCall>,
}

struct PinnedStackCall {
    owner: elm_model::ElmId,
    generation: elm_model::Generation,
    name: Box<str>,
    contract: Box<str>,
    version: u32,
    rust_abi: &'static str,
    per_cpu: Spinlock<Vec<Option<Arc<PinnedCallSlot>>>>,
}

impl Clone for StackCall {
    fn clone(&self) -> Self {
        match self {
            Self::Integrated(call) => Self::Integrated(*call),
            Self::Pinned(call) => Self::Pinned(Arc::clone(call)),
        }
    }
}

impl StackCall {
    fn invoke(
        &self,
        turn: &mut NetStackShardTurn,
        host_ranges: &[(usize, usize)],
    ) -> Result<i32, i32> {
        match self {
            Self::Integrated(call) => Ok(call(turn)),
            Self::Pinned(call) => call.invoke(turn, host_ranges),
        }
    }
}

impl PinnedStackCall {
    fn new(
        endpoint: &net::stack::PinnedNetStackShardTurnEndpoint,
        rust_abi: &'static str,
    ) -> Result<Self, ()> {
        let owner = elm_model::ElmId(endpoint.owner_cell());
        let generation = elm_model::Generation(endpoint.owner_generation());
        let name: Box<str> = endpoint.export_name().into();
        let contract: Box<str> = endpoint.export_contract().into();
        let version = endpoint.export_version();
        let mut per_cpu = Vec::new();
        per_cpu.try_reserve_exact(sched::NR_CPUS).map_err(|_| ())?;
        per_cpu.resize_with(sched::NR_CPUS, || None);
        let current_cpu = sched::current_cpu_id();
        if current_cpu >= sched::NR_CPUS {
            return Err(());
        }
        per_cpu[current_cpu] = Some(Arc::new(Self::new_slot(
            owner, generation, &name, &contract, version, rust_abi,
        )?));
        Ok(Self {
            owner,
            generation,
            name,
            contract,
            version,
            rust_abi,
            per_cpu: Spinlock::new(per_cpu),
        })
    }

    fn new_slot(
        owner: elm_model::ElmId,
        generation: elm_model::Generation,
        name: &str,
        contract: &str,
        version: u32,
        rust_abi: &str,
    ) -> Result<PinnedCallSlot, ()> {
        let call =
            crate::elm::PinnedNativeCall::new(owner, generation, name, contract, version, rust_abi)
                .map_err(|_| ())?;
        Ok(PinnedCallSlot {
            call: Spinlock::new(call),
        })
    }

    fn current_slot(&self) -> Result<Arc<PinnedCallSlot>, i32> {
        let cpu = sched::current_cpu_id();
        if cpu >= sched::NR_CPUS {
            return Err(elm_model::ELM_MGR_STATUS_BUSY);
        }
        if let Some(slot) = self.per_cpu.lock()[cpu].as_ref().map(Arc::clone) {
            return Ok(slot);
        }
        let slot = Arc::new(
            Self::new_slot(
                self.owner,
                self.generation,
                &self.name,
                &self.contract,
                self.version,
                self.rust_abi,
            )
            .map_err(|_| elm_model::ELM_MGR_STATUS_BUSY)?,
        );
        let mut per_cpu = self.per_cpu.lock();
        if let Some(existing) = per_cpu[cpu].as_ref() {
            return Ok(Arc::clone(existing));
        }
        per_cpu[cpu] = Some(Arc::clone(&slot));
        Ok(slot)
    }

    fn invoke<T>(&self, frame: &mut T, host_ranges: &[(usize, usize)]) -> Result<i32, i32> {
        let slot = self.current_slot()?;
        // 网络 ABI 都有显式的批次、报文或状态机边界；卸载取消仍由 ELM 保护域处理。
        if let Some(call) = slot.call.try_lock() {
            return crate::elm::invoke_pinned_native(
                &call,
                frame,
                host_ranges,
                crate::elm::NO_WATCHDOG_DEADLINE_NS,
            );
        }
        Err(elm_model::ELM_MGR_STATUS_BUSY)
    }
}

impl ElmShardTurnClient {
    pub(crate) const fn new(id: net::ShardId) -> Self {
        Self { id }
    }

    fn invoke_control(
        &self,
        command: NetStackControlCommand,
        extra_ranges: &[(usize, usize)],
    ) -> Result<NetStackControlCommand, (ShardTurnError, NetStackControlCommand)> {
        let mut control_commands = Vec::with_capacity(1);
        control_commands.push(command);
        match self.invoke_batches(control_commands, Vec::new(), extra_ranges) {
            Ok(mut batch) => Ok(batch
                .control_commands
                .pop()
                .expect("single shard-turn control command")),
            Err((error, mut batch)) => Err((
                error,
                batch
                    .control_commands
                    .pop()
                    .expect("single shard-turn control command"),
            )),
        }
    }

    #[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
    pub(crate) fn invoke_turn(
        &self,
        commands: Vec<NetStackFlowCommand>,
        extra_ranges: &[(usize, usize)],
    ) -> Result<Vec<NetStackFlowCommand>, (ShardTurnError, Vec<NetStackFlowCommand>)> {
        if commands.len() > net::stack::NET_STACK_SHARD_TURN_COMMAND_CAPACITY {
            return Err((ShardTurnError::CallFailed, commands));
        }
        match self.invoke_batches(Vec::new(), commands, extra_ranges) {
            Ok(batch) => Ok(batch.commands.into_vec()),
            Err((error, batch)) => Err((error, batch.commands.into_vec())),
        }
    }

    fn invoke_batches(
        &self,
        mut control_commands: Vec<NetStackControlCommand>,
        mut commands: Vec<NetStackFlowCommand>,
        extra_ranges: &[(usize, usize)],
    ) -> Result<ShardTurnBatch, (ShardTurnError, ShardTurnBatch)> {
        if control_commands.len().saturating_add(commands.len())
            > net::stack::NET_STACK_SHARD_TURN_COMMAND_CAPACITY
        {
            unreachable!("cold shard-turn caller 必须在提交前限制批次容量");
        }
        let mut control_batch = net::stack::NetStackCommandBatch::new();
        let mut command_batch = net::stack::NetStackCommandBatch::new();
        control_batch
            .move_from_vec(&mut control_commands)
            .unwrap_or_else(|_| unreachable!());
        command_batch
            .move_from_vec(&mut commands)
            .unwrap_or_else(|_| unreachable!());
        self.invoke_fixed_batches(
            control_batch,
            command_batch,
            net::stack::TxPlanBatch::new(),
            extra_ranges,
        )
    }

    fn invoke_fixed_batches(
        &self,
        control_commands: net::stack::NetStackCommandBatch<NetStackControlCommand>,
        commands: net::stack::NetStackCommandBatch<NetStackFlowCommand>,
        tx_plans: net::stack::TxPlanBatch,
        extra_ranges: &[(usize, usize)],
    ) -> Result<ShardTurnBatch, (ShardTurnError, ShardTurnBatch)> {
        let (generation, call) = {
            let broker = BROKER.lock();
            let snapshot = broker.lifecycle.snapshot();
            if snapshot.state != NetStackState::Active || !snapshot.ready {
                return Err((
                    ShardTurnError::StackUnavailable,
                    ShardTurnBatch {
                        control_commands,
                        commands,
                        tx_plans,
                    },
                ));
            }
            let Some(record) = broker.record.as_ref() else {
                return Err((
                    ShardTurnError::StackUnavailable,
                    ShardTurnBatch {
                        control_commands,
                        commands,
                        tx_plans,
                    },
                ));
            };
            (record.generation, record.call.clone())
        };
        if control_commands.len().saturating_add(commands.len())
            > net::stack::NET_STACK_SHARD_TURN_COMMAND_CAPACITY
        {
            return Err((
                ShardTurnError::CallFailed,
                ShardTurnBatch {
                    control_commands,
                    commands,
                    tx_plans,
                },
            ));
        }
        let mut flow = NetStackShardTurn::batch_with_output(
            generation,
            self.id,
            control_commands,
            commands,
            tx_plans,
        );
        if extra_ranges.len() > 2 {
            return Err((ShardTurnError::CallFailed, ShardTurnBatch::from_call(flow)));
        }
        let mut ranges = [(0usize, 0usize); 5];
        let mut range_count = 0;
        if !flow.control_commands.is_empty() {
            let Some(range) = host_slice_range(flow.control_commands.slots()) else {
                return Err((ShardTurnError::CallFailed, ShardTurnBatch::from_call(flow)));
            };
            ranges[range_count] = range;
            range_count += 1;
        }
        if !flow.commands.is_empty() {
            let Some(range) = host_slice_range(flow.commands.slots()) else {
                return Err((ShardTurnError::CallFailed, ShardTurnBatch::from_call(flow)));
            };
            ranges[range_count] = range;
            range_count += 1;
        }
        let Some(range) = host_slice_range(flow.tx_plans.slots()) else {
            return Err((ShardTurnError::CallFailed, ShardTurnBatch::from_call(flow)));
        };
        ranges[range_count] = range;
        range_count += 1;
        ranges[range_count..range_count + extra_ranges.len()].copy_from_slice(extra_ranges);
        range_count += extra_ranges.len();
        if !claim_stack_call() {
            return Err((ShardTurnError::Busy, ShardTurnBatch::from_call(flow)));
        }
        let result = call.invoke(&mut flow, &ranges[..range_count]);
        let committed_valid = flow.valid_committed(generation);
        let valid = matches!(result, Ok(NET_STACK_SHARD_TURN_STATUS_OK)) && committed_valid;
        if valid {
            return Ok(ShardTurnBatch::from_call(flow));
        }
        if matches!(result, Ok(NET_STACK_SHARD_TURN_STATUS_BUSY)) && flow.committed == 0 {
            return Err((ShardTurnError::Busy, ShardTurnBatch::from_call(flow)));
        }
        log::error!(
            "[net-stack] shard-turn failed: result={:?} generation={} shard={} committed={} committed_valid={} control={} flow={} ranges={}",
            result,
            generation,
            flow.shard.0,
            flow.committed,
            committed_valid,
            flow.control_commands.len(),
            flow.commands.len(),
            range_count,
        );
        Err((ShardTurnError::CallFailed, ShardTurnBatch::from_call(flow)))
    }

    pub(crate) fn run_worker_turn(
        &self,
        mut control_commands: Vec<NetStackControlCommand>,
        mut commands: Vec<NetStackFlowCommand>,
        mut control_batch: net::stack::NetStackCommandBatch<NetStackControlCommand>,
        mut command_batch: net::stack::NetStackCommandBatch<NetStackFlowCommand>,
        tx_plans: net::stack::TxPlanBatch,
        config: &net::control::ConfigSnapshot,
        now_ns: u64,
        tcp_output: &mut Vec<net::transport::PreparedTcpTx>,
        inline_pool_installs: &mut Vec<(Arc<net::SocketFacade>, net::InterfaceId)>,
        inline_local_tcp: bool,
    ) -> ShardWorkerTurnOutput {
        let Some(config_range) = host_range(config) else {
            return ShardWorkerTurnOutput::failed(
                control_commands,
                commands,
                control_batch,
                command_batch,
                tx_plans,
            );
        };
        commands.push(NetStackFlowCommand::RunDueTimers { now_ns });
        commands.push(NetStackFlowCommand::RunNeighborTimers {
            now_ns,
            output: None,
        });
        commands.push(NetStackFlowCommand::TakeTcpOutputBatch {
            output: Some(core::mem::take(tcp_output)),
            inline_pool_installs: Some(core::mem::take(inline_pool_installs)),
            needs_resume: None,
            limit: 256,
            resume_budget: 256,
            inline_local_tcp,
            config: config as *const _,
            now_ns,
        });
        commands.push(NetStackFlowCommand::NextTimerDeadline { output: None });
        commands.push(NetStackFlowCommand::Stats { output: None });
        if control_commands.len().saturating_add(commands.len())
            > net::stack::NET_STACK_SHARD_TURN_COMMAND_CAPACITY
        {
            return ShardWorkerTurnOutput::failed(
                control_commands,
                commands,
                control_batch,
                command_batch,
                tx_plans,
            );
        }
        control_batch
            .move_from_vec(&mut control_commands)
            .unwrap_or_else(|_| unreachable!());
        command_batch
            .move_from_vec(&mut commands)
            .unwrap_or_else(|_| unreachable!());
        let (mut control_batch, mut command_batch, tx_plans, committed) = match self
            .invoke_fixed_batches(control_batch, command_batch, tx_plans, &[config_range])
        {
            Ok(batch) => (batch.control_commands, batch.commands, batch.tx_plans, true),
            Err((_, batch)) => (
                batch.control_commands,
                batch.commands,
                batch.tx_plans,
                false,
            ),
        };
        control_batch.drain_into_vec(&mut control_commands);
        command_batch.drain_into_vec(&mut commands);
        let stats = match commands.pop() {
            Some(NetStackFlowCommand::Stats {
                output: Some(stats),
            }) => stats,
            _ => net::flow::FlowShardStats::default(),
        };
        let next_timer_deadline = match commands.pop() {
            Some(NetStackFlowCommand::NextTimerDeadline {
                output: Some(deadline),
            }) => deadline,
            _ => None,
        };
        let blocked = match commands.pop() {
            Some(NetStackFlowCommand::TakeTcpOutputBatch {
                output: Some(returned),
                inline_pool_installs: Some(returned_pool_installs),
                needs_resume,
                ..
            }) => {
                *tcp_output = returned;
                *inline_pool_installs = returned_pool_installs;
                needs_resume.unwrap_or(false)
            }
            _ => false,
        };
        let neighbor_timers = match commands.pop() {
            Some(NetStackFlowCommand::RunNeighborTimers {
                output: Some(output),
                ..
            }) => Some(output),
            _ => None,
        };
        let _ = commands.pop();
        ShardWorkerTurnOutput {
            control_commands,
            commands,
            control_batch,
            command_batch,
            next_timer_deadline,
            neighbor_timers,
            blocked,
            stats,
            tx_plans,
            committed,
        }
    }
}

struct ShardTurnBatch {
    control_commands: net::stack::NetStackCommandBatch<NetStackControlCommand>,
    commands: net::stack::NetStackCommandBatch<NetStackFlowCommand>,
    tx_plans: net::stack::TxPlanBatch,
}

impl ShardTurnBatch {
    fn from_call(call: NetStackShardTurn) -> Self {
        Self {
            control_commands: call.control_commands,
            commands: call.commands,
            tx_plans: call.tx_plans,
        }
    }
}

pub(crate) struct ShardWorkerTurnOutput {
    pub(crate) control_commands: Vec<NetStackControlCommand>,
    pub(crate) commands: Vec<NetStackFlowCommand>,
    pub(crate) control_batch: net::stack::NetStackCommandBatch<NetStackControlCommand>,
    pub(crate) command_batch: net::stack::NetStackCommandBatch<NetStackFlowCommand>,
    pub(crate) next_timer_deadline: Option<u64>,
    pub(crate) neighbor_timers: Option<net::stack::NeighborTimerOutput>,
    pub(crate) blocked: bool,
    pub(crate) stats: net::flow::FlowShardStats,
    pub(crate) tx_plans: net::stack::TxPlanBatch,
    pub(crate) committed: bool,
}

impl ShardWorkerTurnOutput {
    fn failed(
        control_commands: Vec<NetStackControlCommand>,
        commands: Vec<NetStackFlowCommand>,
        mut control_batch: net::stack::NetStackCommandBatch<NetStackControlCommand>,
        mut command_batch: net::stack::NetStackCommandBatch<NetStackFlowCommand>,
        mut tx_plans: net::stack::TxPlanBatch,
    ) -> Self {
        control_batch.clear();
        command_batch.clear();
        tx_plans.clear();
        Self {
            control_commands,
            commands,
            control_batch,
            command_batch,
            next_timer_deadline: None,
            neighbor_timers: None,
            blocked: false,
            stats: net::flow::FlowShardStats::default(),
            tx_plans,
            committed: false,
        }
    }
}

impl ElmControlPlane {
    pub(crate) const fn new() -> Self {
        Self {
            call: ElmShardTurnClient::new(net::ShardId(0)),
        }
    }

    fn invoke_with_ranges(
        &self,
        command: NetStackControlCommand,
        ranges: &[(usize, usize)],
    ) -> Option<NetStackControlCommand> {
        match self.call.invoke_control(command, ranges) {
            Ok(command) => Some(command),
            _ => None,
        }
    }

    fn invoke(&self, command: NetStackControlCommand) -> Option<NetStackControlCommand> {
        self.invoke_with_ranges(command, &[])
    }

    pub(crate) fn configure_active_shards(&self, count: usize) -> bool {
        let Ok(count) = u16::try_from(count) else {
            return false;
        };
        matches!(
            self.invoke(NetStackControlCommand::ConfigureActiveShards {
                count,
                output: None,
            }),
            Some(NetStackControlCommand::ConfigureActiveShards {
                output: Some(true),
                ..
            })
        )
    }

    pub(crate) fn initialize_autoconfig(
        &self,
        config: &net::control::ConfigSnapshot,
        now_ns: u64,
    ) -> bool {
        let pointer = config as *const net::control::ConfigSnapshot;
        let Some(range) = host_range(pointer) else {
            return false;
        };
        matches!(
            self.invoke_with_ranges(
                NetStackControlCommand::InitializeAutoconfig {
                    config: pointer,
                    now_ns,
                    output: None,
                },
                &[range],
            ),
            Some(NetStackControlCommand::InitializeAutoconfig {
                output: Some(true),
                ..
            })
        )
    }
}

struct StackRecord {
    generation: u64,
    call: StackCall,
}

struct KernelNetStackBroker {
    lifecycle: NetStackLifecycle,
    record: Option<StackRecord>,
}

#[derive(Clone, Copy)]
pub(crate) struct ElmShardTurnClient {
    id: net::ShardId,
}

#[derive(Clone, Copy)]
pub(crate) struct ElmControlPlane {
    call: ElmShardTurnClient,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShardTurnError {
    StackUnavailable,
    Busy,
    CallFailed,
}

impl KernelNetStackBroker {
    const fn new() -> Self {
        Self {
            lifecycle: NetStackLifecycle::new(),
            record: None,
        }
    }

    fn build_call(endpoint: &NetStackEndpoint) -> Result<StackCall, ()> {
        match endpoint {
            NetStackEndpoint::Integrated(call) if *call as usize != 0 => {
                Ok(StackCall::Integrated(*call))
            }
            NetStackEndpoint::Integrated(_) => Err(()),
            NetStackEndpoint::Pinned(endpoint) => {
                let call = PinnedStackCall::new(endpoint, NET_STACK_SHARD_TURN_RUST_ABI)?;
                Ok(StackCall::Pinned(Arc::new(call)))
            }
        }
    }
}

impl NetStackRegistrar for KernelNetStackRegistrar {
    fn register_stack(
        &self,
        registration: NetStackRegistration,
    ) -> Result<NetStackHandle, NetStackRegisterError> {
        let handle = registration.handle();
        let owner_cell = registration.owner_cell();
        let generation = registration.generation();
        let mut broker = BROKER.lock();
        if broker.lifecycle.snapshot().state != net::stack::NetStackState::Absent {
            return Err(NetStackRegisterError {
                kind: NetStackRegisterErrorKind::AlreadyActive,
                registration,
            });
        }
        let call = match KernelNetStackBroker::build_call(registration.endpoint()) {
            Ok(call) => call,
            Err(()) => {
                return Err(NetStackRegisterError {
                    kind: NetStackRegisterErrorKind::ResourceExhausted,
                    registration,
                });
            }
        };
        if let Err(kind) = broker.lifecycle.activate(handle, owner_cell, generation) {
            return Err(NetStackRegisterError { kind, registration });
        }
        broker.record = Some(StackRecord { generation, call });
        assert!(
            broker.lifecycle.mark_ready(handle),
            "新注册的 net.stack generation 必须可立即进入 shard turn"
        );
        log::info!(
            "[net-stack] registered ready generation: cell={} generation={} handle={}",
            owner_cell,
            generation,
            handle.0
        );
        Ok(handle)
    }

    fn begin_remove(
        &self,
        handle: NetStackHandle,
        owner_cell: u64,
        generation: u64,
    ) -> Result<(), NetStackRemoveError> {
        {
            let mut broker = BROKER.lock();
            broker
                .lifecycle
                .begin_remove(handle, owner_cell, generation)?;
            if !broker.lifecycle.begin_drain(handle) {
                return Err(NetStackRemoveError::Busy);
            }
        }
        let detached_proxies = net::detach_proxy_stack(handle.0);
        let detached_socket_facades = net::detach_socket_generation(generation);
        let mut broker = BROKER.lock();
        broker.record = None;
        if !broker.lifecycle.finish_remove(handle) {
            return Err(NetStackRemoveError::Busy);
        }
        log::info!(
            "[net-stack] removed generation: cell={} generation={} handle={}",
            owner_cell,
            generation,
            handle.0
        );
        if detached_proxies != 0 || detached_socket_facades != 0 {
            log::info!(
                "[net-stack] detached sockets: generation={} proxies={} socket_facades={}",
                generation,
                detached_proxies,
                detached_socket_facades
            );
        }
        Ok(())
    }

    fn snapshot(&self) -> NetStackSnapshot {
        BROKER.lock().lifecycle.snapshot()
    }
}

pub(crate) fn registrar() -> &'static dyn NetStackRegistrar {
    &KernelNetStackRegistrar
}

/// 启动允许 stack 缺席的常驻 host。
pub(crate) fn start_host() {
    let snapshot = net::stack::stack_snapshot();
    if snapshot.state == net::stack::NetStackState::Absent {
        log::info!("[net-stack] host started without stack generation");
    } else {
        assert!(snapshot.ready, "已注册 net.stack generation 必须已经 ready");
        log::info!(
            "[net-stack] host started with ready generation={} handle={}",
            snapshot.generation,
            snapshot.handle.map_or(0, |handle| handle.0),
        );
    }
}

fn host_range<T>(pointer: *const T) -> Option<(usize, usize)> {
    let start = pointer as usize;
    let end = start.checked_add(core::mem::size_of::<T>())?;
    (start != 0 && start < end).then_some((start, end))
}

fn host_slice_range<T>(slice: &[T]) -> Option<(usize, usize)> {
    let start = slice.as_ptr() as usize;
    let bytes = core::mem::size_of::<T>().checked_mul(slice.len())?;
    let end = start.checked_add(bytes)?;
    (start != 0 && start < end).then_some((start, end))
}
